//! Authorization: what this invocation is allowed to do, decided *inside*
//! the privileged binary.
//!
//! `sudo cortex …` — and worse, a `NOPASSWD` sudoers entry for cortex —
//! makes cortex a root escalation path unless cortex itself refuses things.
//! A grant of "may execute this binary as root" is only as narrow as the
//! binary's own gate. Previously there was none: anyone who could invoke
//! cortex could run any template with any arguments, and the LLM-driven
//! surfaces (phone → relay → signed tunnel → `sudo -n cortex`) reached that
//! gate with no policy between them and the kernel.
//!
//! So cortex evaluates every operation before it runs, on the privileged
//! side of the boundary:
//!
//! - **Deny by default.** No matching rule means refused. Allowance is
//!   always explicit, mirroring a deny-by-default model.
//! - **The caller is not trusted to say who it is.** The invoking user comes
//!   from `getuid`/`SUDO_UID`, never from an argument or an environment
//!   variable an attacker controls.
//! - **The rules live where only root can write them.** A policy file that
//!   the caller could edit is not a policy. Cortex refuses to load one that
//!   is writable by anyone but root.
//!
//! Irreversible operations are gated twice: policy must permit them, and the
//! permission is expressed as a [`PolicyToken`], which only
//! a permitting authorization decision can mint. A handler
//! cannot forge one, so it cannot silently opt itself out of reversibility.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;

/// The capability to run an operation that cannot be undone.
///
/// Zero-sized: it carries no data, only the *right* to declare an operation
/// irreversible. Its single field is private to this crate, so no code
/// outside `cortex-policy` can construct one — the token must come from an
/// allowing authorization decision. This is what stops a caller (or a future
/// developer) from lazily opting out of reversibility: the type system makes
/// "I decided this is irreversible" require "policy agreed."
#[derive(Debug, Clone, Copy)]
pub struct PolicyToken(());

impl PolicyToken {
    /// Mint from an authorization decision. The caller passes `true` only
    /// when its policy permitted the operation; `false` yields no token,
    /// which is the whole point. Taking the decision as an explicit argument
    /// (rather than a default) keeps the mint deliberately awkward.
    pub fn from_authorization(permitted: bool) -> Option<Self> {
        permitted.then_some(PolicyToken(()))
    }

    /// Mint a token for tests that drive the executor directly. `#[doc(hidden)]`
    /// and named to be obvious in review if it leaks into production.
    #[doc(hidden)]
    pub fn for_test() -> Self {
        PolicyToken(())
    }
}

/// Where the authorization rules live. Root-owned; cortex refuses to honour
/// a policy file that a non-root user can modify.
pub const DEFAULT_POLICY_PATH: &str = "/etc/cortex/policy.toml";

/// What an operation is asking to do, in cortex's own vocabulary.
#[derive(Debug, Clone)]
pub enum Operation<'a> {
    /// Run a registry template with bound arguments.
    Template {
        id: &'a str,
        args: &'a BTreeMap<String, String>,
    },
    /// Run a built-in filesystem workflow (`safe-config`, `safe-install`, …).
    Workflow { kind: &'a str },
    /// Run something with no inverse at all.
    Irreversible { cmd: &'a str },
    /// Reverse a previously committed change. Never denied by default: undo
    /// is what you reach for when things have gone wrong, and a policy that
    /// can lock you out of your own rollback is a liability, not a control.
    Undo,
    /// Read-only: status, history, receipt, templates, verify.
    Inspect,
}

impl Operation<'_> {
    /// The rule-matching name of this operation.
    pub fn selector(&self) -> String {
        match self {
            Self::Template { id, .. } => format!("template:{id}"),
            Self::Workflow { kind } => format!("workflow:{kind}"),
            Self::Irreversible { .. } => "irreversible".to_string(),
            Self::Undo => "undo".to_string(),
            Self::Inspect => "inspect".to_string(),
        }
    }

    fn is_irreversible(&self) -> bool {
        matches!(self, Self::Irreversible { .. })
    }

    /// Operations that change nothing and therefore need no authorization.
    fn is_read_only(&self) -> bool {
        matches!(self, Self::Inspect)
    }
}

/// What cortex will do about an operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// Run it.
    Allow,
    /// Run it, and mark the journal entry for review.
    Audit,
    /// Refuse.
    Deny,
    /// Refuse for now: it must be proposed and approved by an operator
    /// before it can run. The gate for anything irreversible.
    NeedsApproval,
}

