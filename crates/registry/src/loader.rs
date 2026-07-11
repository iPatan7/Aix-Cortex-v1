//! Operator-written templates, loaded from `~/.cortex/templates/*.toml`.
//!
//! A user template is the same promise a built-in makes — forward, inverse,
//! and a verifier for each — written by the operator instead of shipped in
//! the binary. It passes the identical [`crate::well_formed`] gate, and it
//! is subject to the same policy engine (`template:<id>` selectors match
//! user templates too, so an admin can `deny template:user.*`).
//!
//! Two rules keep this from becoming a privilege escalation:
//!
//! - **A user template may not shadow a built-in.** Redefining `docker.run`
//!   with a weaker inverse would silently replace a promise a human reviewed
//!   with one nobody did.
//! - **When cortex runs as root, template files must be root-owned** and not
//!   group/world-writable — the same rule the policy file lives under. A
//!   root binary executing commands from a file any user can edit is sudo
//!   with extra steps.
//!
//! Format (TOML, one template per file):
//!
//! ```toml
//! id = "app.cache-clear"
//! summary = "Clear the app cache directory"
//! category = "app"
//! example = "cortex do app.cache-clear"
//! keywords = [["cache"], ["clear", "flush", "empty"]]
//! host_side = true
//! drift_note = "the cache is rebuilt on demand"
//!
//! forward = "redis-cli -n 2 flushdb"
//! verify_forward = "test \"$(redis-cli -n 2 dbsize)\" = 0"
//! inverse = "systemctl restart app-cache-warmer"
//! verify_inverse = "systemctl is-active --quiet app-cache-warmer"
//!
//! [[params]]
//! name = "tier"
//! about = "cache tier to clear"
//! kind = "ident"
//! default = "all"
//! ```
//!
//! TOML rather than YAML: cortex already parses TOML for its policy file, so
//! this adds no dependency to the static binary, and the two operator-edited
//! file formats stay consistent.

use crate::{well_formed, Param, Template};
use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::path::{Path, PathBuf};

/// Environment override for where user templates live.
pub const ENV_TEMPLATE_DIR: &str = "CORTEX_TEMPLATE_DIR";

/// The on-disk shape. Deliberately a separate struct from [`Template`]: the
/// file format can stay stable while the internal model moves.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct TemplateFile {
    id: String,
    summary: String,
    #[serde(default = "default_category")]
    category: String,
    #[serde(default)]
    keywords: Vec<Vec<String>>,
    #[serde(default)]
    verbs: Vec<String>,
    #[serde(default)]
    example: String,
    #[serde(default)]
    params: Vec<Param>,
    forward: String,
    verify_forward: String,
    inverse: String,
    verify_inverse: String,
    #[serde(default)]
    host_side: bool,
    #[serde(default)]
    drift_note: String,
}

fn default_category() -> String {
    "custom".to_string()
}

/// Where user templates are read from: `$CORTEX_TEMPLATE_DIR` if set,
/// otherwise `~/.cortex/templates`.
pub fn user_template_dir() -> Option<PathBuf> {
    if let Ok(dir) = std::env::var(ENV_TEMPLATE_DIR) {
        return Some(PathBuf::from(dir));
    }
    std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cortex/templates"))
}

/// Load every `*.toml` under the user template dir. Files are loaded in
/// filename order so the registry is deterministic. A file that fails to
/// parse or validate fails the whole load — half a template catalog is worse
/// than none, because the operator would not know which half they have.
pub fn load_user_templates(builtins: &[Template]) -> Result<Vec<Template>> {
    let Some(dir) = user_template_dir() else {
        return Ok(Vec::new());
    };
    load_from_dir(&dir, builtins)
}

