//! Cortex — run any change transactionally, verify it, commit or undo it.
//!
//! Hand-rolled dispatcher rather than clap: startup time is a feature, and
//! the surface is small enough that a match is clearer than a builder.

mod llm;
mod workflow;

use anyhow::{bail, Context, Result};
use cortex_core::journal::DEFAULT_JOURNAL_DIR;
use cortex_core::ui;
use cortex_policy::{self as authz, Operation, DEFAULT_POLICY_PATH};
use cortex_registry as registry;
use std::collections::BTreeMap;
use std::path::PathBuf;

const USAGE: &str = "\
cortex — run any change transactionally, verify it, undo it with proof.

USAGE
  cortex try \"<what you want>\"     Plan it, run it in a sandbox, verify, commit
  cortex status                     What is applied, what is undoable, what is blocked
  cortex undo [id]                  Reverse the last change (or one by id), with proof
  cortex receipt [id]               Signed summary of one transaction
  cortex demo                       Prove the guarantee in ~2s (no root, no docker)
  cortex history                    Everything cortex has committed
  cortex verify --self              Prove every template's undo actually undoes
  cortex do <template> k=v ...      Run a known-good template directly

FLAGS
  --all             undo: reverse every pending change, newest first
  --force           undo: proceed despite drift (rescues current contents first)
  --yes-irreversible  try/do: consent to an operation that cannot be undone
  --json            machine-readable output
  --no-color        never emit ANSI colour

  --journal-dir <d>  where undo records live   (default /var/lib/cortex/journal)
  --lower <d>        overlay base layer        (default /)
  --state-dir <d>    overlay scratch           (default /run/cortex/transactions)

EXAMPLES
  cortex try \"run nginx on port 8080\"
  cortex do docker.run name=web image=nginx ports=8080:80
  cortex status
  cortex undo

Every committed change records an inverse *and* a post-condition that proves
the inverse worked. Undo refuses if anyone changed those files since. Run
`cortex verify --self` to check the guarantee on your own machine.";

fn main() {
    if let Err(e) = run() {
        // An error without a next step is a dead end. anyhow's context chain
        // carries the "what to do" text, so render the whole chain.
        let msg = format!("{e}");
        let hint = e.source().map(|s| format!("{s:#}"));
        ui::error(&msg, hint.as_deref());
        std::process::exit(1);
    }
}

/// Global options, stripped from argv before subcommand parsing.
struct Global {
    journal_dir: PathBuf,
    lower: Option<PathBuf>,
    state_dir: Option<PathBuf>,
    policy_path: PathBuf,
    force: bool,
    all: bool,
    yes_irreversible: bool,
}

impl Global {
    /// Evaluate an operation against the root-owned policy and refuse unless
    /// it may proceed. Every mutating path calls this *before* it touches
    /// anything — cortex is a root binary, so its own gate is the only thing
    /// standing between a caller and the kernel.
    fn authorize(&self, op: &Operation) -> Result<authz::Authorization> {
        let auth = authz::authorize(&self.policy_path, op)?;
        auth.require(op, self.yes_irreversible)?;
        if auth.decision == authz::Decision::Audit {
            ui::warn(&format!("policy: audited — {}", auth.reason));
        }
        Ok(auth)
    }
}