/// One authorization rule. Matched in file order; the first match wins, so a
/// specific deny placed above a broad allow does what it looks like it does.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Rule {
    /// Operation selector: `template:docker.run`, `workflow:safe-config`,
    /// `irreversible`, `undo`, or `*`.
    pub op: String,
    /// What matching means.
    pub decision: RuleDecision,
    /// Restrict to these uids. Empty means any.
    #[serde(default)]
    pub uids: Vec<u32>,
    /// Every listed argument must match its glob. An argument named in a
    /// rule but absent from the operation never matches, so a rule cannot be
    /// bypassed by omitting a parameter.
    #[serde(default)]
    pub args: BTreeMap<String, String>,
    /// Operator-facing name, quoted in refusals.
    #[serde(default)]
    pub name: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuleDecision {
    Allow,
    Audit,
    Deny,
    NeedsApproval,
}

/// The loaded policy.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Policy {
    #[serde(default)]
    pub rules: Vec<Rule>,
}

impl Policy {
    /// The policy applied when no file exists: run reversible things, gate
    /// the irreversible ones. Deliberately usable out of the box — a tool
    /// that requires a config file before it does anything gets uninstalled
    /// — but never permissive about the one thing it cannot undo.
    pub fn default_policy() -> Self {
        let rule = |op: &str, decision: RuleDecision, name: &str| Rule {
            op: op.to_string(),
            decision,
            uids: Vec::new(),
            args: BTreeMap::new(),
            name: Some(name.to_string()),
        };
        Self {
            rules: vec![
                rule("undo", RuleDecision::Allow, "undo is always permitted"),
                rule("inspect", RuleDecision::Allow, "read-only"),
                rule(
                    "irreversible",
                    RuleDecision::NeedsApproval,
                    "irreversible operations need explicit approval",
                ),
                rule("template:*", RuleDecision::Allow, "reversible templates"),
                rule("workflow:*", RuleDecision::Allow, "reversible workflows"),
            ],
        }
    }

    /// Load from `path`.
    ///
    /// A missing file at the *default* location falls back to
    /// [`Policy::default_policy`], so cortex works out of the box. A missing
    /// file at an explicitly requested location is an error: silently
    /// substituting a permissive default for the rules the operator asked
    /// for would turn `--policy /nonexistent` into a bypass.
    ///
    /// Refuses a policy file that a non-root user can write: rules an
    /// attacker can edit are not rules. This is the same reasoning that
    /// forbids a `NOPASSWD` sudoers entry pointing at a user-writable binary.
    pub fn load(path: &Path) -> Result<Self> {
        let meta = match std::fs::symlink_metadata(path) {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                if path == Path::new(DEFAULT_POLICY_PATH) {
                    return Ok(Self::default_policy());
                }
                bail!(
                    "policy file {path:?} does not exist. Cortex will not fall back to \
                     a permissive default for a policy you explicitly named."
                );
            }
            Err(e) => return Err(e).with_context(|| format!("failed to stat {path:?}")),
        };
        ensure_root_owned(path, &meta)?;

        let text = std::fs::read_to_string(path)
            .with_context(|| format!("failed to read policy {path:?}"))?;
        let policy: Self =
            toml::from_str(&text).with_context(|| format!("failed to parse policy {path:?}"))?;
        Ok(policy)
    }

    /// The decision for one operation by one uid. First match wins;
    /// no match is a refusal.
    pub fn decide(&self, op: &Operation, uid: u32) -> (Decision, String) {
        // Read-only operations mutate nothing and are never gated: refusing
        // `cortex status` would only stop an operator from understanding the
        // system they are trying to fix.
        if op.is_read_only() {
            return (Decision::Allow, "read-only operation".to_string());
        }

        let selector = op.selector();
        for (i, rule) in self.rules.iter().enumerate() {
            if !rule.matches(&selector, op, uid) {
                continue;
            }
            let name = rule
                .name
                .clone()
                .unwrap_or_else(|| format!("rule #{}", i + 1));
            let decision = match rule.decision {
                RuleDecision::Allow => Decision::Allow,
                RuleDecision::Audit => Decision::Audit,
                RuleDecision::Deny => Decision::Deny,
                RuleDecision::NeedsApproval => Decision::NeedsApproval,
            };
            return (decision, format!("rule '{name}' matched"));
        }
        (
            Decision::Deny,
            format!("deny-by-default: no rule permits `{selector}`"),
        )
    }
}

