<h1 align="center">cortex</h1>

<p align="center">
  <b>Run any change to a Linux box transactionally.<br>
  Verify it works. Undo it perfectly, with proof.</b>
</p>

<p align="center">
  <code>curl -sSL https://get.cortex.dev | sh</code>
</p>

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
  ✔ docker.run          run it, prove it ran, undo it, prove it undid
  ✔ symlink.swap
✔ 2 template(s) proved reversible on this machine
```

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
of human-written `(forward, inverse, verify)` triples. The planner — LLM or
regex — *selects* a template and fills parameters. It never authors an inverse,
because an inverse is a semantic claim no compiler can check. Anything outside
the registry is `Irreversible`, runs only with explicit consent, and undo
refuses it out loud.

## Commands

| | |
|---|---|
| `cortex try "<what you want>"` | Plan it, sandbox it, verify, commit |
| `cortex do <template> k=v …` | Run a known-good template — no LLM, no network |
| `cortex status` | What's applied, what's undoable, what's blocked |
| `cortex undo [id]` | Reverse it, with proof (`--all`, `--force`) |
| `cortex receipt [id]` | Signed summary of one transaction |
| `cortex demo` | Prove the guarantee in ~2s, no root or docker |
| `cortex verify --self` | Prove every template's undo, on this machine |

## Speed

`cortex try` matches common intents **locally**, with no model call:

```console
$ time cortex do docker.run name=web image=nginx ports=8080:80
real  0m0.3s          # includes starting the container
```

An LLM is consulted only for genuinely novel requests. That keeps the hero
command fast and keeps cortex working on the box with no network — usually the
box you are trying to fix. `undo` never calls a model at all: it's what you
reach for when things have gone wrong, and the model may be what's broken.

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
crates/registry   verified (forward, inverse, verify) templates
crates/policy     deny-by-default authorization gate
crates/sandbox    landlock confinement (defense in depth)
crates/cli        try / status / undo / receipt / demo / verify
```

Five crates, ~6k lines. The engine is testable in isolation; orchestration
(which template, which policy) lives in the CLI.

**Defense in depth.** A command inside a transaction is already isolated by the
overlay and a private mount namespace. On top of that, `run_in_root` enforces a
Landlock allowlist right before it execs the command: the `/proc`, `/sys`,
`/dev`, `/run` trees bound in from the host for functionality are read-only, so
even a command running as root inside the sandbox cannot reach out and write to
the host through a bind mount. On a kernel without Landlock the namespace
isolation still holds and confinement is skipped rather than failing the run.

Plugin, daemon, and eBPF are documented seams, wired when a user needs them
(see [docs/roadmap.md]).

## Building

```sh
cargo build --release                                    # dynamic, dev
cargo build --release --target x86_64-unknown-linux-musl # static, shipping
cargo test  --workspace                                  # unit + guarantee
sudo -E cargo test -p cortex-core                        # + overlay as root
cortex verify --self                                     # + real daemons
```

The static musl binary is 2.2 MB and depends on nothing.

## License

Apache-2.0 (open core). See [docs/monetization.md] for the enterprise tier.
