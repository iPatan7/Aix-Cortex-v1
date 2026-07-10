//! Drift detection and rescue: the precondition that makes undo safe.
//!
//! A journal entry records the state cortex *left behind* at commit time.
//! Undo may only run if the world still looks like that. If someone else
//! changed a path since the commit — a colleague's hotfix, another tool, a
//! config-management run — then applying the stored inverse would silently
//! destroy their work. Every real system with rollback refuses here: git
//! rejects a non-fast-forward, databases use optimistic concurrency,
//! Terraform refuses on state drift. Cortex refuses too.
//!
//! The check is content-addressed, not timestamp-based: mtime lies (it is
//! settable, and a restore can preserve it), while a hash of the bytes does
//! not. For each path the commit touched we record what cortex left there —
//! a digest for a file, the target for a symlink, `Absent` for something the
//! commit deleted. At undo time we re-derive that fingerprint and compare.
//!
//! When drift is found, undo refuses by default. `--force` proceeds, but
//! never silently: every drifted path is copied to a rescue directory first,
//! so the work about to be overwritten is recoverable. A destructive
//! operation that keeps no copy of what it destroyed is not a feature.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs;
use std::path::{Path, PathBuf};

/// What cortex left at a path when it committed — the precondition undo
/// checks before it restores.
///
/// Contents are hashed rather than compared byte-for-byte so an entry stays
/// small regardless of how large the committed files were.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Fingerprint {
    /// A regular file with this content digest and permission bits.
    File { sha256: String, mode: u32 },
    /// A symlink pointing here.
    Symlink { target: PathBuf },
    /// A directory (its children are fingerprinted individually).
    Dir { mode: u32 },
    /// Nothing is here — the commit deleted it, and undo will recreate it.
    Absent,
    /// A socket, fifo or device node: `merge_layer` cannot reproduce these,
    /// so cortex neither committed nor will restore their contents. Recorded
    /// so drift on the *type* is still visible.
    Special,
}

impl Fingerprint {
    /// Read the current fingerprint of a path. A missing path is `Absent`,
    /// not an error: "deleted" is a state undo must be able to observe.
    pub fn read(path: &Path) -> Result<Self> {
        let meta = match fs::symlink_metadata(path) {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Self::Absent),
            Err(e) => return Err(e).with_context(|| format!("failed to stat {path:?}")),
        };
        use std::os::unix::fs::PermissionsExt;
        let mode = meta.permissions().mode() & 0o7777;

        if meta.file_type().is_symlink() {
            return Ok(Self::Symlink {
                target: fs::read_link(path)?,
            });
        }
        if meta.is_dir() {
            return Ok(Self::Dir { mode });
        }
        if meta.is_file() {
            return Ok(Self::File {
                sha256: sha256_file(path)?,
                mode,
            });
        }
        Ok(Self::Special)
    }

    /// A short, human-readable description for drift reports.
    pub fn describe(&self) -> String {
        match self {
            Self::File { sha256, mode } => format!("file {} mode {:o}", &sha256[..12], mode),
            Self::Symlink { target } => format!("symlink -> {}", target.display()),
            Self::Dir { mode } => format!("directory mode {mode:o}"),
            Self::Absent => "absent".to_string(),
            Self::Special => "special file".to_string(),
        }
    }
}

fn sha256_file(path: &Path) -> Result<String> {
    use std::io::Read;
    let mut file = fs::File::open(path).with_context(|| format!("failed to open {path:?}"))?;
    let mut hasher = Sha256::new();
    // Stream rather than read_to_end: a committed file may be a large binary.
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex(&hasher.finalize()))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// One path whose current state disagrees with what cortex committed.
#[derive(Debug, Clone)]
pub struct Drift {
    /// Path relative to the entry's `lower` directory.
    pub path: PathBuf,
    /// What cortex left there at commit time.
    pub expected: Fingerprint,
    /// What is there now.
    pub found: Fingerprint,
}

impl Drift {
    pub fn describe(&self) -> String {
        format!(
            "/{}: cortex left {}, found {}",
            self.path.display(),
            self.expected.describe(),
            self.found.describe()
        )
    }
}

/// Compare every recorded fingerprint against the live filesystem.
/// Returns the paths that moved since the commit, in recorded order.
pub fn detect_drift(lower: &Path, expected: &[(PathBuf, Fingerprint)]) -> Result<Vec<Drift>> {
    let mut drifted = Vec::new();
    for (rel, expect) in expected {
        let found = Fingerprint::read(&lower.join(rel))?;
        if &found != expect {
            drifted.push(Drift {
                path: rel.clone(),
                expected: expect.clone(),
                found,
            });
        }
    }
    Ok(drifted)
}

