//! The offline planner: approximate English in, a reversible plan out.
//!
//! This is the *default and only* planner. It is deterministic — the same
//! sentence always produces the same plan — auditable (every rule is a line
//! of Rust you can read), and it works on a box with no network, which is
//! frequently the box you are trying to fix. No model is consulted; the CLI
//! may separately offer an LLM fallback, but nothing here depends on one.
//!
//! How a sentence becomes a plan:
//!
//! 1. **Tokenize.** Lowercased words for matching, original casing kept for
//!    values, `key=value` tokens split out as explicit parameters.
//! 2. **Match.** Every registry template declares trigger keyword groups; a
//!    template is a candidate when each group has a hit (typos and
//!    inflections allowed, within strict bounds — see [`fuzzy`]). Built-in
//!    intents (service control, container deploys) join the same scored
//!    pool.
//! 3. **Bind.** Candidates are tried best-score-first; parameters are filled
//!    from `key=value`, then from shape-based extraction ("port 8080", an
//!    absolute path), then from declared defaults. The first candidate that
//!    fully binds is the plan.
//! 4. **Teach.** If the best candidate is missing parameters, the answer is
//!    not "unknown command" — it is the exact `cortex do` line with the
//!    holes marked. If nothing matched, the nearest templates are suggested.
//!
//! The matcher is deliberately conservative where it must be: a request that
//! names a port is never satisfied by a bare `systemctl start` (which cannot
//! honour the port), and a false match is treated as strictly worse than a
//! miss, because every plan is executed for real after it is shown.

mod extract;
mod fuzzy;

pub use fuzzy::{closest, word_matches};

use cortex_registry::{Param, Template};
use serde_json::{json, Value};
use std::collections::BTreeMap;

/// A parsed request: words for matching, raw casing for values, and any
/// explicit `key=value` parameters.
pub struct Request {
    lows: Vec<String>,
    raws: Vec<String>,
    kv: BTreeMap<String, String>,
}

impl Request {
    pub fn parse(text: &str) -> Self {
        let mut lows = Vec::new();
        let mut raws = Vec::new();
        let mut kv = BTreeMap::new();
        for token in text.split_whitespace() {
            let token = token.trim_matches(|c: char| ",.!?\"'".contains(c));
            if token.is_empty() {
                continue;
            }
            if let Some((k, v)) = token.split_once('=') {
                if !k.is_empty() && !v.is_empty() {
                    kv.insert(k.to_lowercase(), v.to_string());
                    continue;
                }
            }
            lows.push(token.to_lowercase());
            raws.push(token.to_string());
        }
        Self { lows, raws, kv }
    }

    pub fn low_words(&self) -> &[String] {
        &self.lows
    }

    pub fn raw_words(&self) -> &[String] {
        &self.raws
    }

    pub fn kv(&self) -> &BTreeMap<String, String> {
        &self.kv
    }

    fn has_word(&self, keyword: &str) -> bool {
        self.lows.iter().any(|w| fuzzy::word_matches(w, keyword))
    }
}

/// What the planner understood. Every variant is an answer, not a shrug:
/// even `Unknown` carries the nearest templates and why nothing matched.
pub enum Understanding {
    /// A complete, executable plan.
    Plan(Planned),
    /// Several complete plans, to run in order ("install htop and open port
    /// 8080"). Each step journals separately, so undo unwinds newest-first.
    Composite(Vec<Planned>),
    /// The right template is clear but parameters are missing; here is the
    /// exact command line that would run it.
    NeedsInput(NeedsInput),
    /// Nothing matched (or the only match had to refuse); here is what came
    /// closest.
    Unknown(Unknown),
    /// The user asked to reverse things, not do something.
    Undo,
}

/// A plan in the shape the CLI's dispatcher executes: either
/// `{"workflow": "safe-service", ...}` or
/// `{"workflow": "template", "template": id, "args": {...}}`.
pub struct Planned {
    pub summary: String,
    pub value: Value,
}

pub struct NeedsInput {
    pub template_id: String,
    pub summary: String,
    /// (name, about, expected type) for each missing parameter.
    pub missing: Vec<(String, String, String)>,
    /// A ready-to-edit invocation with `<holes>` for the missing values.
    pub do_command: String,
}

#[derive(Default)]
pub struct Unknown {
    /// Why the planner could not proceed, when it knows (a refusal reason or
    /// a validation error), beyond simply "no match".
    pub reason: Option<String>,
    pub suggestions: Vec<Suggestion>,
}

pub struct Suggestion {
    pub id: String,
    pub summary: String,
    pub example: String,
}