/// The loader itself, directory-explicit so tests can drive it.
pub fn load_from_dir(dir: &Path, builtins: &[Template]) -> Result<Vec<Template>> {
    if !dir.is_dir() {
        return Ok(Vec::new());
    }
    let mut paths: Vec<PathBuf> = std::fs::read_dir(dir)
        .with_context(|| format!("failed to read template dir {dir:?}"))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|e| e == "toml"))
        .collect();
    paths.sort();

    let mut out: Vec<Template> = Vec::new();
    for path in paths {
        let t = load_one(&path).with_context(|| format!("in user template {}", path.display()))?;
        if builtins.iter().any(|b| b.id == t.id) {
            bail!(
                "user template {} redefines built-in `{}` — a built-in's inverse was \
                 reviewed with the code and cannot be shadowed. Pick a namespaced id \
                 like `user.{}`.",
                path.display(),
                t.id,
                t.id
            );
        }
        if out.iter().any(|prev| prev.id == t.id) {
            bail!(
                "user template {} redefines `{}`, already defined by an earlier file",
                path.display(),
                t.id
            );
        }
        out.push(t);
    }
    Ok(out)
}

fn load_one(path: &Path) -> Result<Template> {
    let meta =
        std::fs::symlink_metadata(path).with_context(|| format!("failed to stat {path:?}"))?;
    ensure_trusted_when_root(path, &meta)?;

    let text = std::fs::read_to_string(path)?;
    let file: TemplateFile = toml::from_str(&text).context("not valid template TOML")?;
    let example = if file.example.is_empty() {
        format!("cortex do {} ...", file.id)
    } else {
        file.example
    };
    let t = Template {
        id: file.id,
        summary: file.summary,
        category: file.category,
        keywords: file.keywords,
        verbs: file.verbs,
        example,
        params: file.params,
        forward: file.forward,
        verify_forward: file.verify_forward,
        inverse: file.inverse,
        verify_inverse: file.verify_inverse,
        host_side: file.host_side,
        drift_note: file.drift_note,
    };
    well_formed(&t)?;
    Ok(t)
}

/// When cortex runs as root, a template file is executable input to a root
/// process: it must be root-owned and not writable by anyone else — the same
/// standard the policy file is held to. Unprivileged runs skip the check;
/// a user-owned template constrains nothing but that user's own process.
fn ensure_trusted_when_root(path: &Path, meta: &std::fs::Metadata) -> Result<()> {
    if !nix::unistd::geteuid().is_root() {
        return Ok(());
    }
    check_root_owned(path, meta)
}

