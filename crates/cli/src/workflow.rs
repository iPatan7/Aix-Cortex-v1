//! Transactional DevOps workflows built on [`Transaction`].
//!
//! Each workflow stages its changes in an OverlayFS transaction, verifies
//! them inside the sandbox, and only merges them into the real filesystem
//! when verification passes. Any failure triggers an automatic rollback that
//! leaves the host untouched.
//!
//! Every commit first arms its inverse in the persistent [`journal`]
//! (saga discipline: reversible **and** audited), so a committed workflow
//! can be reverted later with `cortex workflow undo`.
//!
//! Workflows are *hybrid*: each has a structured manual trigger
//! (`cortex workflow safe-symlink-swap --link ... --target ...`) and all of
//! them can be reached through `cortex workflow llm "description"`, where an
//! LLM (see [`crate::llm`]) turns the description into the shell command
//! that then runs under the exact same transaction/verify/journal rules.

use crate::llm::LlmClient;
use anyhow::{bail, Context, Result};
use cortex_core::journal::{Journal, DEFAULT_JOURNAL_DIR};
use cortex_core::transaction::Transaction;
use cortex_core::ui;
use cortex_policy::PolicyToken;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Run a shell predicate; true when it exits 0. Used to prove that a command
/// actually took effect, rather than merely exiting successfully.
fn predicate_holds(cmd: &str) -> Result<bool> {
    Ok(Command::new("/bin/sh")
        .args(["-c", cmd])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .with_context(|| format!("failed to run verifier `{cmd}`"))?
        .success())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Manager {
    Apt,
    Pip,
    Npm,
}

impl std::str::FromStr for Manager {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> Result<Self> {
        match s {
            "apt" => Ok(Self::Apt),
            "pip" => Ok(Self::Pip),
            "npm" => Ok(Self::Npm),
            other => bail!("unknown package manager `{other}` (expected apt, pip or npm)"),
        }
    }
}

pub enum WorkflowKind {
    /// Apply a config-editing command, verify with `<service> -t`.
    SafeConfig { service: String, cmd: String },
    /// `apt-get install` a package, verify the binary reports a version.
    SafeInstall { package: String },
    /// Edit any text file with an arbitrary command; verify by file type.
    SafeFileEdit { file: PathBuf, cmd: String },
    // NOTE: there is deliberately no database-migration workflow. A
    // migration's undo is inherently lossy — `ALTER TABLE ... DROP COLUMN`
    // does not restore the dropped data — so presenting it as "reversible"
    // would be a lie, and this product's entire claim is that it does not
    // lie about reversibility. A future snapshot-backed template (dump the
    // affected tables, restore from the dump as a verified inverse) could be
    // genuinely reversible; until then, there is nothing here.
    /// Upgrade a package via apt/pip/npm; undo restores the old files.
    SafeDependencyUpgrade { manager: Manager, package: String },
    /// Append a crontab entry; undo restores the previous crontab.
    SafeCronInstall { user: String, entry: String },
    /// Atomically repoint a symlink; undo restores the old target.
    SafeSymlinkSwap { link: PathBuf, target: PathBuf },
    /// Run a registry template: a human-written (forward, inverse, verify)
    /// triple with parameters bound. No overlay — the effect lives outside
    /// the filesystem — so reversibility is the *verified* compensation.
    Template(cortex_registry::Bound),
    /// Run something cortex cannot reverse, with explicit consent and a
    /// policy token. Journaled as irreversible; `undo` refuses it out loud.
    Irreversible { cmd: String, token: PolicyToken },
    /// Control a systemd unit, recording the true inverse of the transition.
    SafeService { op: ServiceOp, service: String },
    /// Natural-language trigger: an LLM generates the shell command.
    Llm { description: String },
}

/// A systemd transition and its inverse. `start`'s inverse is `stop` only
/// when the unit was not already running — otherwise the command changed
/// nothing and an "inverse" would wrongly stop a unit cortex never started.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServiceOp {
    Start,
    Stop,
    Restart,
    Enable,
    Disable,
}

impl ServiceOp {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Start => "start",
            Self::Stop => "stop",
            Self::Restart => "restart",
            Self::Enable => "enable",
            Self::Disable => "disable",
        }
    }
}

impl std::str::FromStr for ServiceOp {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> Result<Self> {
        match s {
            "start" => Ok(Self::Start),
            "stop" => Ok(Self::Stop),
            "restart" => Ok(Self::Restart),
            "enable" => Ok(Self::Enable),
            "disable" => Ok(Self::Disable),
            other => bail!(
                "unknown service op `{other}` (expected start, stop, restart, enable or disable)"
            ),
        }
    }
}

pub struct Workflow {
    kind: WorkflowKind,
    lower: PathBuf,
    state_dir: PathBuf,
    journal_dir: PathBuf,
}

impl Workflow {
    pub fn safe_config(service: impl Into<String>, cmd: impl Into<String>) -> Self {
        Self::with_kind(WorkflowKind::SafeConfig {
            service: service.into(),
            cmd: cmd.into(),
        })
    }

    pub fn safe_install(package: impl Into<String>) -> Self {
        Self::with_kind(WorkflowKind::SafeInstall {
            package: package.into(),
        })
    }

    pub fn safe_file_edit(file: impl Into<PathBuf>, cmd: impl Into<String>) -> Self {
        Self::with_kind(WorkflowKind::SafeFileEdit {
            file: file.into(),
            cmd: cmd.into(),
        })
    }

    pub fn safe_dependency_upgrade(manager: Manager, package: impl Into<String>) -> Self {
        Self::with_kind(WorkflowKind::SafeDependencyUpgrade {
            manager,
            package: package.into(),
        })
    }

    pub fn safe_cron_install(user: impl Into<String>, entry: impl Into<String>) -> Self {
        Self::with_kind(WorkflowKind::SafeCronInstall {
            user: user.into(),
            entry: entry.into(),
        })
    }

    pub fn safe_symlink_swap(link: impl Into<PathBuf>, target: impl Into<PathBuf>) -> Self {
        Self::with_kind(WorkflowKind::SafeSymlinkSwap {
            link: link.into(),
            target: target.into(),
        })
    }

    /// Run a bound registry template. This is the only way to run a
    /// host-side operation reversibly: the inverse and its post-condition
    /// come from the registry, not from a caller or a model.
    pub fn template(bound: cortex_registry::Bound) -> Self {
        Self::with_kind(WorkflowKind::Template(bound))
    }

    /// Run a command cortex cannot reverse. The [`PolicyToken`] can only be
    /// obtained from a real policy verdict, so a caller cannot silently opt
    /// itself out of reversibility.
    ///
    /// A seam for v1.1: no command constructs this yet (there is no verb that
    /// runs a raw irreversible command), but the plumbing — policy gate,
    /// token, honest journaling, undo refusal — is in place and tested, so
    /// wiring a `cortex run-unsafe` later is a CLI change, not an engine one.
    #[allow(dead_code)]
    pub fn irreversible(cmd: impl Into<String>, token: PolicyToken) -> Self {
        Self::with_kind(WorkflowKind::Irreversible {
            cmd: cmd.into(),
            token,
        })
    }

    pub fn safe_service(op: ServiceOp, service: impl Into<String>) -> Self {
        Self::with_kind(WorkflowKind::SafeService {
            op,
            service: service.into(),
        })
    }

    /// Construct directly from an LLM description. The `try` path builds its
    /// workflow through `from_plan` instead (the model selects a template, it
    /// does not author one), so this direct constructor is currently unused —
    /// kept as the seam for a future `cortex llm "…"` verb.
    #[allow(dead_code)]
    pub fn llm(description: impl Into<String>) -> Self {
        Self::with_kind(WorkflowKind::Llm {
            description: description.into(),
        })
    }

    fn with_kind(kind: WorkflowKind) -> Self {
        Self {
            kind,
            lower: PathBuf::from("/"),
            // Must live on a different filesystem than `lower`: the kernel
            // rejects overlay mounts whose upper/work dirs sit inside a
            // lower layer ("overlapping layers", EBUSY, kernel >= 5.2).
            // /run is tmpfs on systemd hosts, so it is safe with lower=/;
            // persistence comes from commit() merging into the lower layer.
            state_dir: PathBuf::from("/run/cortex/transactions"),
            journal_dir: PathBuf::from(DEFAULT_JOURNAL_DIR),
        }
    }