/// True when the user is asking to reverse things rather than do something.
/// Matched first, so `undo` works with nothing else in the pipeline healthy.
pub fn is_undo(text: &str) -> bool {
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

/// Understand a request against the full registry (built-ins + user
/// templates).
pub fn understand(text: &str) -> Understanding {
    understand_with(text, cortex_registry::all())
}

/// The planner proper, registry-explicit so tests can drive it.
///
/// Composition comes first: a request with conjunctions ("install htop and
/// open port 8080") is split on them, and if **every** segment independently
/// yields a complete plan, the result is a composite of those plans, in
/// order. Anything less — one segment ambiguous, one missing a parameter —
/// falls back to reading the whole sentence as one request, so composition
/// can only add plans, never change what a single-intent sentence means.
pub fn understand_with(text: &str, templates: &[Template]) -> Understanding {
    if is_undo(text) {
        return Understanding::Undo;
    }
    let segments = split_tasks(text);
    if segments.len() > 1 {
        let mut plans = Vec::with_capacity(segments.len());
        for seg in &segments {
            match understand_one(seg, templates) {
                Understanding::Plan(p) => plans.push(p),
                _ => {
                    plans.clear();
                    break;
                }
            }
        }
        if !plans.is_empty() {
            return Understanding::Composite(plans);
        }
    }
    understand_one(text, templates)
}

/// Split a request at explicit conjunctions: `and`, `then`, `also`, `&&`,
/// `;`, and a trailing comma. Conservative on purpose — the caller falls
/// back to the whole sentence unless every piece plans cleanly.
fn split_tasks(text: &str) -> Vec<String> {
    let mut segments: Vec<Vec<&str>> = vec![Vec::new()];
    for token in text.split_whitespace() {
        let low = token
            .trim_matches(|c: char| ",.!?".contains(c))
            .to_lowercase();
        if matches!(low.as_str(), "and" | "then" | "also" | "&&" | ";") {
            if !segments.last().is_some_and(Vec::is_empty) {
                segments.push(Vec::new());
            }
            continue;
        }
        let breaks_after = token.ends_with(',') || token.ends_with(';');
        segments
            .last_mut()
            .expect("segments is never empty")
            .push(token.trim_end_matches([',', ';']));
        if breaks_after {
            segments.push(Vec::new());
        }
    }
    segments
        .into_iter()
        .filter(|s| !s.is_empty())
        .map(|s| s.join(" "))
        .collect()
}

/// Understand exactly one task (no conjunction splitting).
fn understand_one(text: &str, templates: &[Template]) -> Understanding {
    let req = Request::parse(text);
    if req.lows.is_empty() && req.kv.is_empty() {
        return Understanding::Unknown(Unknown::default());
    }

    // A request that leads with a template id is explicit: bind it directly,
    // no scoring. `cortex try "docker.run image=nginx ports=80:80"` works.
    if let Some(first) = req.lows.first() {
        if let Some(t) = templates.iter().find(|t| &t.id == first) {
            return bind_template(t, &req, req.kv().clone());
        }
    }

    let mut candidates = collect_candidates(&req, templates);
    // Highest score first; ties resolve by declaration order, which puts
    // more specific templates ahead of broader ones (see builtin.rs).
    candidates.sort_by(|a, b| b.score.cmp(&a.score).then(a.index.cmp(&b.index)));

    let mut needs_input: Option<(NeedsInput, usize)> = None;
    let mut refusal: Option<String> = None;

    for cand in &candidates {
        match try_bind(cand, &req, templates) {
            Attempt::Plan(p) => {
                // Between templates, a strictly better-scoring match that
                // only lacked parameters outranks a weaker match that happens
                // to bind: "serve nginx over https" must teach nginx.tls's
                // cert/key, not silently plan the plain-HTTP template. Ties
                // still go to whichever binds, and the built-in intents
                // (service control, deploy) are deliberate broad fallbacks
                // that bypass the gate — "run nginx server" is a service
                // start even though nginx.serve scored higher and wants a
                // port.
                if matches!(cand.kind, CandidateKind::Template(_)) {
                    if let Some((ni, score)) = needs_input {
                        if score > cand.score {
                            return Understanding::NeedsInput(ni);
                        }
                    }
                }
                return Understanding::Plan(p);
            }
            Attempt::Missing(ni) => {
                needs_input.get_or_insert((ni, cand.score));
            }
            Attempt::Refuse(why) => {
                refusal.get_or_insert(why);
            }
        }
    }

    if let Some((ni, _)) = needs_input {
        return Understanding::NeedsInput(ni);
    }
    Understanding::Unknown(Unknown {
        reason: refusal,
        suggestions: suggest(&req, templates),
    })
}

/// One scored match candidate.
struct Candidate {
    score: usize,
    /// Sort tiebreaker: built-in intents use small indices so they win ties
    /// against templates only when scores are equal and specificity is too.
    index: usize,
    kind: CandidateKind,
}

enum CandidateKind {
    /// systemd control through the prior-state-aware workflow.
    Service,
    /// "deploy <name> image=... ports=..." → docker.run.
    Deploy,
    /// A registry template, by position in `templates`.
    Template(usize),
}

enum Attempt {
    Plan(Planned),
    Missing(NeedsInput),
    Refuse(String),
}

fn collect_candidates(req: &Request, templates: &[Template]) -> Vec<Candidate> {
    let mut out = Vec::new();

    if let Some(score) = service_score(req) {
        out.push(Candidate {
            score,
            index: 0,
            kind: CandidateKind::Service,
        });
    }
    if req.has_word("deploy") || req.has_word("ship") {
        out.push(Candidate {
            score: 1,
            index: 1,
            kind: CandidateKind::Deploy,
        });
    }

    for (i, t) in templates.iter().enumerate() {
        if t.keywords.is_empty() {
            continue; // reachable only by `cortex do` or an explicit plan
        }
        let mut score = 0usize;
        let mut all_groups = true;
        for group in &t.keywords {
            // An explicit `key=value` names its parameter, and naming the
            // parameter is the strongest possible signal: `env=A=b` satisfies
            // an "env" keyword group exactly like the word "env" would.
            let hits = group
                .iter()
                .filter(|k| req.has_word(k) || req.kv().contains_key(k.as_str()))
                .count();
            if hits == 0 {
                all_groups = false;
                break;
            }
            score += hits;
        }
        if !all_groups {
            continue;
        }
        score += t.verbs.iter().filter(|v| req.has_word(v)).count();
        out.push(Candidate {
            score,
            // Offset template indices past the built-in intents so intent
            // order is stable and documented.
            index: i + 8,
            kind: CandidateKind::Template(i),
        });
    }
    out
}

/// Score the systemd-service intent: a control verb plus a nameable unit.
fn service_score(req: &Request) -> Option<usize> {
    service_op(req)?;
    Some(1 + usize::from(service_unit(req).is_some()))
}

fn service_op(req: &Request) -> Option<&'static str> {
    // Order matters: "restart" contains the stem of nothing else, but a
    // request saying both "stop" and "start" is read as restart-ish; the
    // explicit words win in this order.
    for (word, op) in [
        ("restart", "restart"),
        ("stop", "stop"),
        ("enable", "enable"),
        ("disable", "disable"),
        ("start", "start"),
        ("run", "start"),
        ("launch", "start"),
        ("bring", "start"),
    ] {
        if req.has_word(word) {
            return Some(op);
        }
    }
    None
}

