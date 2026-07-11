//! The command dispatcher and planner UX, exercised through the real binary.
//!
//! These lock in the product's promises about *input*: approximate English
//! plans the right template, `--plan` shows everything and executes nothing,
//! typos get suggestions, and a miss teaches the exact command — all fully
//! offline (no LLM endpoint is configured in any of these tests).

use std::process::Command;

fn cortex(args: &[&str]) -> (String, bool) {
    let out = Command::new(env!("CARGO_BIN_EXE_cortex"))
        // Fully offline and hermetic: no LLM endpoint, no user templates.
        .env_remove("CORTEX_LLM_ENDPOINT")
        .env("CORTEX_TEMPLATE_DIR", "/nonexistent-cortex-test-templates")
        .args(args)
        .output()
        .expect("run cortex");
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    (combined, out.status.success())
}

/// The hero command: plain English, no quotes needed, plans offline. With
/// `--plan` it must succeed without root, print the full contract (run,
/// prove, undo, prove undo), and execute nothing.
#[test]
fn plain_english_plans_offline() {
    let (out, ok) = cortex(&["--plan", "run", "nginx", "on", "port", "8080"]);
    assert!(ok, "planning must not need root or a model: {out}");
    assert!(out.contains("nginx.serve"), "got: {out}");
    assert!(out.contains("port=8080"), "got: {out}");
    assert!(out.contains("undo"), "the plan must show its undo: {out}");
    assert!(
        out.contains("nothing was executed"),
        "--plan must stop before execution: {out}"
    );
}

/// The spec's second hero command: `cortex deploy myapp image=foo
/// ports=80:8080` — a deploy is a named container run.
#[test]
fn deploy_with_parameters_plans_a_container() {
    let (out, ok) = cortex(&["--plan", "deploy", "myapp", "image=nginx", "ports=8080:80"]);
    assert!(ok, "got: {out}");
    assert!(out.contains("docker.run"), "got: {out}");
    assert!(out.contains("name=myapp"), "got: {out}");
    assert!(out.contains("docker rm -f 'myapp'"), "got: {out}");
}

/// The explicit `try` verb keeps working unchanged.
#[test]
fn the_try_verb_still_works() {
    let (out, ok) = cortex(&["--plan", "try", "run nginx on port 8080"]);
    assert!(ok, "got: {out}");
    assert!(out.contains("nginx.serve"), "got: {out}");
}

/// A quoted sentence with no verb at all is a task, not a subcommand.
#[test]
fn bare_quoted_english_is_treated_as_a_task() {
    let (out, ok) = cortex(&["--plan", "add user alice"]);
    assert!(ok, "got: {out}");
    assert!(out.contains("user.add"), "got: {out}");
    assert!(!out.contains("unknown command"), "got: {out}");
}

/// A single mistyped word is almost certainly a subcommand typo, not a task.
/// It must error clearly — and now, suggest the fix.
#[test]
fn a_typo_subcommand_errors_with_a_suggestion() {
    let (out, ok) = cortex(&["stauts"]);
    assert!(!ok);
    assert!(out.contains("unknown command"), "got: {out}");
    assert!(out.contains("did you mean `cortex status`"), "got: {out}");
}

/// A request that names a port must never be satisfied by a bare service
/// start that would silently drop the port. The refusal names alternatives.
#[test]
fn a_named_port_is_never_silently_dropped() {
    let (out, ok) = cortex(&["--plan", "start postgres on port 5432"]);
    assert!(!ok, "must refuse rather than guess: {out}");
    assert!(out.contains("names a port"), "got: {out}");
    assert!(!out.contains("unknown command"), "got: {out}");
}

/// A recognised template with missing parameters teaches the exact command
/// instead of failing with "unknown".
#[test]
fn a_partial_match_teaches_the_do_command() {
    let (out, ok) = cortex(&["--plan", "serve nginx"]);
    assert!(!ok, "cannot plan without the port: {out}");
    assert!(out.contains("understood: nginx.serve"), "got: {out}");
    assert!(out.contains("cortex do nginx.serve"), "got: {out}");
    assert!(out.contains("port=<port>"), "got: {out}");
}

/// A typo'd keyword still finds its template: "simlink" matches
/// symlink.swap, and the miss is a parameter lesson, not "unknown".
#[test]
fn a_typo_keyword_still_finds_the_template() {
    let (out, ok) = cortex(&["--plan", "please make the simlink point at /srv/v2"]);
    assert!(!ok, "parameters are missing, so it must not plan: {out}");
    assert!(out.contains("symlink.swap"), "got: {out}");
    assert!(out.contains("cortex do symlink.swap"), "got: {out}");
}

