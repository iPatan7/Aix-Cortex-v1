<h1 align="center">cortex</h1>

<p align="center">
  <b>Run any change to a Linux box transactionally.<br>
  Verify it works. Undo it perfectly, with proof.</b>
</p>

<p align="center">
  <code>curl -sSL https://raw.githubusercontent.com/iPatan7/Aix-Cortex-v1/main/scripts/install.sh | sh</code>
</p>

## Quickstart

```sh
curl -sSL https://raw.githubusercontent.com/iPatan7/Aix-Cortex-v1/main/scripts/install.sh | sh

cortex demo                                # see the guarantee in ~2s (no root)
cortex run nginx on port 8080 --plan       # see exactly what would run (no root)
sudo cortex run nginx on port 8080         # plan → sandbox → verify → commit
sudo cortex deploy myapp image=nginx ports=8080:80
sudo cortex try "install htop and open port 8080"   # two steps, in order
sudo cortex status                         # what's applied, what's undoable
sudo cortex undo cortex-nginx              # reverse it by name, with proof
```

No flags, no setup, no scratch directories to create — cortex uses sensible
defaults (`/var/lib/cortex`) and creates them on first run.

---

Every tool will happily make a change. Almost none can take it back.
`terraform plan` predicts but does not execute. `ansible --check` simulates.
NixOS gives atomic rollback if you first rewrite your world in Nix. Docker
isolates but never merges back.

Cortex runs the change **for real, in a sandbox**, proves it worked, and only
then merges it into your filesystem — recording an inverse *and a
post-condition that proves the inverse worked* before it commits.

```console
$ cortex demo
▸ cortex demo
  committing a real change, then proving undo is safe — no root, no docker

  ✔ commit: change listen 80 → 8080   entry 20260710T124147
▸ someone else edits the file
    their change: listen 8443 # hotfix
  ✔ undo (safe): should REFUSE to clobber the hotfix   refused; the hotfix is safe
    file still: listen 8443 # hotfix
▸ resolve the drift, then undo
  ✔ undo: restore the pre-change config
    restored: listen 80

✔ that is the whole product: a change you can take back, with proof.
```

`cortex demo` needs no root, no docker, no network. It runs in about two
seconds and shows the whole differentiator.

## The guarantee, and why you can check it yourself

Most "rollback" is a promise. This is a property you can verify:

```console
$ cortex verify --self
▸ reversibility conformance
  for each template: run it, prove it worked, undo it, prove it undid

  ✔ docker.run
  ✔ docker.app
  ✔ docker.volume.create
  ✔ docker.network.create
  ✔ file.deploy
  ✔ dir.create
  ✔ symlink.swap
  ✔ backup.dir
  – package.install (would install/remove a package; covered in CI)
  …
✔ 8 template(s) proved reversible on this machine
```

Templates that would mutate the host behind your back (packages, users,
services, firewall) are skipped locally with the reason printed — a suite
that hides its own coverage is the same lie as an undo that hides its own
failure — and run in CI inside a container.

Three properties hold, each because it was once false and reproduced on a real
machine:

**1. An inverse must prove itself.** A compensation that exits `0` proves
nothing — `echo done` exits `0`. Every reversible operation ships a
post-condition that must hold *after* the inverse runs. If it doesn't, cortex
says so and leaves the entry pending rather than claiming success.

**2. Undo refuses to destroy someone else's work.** Each commit fingerprints
what it left behind (content hash, mode, owner, link target). If anyone touched
those paths since, undo stops. `--force` proceeds, but rescues the current
contents first — a destructive op that keeps no copy of what it destroyed is
not a feature.

**3. Nothing invents an inverse.** Reversible operations come from a registry
of human-written `(forward, inverse, verify)` triples. The planner *selects*
a template and fills parameters. It never authors an inverse, because an
inverse is a semantic claim no compiler can check. Anything outside the
registry is `Irreversible`, runs only with explicit consent, and undo refuses
it out loud.

## The planner: plain English, zero AI

`cortex try` (the verb is optional) turns approximate, human phrasing into a
plan — **offline, deterministically, with no model and no network**. It is
keyword matching over the template registry, with typed parameter extraction
("port 8080", `key=value`, absolute paths) and bounded typo tolerance
(`instal htop`, `ngnix` — one edit, never two, so `reinstall` can never match
`uninstall`).

```console
$ cortex run nginx on port 8080 --plan
  · understood: nginx.serve name=site port=8080 root=/var/www/html
▸ plan
  template     nginx.serve — Serve a directory over nginx on a chosen port
  run          mkdir -p /etc/nginx/conf.d && printf 'server {…listen 8080…}' > …
  prove        ss -ltn | grep -q ':8080 '
  undo         rm -f /etc/nginx/conf.d/cortex-site.conf && systemctl reload-or-restart nginx
  prove undo   ! ss -ltn | grep -q ':8080 '
  drift        undo removes the file cortex owns; hand edits to it go with it
```