fn service_unit(req: &Request) -> Option<String> {
    const NOISE: &[&str] = &[
        "the",
        "a",
        "an",
        "service",
        "server",
        "daemon",
        "unit",
        "up",
        "please",
        "my",
        "start",
        "stop",
        "restart",
        "enable",
        "disable",
        "run",
        "launch",
        "bring",
        "on",
        "boot",
        "at",
        "now",
        "again",
        "back",
        "it",
        "them",
        "this",
        "that",
        "everything",
    ];
    let unit = req
        .lows
        .iter()
        .find(|w| !NOISE.iter().any(|n| fuzzy::word_matches(w, n)))?;
    (unit.len() >= 2
        && cortex_registry::ParamKind::Ident
            .validate("unit", unit)
            .is_ok())
    .then(|| unit.clone())
}

fn try_bind(cand: &Candidate, req: &Request, templates: &[Template]) -> Attempt {
    match cand.kind {
        CandidateKind::Service => bind_service(req),
        CandidateKind::Deploy => {
            // A deploy that supplies env/volume needs the template that has
            // those parameters; a plain one stays on docker.run.
            let id = if req.kv().contains_key("env") || req.kv().contains_key("volume") {
                "docker.app"
            } else {
                "docker.run"
            };
            match templates.iter().find(|t| t.id == id) {
                Some(t) => bind_deploy(t, req),
                None => Attempt::Refuse(format!("no {id} template available")),
            }
        }
        CandidateKind::Template(i) => {
            let t = &templates[i];
            let mut have = extract_for(t, req);
            // Explicit key=value always beats extraction.
            for (k, v) in req.kv() {
                have.insert(k.clone(), v.clone());
            }
            attempt_template(t, have)
        }
    }
}

fn bind_template(t: &Template, req: &Request, mut have: BTreeMap<String, String>) -> Understanding {
    for (k, v) in req.kv() {
        have.insert(k.clone(), v.clone());
    }
    match attempt_template(t, have) {
        Attempt::Plan(p) => Understanding::Plan(p),
        Attempt::Missing(ni) => Understanding::NeedsInput(ni),
        Attempt::Refuse(reason) => Understanding::Unknown(Unknown {
            reason: Some(reason),
            suggestions: vec![to_suggestion(t)],
        }),
    }
}

