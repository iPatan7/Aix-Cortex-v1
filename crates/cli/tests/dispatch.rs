//! The command dispatcher's routing rules, exercised through the real binary.
//!
//! These lock in a UX fix: `cortex "run nginx on port 8080"` (no `try` verb)
//! must be treated as a task, while a typo'd subcommand must still error
//! rather than being silently sent to the planner.

use std::process::Command;

fn cortex(args: &[&str]) -> (String, bool) {
    let out = Command::new(env!("CARGO_BIN_EXE_cortex"))
        // No LLM endpoint, so a task that can't be matched offline fails fast
        // with a message rather than hanging on a network call.
        .env_remove("CORTEX_LLM_ENDPOINT")
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

/// A quoted sentence with no verb is a task: it routes to `try`, which here
/// (no LLM, a port named) produces the ambiguity message — proving it reached
/// the planner rather than the "unknown command" path.
#[test]
fn bare_quoted_english_is_treated_as_a_task() {
    let (out, ok) = cortex(&["run nginx on port 8080"]);
    assert!(
        !ok,
        "should fail without an LLM, but as a task not a bad command"
    );
    assert!(
        out.contains("names a port") || out.contains("ambiguous"),
        "bare english must route to `try`, got: {out}"
    );
    assert!(
        !out.contains("unknown command"),
        "must NOT be treated as an unknown subcommand: {out}"
    );
}

/// A single mistyped word is almost certainly a subcommand typo, not a task.
/// It must error clearly, not get silently planned.
#[test]
fn a_typo_subcommand_still_errors() {
    let (out, ok) = cortex(&["stauts"]);
    assert!(!ok);
    assert!(out.contains("unknown command"), "got: {out}");
    // And it should point the user at the task form.
    assert!(out.contains("cortex try"), "got: {out}");
}

/// The explicit `try` verb keeps working unchanged.
#[test]
fn the_try_verb_still_works() {
    let (out, _) = cortex(&["try", "run nginx on port 8080"]);
    assert!(
        out.contains("names a port") || out.contains("ambiguous"),
        "got: {out}"
    );
    assert!(!out.contains("unknown command"), "got: {out}");
}

/// A real subcommand is never mistaken for a task.
#[test]
fn real_subcommands_are_not_hijacked() {
    let (out, ok) = cortex(&["demo"]);
    assert!(ok, "demo should succeed: {out}");
    assert!(out.contains("whole product"), "got: {out}");
}
