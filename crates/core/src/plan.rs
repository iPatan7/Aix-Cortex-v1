//! Offline intent matching: the reason `cortex try` is fast.
//!
//! Most DevOps requests are drawn from a tiny vocabulary — start a service,
//! install a package, run a container. Sending those to a language model
//! costs a network round trip (often seconds) to rediscover something a
//! regex knows. Worse, it makes the tool useless on a box with no network,
//! which is frequently the box you are trying to fix.
//!
//! So the hero command matches locally first and only falls back to a model
//! for genuinely novel requests. Every plan produced here names a registry
//! template or a built-in workflow, so the offline path is exactly as
//! reversible as the model path — it is faster, not looser.
//!
//! The matcher is deliberately conservative: it fires only on unambiguous
//! phrasing. A miss costs one model call; a false match would run the wrong
//! command, so ambiguity always defers.

use serde_json::{json, Value};

/// A plan matched without a model.
pub struct Offline {
    /// One line naming what was understood, echoed to the operator.
    pub summary: String,
    /// The plan object, in the same shape the CLI's plan dispatcher
    /// accepts from the model.
    pub value: Value,
}

/// Try to understand `text` without a model. `None` means "ask the model".
pub fn offline(text: &str) -> Option<Offline> {
    let t = normalize(text);
    let w: Vec<&str> = t.split_whitespace().collect();

    docker_run(&t, &w)
        .or_else(|| service(&t, &w))
        .or_else(|| install(&t, &w))
}