/// The ownership rule itself, independent of who is running — split out so
/// it is testable at any uid.
fn check_root_owned(path: &Path, meta: &std::fs::Metadata) -> Result<()> {
    use std::os::unix::fs::MetadataExt;
    use std::os::unix::fs::PermissionsExt;

    if meta.file_type().is_symlink() {
        bail!(
            "refusing template {path:?}: it is a symlink, and its target could be \
             swapped after this check"
        );
    }
    if meta.uid() != 0 {
        bail!(
            "refusing template {path:?}: owned by uid {}, but cortex is running as \
             root — commands a non-root user can edit must not run as root. \
             chown it to root, or run cortex unprivileged.",
            meta.uid()
        );
    }
    let mode = meta.permissions().mode();
    if mode & 0o022 != 0 {
        bail!(
            "refusing template {path:?}: mode {:o} is group- or world-writable. \
             Run: chmod 644 {}",
            mode & 0o7777,
            path.display()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const GOOD: &str = r#"
id = "user.touch-flag"
summary = "Drop a flag file"
example = "cortex do user.touch-flag path=/tmp/flag"
keywords = [["flag"], ["drop", "touch"]]
forward = "touch {path}"
verify_forward = "test -e {path}"
inverse = "rm -f {path}"
verify_inverse = "! test -e {path}"

[[params]]
name = "path"
about = "where the flag goes"
kind = "abs-path"
"#;

    fn write(dir: &Path, name: &str, body: &str) {
        std::fs::write(dir.join(name), body).unwrap();
    }

    #[test]
    fn a_valid_template_loads_and_binds() {
        let d = tempfile::tempdir().unwrap();
        write(d.path(), "flag.toml", GOOD);
        let loaded = load_from_dir(d.path(), &[]).unwrap();
        assert_eq!(loaded.len(), 1);
        let t = &loaded[0];
        assert_eq!(t.id, "user.touch-flag");
        assert_eq!(t.keywords, vec![vec!["flag"], vec!["drop", "touch"]]);

        let mut args = std::collections::BTreeMap::new();
        args.insert("path".to_string(), "/tmp/flag".to_string());
        let b = t.bind(&args).unwrap();
        assert_eq!(b.forward, "touch '/tmp/flag'");
        assert_eq!(b.verify_inverse, "! test -e '/tmp/flag'");
    }

    #[test]
    fn a_template_without_an_inverse_verifier_is_refused() {
        let d = tempfile::tempdir().unwrap();
        write(
            d.path(),
            "bad.toml",
            r#"
id = "user.bad"
summary = "no undo proof"
forward = "touch /tmp/x"
verify_forward = "test -e /tmp/x"
inverse = "rm -f /tmp/x"
verify_inverse = "  "
"#,
        );
        let err = format!("{:#}", load_from_dir(d.path(), &[]).unwrap_err());
        assert!(err.contains("INVERSE verifier"), "{err}");
    }

    #[test]
    fn an_undeclared_placeholder_is_refused() {
        let d = tempfile::tempdir().unwrap();
        write(
            d.path(),
            "bad.toml",
            r#"
id = "user.bad"
summary = "uses a parameter it never declared"
forward = "touch {path}"
verify_forward = "test -e {path}"
inverse = "rm -f {path}"
verify_inverse = "! test -e {path}"
"#,
        );
        let err = format!("{:#}", load_from_dir(d.path(), &[]).unwrap_err());
        assert!(err.contains("undeclared placeholder"), "{err}");
    }

    #[test]
    fn shadowing_a_builtin_is_refused() {
        let d = tempfile::tempdir().unwrap();
        write(
            d.path(),
            "evil.toml",
            r#"
id = "docker.run"
summary = "a weaker docker.run"
forward = "true"
verify_forward = "true"
inverse = "echo done"
verify_inverse = "true"
"#,
        );
        let err = format!(
            "{:#}",
            load_from_dir(d.path(), crate::builtins()).unwrap_err()
        );
        assert!(err.contains("cannot be shadowed"), "{err}");
    }

    #[test]
    fn duplicate_user_ids_are_refused() {
        let d = tempfile::tempdir().unwrap();
        write(d.path(), "a.toml", GOOD);
        write(d.path(), "b.toml", GOOD);
        let err = format!("{:#}", load_from_dir(d.path(), &[]).unwrap_err());
        assert!(err.contains("already defined"), "{err}");
    }

    /// The phase-2 parameter kinds are reachable from TOML with the same
    /// kebab-case names `templates show` prints.
    #[test]
    fn new_param_kinds_parse_from_toml() {
        let d = tempfile::tempdir().unwrap();
        write(
            d.path(),
            "app.toml",
            r#"
id = "user.app-container"
summary = "Run the app container with its volume"
host_side = true
forward = "docker run -d --name app -e {env} -v {volume} myapp"
verify_forward = "docker ps -q --filter name=app | grep -q ."
inverse = "docker rm -f app"
verify_inverse = "! docker ps -aq --filter name=app | grep -q ."

[[params]]
name = "env"
about = "environment variable"
kind = "env-var"

[[params]]
name = "volume"
about = "host:container mount"
kind = "volume-mapping"
"#,
        );
        let loaded = load_from_dir(d.path(), &[]).unwrap();
        assert_eq!(loaded[0].params[0].kind, crate::ParamKind::EnvVar);
        assert_eq!(loaded[0].params[1].kind, crate::ParamKind::VolumeMapping);

        let mut args = std::collections::BTreeMap::new();
        args.insert("env".to_string(), "MODE=prod".to_string());
        args.insert("volume".to_string(), "/srv/x:/x".to_string());
        assert!(loaded[0].bind(&args).is_ok());
    }

    #[test]
    fn a_missing_dir_is_not_an_error() {
        let d = tempfile::tempdir().unwrap();
        let loaded = load_from_dir(&d.path().join("nope"), &[]).unwrap();
        assert!(loaded.is_empty());
    }

    /// The root-trust rule, tested at any uid via the pure check.
    #[test]
    fn root_refuses_non_root_owned_template_files() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("t.toml");
        std::fs::write(&p, GOOD).unwrap();
        let meta = std::fs::symlink_metadata(&p).unwrap();
        let err = format!("{:#}", check_root_owned(&p, &meta).unwrap_err());
        assert!(err.contains("must not run as root"), "{err}");
    }
}
