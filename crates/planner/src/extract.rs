//! Parameter extraction: pull typed values out of approximate English.
//!
//! Everything here is a pure function over the tokenized request. The rules
//! are deliberately narrow — a value is extracted only when its shape is
//! unambiguous (a `host:container` mapping, an absolute path, "port 8080").
//! A missed extraction costs the user one `key=value`; a wrong extraction
//! would put a wrong value in a plan, so ambiguity always extracts nothing.

use crate::{fuzzy, Request};
use cortex_registry::ParamKind;

/// Noise filtering must use the same approximate matcher that triggers
/// templates: "instal htop" fuzzy-matches the install intent, so "instal"
/// must also count as scaffolding — otherwise the typo'd verb itself would
/// be extracted as the package name.
pub(crate) fn is_noise(word: &str, noise: &[&str]) -> bool {
    noise.iter().any(|n| fuzzy::word_matches(word, n))
}

/// "on port 8080", "port 8080", "on 8080" → "8080".
pub fn port(req: &Request) -> Option<String> {
    let words = req.low_words();
    for (i, w) in words.iter().enumerate() {
        if (*w == "port" || *w == "on") && i + 1 < words.len() && is_port(&words[i + 1]) {
            return Some(words[i + 1].clone());
        }
    }
    None
}

/// An explicit "8080:80" token → itself; otherwise "port 8080"/"on 8080" is
/// read as publishing host port N to container port 80 — the assumption that
/// is right for the web-server images this phrasing is used with, and stated
/// in the rendered plan either way.
pub fn port_mapping(req: &Request) -> Option<String> {
    for t in req.low_words() {
        if let Some((h, c)) = t.split_once(':') {
            if is_port_or_zero(h) && is_port(c) {
                return Some(t.clone());
            }
        }
    }
    port(req).map(|p| format!("{p}:80"))
}

pub fn is_port(s: &str) -> bool {
    !s.is_empty() && s.chars().all(|c| c.is_ascii_digit()) && s.parse::<u16>().is_ok_and(|n| n > 0)
}

fn is_port_or_zero(s: &str) -> bool {
    !s.is_empty() && s.chars().all(|c| c.is_ascii_digit()) && s.parse::<u16>().is_ok()
}

/// Does the text name a network port at all? Used by the service intent to
/// refuse rather than silently drop a port it cannot honour.
pub fn mentions_a_port(req: &Request) -> bool {
    port(req).is_some()
        || req.low_words().iter().any(|t| {
            t.split_once(':')
                .is_some_and(|(h, c)| is_port(h) && is_port(c))
        })
}

/// The container image: an `image:tag` token, or the first word that is not
/// scaffolding.
pub fn image(req: &Request) -> Option<String> {
    const NOISE: &[&str] = &[
        "a",
        "an",
        "the",
        "docker",
        "podman",
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
        "deploy",
        "new",
    ];
    let words = req.low_words();
    // Prefer an explicit `image:tag` (a tag is not all digits — that is a
    // port mapping's job).
    if let Some(tagged) = words.iter().find(|s| {
        s.contains(':')
            && !s.contains('/')
            && s.split(':')
                .nth(1)
                .is_some_and(|p| !p.is_empty() && !p.chars().all(|c| c.is_ascii_digit()))
    }) {
        return Some(tagged.clone());
    }
    words
        .iter()
        .find(|s| !is_noise(s, NOISE) && !s.chars().all(|c| c.is_ascii_digit() || c == ':'))
        .cloned()
}

/// A container name the inverse can address, derived from the image: stable,
/// predictable, and visibly cortex's.
pub fn container_name(image: &str) -> String {
    format!(
        "cortex-{}",
        image.split(':').next().unwrap_or(image).replace('/', "-")
    )
}

/// The first absolute path in the request (original casing preserved).
pub fn abs_path(req: &Request) -> Option<String> {
    req.raw_words()
        .iter()
        .find(|t| ParamKind::AbsPath.validate("path", t).is_ok())
        .cloned()
}

