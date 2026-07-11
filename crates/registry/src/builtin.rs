//! The built-in templates: every reversible operation cortex ships with.
//!
//! Adding a template here is the way to extend cortex. Each entry is a
//! promise a human made and a test can check: `cortex verify --self` runs
//! every exercisable template's forward, asserts `verify_forward`, runs the
//! inverse, and asserts `verify_inverse`.
//!
//! Ordering matters to the planner: when two templates match a request with
//! the same score, the one defined first wins. More specific templates
//! (a container run, an nginx site) come before broader ones (a package
//! install), so "install nginx on port 8080" plans the port-honouring
//! template rather than silently dropping the port.

use crate::{Param, ParamKind, Template};

fn p(name: &str, about: &str, kind: ParamKind) -> Param {
    Param {
        name: name.into(),
        about: about.into(),
        kind,
        default: None,
    }
}

fn pd(name: &str, about: &str, kind: ParamKind, default: &str) -> Param {
    Param {
        name: name.into(),
        about: about.into(),
        kind,
        default: Some(default.into()),
    }
}

fn kw(groups: &[&[&str]]) -> Vec<Vec<String>> {
    groups
        .iter()
        .map(|g| g.iter().map(|s| s.to_string()).collect())
        .collect()
}

fn vs(words: &[&str]) -> Vec<String> {
    words.iter().map(|s| s.to_string()).collect()
}