/// A request that hits some trigger words but no full template gets the
/// nearest templates and the pointer to the catalog — and no LLM is
/// consulted (none is configured; the failure must be instant and offline,
/// not a network timeout).
#[test]
fn an_unplannable_request_suggests_templates() {
    let (out, ok) = cortex(&["--plan", "do something clever with the firewall"]);
    assert!(!ok);
    assert!(out.contains("firewall.allow"), "should suggest: {out}");
    assert!(out.contains("cortex templates"), "got: {out}");
}

/// `cortex do` renders the same plan block before executing, and honours
/// `--plan`.
#[test]
fn do_shows_the_plan_and_dry_runs() {
    let (out, ok) = cortex(&[
        "--plan",
        "do",
        "docker.run",
        "name=web",
        "image=nginx",
        "ports=8080:80",
    ]);
    assert!(ok, "got: {out}");
    assert!(
        out.contains("docker run -d --restart=no --name 'web'"),
        "got: {out}"
    );
    assert!(out.contains("nothing was executed"), "got: {out}");
}

/// A typo'd template id in `do` suggests the real one.
#[test]
fn do_suggests_on_a_template_typo() {
    let (out, ok) = cortex(&["do", "docker.rnu", "name=x", "image=y", "ports=80:80"]);
    assert!(!ok);
    assert!(out.contains("did you mean `docker.run`"), "got: {out}");
}

/// `templates` lists by category; `templates show` prints the full contract.
#[test]
fn templates_list_and_show() {
    let (out, ok) = cortex(&["templates"]);
    assert!(ok, "got: {out}");
    for id in ["docker.run", "nginx.serve", "user.add", "firewall.allow"] {
        assert!(out.contains(id), "listing must contain {id}: {out}");
    }

    let (out, ok) = cortex(&["templates", "show", "nginx.serve"]);
    assert!(ok, "got: {out}");
    assert!(out.contains("parameters"), "got: {out}");
    assert!(out.contains("what runs"), "got: {out}");
    assert!(out.contains("drift"), "got: {out}");
}

/// A real subcommand is never mistaken for a task.
#[test]
fn real_subcommands_are_not_hijacked() {
    let (out, ok) = cortex(&["demo"]);
    assert!(ok, "demo should succeed: {out}");
    assert!(out.contains("whole product"), "got: {out}");
}

/// Composition: a conjunction plans every step, numbered, before anything
/// runs — and `--plan` still executes nothing.
#[test]
fn conjunctions_plan_every_step() {
    let (out, ok) = cortex(&["--plan", "install htop and open port 8080"]);
    assert!(ok, "got: {out}");
    assert!(out.contains("understood 2 steps"), "got: {out}");
    assert!(out.contains("step 1/2"), "got: {out}");
    assert!(out.contains("step 2/2"), "got: {out}");
    assert!(out.contains("package.install"), "got: {out}");
    assert!(out.contains("firewall.allow"), "got: {out}");
    assert!(out.contains("nothing was executed"), "got: {out}");
}

/// Phase-2 phrasings reach the phase-2 templates.
#[test]
fn phase_two_phrasings_plan_offline() {
    let (out, ok) = cortex(&["--plan", "install htop with dnf"]);
    assert!(ok, "got: {out}");
    assert!(out.contains("package.install-dnf"), "got: {out}");

    let (out, ok) = cortex(&[
        "--plan",
        "clone https://github.com/user/app.git to /srv/app",
    ]);
    assert!(ok, "got: {out}");
    assert!(out.contains("git.clone"), "got: {out}");
    assert!(out.contains("/srv/app"), "got: {out}");
}

/// "https" must never silently plan the plain-HTTP template: the TLS
/// template teaches its cert/key instead.
#[test]
fn https_is_never_silently_downgraded() {
    let (out, ok) = cortex(&["--plan", "serve nginx over https on port 8443"]);
    assert!(!ok, "cert/key are missing, so it must not plan: {out}");
    assert!(out.contains("nginx.tls"), "got: {out}");
    assert!(out.contains("cert=<cert>"), "got: {out}");
    assert!(
        !out.contains("nginx.serve —"),
        "must not fall back to plain http: {out}"
    );
}

/// `templates search` finds by approximate words and shows examples.
#[test]
fn templates_search_finds_and_teaches() {
    let (out, ok) = cortex(&["templates", "search", "container"]);
    assert!(ok, "got: {out}");
    assert!(out.contains("docker.run"), "got: {out}");
    assert!(out.contains("e.g."), "got: {out}");

    let (out, ok) = cortex(&["templates", "search", "swapfile"]);
    assert!(ok, "got: {out}");
    assert!(out.contains("swap.create"), "got: {out}");

    let (out, ok) = cortex(&["templates", "search", "zzzunmatchable"]);
    assert!(!ok);
    assert!(out.contains("cortex templates"), "got: {out}");

    let (out, ok) = cortex(&["templates", "search"]);
    assert!(!ok);
    assert!(out.contains("usage"), "got: {out}");
}

