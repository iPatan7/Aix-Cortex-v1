//! Render a plan before it runs: what will execute, what proves it worked,
//! what undoes it, and what proves the undo. The plan stage is not optional
//! chrome — showing the operator the exact commands *before* anything
//! executes is half the product's promise (`--plan` stops here entirely).

use anyhow::{bail, Context, Result};
use cortex_core::ui;
use cortex_registry::{Bound, Template};
use serde_json::Value;
use std::collections::BTreeMap;

fn line(label: &str, text: &str) {
    println!("  {:<12} {}", ui::dim(label), text);
}

/// Render any plan object the dispatcher can execute.
pub fn render(plan: &Value) -> Result<()> {
    render_titled(plan, "plan")
}

/// The same, with a caller-chosen section title (composite plans number
/// their steps).
pub fn render_titled(plan: &Value, title: &str) -> Result<()> {
    let kind = plan
        .get("workflow")
        .and_then(Value::as_str)
        .context("plan names no workflow")?;
    ui::section(title);
    match kind {
        "template" => {
            let id = plan
                .get("template")
                .and_then(Value::as_str)
                .context("plan names no template")?;
            let t = cortex_registry::lookup(id)
                .with_context(|| format!("plan names unknown template `{id}`"))?;
            let args: BTreeMap<String, String> = plan
                .get("args")
                .and_then(Value::as_object)
                .map(|m| {
                    m.iter()
                        .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                        .collect()
                })
                .unwrap_or_default();
            let bound = t.bind(&args)?;
            render_bound(t, &bound);
        }
        "safe-service" => {
            let op = plan.get("op").and_then(Value::as_str).unwrap_or("?");
            let service = plan.get("service").and_then(Value::as_str).unwrap_or("?");
            line("workflow", &format!("service {op} {service}"));
            line("run", &format!("systemctl {op} {service}"));
            line(
                "undo",
                "the inverse of the transition that actually happens (a unit already \
                 in the target state journals nothing)",
            );
            line(
                "prove undo",
                "the opposite state check, journaled with the entry",
            );
        }
        other => {
            // A legacy workflow: name it and its arguments; the workflow
            // prints its own commands as it stages them.
            line("workflow", other);
            if let Some(obj) = plan.as_object() {
                for (k, v) in obj {
                    if k != "workflow" {
                        line(k, v.as_str().unwrap_or(&v.to_string()));
                    }
                }
            }
        }
    }
    Ok(())
}

/// Render a bound template: the four commands, verbatim.
pub fn render_bound(t: &Template, b: &Bound) {
    line("template", &format!("{} — {}", ui::bold(&t.id), t.summary));
    line(
        "args",
        &b.args
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join("  "),
    );
    line("run", &b.forward);
    line("prove", &b.verify_forward);
    if b.inverse == cortex_registry::FS_RESTORE {
        line(
            "undo",
            "restore every touched file from the journal, byte for byte",
        );
    } else {
        line("undo", &b.inverse);
        line("prove undo", &b.verify_inverse);
    }
    if !b.host_side {
        line(
            "sandbox",
            "runs in an OverlayFS transaction first; the host changes only on verified commit",
        );
    }
    if !t.drift_note.is_empty() {
        line("drift", &t.drift_note);
    }
}

/// The `--plan` epilogue: how to actually run it.
pub fn plan_only_footer() {
    println!(
        "\n{} {}",
        ui::dim("plan only —"),
        ui::dim("nothing was executed. Re-run without --plan to apply."),
    );
}

/// A planner miss that still identified the template: show what is missing
/// and the exact command that would run.
pub fn render_needs_input(n: &cortex_planner::NeedsInput) -> Result<()> {
    ui::section(&format!("understood: {}", n.template_id));
    println!("  {}", ui::dim(&n.summary));
    println!();
    for (name, about, kind) in &n.missing {
        println!(
            "  {} {:<10} {} {}",
            ui::yellow("?"),
            ui::bold(name),
            about,
            ui::dim(&format!("({kind})"))
        );
    }
    println!(
        "\n  {} {}",
        ui::dim("run it with:"),
        ui::bold(&n.do_command)
    );
    bail!("missing {} parameter(s)", n.missing.len())
}

/// A planner miss with nothing bindable: say why (when known) and point at
/// the nearest templates rather than shrugging.
pub fn render_unknown(description: &str, u: &cortex_planner::Unknown) -> String {
    if let Some(reason) = &u.reason {
        ui::warn(reason);
    }
    if !u.suggestions.is_empty() {
        ui::section("closest templates");
        for s in &u.suggestions {
            println!("  {:<20} {}", ui::bold(&s.id), ui::dim(&s.summary));
            println!("    {} {}", ui::dim("e.g."), s.example);
        }
        println!();
    }
    format!(
        "could not turn \"{description}\" into a reversible plan. \
         See `cortex templates` for everything cortex can do, or run a \
         template directly: cortex do <template> key=value ..."
    )
}
