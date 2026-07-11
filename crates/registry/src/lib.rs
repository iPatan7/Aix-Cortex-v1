//! The template registry: known-good (forward, inverse, verify) triples.
//!
//! An inverse is a *semantic claim* that no compiler can check — `saga.rs`
//! says as much. The previous design let an LLM author that claim freely,
//! which meant the safety property of the whole system rested on a model not
//! hallucinating. It did not survive contact: `--undo-cmd "echo done"` was
//! accepted, committed, and reported as a successful undo while the container
//! it was supposed to destroy kept running.
//!
//! So nothing generated writes inverses. The planner *selects a template* and
//! fills in parameters. Each template ships four commands written by a human:
//!
//! - `forward` — what to do
//! - `verify_forward` — proves the forward command took effect
//! - `inverse` — what undoes it
//! - `verify_inverse` — proves the inverse took effect
//!
//! The verifiers are the point. A compensation that exits 0 proves nothing
//! (`echo` exits 0); a compensation whose post-condition holds proves the
//! world actually changed back. Undo runs the inverse and then *checks*.
//!
//! Templates come from two places, both human-written:
//!
//! - [`builtin::builtins`] — compiled in, reviewed with the code.
//! - [`loader`] — operator-written TOML files under `~/.cortex/templates/`
//!   (root-owned when cortex runs as root, for the same reason the policy
//!   file must be: rules the subject can rewrite are not rules).
//!
//! Anything outside the registry is not reversible. Cortex will still run it,
//! but only as an explicitly `Irreversible` operation that the policy engine
//! authorised and the operator consented to — never silently.

mod builtin;
pub mod loader;

use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::sync::OnceLock;

/// The inverse of a filesystem-backed template whose real undo is the
/// journal's inverse layer (files restored byte-for-byte from the overlay).
/// The literal command `true` is honest here: there is no *host-side* action
/// to compensate, and the executor skips journaling a compensation for it.
pub const FS_RESTORE: &str = "true";

/// What a parameter value is, and therefore how it is validated and how the
/// planner may extract it from free text. Validation is a UX property, not a
/// security one — every value is shell-quoted at render time regardless.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ParamKind {
    /// A name: container, unit, site. `[A-Za-z0-9_][A-Za-z0-9._-]*`
    Ident,
    /// A system user name.
    User,
    /// A container image reference (`nginx`, `redis:7`, `ghcr.io/a/b:v1`).
    Image,
    /// A single TCP/UDP port number.
    Port,
    /// A `host:container` port mapping like `8080:80`.
    PortMapping,
    /// A `host:container` volume mapping of two absolute paths, like
    /// `/srv/data:/data`.
    VolumeMapping,
    /// A `KEY=value` environment variable assignment.
    EnvVar,
    /// An absolute filesystem path with no `..` segments.
    AbsPath,
    /// An octal file mode like `0644` or `755`.
    Mode,
    /// One line of free text (no newlines; still shell-quoted).
    Line,
    /// Free text (still shell-quoted; never interpolated).
    Text,
}