    /// Override the read-only base layer (defaults to `/`).
    pub fn lower(mut self, lower: impl Into<PathBuf>) -> Self {
        self.lower = lower.into();
        self
    }

    /// Override where upper/work/merged directories are kept
    /// (defaults to `/run/cortex/transactions`). Must be on a different
    /// filesystem than the lower layer, or the overlay mount is rejected
    /// with "overlapping layers".
    pub fn state_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.state_dir = dir.into();
        self
    }

    /// Override where commit inverses are journaled (defaults to
    /// `/var/lib/cortex/journal`). Must be persistent storage — an undo
    /// journal on tmpfs vanishes exactly when it is needed most.
    pub fn journal_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.journal_dir = dir.into();
        self
    }

    pub fn run(&self) -> Result<()> {
        // Host-side kinds never open an overlay: dockerd and systemd live
        // outside it, so a sandboxed copy of their state would be a lie.
        // Their safety is the journaled compensation instead.
        match &self.kind {
            WorkflowKind::Template(bound) if bound.host_side => return self.run_template(bound),
            WorkflowKind::Irreversible { cmd, token } => return self.run_irreversible(cmd, *token),
            WorkflowKind::SafeService { op, service } => {
                return self.run_safe_service(*op, service)
            }
            _ => {}
        }

        // An LLM-triggered run resolves the command before any sandbox
        // exists, so a failed LLM call costs nothing and falls back cleanly.
        let llm_cmd = match &self.kind {
            WorkflowKind::Llm { description } => Some(resolve_llm_command(description)?),
            _ => None,
        };

        // Confine every sandbox mount to a private mount namespace: the
        // overlay and binds never appear in the host mount table, so no
        // teardown bug can propagate unmounts to the host, and the kernel
        // reaps everything when this process exits. Host-visible changes
        // happen only through commit()'s explicit merge into the lower dir.
        nix::sched::unshare(nix::sched::CloneFlags::CLONE_NEWNS)
            .context("unshare(CLONE_NEWNS) failed (root required)")?;
        nix::mount::mount(
            None::<&str>,
            "/",
            None::<&str>,
            nix::mount::MsFlags::MS_SLAVE | nix::mount::MsFlags::MS_REC,
            None::<&str>,
        )
        .context("failed to make / rslave in the private mount namespace")?;

        let tx_root = self.state_dir.join(uuid::Uuid::new_v4().to_string());
        let upper = tx_root.join("upper");
        let work = tx_root.join("work");
        let merged = tx_root.join("merged");

        let mut tx = Transaction::new(&[self.lower.as_path()], &upper, &work, &merged).context(
            "failed to open transaction (needs root; also note --state-dir must be \
                 on a different filesystem than --lower, e.g. tmpfs when lower is /)",
        )?;
        tx.bind_system_dirs()
            .context("failed to bind system directories into the transaction")?;
        println!("[cortex] transaction opened at {}", merged.display());

        match &self.kind {
            WorkflowKind::SafeConfig { service, cmd } => self.run_safe_config(tx, service, cmd),
            WorkflowKind::SafeInstall { package } => self.run_safe_install(tx, package),
            WorkflowKind::SafeFileEdit { file, cmd } => self.run_safe_file_edit(tx, file, cmd),
            WorkflowKind::SafeDependencyUpgrade { manager, package } => {
                self.run_safe_dependency_upgrade(tx, *manager, package)
            }
            WorkflowKind::SafeCronInstall { user, entry } => {
                self.run_safe_cron_install(tx, user, entry)
            }
            WorkflowKind::SafeSymlinkSwap { link, target } => {
                self.run_safe_symlink_swap(tx, link, target)
            }
            WorkflowKind::Llm { description } => {
                let cmd = llm_cmd.expect("resolved before the sandbox opened");
                self.run_llm(tx, description, &cmd)
            }
            // A filesystem-backed template still needs the overlay.
            WorkflowKind::Template(bound) => self.run_fs_template(tx, bound),
            WorkflowKind::Irreversible { .. } | WorkflowKind::SafeService { .. } => {
                unreachable!("host-side kinds returned before the sandbox opened")
            }
        }
    }

    /// Run a bound registry template whose effects live outside the
    /// filesystem (a container, a unit). The forward command runs on the
    /// host — there is no overlay to roll back — so a failure leaves nothing
    /// journaled, and only a *verified* success arms the compensation.
    ///
    /// Both the forward and the inverse carry human-written post-conditions
    /// from the registry. Cortex proves the forward command took effect
    /// before it commits, and proves the inverse took effect before it calls
    /// an entry undone.
    fn run_template(&self, bound: &cortex_registry::Bound) -> Result<()> {
        println!("[cortex] {}", bound.forward);
        let out = Command::new("/bin/sh")
            .args(["-c", &bound.forward])
            .output()?;
        print!("{}", String::from_utf8_lossy(&out.stdout));
        if !out.status.success() {
            eprint!("{}", String::from_utf8_lossy(&out.stderr));
            bail!(
                "`{}` failed ({}); nothing journaled",
                bound.forward,
                out.status
            );
        }

        // A command that exits 0 without taking effect is exactly the class
        // of lie this system exists to catch. Prove it worked.
        if !predicate_holds(&bound.verify_forward)? {
            bail!(
                "`{}` exited 0 but its post-condition `{}` does not hold; \
                 nothing journaled",
                bound.forward,
                bound.verify_forward
            );
        }
        println!("[cortex] verified: {}", bound.verify_forward);

        let entry = Journal::new(&self.journal_dir).capture_compensation(
            &self.lower,
            &bound.template_id,
            None,
            &bound.forward,
            &bound.inverse,
            &bound.verify_inverse,
            Some(&bound.template_id),
        )?;
        println!(
            "[cortex] committed (entry {}); undo runs `{}` and proves `{}`",
            entry.id, bound.inverse, bound.verify_inverse
        );
        Ok(())
    }

    /// Run a filesystem-backed template inside the overlay: the forward
    /// command and its post-condition both run in the sandbox, so a template
    /// that does not take effect never reaches the real filesystem. The
    /// journal's inverse layer is the undo; the template's `verify_inverse`
    /// is checked after it is applied.
    fn run_fs_template(&self, tx: Transaction, bound: &cortex_registry::Bound) -> Result<()> {
        let staged = (|| {
            run_step(&tx, &bound.forward, "template command")?;
            print_changes(&self.require_changes(&tx)?);
            verify_step(&tx, &bound.verify_forward)
        })();
        if let Err(e) = staged {
            return Err(rolled_back(tx, e));
        }
        self.commit_applied(
            tx,
            &bound.template_id,
            None,
            &bound.forward,
            // The filesystem restore is the undo; the inverse command only
            // exists to satisfy templates that also need a host-side action.
            (bound.inverse != "true").then_some(bound.inverse.as_str()),
            (bound.inverse != "true").then_some(bound.verify_inverse.as_str()),
            Some(&bound.template_id),
        )
    }

    /// Run a command cortex cannot reverse, with the operator's explicit
    /// consent and a policy token proving the policy engine allowed it.
    ///
    /// This is the honest path for anything outside the registry. It is
    /// journaled as `Irreversible` so `cortex status` and the audit log show
    /// it, and `undo` will refuse it rather than pretend.
    fn run_irreversible(&self, cmd: &str, _token: PolicyToken) -> Result<()> {
        println!("[cortex] IRREVERSIBLE: {cmd}");
        let status = Command::new("/bin/sh").args(["-c", cmd]).status()?;
        if !status.success() {
            bail!("`{cmd}` failed ({status})");
        }
        Journal::new(&self.journal_dir).capture_irreversible(&self.lower, cmd)?;
        println!("[cortex] done. This operation cannot be undone; it is recorded in the journal.");
        Ok(())
    }

    /// Drive a systemd unit, journaling the inverse of the transition that
    /// actually happened. A unit already in the target state is NoEffect: we
    /// journal nothing, so a later undo cannot stop a service cortex never
    /// started.
    fn run_safe_service(&self, op: ServiceOp, service: &str) -> Result<()> {
        if !unit_exists(service) {
            bail!("no systemd unit `{service}` (try `cortex workflow safe-install --package {service}` first)");
        }
        let was_active = unit_is(service, "is-active");
        let was_enabled = unit_is(service, "is-enabled");

        let Some(inverse) = service_inverse(op, service, was_active, was_enabled) else {
            println!(
                "[cortex] {service} is already {}; nothing to do (NoEffect, nothing journaled)",
                if matches!(op, ServiceOp::Start) {
                    "running"
                } else {
                    "in the target state"
                }
            );
            return Ok(());
        };

        println!("[cortex] systemctl {} {service}", op.as_str());
        println!("[cortex] compensated by: {inverse}");
        let status = Command::new("systemctl")
            .args([op.as_str(), service])
            .status()?;
        if !status.success() {
            bail!(
                "`systemctl {} {service}` failed ({status}); nothing journaled — \
                 inspect with `systemctl status {service}`",
                op.as_str()
            );
        }
        // A unit that exits immediately reports success from systemctl but is
        // not actually up; treat that as a failed start rather than journal a
        // compensation for something that is not running.
        if matches!(op, ServiceOp::Start | ServiceOp::Restart) && !unit_is(service, "is-active") {
            bail!(
                "`systemctl {} {service}` returned success but the unit is not active; \
                 nothing journaled — inspect with `systemctl status {service}`",
                op.as_str()
            );
        }

        // The inverse's post-condition mirrors the forward one: a `stop` that
        // exits 0 while the unit is still active has not undone anything.
        let verify_inverse = match op {
            ServiceOp::Start => format!("! systemctl is-active --quiet {service}"),
            ServiceOp::Stop => format!("systemctl is-active --quiet {service}"),
            ServiceOp::Restart if was_active => format!("systemctl is-active --quiet {service}"),
            ServiceOp::Restart => format!("! systemctl is-active --quiet {service}"),
            ServiceOp::Enable => format!("! systemctl is-enabled --quiet {service}"),
            ServiceOp::Disable => format!("systemctl is-enabled --quiet {service}"),
        };

        let entry = Journal::new(&self.journal_dir).capture_compensation(
            &self.lower,
            "safe-service",
            Some(service),
            &format!("systemctl {} {service}", op.as_str()),
            &inverse,
            &verify_inverse,
            None,
        )?;
        println!(
            "[cortex] {service} {}ed (journal entry {}); undo runs `{inverse}` and proves `{verify_inverse}`",
            op.as_str(),
            entry.id
        );
        Ok(())
    }

    fn run_safe_config(&self, tx: Transaction, service: &str, cmd: &str) -> Result<()> {
        let staged = (|| {
            run_step(&tx, cmd, "config command")?;
            print_changes(&self.require_changes(&tx)?);
            verify_step(&tx, &format!("{service} -t"))
        })();
        if let Err(e) = staged {
            return Err(rolled_back(tx, e));
        }
        self.commit_applied(tx, "safe-config", Some(service), cmd, None, None, None)?;
        reload_service(service)
    }

    fn run_safe_install(&self, tx: Transaction, package: &str) -> Result<()> {
        let install = format!("DEBIAN_FRONTEND=noninteractive apt-get install -y {package}");
        let staged = (|| {
            run_step(&tx, &install, "install")?;
            verify_step(
                &tx,
                &format!(
                    "command -v {package} >/dev/null 2>&1 && \
                     ({package} -v 2>/dev/null || {package} --version)"
                ),
            )
        })();
        if let Err(e) = staged {
            return Err(rolled_back(tx, e));
        }
        self.commit_applied(
            tx,
            "safe-install",
            Some(package),
            &install,
            None,
            None,
            None,
        )?;
        println!("[cortex] {package} verified; install committed");

        // Packages installed inside the sandbox cannot start their systemd
        // units (no systemd in the chroot); tell the host about the new unit
        // files and leave starting them as a deliberate, visible step.
        let _ = Command::new("systemctl").arg("daemon-reload").status();
        println!(
            "[cortex] note: services are not auto-started from the sandbox; \
             run `systemctl enable --now {package}` if the package ships a unit"
        );
        Ok(())
    }

    fn run_safe_file_edit(&self, tx: Transaction, file: &Path, cmd: &str) -> Result<()> {
        let staged = (|| {
            run_step(&tx, cmd, "edit command")?;
            print_changes(&self.require_changes(&tx)?);
            verify_step(&tx, &file_verify_cmd(file))
        })();
        if let Err(e) = staged {
            return Err(rolled_back(tx, e));
        }
        self.commit_applied(tx, "safe-file-edit", None, cmd, None, None, None)?;
        println!("[cortex] {} edited and verified; committed", file.display());
        Ok(())
    }

    fn run_safe_dependency_upgrade(
        &self,
        tx: Transaction,
        manager: Manager,
        package: &str,
    ) -> Result<()> {
        let version_cmd = version_query(manager, package);
        let staged = (|| {
            let old = query_version(&tx, &version_cmd)
                .with_context(|| format!("{package} does not appear to be installed"))?;
            run_step(&tx, &upgrade_cmd(manager, package), "upgrade")?;
            let new = query_version(&tx, &version_cmd)
                .context("package version unreadable after upgrade")?;
            if old == new {
                bail!(
                    "{package} is already at {old}; nothing to upgrade — transaction rolled back"
                );
            }
            Ok((old, new))
        })();
        let (old, new) = match staged {
            Ok(v) => v,
            Err(e) => return Err(rolled_back(tx, e)),
        };

        let description = format!("{} {package} {old} -> {new}", manager_name(manager));
        // The overlay captured every replaced file (binaries, dpkg/pip/npm
        // metadata), so undo restores the exact old version from the
        // journal's inverse layer — no re-download of the old package.
        self.commit_applied(
            tx,
            "safe-dependency-upgrade",
            Some(package),
            &description,
            None,
            None,
            None,
        )?;
        println!("[cortex] {description}; committed");

        if manager == Manager::Apt && unit_exists(package) {
            println!("[cortex] restarting {package} on the new version");
            let ok = Command::new("systemctl")
                .args(["try-restart", package])
                .status()
                .map(|s| s.success())
                .unwrap_or(false);
            let active = Command::new("systemctl")
                .args(["is-active", "--quiet", package])
                .status()
                .map(|s| s.success())
                .unwrap_or(false);
            if !(ok && active) {
                bail!(
                    "upgrade committed but {package} did not come back up; \
                     `systemctl status {package}` to inspect, `cortex workflow undo` \
                     to restore {old}"
                );
            }
        }
        Ok(())
    }

    fn run_safe_cron_install(&self, tx: Transaction, user: &str, entry: &str) -> Result<()> {
        if !valid_cron_entry(entry) {
            bail!("`{entry}` does not look like a crontab entry (5 time fields or @keyword, then a command)");
        }
        let q = shell_quote(entry);
        let staged = (|| {
            run_step(
                &tx,
                &format!("{{ crontab -u {user} -l 2>/dev/null || true; echo {q}; }} | crontab -u {user} -"),
                "crontab install",
            )?;
            print_changes(&self.require_changes(&tx)?);
            verify_step(&tx, &format!("crontab -u {user} -l | grep -qF {q}"))
        })();
        if let Err(e) = staged {
            return Err(rolled_back(tx, e));
        }
        // The prior crontab file is captured whole in the inverse layer, so
        // undo restores it exactly (including when there was none: whiteout).
        self.commit_applied(
            tx,
            "safe-cron-install",
            None,
            &format!("crontab[{user}] += {entry}"),
            None,
            None,
            None,
        )?;
        println!("[cortex] cron entry installed for {user}; cron picks it up within a minute");
        Ok(())
    }

    fn run_safe_symlink_swap(&self, tx: Transaction, link: &Path, target: &Path) -> Result<()> {
        let (l, t) = (
            shell_quote(&link.display().to_string()),
            shell_quote(&target.display().to_string()),
        );
        let staged = (|| {
            verify_step(&tx, &format!("test -e {t}"))?;
            run_step(&tx, &format!("ln -sfn {t} {l}"), "symlink swap")?;
            print_changes(&self.require_changes(&tx)?); // already-pointing = NoEffect
            verify_step(&tx, &format!("[ \"$(readlink {l})\" = {t} ]"))
        })();
        if let Err(e) = staged {
            return Err(rolled_back(tx, e));
        }
        self.commit_applied(
            tx,
            "safe-symlink-swap",
            None,
            &format!("{} -> {}", link.display(), target.display()),
            None,
            None,
            None,
        )?;
        println!(
            "[cortex] {} now points to {}; undo restores the previous target",
            link.display(),
            target.display()
        );
        Ok(())
    }

    fn run_llm(&self, tx: Transaction, description: &str, cmd: &str) -> Result<()> {
        println!("[cortex] llm task: {description}");
        println!("[cortex] llm command: {cmd}");
        let staged = (|| {
            run_step(&tx, cmd, "llm-generated command")?;
            print_changes(&self.require_changes(&tx)?);
            Ok(())
        })();
        if let Err(e) = staged {
            return Err(rolled_back(tx, e));
        }
        self.commit_applied(
            tx,
            "llm",
            None,
            &format!("{description} => {cmd}"),
            None,
            None,
            None,
        )?;
        println!("[cortex] llm workflow committed; `cortex workflow undo` reverts it");
        Ok(())
    }

    /// NoEffect check: `sed -i` and friends rewrite their file even when no
    /// pattern matched, so an exit status of 0 proves nothing. Compare the
    /// staged upper layer against the lower and refuse to commit a change
    /// that changes nothing.
    fn require_changes(&self, tx: &Transaction) -> Result<Vec<PathBuf>> {
        let changes = cortex_core::journal::staged_changes(tx.upper_path(), &self.lower)?;
        if changes.is_empty() {
            bail!(
                "the command ran successfully but changed nothing under {} \
                 (contents and permissions identical — a pattern that matched \
                 nothing?); nothing to commit",
                self.lower.display()
            );
        }
        Ok(changes)
    }

    /// Arm the inverse in the journal (saga: the undo exists *before* the
    /// mutation is visible), then merge the upper layer into the lower.
    #[allow(clippy::too_many_arguments)] // mirrors journal::capture's fields
    fn commit_applied(
        &self,
        tx: Transaction,
        kind: &str,
        service: Option<&str>,
        description: &str,
        undo_cmd: Option<&str>,
        undo_verify: Option<&str>,
        template_id: Option<&str>,
    ) -> Result<()> {
        let upper = tx.upper_path().to_path_buf();
        let journal = Journal::new(&self.journal_dir);
        let entry = journal
            .capture(
                &upper,
                &self.lower,
                kind,
                service,
                description,
                undo_cmd,
                undo_verify,
                template_id,
            )
            .context("failed to journal the commit's inverse; nothing was committed")?;

        if let Err(e) = tx.commit(true) {
            // The merge may have partially applied; the inverse was captured
            // before it started, so undo restores the full prior state.
            eprintln!(
                "[cortex] commit failed mid-merge; run `cortex undo` \
                 to restore the prior state (journal entry {})",
                entry.id
            );
            return Err(e).with_context(|| format!("commit failed; upper kept at {upper:?}"));
        }

        // Record what we left behind, now that the merge has landed. Undo
        // will require this to still be true before it touches anything.
        let entry = journal.seal(&entry).context(
            "committed, but failed to fingerprint the result; \
                      undo cannot detect drift for this entry",
        )?;
        println!(
            "[cortex] committed: {} changes merged into {} (undo: `cortex undo`, journal entry {})",
            entry.changes,
            self.lower.display(),
            entry.id
        );
        Ok(())
    }
}

