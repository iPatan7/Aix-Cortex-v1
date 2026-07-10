//! Transactional filesystem execution built on kernel OverlayFS.
//!
//! A [`Transaction`] mounts an overlay of one or more read-only lower layers
//! with a writable upper layer. Every write performed through the merged view
//! lands in the upper layer, so the whole change-set can be committed
//! (kept, optionally merged back into the lower layer) or rolled back
//! (discarded) as a unit.
//!
//! Layout follows the standard kernel OverlayFS model:
//! - `lower`  – immutable base (e.g. `/` or an extracted rootfs)
//! - `upper`  – persistent copy-on-write layer holding all changes
//! - `work`   – kernel scratch space, must live on the same fs as `upper`
//! - `merged` – the unified mount point commands operate on
//!
//! Requires CAP_SYS_ADMIN (or a user namespace owning the mount namespace).

use anyhow::{bail, Context, Result};
use nix::mount::{mount, umount2, MntFlags, MsFlags};
use std::fs;
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// Host directories bind-mounted into the merged view so chrooted commands
/// (package managers, `nginx -t`, ...) have a functional system environment.
const SYSTEM_BINDS: &[&str] = &["proc", "sys", "dev", "run"];

/// A transactional OverlayFS sandbox.
///
/// The overlay is mounted on construction and unmounted on [`commit`],
/// [`rollback`] or drop.
///
/// [`commit`]: Transaction::commit
/// [`rollback`]: Transaction::rollback
pub struct Transaction {
    lower: Vec<PathBuf>,
    upper: PathBuf,
    work: PathBuf,
    merged: PathBuf,
    binds: Vec<PathBuf>,
    use_system_binds: bool,
    mounted: bool,
}

impl Transaction {
    /// Mount an overlay and return the live transaction.
    ///
    /// `lower` order matters: the first entry is the topmost lower layer.
    /// All directories are created if missing. `upper` and `work` must be on
    /// the same filesystem.
    pub fn new(lower: &[&Path], upper: &Path, work: &Path, merged: &Path) -> Result<Self> {
        if lower.is_empty() {
            bail!("at least one lower directory is required");
        }
        for dir in lower.iter().copied().chain([upper, work, merged]) {
            fs::create_dir_all(dir).with_context(|| format!("failed to create {dir:?}"))?;
        }

        // GAP 7: the upper layer holds every byte a command writes. If it sits
        // on tmpfs (the old default was /run), a large `apt-get install`
        // writes the whole package into RAM and can OOM the host. Refuse to
        // open a transaction whose upper dir is on tmpfs unless it has been
        // explicitly opted into, and warn when free space is dangerously low.
        preflight_upper(upper)?;

        let lower: Vec<PathBuf> = lower.iter().map(|p| p.to_path_buf()).collect();
        Self::mount_overlay(&lower, upper, work, merged)?;

        Ok(Self {
            lower,
            upper: upper.to_path_buf(),
            work: work.to_path_buf(),
            merged: merged.to_path_buf(),
            binds: Vec::new(),
            use_system_binds: false,
            mounted: true,
        })
    }

    fn mount_overlay(lower: &[PathBuf], upper: &Path, work: &Path, merged: &Path) -> Result<()> {
        let mut lower_spec = String::new();
        for (i, dir) in lower.iter().enumerate() {
            if i > 0 {
                lower_spec.push(':');
            }
            lower_spec.push_str(dir.to_str().context("non-UTF-8 lower path")?);
        }
        let options = format!(
            "lowerdir={},upperdir={},workdir={}",
            lower_spec,
            upper.to_str().context("non-UTF-8 upper path")?,
            work.to_str().context("non-UTF-8 work path")?,
        );

        mount(
            None::<&str>,
            merged,
            Some("overlay"),
            MsFlags::empty(),
            Some(options.as_str()),
        )
        .with_context(|| format!("overlay mount on {merged:?} failed (options: {options})"))?;

        // Detach from any shared peer group (systemd makes mounts shared by
        // default). Without this, binds of shared trees into the sandbox
        // create peer links, and tearing them down propagates unmounts back
        // to this overlay — or worse, to the host's own mounts.
        make_private(merged)
    }

    /// The unified view where commands operate.
    pub fn merged_path(&self) -> &Path {
        &self.merged
    }

    /// The copy-on-write layer holding this transaction's changes.
    pub fn upper_path(&self) -> &Path {
        &self.upper
    }

