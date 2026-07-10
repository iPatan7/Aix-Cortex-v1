# Roadmap & extension seams

v1 ships the guarantee, rock-solid. The features below are deliberately *not*
in v1 — each would add surface that dilutes "provable" without a user asking
for it yet. They are listed with the seam already present in the code, so
adding them is a bounded change, not a redesign.

## Plugins (seam: the registry)

A plugin is a set of templates. The registry
(`crates/registry/src/lib.rs`) is a `&[Template]` today; making it loadable
from `/etc/cortex/templates.d/*.toml` at startup turns "add a template" into a
config change. The invariant CI already enforces — every template's inverse
must pass its own post-condition — applies unchanged to loaded plugins, so a
third-party template cannot ship a rollback that lies.

- **Seam:** `Template` is `Deserialize`-ready; `registry::lookup` is the one
  call site.
- **Guardrail:** `cortex verify --self` must run over loaded plugins too, and
  refuse to enable one whose conformance fails.

## Daemon (seam: the journal is already a durable log)

Long-running and fleet operations want a process that owns the journal and
serves status/undo over a socket. The journal
(`crates/core/src/journal.rs`) is already a persistent, append-mostly directory
with atomic entry renames, so a daemon is a *reader/writer* of existing state,
not a new source of truth. `cortex status` / `undo` become thin clients.

- **Seam:** every journal operation takes a `journal_dir`; a daemon owns it.
- **Guardrail:** the daemon must not become a second authorization path — it
  evaluates the same `cortex-policy` gate, or it is a hole.

## eBPF (seam: the transaction wraps command execution)

eBPF earns its place for *visibility*: which syscalls, files, and network a
template's command actually touched, attached as evidence to the receipt. It is
deliberately out of v1 because it needs a privileged loader and a separate
build toolchain, and cannot run in most CI — it fights "static binary" and
"provable."

- **Seam:** `Transaction::run_in_root` is the single point where the command
  is spawned; a tracer attaches there.
- **Guardrail:** tracing is observation, never enforcement. A failed trace must
  degrade to "no evidence captured," never block a legitimate operation.

## Snapshot-backed database migrations (the honest GAP 8 successor)

v1 ships *no* migration workflow, because a migration's undo is lossy. The
honest design: before the migration, dump the affected tables
(`pg_dump -t`); the inverse restores from that dump; the post-condition checks
the restored schema and row count. That makes it genuinely `Exact`-reversible
rather than a comforting fiction.

- **Seam:** it is a registry template with `host_side: true` and a captured
  artifact path, no engine change.
- **Guardrail:** it may only be labelled reversible if the restore is verified;
  otherwise it is `Irreversible` and demands consent.

## SLO-gated commit (the differentiator nobody else has)

`cortex try "upgrade nginx" --verify-slo p95<200ms` — run the change in the
sandbox, drive real traffic, and *refuse to commit* if the latency objective is
missed. The forward post-condition (`verify_forward`) is already a gate; an SLO
probe is a slower one. This is the demo that makes a staff engineer sit up, and
it falls out of the existing architecture.

## Cutting a release

Releases are built by CI, not a laptop, so every binary is reproducible from a
clean runner. Push a version tag and `.github/workflows/release.yml` does the
rest:

```sh
git tag -a v0.2.0 -m "cortex v0.2.0"
git push origin v0.2.0
```

It builds the static musl binary for **x86_64 and aarch64** on native runners
of each architecture (no cross-compilation, so the static-linkage and smoke
checks run on the real target), packages each as
`cortex-<tag>-<target>.tar.gz` with a `.sha256`, verifies every checksum, and
publishes the GitHub release. The asset names match `scripts/install.sh`, so
`curl | sh` picks the right architecture automatically.
