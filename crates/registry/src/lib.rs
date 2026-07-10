//! The template registry: known-good (forward, inverse, verify) triples.
//!
//! An inverse is a *semantic claim* that no compiler can check — `saga.rs`
//! says as much. The previous design let an LLM author that claim freely,
//! which meant the safety property of the whole system rested on a model not
//! hallucinating. It did not survive contact: `--undo-cmd "echo done"` was
//! accepted, committed, and reported as a successful undo while the container
//! it was supposed to destroy kept running.
//!
//! So the model no longer writes inverses. It *selects a template* and fills
//! in parameters. Each template ships three commands written by a human:
//!
//! - `forward` — what to do
//! - `inverse` — what undoes it
//! - `verify_forward` — proves the forward command took effect
//! - `verify_inverse` — proves the inverse took effect
//!
//! The verifiers are the point. A compensation that exits 0 proves nothing
//! (`echo` exits 0); a compensation whose post-condition holds proves the
//! world actually changed back. Undo runs the inverse and then *checks*.
//!
//! Anything outside the registry is not reversible. Cortex will still run it,
//! but only as an explicitly `Irreversible` operation that the policy engine
//! authorised and the operator consented to — never silently.

use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// A parameterised, reversible operation. Commands are shell templates in
/// which `{name}` is replaced by the named argument, shell-quoted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Template {
    /// Stable identifier the planner selects by name.
    pub id: &'static str,
    /// One line describing what it does, shown in plans and `cortex explain`.
    pub summary: &'static str,
    /// Parameters this template requires, in prompt order.
    pub params: &'static [&'static str],
    /// The command that performs the operation.
    pub forward: &'static str,
    /// Proves `forward` took effect. Runs after it, must exit 0.
    pub verify_forward: &'static str,
    /// The command that reverses the operation.
    pub inverse: &'static str,
    /// Proves `inverse` took effect. Runs after it, must exit 0. This is
    /// what makes undo trustworthy rather than merely attempted.
    pub verify_inverse: &'static str,
    /// True when the operation's effects live outside the filesystem
    /// (dockerd, systemd, a database), so the overlay cannot capture them
    /// and the journaled compensation *is* the whole undo.
    pub host_side: bool,
}

/// Every reversible operation cortex knows how to undo, by name.
///
/// Adding a template is the way to extend cortex. Each entry is a promise a
/// human made and a test can check: `cortex verify --self` runs every
/// template's forward, asserts `verify_forward`, runs the inverse, and
/// asserts `verify_inverse`.
pub const TEMPLATES: &[Template] = &[
    Template {
        id: "docker.run",
        summary: "Run a container detached, published on a host port",
        params: &["name", "image", "ports"],
        // `--restart=no` so an undone container cannot be resurrected by the
        // daemon; the name is what the inverse addresses.
        forward: "docker run -d --restart=no --name {name} -p {ports} {image}",
        verify_forward: "docker ps --filter name=^{name}$ --filter status=running -q | grep -q .",
        inverse: "docker rm -f {name}",
        // The post-condition that `echo` could never satisfy.
        verify_inverse: "! docker ps -a --filter name=^{name}$ -q | grep -q .",
        host_side: true,
    },
    Template {
        id: "docker.compose.up",
        summary: "Bring up a compose project",
        params: &["project", "file"],
        forward: "docker compose -p {project} -f {file} up -d",
        verify_forward: "docker compose -p {project} -f {file} ps --status running -q | grep -q .",
        inverse: "docker compose -p {project} -f {file} down -v",
        verify_inverse: "! docker compose -p {project} -f {file} ps -q | grep -q .",
        host_side: true,
    },
    Template {
        id: "service.start",
        summary: "Start a systemd unit",
        params: &["unit"],
        forward: "systemctl start {unit}",
        verify_forward: "systemctl is-active --quiet {unit}",
        inverse: "systemctl stop {unit}",
        verify_inverse: "! systemctl is-active --quiet {unit}",
        host_side: true,
    },
    Template {
        id: "service.stop",
        summary: "Stop a systemd unit",
        params: &["unit"],
        forward: "systemctl stop {unit}",
        verify_forward: "! systemctl is-active --quiet {unit}",
        inverse: "systemctl start {unit}",
        verify_inverse: "systemctl is-active --quiet {unit}",
        host_side: true,
    },
    Template {
        id: "service.enable",
        summary: "Enable a systemd unit at boot",
        params: &["unit"],
        forward: "systemctl enable {unit}",
        verify_forward: "systemctl is-enabled --quiet {unit}",
        inverse: "systemctl disable {unit}",
        verify_inverse: "! systemctl is-enabled --quiet {unit}",
        host_side: true,
    },
    Template {
        id: "package.install",
        summary: "Install an apt package",
        params: &["package"],
        forward: "DEBIAN_FRONTEND=noninteractive apt-get install -y {package}",
        verify_forward: "dpkg-query -W -f='${{Status}}' {package} | grep -q '^install ok installed'",
        // Filesystem-backed: the overlay captured the package's files, so the
        // journal's inverse layer is the real undo. This command removes the
        // dpkg registration that the inverse layer restores separately.
        inverse: "DEBIAN_FRONTEND=noninteractive apt-get remove -y {package}",
        verify_inverse: "! dpkg-query -W -f='${{Status}}' {package} 2>/dev/null | grep -q '^install ok installed'",
        host_side: false,
    },
    Template {
        id: "symlink.swap",
        summary: "Repoint a symlink (blue/green)",
        params: &["link", "target", "previous"],
        forward: "test -e {target} && ln -sfn {target} {link}",
        verify_forward: "[ \"$(readlink {link})\" = {target} ]",
        // Explicit rather than relying on the overlay restore: the inverse is
        // a real command with a real post-condition, so `cortex verify --self`
        // can exercise it end to end like any other template.
        inverse: "ln -sfn {previous} {link}",
        verify_inverse: "[ \"$(readlink {link})\" = {previous} ]",
        host_side: true,
    },
];

