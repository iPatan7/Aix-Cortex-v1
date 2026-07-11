# Contributing to cortex

Thanks for helping. Cortex's whole value is a guarantee — a committed change
can be taken back, with proof — so contributions are judged first by whether
they keep that guarantee checkable.

## Ground rules

- **Every reversible operation ships four commands**: `forward`,
  `verify_forward`, `inverse`, `verify_inverse`. A template whose inverse
  has no post-condition is refused by `well_formed()` and will not merge.
- **Nothing generated writes inverses.** The planner (and any LLM path)
  selects templates and fills parameters. If your change lets anything else
  author an undo, it will be declined regardless of how useful it is.
- **A false match is worse than a miss.** The planner must prefer teaching
  (`cortex do <template> key=<hole>`) over guessing. New matching rules need
  tests for the phrases they must *not* match.
- **Skips are stated, never silent.** `cortex verify --self` prints why a
  template was not exercised. Keep it that way.

## Getting started

```sh
cargo build
cargo test --workspace          # 150+ tests, no root needed
sudo -E cargo test -p cortex-core   # overlay/journal tests, as root
./target/debug/cortex demo      # the guarantee in ~2s
./target/debug/cortex verify --self
cargo clippy --all-targets --all-features   # zero warnings policy
cargo fmt --all
```

CI (`.github/workflows/ci.yml`) runs fmt, clippy (`-D warnings`), the
guarantee suite, root overlay tests, real-docker conformance (including a
mutation test that sabotages an inverse and asserts the suite goes red),
end-to-end try→undo, the policy gate, and a static musl build. All of it
must stay green.

## Adding a built-in template

1. Add the entry to `crates/registry/src/builtin.rs`. Order matters for
   tie-breaking: more specific templates come before broader ones.
2. Write the four commands. The verifiers must check *effects* (something
   listening on the port, the file byte-identical), not exit codes.
3. Fill `drift_note`: what undo does if the world moved between commit and
   undo. This line renders in every plan; write it for the operator.
4. Add the id to `phase_two_coverage` (or a new coverage test) in
   `crates/registry/src/lib.rs`.
5. If the template can be exercised without mutating a host anyone cares
   about, add a fixture in `self_test_args()`
   (`crates/cli/src/workflow.rs`); otherwise add an honest reason to
   `unavailable()`.
6. Add planner phrasing tests in `crates/planner/src/lib.rs` — including at
   least one phrase that must *not* reach your template.

Prototype as a TOML user template first if you like
(`~/.cortex/templates/*.toml`, see [docs/templates.md](docs/templates.md)) —
the format is identical and the load-time gate gives instant feedback.

## What gets declined

- Reversible-looking operations whose undo is actually lossy (the database
  migration rule — see [docs/safety.md](docs/safety.md)).
- Inverses verified only by exit status.
- Dependencies that break the static musl build or meaningfully grow the
  binary (it ships at ~2.4 MB; that is a feature).
- Network calls anywhere in the planner or undo paths.

## Commit style

Present-tense, single-purpose commits whose message says why, not just what.
Run `cargo fmt`, `cargo clippy` and the full test suite before pushing.
