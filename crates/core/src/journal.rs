//! Persistent undo journal for committed transactions — the broker's saga
//! discipline (`crates/broker/src/saga.rs`) applied to filesystem merges:
//! every commit arms its exact inverse *before* the forward operation runs,
//! and the inverse is recorded as data so a revert is journaled and audited.
//!
//! At commit time, before the upper layer is merged into the lower, the
//! journal captures an inverse layer: for every path the transaction touched
//! it saves the prior lower-layer version, and for paths the transaction
//! created it writes an OverlayFS-style whiteout (undo = delete). Undoing a
//! commit is therefore the same operation as the commit itself — apply a
//! layer onto the lower dir — just with the inverse layer instead of the
//! upper one.
//!
//! Entries live under a persistent root (default `/var/lib/cortex/journal`,
//! deliberately not tmpfs so undo survives reboot) and are reverted LIFO
//! like `Saga::revert`. An undone entry is renamed with an `.undone` suffix
//! rather than deleted: reversible *and* audited.
//!
//! Two preconditions make the undo trustworthy rather than merely attempted:
//!
//! 1. **Drift detection.** Each entry records a fingerprint of what cortex
//!    *left behind* at every path it touched. Undo re-derives those
//!    fingerprints and refuses if the world moved — someone else's edit is
//!    never silently clobbered. See [`crate::guard`].
//! 2. **Verified compensation.** An entry whose effects live outside the
//!    filesystem carries an `undo_cmd` *and* an `undo_verify` post-condition.
//!    Undo runs the compensation and then proves it worked; an inverse that
//!    exits 0 without changing the world (`echo done`) fails the check and
//!    the entry stays pending. See [`crate::registry`].

use crate::guard::{detect_drift, Drift, Fingerprint};
use crate::transaction::{is_whiteout, merge_layer};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Where committed transactions record their inverses. Must be persistent
/// storage — an undo journal on tmpfs would vanish exactly when it is most
/// needed (post-reboot).
pub const DEFAULT_JOURNAL_DIR: &str = "/var/lib/cortex/journal";

/// How many changed paths `EntryMeta::sample` keeps; the inverse layer
/// itself is the authoritative record.
const SAMPLE_LIMIT: usize = 50;

/// The `kind` of an entry that was run with explicit consent and cannot be
/// undone. `undo` refuses these by name.
pub const KIND_IRREVERSIBLE: &str = "irreversible";

/// One committed transaction's journal record — the undo as data.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EntryMeta {
    pub id: String,
    /// RFC 3339 UTC timestamp of the commit.
    pub created: String,
    /// Which workflow committed (`safe-config`, `safe-install`, ...).
    pub kind: String,
    /// Service/unit the workflow concerns, if any — undo uses it to stop or
    /// reload the unit around the filesystem restore.
    pub service: Option<String>,
    /// The forward operation, quoted in reports (a command line, a package).
    pub description: String,
    /// The directory the commit merged into and undo applies the inverse to.
    pub lower: PathBuf,
    /// Total paths the commit changed.
    pub changes: usize,
    /// The first [`SAMPLE_LIMIT`] changed paths, relative to `lower`.
    pub sample: Vec<String>,
    /// A shell command undo must run *before* restoring files — the
    /// compensation for state the overlay cannot capture (a container, a
    /// systemd unit, a database migration).
    #[serde(default)]
    pub undo_cmd: Option<String>,
    /// The post-condition that proves `undo_cmd` actually worked. Required
    /// whenever `undo_cmd` is set: an exit code proves nothing (`echo`
    /// exits 0), a verified post-condition proves the world changed back.
    #[serde(default)]
    pub undo_verify: Option<String>,
    /// The registry template this entry came from, when it came from one.
    /// An entry with no template was authorised as explicitly irreversible.
    #[serde(default)]
    pub template_id: Option<String>,
    /// What cortex left at every path it touched, recorded *after* the
    /// merge. Undo compares against this and refuses on drift, so a
    /// concurrent edit is never silently destroyed.
    #[serde(default)]
    pub fingerprints: Vec<(PathBuf, Fingerprint)>,
}