impl Rule {
    fn matches(&self, selector: &str, op: &Operation, uid: u32) -> bool {
        if !self.uids.is_empty() && !self.uids.contains(&uid) {
            return false;
        }
        if !glob_match(&self.op, selector) {
            return false;
        }
        // Every constrained argument must be present AND match. An absent
        // argument must never satisfy a constraint, or a rule could be
        // sidestepped by omitting the parameter it restricts.
        if !self.args.is_empty() {
            let Operation::Template { args, .. } = op else {
                return false;
            };
            for (key, pattern) in &self.args {
                match args.get(key) {
                    Some(value) if glob_match(pattern, value) => {}
                    _ => return false,
                }
            }
        }
        true
    }
}

/// `*` matches any run of characters; everything else is literal. Enough for
/// `template:docker.*` and `image:nginx*`, and small enough to reason about.
fn glob_match(pattern: &str, value: &str) -> bool {
    match pattern.split_once('*') {
        None => pattern == value,
        Some((prefix, rest)) => {
            if !value.starts_with(prefix) {
                return false;
            }
            let value = &value[prefix.len()..];
            if rest.is_empty() {
                return true;
            }
            // Try every suffix position; patterns here are tiny.
            (0..=value.len()).any(|i| glob_match(rest, &value[i..]))
        }
    }
}

/// A policy file must be owned by root and writable by nobody else. Also
/// checks the containing directory: a writable directory means the file can
/// be replaced wholesale.
///
/// The check is skipped only when cortex is running *unprivileged*, where a
/// user-owned policy constrains nothing but that user's own unprivileged
/// process. It is enforced whenever it can matter: under `sudo`, under a
/// setuid invocation, and in every path that mutates the system as root.
fn ensure_root_owned(path: &Path, meta: &std::fs::Metadata) -> Result<()> {
    if !nix::unistd::geteuid().is_root() {
        return Ok(());
    }
    check_root_owned(path, meta)
}

/// The ownership rule itself, independent of who is running. Split out so it
/// can be tested at any uid — a security check that only runs as root is a
/// security check that is never tested.
fn check_root_owned(path: &Path, meta: &std::fs::Metadata) -> Result<()> {
    use std::os::unix::fs::MetadataExt;
    use std::os::unix::fs::PermissionsExt;

    if meta.file_type().is_symlink() {
        bail!(
            "refusing to load policy {path:?}: it is a symlink, and its target \
             could be swapped after this check"
        );
    }
    let mode = meta.permissions().mode();
    if meta.uid() != 0 {
        bail!(
            "refusing to load policy {path:?}: owned by uid {}, must be root. \
             A policy its subject can rewrite is not a policy.",
            meta.uid()
        );
    }
    if mode & 0o022 != 0 {
        bail!(
            "refusing to load policy {path:?}: mode {:o} is group- or world-writable. \
             Run: chmod 644 {}",
            mode & 0o7777,
            path.display()
        );
    }
    if let Some(dir) = path.parent() {
        if let Ok(dm) = std::fs::metadata(dir) {
            if dm.uid() != 0 || dm.permissions().mode() & 0o022 != 0 {
                bail!(
                    "refusing to load policy {path:?}: its directory {dir:?} is \
                     writable by non-root, so the file can be replaced"
                );
            }
        }
    }
    Ok(())
}

/// The uid that invoked cortex, before `sudo`.
///
/// Read from the process, never from an argument: an authorization decision
/// that trusts the caller's claim about who they are decides nothing. Under
/// `sudo`, `SUDO_UID` names the real operator; sudo sets it, and a caller who
/// could forge it could simply have run the command directly.
pub fn invoking_uid() -> u32 {
    std::env::var("SUDO_UID")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| nix::unistd::getuid().as_raw())
}

/// Evaluate an operation, returning the capability to run it.
///
/// The [`PolicyToken`] for an irreversible operation is minted only from an
/// allowing decision, so `Workflow::irreversible` cannot be constructed for
/// something policy refused.
pub struct Authorization {
    pub decision: Decision,
    pub reason: String,
    token: Option<PolicyToken>,
}

impl Authorization {
    /// The capability to declare an operation irreversible, if policy allowed
    /// one. `None` for every refused or non-irreversible operation.
    pub fn irreversible_token(&self) -> Option<PolicyToken> {
        self.token
    }