    /// Bind-mount `/proc`, `/sys`, `/dev` and `/run` into the merged view so
    /// that [`run_in_root`](Transaction::run_in_root) can execute package
    /// managers and daemons that expect a live system.
    pub fn bind_system_dirs(&mut self) -> Result<()> {
        self.use_system_binds = true;
        for name in SYSTEM_BINDS {
            let src = Path::new("/").join(name);
            if !src.is_dir() {
                continue;
            }
            let target = self.merged.join(name);
            fs::create_dir_all(&target)?;
            mount(
                Some(src.as_path()),
                &target,
                None::<&str>,
                MsFlags::MS_BIND | MsFlags::MS_REC,
                None::<&str>,
            )
            .with_context(|| format!("bind mount {src:?} -> {target:?} failed"))?;
            // Sever peer links to the host mounts before anything can be
            // unmounted, so teardown of the sandbox never propagates out.
            make_private(&target)?;
            self.binds.push(target);
        }
        Ok(())
    }

    /// Execute a shell command with the merged mount as the working
    /// directory. Paths in `cmd` resolve against the host root; use
    /// [`run_in_root`](Transaction::run_in_root) to confine absolute paths to
    /// the transaction.
    pub fn run_command(&self, cmd: &str) -> Result<Output> {
        Command::new("/bin/sh")
            .arg("-c")
            .arg(cmd)
            .current_dir(&self.merged)
            .output()
            .with_context(|| format!("failed to spawn `{cmd}` in {:?}", self.merged))
    }

    /// Execute a shell command chrooted into the merged view, so absolute
    /// paths (`/etc/nginx/nginx.conf`, `/usr/sbin/nginx`, ...) resolve inside
    /// the transaction and every write is captured by the upper layer.
    ///
    /// Defense in depth: after the chroot, a Landlock allowlist is enforced on
    /// the command (see [`cortex_sandbox`]). The chroot already stops the
    /// command from *naming* host paths outside the sandbox; Landlock closes
    /// the remaining gap — the `/proc`, `/sys`, `/dev`, `/run` trees bound in
    /// from the host for functionality — by denying writes to them. So even a
    /// command that runs as root inside the sandbox cannot reach out and
    /// modify the host through a bind mount. On a kernel without Landlock the
    /// namespace + overlay isolation still holds; confinement is skipped
    /// rather than failing the operation.
    pub fn run_in_root(&self, cmd: &str) -> Result<Output> {
        let root = self.merged.clone();
        let mut command = Command::new("/bin/sh");
        command.arg("-c").arg(cmd);
        unsafe {
            command.pre_exec(move || {
                // 1. Enter the sandbox filesystem.
                nix::unistd::chroot(root.as_path()).map_err(std::io::Error::from)?;
                std::env::set_current_dir("/")?;

                // 2. Confine within it, now that `/usr`, `/etc`, `/proc`, ...
                //    resolve to the sandbox's own view. Enforcement is on this
                //    (single) thread, and irreversible, so it must be the last
                //    thing before exec — which it is.
                let allow = cortex_sandbox::Allowlist::sandbox_interior(SYSTEM_BINDS);
                match allow.enforce() {
                    // Enforced or partially enforced: the restriction is live.
                    Ok(_) => {}
                    // A real ruleset failure on a kernel that has Landlock is a
                    // genuine error — fail the command rather than run it
                    // unconfined and pretend it was contained.
                    Err(e) => {
                        return Err(std::io::Error::other(format!(
                            "sandbox confinement failed: {e}"
                        )));
                    }
                }
                Ok(())
            });
        }
        command
            .output()
            .with_context(|| format!("failed to spawn `{cmd}` chrooted in {:?}", self.merged))
    }

    /// Snapshot the upper layer into `snapshot_dir` using `cp -al`
    /// (hardlink farm — effectively instant and near-zero extra space).
    pub fn snapshot(&self, snapshot_dir: &Path) -> Result<()> {
        if snapshot_dir.exists() {
            bail!("snapshot target {snapshot_dir:?} already exists");
        }
        if let Some(parent) = snapshot_dir.parent() {
            fs::create_dir_all(parent)?;
        }
        let status = Command::new("cp")
            .arg("-al")
            .arg(&self.upper)
            .arg(snapshot_dir)
            .status()
            .context("failed to spawn cp -al")?;
        if !status.success() {
            bail!(
                "cp -al {:?} {snapshot_dir:?} exited with {status}",
                self.upper
            );
        }
        Ok(())
    }