impl EntryMeta {
    /// True when this entry's undo is a verified compensation rather than
    /// (or in addition to) a filesystem restore.
    pub fn has_compensation(&self) -> bool {
        self.undo_cmd.is_some()
    }
}

/// Why an undo refused to run. Each variant is a state the operator must be
/// able to distinguish, because the remedy differs.
#[derive(Debug)]
pub enum UndoRefusal {
    /// The world moved since the commit. Undoing would destroy someone
    /// else's work.
    Drift { entry: String, drifted: Vec<Drift> },
    /// The compensation ran but its post-condition did not hold: the
    /// inverse did not actually reverse the operation.
    CompensationUnverified {
        entry: String,
        cmd: String,
        verify: String,
    },
}

impl std::fmt::Display for UndoRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Drift { entry, drifted } => {
                writeln!(
                    f,
                    "refusing to undo {entry}: {} path(s) changed since cortex committed",
                    drifted.len()
                )?;
                for d in drifted.iter().take(10) {
                    writeln!(f, "  {}", d.describe())?;
                }
                if drifted.len() > 10 {
                    writeln!(f, "  ... and {} more", drifted.len() - 10)?;
                }
                write!(
                    f,
                    "undoing would overwrite that work. Re-run with --force to \
                     proceed; the current contents are copied to a rescue \
                     directory first."
                )
            }
            Self::CompensationUnverified { entry, cmd, verify } => write!(
                f,
                "undo of {entry} is NOT complete: the compensation `{cmd}` exited 0 \
                 but its post-condition `{verify}` does not hold — the operation was \
                 not actually reversed. The entry is left pending. Investigate before \
                 retrying."
            ),
        }
    }
}

impl std::error::Error for UndoRefusal {}

/// A journal entry as listed on disk.
#[derive(Debug, Clone)]
pub struct ListedEntry {
    pub meta: EntryMeta,
    /// No longer pending: either reverted, or explicitly forgotten.
    pub undone: bool,
    /// Dismissed rather than reverted — the change is still applied.
    pub forgotten: bool,
}

/// A compensation without a post-condition is an unverifiable claim. This is
/// the invariant that `--undo-cmd "echo done"` violated: it exited 0, cortex
/// reported the entry undone, and the container kept running. The journal
/// refuses to record such an entry at all.
fn require_verified_compensation(cmd: Option<&str>, verify: Option<&str>) -> Result<()> {
    match (cmd, verify) {
        (Some(c), None) if !c.trim().is_empty() => bail!(
            "refusing to journal a compensation with no post-condition: `{c}` \
             would be trusted on its exit code alone. Every inverse needs an \
             `undo_verify` that proves it worked."
        ),
        (Some(c), Some(v)) if c.trim().is_empty() || v.trim().is_empty() => {
            bail!("compensation and its verifier must both be non-empty")
        }
        _ => Ok(()),
    }
}

/// Run a shell predicate; true when it exits 0.
fn predicate_holds(cmd: &str) -> Result<bool> {
    Ok(Command::new("/bin/sh")
        .args(["-c", cmd])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .with_context(|| format!("failed to run verifier `{cmd}`"))?
        .success())
}

/// Nanosecond precision: ids *are* the LIFO order, so two commits in the
/// same second must still sort in commit order.
fn new_id() -> String {
    format!(
        "{}-{}",
        chrono::Utc::now().format("%Y%m%dT%H%M%S%f"),
        &uuid::Uuid::new_v4().to_string()[..8]
    )
}

fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

pub struct Journal {
    root: PathBuf,
}