Every run shows this plan stage before anything executes; `--plan` /
`--dry-run` stops there. A recognised request with missing parameters teaches
the exact command instead of failing:

```console
$ cortex try "serve nginx"
▸ understood: nginx.serve
  ? port    TCP port nginx should listen on (port number)
  run it with: cortex do nginx.serve port=<port>
```

And a miss suggests the nearest templates — never a shrug, never a model.
The same sentence always produces the same plan; every matching rule is a
line of Rust you can read. An LLM is consulted **only** if you explicitly set
`CORTEX_LLM_ENDPOINT`, only when the offline planner found nothing, and its
plan goes through the same render → authorize → execute path.

Conjunctions **compose**: `cortex try "install htop and open port 8080"`
plans both steps (numbered, shown in full before anything runs), executes
them in order, journals each separately, and `cortex undo --all` unwinds
them newest-first. If any segment is ambiguous, the whole sentence is read
as one request — composition can never change what a single-intent sentence
means.

**34 built-in templates** cover the daily surface: docker / podman /
compose, containers with env vars and volumes, docker volumes and networks,
nginx sites (plain and TLS), certbot, systemd units
(start/stop/enable/create), apt **and dnf** install/remove, users (create,
sudo, SSH keys, remove), git deploys, directory backups, sysctl and swap
tuning, sshd hardening, /etc/hosts entries, ufw rules, file and directory
deployment, symlink swaps. `cortex templates` lists them, `cortex templates
search <words>` finds the one you mean, and `cortex templates show <id>`
prints any template's full contract including its undo and drift behaviour.

**Add your own** in `~/.cortex/templates/*.toml` — same format, same
well-formedness gate (an inverse without a post-condition is refused), same
policy engine, and they match through their own keywords. When cortex runs as
root, template files must be root-owned, for the same reason the policy file
must be. See [docs/templates.md](docs/templates.md).

## Ten everyday tasks

```sh
sudo cortex run nginx on port 8080                      # serve a directory
sudo cortex do nginx.tls cert=/etc/ssl/c.pem key=/etc/ssl/k.pem   # with TLS
sudo cortex deploy myapp image=nginx ports=8080:80 \
     env=NODE_ENV=production volume=/srv/data:/data     # container + volume
sudo cortex try "install htop"                          # apt (or "with dnf")
sudo cortex try "create user deploy with sudo"          # users
sudo cortex do user.ssh-key username=deploy key="ssh-ed25519 AAAA… deploy" # ssh
sudo cortex try "open port 8080"                        # firewall
sudo cortex do backup.dir src=/etc dest=/var/backups/etc.tar.gz   # backup
sudo cortex do sysctl.set key=vm.swappiness value=10 previous=60  # tuning
sudo cortex do sshd.set option=PasswordAuthentication value=no    # hardening
```

Each one: full plan first, sandbox or verified compensation, proof before
commit, and a real undo — `cortex undo`, `cortex undo last`,
`cortex undo <journal id>`, or by name: `cortex undo cortex-nginx`,
`cortex undo nginx.serve`.

## Commands

| | |
|---|---|
| `cortex <what you want>` | Plan it, sandbox it, verify, commit (`try` optional; "X and Y" composes) |
| `cortex … --plan` | Show the full plan — commands, undo, proofs — and stop |
| `cortex do <template> k=v …` | Run a known-good template with exact parameters |
| `cortex templates` | Every reversible operation, by category |
| `cortex templates show <id>` | One template's full contract, undo included |
| `cortex templates search <words>` | Find the template for a job |
| `cortex status` | What's applied, what's undoable, what's blocked |
| `cortex undo [id\|last\|name]` | Reverse it, with proof (`--all`, `--force`) |
| `cortex receipt [id]` | Signed summary of one transaction |
| `cortex demo` | Prove the guarantee in ~2s, no root or docker |
| `cortex verify --self` | Prove every template's undo, on this machine |

## Speed

Planning is local string matching — there is nothing to wait for:

```console
$ time cortex do docker.run name=web image=nginx ports=8080:80
real  0m0.3s          # includes starting the container
```

The planner works identically on a box with no network — usually the box you
are trying to fix. `undo` never depends on anything remote at all: it's what
you reach for when things have gone wrong.

## Authorization

cortex runs as root, so its own gate is the security boundary. Every mutating
operation is evaluated **deny-by-default** against a root-owned
`/etc/cortex/policy.toml` before it runs. Rules match templates (down to
argument globs), workflows, and the irreversible escape hatch.

```toml
# only images from your registry, and only reversibly
[[rules]]
op = "template:docker.run"
decision = "allow"
[rules.args]
image = "ghcr.io/your-org/*"

[[rules]]
op = "*"
decision = "deny"
```