    /// Discard the current upper layer and replace it with `snapshot_dir`
    /// (created by [`snapshot`](Transaction::snapshot)), then remount.
    /// The snapshot directory is consumed (renamed into place — O(1)).
    pub fn rollback_to_snapshot(&mut self, snapshot_dir: &Path) -> Result<()> {
        if !snapshot_dir.is_dir() {
            bail!("snapshot {snapshot_dir:?} does not exist");
        }
        self.unmount_all()?;

        fs::remove_dir_all(&self.upper)
            .with_context(|| format!("failed to remove upper {:?}", self.upper))?;
        fs::rename(snapshot_dir, &self.upper)
            .with_context(|| format!("failed to restore snapshot {snapshot_dir:?}"))?;

        // workdir must be pristine for the new mount
        let _ = fs::remove_dir_all(&self.work);
        fs::create_dir_all(&self.work)?;

        Self::mount_overlay(&self.lower, &self.upper, &self.work, &self.merged)?;
        self.mounted = true;
        if self.use_system_binds {
            self.bind_system_dirs()?;
        }
        Ok(())
    }

    /// Unmount the overlay, keeping the upper layer intact. When
    /// `merge_into_lower` is true, the upper layer is additionally merged
    /// into the topmost lower layer, applying the transaction to the real
    /// filesystem (whiteouts delete the corresponding lower entries).
    pub fn commit(mut self, merge_into_lower: bool) -> Result<()> {
        self.unmount_all()?;
        if merge_into_lower {
            let dest = self.lower.first().expect("checked in new()").clone();
            merge_layer(&self.upper, &dest)
                .with_context(|| format!("failed to merge {:?} into {dest:?}", self.upper))?;
        }
        Ok(())
    }

    /// Unmount the overlay and delete the upper layer, discarding every
    /// change made inside the transaction.
    pub fn rollback(mut self) -> Result<()> {
        self.unmount_all()?;
        fs::remove_dir_all(&self.upper)
            .with_context(|| format!("failed to remove upper {:?}", self.upper))?;
        fs::create_dir_all(&self.upper)?;
        Ok(())
    }

    fn unmount_all(&mut self) -> Result<()> {
        for bind in self.binds.drain(..).rev() {
            let _ = umount2(&bind, MntFlags::MNT_DETACH);
        }
        if self.mounted {
            match umount2(&self.merged, MntFlags::MNT_DETACH) {
                // EINVAL: not a mount point — already unmounted out from
                // under us; the goal state is reached either way.
                Ok(()) | Err(nix::errno::Errno::EINVAL) => {}
                Err(e) => {
                    return Err(e).with_context(|| format!("failed to unmount {:?}", self.merged));
                }
            }
            self.mounted = false;
        }
        Ok(())
    }
}

impl Drop for Transaction {
    fn drop(&mut self) {
        for bind in self.binds.drain(..).rev() {
            let _ = umount2(&bind, MntFlags::MNT_DETACH);
        }
        if self.mounted {
            let _ = umount2(&self.merged, MntFlags::MNT_DETACH);
        }
    }
}

/// Convert a mount (and everything replicated under it) to private
/// propagation so sandbox teardown cannot leak unmount events to the host.
fn make_private(target: &Path) -> Result<()> {
    mount(
        None::<&str>,
        target,
        None::<&str>,
        MsFlags::MS_PRIVATE | MsFlags::MS_REC,
        None::<&str>,
    )
    .with_context(|| format!("failed to make {target:?} private"))
}

/// OverlayFS marks a deleted lower entry with a 0:0 character device.
pub(crate) fn is_whiteout(meta: &fs::Metadata) -> bool {
    meta.file_type().is_char_device() && meta.rdev() == 0
}

fn remove_existing(target: &Path) -> Result<()> {
    match fs::symlink_metadata(target) {
        Ok(m) if m.is_dir() => fs::remove_dir_all(target)?,
        Ok(_) => fs::remove_file(target)?,
        Err(_) => {}
    }
    Ok(())
}

