//! Defense-in-depth confinement for commands run inside a transaction.
//!
//! The [`Transaction`](cortex_core::transaction) already isolates the
//! filesystem: a private mount namespace and an overlay, so a command's writes
//! land in the upper layer and never touch the host until commit. That is the
//! primary containment.
//!
//! This crate is the *second* layer: it restricts what a command can reach
//! even inside the sandbox, so a template that runs an untrusted binary cannot
//! wander outside the paths its operation legitimately needs. Landlock
//! (kernel ≥ 5.13) enforces a filesystem allowlist that no capability can
//! escape — it applies to root too.
//!
//! Design choices that matter for a safety tool:
//!
//! - **Degrade, don't fail closed on absence.** On a kernel without Landlock,
//!   confinement reports `Unsupported` rather than refusing to run. The
//!   transaction's namespace isolation still holds; we do not turn "extra
//!   defense unavailable" into "cannot operate."
//! - **Fail closed on error.** If Landlock is present but a ruleset cannot be
//!   enforced, that is a real failure and is returned as one.
//! - **Apply in the child, before exec.** Confinement is per-thread and
//!   irreversible; it must be the last thing before `execve`, in the forked
//!   child, never in the parent.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum ConfineError {
    #[error("landlock ruleset could not be enforced: {0}")]
    Ruleset(String),
}

/// The outcome of applying confinement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Confinement {
    /// The kernel enforced the full ruleset.
    Enforced,
    /// The kernel enforced a subset (older Landlock ABI). Still real, just
    /// not every access right was available to restrict.
    PartiallyEnforced,
    /// The kernel has no Landlock support. The transaction's namespace
    /// isolation remains; this layer is simply absent.
    Unsupported,
}

impl Confinement {
    /// True when at least some access control was applied.
    pub fn is_active(self) -> bool {
        matches!(self, Self::Enforced | Self::PartiallyEnforced)
    }
}

/// A filesystem allowlist to enforce on the current thread before `execve`.
///
/// Paths are granted the rights a DevOps command legitimately needs: read and
/// execute broadly (so binaries and libraries resolve), but write only under
/// the roots the operation is expected to touch. Everything else is denied.
#[derive(Debug, Clone, Default)]
pub struct Allowlist {
    read_exec: Vec<std::path::PathBuf>,
    read_write: Vec<std::path::PathBuf>,
}

impl Allowlist {
    /// The baseline a chrooted command needs to function: read+exec on the
    /// standard binary and library trees, read+write on the transaction's own
    /// scratch. Callers extend it with the paths their operation needs.
    pub fn baseline() -> Self {
        let mut a = Self::default();
        for p in ["/usr", "/bin", "/sbin", "/lib", "/lib64", "/etc"] {
            a.read_exec.push(p.into());
        }
        for p in ["/tmp", "/run", "/var/tmp", "/dev"] {
            a.read_write.push(p.into());
        }
        a
    }

    /// The allowlist for a command running **inside** a transaction, after the
    /// chroot into the merged view. Applied here, `/usr`, `/etc`, ... are the
    /// sandbox's own copy-on-write files, so writes to them are captured and
    /// reversible — they may be written freely.
    ///
    /// The point of confinement in this position is the *bind mounts*. To give
    /// a chrooted command a working system, `/proc`, `/sys`, `/dev` and `/run`
    /// are bound in from the host; those are the one surface through which a
    /// command in the sandbox could reach out and affect the host (writing a
    /// `/proc/sysrq-trigger`, a `/sys` attribute, a `/dev` node, a socket under
    /// `/run`). This allowlist grants read+write on the writable *filesystem*
    /// roots and read-only on the bound host trees, so the sandbox stays a
    /// sandbox even for a command that tries to escape through them.
    ///
    /// `bind_names` are the top-level directories bound in from the host
    /// (relative names like `proc`, `sys`), granted read-only here.
    pub fn sandbox_interior(bind_names: &[&str]) -> Self {
        let mut a = Self::default();
        // Read + execute on everything: binaries and libraries must resolve,
        // and reads never affect the host.
        a.read_exec.push("/".into());
        // Read + write on the writable filesystem roots. These are overlay
        // upper-layer paths inside the chroot, so writes are captured.
        for p in [
            "/etc", "/usr", "/var", "/opt", "/home", "/root", "/srv", "/tmp", "/bin", "/sbin",
            "/lib", "/lib64",
        ] {
            a.read_write.push(p.into());
        }
        // The bound host dirs get NO write grant — only the read-exec on `/`
        // above covers them. A write attempt under these is denied by the
        // allowlist even though the command runs as root.
        let _ = bind_names; // named for the doc contract; denial is by omission
        a
    }