impl Journal {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// Capture the exact inverse of merging `upper` into `lower`. Must be
    /// called *before* the merge, while `lower` still holds the prior state.
    #[allow(clippy::too_many_arguments)] // each argument is a distinct journal field
    pub fn capture(
        &self,
        upper: &Path,
        lower: &Path,
        kind: &str,
        service: Option<&str>,
        description: &str,
        undo_cmd: Option<&str>,
        undo_verify: Option<&str>,
        template_id: Option<&str>,
    ) -> Result<EntryMeta> {
        require_verified_compensation(undo_cmd, undo_verify)?;
        let id = new_id();
        let entry_dir = self.root.join(&id);
        let inverse = entry_dir.join("inverse");
        fs::create_dir_all(&inverse)
            .with_context(|| format!("failed to create journal entry {entry_dir:?}"))?;

        let mut changed = Vec::new();
        walk_layer(upper, lower, Some(&inverse), Path::new(""), &mut changed)
            .context("failed to capture inverse layer")?;

        let meta = EntryMeta {
            id,
            created: now_rfc3339(),
            kind: kind.to_string(),
            service: service.map(str::to_string),
            description: description.to_string(),
            lower: lower.to_path_buf(),
            changes: changed.len(),
            sample: changed
                .iter()
                .take(SAMPLE_LIMIT)
                .map(|p| p.display().to_string())
                .collect(),
            undo_cmd: undo_cmd.map(str::to_string),
            undo_verify: undo_verify.map(str::to_string),
            template_id: template_id.map(str::to_string),
            // Recorded by `seal`, once the merge has actually happened: a
            // fingerprint taken now would describe the pre-commit world.
            fingerprints: Vec::new(),
        };
        self.write_meta(&entry_dir, &meta)?;
        // The changed paths drive `seal`; hand them back with the entry.
        Ok(EntryMeta {
            fingerprints: changed
                .into_iter()
                .map(|p| (p, Fingerprint::Absent))
                .collect(),
            ..meta
        })
    }

    /// Record what cortex left at every path it touched. Must be called
    /// *after* the merge lands, because that is the state undo will later
    /// require to still be true. Rewrites the entry's `meta.json` in place.
    ///
    /// The `fingerprints` of the value returned by [`capture`] carry the
    /// changed paths (with placeholder values); this fills in their real
    /// post-commit fingerprints.
    pub fn seal(&self, meta: &EntryMeta) -> Result<EntryMeta> {
        let mut sealed = meta.clone();
        sealed.fingerprints = meta
            .fingerprints
            .iter()
            .map(|(rel, _)| Fingerprint::read(&meta.lower.join(rel)).map(|fp| (rel.clone(), fp)))
            .collect::<Result<Vec<_>>>()
            .context("failed to fingerprint the committed state")?;
        self.write_meta(&self.root.join(&sealed.id), &sealed)?;
        Ok(sealed)
    }

    /// Journal a commit whose effects live entirely outside the filesystem —
    /// a container started through the docker socket, a service brought up
    /// through systemd. There is no upper layer to invert, so the entry
    /// carries only its compensation command: an empty inverse layer plus
    /// `undo_cmd`. Saga vocabulary: `Approximate`, and the caveat is that
    /// the compensation is the whole undo.
    #[allow(clippy::too_many_arguments)] // each argument is a distinct journal field
    pub fn capture_compensation(
        &self,
        lower: &Path,
        kind: &str,
        service: Option<&str>,
        description: &str,
        undo_cmd: &str,
        undo_verify: &str,
        template_id: Option<&str>,
    ) -> Result<EntryMeta> {
        require_verified_compensation(Some(undo_cmd), Some(undo_verify))?;
        let id = new_id();
        let entry_dir = self.root.join(&id);
        fs::create_dir_all(entry_dir.join("inverse"))
            .with_context(|| format!("failed to create journal entry {entry_dir:?}"))?;
        let meta = EntryMeta {
            id,
            created: now_rfc3339(),
            kind: kind.to_string(),
            service: service.map(str::to_string),
            description: description.to_string(),
            lower: lower.to_path_buf(),
            changes: 0,
            sample: Vec::new(),
            undo_cmd: Some(undo_cmd.to_string()),
            undo_verify: Some(undo_verify.to_string()),
            template_id: template_id.map(str::to_string),
            // Nothing on the filesystem changed, so nothing to fingerprint;
            // the compensation's post-condition is this entry's precondition.
            fingerprints: Vec::new(),
        };
        self.write_meta(&entry_dir, &meta)?;
        Ok(meta)
    }