impl ParamKind {
    /// Refuse a value that cannot be what this parameter means. The error
    /// names the expectation, because "invalid value" teaches nothing.
    pub fn validate(&self, name: &str, value: &str) -> Result<()> {
        let ok = match self {
            // `+` so apt names like g++ pass; it is inert once shell-quoted.
            Self::Ident => {
                value.len() <= 128
                    && value
                        .chars()
                        .next()
                        .is_some_and(|c| c.is_ascii_alphanumeric() || c == '_')
                    && value
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || "._-+".contains(c))
            }
            Self::User => {
                value.len() <= 32
                    && value
                        .chars()
                        .next()
                        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
                    && value
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || "._-".contains(c))
            }
            Self::Image => {
                value
                    .chars()
                    .next()
                    .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
                    && value.chars().all(|c| {
                        c.is_ascii_lowercase() || c.is_ascii_digit() || "._-/:@".contains(c)
                    })
            }
            Self::Port => value.parse::<u16>().is_ok_and(|p| p > 0),
            Self::PortMapping => match value.split_once(':') {
                Some((h, c)) => h.parse::<u16>().is_ok() && c.parse::<u16>().is_ok_and(|p| p > 0),
                None => false,
            },
            Self::VolumeMapping => match value.split_once(':') {
                Some((h, c)) => {
                    let abs = |p: &str| {
                        p.starts_with('/')
                            && !p.contains('\n')
                            && !p.split('/').any(|seg| seg == "..")
                    };
                    abs(h) && abs(c)
                }
                None => false,
            },
            Self::EnvVar => match value.split_once('=') {
                Some((k, v)) => {
                    !v.contains('\n')
                        && k.chars()
                            .next()
                            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
                        && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
                }
                None => false,
            },
            Self::AbsPath => {
                value.starts_with('/')
                    && !value.contains('\n')
                    && !value.split('/').any(|seg| seg == "..")
            }
            Self::Mode => {
                (3..=4).contains(&value.len()) && value.chars().all(|c| ('0'..='7').contains(&c))
            }
            Self::Line => !value.contains('\n'),
            Self::Text => true,
        };
        if !ok {
            bail!(
                "parameter `{name}` = `{value}` is not a valid {}",
                self.describe()
            );
        }
        Ok(())
    }

    /// Human name of the expectation, used in errors and `templates show`.
    pub fn describe(&self) -> &'static str {
        match self {
            Self::Ident => "name (letters, digits, . _ -)",
            Self::User => "user name",
            Self::Image => "container image reference",
            Self::Port => "port number (1-65535)",
            Self::PortMapping => "host:container port mapping (e.g. 8080:80)",
            Self::VolumeMapping => "host:container volume mapping (e.g. /srv/data:/data)",
            Self::EnvVar => "KEY=value environment variable",
            Self::AbsPath => "absolute path",
            Self::Mode => "octal file mode (e.g. 0644)",
            Self::Line => "single line of text",
            Self::Text => "text value",
        }
    }
}

/// One template parameter: its meaning, its type, and (optionally) the value
/// it takes when the caller does not supply one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Param {
    pub name: String,
    /// One line shown in `templates show` and in missing-parameter errors.
    pub about: String,
    pub kind: ParamKind,
    /// Applied when the caller omits the parameter. A parameter with no
    /// default is required.
    #[serde(default)]
    pub default: Option<String>,
}

impl Param {
    pub fn required(&self) -> bool {
        self.default.is_none()
    }
}

/// A parameterised, reversible operation. Commands are shell templates in
/// which `{name}` is replaced by the named argument, shell-quoted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Template {
    /// Stable identifier the planner selects by name.
    pub id: String,
    /// One line describing what it does, shown in plans and `cortex templates`.
    pub summary: String,
    /// Grouping for `cortex templates` (containers, services, users, ...).
    pub category: String,
    /// Trigger keyword groups for the planner. A template is a match
    /// candidate only when **every** group has at least one word present in
    /// the request. An empty list means "reachable only by `cortex do`".
    #[serde(default)]
    pub keywords: Vec<Vec<String>>,
    /// Optional verbs that raise the match score but are not required, so
    /// both "run nginx on 8080" and "nginx on 8080" reach the same template.
    #[serde(default)]
    pub verbs: Vec<String>,
    /// A copy-pasteable invocation, shown in listings and suggestions.
    pub example: String,
    /// Parameters, in the order they are asked for.
    pub params: Vec<Param>,
    /// The command that performs the operation.
    pub forward: String,
    /// Proves `forward` took effect. Runs after it, must exit 0.
    pub verify_forward: String,
    /// The command that reverses the operation, or [`FS_RESTORE`] when the
    /// journal's inverse layer is the whole undo.
    pub inverse: String,
    /// Proves `inverse` took effect. This is what makes undo trustworthy
    /// rather than merely attempted.
    pub verify_inverse: String,
    /// True when the operation's effects live outside the filesystem
    /// (dockerd, systemd, a database), so the overlay cannot capture them
    /// and the journaled compensation *is* the whole undo.
    pub host_side: bool,
    /// What happens if the world changes between commit and undo — shown in
    /// the plan so the operator knows the shape of the guarantee up front.
    #[serde(default)]
    pub drift_note: String,
}

/// Every template cortex knows: built-ins first, then operator-written ones
/// from [`loader::user_template_dir`]. Loaded once per process. A user file
/// that fails to load is reported on stderr and skipped — a broken extension
/// must not brick the tool it extends.
pub fn all() -> &'static [Template] {
    static ALL: OnceLock<Vec<Template>> = OnceLock::new();
    ALL.get_or_init(|| {
        let mut templates = builtin::builtins();
        match loader::load_user_templates(&templates) {
            Ok(mut user) => templates.append(&mut user),
            Err(e) => eprintln!("[cortex] warning: user templates not loaded: {e:#}"),
        }
        templates
    })
}