/// Consume the transaction on a failed staging step: discard the upper
/// layer so nothing lingers, and keep the original error primary even if
/// the rollback itself refuses.
fn rolled_back(tx: Transaction, e: anyhow::Error) -> anyhow::Error {
    match tx.rollback() {
        Ok(()) => e,
        Err(re) => e.context(format!("(rollback also failed: {re:#})")),
    }
}

/// Run a mutating step inside the transaction; the caller rolls back on
/// failure (see [`rolled_back`]).
fn run_step(tx: &Transaction, cmd: &str, what: &str) -> Result<()> {
    println!("[cortex] running inside transaction: {cmd}");
    let out = tx.run_in_root(cmd)?;
    if !out.status.success() {
        eprintln!(
            "[cortex] {what} failed ({}):\n{}{}",
            out.status,
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        bail!("{what} failed; transaction rolled back");
    }
    Ok(())
}

fn verify_step(tx: &Transaction, cmd: &str) -> Result<()> {
    println!("[cortex] verifying with `{cmd}`");
    let out = tx.run_in_root(cmd)?;
    if !out.status.success() {
        eprintln!(
            "[cortex] verification failed ({}):\n{}{}",
            out.status,
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        bail!("`{cmd}` failed; transaction rolled back");
    }
    Ok(())
}

fn print_changes(changes: &[PathBuf]) {
    println!("[cortex] staged changes ({}):", changes.len());
    for path in changes.iter().take(20) {
        println!("[cortex]   /{}", path.display());
    }
    if changes.len() > 20 {
        println!("[cortex]   ... and {} more", changes.len() - 20);
    }
}

fn reload_service(service: &str) -> Result<()> {
    // The change is only real once the running service picks it up.
    // reload-or-restart also starts a unit that is not running yet.
    let action = format!("systemctl reload-or-restart {service}");
    println!("[cortex] applying to the running service: {action}");
    let status = Command::new("systemctl")
        .args(["reload-or-restart", service])
        .status()
        .context("failed to spawn systemctl")?;
    if !status.success() {
        bail!(
            "config committed, but `{action}` failed ({status}); inspect with \
             `systemctl status {service}` — `cortex workflow undo` restores the \
             previous config"
        );
    }
    println!("[cortex] {service} reloaded");
    Ok(())
}

/// The command that reverses `op` on a unit that was in the given state, or
/// `None` when `op` would change nothing (the saga's `NoEffect` fate).
///
/// Deriving the inverse from the *prior state* rather than from `op` alone is
/// what keeps undo honest: `start` on an already-running unit must journal
/// nothing, or a later undo would stop a service cortex never started.
fn service_inverse(
    op: ServiceOp,
    service: &str,
    was_active: bool,
    was_enabled: bool,
) -> Option<String> {
    match op {
        ServiceOp::Start if was_active => None,
        ServiceOp::Start => Some(format!("systemctl stop {service}")),
        ServiceOp::Stop if !was_active => None,
        ServiceOp::Stop => Some(format!("systemctl start {service}")),
        // Restarting a stopped unit starts it, so its inverse is `stop`.
        // Restarting a running one is approximate — the process is new
        // either way — and the closest inverse is another restart.
        ServiceOp::Restart if was_active => Some(format!("systemctl restart {service}")),
        ServiceOp::Restart => Some(format!("systemctl stop {service}")),
        ServiceOp::Enable if was_enabled => None,
        ServiceOp::Enable => Some(format!("systemctl disable {service}")),
        ServiceOp::Disable if !was_enabled => None,
        ServiceOp::Disable => Some(format!("systemctl enable {service}")),
    }
}

fn unit_exists(name: &str) -> bool {
    systemctl_quiet(&["cat", name])
}

/// `systemctl is-active` / `is-enabled` as a boolean.
fn unit_is(name: &str, query: &str) -> bool {
    systemctl_quiet(&[query, "--quiet", name])
}

fn systemctl_quiet(args: &[&str]) -> bool {
    Command::new("systemctl")
        .args(args)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Resolve a natural-language description into one shell command via the
/// configured LLM endpoint; a failure falls back to asking for the manual
/// form instead of guessing.
fn resolve_llm_command(description: &str) -> Result<String> {
    let client = LlmClient::from_env().unwrap_or_else(|| {
        println!(
            "[cortex] {} not set; trying the default endpoint {}",
            crate::llm::ENV_ENDPOINT,
            crate::llm::DEFAULT_ENDPOINT
        );
        LlmClient::new(crate::llm::DEFAULT_ENDPOINT, "llama3")
    });
    client.generate_command(description).map_err(|e| {
        e.context(
            "LLM unavailable — run the operation manually instead, e.g. \
             `cortex workflow safe-file-edit --file <path> --cmd \"...\"` \
             (set CORTEX_LLM_ENDPOINT to your cortex-server relay \
             http://127.0.0.1:36702/api/chat or an OpenAI-compatible endpoint)",
        )
    })
}

fn manager_name(m: Manager) -> &'static str {
    match m {
        Manager::Apt => "apt",
        Manager::Pip => "pip",
        Manager::Npm => "npm",
    }
}

fn version_query(m: Manager, package: &str) -> String {
    let p = shell_quote(package);
    match m {
        Manager::Apt => format!("dpkg-query -W -f='${{Version}}' {p}"),
        Manager::Pip => format!("python3 -m pip show {p} 2>/dev/null | sed -n 's/^Version: //p'"),
        Manager::Npm => {
            format!("npm ls -g --depth=0 {p} 2>/dev/null | sed -n 's/.*@\\([0-9][^ ]*\\).*/\\1/p'")
        }
    }
}

fn upgrade_cmd(m: Manager, package: &str) -> String {
    let p = shell_quote(package);
    match m {
        Manager::Apt => {
            format!("DEBIAN_FRONTEND=noninteractive apt-get install -y --only-upgrade {p}")
        }
        Manager::Pip => format!("python3 -m pip install --upgrade {p}"),
        Manager::Npm => format!("npm install -g {p}@latest"),
    }
}

fn query_version(tx: &Transaction, cmd: &str) -> Result<String> {
    let out = tx.run_in_root(cmd)?;
    let v = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if !out.status.success() || v.is_empty() {
        bail!("version query `{cmd}` produced nothing");
    }
    Ok(v)
}

/// Minimal syntactic gate for a crontab line: five time fields (or an
/// @keyword) followed by a command. Vixie cron validates for real when the
/// entry is installed; this catches the obvious mistakes first.
pub fn valid_cron_entry(entry: &str) -> bool {
    let re = regex::Regex::new(
        r"^(@(reboot|yearly|annually|monthly|weekly|daily|hourly)|([0-9*,/A-Za-z-]+\s+){4}[0-9*,/A-Za-z-]+)\s+\S.*$",
    )
    .expect("static regex");
    re.is_match(entry.trim())
}

/// Pick a verification command by what kind of file was edited.
fn file_verify_cmd(file: &Path) -> String {
    let name = file.file_name().and_then(|n| n.to_str()).unwrap_or("");
    let path = file.display().to_string();
    if path.contains("nginx") {
        return "nginx -t".to_string();
    }
    match name {
        "passwd" | "shadow" => "pwck -r".to_string(),
        "group" | "gshadow" => "grpck -r".to_string(),
        _ => format!("test -s {}", shell_quote(&path)),
    }
}

/// Single-quote a string for embedding in `sh -c`.
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// Revert a committed workflow: run its compensation command (inverse SQL)
/// if it has one, apply its journaled inverse layer, and handle the service
/// around the restore (stop an undone install's unit, reload an undone
/// config's unit, restart an undone upgrade's unit).
pub fn undo(journal_dir: &Path, id: Option<&str>, force: bool) -> Result<()> {
    let journal = Journal::new(journal_dir);

    // Validate BEFORE side effects: stopping a service for an undo the
    // journal then refuses would leave the system worse than doing nothing.
    let meta = journal.peek_undo(id, force)?;

    if meta.kind == "safe-install" {
        if let Some(service) = &meta.service {
            // Undoing an install removes the unit and binaries; stop the
            // service first so no process is left running from deleted files.
            println!("[cortex] stopping {service} before removing its files (best effort)");
            let _ = Command::new("systemctl").args(["stop", service]).status();
        }
    }

    // `Journal::undo` owns the whole safe sequence: check drift, rescue if
    // forced, compensate, PROVE the compensation worked, restore, and only
    // then mark the entry undone. Nothing here may duplicate those steps.
    let meta = journal.undo(Some(&meta.id), force)?;
    println!(
        "[cortex] undone {} ({}: {}) — {} paths restored on {}",
        meta.id,
        meta.kind,
        meta.description,
        meta.changes,
        meta.lower.display()
    );

    match meta.kind.as_str() {
        "safe-config" => {
            if let Some(service) = &meta.service {
                println!("[cortex] reloading {service} with the restored config");
                let status = Command::new("systemctl")
                    .args(["reload-or-restart", service])
                    .status()
                    .context("failed to spawn systemctl")?;
                if !status.success() {
                    bail!(
                        "config restored, but `systemctl reload-or-restart {service}` \
                         failed ({status}); inspect with `systemctl status {service}`"
                    );
                }
            }
        }
        "safe-install" => {
            let _ = Command::new("systemctl").arg("daemon-reload").status();
        }
        "safe-dependency-upgrade" => {
            if let Some(service) = &meta.service {
                if unit_exists(service) {
                    println!("[cortex] restarting {service} on the restored version");
                    let _ = Command::new("systemctl")
                        .args(["try-restart", service])
                        .status();
                }
            }
        }
        // safe-run and safe-service are fully reversed by their compensation,
        // which already ran above; there is nothing left to reconcile.
        _ => {}
    }
    Ok(())
}

/// Revert every pending entry, newest first — "undo everything". Stops at
/// the first failure rather than stepping over it: a compensation that
/// refused means the state is not what the older entries were journaled
/// against, and continuing would undo them onto a system that moved.
///
/// A partial result is a real state, not a bug, so it is reported as one:
/// the operator is told exactly how many entries remain and what to run.
pub fn undo_all(journal_dir: &Path, force: bool) -> Result<()> {
    let pending = Journal::new(journal_dir).pending()?;
    if pending.is_empty() {
        println!("{} {}", ui::green("✔"), ui::dim("nothing to undo"));
        return Ok(());
    }
    ui::section(&format!(
        "undoing {} change(s), newest first",
        pending.len()
    ));

    for (i, meta) in pending.iter().enumerate() {
        let step = ui::Step::start(format!(
            "[{}/{}] {}",
            i + 1,
            pending.len(),
            meta.description.chars().take(60).collect::<String>()
        ));
        match undo_one(journal_dir, &meta.id, force) {
            Ok(()) => step.ok(),
            Err(e) => {
                step.fail("blocked");
                let done = i;
                let left = pending.len() - i;
                ui::error(&format!("{e:#}"), None);
                println!();
                ui::warn(&format!(
                    "stopped after undoing {done} of {}; {left} change(s) still applied",
                    pending.len()
                ));
                println!("  {} {}", ui::dim("see:"), ui::bold("cortex status"));
                bail!("undo incomplete: {left} change(s) remain");
            }
        }
    }
    println!(
        "\n{} {} {}",
        ui::green("✔"),
        ui::bold("all changes reversed"),
        ui::dim(&format!("({} entries)", pending.len()))
    );
    Ok(())
}

/// The undo of exactly one entry, without the reporting chrome.
fn undo_one(journal_dir: &Path, id: &str, force: bool) -> Result<()> {
    undo(journal_dir, Some(id), force)
}

/// The conformance suite: prove, on this machine, that every reversible
/// template actually reverses. This is the artifact a skeptic runs.
///
/// For each template that can be exercised safely here, it runs the forward
/// command, asserts the forward post-condition, runs the inverse, and
/// asserts the inverse post-condition. A template whose inverse does not
/// satisfy its own verifier is a failure — which is exactly the class of bug
/// (`--undo-cmd "echo done"`) that this design exists to make impossible.
pub fn verify_self() -> Result<()> {
    use cortex_registry::{lookup, TEMPLATES};

    ui::section("reversibility conformance");
    println!(
        "  {}\n",
        ui::dim("for each template: run it, prove it worked, undo it, prove it undid")
    );

    let mut passed = 0usize;
    let mut skipped = 0usize;
    let mut failed = Vec::new();

    for t in TEMPLATES {
        // Report *why* a template did not run. A suite that hides its own
        // coverage is the same lie as an undo that hides its own failure.
        if let Some(reason) = unavailable(t.id) {
            ui::Step::start(t.id.to_string()).skip(reason);
            skipped += 1;
            continue;
        }
        let Some(args) = self_test_args(t.id) else {
            ui::Step::start(t.id.to_string()).skip("no self-test fixture");
            skipped += 1;
            continue;
        };

        let bound = lookup(t.id).expect("id from TEMPLATES").bind(&args)?;
        let step = ui::Step::start(t.id.to_string());
        match conformance_cycle(&bound) {
            Ok(()) => {
                step.ok();
                passed += 1;
            }
            Err(e) => {
                step.fail(&format!("{e}"));
                failed.push((t.id, format!("{e:#}")));
            }
        }
    }

    println!();
    if failed.is_empty() {
        println!(
            "{} {}",
            ui::green("✔"),
            ui::bold(&format!(
                "{passed} template(s) proved reversible on this machine"
            )),
        );
        if skipped > 0 {
            // Never let a skip read as a pass.
            println!(
                "  {}",
                ui::dim(&format!(
                    "{skipped} not exercised here (see reasons above); CI runs them in a container"
                ))
            );
        }
        Ok(())
    } else {
        for (id, why) in &failed {
            ui::error(&format!("{id}: {why}"), None);
        }
        bail!(
            "{} template(s) failed conformance — the undo guarantee does NOT hold here",
            failed.len()
        )
    }
}

/// forward → verify_forward → inverse → verify_inverse. Any step failing
/// means the template's promise is false on this machine.
fn conformance_cycle(bound: &cortex_registry::Bound) -> Result<()> {
    let sh = |cmd: &str| -> Result<bool> { predicate_holds(cmd) };

    if !sh(&bound.forward)? {
        bail!("forward command failed");
    }
    if !sh(&bound.verify_forward)? {
        // Clean up before reporting: a conformance run must not leave junk.
        let _ = predicate_holds(&bound.inverse);
        bail!("forward ran but its post-condition does not hold");
    }
    if !sh(&bound.inverse)? {
        bail!("inverse command failed (system may be dirty)");
    }
    if !sh(&bound.verify_inverse)? {
        bail!(
            "INVERSE DID NOT REVERSE: post-condition `{}` failed",
            bound.verify_inverse
        );
    }
    Ok(())
}

/// A scratch directory the self-test can safely create and destroy things in.
/// Kept alive for the whole run so fixture paths remain valid.
fn self_test_dir() -> &'static Path {
    use std::sync::OnceLock;
    static DIR: OnceLock<PathBuf> = OnceLock::new();
    DIR.get_or_init(|| {
        let d = std::env::temp_dir().join(format!("cortex-selftest-{}", std::process::id()));
        let _ = std::fs::create_dir_all(d.join("blue"));
        let _ = std::fs::create_dir_all(d.join("green"));
        d
    })
}

/// Fixtures for templates that can be exercised without touching anything
/// the operator cares about. Templates with no fixture are skipped, not
/// silently passed — a suite that reports green for tests it never ran is
/// the same lie it exists to prevent.
fn self_test_args(id: &str) -> Option<std::collections::BTreeMap<String, String>> {
    let m = |pairs: &[(&str, &str)]| -> std::collections::BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    };
    let d = self_test_dir();
    match id {
        "docker.run" => Some(m(&[
            ("name", "cortex-selftest"),
            ("image", "busybox:latest"),
            // Port 0 lets the kernel pick, so a busy host port cannot make
            // the conformance suite spuriously fail.
            ("ports", "0:80"),
        ])),
        "symlink.swap" => {
            // Start the link pointing at blue; the template swaps it to
            // green, and the inverse must move it back to blue exactly.
            let link = d.join("current");
            let _ = std::fs::remove_file(&link);
            std::os::unix::fs::symlink(d.join("blue"), &link).ok()?;
            Some(m(&[
                ("link", link.to_str()?),
                ("target", d.join("green").to_str()?),
                ("previous", d.join("blue").to_str()?),
            ]))
        }
        // service.* and package.install mutate the real host: a conformance
        // run must never install a package or stop a daemon behind the
        // operator's back. They are exercised in CI under a container.
        _ => None,
    }
}