cortex refuses to load a policy file that is not root-owned — rules its subject
can rewrite are not rules. The natural-language paths authorize the *resolved*
operation, so a language model cannot reach the kernel through a rule it never
matched.

## What it can't do

Stated plainly, because a safety tool that oversells is worse than none:

- **You cannot undo the outside world.** An email sent, an S3 object deleted, a
  payment charged. Cortex compensates; it does not reverse. Those are
  `Irreversible` and require consent.
- **You cannot undo time.** If nginx served 500s for 40 seconds before you
  rolled back, those requests are gone. Cortex reduces MTTR, not impact.
- **No database migrations.** A migration's undo is inherently lossy (`DROP
  COLUMN` does not restore the data), so cortex ships *no* migration workflow
  rather than one that lies. A future snapshot-backed template could be honest;
  it is not built.
- **Linux only, needs privileges.** OverlayFS and mount namespaces.

## Architecture

```
crates/core       transactional engine: OverlayFS, journal, drift, undo
crates/registry   verified (forward, inverse, verify) templates + user loader
crates/planner    offline deterministic English → plan matcher
crates/policy     deny-by-default authorization gate
crates/sandbox    landlock confinement (defense in depth)
crates/cli        try / do / status / undo / receipt / demo / verify / templates
```

Six crates, ~10k lines, 150+ tests. The engine is testable in isolation; the
planner never touches the engine (it only emits plans); orchestration (which
template, which policy) lives in the CLI. The guarantee itself is documented
in [docs/safety.md](docs/safety.md), template authoring in
[docs/templates.md](docs/templates.md), and contributions in
[CONTRIBUTING.md](CONTRIBUTING.md).

**Defense in depth.** A command inside a transaction is already isolated by the
overlay and a private mount namespace. On top of that, `run_in_root` enforces a
Landlock allowlist right before it execs the command: the `/proc`, `/sys`,
`/dev`, `/run` trees bound in from the host for functionality are read-only, so
even a command running as root inside the sandbox cannot reach out and write to
the host through a bind mount. On a kernel without Landlock the namespace
isolation still holds and confinement is skipped rather than failing the run.

Plugin, daemon, and eBPF are documented seams, wired when a user needs them
(see [docs/roadmap.md](docs/roadmap.md)).

## Building

```sh
cargo build --release                                    # dynamic, dev
cargo build --release --target x86_64-unknown-linux-musl # static, shipping
cargo test  --workspace                                  # unit + guarantee
sudo -E cargo test -p cortex-core                        # + overlay as root
cortex verify --self                                     # + real daemons
```

The static musl binary is 2.4 MB and depends on nothing.

## License

Apache-2.0 (open core). See [docs/monetization.md](docs/monetization.md) for the enterprise tier.

## Available Templates

### `docker.run`
Run a container detached, published on a host port

**Example:**
```bash
cortex try "run nginx in docker on port 8080"
```
**Parameters:** `name` (required), `image` (required), `ports` (required)

### `podman.run`
Run a podman container detached, published on a host port

**Example:**
```bash
cortex try "run nginx in podman on port 8080"
```
**Parameters:** `name` (required), `image` (required), `ports` (required)

### `docker.compose.up`
Bring up a compose project

**Example:**
```bash
cortex do docker.compose.up project=app file=/srv/app/docker-compose.yml
```
**Parameters:** `project` (required), `file` (required)

### `docker.app`
Run a container with an env var and a persistent volume

**Example:**
```bash
cortex do docker.app name=app image=myapp ports=8080:80 env=NODE_ENV=production volume=/srv/data:/data
```
**Parameters:** `name` (required), `image` (required), `ports` (required), `env` (required), `volume` (required)

### `docker.volume.create`
Create a named docker volume

**Example:**
```bash
cortex do docker.volume.create name=appdata
```
**Parameters:** `name` (required)

### `docker.network.create`
Create a user-defined docker network

**Example:**
```bash
cortex do docker.network.create name=appnet
```
**Parameters:** `name` (required)

### `nginx.tls`
Serve a directory over nginx with TLS on a chosen port

**Example:**
```bash
cortex do nginx.tls cert=/etc/ssl/certs/site.pem key=/etc/ssl/private/site.key
```
**Parameters:** `cert` (required), `key` (required), `port=443`, `root=/var/www/html`, `name=tls`

### `nginx.serve`
Serve a directory over nginx on a chosen port

**Example:**
```bash
cortex try "run nginx on port 8080"
```
**Parameters:** `port` (required), `root=/var/www/html`, `name=site`

### `certbot.issue`
Obtain a Let's Encrypt certificate with certbot (standalone)

**Example:**
```bash
cortex do certbot.issue domain=example.com email=ops@example.com
```
**Parameters:** `domain` (required), `email` (required)

### `service.start`
Start a systemd unit

**Example:**
```bash
cortex do service.start unit=nginx
```
**Parameters:** `unit` (required)

### `service.stop`
Stop a systemd unit

**Example:**
```bash
cortex do service.stop unit=nginx
```
**Parameters:** `unit` (required)

### `service.enable`
Enable a systemd unit at boot

**Example:**
```bash
cortex do service.enable unit=nginx
```
**Parameters:** `unit` (required)

### `service.disable`
Disable a systemd unit at boot

**Example:**
```bash
cortex do service.disable unit=nginx
```
**Parameters:** `unit` (required)

### `service.create`
Create a systemd service from a command, enable and start it

**Example:**
```bash
cortex do service.create name=worker command="/usr/bin/worker --serve"
```
**Parameters:** `name` (required), `command` (required), `description=Managed by cortex`

### `package.install`
Install an apt package

**Example:**
```bash
cortex try "install htop"
```
**Parameters:** `package` (required)

### `package.remove`
Remove an apt package (undo restores its exact files)

**Example:**
```bash
cortex try "uninstall htop"
```
**Parameters:** `package` (required)

### `package.install-dnf`
Install a package with dnf (Fedora/RHEL family)

**Example:**
```bash
cortex try "install htop with dnf"
```
**Parameters:** `package` (required)

### `package.remove-dnf`
Remove a dnf package (undo restores its exact files)

**Example:**
```bash
cortex try "remove htop with dnf"
```
**Parameters:** `package` (required)

### `user.add`
Create a system user with a home directory

**Example:**
```bash
cortex try "add user alice"
```
**Parameters:** `username` (required), `shell=/bin/bash`

### `user.add-sudo`
Create a system user with a home directory and sudo access

**Example:**
```bash
cortex try "create user deploy with sudo"
```
**Parameters:** `username` (required), `shell=/bin/bash`

### `user.grant-sudo`
Add an existing user to the sudo group

**Example:**
```bash
cortex try "give alice sudo"
```
**Parameters:** `username` (required)

### `user.remove`
Remove a user, their home directory and mail spool

**Example:**
```bash
cortex try "remove user alice"
```
**Parameters:** `username` (required)

### `user.ssh-key`
Authorize an SSH public key for a user

**Example:**
```bash
cortex do user.ssh-key username=alice key="ssh-ed25519 AAAA... alice@laptop"
```
**Parameters:** `username` (required), `key` (required)

### `file.deploy`
Write a file with given content, mode and owner (backup automatic)

**Example:**
```bash
cortex do file.deploy path=/etc/motd content="welcome" mode=0644
```
**Parameters:** `path` (required), `content` (required), `mode=0644`, `owner=root`

### `dir.create`
Create a directory with given mode and owner

**Example:**
```bash
cortex try "create directory /opt/app"
```
**Parameters:** `path` (required), `mode=0755`, `owner=root`

### `symlink.swap`
Repoint a symlink (blue/green)

**Example:**
```bash
cortex do symlink.swap link=/srv/current target=/srv/v2 previous=/srv/v1
```
**Parameters:** `link` (required), `target` (required), `previous` (required)

### `firewall.allow`
Allow a port through ufw

**Example:**
```bash
cortex try "open port 8080"
```
**Parameters:** `port` (required), `proto=tcp`

### `firewall.remove`
Remove a ufw allow rule

**Example:**
```bash
cortex try "close port 8080"
```
**Parameters:** `port` (required), `proto=tcp`

### `hosts.add`
Add an /etc/hosts entry (undo restores the exact file)

**Example:**
```bash
cortex do hosts.add ip=10.0.0.5 hostname=db.internal
```
**Parameters:** `ip` (required), `hostname` (required)

### `git.clone`
Clone a git repository to a directory (undo removes the tree)

**Example:**
```bash
cortex do git.clone repo=https://github.com/user/app.git path=/srv/app
```
**Parameters:** `repo` (required), `path` (required)

### `backup.dir`
Archive a directory to a .tar.gz (undo removes the archive)

**Example:**
```bash
cortex do backup.dir src=/etc dest=/var/backups/etc.tar.gz
```
**Parameters:** `src` (required), `dest` (required)

### `sysctl.set`
Set a kernel parameter, runtime and persisted

**Example:**
```bash
cortex do sysctl.set key=vm.swappiness value=10 previous=60
```
**Parameters:** `key` (required), `value` (required), `previous` (required)

### `swap.create`
Create and enable a swap file

**Example:**
```bash
cortex do swap.create size=2G
```
**Parameters:** `size` (required), `path=/swapfile`

### `sshd.set`
Set an sshd option via a drop-in, validate, reload

**Example:**
```bash
cortex do sshd.set option=PasswordAuthentication value=no
```
**Parameters:** `option` (required), `value` (required)