/// Lowercase, strip punctuation that never carries meaning here.
fn normalize(text: &str) -> String {
    text.trim()
        .to_lowercase()
        .replace([',', '.', '!', '?', '"', '\''], " ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// "spin up a docker image of nginx on port 8080"
/// "run nginx in docker on 8080:80"
fn docker_run(t: &str, w: &[&str]) -> Option<Offline> {
    if !t.contains("docker") && !t.contains("container") {
        return None;
    }
    // Only forward intents; "stop the docker container" is not a run.
    const VERBS: &[&str] = &["run", "start", "spin", "launch", "bring"];
    if !VERBS.iter().any(|v| w.contains(v)) {
        return None;
    }

    let image = image_name(w)?;
    let ports = port_mapping(t)?;
    // A stable, predictable name so the inverse can address it and a second
    // `try` of the same thing is visibly idempotent rather than duplicated.
    let name = format!(
        "cortex-{}",
        image.split(':').next().unwrap_or(&image).replace('/', "-")
    );

    Some(Offline {
        summary: format!("docker.run {image} on {ports}"),
        value: json!({
            "workflow": "template",
            "template": "docker.run",
            "args": { "name": name, "image": image, "ports": ports }
        }),
    })
}

/// The token after "of"/"image"/"container", or a known image word.
fn image_name(w: &[&str]) -> Option<String> {
    const NOISE: &[&str] = &[
        "a",
        "an",
        "the",
        "docker",
        "image",
        "container",
        "of",
        "up",
        "in",
        "on",
        "port",
        "with",
        "run",
        "start",
        "spin",
        "launch",
        "bring",
        "and",
        "to",
        "using",
    ];
    // Prefer an explicit `image:tag`.
    if let Some(tagged) = w.iter().find(|s| {
        s.contains(':')
            && !s.contains('/')
            && s.split(':')
                .nth(1)
                .is_some_and(|p| !p.chars().all(|c| c.is_ascii_digit()))
    }) {
        return Some((*tagged).to_string());
    }
    w.iter()
        .find(|s| !NOISE.contains(s) && !s.chars().all(|c| c.is_ascii_digit() || c == ':'))
        .map(|s| s.to_string())
}

/// "on port 8080" -> "8080:80"; "8080:80" -> itself.
fn port_mapping(t: &str) -> Option<String> {
    // Explicit host:container mapping wins.
    for tok in t.split_whitespace() {
        if let Some((h, c)) = tok.split_once(':') {
            if !h.is_empty()
                && h.chars().all(|c| c.is_ascii_digit())
                && c.chars().all(|c| c.is_ascii_digit())
            {
                return Some(tok.to_string());
            }
        }
    }
    // "port 8080" / "on 8080" — assume the container listens on 80, which is
    // right for the web servers this phrasing is used for.
    let w: Vec<&str> = t.split_whitespace().collect();
    for (i, tok) in w.iter().enumerate() {
        if (*tok == "port" || *tok == "on") && i + 1 < w.len() {
            let p = w[i + 1];
            if !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()) {
                return Some(format!("{p}:80"));
            }
        }
    }
    None
}

/// "run nginx server", "start nginx", "stop the postgres service"
fn service(t: &str, w: &[&str]) -> Option<Offline> {
    // A docker phrasing already claimed this text.
    if t.contains("docker") || t.contains("container") {
        return None;
    }
    let op = if w.contains(&"restart") {
        "restart"
    } else if w.contains(&"stop") {
        "stop"
    } else if w.contains(&"enable") {
        "enable"
    } else if w.contains(&"disable") {
        "disable"
    } else if w.contains(&"start") || w.contains(&"run") {
        "start"
    } else {
        return None;
    };

    // The unit is the word that is not scaffolding.
    const NOISE: &[&str] = &[
        "the", "a", "an", "service", "server", "daemon", "unit", "up", "please", "my", "start",
        "stop", "restart", "enable", "disable", "run", "on", "boot",
    ];
    let unit = w.iter().find(|s| !NOISE.contains(s))?;
    // Bare "start the server" names nothing runnable.
    if unit.len() < 2 {
        return None;
    }

    Some(Offline {
        summary: format!("service {op} {unit}"),
        value: json!({ "workflow": "safe-service", "op": op, "service": unit }),
    })
}

/// "install nginx", "apt install htop"
fn install(t: &str, w: &[&str]) -> Option<Offline> {
    if !w.contains(&"install") {
        return None;
    }
    if t.contains("docker") || t.contains("upgrade") {
        return None;
    }
    const NOISE: &[&str] = &[
        "install", "the", "a", "an", "package", "apt", "get", "please", "using",
    ];
    let pkg = w.iter().find(|s| !NOISE.contains(s))?;
    Some(Offline {
        summary: format!("install {pkg}"),
        value: json!({ "workflow": "safe-install", "package": pkg }),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plan(text: &str) -> Option<Value> {
        offline(text).map(|o| o.value)
    }

    #[test]
    fn service_intents_need_no_model() {
        let p = plan("run nginx server").unwrap();
        assert_eq!(p["workflow"], "safe-service");
        assert_eq!(p["op"], "start");
        assert_eq!(p["service"], "nginx");

        assert_eq!(plan("stop the postgres service").unwrap()["op"], "stop");
        assert_eq!(plan("restart nginx").unwrap()["op"], "restart");
        assert_eq!(plan("enable nginx on boot").unwrap()["op"], "enable");
    }

    #[test]
    fn docker_intents_bind_the_registry_template() {
        let p = plan("spin up a docker image of nginx on port 8080").unwrap();
        assert_eq!(p["workflow"], "template");
        assert_eq!(p["template"], "docker.run");
        assert_eq!(p["args"]["image"], "nginx");
        assert_eq!(p["args"]["ports"], "8080:80");
        // A stable name is what makes the inverse addressable.
        assert_eq!(p["args"]["name"], "cortex-nginx");

        let p = plan("run redis:7 in docker on 6379:6379").unwrap();
        assert_eq!(p["args"]["image"], "redis:7");
        assert_eq!(p["args"]["ports"], "6379:6379");
        assert_eq!(p["args"]["name"], "cortex-redis");
    }

    #[test]
    fn install_intents_are_matched() {
        assert_eq!(plan("install nginx").unwrap()["package"], "nginx");
        assert_eq!(plan("apt install htop").unwrap()["package"], "htop");
    }

    /// A miss costs one model call. A false match runs the wrong command.
    /// Anything ambiguous must defer.
    #[test]
    fn ambiguous_text_defers_to_the_model() {
        assert!(plan("upgrade nginx, apply new config, test traffic").is_none());
        assert!(plan("why is my disk full").is_none());
        assert!(plan("").is_none());
        // "docker" with no port has no safe default mapping.
        assert!(plan("run nginx in docker").is_none());
        // A service phrasing with no unit named.
        assert!(plan("start the server").is_none());
        // Not an install verb we recognise.
        assert!(plan("reinstall the world").is_none());
    }

    /// Docker phrasing must not be stolen by the service matcher, which
    /// would `systemctl start nginx` when the user asked for a container.
    #[test]
    fn docker_wins_over_service_for_container_phrasing() {
        let p = plan("run nginx in docker on port 8080").unwrap();
        assert_eq!(p["template"], "docker.run");
        assert!(plan("start the nginx container on port 80").is_some());
    }
}