/// Why a template cannot be exercised here (missing daemon, no root).
fn unavailable(id: &str) -> Option<&'static str> {
    match id {
        "docker.run" | "docker.compose.up" => {
            if predicate_holds("docker info").unwrap_or(false) {
                None
            } else {
                Some("docker unavailable")
            }
        }
        "service.start" | "service.stop" | "service.enable" => {
            Some("would mutate host services; covered in CI")
        }
        "package.install" => Some("would install a package; covered in CI"),
        _ => None,
    }
}

/// Build a workflow from the planner's JSON object.
///
/// The planner may **select** a registry template and bind its parameters;
/// it may not author an inverse. That is the difference between a model
/// choosing from operations a human verified, and a model inventing a claim
/// about how to undo something. A plan naming `template` is bound through
/// [`crate::registry`], which supplies the inverse and both post-conditions.
pub fn from_plan(plan: &serde_json::Value) -> Result<Workflow> {
    let field = |name: &str| -> Result<String> {
        plan.get(name)
            .and_then(serde_json::Value::as_str)
            .map(str::to_string)
            .filter(|s| !s.trim().is_empty())
            .with_context(|| format!("plan is missing `{name}`: {plan}"))
    };
    let workflow = field("workflow")?;
    Ok(match workflow.as_str() {
        "safe-service" => Workflow::safe_service(field("op")?.parse()?, field("service")?),
        "safe-install" => Workflow::safe_install(field("package")?),
        "safe-dependency-upgrade" => {
            Workflow::safe_dependency_upgrade(field("manager")?.parse()?, field("package")?)
        }
        "safe-file-edit" => Workflow::safe_file_edit(field("file")?, field("cmd")?),
        "safe-config" => Workflow::safe_config(field("service")?, field("cmd")?),
        "safe-symlink-swap" => Workflow::safe_symlink_swap(field("link")?, field("target")?),
        "safe-cron-install" => Workflow::safe_cron_install(
            plan.get("user")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("root"),
            field("entry")?,
        ),
        // The planner selects a template by id and supplies its parameters.
        // The inverse and both verifiers come from the registry, never from
        // the model — a hallucinated inverse cannot enter the journal.
        "template" => {
            let id = field("template")?;
            let template = cortex_registry::lookup(&id).with_context(|| {
                format!(
                    "planner chose unknown template `{id}`; known templates: {}",
                    cortex_registry::TEMPLATES
                        .iter()
                        .map(|t| t.id)
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            })?;
            let args = plan
                .get("args")
                .and_then(serde_json::Value::as_object)
                .map(|m| {
                    m.iter()
                        .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                        .collect()
                })
                .unwrap_or_default();
            Workflow::template(template.bind(&args)?)
        }
        other => bail!(
            "planner chose an unknown workflow `{other}`. Reversible operations \
             outside the built-in workflows must name a registry `template`."
        ),
    })
}

/// True when the user is asking to reverse things rather than do something.
/// Matched before any LLM call so `undo` keeps working with no model
/// reachable — the one command you most need when things have gone wrong.
pub fn is_undo_intent(text: &str) -> bool {
    let t = text.trim().to_lowercase();
    let t = t.trim_end_matches(['.', '!', '?']).trim();
    const VERBS: &[&str] = &["undo", "revert", "roll back", "rollback", "reverse"];
    VERBS.iter().any(|v| {
        t == *v
            || t.starts_with(&format!("{v} "))
            || t.starts_with(&format!("please {v}"))
            || t.starts_with(&format!("can you {v}"))
    })
}

/// Authorize a resolved plan. Returns `Err` to refuse it.
///
/// Taken as a callback so `cortex-core` does not decide *where* policy comes
/// from — the binary supplies the root-owned ruleset. What matters here is
/// that the gate runs on the **resolved operation**, after a model has
/// chosen it and before anything executes.
pub type Authorizer<'a> = dyn Fn(&cortex_policy::Operation) -> Result<()> + 'a;

/// The natural-language entry point shared by the CLI, the Obsidian command
/// palette and the server relay: English in, a reversible workflow out.
pub fn run_natural_language(
    description: &str,
    journal_dir: &Path,
    lower: Option<PathBuf>,
    state_dir: Option<PathBuf>,
    authorize: &Authorizer<'_>,
) -> Result<()> {
    if is_undo_intent(description) {
        ui::info("understood as an undo request");
        authorize(&cortex_policy::Operation::Undo)?;
        return undo_all(journal_dir, false);
    }

    // Fast path: the most common intents are matched locally, with no model
    // call at all. This is most of the latency budget, and it means the
    // hero command still works on a box with no network.
    if let Some(plan) = cortex_core::plan::offline(description) {
        ui::info(&format!("matched offline: {}", plan.summary));
        return run_plan(&plan.value, journal_dir, lower, state_dir, authorize);
    }

    let client = LlmClient::from_env().unwrap_or_else(|| {
        println!(
            "[cortex] {} not set; trying the default endpoint {}",
            crate::llm::ENV_ENDPOINT,
            crate::llm::DEFAULT_ENDPOINT
        );
        LlmClient::new(crate::llm::DEFAULT_ENDPOINT, "llama3")
    });
    let plan = client.generate_plan(description).map_err(|e| {
        e.context(
            "could not turn that into a reversible workflow — run it explicitly, \
             e.g. `cortex workflow safe-service --op start --service nginx` \
             (set CORTEX_LLM_ENDPOINT to your cortex-server relay, \
             http://127.0.0.1:36702/api/chat)",
        )
    })?;
    ui::info(&format!("plan: {plan}"));
    run_plan(&plan, journal_dir, lower, state_dir, authorize)
}

/// The single point where a plan becomes a running operation. Both the
/// offline matcher and the LLM funnel through here, so authorization cannot
/// be skipped by whichever path produced the plan.
fn run_plan(
    plan: &serde_json::Value,
    journal_dir: &Path,
    lower: Option<PathBuf>,
    state_dir: Option<PathBuf>,
    authorize: &Authorizer<'_>,
) -> Result<()> {
    // Own the arguments here so the borrowed `Operation` handed to policy
    // lives exactly as long as the check that reads it.
    let args = plan_args(plan);
    authorize(&plan_operation(plan, &args)?)?;

    let mut workflow = from_plan(plan)?.journal_dir(journal_dir);
    if let Some(lower) = lower {
        workflow = workflow.lower(lower);
    }
    if let Some(dir) = state_dir {
        workflow = workflow.state_dir(dir);
    }
    workflow.run()
}

/// Describe a plan in the authorization vocabulary, so policy sees the same
/// operation `from_plan` is about to build.
fn plan_operation<'a>(
    plan: &'a serde_json::Value,
    args: &'a std::collections::BTreeMap<String, String>,
) -> Result<cortex_policy::Operation<'a>> {
    use cortex_policy::Operation;
    let kind = plan
        .get("workflow")
        .and_then(serde_json::Value::as_str)
        .context("plan names no workflow")?;

    if kind == "template" {
        let id = plan
            .get("template")
            .and_then(serde_json::Value::as_str)
            .context("plan names no template")?;
        return Ok(Operation::Template { id, args });
    }
    Ok(Operation::Workflow { kind })
}