/// The compiled-in templates only (no user dir). What `verify --self` and
/// the tests run against.
pub fn builtins() -> &'static [Template] {
    static BUILTIN: OnceLock<Vec<Template>> = OnceLock::new();
    BUILTIN.get_or_init(builtin::builtins)
}

/// Find a template by id.
pub fn lookup(id: &str) -> Option<&'static Template> {
    all().iter().find(|t| t.id == id)
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
    /// never inject shell syntax into a command a human wrote. Missing
    /// parameters take their declared default or are refused; unknown or
    /// blank parameters are refused: a template invoked wrongly is not the
    /// operation its verifier was written for.
    pub fn bind(&self, args: &BTreeMap<String, String>) -> Result<Bound> {
        let mut bound_args = BTreeMap::new();
        for param in &self.params {
            let value = match args.get(&param.name) {
                Some(v) if !v.trim().is_empty() => v.clone(),
                Some(_) => bail!(
                    "template `{}`: parameter `{}` is blank",
                    self.id,
                    param.name
                ),
                None => match &param.default {
                    Some(d) => d.clone(),
                    None => bail!(
                        "template `{}` requires parameter `{}` ({})\n  e.g.  {}",
                        self.id,
                        param.name,
                        param.about,
                        self.example
                    ),
                },
            };
            param.kind.validate(&param.name, &value)?;
            bound_args.insert(param.name.clone(), value);
        }
        for got in args.keys() {
            if !self.params.iter().any(|p| &p.name == got) {
                bail!(
                    "template `{}` has no parameter `{got}` (expected: {})",
                    self.id,
                    self.params
                        .iter()
                        .map(|p| p.name.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                );
            }
        }
        Ok(Bound {
            template_id: self.id.clone(),
            forward: render(&self.forward, &bound_args),
            verify_forward: render(&self.verify_forward, &bound_args),
            inverse: render(&self.inverse, &bound_args),
            verify_inverse: render(&self.verify_inverse, &bound_args),
            host_side: self.host_side,
            args: bound_args,
        })
    }

    /// The parameters a caller must supply (no default), given what they
    /// have supplied so far.
    pub fn missing_params(&self, have: &BTreeMap<String, String>) -> Vec<&Param> {
        self.params
            .iter()
            .filter(|p| p.required() && !have.contains_key(&p.name))
            .collect()
    }

    /// A ready-to-edit `cortex do` line for this template: supplied values
    /// filled in, missing ones shown as `<placeholders>`.
    pub fn do_command(&self, have: &BTreeMap<String, String>) -> String {
        let mut parts = vec![format!("cortex do {}", self.id)];
        for p in &self.params {
            match have.get(&p.name) {
                Some(v) => parts.push(format!("{}={}", p.name, v)),
                None if p.required() => parts.push(format!("{}=<{}>", p.name, p.name)),
                None => {}
            }
        }
        parts.join(" ")
    }
}