fn run() -> Result<()> {
    let mut args: Vec<String> = std::env::args().skip(1).collect();

    let mut g = Global {
        journal_dir: PathBuf::from(DEFAULT_JOURNAL_DIR),
        lower: None,
        state_dir: None,
        policy_path: PathBuf::from(DEFAULT_POLICY_PATH),
        force: false,
        all: false,
        yes_irreversible: false,
    };

    // Strip global flags wherever they appear, so `cortex undo --all` and
    // `cortex --all undo` both work and subcommands stay simple.
    let mut rest = Vec::new();
    let mut it = args.drain(..).peekable();
    while let Some(a) = it.next() {
        let mut value = |name: &str| -> Result<String> {
            it.next()
                .ok_or_else(|| anyhow::anyhow!("{name} requires a value"))
        };
        match a.as_str() {
            "--journal-dir" => g.journal_dir = PathBuf::from(value("--journal-dir")?),
            "--lower" => g.lower = Some(PathBuf::from(value("--lower")?)),
            "--state-dir" => g.state_dir = Some(PathBuf::from(value("--state-dir")?)),
            // Pointing at a different policy file buys nothing: `Policy::load`
            // refuses any file that is not root-owned and root-writable-only,
            // so a caller cannot supply rules they wrote themselves.
            "--policy" => g.policy_path = PathBuf::from(value("--policy")?),
            "--force" => g.force = true,
            "--all" => g.all = true,
            "--yes-irreversible" => g.yes_irreversible = true,
            "--no-color" | "--json" => ui::disable_color(),
            other => rest.push(other.to_string()),
        }
    }

    match rest.first().map(String::as_str) {
        Some("try") | Some("run") => cmd_try(&rest[1..], &g),
        Some("do") => cmd_do(&rest[1..], &g),
        Some("status") => workflow::status(&g.journal_dir),
        Some("undo") => cmd_undo(&rest[1..], &g),
        Some("receipt") => workflow::receipt(&g.journal_dir, rest.get(1).map(String::as_str)),
        Some("history") => workflow::history(&g.journal_dir),
        Some("forget") => {
            let id = rest.get(1).context("forget needs a journal entry id")?;
            workflow::forget(&g.journal_dir, id)
        }
        Some("verify") => cmd_verify(&rest[1..]),
        Some("templates") => cmd_templates(),
        Some("demo") => cmd_demo(&g),
        // `workflow` kept as a hidden alias so existing scripts keep working.
        Some("workflow") => legacy_workflow(&rest[1..], &g),
        Some("help") | Some("--help") | Some("-h") | None => {
            println!("{USAGE}");
            Ok(())
        }
        Some("version") | Some("--version") | Some("-V") => {
            println!("cortex {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        Some(other) => bail!("unknown command `{other}`\n\n{USAGE}"),
    }
}

/// `cortex try "<english>"` — the hero command.
fn cmd_try(args: &[String], g: &Global) -> Result<()> {
    let description = args.join(" ");
    if description.trim().is_empty() {
        bail!("try needs a description, e.g. cortex try \"run nginx on port 8080\"");
    }
    // The plan is authorized after it resolves, not before: policy must see
    // the concrete operation a model chose, never the English that produced
    // it. `run_natural_language` calls back into `authorize` with the bound
    // plan, so an LLM cannot reach the kernel through a rule it never matched.
    workflow::run_natural_language(
        &description,
        &g.journal_dir,
        g.lower.clone(),
        g.state_dir.clone(),
        &|op| {
            authz::authorize(&g.policy_path, op)
                .and_then(|a| a.require(op, g.yes_irreversible).map(|_| a))
                .map(|_| ())
        },
    )
}

/// `cortex do <template> k=v ...` — run a known-good template with no LLM.
/// This is the zero-latency path: no model call, no network, just the
/// human-written triple.
fn cmd_do(args: &[String], g: &Global) -> Result<()> {
    let Some(id) = args.first() else {
        cmd_templates()?;
        bail!("`do` needs a template id (listed above)");
    };
    let template = registry::lookup(id).with_context(|| {
        format!("unknown template `{id}` — run `cortex templates` to see them all")
    })?;

    let mut bound_args = BTreeMap::new();
    for kv in &args[1..] {
        let (k, v) = kv
            .split_once('=')
            .with_context(|| format!("expected key=value, got `{kv}`"))?;
        bound_args.insert(k.to_string(), v.to_string());
    }

    // Authorize the operation with its *bound* arguments, before binding the
    // command line. A rule that constrains `image=nginx*` must see the image.
    g.authorize(&Operation::Template {
        id,
        args: &bound_args,
    })?;

    let bound = template.bind(&bound_args)?;
    let mut w = workflow::Workflow::template(bound).journal_dir(&g.journal_dir);
    if let Some(l) = &g.lower {
        w = w.lower(l.clone());
    }
    if let Some(s) = &g.state_dir {
        w = w.state_dir(s.clone());
    }
    w.run()
}

fn cmd_undo(args: &[String], g: &Global) -> Result<()> {
    // Undo is authorized like anything else, but the default policy always
    // permits it: a rule that can lock an operator out of their own rollback
    // is a liability, not a control.
    g.authorize(&Operation::Undo)?;
    let id = args.first().map(String::as_str);
    match (g.all, id) {
        (true, Some(_)) => bail!("--all and an explicit id are mutually exclusive"),
        (true, None) => workflow::undo_all(&g.journal_dir, g.force),
        (false, _) => workflow::undo(&g.journal_dir, id, g.force),
    }
}

/// `cortex verify --self` — the conformance suite, runnable by a skeptic.
fn cmd_verify(args: &[String]) -> Result<()> {
    if args.first().map(String::as_str) != Some("--self") {
        bail!("usage: cortex verify --self");
    }
    workflow::verify_self()
}

fn cmd_templates() -> Result<()> {
    ui::section("templates");
    for t in registry::TEMPLATES {
        println!("  {:<22} {}", ui::bold(t.id), ui::dim(t.summary));
        println!("    {} {}", ui::dim("params:"), t.params.join(", "));
    }
    println!(
        "\n  {}",
        ui::dim("every template ships a human-written inverse and a post-condition that proves it")
    );
    Ok(())
}

/// `cortex demo` — prove the guarantee on this machine in ~2 seconds, with no
/// root, no docker, no network. It commits a real filesystem change in a
/// throwaway lower dir, shows drift refusal, then a clean verified undo.
///
/// This is the 60-second evaluation a DevOps engineer gives a new tool: it
/// must *show* the differentiator, not describe it.
fn cmd_demo(g: &Global) -> Result<()> {
    use std::fs;

    ui::section("cortex demo");
    println!(
        "  {}\n",
        ui::dim("committing a real change, then proving undo is safe — no root, no docker")
    );

    // A self-contained sandbox: our own lower dir and journal in a temp space,
    // so the demo never touches the host.
    let base = std::env::temp_dir().join(format!("cortex-demo-{}", std::process::id()));
    let lower = base.join("etc");
    let journal_dir = base.join("journal");
    fs::create_dir_all(&lower)?;
    let conf = lower.join("app.conf");
    fs::write(&conf, "listen 80\n")?;

    // We drive the journal directly (the demo must not require overlay mounts,
    // which need root). This exercises the exact capture → seal → undo path.
    let journal = cortex_core::journal::Journal::new(&journal_dir);
    let upper = base.join("upper");
    fs::create_dir_all(&upper)?;
    fs::write(upper.join("app.conf"), "listen 8080\n")?;

    let step = ui::Step::start("commit: change listen 80 → 8080");
    let entry = journal.capture(
        &upper,
        &lower,
        "demo",
        None,
        "listen 8080",
        None,
        None,
        None,
    )?;
    fs::write(&conf, "listen 8080\n")?; // the merge
    let entry = journal.seal(&entry)?;
    step.ok_with(&format!("entry {}", &entry.id[..17]));
    println!(
        "    {} {}",
        ui::dim("now:"),
        fs::read_to_string(&conf)?.trim()
    );

    // 1. Drift refusal: a colleague edits the same file.
    ui::section("someone else edits the file");
    fs::write(&conf, "listen 8443 # hotfix\n")?;
    println!("    {} listen 8443 # hotfix", ui::dim("their change:"));
    let step = ui::Step::start("undo (safe): should REFUSE to clobber the hotfix");
    match journal.undo(None, false) {
        Ok(_) => {
            step.fail("undo did not detect drift — this is a bug");
            bail!("demo invariant broken: undo overwrote a concurrent edit");
        }
        Err(_) => step.ok_with("refused; the hotfix is safe"),
    }
    println!(
        "    {} {}",
        ui::dim("file still:"),
        fs::read_to_string(&conf)?.trim()
    );

    // 2. Clean undo: restore the file, then undo succeeds and verifies.
    ui::section("resolve the drift, then undo");
    fs::write(&conf, "listen 8080\n")?; // back to what cortex left
    let step = ui::Step::start("undo: restore the pre-change config");
    journal.undo(None, false)?;
    step.ok();
    println!(
        "    {} {}",
        ui::dim("restored:"),
        fs::read_to_string(&conf)?.trim()
    );

    let _ = fs::remove_dir_all(&base);
    let _ = g; // journal_dir override doesn't apply to the sandboxed demo

    println!(
        "\n{} {}",
        ui::green("✔"),
        ui::bold("that is the whole product: a change you can take back, with proof.")
    );
    println!(
        "  {} {}",
        ui::dim("try it for real:"),
        ui::bold("cortex try \"run nginx on port 8080\"")
    );
    println!(
        "  {} {}",
        ui::dim("prove every template:"),
        ui::bold("cortex verify --self")
    );
    Ok(())
}

/// The pre-1.0 `cortex workflow …` surface, kept working.
fn legacy_workflow(args: &[String], g: &Global) -> Result<()> {
    let Some(sub) = args.first().map(String::as_str) else {
        bail!("missing workflow name\n\n{USAGE}");
    };
    let mut flags: BTreeMap<String, String> = BTreeMap::new();
    let mut it = args[1..].iter();
    while let Some(f) = it.next() {
        if let Some(name) = f.strip_prefix("--") {
            let v = it.next().cloned().unwrap_or_else(|| "true".to_string());
            flags.insert(name.to_string(), v);
        }
    }
    let need = |k: &str| -> Result<String> {
        flags
            .get(k)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("--{k} is required"))
    };

    // The legacy surface is a different spelling of the same operations, so
    // it goes through the same gate. `undo`/`history` authorize themselves.
    if !matches!(sub, "undo" | "history") {
        g.authorize(&Operation::Workflow { kind: sub })?;
    }

    let w = match sub {
        "safe-config" => workflow::Workflow::safe_config(need("service")?, need("cmd")?),
        "safe-install" => workflow::Workflow::safe_install(need("package")?),
        "safe-file-edit" => workflow::Workflow::safe_file_edit(need("file")?, need("cmd")?),
        "safe-service" => workflow::Workflow::safe_service(need("op")?.parse()?, need("service")?),
        "safe-symlink-swap" => {
            workflow::Workflow::safe_symlink_swap(need("link")?, need("target")?)
        }
        "safe-cron-install" => workflow::Workflow::safe_cron_install(
            flags.get("user").cloned().unwrap_or_else(|| "root".into()),
            need("entry")?,
        ),
        "safe-dependency-upgrade" => {
            workflow::Workflow::safe_dependency_upgrade(need("manager")?.parse()?, need("package")?)
        }
        "safe-db-migration" => bail!(
            "`safe-db-migration` was removed: a migration's undo is inherently \
             lossy (DROP COLUMN does not restore the data), so calling it \
             'reversible' would be a lie. If you need this, a snapshot-backed \
             template that dumps the affected tables and restores from the dump \
             is the honest design — it is not built yet."
        ),
        "safe-run" => bail!(
            "`safe-run` is gone: a caller-authored inverse cannot be trusted \
             (an inverse that exits 0 without reversing anything was accepted \
             and reported as a successful undo).\n\nUse a registry template \
             instead:\n  cortex do docker.run name=web image=nginx ports=8080:80\n\
             \nSee `cortex templates`. For something with no template, \
             `cortex try` will offer the irreversible path with consent."
        ),
        "undo" => return cmd_undo(&[], g),
        "history" => return workflow::history(&g.journal_dir),
        other => bail!("unknown workflow `{other}`"),
    };
    let mut w = w.journal_dir(&g.journal_dir);
    if let Some(l) = &g.lower {
        w = w.lower(l.clone());
    }
    if let Some(s) = &g.state_dir {
        w = w.state_dir(s.clone());
    }
    w.run()
}