fn plan_args(plan: &serde_json::Value) -> std::collections::BTreeMap<String, String> {
    plan.get("args")
        .and_then(serde_json::Value::as_object)
        .map(|m| {
            m.iter()
                .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                .collect()
        })
        .unwrap_or_default()
}

/// Answer the question an operator actually has after something went wrong:
/// *what state is this machine in, and what do I do next?*
///
/// Reports pending entries newest-first, flags drift against each one (so a
/// blocked undo is visible before it is attempted), names irreversible
/// entries that `undo` will refuse, and ends with the single next command.
pub fn status(journal_dir: &Path) -> Result<()> {
    let journal = Journal::new(journal_dir);
    let pending = journal.pending()?;

    if pending.is_empty() {
        println!("{} {}", ui::green("✔"), ui::bold("clean"));
        println!("  {}", ui::dim("no pending transactions; nothing to undo"));
        return Ok(());
    }

    ui::section(&format!("{} pending transaction(s)", pending.len()));
    let mut blocked: Vec<(&str, String)> = Vec::new();
    let mut irreversible = 0usize;

    for meta in &pending {
        let drifted =
            cortex_core::guard::detect_drift(&meta.lower, &meta.fingerprints).unwrap_or_default();
        let tag = if meta.kind == cortex_core::journal::KIND_IRREVERSIBLE {
            irreversible += 1;
            ui::red("irreversible")
        } else if !drifted.is_empty() {
            blocked.push((
                &meta.id,
                format!("{} path(s) changed since commit", drifted.len()),
            ));
            ui::yellow("blocked by drift")
        } else {
            ui::green("undoable")
        };
        println!(
            "  {}  {}  {}",
            ui::dim(&meta.id),
            tag,
            meta.description.chars().take(70).collect::<String>()
        );
        for d in drifted.iter().take(3) {
            println!("      {}", ui::yellow(&d.describe()));
        }
    }

    println!();
    if irreversible > 0 {
        ui::warn(&format!(
            "{irreversible} entry(s) cannot be undone; reverse them by hand, then `cortex forget <id>`"
        ));
    }
    if !blocked.is_empty() {
        ui::warn(&format!(
            "{} entry(s) blocked: someone changed those paths after cortex committed",
            blocked.len()
        ));
        println!(
            "  {} {}",
            ui::dim("inspect:"),
            ui::bold("cortex receipt <id>")
        );
        println!(
            "  {} {}",
            ui::dim("override:"),
            ui::bold("cortex undo --force"),
        );
        println!(
            "  {}",
            ui::dim("(--force rescues the current contents before overwriting)")
        );
    } else if irreversible < pending.len() {
        println!("  {} {}", ui::dim("next:"), ui::bold("cortex undo"));
    }
    Ok(())
}