/// A username: the token after "user"/"account"/"for", or failing that the
/// first token that could be a login name and is not scaffolding.
pub fn username(req: &Request) -> Option<String> {
    const MARKERS: &[&str] = &["user", "account", "for"];
    const NOISE: &[&str] = &[
        "a",
        "an",
        "the",
        "new",
        "add",
        "create",
        "make",
        "remove",
        "delete",
        "del",
        "drop",
        "user",
        "account",
        "with",
        "and",
        "please",
        "sudo",
        "sudoer",
        "admin",
        "administrator",
        "grant",
        "give",
        "promote",
        "allow",
        "to",
        "access",
        "rights",
        "privileges",
        "named",
        "called",
        "ssh",
        "key",
        "for",
    ];
    let low = req.low_words();
    let raw = req.raw_words();
    for (i, w) in low.iter().enumerate() {
        if MARKERS.contains(&w.as_str()) && i + 1 < low.len() {
            let candidate = &raw[i + 1];
            if ParamKind::User.validate("username", candidate).is_ok()
                && !is_noise(&low[i + 1], NOISE)
            {
                return Some(candidate.clone());
            }
        }
    }
    low.iter()
        .zip(raw.iter())
        .find(|(l, r)| !is_noise(l, NOISE) && ParamKind::User.validate("username", r).is_ok())
        .map(|(_, r)| r.clone())
}

/// A package name: the first token that is not install/remove scaffolding.
pub fn package(req: &Request) -> Option<String> {
    const NOISE: &[&str] = &[
        "install",
        "uninstall",
        "purge",
        "remove",
        "delete",
        "the",
        "a",
        "an",
        "package",
        "apt",
        "dnf",
        "yum",
        "get",
        "please",
        "using",
        "with",
        "for",
        "me",
        "now",
    ];
    req.low_words()
        .iter()
        .find(|s| !is_noise(s, NOISE))
        .cloned()
}

/// The token after "called"/"named" (for "a service called worker").
pub fn named(req: &Request) -> Option<String> {
    let low = req.low_words();
    let raw = req.raw_words();
    for (i, w) in low.iter().enumerate() {
        if (w == "called" || w == "named") && i + 1 < low.len() {
            return Some(raw[i + 1].clone());
        }
    }
    None
}

/// A repository URL: an `https://`, `http://`, `ssh://` or `git@` token
/// (original casing preserved — URLs are case-sensitive).
pub fn url(req: &Request) -> Option<String> {
    req.raw_words()
        .iter()
        .find(|t| {
            let l = t.to_lowercase();
            l.starts_with("https://")
                || l.starts_with("http://")
                || l.starts_with("ssh://")
                || l.starts_with("git@")
        })
        .cloned()
}

/// A storage size like `2G`, `512M` or `1T` — digits then one unit letter.
pub fn size(req: &Request) -> Option<String> {
    req.raw_words()
        .iter()
        .find(|t| {
            t.len() >= 2
                && t[..t.len() - 1].chars().all(|c| c.is_ascii_digit())
                && t.chars().last().is_some_and(|c| "KMGTkmgt".contains(c))
        })
        .map(|t| t.to_uppercase())
}

/// A compose file: an absolute path ending in .yml/.yaml.
pub fn compose_file(req: &Request) -> Option<String> {
    req.raw_words()
        .iter()
        .find(|t| {
            let l = t.to_lowercase();
            (l.ends_with(".yml") || l.ends_with(".yaml")) && t.starts_with('/')
        })
        .cloned()
}