/// The new categories appear in the listing alongside the old ones.
#[test]
fn the_full_catalog_is_listed() {
    let (out, ok) = cortex(&["templates"]);
    assert!(ok, "got: {out}");
    for id in [
        "docker.app",
        "docker.volume.create",
        "nginx.tls",
        "package.install-dnf",
        "git.clone",
        "backup.dir",
        "sysctl.set",
        "swap.create",
        "sshd.set",
        "hosts.add",
        "certbot.issue",
    ] {
        assert!(out.contains(id), "listing must contain {id}: {out}");
    }
}

/// `undo last` is the same as bare `undo`, and an unknown name fails with
/// the journal's own message, not a selector panic.
#[test]
fn undo_selectors_resolve_or_fail_clearly() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().to_str().unwrap();

    let (out, ok) = cortex(&["--journal-dir", dir, "undo", "last"]);
    assert!(!ok);
    assert!(!out.contains("panicked"), "got: {out}");

    let (out, ok) = cortex(&["--journal-dir", dir, "undo", "no-such-thing"]);
    assert!(!ok);
    assert!(!out.contains("panicked"), "got: {out}");
}

/// `version` prints the version and how to update.
#[test]
fn version_prints_update_hint() {
    let (out, ok) = cortex(&["version"]);
    assert!(ok);
    assert!(out.contains(env!("CARGO_PKG_VERSION")), "got: {out}");
    assert!(out.contains("install.sh"), "got: {out}");
}

/// The help text advertises the whole v0.3 surface.
#[test]
fn help_covers_the_new_surface() {
    let (out, ok) = cortex(&["help"]);
    assert!(ok);
    assert!(out.contains("templates search"), "got: {out}");
    assert!(out.contains("undo [id|last|name]"), "got: {out}");
    assert!(
        out.contains("install htop and open port 8080"),
        "help must mention composition: {out}"
    );
}

/// The env/volume deploy: full contract shown, env and volume quoted in.
#[test]
fn do_docker_app_dry_runs_with_env_and_volume() {
    let (out, ok) = cortex(&[
        "--plan",
        "do",
        "docker.app",
        "name=web",
        "image=nginx",
        "ports=8080:80",
        "env=NODE_ENV=production",
        "volume=/srv/data:/data",
    ]);
    assert!(ok, "got: {out}");
    assert!(out.contains("-e 'NODE_ENV=production'"), "got: {out}");
    assert!(out.contains("-v '/srv/data:/data'"), "got: {out}");
    assert!(out.contains("nothing was executed"), "got: {out}");
}

/// `templates show` explains the declared-undo-target pattern.
#[test]
fn templates_show_a_phase_two_contract() {
    let (out, ok) = cortex(&["templates", "show", "sysctl.set"]);
    assert!(ok, "got: {out}");
    assert!(out.contains("previous"), "got: {out}");
    assert!(out.contains("drift"), "got: {out}");
}

/// "called <name>" fills the one parameter a volume needs, end to end.
#[test]
fn docker_volume_plans_from_english() {
    let (out, ok) = cortex(&["--plan", "create a docker volume called appdata"]);
    assert!(ok, "got: {out}");
    assert!(out.contains("docker.volume.create"), "got: {out}");
    assert!(out.contains("docker volume create 'appdata'"), "got: {out}");
}

/// An empty journal answers status/history/receipt calmly, never with a
/// panic or a raw io error.
#[test]
fn empty_journal_reads_are_calm() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().to_str().unwrap();

    let (out, ok) = cortex(&["--journal-dir", dir, "status"]);
    assert!(ok, "got: {out}");
    assert!(out.contains("clean"), "got: {out}");

    let (out, ok) = cortex(&["--journal-dir", dir, "history"]);
    assert!(ok, "got: {out}");
    assert!(out.contains("empty"), "got: {out}");

    let (out, ok) = cortex(&["--journal-dir", dir, "receipt"]);
    assert!(!ok);
    assert!(out.contains("empty"), "got: {out}");
    assert!(!out.contains("panicked"), "got: {out}");
}

/// Composition survives the dispatcher's bare-words path too (no quotes).
#[test]
fn bare_words_compose_without_quotes() {
    let (out, ok) = cortex(&["--plan", "install", "htop", "and", "open", "port", "8080"]);
    assert!(ok, "got: {out}");
    assert!(out.contains("understood 2 steps"), "got: {out}");
}