/// Copy the current content of every drifted path into `rescue_dir` before
/// it is overwritten, preserving relative layout. Returns the rescue
/// directory once something was actually saved.
///
/// Best-effort per path: a rescue that fails for one file must not abort the
/// undo the operator explicitly forced, but it is reported.
pub fn rescue(lower: &Path, drifted: &[Drift], rescue_dir: &Path) -> Result<Vec<String>> {
    let mut problems = Vec::new();
    fs::create_dir_all(rescue_dir)
        .with_context(|| format!("failed to create rescue dir {rescue_dir:?}"))?;

    for d in drifted {
        // Nothing to rescue if the drift is that the path is now gone.
        if matches!(d.found, Fingerprint::Absent) {
            continue;
        }
        let src = lower.join(&d.path);
        let dst = rescue_dir.join(&d.path);
        let res = (|| -> Result<()> {
            if let Some(parent) = dst.parent() {
                fs::create_dir_all(parent)?;
            }
            match &d.found {
                Fingerprint::File { .. } => {
                    fs::copy(&src, &dst)?;
                }
                Fingerprint::Symlink { target } => {
                    std::os::unix::fs::symlink(target, &dst)?;
                }
                Fingerprint::Dir { .. } => {
                    fs::create_dir_all(&dst)?;
                }
                Fingerprint::Absent | Fingerprint::Special => {}
            }
            Ok(())
        })();
        if let Err(e) = res {
            problems.push(format!("could not rescue /{}: {e:#}", d.path.display()));
        }
    }
    Ok(problems)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn tmp() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    #[test]
    fn fingerprint_reads_each_file_type() {
        let t = tmp();
        let root = t.path();
        fs::write(root.join("f"), "hello").unwrap();
        fs::set_permissions(root.join("f"), fs::Permissions::from_mode(0o600)).unwrap();
        fs::create_dir(root.join("d")).unwrap();
        std::os::unix::fs::symlink("/target", root.join("l")).unwrap();

        match Fingerprint::read(&root.join("f")).unwrap() {
            Fingerprint::File { sha256, mode } => {
                // sha256("hello")
                assert_eq!(
                    sha256,
                    "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
                );
                assert_eq!(mode, 0o600);
            }
            other => panic!("expected file, got {other:?}"),
        }
        assert!(matches!(
            Fingerprint::read(&root.join("d")).unwrap(),
            Fingerprint::Dir { .. }
        ));
        assert!(matches!(
            Fingerprint::read(&root.join("l")).unwrap(),
            Fingerprint::Symlink { .. }
        ));
        assert_eq!(
            Fingerprint::read(&root.join("nope")).unwrap(),
            Fingerprint::Absent
        );
    }

    /// The lost-update scenario, which is the whole reason this module
    /// exists: cortex commits v2, a human writes v3, undo must SEE it.
    #[test]
    fn detects_a_concurrent_edit() {
        let t = tmp();
        let lower = t.path();
        fs::write(lower.join("app.conf"), "v2-cortex").unwrap();

        let expected = vec![(
            PathBuf::from("app.conf"),
            Fingerprint::read(&lower.join("app.conf")).unwrap(),
        )];
        assert!(detect_drift(lower, &expected).unwrap().is_empty());

        fs::write(lower.join("app.conf"), "v3-human-hotfix").unwrap();
        let drift = detect_drift(lower, &expected).unwrap();
        assert_eq!(drift.len(), 1);
        assert_eq!(drift[0].path, PathBuf::from("app.conf"));
    }

    /// A permission change with identical bytes is drift too — the same
    /// blind spot that made undo lose chmods before.
    #[test]
    fn detects_permission_only_drift() {
        let t = tmp();
        let lower = t.path();
        let f = lower.join("k");
        fs::write(&f, "same").unwrap();
        fs::set_permissions(&f, fs::Permissions::from_mode(0o644)).unwrap();
        let expected = vec![(PathBuf::from("k"), Fingerprint::read(&f).unwrap())];

        fs::set_permissions(&f, fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(detect_drift(lower, &expected).unwrap().len(), 1);
    }

    /// A path the commit deleted, which someone has since recreated.
    #[test]
    fn detects_resurrected_path() {
        let t = tmp();
        let lower = t.path();
        let expected = vec![(PathBuf::from("gone"), Fingerprint::Absent)];
        assert!(detect_drift(lower, &expected).unwrap().is_empty());

        fs::write(lower.join("gone"), "i am back").unwrap();
        assert_eq!(detect_drift(lower, &expected).unwrap().len(), 1);
    }

    #[test]
    fn rescue_preserves_the_content_about_to_be_overwritten() {
        let t = tmp();
        let lower = t.path().join("lower");
        fs::create_dir_all(lower.join("etc")).unwrap();
        fs::write(lower.join("etc/app.conf"), "v3-human-hotfix").unwrap();
        std::os::unix::fs::symlink("/new", lower.join("link")).unwrap();

        let drifted = vec![
            Drift {
                path: PathBuf::from("etc/app.conf"),
                expected: Fingerprint::Absent,
                found: Fingerprint::read(&lower.join("etc/app.conf")).unwrap(),
            },
            Drift {
                path: PathBuf::from("link"),
                expected: Fingerprint::Absent,
                found: Fingerprint::read(&lower.join("link")).unwrap(),
            },
        ];
        let rescue_dir = t.path().join("rescue");
        let problems = rescue(&lower, &drifted, &rescue_dir).unwrap();
        assert!(problems.is_empty(), "{problems:?}");

        assert_eq!(
            fs::read_to_string(rescue_dir.join("etc/app.conf")).unwrap(),
            "v3-human-hotfix"
        );
        assert_eq!(
            fs::read_link(rescue_dir.join("link")).unwrap(),
            PathBuf::from("/new")
        );
    }
}