    /// Drop an entry from the pending list without running its undo. The
    /// entry is renamed, not deleted: an audit log you can erase is not an
    /// audit log. Used for irreversible operations the operator reversed by
    /// hand, and for undos that are no longer wanted.
    pub fn forget(&self, id: &str) -> Result<EntryMeta> {
        let entry = self
            .entries()?
            .into_iter()
            .find(|e| e.meta.id == id)
            .with_context(|| format!("no journal entry {id}"))?;
        if entry.undone {
            bail!("journal entry {id} is already undone");
        }
        fs::rename(
            self.root.join(id),
            self.root.join(format!("{id}.forgotten")),
        )
        .with_context(|| format!("failed to mark {id} forgotten"))?;
        Ok(entry.meta)
    }

    /// Record an operation that cannot be reversed. It is journaled so the
    /// audit log and `cortex status` are complete, and marked so `undo`
    /// refuses it out loud instead of skipping it silently.
    pub fn capture_irreversible(&self, lower: &Path, description: &str) -> Result<EntryMeta> {
        let id = new_id();
        let entry_dir = self.root.join(&id);
        fs::create_dir_all(entry_dir.join("inverse"))
            .with_context(|| format!("failed to create journal entry {entry_dir:?}"))?;
        let meta = EntryMeta {
            id,
            created: now_rfc3339(),
            kind: KIND_IRREVERSIBLE.to_string(),
            service: None,
            description: description.to_string(),
            lower: lower.to_path_buf(),
            changes: 0,
            sample: Vec::new(),
            undo_cmd: None,
            undo_verify: None,
            template_id: None,
            fingerprints: Vec::new(),
        };
        self.write_meta(&entry_dir, &meta)?;
        Ok(meta)
    }

    fn write_meta(&self, entry_dir: &Path, meta: &EntryMeta) -> Result<()> {
        fs::write(
            entry_dir.join("meta.json"),
            serde_json::to_vec_pretty(meta)?,
        )
        .with_context(|| format!("failed to write meta.json for journal entry {}", meta.id))
    }

    /// All entries, newest first (ids sort chronologically).
    pub fn entries(&self) -> Result<Vec<ListedEntry>> {
        let mut out = Vec::new();
        let dir = match fs::read_dir(&self.root) {
            Ok(dir) => dir,
            Err(_) => return Ok(out), // no journal yet
        };
        for entry in dir {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().into_owned();
            // Both suffixes mean "no longer pending": one was reverted, the
            // other was dismissed. Neither may be picked up by `undo --all`.
            let undone = name.ends_with(".undone") || name.ends_with(".forgotten");
            let forgotten = name.ends_with(".forgotten");
            let meta_path = entry.path().join("meta.json");
            let Ok(bytes) = fs::read(&meta_path) else {
                continue; // half-written entry; skip rather than fail history
            };
            let meta: EntryMeta = serde_json::from_slice(&bytes)
                .with_context(|| format!("corrupt journal meta {meta_path:?}"))?;
            out.push(ListedEntry {
                meta,
                undone,
                forgotten,
            });
        }
        out.sort_by(|a, b| b.meta.id.cmp(&a.meta.id));
        Ok(out)
    }

    /// The newest entry that has not been undone.
    pub fn latest_pending(&self) -> Result<Option<EntryMeta>> {
        Ok(self.pending()?.into_iter().next())
    }

    /// Every entry not yet undone, newest first — the order `undo --all`
    /// must revert them in.
    pub fn pending(&self) -> Result<Vec<EntryMeta>> {
        Ok(self
            .entries()?
            .into_iter()
            .filter(|e| !e.undone)
            .map(|e| e.meta)
            .collect())
    }