/// Bind a template from a `have` map: report missing parameters as the
/// teachable moment, validation failures as refusals.
fn attempt_template(t: &Template, have: BTreeMap<String, String>) -> Attempt {
    // A key that is not a parameter poisons the whole bind; report it
    // instead of guessing which template the user meant.
    if let Some(bad) = have
        .keys()
        .find(|k| !t.params.iter().any(|p| &&p.name == k))
    {
        return Attempt::Refuse(format!(
            "template `{}` has no parameter `{bad}` (expected: {})",
            t.id,
            t.params
                .iter()
                .map(|p| p.name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    let missing: Vec<&Param> = t.missing_params(&have);
    if !missing.is_empty() {
        return Attempt::Missing(NeedsInput {
            template_id: t.id.clone(),
            summary: t.summary.clone(),
            missing: missing
                .iter()
                .map(|p| {
                    (
                        p.name.clone(),
                        p.about.clone(),
                        p.kind.describe().to_string(),
                    )
                })
                .collect(),
            do_command: t.do_command(&have),
        });
    }
    match t.bind(&have) {
        Ok(bound) => Attempt::Plan(Planned {
            summary: format!(
                "{} {}",
                t.id,
                bound
                    .args
                    .iter()
                    .map(|(k, v)| format!("{k}={v}"))
                    .collect::<Vec<_>>()
                    .join(" ")
            ),
            value: json!({
                "workflow": "template",
                "template": t.id,
                "args": bound.args,
            }),
        }),
        Err(e) => Attempt::Refuse(format!("{e:#}")),
    }
}

/// The systemd intent. A named port is refused here on purpose: `systemctl
/// start nginx` cannot put nginx on port 8080, and silently dropping the
/// port would do the wrong thing convincingly.
fn bind_service(req: &Request) -> Attempt {
    if extract::mentions_a_port(req) {
        return Attempt::Refuse(
            "the request names a port, which a plain service start/stop cannot honour. \
             For a container on that port: cortex try \"run <image> in docker on port <p>\". \
             For nginx serving on it: cortex try \"run nginx on port <p>\"."
                .into(),
        );
    }
    let Some(op) = service_op(req) else {
        return Attempt::Refuse("no service operation recognised".into());
    };
    let Some(unit) = service_unit(req) else {
        return Attempt::Refuse(format!(
            "understood `{op}` but not which unit; name it: cortex try \"{op} nginx\""
        ));
    };
    Attempt::Plan(Planned {
        summary: format!("service {op} {unit}"),
        value: json!({ "workflow": "safe-service", "op": op, "service": unit }),
    })
}

/// "deploy myapp image=nginx ports=8080:80" — a deploy is a named container
/// run; the bare word after "deploy" is the name.
fn bind_deploy(docker_run: &Template, req: &Request) -> Attempt {
    const NOISE: &[&str] = &[
        "deploy",
        "ship",
        "the",
        "a",
        "an",
        "new",
        "app",
        "application",
        "to",
        "production",
        "prod",
        "staging",
        "with",
        "as",
        "on",
        "port",
        "in",
        "docker",
        "podman",
        "container",
        "image",
        "of",
        "run",
    ];
    let mut have = BTreeMap::new();
    if let Some(image) = extract::image(req) {
        have.insert("image".to_string(), image);
    }
    if let Some(ports) = extract::port_mapping(req) {
        have.insert("ports".to_string(), ports);
    }
    for (k, v) in req.kv() {
        have.insert(k.clone(), v.clone());
    }
    if !have.contains_key("name") {
        let name = req
            .lows
            .iter()
            .find(|w| {
                !extract::is_noise(w, NOISE)
                    && Some(w.as_str()) != have.get("image").map(String::as_str)
                    && !w.chars().all(|c| c.is_ascii_digit() || c == ':')
            })
            .cloned()
            .or_else(|| have.get("image").map(|i| extract::container_name(i)));
        if let Some(name) = name {
            have.insert("name".to_string(), name);
        }
    }
    attempt_template(docker_run, have)
}

/// Shape-based extraction for the built-in templates, keyed by id. User
/// templates get the generic kind-driven rules.
fn extract_for(t: &Template, req: &Request) -> BTreeMap<String, String> {
    let mut have = BTreeMap::new();
    let mut put = |k: &str, v: Option<String>| {
        if let Some(v) = v {
            have.insert(k.to_string(), v);
        }
    };
    match t.id.as_str() {
        "docker.run" | "podman.run" | "docker.app" => {
            let image = extract::image(req);
            put("name", image.as_deref().map(extract::container_name));
            put("image", image);
            put("ports", extract::port_mapping(req));
        }
        "docker.volume.create" | "docker.network.create" => put("name", extract::named(req)),
        "git.clone" => {
            put("repo", extract::url(req));
            put("path", extract::abs_path(req));
        }
        "swap.create" => put("size", extract::size(req)),
        "docker.compose.up" => {
            let file = extract::compose_file(req);
            put(
                "project",
                file.as_deref().and_then(extract::compose_project),
            );
            put("file", file);
        }
        "nginx.serve" => {
            put("port", extract::port(req));
            put("root", extract::abs_path(req));
        }
        "package.install" | "package.remove" | "package.install-dnf" | "package.remove-dnf" => {
            put("package", extract::package(req));
        }
        "user.add" | "user.add-sudo" | "user.grant-sudo" | "user.remove" | "user.ssh-key" => {
            put("username", extract::username(req));
        }
        "firewall.allow" | "firewall.remove" => {
            put("port", extract::port(req));
            if req.has_word("udp") {
                put("proto", Some("udp".to_string()));
            }
        }
        "dir.create" | "file.deploy" => put("path", extract::abs_path(req)),
        "service.create" => put("name", extract::named(req)),
        _ => generic_extract(t, req, &mut have),
    }
    have
}

/// Kind-driven extraction for operator-written templates: only shapes that
/// cannot be mistaken for prose (ports, mappings, absolute paths, a marked
/// username) are pulled from free text; everything else comes from
/// `key=value` or defaults. A kind that two parameters share is never
/// extracted — one path in a request that needs a `link` and a `target`
/// answers neither.
fn generic_extract(t: &Template, req: &Request, have: &mut BTreeMap<String, String>) {
    use cortex_registry::ParamKind;
    for p in &t.params {
        if t.params.iter().filter(|q| q.kind == p.kind).count() > 1 {
            continue;
        }
        let v = match p.kind {
            ParamKind::Port => extract::port(req),
            ParamKind::PortMapping => extract::port_mapping(req),
            ParamKind::AbsPath => extract::abs_path(req),
            ParamKind::User => extract::username(req),
            ParamKind::Image => extract::image(req),
            _ => None,
        };
        if let Some(v) = v {
            have.entry(p.name.clone()).or_insert(v);
        }
    }
}

fn to_suggestion(t: &Template) -> Suggestion {
    Suggestion {
        id: t.id.clone(),
        summary: t.summary.clone(),
        example: t.example.clone(),
    }
}

/// The nearest templates when nothing matched: ranked by how many of their
/// trigger words appear at all, so "swap the simlink" still points at
/// symlink.swap.
fn suggest(req: &Request, templates: &[Template]) -> Vec<Suggestion> {
    let mut scored: Vec<(usize, usize, &Template)> = templates
        .iter()
        .enumerate()
        .filter_map(|(i, t)| {
            let hits = t
                .keywords
                .iter()
                .flatten()
                .chain(t.verbs.iter())
                .filter(|k| req.has_word(k))
                .count();
            (hits > 0).then_some((hits, i, t))
        })
        .collect();
    scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    scored
        .into_iter()
        .take(3)
        .map(|(_, _, t)| to_suggestion(t))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plan(text: &str) -> Option<Value> {
        match understand(text) {
            Understanding::Plan(p) => Some(p.value),
            _ => None,
        }
    }

    fn needs(text: &str) -> Option<NeedsInput> {
        match understand(text) {
            Understanding::NeedsInput(n) => Some(n),
            _ => None,
        }
    }

    // ---- the hero commands --------------------------------------------

    #[test]
    fn run_nginx_on_a_port_plans_the_port_honouring_template() {
        for phrasing in [
            "run nginx on port 8080",
            "nginx on port 8080",
            "run nginx on 8080",
            "serve nginx on port 8080",
            "set up nginx on port 8080",
        ] {
            let p = plan(phrasing).unwrap_or_else(|| panic!("no plan for: {phrasing}"));
            assert_eq!(p["template"], "nginx.serve", "for: {phrasing}");
            assert_eq!(p["args"]["port"], "8080", "for: {phrasing}");
        }
    }

    #[test]
    fn deploy_with_explicit_parameters_runs_a_named_container() {
        let p = plan("deploy myapp image=foo ports=80:8080").unwrap();
        assert_eq!(p["template"], "docker.run");
        assert_eq!(p["args"]["name"], "myapp");
        assert_eq!(p["args"]["image"], "foo");
        assert_eq!(p["args"]["ports"], "80:8080");
    }

    /// Without an explicit name, an env/volume deploy still derives the
    /// container name from the image, exactly like a plain deploy.
    #[test]
    fn env_deploys_derive_a_name_from_the_image() {
        let p = plan("deploy image=nginx ports=8080:80 env=MODE=on volume=/srv/x:/x").unwrap();
        assert_eq!(p["template"], "docker.app");
        assert_eq!(p["args"]["name"], "cortex-nginx");
    }

    /// A deploy that names env/volume routes to the template that has those
    /// parameters, rather than refusing them as unknown.
    #[test]
    fn deploy_with_env_or_volume_routes_to_docker_app() {
        let p = plan(
            "deploy myapp image=nginx ports=8080:80 env=NODE_ENV=production volume=/srv/data:/data",
        )
        .unwrap();
        assert_eq!(p["template"], "docker.app");
        assert_eq!(p["args"]["name"], "myapp");
        assert_eq!(p["args"]["env"], "NODE_ENV=production");
        assert_eq!(p["args"]["volume"], "/srv/data:/data");
    }

    #[test]
    fn docker_phrasings_bind_the_registry_template() {
        let p = plan("spin up a docker image of nginx on port 8080").unwrap();
        assert_eq!(p["template"], "docker.run");
        assert_eq!(p["args"]["image"], "nginx");
        assert_eq!(p["args"]["ports"], "8080:80");
        assert_eq!(p["args"]["name"], "cortex-nginx");

        let p = plan("run redis:7 in docker on 6379:6379").unwrap();
        assert_eq!(p["args"]["image"], "redis:7");
        assert_eq!(p["args"]["name"], "cortex-redis");

        // Saying docker explicitly beats the nginx template.
        let p = plan("run nginx in docker on port 8080").unwrap();
        assert_eq!(p["template"], "docker.run");

        // And podman goes to podman.
        let p = plan("run nginx in podman on port 8080").unwrap();
        assert_eq!(p["template"], "podman.run");
    }

    #[test]
    fn service_intents_use_the_prior_state_aware_workflow() {
        let p = plan("run nginx server").unwrap();
        assert_eq!(p["workflow"], "safe-service");
        assert_eq!(p["op"], "start");
        assert_eq!(p["service"], "nginx");

        assert_eq!(plan("stop the postgres service").unwrap()["op"], "stop");
        assert_eq!(plan("restart nginx").unwrap()["op"], "restart");
        assert_eq!(plan("enable nginx on boot").unwrap()["op"], "enable");
        assert_eq!(plan("run nginx").unwrap()["op"], "start");
    }

    #[test]
    fn installs_and_removals_are_planned() {
        let p = plan("install htop").unwrap();
        assert_eq!(p["template"], "package.install");
        assert_eq!(p["args"]["package"], "htop");

        assert_eq!(
            plan("please install the htop package").unwrap()["args"]["package"],
            "htop"
        );
        assert_eq!(
            plan("uninstall htop").unwrap()["template"],
            "package.remove"
        );
        assert_eq!(plan("remove htop").unwrap()["template"], "package.remove");
    }

    #[test]
    fn user_management_is_planned() {
        let p = plan("add user alice").unwrap();
        assert_eq!(p["template"], "user.add");
        assert_eq!(p["args"]["username"], "alice");

        let p = plan("create user deploy with sudo").unwrap();
        assert_eq!(p["template"], "user.add-sudo");
        assert_eq!(p["args"]["username"], "deploy");

        let p = plan("give alice sudo").unwrap();
        assert_eq!(p["template"], "user.grant-sudo");

        let p = plan("remove user alice").unwrap();
        assert_eq!(p["template"], "user.remove");
        // "delete bob's account" style
        let p = plan("delete the account for bob").unwrap();
        assert_eq!(p["template"], "user.remove");
        assert_eq!(p["args"]["username"], "bob");
    }

    #[test]
    fn firewall_and_directories_are_planned() {
        let p = plan("open port 8080").unwrap();
        assert_eq!(p["template"], "firewall.allow");
        assert_eq!(p["args"]["port"], "8080");
        assert_eq!(p["args"]["proto"], "tcp");

        let p = plan("allow udp port 514 through the firewall").unwrap();
        assert_eq!(p["args"]["proto"], "udp");

        let p = plan("close port 8080").unwrap();
        assert_eq!(p["template"], "firewall.remove");

        let p = plan("create directory /opt/app").unwrap();
        assert_eq!(p["template"], "dir.create");
        assert_eq!(p["args"]["path"], "/opt/app");
    }

    /// Typos must not only reach the right template — the typo'd word
    /// itself must never leak into a parameter ("instal htop" once planned
    /// `package=instal`).
    #[test]
    fn typos_still_plan_with_the_right_values() {
        let p = plan("instal htop").unwrap();
        assert_eq!(p["template"], "package.install");
        assert_eq!(p["args"]["package"], "htop");

        let p = plan("run ngnix on port 8080").unwrap();
        assert_eq!(p["template"], "nginx.serve");
        assert_eq!(p["args"]["port"], "8080");

        let p = plan("spin up a dokcer container of nginx on 8080:80").unwrap();
        assert_eq!(p["template"], "docker.run");
        assert_eq!(p["args"]["image"], "nginx");

        let p = plan("removing the user alice").unwrap();
        assert_eq!(p["template"], "user.remove");
        assert_eq!(p["args"]["username"], "alice");
    }

    // ---- phase-2 templates ----------------------------------------------

    #[test]
    fn dnf_phrasings_reach_the_dnf_templates() {
        let p = plan("install htop with dnf").unwrap();
        assert_eq!(p["template"], "package.install-dnf");
        assert_eq!(p["args"]["package"], "htop");

        let p = plan("remove htop with yum").unwrap();
        assert_eq!(p["template"], "package.remove-dnf");
        assert_eq!(p["args"]["package"], "htop");

        // Without a distro word, apt stays the default.
        assert_eq!(plan("install htop").unwrap()["template"], "package.install");
    }

    #[test]
    fn git_clones_extract_url_and_path() {
        let p = plan("clone https://github.com/user/app.git to /srv/app").unwrap();
        assert_eq!(p["template"], "git.clone");
        assert_eq!(p["args"]["repo"], "https://github.com/user/app.git");
        assert_eq!(p["args"]["path"], "/srv/app");

        // Without a destination, it teaches rather than guessing one.
        let n = needs("clone https://github.com/user/app.git").unwrap();
        assert_eq!(n.template_id, "git.clone");
        assert!(n.do_command.contains("path=<path>"), "{}", n.do_command);
    }

    #[test]
    fn docker_volumes_networks_and_swap_are_planned() {
        let p = plan("create a docker volume called appdata").unwrap();
        assert_eq!(p["template"], "docker.volume.create");
        assert_eq!(p["args"]["name"], "appdata");

        let p = plan("create a docker network named appnet").unwrap();
        assert_eq!(p["template"], "docker.network.create");
        assert_eq!(p["args"]["name"], "appnet");

        let p = plan("create a 2G swap file").unwrap();
        assert_eq!(p["template"], "swap.create");
        assert_eq!(p["args"]["size"], "2G");
        assert_eq!(p["args"]["path"], "/swapfile");
    }

    #[test]
    fn tls_and_ssh_teach_their_parameters() {
        // nginx + a TLS word beats the plain nginx template, then teaches.
        let n = needs("serve nginx over https on port 8443").unwrap();
        assert_eq!(n.template_id, "nginx.tls");
        assert!(n.missing.iter().any(|(name, _, _)| name == "cert"));
        assert!(n.do_command.contains("port=8443"), "{}", n.do_command);

        let n = needs("harden the ssh config").unwrap();
        assert_eq!(n.template_id, "sshd.set");
    }

    #[test]
    fn tuning_backup_hosts_and_certbot_teach_their_parameters() {
        let n = needs("set the kernel swappiness").unwrap();
        assert_eq!(n.template_id, "sysctl.set");
        assert!(n.missing.iter().any(|(name, _, _)| name == "previous"));

        let n = needs("take a backup of /etc").unwrap();
        assert_eq!(n.template_id, "backup.dir");
        assert!(n.do_command.contains("dest=<dest>"), "{}", n.do_command);

        let n = needs("add a hosts entry").unwrap();
        assert_eq!(n.template_id, "hosts.add");

        let n = needs("get a letsencrypt certificate").unwrap();
        assert_eq!(n.template_id, "certbot.issue");
        assert!(n.missing.iter().any(|(name, _, _)| name == "domain"));
    }

    // ---- composition ------------------------------------------------------

    #[test]
    fn conjunctions_compose_multiple_plans_in_order() {
        match understand("install htop and open port 8080") {
            Understanding::Composite(plans) => {
                assert_eq!(plans.len(), 2);
                assert_eq!(plans[0].value["template"], "package.install");
                assert_eq!(plans[0].value["args"]["package"], "htop");
                assert_eq!(plans[1].value["template"], "firewall.allow");
                assert_eq!(plans[1].value["args"]["port"], "8080");
            }
            _ => panic!("expected a composite plan"),
        }

        match understand("create user deploy with sudo, then give alice sudo") {
            Understanding::Composite(plans) => {
                assert_eq!(plans.len(), 2);
                assert_eq!(plans[0].value["template"], "user.add-sudo");
                assert_eq!(plans[0].value["args"]["username"], "deploy");
                assert_eq!(plans[1].value["template"], "user.grant-sudo");
                assert_eq!(plans[1].value["args"]["username"], "alice");
            }
            _ => panic!("expected a composite plan"),
        }

        // Three steps, order preserved.
        match understand("install htop, open port 8080, then add user alice") {
            Understanding::Composite(plans) => {
                assert_eq!(plans.len(), 3);
                assert_eq!(plans[0].value["template"], "package.install");
                assert_eq!(plans[1].value["template"], "firewall.allow");
                assert_eq!(plans[2].value["template"], "user.add");
            }
            _ => panic!("expected a three-step composite"),
        }
    }

    /// Composition must never change what a single-intent sentence means: if
    /// any segment fails to plan, the whole sentence is read as one request.
    #[test]
    fn a_failed_segment_falls_back_to_whole_sentence_reading() {
        // "and" inside a single intent: seg2 ("expose it on port 8080") plans
        // firewall.allow, but seg1 ("spin up nginx") lacks a port, so the
        // whole sentence is read as one — nginx.serve on 8080.
        let p = plan("spin up nginx and expose it on port 8080").unwrap();
        assert_eq!(p["template"], "nginx.serve");
        assert_eq!(p["args"]["port"], "8080");
    }

    /// Pronouns are never unit names: "start it" must ask which unit rather
    /// than plan `systemctl start it`.
    #[test]
    fn pronouns_are_not_service_units() {
        for text in ["start it", "restart them", "stop everything"] {
            match understand(text) {
                Understanding::Plan(p) => panic!("must not plan `{text}`: {}", p.summary),
                Understanding::Unknown(u) => {
                    let r = u.reason.unwrap_or_default();
                    assert!(r.contains("which unit"), "{text}: {r}");
                }
                _ => {}
            }
        }
    }

    /// An undo segment never composes: reversing is a different mode, not a
    /// step in a sequence.
    #[test]
    fn undo_never_composes_with_other_steps() {
        for text in ["install htop and then undo", "undo and install htop"] {
            assert!(
                !matches!(understand(text), Understanding::Composite(_)),
                "{text} must not become a composite"
            );
        }
        // Bare undo still wins outright.
        assert!(matches!(understand("undo everything"), Understanding::Undo));
    }

    /// docker.app: env/volume come from key=value, the rest from extraction.
    #[test]
    fn docker_app_binds_from_mixed_sources() {
        let p =
            plan("run myapp in docker on port 8080 env=NODE_ENV=production volume=/srv/data:/data")
                .unwrap();
        assert_eq!(p["template"], "docker.app");
        assert_eq!(p["args"]["image"], "myapp");
        assert_eq!(p["args"]["ports"], "8080:80");
        assert_eq!(p["args"]["env"], "NODE_ENV=production");
        assert_eq!(p["args"]["volume"], "/srv/data:/data");
    }

    #[test]
    fn split_tasks_is_conservative() {
        assert_eq!(
            split_tasks("install htop and open port 8080"),
            vec!["install htop", "open port 8080"]
        );
        assert_eq!(
            split_tasks("install htop; open port 8080, then add user alice"),
            vec!["install htop", "open port 8080", "add user alice"]
        );
        assert_eq!(split_tasks("run nginx on port 8080").len(), 1);
        assert_eq!(split_tasks("").len(), 0);
    }

    #[test]
    fn a_leading_template_id_is_explicit() {
        let p = plan("docker.run name=web image=nginx ports=8080:80").unwrap();
        assert_eq!(p["template"], "docker.run");
        assert_eq!(p["args"]["name"], "web");

        // With holes, it teaches instead of failing.
        let n = needs("docker.run image=nginx").unwrap();
        assert!(n.do_command.contains("name=<name>"), "{}", n.do_command);
        assert!(n.do_command.contains("ports=<ports>"), "{}", n.do_command);
    }

    // ---- the teachable misses -------------------------------------------

    #[test]
    fn missing_parameters_teach_the_exact_command() {
        // nginx without a port: the template is clear, the port is not.
        let n = needs("serve nginx").unwrap();
        assert_eq!(n.template_id, "nginx.serve");
        assert!(n.missing.iter().any(|(name, _, _)| name == "port"));
        assert!(n.do_command.starts_with("cortex do nginx.serve"));
    }

    #[test]
    fn a_named_port_stops_the_service_intent_guessing() {
        // `systemctl start postgres` cannot honour a port; the planner must
        // refuse with the alternatives, not silently drop the port.
        match understand("start postgres on port 5432") {
            Understanding::Unknown(u) => {
                let r = u.reason.expect("should carry the port explanation");
                assert!(r.contains("names a port"), "{r}");
            }
            _ => panic!("must not plan a bare service start when a port is named"),
        }
    }

    #[test]
    fn ambiguity_defers_rather_than_guessing() {
        for text in [
            "why is my disk full",
            "upgrade nginx, apply new config, test traffic",
            "",
            "reinstall the world backwards",
        ] {
            match understand(text) {
                Understanding::Plan(p) => panic!("must not plan `{text}`: {}", p.summary),
                Understanding::Undo => panic!("must not read `{text}` as undo"),
                _ => {}
            }
        }
    }

    #[test]
    fn unknown_requests_suggest_the_nearest_templates() {
        match understand("swap the simlink at /srv/current") {
            Understanding::Unknown(u) => {
                assert!(
                    u.suggestions.iter().any(|s| s.id == "symlink.swap"),
                    "suggestions: {:?}",
                    u.suggestions.iter().map(|s| &s.id).collect::<Vec<_>>()
                );
            }
            Understanding::NeedsInput(n) => {
                // Also acceptable: matched symlink.swap, asking for params.
                assert_eq!(n.template_id, "symlink.swap");
            }
            _ => panic!("expected unknown-with-suggestions or needs-input"),
        }
    }

    #[test]
    fn undo_intent_is_recognised_first() {
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
            assert!(matches!(understand(yes), Understanding::Undo), "{yes}");
        }
        for no in [
            "run nginx server",
            "undocker the thing",
            "install undo-manager",
        ] {
            assert!(!matches!(understand(no), Understanding::Undo), "{no}");
        }
    }

    // ---- user templates participate ------------------------------------

    #[test]
    fn user_templates_match_through_their_own_keywords() {
        let mut templates = cortex_registry::builtins().to_vec();
        templates.push(Template {
            id: "user.flag".into(),
            summary: "Drop a flag file".into(),
            category: "custom".into(),
            keywords: vec![vec!["flag".into()]],
            verbs: vec!["drop".into()],
            example: "cortex do user.flag path=/tmp/flag".into(),
            params: vec![cortex_registry::Param {
                name: "path".into(),
                about: "where".into(),
                kind: cortex_registry::ParamKind::AbsPath,
                default: None,
            }],
            forward: "touch {path}".into(),
            verify_forward: "test -e {path}".into(),
            inverse: "rm -f {path}".into(),
            verify_inverse: "! test -e {path}".into(),
            host_side: true,
            drift_note: String::new(),
        });
        match understand_with("drop a flag at /tmp/f", &templates) {
            Understanding::Plan(p) => {
                assert_eq!(p.value["template"], "user.flag");
                assert_eq!(p.value["args"]["path"], "/tmp/f");
            }
            _ => panic!("user template should match and bind"),
        }
    }

    #[test]
    fn determinism_same_text_same_plan() {
        let a = plan("run nginx on port 8080").unwrap();
        for _ in 0..10 {
            assert_eq!(a, plan("run nginx on port 8080").unwrap());
        }
    }
}