/// Apply an upper layer onto a lower directory: whiteouts delete, everything
/// else overwrites. Opaque-directory xattrs are not interpreted; directories
/// are merged rather than replaced. Also the undo primitive: applying a
/// journal entry's inverse layer (see [`crate::journal`]) reverses a commit.
pub(crate) fn merge_layer(src: &Path, dst: &Path) -> Result<()> {
    for entry in fs::read_dir(src).with_context(|| format!("failed to read {src:?}"))? {
        let entry = entry?;
        let meta = entry.metadata()?;
        let target = dst.join(entry.file_name());

        if is_whiteout(&meta) {
            remove_existing(&target)?;
        } else if meta.is_dir() {
            if fs::symlink_metadata(&target)
                .map(|m| !m.is_dir())
                .unwrap_or(false)
            {
                remove_existing(&target)?;
            }
            if fs::symlink_metadata(&target).is_err() {
                fs::create_dir(&target)?;
            }
            fs::set_permissions(&target, meta.permissions())?;
            let _ = std::os::unix::fs::chown(&target, Some(meta.uid()), Some(meta.gid()));
            merge_layer(&entry.path(), &target)?;
        } else if meta.file_type().is_symlink() {
            let link = fs::read_link(entry.path())?;
            remove_existing(&target)?;
            std::os::unix::fs::symlink(link, &target)?;
        } else if meta.is_file() {
            remove_existing(&target)?;
            fs::copy(entry.path(), &target)
                .with_context(|| format!("failed to copy {:?} -> {target:?}", entry.path()))?;
            let _ = std::os::unix::fs::chown(&target, Some(meta.uid()), Some(meta.gid()));
        }
        // sockets/fifos/device nodes other than whiteouts are skipped
        else if !meta.is_file() && !is_whiteout(&meta) {
            // GAP 7: previously skipped silently. A package that ships a
            // device node or socket would be committed *incompletely* with no
            // warning. merge_layer cannot faithfully reproduce these, so it
            // reports them rather than pretending the merge was complete.
            eprintln!(
                "[cortex] warning: {:?} is a special file (device/socket/fifo) and \
                 was not merged; this operation is not fully captured",
                entry.path()
            );
        }
    }
    Ok(())
}

/// How much free space the upper layer should have before we let a command
/// write into it. Not a hard limit on the write — the kernel enforces that —
/// but a floor below which opening a transaction is asking for a wedged host.
const MIN_UPPER_FREE_BYTES: u64 = 256 * 1024 * 1024; // 256 MiB

/// Refuse a transaction whose upper layer is on tmpfs (writes go to RAM), and
/// warn when free space on the upper filesystem is dangerously low.
///
/// Set `CORTEX_ALLOW_TMPFS_UPPER=1` to override the tmpfs refusal — for a
/// deliberately tiny, RAM-backed transaction where the caller knows the write
/// is small. The default is safety, because the failure mode is an OOM that
/// takes the whole box down, not a clean error.
fn preflight_upper(upper: &Path) -> Result<()> {
    let stat = nix::sys::statvfs::statvfs(upper)
        .with_context(|| format!("failed to statvfs upper dir {upper:?}"))?;

    let free = stat.blocks_available() as u64 * stat.fragment_size() as u64;

    // TMPFS_MAGIC. statfs would give f_type directly; statvfs does not carry
    // it portably, so we detect tmpfs by its hallmark: the backing store is
    // RAM, which shows up as the mount being on a tmpfs. We check via the
    // filesystem type reported by the mount, read from /proc/mounts.
    if is_tmpfs(upper) && std::env::var_os("CORTEX_ALLOW_TMPFS_UPPER").is_none() {
        bail!(
            "refusing to open a transaction with its upper layer on tmpfs ({}): \
             every byte a command writes would go to RAM, and a large install \
             can OOM the host.\n\
             Use a disk-backed --state-dir (e.g. /var/lib/cortex/transactions), \
             or set CORTEX_ALLOW_TMPFS_UPPER=1 if you know the write is small.",
            upper.display()
        );
    }

    if free < MIN_UPPER_FREE_BYTES {
        eprintln!(
            "[cortex] warning: only {} MiB free on the filesystem holding {}; \
             a large change may fail mid-write",
            free / (1024 * 1024),
            upper.display()
        );
    }
    Ok(())
}

/// Whether `path` is on a tmpfs mount, by consulting /proc/mounts for the
/// longest mount-point prefix. Best-effort: if /proc/mounts is unreadable we
/// assume not-tmpfs rather than blocking (the free-space warning still fires).
fn is_tmpfs(path: &Path) -> bool {
    let canonical = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let mounts = match fs::read_to_string("/proc/mounts") {
        Ok(m) => m,
        Err(_) => return false,
    };
    let mut best: Option<(usize, bool)> = None;
    for line in mounts.lines() {
        let mut it = line.split_whitespace();
        let (Some(_dev), Some(mount_point), Some(fstype)) = (it.next(), it.next(), it.next())
        else {
            continue;
        };
        if canonical.starts_with(mount_point) {
            let len = mount_point.len();
            if best.map(|(l, _)| len > l).unwrap_or(true) {
                best = Some((len, fstype == "tmpfs" || fstype == "ramfs"));
            }
        }
    }
    best.map(|(_, is)| is).unwrap_or(false)
}