    /// Validate an undo request and return the entry it would revert,
    /// without touching anything. Callers with side effects to sequence
    /// around the restore (stopping a service) must peek first — refusing
    /// an undo after stopping a service would leave the system worse than
    /// doing nothing.
    pub fn peek_undo(&self, id: Option<&str>, force: bool) -> Result<EntryMeta> {
        let entries = self.entries()?;
        let newest_pending = entries
            .iter()
            .find(|e| !e.undone)
            .map(|e| e.meta.id.clone());
        let meta = match id {
            Some(id) => {
                let entry = entries
                    .into_iter()
                    .find(|e| e.meta.id == id)
                    .with_context(|| format!("no journal entry {id}"))?;
                if entry.undone {
                    bail!("journal entry {id} is already undone");
                }
                if let Some(newest) = newest_pending {
                    if newest != id && !force {
                        bail!(
                            "journal entry {newest} is newer and still pending; \
                             undo newest-first (LIFO), or pass --force to undo {id} anyway"
                        );
                    }
                }
                entry.meta
            }
            None => newest_pending
                .and_then(|id| entries.into_iter().find(|e| e.meta.id == id))
                .map(|e| e.meta)
                .context("nothing to undo: no pending journal entries")?,
        };
        Ok(meta)
    }

    /// Revert one committed transaction: apply its inverse layer onto the
    /// lower dir it was committed to, then mark the entry undone. LIFO by
    /// default — undoing an entry older than a still-pending one requires
    /// `force`, because later commits may depend on it.
    /// Undo one entry, in the only order that is safe:
    ///
    /// 1. **Check drift.** If the world moved, refuse — unless forced, in
    ///    which case rescue the about-to-be-clobbered content first.
    /// 2. **Compensate.** Run the inverse for effects outside the filesystem.
    /// 3. **Verify the compensation.** Its post-condition must hold. If it
    ///    does not, stop: the entry stays pending, because marking it undone
    ///    would be a lie, and a lie here is worse than a failure.
    /// 4. **Restore.** Apply the inverse layer to the filesystem.
    /// 5. **Seal.** Only now is the entry marked `.undone`.
    ///
    /// Every step before 5 is a gate. Nothing is marked reverted that was
    /// not proven reverted.
    pub fn undo(&self, id: Option<&str>, force: bool) -> Result<EntryMeta> {
        let meta = self.peek_undo(id, force)?;
        if meta.kind == KIND_IRREVERSIBLE {
            bail!(
                "entry {} is irreversible and was run with explicit consent: `{}`. \
                 Cortex will not pretend to undo it. Reverse it by hand, then \
                 `cortex forget {}` to clear it from the pending list.",
                meta.id,
                meta.description,
                meta.id
            );
        }
        let entry_dir = self.root.join(&meta.id);
        let inverse = entry_dir.join("inverse");
        if !inverse.is_dir() {
            bail!("journal entry {} has no inverse layer", meta.id);
        }

        // 1. Drift: has anyone touched these paths since cortex committed?
        let drifted = detect_drift(&meta.lower, &meta.fingerprints)
            .context("failed to check whether the committed state still holds")?;
        if !drifted.is_empty() {
            if !force {
                return Err(UndoRefusal::Drift {
                    entry: meta.id.clone(),
                    drifted,
                }
                .into());
            }
            let rescue_dir = entry_dir.join("rescued");
            let problems = crate::guard::rescue(&meta.lower, &drifted, &rescue_dir)?;
            eprintln!(
                "[cortex] --force: {} drifted path(s); current contents rescued to {}",
                drifted.len(),
                rescue_dir.display()
            );
            for d in drifted.iter().take(10) {
                eprintln!("[cortex]   {}", d.describe());
            }
            for p in &problems {
                eprintln!("[cortex]   warning: {p}");
            }
        }

        // 2 + 3. Compensate, then PROVE the compensation worked.
        if let Some(cmd) = &meta.undo_cmd {
            let verify = meta.undo_verify.as_deref().ok_or_else(|| {
                anyhow::anyhow!(
                    "journal entry {} has a compensation but no post-condition; \
                     it predates verified undo and cannot be trusted. Inspect \
                     `{cmd}` and reverse it by hand.",
                    meta.id
                )
            })?;

            eprintln!("[cortex] compensating: {cmd}");
            let status = Command::new("/bin/sh").args(["-c", cmd]).status()?;
            if !status.success() {
                bail!(
                    "compensation `{cmd}` failed ({status}); undo aborted before \
                     touching any files — entry {} is still pending",
                    meta.id
                );
            }
            if !predicate_holds(verify)? {
                return Err(UndoRefusal::CompensationUnverified {
                    entry: meta.id.clone(),
                    cmd: cmd.clone(),
                    verify: verify.to_string(),
                }
                .into());
            }
            eprintln!("[cortex] compensation verified: {verify}");
        }

        // 4. Restore the filesystem.
        merge_layer(&inverse, &meta.lower).with_context(|| {
            format!(
                "failed to apply inverse of {} onto {:?}",
                meta.id, meta.lower
            )
        })?;

        // 5. Only now may the entry claim to be undone.
        fs::rename(&entry_dir, self.root.join(format!("{}.undone", meta.id)))
            .with_context(|| format!("inverse applied, but failed to mark {} undone", meta.id))?;
        Ok(meta)
    }
}