/// Find a template by id.
pub fn lookup(id: &str) -> Option<&'static Template> {
    TEMPLATES.iter().find(|t| t.id == id)
}

/// A template with its parameters bound — the concrete commands to run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Bound {
    pub template_id: String,
    pub args: BTreeMap<String, String>,
    pub forward: String,
    pub verify_forward: String,
    pub inverse: String,
    pub verify_inverse: String,
    pub host_side: bool,
}

impl Template {
    /// Bind arguments to this template, rendering the four commands.
    ///
    /// Every argument is shell-quoted at substitution, so a parameter can
    /// never inject shell syntax into a command a human wrote. Missing or
    /// extra parameters are refused: a template invoked wrongly is not the
    /// operation its verifier was written for.
    pub fn bind(&self, args: &BTreeMap<String, String>) -> Result<Bound> {
        for want in self.params {
            let value = args
                .get(*want)
                .map(String::as_str)
                .filter(|v| !v.trim().is_empty());
            if value.is_none() {
                bail!("template `{}` requires parameter `{want}`", self.id);
            }
        }
        for got in args.keys() {
            if !self.params.contains(&got.as_str()) {
                bail!(
                    "template `{}` has no parameter `{got}` (expected: {})",
                    self.id,
                    self.params.join(", ")
                );
            }
        }
        Ok(Bound {
            template_id: self.id.to_string(),
            forward: render(self.forward, args),
            verify_forward: render(self.verify_forward, args),
            inverse: render(self.inverse, args),
            verify_inverse: render(self.verify_inverse, args),
            host_side: self.host_side,
            args: args.clone(),
        })
    }
}

/// Substitute `{name}` with the shell-quoted argument. `{{` and `}}` are
/// literal braces, so a command may contain shell brace syntax
/// (`${{Status}}` renders as `${Status}`).
fn render(tmpl: &str, args: &BTreeMap<String, String>) -> String {
    let mut out = String::with_capacity(tmpl.len());
    let mut chars = tmpl.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '{' if chars.peek() == Some(&'{') => {
                chars.next();
                out.push('{');
            }
            '}' if chars.peek() == Some(&'}') => {
                chars.next();
                out.push('}');
            }
            '{' => {
                let mut name = String::new();
                for c in chars.by_ref() {
                    if c == '}' {
                        break;
                    }
                    name.push(c);
                }
                match args.get(&name) {
                    Some(v) => out.push_str(&shell_quote(v)),
                    // bind() has already checked every placeholder is bound.
                    None => out.push_str(&format!("{{{name}}}")),
                }
            }
            c => out.push(c),
        }
    }
    out
}