/// Drop an entry from the pending list without undoing it. For irreversible
/// operations the operator reversed by hand, and for entries whose undo is
/// no longer wanted. The entry is retained, marked, and still auditable —
/// forgetting is not deleting.
pub fn forget(journal_dir: &Path, id: &str) -> Result<()> {
    let meta = Journal::new(journal_dir).forget(id)?;
    println!(
        "{} {} {}",
        ui::yellow("⊘"),
        ui::bold("forgotten"),
        ui::dim(&format!("{} ({})", meta.id, meta.description))
    );
    println!(
        "  {}",
        ui::dim("it stays in the journal for audit, but undo will skip it")
    );
    Ok(())
}

/// A signed, human-readable summary of one transaction.
pub fn receipt(journal_dir: &Path, id: Option<&str>) -> Result<()> {
    let journal = Journal::new(journal_dir);
    let entries = journal.entries()?;
    let entry = match id {
        Some(id) => entries
            .into_iter()
            .find(|e| e.meta.id == id)
            .with_context(|| format!("no journal entry {id}"))?,
        None => entries.into_iter().next().context("the journal is empty")?,
    };
    let m = &entry.meta;

    ui::section(&format!("receipt {}", m.id));
    println!("  {:<12} {}", ui::dim("when"), m.created);
    println!("  {:<12} {}", ui::dim("what"), m.description);
    println!("  {:<12} {}", ui::dim("kind"), m.kind);
    if let Some(t) = &m.template_id {
        println!("  {:<12} {}", ui::dim("template"), t);
    }
    println!(
        "  {:<12} {}",
        ui::dim("state"),
        if entry.undone {
            ui::dim("undone")
        } else if m.kind == cortex_core::journal::KIND_IRREVERSIBLE {
            ui::red("applied (irreversible)")
        } else {
            ui::green("applied")
        }
    );

    if let (Some(cmd), Some(v)) = (&m.undo_cmd, &m.undo_verify) {
        println!("\n  {}", ui::bold("undo"));
        println!("    {:<10} {}", ui::dim("run"), cmd);
        println!("    {:<10} {}", ui::dim("prove"), v);
    }

    if !m.fingerprints.is_empty() {
        println!("\n  {} ({})", ui::bold("paths"), m.fingerprints.len());
        let drifted =
            cortex_core::guard::detect_drift(&m.lower, &m.fingerprints).unwrap_or_default();
        for (rel, fp) in m.fingerprints.iter().take(20) {
            let moved = drifted.iter().any(|d| &d.path == rel);
            let mark = if moved { ui::yellow("~") } else { ui::dim(" ") };
            println!("    {mark} /{}  {}", rel.display(), ui::dim(&fp.describe()));
        }
        if m.fingerprints.len() > 20 {
            println!(
                "    {}",
                ui::dim(&format!("... and {} more", m.fingerprints.len() - 20))
            );
        }
        if !drifted.is_empty() {
            println!(
                "\n  {}",
                ui::yellow(&format!(
                    "{} path(s) marked ~ changed after cortex committed; undo will refuse without --force",
                    drifted.len()
                ))
            );
        }
    }
    Ok(())
}