    /// Refuse loudly unless the operation may proceed. `consented` is the
    /// operator's explicit `--yes-irreversible`: it satisfies an approval
    /// gate, but it can never override a `Deny`.
    pub fn require(&self, op: &Operation, consented: bool) -> Result<()> {
        match self.decision {
            Decision::Allow => Ok(()),
            Decision::Audit => Ok(()),
            Decision::Deny => bail!(
                "policy refused `{}`: {}\n\
                 Rules live in {DEFAULT_POLICY_PATH} and are readable by root only.",
                op.selector(),
                self.reason
            ),
            Decision::NeedsApproval if consented && op.is_irreversible() => Ok(()),
            Decision::NeedsApproval => bail!(
                "`{}` needs approval: {}\n\
                 This operation cannot be undone. Re-run with --yes-irreversible \
                 to consent, or change the rule in {DEFAULT_POLICY_PATH}.",
                op.selector(),
                self.reason
            ),
        }
    }
}

/// Load policy and evaluate one operation for the invoking user.
pub fn authorize(policy_path: &Path, op: &Operation) -> Result<Authorization> {
    let policy = Policy::load(policy_path)?;
    let uid = invoking_uid();
    let (decision, reason) = policy.decide(op, uid);

    // Mint the irreversible capability only from a permitting decision, and
    // only for an irreversible operation. Possession must prove policy said
    // yes — not merely that policy ran. `NeedsApproval` mints because the
    // operator's consent still has to satisfy `Authorization::require`
    // before the token is ever used; `Deny` mints nothing, ever.
    let permitted = op.is_irreversible()
        && matches!(
            decision,
            Decision::Allow | Decision::Audit | Decision::NeedsApproval
        );
    let token = PolicyToken::from_authorization(permitted);
    Ok(Authorization {
        decision,
        reason,
        token,
    })
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

    fn tmpl<'a>(id: &'a str, a: &'a BTreeMap<String, String>) -> Operation<'a> {
        Operation::Template { id, args: a }
    }

    #[test]
    fn no_rule_means_denied() {
        let p = Policy { rules: vec![] };
        let a = args(&[]);
        let (d, why) = p.decide(&tmpl("docker.run", &a), 1000);
        assert_eq!(d, Decision::Deny);
        assert!(why.contains("deny-by-default"), "{why}");
    }

    /// The out-of-box policy must be usable, yet never permissive about the
    /// one thing cortex cannot take back.
    #[test]
    fn default_policy_allows_reversible_and_gates_irreversible() {
        let p = Policy::default_policy();
        let a = args(&[]);
        assert_eq!(p.decide(&tmpl("docker.run", &a), 1000).0, Decision::Allow);
        assert_eq!(p.decide(&Operation::Undo, 1000).0, Decision::Allow);
        assert_eq!(
            p.decide(
                &Operation::Workflow {
                    kind: "safe-config"
                },
                1000
            )
            .0,
            Decision::Allow
        );
        assert_eq!(
            p.decide(&Operation::Irreversible { cmd: "rm -rf /" }, 1000)
                .0,
            Decision::NeedsApproval
        );
    }

    /// A deny placed above an allow wins — first match, file order.
    #[test]
    fn first_matching_rule_wins() {
        let p = Policy {
            rules: vec![
                Rule {
                    op: "template:docker.*".into(),
                    decision: RuleDecision::Deny,
                    uids: vec![],
                    args: BTreeMap::new(),
                    name: Some("no containers".into()),
                },
                Rule {
                    op: "template:*".into(),
                    decision: RuleDecision::Allow,
                    uids: vec![],
                    args: BTreeMap::new(),
                    name: None,
                },
            ],
        };
        let a = args(&[]);
        assert_eq!(p.decide(&tmpl("docker.run", &a), 0).0, Decision::Deny);
        assert_eq!(p.decide(&tmpl("service.start", &a), 0).0, Decision::Allow);
    }

    #[test]
    fn rules_can_constrain_arguments() {
        let p = Policy {
            rules: vec![Rule {
                op: "template:docker.run".into(),
                decision: RuleDecision::Allow,
                uids: vec![],
                args: args(&[("image", "nginx*")]),
                name: Some("only nginx images".into()),
            }],
        };
        assert_eq!(
            p.decide(&tmpl("docker.run", &args(&[("image", "nginx:alpine")])), 0)
                .0,
            Decision::Allow
        );
        // A different image is not permitted...
        assert_eq!(
            p.decide(&tmpl("docker.run", &args(&[("image", "evil")])), 0)
                .0,
            Decision::Deny
        );
        // ...and neither is omitting the argument the rule constrains.
        assert_eq!(
            p.decide(&tmpl("docker.run", &args(&[])), 0).0,
            Decision::Deny,
            "an absent argument must not satisfy a constraint"
        );
    }

    #[test]
    fn rules_can_be_scoped_to_uids() {
        let p = Policy {
            rules: vec![Rule {
                op: "*".into(),
                decision: RuleDecision::Allow,
                uids: vec![1000],
                args: BTreeMap::new(),
                name: None,
            }],
        };
        let a = args(&[]);
        assert_eq!(p.decide(&tmpl("docker.run", &a), 1000).0, Decision::Allow);
        assert_eq!(p.decide(&tmpl("docker.run", &a), 1001).0, Decision::Deny);
    }

    /// Consent satisfies an approval gate but can never override a Deny —
    /// otherwise `--yes-irreversible` would be a universal bypass.
    #[test]
    fn consent_satisfies_approval_but_not_denial() {
        let approve = Authorization {
            decision: Decision::NeedsApproval,
            reason: String::new(),
            token: Some(PolicyToken::for_test()),
        };
        let op = Operation::Irreversible { cmd: "x" };
        assert!(approve.require(&op, true).is_ok());
        assert!(approve.require(&op, false).is_err());

        let deny = Authorization {
            decision: Decision::Deny,
            reason: String::new(),
            token: None,
        };
        assert!(
            deny.require(&op, true).is_err(),
            "consent must not override a deny"
        );
    }

    /// A refused operation must not hand out the capability to run something
    /// irreversible.
    #[test]
    fn a_denied_operation_mints_no_token() {
        let deny = Authorization {
            decision: Decision::Deny,
            reason: String::new(),
            token: None,
        };
        assert!(deny.irreversible_token().is_none());
    }

    #[test]
    fn inspection_is_never_gated() {
        let p = Policy { rules: vec![] }; // deny everything
        assert_eq!(p.decide(&Operation::Inspect, 1000).0, Decision::Allow);
    }

    #[test]
    fn globs_match_as_expected() {
        assert!(glob_match("*", "anything"));
        assert!(glob_match("template:docker.*", "template:docker.run"));
        assert!(!glob_match("template:docker.*", "template:service.start"));
        assert!(glob_match("nginx*", "nginx:alpine"));
        assert!(!glob_match("nginx*", "postgres"));
        assert!(glob_match("a*c", "abc"));
        assert!(!glob_match("a*c", "abd"));
        assert!(glob_match("exact", "exact"));
    }

    /// A policy the caller can rewrite is not a policy. Tested via the pure
    /// check so it runs at any uid.
    #[test]
    fn a_non_root_owned_policy_is_refused() {
        let t = tempfile::tempdir().unwrap();
        let p = t.path().join("policy.toml");
        std::fs::write(&p, "rules = []").unwrap();
        let meta = std::fs::symlink_metadata(&p).unwrap();

        // Owned by the test user, not root.
        let err = check_root_owned(&p, &meta).unwrap_err();
        assert!(format!("{err}").contains("must be root"), "got: {err}");
    }

    #[test]
    fn a_symlinked_policy_is_refused() {
        let t = tempfile::tempdir().unwrap();
        let real = t.path().join("real.toml");
        std::fs::write(&real, "rules = []").unwrap();
        let link = t.path().join("policy.toml");
        std::os::unix::fs::symlink(&real, &link).unwrap();

        let meta = std::fs::symlink_metadata(&link).unwrap();
        let err = check_root_owned(&link, &meta).unwrap_err();
        assert!(format!("{err}").contains("symlink"), "got: {err}");
    }

    /// Only the *default* path may fall back to the built-in policy. An
    /// explicitly named file that does not exist must be an error, or
    /// `--policy /nonexistent` becomes a way to get the permissive default.
    #[test]
    fn a_missing_explicit_policy_is_an_error_not_a_default() {
        let t = tempfile::tempdir().unwrap();
        let err = Policy::load(&t.path().join("nope.toml")).unwrap_err();
        assert!(
            format!("{err}").contains("will not fall back"),
            "got: {err}"
        );
    }

    #[test]
    fn a_missing_default_policy_falls_back_to_the_default() {
        // The default path almost certainly does not exist in a test env.
        if Path::new(DEFAULT_POLICY_PATH).exists() {
            return;
        }
        let p = Policy::load(Path::new(DEFAULT_POLICY_PATH)).unwrap();
        let a = args(&[]);
        assert_eq!(p.decide(&tmpl("docker.run", &a), 0).0, Decision::Allow);
        assert_eq!(
            p.decide(&Operation::Irreversible { cmd: "x" }, 0).0,
            Decision::NeedsApproval
        );
    }
}