/// The paths whose content actually differs between an upper layer and the
/// lower it would merge into — the saga's `NoEffect` detector. A command
/// like `sed -i` rewrites its file even when nothing matched, so a non-empty
/// upper layer does not prove a real change; byte comparison does.
pub fn staged_changes(upper: &Path, lower: &Path) -> Result<Vec<PathBuf>> {
    let mut changed = Vec::new();
    walk_layer(upper, lower, None, Path::new(""), &mut changed)?;
    Ok(changed)
}

/// Walk an upper layer against its lower, recording effective changes and —
/// when `inverse` is given — materializing the prior state: prior versions
/// for changed/deleted paths, whiteouts for created ones. Uses the same
/// vocabulary `merge_layer` applies, so `merge_layer(inverse, lower)` is an
/// exact undo.
fn walk_layer(
    upper: &Path,
    lower: &Path,
    inverse: Option<&Path>,
    rel: &Path,
    changed: &mut Vec<PathBuf>,
) -> Result<()> {
    let upper_dir = upper.join(rel);
    for entry in
        fs::read_dir(&upper_dir).with_context(|| format!("failed to read {upper_dir:?}"))?
    {
        let entry = entry?;
        let rel_path = rel.join(entry.file_name());
        let umeta = entry.metadata()?;
        let lpath = lower.join(&rel_path);
        let lmeta = fs::symlink_metadata(&lpath).ok();
        let ipath = inverse.map(|i| i.join(&rel_path));

        if is_whiteout(&umeta) {
            // Staged deletion: the inverse is the prior file, if there was one.
            if let Some(lmeta) = lmeta {
                changed.push(rel_path);
                if let Some(ipath) = &ipath {
                    copy_prior(&lpath, ipath, &lmeta)?;
                }
            }
        } else if umeta.is_dir() {
            match lmeta {
                Some(lmeta) if lmeta.is_dir() => {
                    // Existing dir: recurse; capture prior perms/owner on the
                    // inverse dir so undo restores them, prune if untouched.
                    if let Some(ipath) = &ipath {
                        fs::create_dir_all(ipath)?;
                        fs::set_permissions(ipath, lmeta.permissions())?;
                        let _ =
                            std::os::unix::fs::chown(ipath, Some(lmeta.uid()), Some(lmeta.gid()));
                    }
                    let before = changed.len();
                    walk_layer(upper, lower, inverse, &rel_path, changed)?;
                    if changed.len() == before {
                        if let Some(ipath) = &ipath {
                            let _ = fs::remove_dir(ipath); // only if empty
                        }
                    }
                }
                Some(lmeta) => {
                    // Non-dir replaced by dir: inverse restores the file.
                    changed.push(rel_path);
                    if let Some(ipath) = &ipath {
                        copy_prior(&lpath, ipath, &lmeta)?;
                    }
                }
                None => {
                    // Created dir: inverse deletes the whole subtree.
                    changed.push(rel_path);
                    if let Some(ipath) = &ipath {
                        write_whiteout(ipath)?;
                    }
                }
            }
        } else if umeta.file_type().is_symlink() {
            let target = fs::read_link(entry.path())?;
            let identical = lmeta.as_ref().is_some_and(|lm| {
                lm.file_type().is_symlink()
                    && fs::read_link(&lpath).map(|t| t == target).unwrap_or(false)
            });
            if !identical {
                changed.push(rel_path);
                if let Some(ipath) = &ipath {
                    match lmeta {
                        Some(lmeta) => copy_prior(&lpath, ipath, &lmeta)?,
                        None => write_whiteout(ipath)?,
                    }
                }
            }
        } else if umeta.is_file() {
            // A change is not just different bytes: `chmod`/`chown` copy a
            // file up with identical content but different metadata, and
            // commit's merge_layer applies that metadata to the lower.
            // Treating those as identical made permission changes invisible
            // to the journal — committed but never captured, so undo could
            // not restore them.
            let identical = lmeta.as_ref().is_some_and(|lm| {
                lm.is_file()
                    && lm.size() == umeta.size()
                    && lm.mode() & 0o7777 == umeta.mode() & 0o7777
                    && lm.uid() == umeta.uid()
                    && lm.gid() == umeta.gid()
                    && contents_equal(&entry.path(), &lpath)
            });
            if !identical {
                changed.push(rel_path);
                if let Some(ipath) = &ipath {
                    match lmeta {
                        Some(lmeta) => copy_prior(&lpath, ipath, &lmeta)?,
                        None => write_whiteout(ipath)?,
                    }
                }
            }
        }
        // sockets/fifos/device nodes other than whiteouts are skipped, in
        // the same way merge_layer skips applying them
    }
    Ok(())
}