    /// Grant read + execute under `path`.
    pub fn allow_read_exec(mut self, path: impl Into<std::path::PathBuf>) -> Self {
        self.read_exec.push(path.into());
        self
    }

    /// Grant read + write under `path`.
    pub fn allow_read_write(mut self, path: impl Into<std::path::PathBuf>) -> Self {
        self.read_write.push(path.into());
        self
    }

    /// Enforce this allowlist on the current thread. After this returns
    /// `Enforced`/`PartiallyEnforced`, the thread cannot reach any path
    /// outside the lists, and the restriction cannot be undone.
    ///
    /// Missing paths in the lists are skipped, not fatal: `/lib64` does not
    /// exist on every distro, and refusing to confine because an allowed path
    /// is absent would be worse than confining to the paths that do exist.
    pub fn enforce(&self) -> Result<Confinement, ConfineError> {
        use landlock::{
            Access, AccessFs, PathBeneath, PathFd, Ruleset, RulesetAttr, RulesetCreatedAttr,
            RulesetStatus, ABI,
        };

        let abi = ABI::V1;
        let ruleset = Ruleset::default()
            .handle_access(AccessFs::from_all(abi))
            .map_err(|e| ConfineError::Ruleset(e.to_string()))?
            .create()
            .map_err(|e| ConfineError::Ruleset(e.to_string()))?;

        let read_exec = AccessFs::from_read(abi);
        let read_write = AccessFs::from_all(abi);

        let mut ruleset = ruleset;
        for (paths, access) in [(&self.read_exec, read_exec), (&self.read_write, read_write)] {
            for path in paths {
                let fd = match PathFd::new(path) {
                    Ok(fd) => fd,
                    Err(_) => continue, // absent path: skip, don't fail
                };
                ruleset = ruleset
                    .add_rule(PathBeneath::new(fd, access))
                    .map_err(|e| ConfineError::Ruleset(e.to_string()))?;
            }
        }

        let status = ruleset
            .restrict_self()
            .map_err(|e| ConfineError::Ruleset(e.to_string()))?;

        Ok(match status.ruleset {
            RulesetStatus::FullyEnforced => Confinement::Enforced,
            RulesetStatus::PartiallyEnforced => Confinement::PartiallyEnforced,
            RulesetStatus::NotEnforced => Confinement::Unsupported,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn baseline_lists_the_expected_roots() {
        let a = Allowlist::baseline();
        assert!(a.read_exec.iter().any(|p| p.ends_with("usr")));
        assert!(a.read_write.iter().any(|p| p.ends_with("tmp")));
    }

    #[test]
    fn builder_extends_the_lists() {
        let a = Allowlist::baseline()
            .allow_read_write("/var/lib/cortex")
            .allow_read_exec("/opt/app");
        assert!(a.read_write.iter().any(|p| p.ends_with("cortex")));
        assert!(a.read_exec.iter().any(|p| p.ends_with("app")));
    }

    /// The interior allowlist grants no write to the bound host trees, so a
    /// `/proc`, `/sys`, `/dev`, `/run` write is denied by omission.
    #[test]
    fn interior_allowlist_denies_writes_to_bound_host_trees() {
        let a = Allowlist::sandbox_interior(&["proc", "sys", "dev", "run"]);
        // Read-exec covers `/`, so reads work everywhere...
        assert!(a.read_exec.iter().any(|p| p == std::path::Path::new("/")));
        // ...but no write grant names a bound host tree.
        for host in ["/proc", "/sys", "/dev", "/run"] {
            assert!(
                !a.read_write.iter().any(|p| p == std::path::Path::new(host)),
                "{host} must not be writable inside the sandbox"
            );
        }
        // The writable filesystem roots that overlay captures ARE granted.
        assert!(a
            .read_write
            .iter()
            .any(|p| p == std::path::Path::new("/etc")));
    }

    /// The real thing: enforce a write allowlist, then prove a write outside it
    /// is actually denied by the kernel. Runs in a forked child because the
    /// restriction is irreversible and must not leak into the test process.
    ///
    /// On a kernel without Landlock this reports `Unsupported` and the write is
    /// *not* denied — which the test accepts, because the contract is "confine
    /// where the kernel can," not "refuse to run on old kernels."
    #[test]
    fn enforcement_actually_denies_an_out_of_list_write() {
        use std::io::Write;
        let dir = std::env::temp_dir().join(format!("cortex-ll-{}", std::process::id()));
        let allowed = dir.join("allowed");
        let denied = dir.join("denied");
        std::fs::create_dir_all(&allowed).unwrap();
        std::fs::create_dir_all(&denied).unwrap();

        // Fork: the child enforces and reports what it could write.
        match unsafe { libc_fork() } {
            0 => {
                // Child. Allow read-exec on `/`, read-write only under `allowed`.
                let list = Allowlist::default()
                    .allow_read_exec("/")
                    .allow_read_write(&allowed);
                let outcome = list.enforce().expect("enforce must not error");

                let wrote_allowed = std::fs::write(allowed.join("ok"), b"x").is_ok();
                let wrote_denied = std::fs::write(denied.join("no"), b"x").is_ok();

                // Encode the result in the exit code for the parent to read.
                // bit0 = wrote_allowed, bit1 = wrote_denied, bit2 = enforced.
                let code = (wrote_allowed as i32)
                    | ((wrote_denied as i32) << 1)
                    | ((outcome.is_active() as i32) << 2);
                std::io::stdout().flush().ok();
                unsafe { libc_exit(code) };
            }
            child if child > 0 => {
                let code = wait_exit_code(child);
                let wrote_allowed = code & 1 != 0;
                let wrote_denied = code & 2 != 0;
                let enforced = code & 4 != 0;

                let _ = std::fs::remove_dir_all(&dir);

                // A legitimate write always succeeds.
                assert!(wrote_allowed, "write to the allowed path must succeed");
                if enforced {
                    // This is the property that matters: an out-of-list write
                    // is actually denied by the kernel.
                    assert!(
                        !wrote_denied,
                        "Landlock enforced but a denied write succeeded — confinement is a no-op"
                    );
                } else {
                    eprintln!(
                        "kernel has no Landlock; enforcement skipped (expected on old kernels)"
                    );
                }
            }
            _ => panic!("fork failed"),
        }
    }

    // Minimal libc bindings so this crate needs no libc dependency for one
    // test. fork/waitpid/_exit are the async-signal-safe primitives we need.
    extern "C" {
        fn fork() -> i32;
        fn _exit(code: i32) -> !;
        fn waitpid(pid: i32, status: *mut i32, options: i32) -> i32;
    }
    unsafe fn libc_fork() -> i32 {
        fork()
    }
    unsafe fn libc_exit(code: i32) -> ! {
        _exit(code)
    }
    fn wait_exit_code(pid: i32) -> i32 {
        let mut status: i32 = 0;
        unsafe { waitpid(pid, &mut status as *mut i32, 0) };
        // WEXITSTATUS: (status >> 8) & 0xff
        (status >> 8) & 0xff
    }
}