/// Single-quote a value for embedding in `sh -c`.
pub fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn docker_run_binds_to_a_verifiable_pair() {
        let t = lookup("docker.run").unwrap();
        let b = t
            .bind(&args(&[
                ("name", "web"),
                ("image", "nginx"),
                ("ports", "8080:80"),
            ]))
            .unwrap();
        assert_eq!(
            b.forward,
            "docker run -d --restart=no --name 'web' -p '8080:80' 'nginx'"
        );
        assert_eq!(b.inverse, "docker rm -f 'web'");
        // The inverse's post-condition is what `echo done` could never pass.
        assert!(b.verify_inverse.starts_with("! docker ps -a"));
        assert!(b.verify_inverse.contains("'web'"));
    }

    #[test]
    fn parameters_are_shell_quoted_not_interpolated() {
        let t = lookup("service.start").unwrap();
        let b = t.bind(&args(&[("unit", "evil; rm -rf /")])).unwrap();
        // The injection is a single quoted argument, not two commands.
        assert_eq!(b.forward, "systemctl start 'evil; rm -rf /'");
        assert!(!b.forward.contains("start evil;"));
    }

    #[test]
    fn escaped_braces_survive_rendering() {
        let t = lookup("package.install").unwrap();
        let b = t.bind(&args(&[("package", "nginx")])).unwrap();
        // `${{Status}}` in the template must render as shell `${Status}`.
        assert!(
            b.verify_forward.contains("${Status}"),
            "got: {}",
            b.verify_forward
        );
        assert!(!b.verify_forward.contains("${{"));
    }

    #[test]
    fn missing_or_unknown_parameters_are_refused() {
        let t = lookup("docker.run").unwrap();
        assert!(
            t.bind(&args(&[("name", "web")])).is_err(),
            "missing image/ports"
        );
        assert!(
            t.bind(&args(&[
                ("name", "web"),
                ("image", "nginx"),
                ("ports", "80:80"),
                ("extra", "x")
            ]))
            .is_err(),
            "unknown parameter must be refused"
        );
        assert!(
            t.bind(&args(&[
                ("name", " "),
                ("image", "nginx"),
                ("ports", "80:80")
            ]))
            .is_err(),
            "blank parameter must be refused"
        );
    }

    #[test]
    fn unknown_template_is_not_found() {
        assert!(lookup("docker.rm-rf-the-universe").is_none());
    }

    /// Every template must be internally coherent: it names its parameters,
    /// every placeholder it uses is a declared parameter, and it carries a
    /// verifier for BOTH directions. A template with an unverified inverse
    /// would reintroduce exactly the bug this module exists to prevent.
    #[test]
    fn every_template_is_well_formed() {
        for t in TEMPLATES {
            assert!(!t.id.is_empty() && !t.summary.is_empty(), "{}", t.id);
            assert!(
                !t.verify_forward.trim().is_empty(),
                "{} has no forward verifier",
                t.id
            );
            assert!(
                !t.verify_inverse.trim().is_empty(),
                "{} has no INVERSE verifier — undo could not be trusted",
                t.id
            );

            // Every placeholder must be a declared parameter.
            for cmd in [t.forward, t.verify_forward, t.inverse, t.verify_inverse] {
                for name in placeholders(cmd) {
                    assert!(
                        t.params.contains(&name.as_str()),
                        "template {} uses undeclared placeholder {{{name}}} in: {cmd}",
                        t.id
                    );
                }
            }

            // And binding with every parameter present must succeed.
            let a = args(&t.params.iter().map(|p| (*p, "x")).collect::<Vec<_>>());
            t.bind(&a)
                .unwrap_or_else(|e| panic!("{} failed to bind: {e}", t.id));
        }
    }

    /// Template ids are the planner's vocabulary and the journal's record;
    /// a duplicate would make `lookup` silently ambiguous.
    #[test]
    fn template_ids_are_unique() {
        let mut ids: Vec<_> = TEMPLATES.iter().map(|t| t.id).collect();
        ids.sort_unstable();
        let before = ids.len();
        ids.dedup();
        assert_eq!(before, ids.len(), "duplicate template id");
    }

    /// Collect `{name}` placeholders, honouring `{{`/`}}` escapes.
    fn placeholders(tmpl: &str) -> Vec<String> {
        let mut out = Vec::new();
        let mut chars = tmpl.chars().peekable();
        while let Some(c) = chars.next() {
            match c {
                '{' if chars.peek() == Some(&'{') => {
                    chars.next();
                }
                '}' if chars.peek() == Some(&'}') => {
                    chars.next();
                }
                '{' => {
                    let mut name = String::new();
                    for c in chars.by_ref() {
                        if c == '}' {
                            break;
                        }
                        name.push(c);
                    }
                    out.push(name);
                }
                _ => {}
            }
        }
        out
    }
}
