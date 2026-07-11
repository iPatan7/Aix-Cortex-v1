# Changelog

## v0.3.1 — 2026-07-11

- **Fix:** Fixed a race condition where verifying a service start too quickly after a `systemctl reload` could incorrectly fail before the daemon finished initializing. Verification now intelligently waits up to 2 seconds for asynchronous operations to settle.

## v0.3.0 — 2026-07-11

The "finish everything" release: 34 built-in templates, composition,
undo-by-name, template search, full docs, man page.

### Templates (21 → 34)

- **Containers:** `docker.app` (env var + persistent volume),
  `docker.volume.create`, `docker.network.create`.
- **Web:** `nginx.tls` (TLS site with cert/key validation before reload),
  `certbot.issue` (standalone; undo removes local files, stated plainly
  that it does not revoke).
- **Packages:** `package.install-dnf`, `package.remove-dnf` — say
  "install htop with dnf" on Fedora/RHEL boxes.
- **Deploy:** `git.clone` (journal-backed undo removes exactly the cloned
  tree).
- **Backup:** `backup.dir` (tar.gz; forward refuses to overwrite, undo
  deletes only the archive it created).
- **Tuning:** `sysctl.set` (runtime + persisted, undo target declared
  upfront like symlink.swap), `swap.create` (refuses existing paths, undo
  swapoffs and removes exactly its file).
- **SSH:** `sshd.set` (drop-in config, `sshd -t` validates before the
  daemon reloads, in both directions).
- **Network:** `hosts.add` (undo restores /etc/hosts byte for byte).
- New parameter kinds `volume-mapping` and `env-var`, validated and
  available to user TOML templates.

### Planner

- **Composition:** "install htop and open port 8080" splits on
  conjunctions; if every segment plans, the steps run in order, each
  journaled separately, `undo --all` unwinding newest-first. Any ambiguous
  segment falls back to whole-sentence reading — composition can never
  change what a single-intent sentence means.
- A strictly better-scoring template that only lacks parameters now
  outranks a weaker template that happens to bind: "serve nginx over
  https" teaches nginx.tls's cert/key instead of silently planning plain
  HTTP.
- Explicit `key=value` tokens count as keyword hits for their parameter
  name; `deploy ... env=... volume=...` routes to `docker.app`.
- New extractors: repository URLs, storage sizes ("2G"), and pronouns are
  no longer mistaken for systemd unit names ("start it" asks which unit).

### CLI

- `cortex undo [id|last|name]` — undo by journal id, template id
  (`cortex undo nginx.serve`), service or container name
  (`cortex undo cortex-nginx`).
- `cortex templates search <words>` — approximate search over the catalog
  with copy-pasteable examples.
- `cortex version` prints the one-line update command.
- Help text covers the full surface; man page ships in the release tarball
  and installs with the installer.

### Verification

- 150+ tests (was 111), all green, clippy clean with `-D warnings`.
- `cortex verify --self` now exercises `docker.app`,
  `docker.volume.create`, `docker.network.create` and `backup.dir` (8
  templates proved reversible on a typical docker-equipped machine), with
  stated reasons for everything skipped.
- CI end-to-end now proves undo-by-name against real docker, and a
  composite try → `undo --all` round-trip (directory + hosts entry, both
  reversed).

### Docs

- `docs/safety.md` — the guarantee, drift, and what cortex refuses to
  promise.
- `docs/templates.md` — composition, search, new kinds, matching rules.
- `CONTRIBUTING.md`, `CHANGELOG.md`, example user templates in
  `examples/templates/`, man page `docs/cortex.1`, launch assets in
  `docs/launch.md`.

## v0.2.1 — 2026-07-10

- Quoted tasks run without the `try` verb.
- Offline planner stops silently dropping a named port.
- Undo that needs root is refused upfront with the exact fix.
- Installer places cortex where sudo can find it.

## v0.2.0 — 2026-07-10

- Dedicated offline deterministic planner (`cortex-planner`).
- 21 built-in templates; user templates in `~/.cortex/templates/*.toml`
  with root-trust gates.
- Deny-by-default policy engine; release workflow for x86_64 + aarch64
  static musl binaries.