/// Substitute `{name}` with the shell-quoted argument. `{{` and `}}` are
/// literal braces, so a command may contain shell brace syntax
/// (`${{Status}}` renders as `${Status}`).
pub fn render(tmpl: &str, args: &BTreeMap<String, String>) -> String {
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

/// Collect `{name}` placeholders, honouring `{{`/`}}` escapes. Used by the
/// well-formedness checks here and by the user template loader.
pub fn placeholders(tmpl: &str) -> Vec<String> {
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

/// Single-quote a value for embedding in `sh -c`.
pub fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// A template must be internally coherent before it may enter the registry:
/// verifiers in BOTH directions, every placeholder declared, valid defaults.
/// Built-ins are checked by tests; user templates are checked at load time —
/// same rule, both origins.
pub fn well_formed(t: &Template) -> Result<()> {
    if t.id.is_empty()
        || !t
            .id
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || ".-_".contains(c))
    {
        bail!("template id `{}` must be lowercase [a-z0-9.-_]", t.id);
    }
    if t.summary.is_empty() {
        bail!("template `{}` has no summary", t.id);
    }
    if t.verify_forward.trim().is_empty() {
        bail!("template `{}` has no forward verifier", t.id);
    }
    if t.verify_inverse.trim().is_empty() {
        bail!(
            "template `{}` has no INVERSE verifier — undo could not be trusted",
            t.id
        );
    }
    for cmd in [&t.forward, &t.verify_forward, &t.inverse, &t.verify_inverse] {
        for name in placeholders(cmd) {
            if !t.params.iter().any(|p| p.name == name) {
                bail!(
                    "template `{}` uses undeclared placeholder {{{name}}} in: {cmd}",
                    t.id
                );
            }
        }
    }
    for p in &t.params {
        if let Some(d) = &p.default {
            p.kind.validate(&p.name, d)?;
        }
    }
    Ok(())
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
        // An injection attempt fails *validation* now (a unit name cannot
        // contain `;`), which is even earlier than quoting.
        assert!(t.bind(&args(&[("unit", "evil; rm -rf /")])).is_err());
        // And a value that passes validation is still quoted, not spliced.
        let b = t.bind(&args(&[("unit", "my-app_2")])).unwrap();
        assert_eq!(b.forward, "systemctl start 'my-app_2'");
    }

    #[test]
    fn free_text_parameters_are_quoted() {
        let t = lookup("service.create").unwrap();
        let b = t
            .bind(&args(&[
                ("name", "pinger"),
                ("command", "/bin/sh -c 'ping x; rm -rf /'"),
            ]))
            .unwrap();
        // The whole command is one single-quoted argument to the unit file
        // writer — the embedded quote is escaped, not an escape hatch.
        assert!(b.forward.contains(r"'/bin/sh -c '\''ping x; rm -rf /'\'''"));
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
    fn defaults_fill_missing_parameters() {
        let t = lookup("nginx.serve").unwrap();
        let b = t.bind(&args(&[("port", "8080")])).unwrap();
        assert_eq!(b.args["root"], "/var/www/html");
        assert_eq!(b.args["name"], "site");
        assert!(b.forward.contains("'8080'"));

        // A missing-required error must teach the fix, not just refuse.
        let err = format!("{:#}", t.bind(&args(&[])).unwrap_err());
        assert!(err.contains("requires parameter `port`"), "{err}");
        assert!(err.contains("e.g."), "{err}");
    }

    #[test]
    fn typed_parameters_are_validated() {
        let t = lookup("docker.run").unwrap();
        let bad_port = t.bind(&args(&[
            ("name", "web"),
            ("image", "nginx"),
            ("ports", "eighty:80"),
        ]));
        let err = format!("{:#}", bad_port.unwrap_err());
        assert!(err.contains("host:container"), "{err}");

        let t = lookup("dir.create").unwrap();
        assert!(t.bind(&args(&[("path", "relative/path")])).is_err());
        assert!(t.bind(&args(&[("path", "/opt/../etc/shadow")])).is_err());
        assert!(t.bind(&args(&[("path", "/opt/app")])).is_ok());

        let t = lookup("file.deploy").unwrap();
        assert!(t
            .bind(&args(&[
                ("path", "/tmp/x"),
                ("content", "hi"),
                ("mode", "banana")
            ]))
            .is_err());
    }

    /// The container-app kinds: a volume must map two absolute paths, an env
    /// var must be a shell-safe KEY=value.
    #[test]
    fn volume_and_env_parameters_are_validated() {
        let vol = ParamKind::VolumeMapping;
        assert!(vol.validate("volume", "/srv/data:/data").is_ok());
        assert!(vol.validate("volume", "data:/data").is_err()); // relative host side
        assert!(vol.validate("volume", "/srv/../etc:/data").is_err()); // dot-dot
        assert!(vol.validate("volume", "/srv/data").is_err()); // no mapping

        let env = ParamKind::EnvVar;
        assert!(env.validate("env", "NODE_ENV=production").is_ok());
        assert!(env.validate("env", "_X=1").is_ok());
        assert!(env.validate("env", "1BAD=x").is_err()); // key can't start with digit
        assert!(env.validate("env", "NOVALUE").is_err()); // no assignment
        assert!(env.validate("env", "A=b\nc").is_err()); // newline smuggling
    }

    #[test]
    fn unknown_template_is_not_found() {
        assert!(lookup("docker.rm-rf-the-universe").is_none());
    }

    #[test]
    fn docker_app_binds_env_and_volume() {
        let t = lookup("docker.app").unwrap();
        let b = t
            .bind(&args(&[
                ("name", "app"),
                ("image", "myapp:v2"),
                ("ports", "8080:80"),
                ("env", "NODE_ENV=production"),
                ("volume", "/srv/data:/data"),
            ]))
            .unwrap();
        assert!(
            b.forward.contains("-e 'NODE_ENV=production'"),
            "{}",
            b.forward
        );
        assert!(b.forward.contains("-v '/srv/data:/data'"), "{}", b.forward);
        assert_eq!(b.inverse, "docker rm -f 'app'");
        // A relative host path or a bare env word must be refused early.
        assert!(t
            .bind(&args(&[
                ("name", "app"),
                ("image", "myapp"),
                ("ports", "8080:80"),
                ("env", "NODE_ENV=production"),
                ("volume", "data:/data"),
            ]))
            .is_err());
    }

    /// The shell renderings the tuning templates depend on: a quoted
    /// key=value pair concatenates into one argument.
    #[test]
    fn sysctl_and_swap_render_working_shell() {
        let t = lookup("sysctl.set").unwrap();
        let b = t
            .bind(&args(&[
                ("key", "vm.swappiness"),
                ("value", "10"),
                ("previous", "60"),
            ]))
            .unwrap();
        assert!(
            b.forward.contains("sysctl -w 'vm.swappiness'='10'"),
            "{}",
            b.forward
        );
        assert!(
            b.inverse.contains("sysctl -w 'vm.swappiness'='60'"),
            "{}",
            b.inverse
        );

        let t = lookup("swap.create").unwrap();
        let b = t.bind(&args(&[("size", "2G")])).unwrap();
        assert_eq!(b.args["path"], "/swapfile");
        // The forward must refuse to clobber an existing file.
        assert!(
            b.forward.starts_with("! test -e '/swapfile'"),
            "{}",
            b.forward
        );
    }

    #[test]
    fn nginx_tls_defaults_to_443_and_checks_the_key_material() {
        let t = lookup("nginx.tls").unwrap();
        let b = t
            .bind(&args(&[
                ("cert", "/etc/ssl/certs/site.pem"),
                ("key", "/etc/ssl/private/site.key"),
            ]))
            .unwrap();
        assert_eq!(b.args["port"], "443");
        // The forward refuses before touching nginx if the material is absent.
        assert!(
            b.forward.starts_with("test -s '/etc/ssl/certs/site.pem'"),
            "{}",
            b.forward
        );
        assert!(b.forward.contains("ssl_certificate"), "{}", b.forward);
        assert!(b.forward.contains("nginx -t"), "{}", b.forward);
    }

    /// The fs-backed phase-2 templates lean on the journal for undo, and
    /// their forwards verify real effects, not exit codes.
    #[test]
    fn fs_backed_phase_two_contracts() {
        let t = lookup("git.clone").unwrap();
        let b = t
            .bind(&args(&[
                ("repo", "https://github.com/user/app.git"),
                ("path", "/srv/app"),
            ]))
            .unwrap();
        assert!(b.verify_forward.contains("/.git"), "{}", b.verify_forward);
        assert_eq!(b.inverse, FS_RESTORE);

        let t = lookup("hosts.add").unwrap();
        let b = t
            .bind(&args(&[("ip", "10.0.0.5"), ("hostname", "db.internal")]))
            .unwrap();
        assert!(
            b.verify_forward.contains("grep -qw"),
            "{}",
            b.verify_forward
        );
        assert_eq!(b.inverse, FS_RESTORE);
    }

    /// sshd.set: the whole config is validated before the daemon reloads, in
    /// both directions — a bad drop-in must never take sshd down.
    #[test]
    fn sshd_set_validates_before_reloading() {
        let t = lookup("sshd.set").unwrap();
        let b = t
            .bind(&args(&[
                ("option", "PasswordAuthentication"),
                ("value", "no"),
            ]))
            .unwrap();
        assert!(b.forward.contains("sshd -t &&"), "{}", b.forward);
        assert!(b.forward.contains("sshd_config.d"), "{}", b.forward);
        assert!(b.verify_inverse.contains("sshd -t"), "{}", b.verify_inverse);
    }

    #[test]
    fn dnf_and_certbot_contracts() {
        let t = lookup("package.install-dnf").unwrap();
        let b = t.bind(&args(&[("package", "htop")])).unwrap();
        assert_eq!(b.verify_forward, "rpm -q 'htop'");
        assert_eq!(b.inverse, "dnf remove -y 'htop'");

        let t = lookup("certbot.issue").unwrap();
        let b = t
            .bind(&args(&[
                ("domain", "example.com"),
                ("email", "ops@example.com"),
            ]))
            .unwrap();
        assert!(
            b.verify_forward
                .contains("live/'example.com'/fullchain.pem"),
            "{}",
            b.verify_forward
        );
        assert_eq!(b.inverse, FS_RESTORE, "undo is the journal's file restore");
    }

    /// backup.dir must never be able to touch the source: the inverse only
    /// removes the archive it wrote.
    #[test]
    fn backup_undo_only_removes_the_archive() {
        let t = lookup("backup.dir").unwrap();
        let b = t
            .bind(&args(&[
                ("src", "/etc"),
                ("dest", "/var/backups/etc.tar.gz"),
            ]))
            .unwrap();
        assert_eq!(b.inverse, "rm -f '/var/backups/etc.tar.gz'");
        assert!(
            !b.inverse.contains("/etc'"),
            "undo must not name the source"
        );
        assert!(
            b.verify_inverse.contains("! test -e"),
            "{}",
            b.verify_inverse
        );
    }

    /// Every built-in must pass the same well-formedness gate user templates
    /// pass at load time, and binding with every parameter present must
    /// succeed. A template with an unverified inverse would reintroduce
    /// exactly the bug this module exists to prevent.
    #[test]
    fn every_builtin_is_well_formed_and_bindable() {
        for t in builtins() {
            well_formed(t).unwrap_or_else(|e| panic!("{}: {e:#}", t.id));
            assert!(!t.example.is_empty(), "{} has no example", t.id);
            assert!(!t.category.is_empty(), "{} has no category", t.id);

            let a: BTreeMap<String, String> = t
                .params
                .iter()
                .map(|p| (p.name.clone(), sample_value(&p.kind)))
                .collect();
            t.bind(&a)
                .unwrap_or_else(|e| panic!("{} failed to bind: {e:#}", t.id));
        }
    }

    fn sample_value(kind: &ParamKind) -> String {
        match kind {
            ParamKind::Ident | ParamKind::User => "sample".into(),
            ParamKind::Image => "nginx:latest".into(),
            ParamKind::Port => "8080".into(),
            ParamKind::PortMapping => "8080:80".into(),
            ParamKind::VolumeMapping => "/tmp/cortex-sample:/data".into(),
            ParamKind::EnvVar => "CORTEX_SAMPLE=1".into(),
            ParamKind::AbsPath => "/tmp/cortex-sample".into(),
            ParamKind::Mode => "0644".into(),
            ParamKind::Line => "sample line".into(),
            ParamKind::Text => "sample text".into(),
        }
    }

    /// Template ids are the planner's vocabulary and the journal's record;
    /// a duplicate would make `lookup` silently ambiguous.
    #[test]
    fn template_ids_are_unique() {
        let mut ids: Vec<_> = builtins().iter().map(|t| t.id.as_str()).collect();
        ids.sort_unstable();
        let before = ids.len();
        ids.dedup();
        assert_eq!(before, ids.len(), "duplicate template id");
    }

    /// The spec's Phase 1 families must all be present.
    #[test]
    fn phase_one_coverage() {
        for id in [
            "nginx.serve",
            "docker.run",
            "podman.run",
            "docker.compose.up",
            "service.start",
            "service.stop",
            "service.enable",
            "service.disable",
            "service.create",
            "package.install",
            "package.remove",
            "user.add",
            "user.add-sudo",
            "user.grant-sudo",
            "user.remove",
            "user.ssh-key",
            "file.deploy",
            "dir.create",
            "firewall.allow",
            "firewall.remove",
            "symlink.swap",
        ] {
            assert!(lookup(id).is_some(), "missing phase-1 template {id}");
        }
        assert!(builtins().len() >= 20, "spec asks for 20+ templates");
    }

    /// Phase 2: docker env/volumes/network, nginx TLS, dnf packages, git
    /// deploy, certbot, backup, tuning, ssh config, hosts entries.
    #[test]
    fn phase_two_coverage() {
        for id in [
            "docker.app",
            "docker.volume.create",
            "docker.network.create",
            "nginx.tls",
            "certbot.issue",
            "package.install-dnf",
            "package.remove-dnf",
            "git.clone",
            "backup.dir",
            "sysctl.set",
            "swap.create",
            "sshd.set",
            "hosts.add",
        ] {
            assert!(lookup(id).is_some(), "missing phase-2 template {id}");
        }
        assert!(builtins().len() >= 30, "v0.3.0 ships 30+ templates");
    }

    #[test]
    fn do_command_shows_the_exact_fix() {
        let t = lookup("docker.run").unwrap();
        let have = args(&[("image", "nginx")]);
        let cmd = t.do_command(&have);
        assert_eq!(
            cmd,
            "cortex do docker.run name=<name> image=nginx ports=<ports>"
        );
    }
}