/// A compose project name derived from the compose file's directory
/// ("/srv/app/docker-compose.yml" → "app").
pub fn compose_project(file: &str) -> Option<String> {
    let parent = std::path::Path::new(file).parent()?.file_name()?;
    let name = parent.to_str()?.to_string();
    ParamKind::Ident
        .validate("project", &name)
        .ok()
        .map(|()| name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Request;

    fn req(s: &str) -> Request {
        Request::parse(s)
    }

    #[test]
    fn ports_and_mappings() {
        assert_eq!(
            port(&req("run nginx on port 8080")).as_deref(),
            Some("8080")
        );
        assert_eq!(port(&req("nginx on 8080")).as_deref(), Some("8080"));
        assert_eq!(port(&req("enable nginx on boot")), None);
        assert_eq!(port(&req("port 99999")), None); // not a u16

        assert_eq!(
            port_mapping(&req("map 8080:80 please")).as_deref(),
            Some("8080:80")
        );
        assert_eq!(
            port_mapping(&req("on port 8080")).as_deref(),
            Some("8080:80")
        );
        assert!(mentions_a_port(&req("run x on port 80")));
        assert!(!mentions_a_port(&req("the port is open")));
    }

    #[test]
    fn images_prefer_tags() {
        assert_eq!(
            image(&req("run redis:7 in docker on 6379:6379")).as_deref(),
            Some("redis:7")
        );
        assert_eq!(
            image(&req("spin up a docker image of nginx on port 8080")).as_deref(),
            Some("nginx")
        );
        assert_eq!(container_name("redis:7"), "cortex-redis");
        assert_eq!(container_name("ghcr.io/a/b"), "cortex-ghcr.io-a-b");
    }

    #[test]
    fn paths_keep_their_case() {
        assert_eq!(
            abs_path(&req("create directory /opt/MyApp")).as_deref(),
            Some("/opt/MyApp")
        );
        assert_eq!(abs_path(&req("create directory apps")), None);
    }

    #[test]
    fn usernames_follow_markers() {
        assert_eq!(username(&req("add user alice")).as_deref(), Some("alice"));
        assert_eq!(
            username(&req("create a new account for bob")).as_deref(),
            Some("bob")
        );
        assert_eq!(username(&req("give carol sudo")).as_deref(), Some("carol"));
        assert_eq!(
            username(&req("remove the user dave please")).as_deref(),
            Some("dave")
        );
    }

    #[test]
    fn packages_skip_scaffolding() {
        assert_eq!(package(&req("install htop")).as_deref(), Some("htop"));
        assert_eq!(
            package(&req("please apt install the htop package")).as_deref(),
            Some("htop")
        );
        assert_eq!(package(&req("uninstall htop")).as_deref(), Some("htop"));
    }

    #[test]
    fn named_follows_its_markers() {
        assert_eq!(
            named(&req("create a docker volume called appdata")).as_deref(),
            Some("appdata")
        );
        assert_eq!(
            named(&req("a network named appnet please")).as_deref(),
            Some("appnet")
        );
        assert_eq!(named(&req("create a docker volume")), None);
    }

    #[test]
    fn urls_and_sizes() {
        assert_eq!(
            url(&req("clone https://github.com/User/App.git to /srv/app")).as_deref(),
            Some("https://github.com/User/App.git")
        );
        assert_eq!(
            url(&req("clone git@github.com:user/app.git to /srv/app")).as_deref(),
            Some("git@github.com:user/app.git")
        );
        assert_eq!(url(&req("clone the repo to /srv/app")), None);

        assert_eq!(size(&req("add a 2G swap file")).as_deref(), Some("2G"));
        assert_eq!(size(&req("make a 512m swapfile")).as_deref(), Some("512M"));
        assert_eq!(size(&req("swap on port 8080")), None); // bare number is not a size
    }

    #[test]
    fn compose_files_and_projects() {
        let r = req("bring up the compose stack at /srv/app/docker-compose.yml");
        assert_eq!(
            compose_file(&r).as_deref(),
            Some("/srv/app/docker-compose.yml")
        );
        assert_eq!(
            compose_project("/srv/app/docker-compose.yml").as_deref(),
            Some("app")
        );
    }
}