/// Print the journal, newest first.
pub fn history(journal_dir: &Path) -> Result<()> {
    let entries = Journal::new(journal_dir).entries()?;
    if entries.is_empty() {
        println!("[cortex] journal is empty (no committed workflows yet)");
        return Ok(());
    }
    for e in entries {
        println!(
            "{}  {}  {:<22}  {:>6} paths  {}{}",
            e.meta.id,
            e.meta.created,
            e.meta.kind,
            e.meta.changes,
            e.meta.description,
            if e.undone { "  [undone]" } else { "" }
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cron_entries_validate() {
        assert!(valid_cron_entry("0 2 * * 0 /opt/backup.sh"));
        assert!(valid_cron_entry("*/5 * * * * /usr/bin/uptime >> /tmp/log"));
        assert!(valid_cron_entry("@daily /opt/backup.sh"));
        assert!(!valid_cron_entry("0 2 * * /opt/backup.sh")); // 4 fields
        assert!(!valid_cron_entry("@sometimes /opt/backup.sh"));
        assert!(!valid_cron_entry("0 2 * * 0")); // no command
    }

    #[test]
    fn manager_commands_are_wired() {
        assert!(upgrade_cmd(Manager::Apt, "nginx").contains("--only-upgrade 'nginx'"));
        assert!(upgrade_cmd(Manager::Pip, "requests").contains("pip install --upgrade"));
        assert!(upgrade_cmd(Manager::Npm, "pm2").contains("'pm2'@latest"));
        assert!(version_query(Manager::Apt, "nginx").starts_with("dpkg-query"));
        assert!("apt".parse::<Manager>().is_ok());
        assert!("cargo".parse::<Manager>().is_err());
    }

    #[test]
    fn file_verifiers_match_file_kind() {
        assert_eq!(file_verify_cmd(Path::new("/etc/passwd")), "pwck -r");
        assert_eq!(file_verify_cmd(Path::new("/etc/group")), "grpck -r");
        assert_eq!(
            file_verify_cmd(Path::new("/etc/nginx/nginx.conf")),
            "nginx -t"
        );
        assert_eq!(
            file_verify_cmd(Path::new("/opt/app.cfg")),
            "test -s '/opt/app.cfg'"
        );
    }

    #[test]
    fn shell_quote_survives_quotes() {
        assert_eq!(shell_quote("a'b"), r"'a'\''b'");
        assert_eq!(shell_quote("plain"), "'plain'");
    }

    #[test]
    fn undo_intent_is_recognised_without_an_llm() {
        for yes in [
            "undo",
            "Undo.",
            "undo everything",
            "please undo that",
            "revert the last change",
            "roll back",
            "rollback everything",
            "can you reverse that",
        ] {
            assert!(is_undo_intent(yes), "should be undo intent: {yes}");
        }
        for no in [
            "run nginx server",
            "spin up docker images",
            "undocker the thing", // prefix must be a whole word
            "install undo-manager",
            "",
        ] {
            assert!(!is_undo_intent(no), "should NOT be undo intent: {no}");
        }
    }

    #[test]
    fn plans_map_to_workflows() {
        use serde_json::json;
        let ok = |p: serde_json::Value| from_plan(&p).map(|_| ()).is_ok();

        assert!(ok(
            json!({"workflow":"safe-service","op":"start","service":"nginx"})
        ));
        assert!(ok(json!({"workflow":"safe-install","package":"nginx"})));
        assert!(ok(
            json!({"workflow":"safe-symlink-swap","link":"/a","target":"/b"})
        ));
        assert!(ok(
            json!({"workflow":"safe-cron-install","entry":"0 2 * * 0 /x.sh"})
        ));

        // Docker is reached by SELECTING a registry template and binding its
        // parameters. The inverse comes from the registry, not the plan.
        assert!(ok(json!({
            "workflow": "template",
            "template": "docker.run",
            "args": {"name": "web", "image": "nginx", "ports": "8080:80"}
        })));

        // The planner may no longer author an inverse. `safe-run` with a
        // hand-written undo_cmd is exactly the hole that let `echo done`
        // masquerade as a rollback: it must not be reachable from a plan.
        assert!(!ok(json!({
            "workflow": "safe-run",
            "cmd": "docker run -d --name web nginx",
            "undo_cmd": "echo done"
        })));
        // A template that does not exist, or is missing a parameter.
        assert!(!ok(json!({"workflow":"template","template":"docker.pwn"})));
        assert!(!ok(json!({
            "workflow":"template","template":"docker.run","args":{"name":"web"}
        })));
        // Migration with no inverse SQL.
        // db-migration was removed (GAP 8): it must be an unknown workflow now.
        assert!(!ok(
            json!({"workflow":"safe-db-migration","db":"d","sql":"X"})
        ));
        // And unknown workflows / ops.
        assert!(!ok(json!({"workflow":"rm-rf-everything"})));
        assert!(!ok(
            json!({"workflow":"safe-service","op":"yeet","service":"nginx"})
        ));
    }

    #[test]
    fn service_ops_parse_and_render() {
        assert_eq!("start".parse::<ServiceOp>().unwrap(), ServiceOp::Start);
        assert_eq!(ServiceOp::Disable.as_str(), "disable");
        assert!("frobnicate".parse::<ServiceOp>().is_err());
    }

    /// The inverse must be derived from the unit's PRIOR state, not from the
    /// op alone. Every (op, was_active, was_enabled) combination is pinned:
    /// a wrong entry here means undo either stops a service cortex never
    /// started, or fails to stop one it did.
    #[test]
    fn service_inverse_is_derived_from_prior_state() {
        use ServiceOp::*;
        let inv = |op, active, enabled| service_inverse(op, "nginx", active, enabled);

        // Starting something already running changes nothing -> journal nothing.
        assert_eq!(inv(Start, true, false), None);
        assert_eq!(
            inv(Start, false, false).as_deref(),
            Some("systemctl stop nginx")
        );

        // Stopping something already stopped changes nothing.
        assert_eq!(inv(Stop, false, false), None);
        assert_eq!(
            inv(Stop, true, false).as_deref(),
            Some("systemctl start nginx")
        );

        // Restarting a stopped unit starts it, so undo must stop it.
        assert_eq!(
            inv(Restart, false, false).as_deref(),
            Some("systemctl stop nginx")
        );
        assert_eq!(
            inv(Restart, true, false).as_deref(),
            Some("systemctl restart nginx")
        );

        // enable/disable key off was_enabled, independent of was_active.
        assert_eq!(inv(Enable, false, true), None);
        assert_eq!(
            inv(Enable, true, false).as_deref(),
            Some("systemctl disable nginx")
        );
        assert_eq!(inv(Disable, true, false), None);
        assert_eq!(
            inv(Disable, false, true).as_deref(),
            Some("systemctl enable nginx")
        );
    }
}