/// Construct the full built-in catalog.
#[allow(clippy::too_many_lines)] // a catalog is long by nature; each entry is flat
pub fn builtins() -> Vec<Template> {
    vec![
        // ---- containers --------------------------------------------------
        Template {
            id: "docker.run".into(),
            summary: "Run a container detached, published on a host port".into(),
            category: "containers".into(),
            keywords: kw(&[&["docker", "container"]]),
            verbs: vs(&["run", "start", "spin", "launch", "up", "deploy"]),
            example: "cortex try \"run nginx in docker on port 8080\"".into(),
            params: vec![
                p("name", "container name (the inverse addresses it)", ParamKind::Ident),
                p("image", "image to run, e.g. nginx or redis:7", ParamKind::Image),
                p("ports", "host:container port mapping", ParamKind::PortMapping),
            ],
            // `--restart=no` so an undone container cannot be resurrected by
            // the daemon; the name is what the inverse addresses.
            forward: "docker run -d --restart=no --name {name} -p {ports} {image}".into(),
            verify_forward:
                "docker ps --filter name=^{name}$ --filter status=running -q | grep -q .".into(),
            inverse: "docker rm -f {name}".into(),
            // The post-condition that `echo` could never satisfy.
            verify_inverse: "! docker ps -a --filter name=^{name}$ -q | grep -q .".into(),
            host_side: true,
            drift_note: "container state lives in dockerd, not the filesystem; undo is the \
                         verified compensation (docker rm -f), which also removes a container \
                         someone restarted in the meantime"
                .into(),
        },
        Template {
            id: "podman.run".into(),
            summary: "Run a podman container detached, published on a host port".into(),
            category: "containers".into(),
            keywords: kw(&[&["podman"]]),
            verbs: vs(&["run", "start", "spin", "launch", "up", "deploy"]),
            example: "cortex try \"run nginx in podman on port 8080\"".into(),
            params: vec![
                p("name", "container name (the inverse addresses it)", ParamKind::Ident),
                p("image", "image to run, e.g. nginx or redis:7", ParamKind::Image),
                p("ports", "host:container port mapping", ParamKind::PortMapping),
            ],
            forward: "podman run -d --restart=no --name {name} -p {ports} {image}".into(),
            verify_forward:
                "podman ps --filter name=^{name}$ --filter status=running -q | grep -q .".into(),
            inverse: "podman rm -f {name}".into(),
            verify_inverse: "! podman ps -a --filter name=^{name}$ -q | grep -q .".into(),
            host_side: true,
            drift_note: "container state lives in podman, not the filesystem; undo is the \
                         verified compensation (podman rm -f)"
                .into(),
        },
        Template {
            id: "docker.compose.up".into(),
            summary: "Bring up a compose project".into(),
            category: "containers".into(),
            keywords: kw(&[&["compose", "stack"]]),
            verbs: vs(&["up", "start", "run", "launch", "bring", "deploy"]),
            example: "cortex do docker.compose.up project=app file=/srv/app/docker-compose.yml"
                .into(),
            params: vec![
                p("project", "compose project name", ParamKind::Ident),
                p("file", "absolute path to the compose file", ParamKind::AbsPath),
            ],
            forward: "docker compose -p {project} -f {file} up -d".into(),
            verify_forward:
                "docker compose -p {project} -f {file} ps --status running -q | grep -q .".into(),
            inverse: "docker compose -p {project} -f {file} down -v".into(),
            verify_inverse: "! docker compose -p {project} -f {file} ps -q | grep -q .".into(),
            host_side: true,
            drift_note: "undo runs `compose down -v`, which removes the project's volumes; \
                         data written into those volumes after commit is removed with them"
                .into(),
        },
        Template {
            id: "docker.app".into(),
            summary: "Run a container with an env var and a persistent volume".into(),
            category: "containers".into(),
            keywords: kw(&[
                &["docker", "container"],
                &["env", "environment", "volume", "mount", "persistent"],
            ]),
            verbs: vs(&["run", "start", "spin", "launch", "up", "deploy"]),
            example: "cortex do docker.app name=app image=myapp ports=8080:80 \
                      env=NODE_ENV=production volume=/srv/data:/data"
                .into(),
            params: vec![
                p("name", "container name (the inverse addresses it)", ParamKind::Ident),
                p("image", "image to run, e.g. myapp:v2", ParamKind::Image),
                p("ports", "host:container port mapping", ParamKind::PortMapping),
                p("env", "KEY=value environment variable", ParamKind::EnvVar),
                p("volume", "host:container volume mapping", ParamKind::VolumeMapping),
            ],
            forward: "docker run -d --restart=no --name {name} -p {ports} -e {env} -v {volume} {image}".into(),
            verify_forward:
                "docker ps --filter name=^{name}$ --filter status=running -q | grep -q .".into(),
            inverse: "docker rm -f {name}".into(),
            verify_inverse: "! docker ps -a --filter name=^{name}$ -q | grep -q .".into(),
            host_side: true,
            drift_note: "undo removes the container but never the host directory behind the \
                         volume mount — data written under it stays where it is"
                .into(),
        },
        Template {
            id: "docker.volume.create".into(),
            summary: "Create a named docker volume".into(),
            category: "containers".into(),
            keywords: kw(&[&["docker"], &["volume"]]),
            verbs: vs(&["create", "add", "make", "new"]),
            example: "cortex do docker.volume.create name=appdata".into(),
            params: vec![p("name", "volume name", ParamKind::Ident)],
            forward: "docker volume create {name}".into(),
            verify_forward: "docker volume inspect {name} >/dev/null 2>&1".into(),
            inverse: "docker volume rm {name}".into(),
            verify_inverse: "! docker volume inspect {name} >/dev/null 2>&1".into(),
            host_side: true,
            drift_note: "undo runs `docker volume rm`, which refuses while a container still \
                         uses the volume — it will not delete data out from under a workload"
                .into(),
        },
        Template {
            id: "docker.network.create".into(),
            summary: "Create a user-defined docker network".into(),
            category: "containers".into(),
            keywords: kw(&[&["docker"], &["network"]]),
            verbs: vs(&["create", "add", "make", "new"]),
            example: "cortex do docker.network.create name=appnet".into(),
            params: vec![p("name", "network name", ParamKind::Ident)],
            forward: "docker network create {name}".into(),
            verify_forward: "docker network inspect {name} >/dev/null 2>&1".into(),
            inverse: "docker network rm {name}".into(),
            verify_inverse: "! docker network inspect {name} >/dev/null 2>&1".into(),
            host_side: true,
            drift_note: "undo runs `docker network rm`, which refuses while a container is \
                         still attached to the network"
                .into(),
        },
        // ---- web ---------------------------------------------------------
        Template {
            id: "nginx.tls".into(),
            summary: "Serve a directory over nginx with TLS on a chosen port".into(),
            category: "web".into(),
            keywords: kw(&[
                &["nginx"],
                &["tls", "ssl", "https", "cert", "certificate", "secure"],
            ]),
            verbs: vs(&["run", "serve", "host", "enable", "setup", "set", "add", "secure"]),
            example: "cortex do nginx.tls cert=/etc/ssl/certs/site.pem key=/etc/ssl/private/site.key".into(),
            params: vec![
                p("cert", "absolute path to the TLS certificate (fullchain)", ParamKind::AbsPath),
                p("key", "absolute path to the TLS private key", ParamKind::AbsPath),
                pd("port", "TCP port nginx should listen on", ParamKind::Port, "443"),
                pd("root", "directory to serve", ParamKind::AbsPath, "/var/www/html"),
                pd("name", "site name (the config file cortex owns)", ParamKind::Ident, "tls"),
            ],
            forward: "test -s {cert} && test -s {key} && mkdir -p /etc/nginx/conf.d && printf 'server {{\\n    listen %s ssl;\\n    server_name _;\\n    ssl_certificate %s;\\n    ssl_certificate_key %s;\\n    root %s;\\n    index index.html index.htm;\\n}}\\n' {port} {cert} {key} {root} > /etc/nginx/conf.d/cortex-{name}.conf && nginx -t && systemctl reload-or-restart nginx".into(),
            verify_forward: "ss -ltn | grep -q ':{port} '".into(),
            inverse: "rm -f /etc/nginx/conf.d/cortex-{name}.conf && systemctl reload-or-restart nginx".into(),
            verify_inverse: "! ss -ltn | grep -q ':{port} '".into(),
            host_side: true,
            drift_note: "the site is a single file cortex owns \
                         (/etc/nginx/conf.d/cortex-<name>.conf); the certificate and key are \
                         referenced, never owned — undo removes the site file only"
                .into(),
        },
        Template {
            id: "nginx.serve".into(),
            summary: "Serve a directory over nginx on a chosen port".into(),
            category: "web".into(),
            keywords: kw(&[&["nginx"]]),
            verbs: vs(&["run", "start", "serve", "host", "launch", "expose", "install", "setup", "set", "put"]),
            example: "cortex try \"run nginx on port 8080\"".into(),
            params: vec![
                p("port", "TCP port nginx should listen on", ParamKind::Port),
                pd("root", "directory to serve", ParamKind::AbsPath, "/var/www/html"),
                pd("name", "site name (the config file cortex owns)", ParamKind::Ident, "site"),
            ],
            // The site is one file cortex owns outright, validated by
            // `nginx -t` before the running server ever sees it.
            forward: "mkdir -p /etc/nginx/conf.d && printf 'server {{\\n    listen %s;\\n    server_name _;\\n    root %s;\\n    index index.html index.htm;\\n}}\\n' {port} {root} > /etc/nginx/conf.d/cortex-{name}.conf && nginx -t && systemctl reload-or-restart nginx".into(),
            // Exiting 0 proves nothing; something listening on the port does.
            verify_forward: "ss -ltn | grep -q ':{port} '".into(),
            inverse: "rm -f /etc/nginx/conf.d/cortex-{name}.conf && systemctl reload-or-restart nginx".into(),
            verify_inverse: "! ss -ltn | grep -q ':{port} '".into(),
            host_side: true,
            drift_note: "the site is a single file cortex owns \
                         (/etc/nginx/conf.d/cortex-<name>.conf); undo removes it and reloads \
                         nginx — hand edits to that file after commit are removed with it"
                .into(),
        },
        Template {
            id: "certbot.issue".into(),
            summary: "Obtain a Let's Encrypt certificate with certbot (standalone)".into(),
            category: "web".into(),
            keywords: kw(&[
                &["certbot", "letsencrypt", "cert", "certificate"],
                &["issue", "obtain", "get", "request", "new", "letsencrypt", "certbot"],
            ]),
            verbs: vec![],
            example: "cortex do certbot.issue domain=example.com email=ops@example.com".into(),
            params: vec![
                p("domain", "domain to issue the certificate for", ParamKind::Ident),
                p("email", "contact email for the ACME account", ParamKind::Line),
            ],
            forward: "certbot certonly --standalone --non-interactive --agree-tos -m {email} -d {domain}".into(),
            verify_forward: "test -s /etc/letsencrypt/live/{domain}/fullchain.pem".into(),
            inverse: crate::FS_RESTORE.into(),
            verify_inverse: crate::FS_RESTORE.into(),
            host_side: false,
            drift_note: "undo restores /etc/letsencrypt exactly as it was before the issuance; \
                         it removes the local certificate files but does NOT revoke the \
                         certificate with the CA. Standalone mode needs port 80 free."
                .into(),
        },
        // ---- services ----------------------------------------------------
        Template {
            id: "service.start".into(),
            summary: "Start a systemd unit".into(),
            category: "services".into(),
            // English service requests route through the prior-state-aware
            // workflow (`start` on a running unit journals nothing); these
            // stay reachable by `cortex do` and by explicit plans.
            keywords: vec![],
            verbs: vec![],
            example: "cortex do service.start unit=nginx".into(),
            params: vec![p("unit", "systemd unit name", ParamKind::Ident)],
            forward: "systemctl start {unit}".into(),
            verify_forward: "systemctl is-active --quiet {unit}".into(),
            inverse: "systemctl stop {unit}".into(),
            verify_inverse: "! systemctl is-active --quiet {unit}".into(),
            host_side: true,
            drift_note: "unit state lives in systemd; undo stops the unit even if something \
                         else restarted it since"
                .into(),
        },
        Template {
            id: "service.stop".into(),
            summary: "Stop a systemd unit".into(),
            category: "services".into(),
            keywords: vec![],
            verbs: vec![],
            example: "cortex do service.stop unit=nginx".into(),
            params: vec![p("unit", "systemd unit name", ParamKind::Ident)],
            forward: "systemctl stop {unit}".into(),
            verify_forward: "! systemctl is-active --quiet {unit}".into(),
            inverse: "systemctl start {unit}".into(),
            verify_inverse: "systemctl is-active --quiet {unit}".into(),
            host_side: true,
            drift_note: "unit state lives in systemd; undo starts the unit again".into(),
        },
        Template {
            id: "service.enable".into(),
            summary: "Enable a systemd unit at boot".into(),
            category: "services".into(),
            keywords: vec![],
            verbs: vec![],
            example: "cortex do service.enable unit=nginx".into(),
            params: vec![p("unit", "systemd unit name", ParamKind::Ident)],
            forward: "systemctl enable {unit}".into(),
            verify_forward: "systemctl is-enabled --quiet {unit}".into(),
            inverse: "systemctl disable {unit}".into(),
            verify_inverse: "! systemctl is-enabled --quiet {unit}".into(),
            host_side: true,
            drift_note: "enablement is a symlink systemd owns; undo disables the unit".into(),
        },
        Template {
            id: "service.disable".into(),
            summary: "Disable a systemd unit at boot".into(),
            category: "services".into(),
            keywords: vec![],
            verbs: vec![],
            example: "cortex do service.disable unit=nginx".into(),
            params: vec![p("unit", "systemd unit name", ParamKind::Ident)],
            forward: "systemctl disable {unit}".into(),
            verify_forward: "! systemctl is-enabled --quiet {unit}".into(),
            inverse: "systemctl enable {unit}".into(),
            verify_inverse: "systemctl is-enabled --quiet {unit}".into(),
            host_side: true,
            drift_note: "enablement is a symlink systemd owns; undo re-enables the unit".into(),
        },
        Template {
            id: "service.create".into(),
            summary: "Create a systemd service from a command, enable and start it".into(),
            category: "services".into(),
            keywords: kw(&[&["service", "unit", "daemon"], &["create", "add", "make", "new", "define"]]),
            verbs: vec![],
            example: "cortex do service.create name=worker command=\"/usr/bin/worker --serve\""
                .into(),
            params: vec![
                p("name", "unit name (without .service)", ParamKind::Ident),
                p("command", "absolute command line for ExecStart", ParamKind::Line),
                pd("description", "unit description", ParamKind::Line, "Managed by cortex"),
            ],
            forward: "printf '[Unit]\\nDescription=%s\\n\\n[Service]\\nExecStart=%s\\nRestart=on-failure\\n\\n[Install]\\nWantedBy=multi-user.target\\n' {description} {command} > /etc/systemd/system/{name}.service && systemctl daemon-reload && systemctl enable --now {name}.service".into(),
            verify_forward: "systemctl is-active --quiet {name}.service".into(),
            inverse: "systemctl disable --now {name}.service && rm -f /etc/systemd/system/{name}.service && systemctl daemon-reload".into(),
            verify_inverse: "! systemctl is-active --quiet {name}.service && ! test -e /etc/systemd/system/{name}.service".into(),
            host_side: true,
            drift_note: "the unit file is owned by cortex; undo stops the service and removes \
                         the file — hand edits to it after commit are removed with it"
                .into(),
        },
        // ---- packages ------------------------------------------------------
        Template {
            id: "package.install".into(),
            summary: "Install an apt package".into(),
            category: "packages".into(),
            keywords: kw(&[&["install"]]),
            verbs: vs(&["package", "apt"]),
            example: "cortex try \"install htop\"".into(),
            params: vec![p("package", "apt package name", ParamKind::Ident)],
            forward: "DEBIAN_FRONTEND=noninteractive apt-get install -y {package}".into(),
            verify_forward:
                "dpkg-query -W -f='${{Status}}' {package} | grep -q '^install ok installed'"
                    .into(),
            // Filesystem-backed: the overlay captured the package's files, so
            // the journal's inverse layer is the real undo. This command
            // removes the dpkg registration that the inverse layer restores
            // separately.
            inverse: "DEBIAN_FRONTEND=noninteractive apt-get remove -y {package}".into(),
            verify_inverse:
                "! dpkg-query -W -f='${{Status}}' {package} 2>/dev/null | grep -q '^install ok installed'"
                    .into(),
            host_side: false,
            drift_note: "the install runs in an overlay sandbox first; undo restores every \
                         file the package touched from the journal, byte for byte"
                .into(),
        },
        Template {
            id: "package.remove".into(),
            summary: "Remove an apt package (undo restores its exact files)".into(),
            category: "packages".into(),
            keywords: kw(&[&["uninstall", "purge", "remove", "delete"]]),
            verbs: vs(&["package", "apt"]),
            example: "cortex try \"uninstall htop\"".into(),
            params: vec![p("package", "apt package name", ParamKind::Ident)],
            forward: "DEBIAN_FRONTEND=noninteractive apt-get remove -y {package}".into(),
            verify_forward:
                "! dpkg-query -W -f='${{Status}}' {package} 2>/dev/null | grep -q '^install ok installed'"
                    .into(),
            inverse: "DEBIAN_FRONTEND=noninteractive apt-get install -y {package}".into(),
            verify_inverse:
                "dpkg-query -W -f='${{Status}}' {package} | grep -q '^install ok installed'"
                    .into(),
            host_side: false,
            drift_note: "the removal runs in an overlay sandbox first; undo restores the \
                         removed files and dpkg records from the journal, not from the network"
                .into(),
        },
        Template {
            id: "package.install-dnf".into(),
            summary: "Install a package with dnf (Fedora/RHEL family)".into(),
            category: "packages".into(),
            keywords: kw(&[
                &["install"],
                &["dnf", "yum", "fedora", "rhel", "centos", "rocky", "alma"],
            ]),
            verbs: vs(&["package"]),
            example: "cortex try \"install htop with dnf\"".into(),
            params: vec![p("package", "dnf package name", ParamKind::Ident)],
            forward: "dnf install -y {package}".into(),
            verify_forward: "rpm -q {package}".into(),
            inverse: "dnf remove -y {package}".into(),
            verify_inverse: "! rpm -q {package}".into(),
            host_side: false,
            drift_note: "the install runs in an overlay sandbox first; undo restores every \
                         file the package touched from the journal, byte for byte"
                .into(),
        },
        Template {
            id: "package.remove-dnf".into(),
            summary: "Remove a dnf package (undo restores its exact files)".into(),
            category: "packages".into(),
            keywords: kw(&[
                &["uninstall", "purge", "remove", "delete"],
                &["dnf", "yum", "fedora", "rhel", "centos", "rocky", "alma"],
            ]),
            verbs: vs(&["package"]),
            example: "cortex try \"remove htop with dnf\"".into(),
            params: vec![p("package", "dnf package name", ParamKind::Ident)],
            forward: "dnf remove -y {package}".into(),
            verify_forward: "! rpm -q {package}".into(),
            inverse: "dnf install -y {package}".into(),
            verify_inverse: "rpm -q {package}".into(),
            host_side: false,
            drift_note: "the removal runs in an overlay sandbox first; undo restores the \
                         removed files and rpm records from the journal, not from the network"
                .into(),
        },
        // ---- users ---------------------------------------------------------
        Template {
            id: "user.add".into(),
            summary: "Create a system user with a home directory".into(),
            category: "users".into(),
            keywords: kw(&[&["user", "account"], &["add", "create", "new", "make"]]),
            verbs: vec![],
            example: "cortex try \"add user alice\"".into(),
            params: vec![
                p("username", "login name for the new user", ParamKind::User),
                pd("shell", "login shell", ParamKind::AbsPath, "/bin/bash"),
            ],
            forward: "useradd -m -s {shell} {username}".into(),
            verify_forward: "id {username}".into(),
            inverse: "userdel -r {username}".into(),
            verify_inverse: "! id {username}".into(),
            host_side: false,
            drift_note: "undo restores /etc/passwd, shadow and group from the journal and \
                         refuses if anyone else edited them since"
                .into(),
        },
        Template {
            id: "user.add-sudo".into(),
            summary: "Create a system user with a home directory and sudo access".into(),
            category: "users".into(),
            keywords: kw(&[
                &["user", "account"],
                &["add", "create", "new", "make"],
                &["sudo", "sudoer", "admin", "administrator"],
            ]),
            verbs: vec![],
            example: "cortex try \"create user deploy with sudo\"".into(),
            params: vec![
                p("username", "login name for the new user", ParamKind::User),
                pd("shell", "login shell", ParamKind::AbsPath, "/bin/bash"),
            ],
            forward: "useradd -m -s {shell} {username} && usermod -aG sudo {username}".into(),
            verify_forward: "id -nG {username} | grep -qw sudo".into(),
            inverse: "userdel -r {username}".into(),
            verify_inverse: "! id {username}".into(),
            host_side: false,
            drift_note: "undo restores /etc/passwd, shadow and group from the journal and \
                         refuses if anyone else edited them since"
                .into(),
        },
        Template {
            id: "user.grant-sudo".into(),
            summary: "Add an existing user to the sudo group".into(),
            category: "users".into(),
            keywords: kw(&[
                &["sudo", "sudoer"],
                &["grant", "give", "make", "promote", "allow", "add"],
            ]),
            verbs: vec![],
            example: "cortex try \"give alice sudo\"".into(),
            params: vec![p("username", "user to promote", ParamKind::User)],
            forward: "usermod -aG sudo {username}".into(),
            verify_forward: "id -nG {username} | grep -qw sudo".into(),
            inverse: "gpasswd -d {username} sudo".into(),
            verify_inverse: "! id -nG {username} | grep -qw sudo".into(),
            host_side: false,
            drift_note: "group membership is a line in /etc/group; undo removes exactly that \
                         membership"
                .into(),
        },
        Template {
            id: "user.remove".into(),
            summary: "Remove a user, their home directory and mail spool".into(),
            category: "users".into(),
            keywords: kw(&[&["user", "account"], &["remove", "delete", "del", "drop"]]),
            verbs: vec![],
            example: "cortex try \"remove user alice\"".into(),
            params: vec![p("username", "user to remove", ParamKind::User)],
            forward: "userdel -r {username}".into(),
            verify_forward: "! id {username}".into(),
            // The journal's inverse layer is the whole undo: it restores
            // passwd, shadow, group AND the home directory byte for byte —
            // no command can recreate a deleted password hash.
            inverse: crate::FS_RESTORE.into(),
            verify_inverse: crate::FS_RESTORE.into(),
            host_side: false,
            drift_note: "undo restores the user's passwd/shadow/group entries and home \
                         directory from the journal exactly as they were at commit"
                .into(),
        },
        Template {
            id: "user.ssh-key".into(),
            summary: "Authorize an SSH public key for a user".into(),
            category: "users".into(),
            keywords: kw(&[&["ssh"], &["key"]]),
            verbs: vs(&["add", "install", "authorize", "authorise", "put"]),
            example: "cortex do user.ssh-key username=alice key=\"ssh-ed25519 AAAA... alice@laptop\""
                .into(),
            params: vec![
                p("username", "user whose authorized_keys to extend", ParamKind::User),
                p("key", "the full public key line", ParamKind::Line),
            ],
            forward: "install -d -m 700 -o {username} -g {username} /home/{username}/.ssh && printf '%s\\n' {key} >> /home/{username}/.ssh/authorized_keys && chown {username}:{username} /home/{username}/.ssh/authorized_keys && chmod 600 /home/{username}/.ssh/authorized_keys".into(),
            verify_forward: "grep -qxF {key} /home/{username}/.ssh/authorized_keys".into(),
            inverse: crate::FS_RESTORE.into(),
            verify_inverse: crate::FS_RESTORE.into(),
            host_side: false,
            drift_note: "undo restores authorized_keys from the journal; keys added after \
                         commit would be restored away, and drift detection refuses that"
                .into(),
        },
        // ---- files -----------------------------------------------------------
        Template {
            id: "file.deploy".into(),
            summary: "Write a file with given content, mode and owner (backup automatic)".into(),
            category: "files".into(),
            keywords: kw(&[&["file", "config", "configuration"]]),
            verbs: vs(&["write", "deploy", "create", "put", "drop"]),
            example: "cortex do file.deploy path=/etc/motd content=\"welcome\" mode=0644".into(),
            params: vec![
                p("path", "absolute destination path", ParamKind::AbsPath),
                p("content", "file content (a trailing newline is added)", ParamKind::Text),
                pd("mode", "octal permissions", ParamKind::Mode, "0644"),
                pd("owner", "owning user", ParamKind::Ident, "root"),
            ],
            forward: "mkdir -p \"$(dirname {path})\" && printf '%s\\n' {content} > {path} && chmod {mode} {path} && chown {owner} {path}".into(),
            // Byte-for-byte: an exit status of 0 proves nothing about content.
            verify_forward: "printf '%s\\n' {content} | cmp -s - {path}".into(),
            // Correct in both cases: undo removes the deployed file, then the
            // journal restore puts back whatever was there before (or nothing).
            inverse: "rm -f {path}".into(),
            verify_inverse: "! test -e {path}".into(),
            host_side: false,
            drift_note: "the previous file (or its absence) is captured in the journal; undo \
                         refuses if the deployed file was edited after commit"
                .into(),
        },
        Template {
            id: "dir.create".into(),
            summary: "Create a directory with given mode and owner".into(),
            category: "files".into(),
            keywords: kw(&[&["directory", "folder", "dir"]]),
            verbs: vs(&["create", "make", "add", "mkdir", "ensure"]),
            example: "cortex try \"create directory /opt/app\"".into(),
            params: vec![
                p("path", "absolute directory path", ParamKind::AbsPath),
                pd("mode", "octal permissions", ParamKind::Mode, "0755"),
                pd("owner", "owning user", ParamKind::Ident, "root"),
            ],
            forward: "install -d -m {mode} -o {owner} {path}".into(),
            verify_forward: "test -d {path}".into(),
            // rmdir, not rm -rf: if anything landed inside since, undo must
            // refuse rather than take the contents with it.
            inverse: "rmdir {path}".into(),
            verify_inverse: "! test -d {path}".into(),
            host_side: false,
            drift_note: "undo uses rmdir and fails on purpose if files appeared inside the \
                         directory after commit — it will not delete work it did not create"
                .into(),
        },
        Template {
            id: "symlink.swap".into(),
            summary: "Repoint a symlink (blue/green)".into(),
            category: "files".into(),
            keywords: kw(&[&["symlink", "link"]]),
            verbs: vs(&["swap", "point", "repoint", "switch", "flip"]),
            example: "cortex do symlink.swap link=/srv/current target=/srv/v2 previous=/srv/v1"
                .into(),
            params: vec![
                p("link", "the symlink to repoint", ParamKind::AbsPath),
                p("target", "where it should point now", ParamKind::AbsPath),
                p("previous", "where it pointed before (the undo target)", ParamKind::AbsPath),
            ],
            forward: "test -e {target} && ln -sfn {target} {link}".into(),
            verify_forward: "[ \"$(readlink {link})\" = {target} ]".into(),
            // Explicit rather than relying on the overlay restore: the inverse
            // is a real command with a real post-condition, so `cortex verify
            // --self` can exercise it end to end like any other template.
            inverse: "ln -sfn {previous} {link}".into(),
            verify_inverse: "[ \"$(readlink {link})\" = {previous} ]".into(),
            host_side: true,
            drift_note: "undo repoints the link at `previous` regardless of what repointed it \
                         in between"
                .into(),
        },
        // ---- network -----------------------------------------------------------
        Template {
            id: "firewall.allow".into(),
            summary: "Allow a port through ufw".into(),
            category: "network".into(),
            keywords: kw(&[
                &["port", "firewall", "ufw"],
                &["allow", "open", "permit", "unblock", "expose"],
            ]),
            verbs: vec![],
            example: "cortex try \"open port 8080\"".into(),
            params: vec![
                p("port", "port to allow", ParamKind::Port),
                pd("proto", "protocol", ParamKind::Ident, "tcp"),
            ],
            forward: "ufw allow {port}/{proto}".into(),
            verify_forward: "ufw status | grep -w {port}/{proto} | grep -q ALLOW".into(),
            inverse: "ufw delete allow {port}/{proto}".into(),
            verify_inverse: "! ufw status | grep -w {port}/{proto} | grep -q ALLOW".into(),
            host_side: true,
            drift_note: "`ufw allow` is idempotent: if an identical rule already existed \
                         before this run, undo deletes that rule too"
                .into(),
        },
        Template {
            id: "firewall.remove".into(),
            summary: "Remove a ufw allow rule".into(),
            category: "network".into(),
            keywords: kw(&[
                &["port", "firewall", "ufw"],
                &["close", "deny", "block", "remove", "delete", "revoke"],
            ]),
            verbs: vec![],
            example: "cortex try \"close port 8080\"".into(),
            params: vec![
                p("port", "port whose allow rule to remove", ParamKind::Port),
                pd("proto", "protocol", ParamKind::Ident, "tcp"),
            ],
            forward: "ufw delete allow {port}/{proto}".into(),
            verify_forward: "! ufw status | grep -w {port}/{proto} | grep -q ALLOW".into(),
            inverse: "ufw allow {port}/{proto}".into(),
            verify_inverse: "ufw status | grep -w {port}/{proto} | grep -q ALLOW".into(),
            host_side: true,
            drift_note: "undo re-adds the allow rule exactly as it was".into(),
        },
        Template {
            id: "hosts.add".into(),
            summary: "Add an /etc/hosts entry (undo restores the exact file)".into(),
            category: "network".into(),
            keywords: kw(&[
                &["hosts", "host"],
                &["entry", "record", "alias", "mapping", "add", "map", "point"],
            ]),
            verbs: vec![],
            example: "cortex do hosts.add ip=10.0.0.5 hostname=db.internal".into(),
            params: vec![
                p("ip", "IPv4 address", ParamKind::Ident),
                p("hostname", "name to map to it", ParamKind::Ident),
            ],
            forward: "printf '%s\\t%s\\n' {ip} {hostname} >> /etc/hosts".into(),
            verify_forward: "grep -qw {hostname} /etc/hosts".into(),
            inverse: crate::FS_RESTORE.into(),
            verify_inverse: crate::FS_RESTORE.into(),
            host_side: false,
            drift_note: "undo restores /etc/hosts from the journal byte for byte, and refuses \
                         if anything else edited the file since commit"
                .into(),
        },
        // ---- deploy --------------------------------------------------------
        Template {
            id: "git.clone".into(),
            summary: "Clone a git repository to a directory (undo removes the tree)".into(),
            category: "deploy".into(),
            keywords: kw(&[&["git", "repo", "repository", "clone"]]),
            verbs: vs(&["clone", "deploy", "checkout", "pull", "get", "fetch"]),
            example: "cortex do git.clone repo=https://github.com/user/app.git path=/srv/app"
                .into(),
            params: vec![
                p("repo", "repository URL (https or ssh)", ParamKind::Line),
                p("path", "absolute directory to clone into", ParamKind::AbsPath),
            ],
            forward: "git clone {repo} {path}".into(),
            verify_forward: "test -d {path}/.git".into(),
            inverse: crate::FS_RESTORE.into(),
            verify_inverse: crate::FS_RESTORE.into(),
            host_side: false,
            drift_note: "the clone runs in an overlay sandbox first; undo removes exactly the \
                         tree the clone created, and refuses if files inside changed since \
                         commit"
                .into(),
        },
        // ---- backup --------------------------------------------------------
        Template {
            id: "backup.dir".into(),
            summary: "Archive a directory to a .tar.gz (undo removes the archive)".into(),
            category: "backup".into(),
            keywords: kw(&[&["backup", "archive", "tarball", "snapshot"]]),
            verbs: vs(&["back", "take", "create", "make", "save"]),
            example: "cortex do backup.dir src=/etc dest=/var/backups/etc.tar.gz".into(),
            params: vec![
                p("src", "directory to archive", ParamKind::AbsPath),
                p("dest", "absolute path of the archive to write", ParamKind::AbsPath),
            ],
            forward: "test -d {src} && ! test -e {dest} && tar -czf {dest} -C {src} .".into(),
            verify_forward: "tar -tzf {dest} >/dev/null".into(),
            inverse: "rm -f {dest}".into(),
            verify_inverse: "! test -e {dest}".into(),
            host_side: true,
            drift_note: "the forward refuses to overwrite an existing archive; undo deletes \
                         only the archive this run created — the source directory is never \
                         touched"
                .into(),
        },
        // ---- tuning --------------------------------------------------------
        Template {
            id: "sysctl.set".into(),
            summary: "Set a kernel parameter, runtime and persisted".into(),
            category: "tuning".into(),
            keywords: kw(&[&["sysctl", "kernel", "swappiness"]]),
            verbs: vs(&["set", "tune", "apply", "configure"]),
            example: "cortex do sysctl.set key=vm.swappiness value=10 previous=60".into(),
            params: vec![
                p("key", "sysctl key, e.g. vm.swappiness", ParamKind::Ident),
                p("value", "value to set", ParamKind::Line),
                p("previous", "current value (the undo target — check with sysctl -n)", ParamKind::Line),
            ],
            forward: "mkdir -p /etc/sysctl.d && printf '%s = %s\\n' {key} {value} > /etc/sysctl.d/99-cortex-{key}.conf && sysctl -w {key}={value}".into(),
            verify_forward: "[ \"$(sysctl -n {key})\" = {value} ]".into(),
            inverse: "rm -f /etc/sysctl.d/99-cortex-{key}.conf && sysctl -w {key}={previous}".into(),
            verify_inverse: "[ \"$(sysctl -n {key})\" = {previous} ]".into(),
            host_side: true,
            drift_note: "like symlink.swap, the undo target is declared upfront: undo removes \
                         the persisted file and sets the runtime value back to `previous`, \
                         regardless of what changed it in between"
                .into(),
        },
        Template {
            id: "swap.create".into(),
            summary: "Create and enable a swap file".into(),
            category: "tuning".into(),
            keywords: kw(&[&["swap", "swapfile"]]),
            verbs: vs(&["create", "add", "make", "enable", "setup", "set"]),
            example: "cortex do swap.create size=2G".into(),
            params: vec![
                p("size", "swap size, e.g. 2G or 512M", ParamKind::Ident),
                pd("path", "where the swap file lives", ParamKind::AbsPath, "/swapfile"),
            ],
            forward: "! test -e {path} && fallocate -l {size} {path} && chmod 600 {path} && mkswap {path} && swapon {path}".into(),
            verify_forward: "swapon --show=NAME --noheadings | grep -qx {path}".into(),
            inverse: "swapoff {path} && rm -f {path}".into(),
            verify_inverse: "! swapon --show=NAME --noheadings | grep -qx {path} && ! test -e {path}".into(),
            host_side: true,
            drift_note: "the forward refuses if the path already exists; undo disables and \
                         deletes exactly the file it created. /etc/fstab is never edited — \
                         the swap does not survive a reboot unless you add it there yourself"
                .into(),
        },
        // ---- ssh -----------------------------------------------------------
        Template {
            id: "sshd.set".into(),
            summary: "Set an sshd option via a drop-in, validate, reload".into(),
            category: "ssh".into(),
            keywords: kw(&[
                &["ssh", "sshd"],
                &["config", "option", "harden", "password", "permitrootlogin", "login"],
            ]),
            verbs: vs(&["set", "disable", "enable", "configure", "harden"]),
            example: "cortex do sshd.set option=PasswordAuthentication value=no".into(),
            params: vec![
                p("option", "sshd_config option name", ParamKind::Ident),
                p("value", "value to set it to", ParamKind::Line),
            ],
            forward: "mkdir -p /etc/ssh/sshd_config.d && printf '%s %s\\n' {option} {value} > /etc/ssh/sshd_config.d/99-cortex-{option}.conf && sshd -t && (systemctl reload-or-restart sshd 2>/dev/null || systemctl reload-or-restart ssh)".into(),
            verify_forward: "sshd -T 2>/dev/null | grep -qi \"^\"{option}\" \"{value}\"$\"".into(),
            inverse: "rm -f /etc/ssh/sshd_config.d/99-cortex-{option}.conf && (systemctl reload-or-restart sshd 2>/dev/null || systemctl reload-or-restart ssh)".into(),
            verify_inverse: "! test -e /etc/ssh/sshd_config.d/99-cortex-{option}.conf && sshd -t".into(),
            host_side: true,
            drift_note: "the drop-in is one file cortex owns; `sshd -t` validates the whole \
                         config before the running daemon ever sees it. Undo removes the \
                         drop-in and reloads — whatever sshd_config said before applies again. \
                         Requires an sshd_config that includes sshd_config.d (the modern \
                         default)."
                .into(),
        },
    ]
}