/// Copy the prior (lower-layer) version of a path into the inverse layer,
/// preserving type, ownership and permissions.
fn copy_prior(lpath: &Path, ipath: &Path, lmeta: &fs::Metadata) -> Result<()> {
    if lmeta.is_dir() {
        fs::create_dir_all(ipath)?;
        fs::set_permissions(ipath, lmeta.permissions())?;
        let _ = std::os::unix::fs::chown(ipath, Some(lmeta.uid()), Some(lmeta.gid()));
        for entry in fs::read_dir(lpath)? {
            let entry = entry?;
            let meta = entry
                .metadata()
                .or_else(|_| fs::symlink_metadata(entry.path()))?;
            copy_prior(&entry.path(), &ipath.join(entry.file_name()), &meta)?;
        }
    } else if lmeta.file_type().is_symlink() {
        let target = fs::read_link(lpath)?;
        std::os::unix::fs::symlink(target, ipath)?;
    } else if lmeta.is_file() {
        fs::copy(lpath, ipath)
            .with_context(|| format!("failed to copy prior {lpath:?} -> {ipath:?}"))?;
        let _ = std::os::unix::fs::chown(ipath, Some(lmeta.uid()), Some(lmeta.gid()));
    }
    // prior sockets/fifos/devices are not restorable through merge_layer;
    // skipped with the same fidelity as commit itself
    Ok(())
}

/// Write an OverlayFS-style whiteout (0:0 char device) so `merge_layer`
/// applied to the inverse deletes the path the transaction created.
fn write_whiteout(path: &Path) -> Result<()> {
    nix::sys::stat::mknod(
        path,
        nix::sys::stat::SFlag::S_IFCHR,
        nix::sys::stat::Mode::empty(),
        0,
    )
    .with_context(|| format!("failed to write whiteout {path:?} (requires root)"))
}

fn contents_equal(a: &Path, b: &Path) -> bool {
    match (fs::read(a), fs::read(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}
