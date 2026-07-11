# Safety: what cortex guarantees, how, and what it refuses to promise

Cortex's entire claim is one sentence: **a committed change can be taken
back, and the take-back is proved, not assumed.** This document explains the
machinery behind that sentence, and — just as important — where the
guarantee ends.

Every mechanism here exists because its absence was once a real bug,
reproduced on a real machine. None of it is speculative hardening.

## The lifecycle of a change

```
English ──planner──▶ plan ──policy──▶ sandbox ──verify──▶ journal ──merge──▶ committed
                      │                  │         │
                      └── shown in full  └── rollback on any failure
                          before anything runs
```

1. **Plan.** The offline planner selects a template — a human-written
   `(forward, verify_forward, inverse, verify_inverse)` quadruple — and
   binds parameters. The full plan, including the undo and both proofs, is
   printed before anything executes. `--plan` stops here.
2. **Authorize.** The *resolved* operation (template id + bound arguments,
   never the English that produced it) is evaluated deny-by-default against
   a root-owned policy file.
3. **Sandbox.** Filesystem-backed operations run inside an OverlayFS
   transaction in a private mount namespace. The host filesystem cannot
   change during this phase; a failure discards the upper layer and the
   host is untouched.
4. **Verify.** The template's forward post-condition must hold *inside the
   sandbox*. A command that exits 0 without taking effect — `sed -i` with a
   pattern that matched nothing, an install that silently failed — is
   caught here and rolled back.
5. **Journal, then merge.** The inverse is armed in the persistent journal
   *before* the merge makes anything visible (saga discipline). If the
   merge dies halfway, the journal already holds the full prior state.
6. **Seal.** After the merge, cortex fingerprints every path it left behind
   (content hash, mode, owner, link target). These fingerprints are what
   make drift detection possible later.

Host-side operations (containers, systemd units, firewall rules) cannot be
sandboxed by an overlay — their state lives in dockerd or systemd, not the
filesystem. For those, safety is the **verified compensation**: the forward
post-condition must hold before anything is journaled, and the journaled
inverse carries its own post-condition that undo will check.

## Provable undo

Most "rollback" is a promise. Cortex's is a property with a checkable proof,
resting on three rules:

**1. An inverse must prove itself.** A compensation that exits `0` proves
nothing — `echo done` exits `0`. That exact failure happened: an early
design accepted `--undo-cmd "echo done"`, committed it, and later reported a
"successful undo" while the container it was supposed to destroy kept
running. Now every inverse ships a post-condition, and `undo` runs the
inverse **and then checks**. If the post-condition does not hold, the entry
stays pending and cortex says so.

**2. Nothing generated writes inverses.** Reversible operations come only
from the template registry — human-written, reviewed with the code (or, for
user templates, written by the operator and held to the same gate). The
planner *selects* and *parameterizes*; it never authors an undo, because an
inverse is a semantic claim no compiler can check. An LLM (if you opt in)
gets exactly the same power: selection, not authorship.

**3. Anything else is `Irreversible`, out loud.** Operations outside the
registry run only with explicit consent (`--yes-irreversible`), are
journaled as irreversible, show up marked in `cortex status`, and `undo`
refuses them with instructions rather than pretending.

You do not have to take any of this on faith:

```console
$ cortex verify --self
```

runs every exercisable template through its full cycle — forward, prove,
inverse, prove — on your machine, and prints *why* each skipped template was
skipped. A suite that hides its own coverage would be the same lie as an
undo that hides its own failure. CI additionally runs the host-mutating
templates (packages, users, services, firewall) in containers, and includes
a mutation test: sabotage a template's inverse and assert the suite goes
red.

## Drift detection

Undo restores the world cortex left behind — but the world moves. Between
commit and undo, a colleague may hotfix the same file. Restoring "your"
bytes over their fix would make undo a data-loss tool.

So every commit seals fingerprints of what it left behind. Before undo
touches anything, it re-checks them:

- **Clean:** the restore proceeds, and the inverse's post-condition is
  checked afterwards.
- **Drifted:** undo refuses, names the changed paths (`cortex receipt <id>`
  shows them marked), and stops. Nothing has been modified at that point —
  the check runs before any side effect, because stopping a service for an
  undo that then refuses would leave the system worse than doing nothing.
- **`--force`:** proceeds anyway, but first *rescues* the current contents
  of every path it is about to overwrite. A destructive operation that
  keeps no copy of what it destroyed is not a feature cortex ships.

Host-side templates state their drift contract in the plan itself (the
`drift` line): e.g. `docker rm -f` removes the container *even if someone
restarted it*, and `docker volume rm` refuses while a container still uses
the volume. Templates whose undo target cannot be derived (a sysctl's prior
value) require it declared upfront (`previous=`), so the undo is exact by
construction rather than guessed.

## What cortex refuses to promise

Stated plainly, because a safety tool that oversells is worse than none:

- **You cannot undo the outside world.** An email sent, an S3 object
  deleted, a certificate issued by a CA (cortex removes the local files;
  it does not revoke). Cortex compensates; it does not reverse time.
- **You cannot undo impact.** If nginx served errors for 40 seconds before
  you rolled back, those requests happened. Cortex reduces MTTR, not
  history.
- **No database migrations.** A migration's undo is inherently lossy
  (`DROP COLUMN` does not restore the data), so cortex ships *no* migration
  workflow rather than one that lies. A snapshot-backed template that dumps
  affected tables and restores from the dump would be honest; it is not
  built yet.
- **Undo `--force` is still a decision.** Drift means two changes touched
  the same state; cortex cannot merge them, only pick one and keep a copy
  of the other.
- **Linux only, root for the real thing.** OverlayFS, mount namespaces, and
  restoring files into root-owned trees need privileges. `cortex demo`,
  `--plan`, `templates`, and the planner all work unprivileged.

## The security boundary

Cortex typically runs as root, so its own gate is the boundary:

- The policy file must be **root-owned and not group/world-writable**, or
  cortex refuses to load it. Rules the subject can rewrite are not rules.
  Pointing `--policy` at a caller-writable file is refused for the same
  reason.
- User templates are held to the same standard when running as root: a root
  binary executing commands from a file any user can edit is sudo with
  extra steps.
- A user template **may not shadow a built-in** — redefining `docker.run`
  with a weaker inverse would silently replace a reviewed promise.
- Every parameter is validated by type, then **shell-quoted at render
  time**. A value can never inject shell syntax into a command a human
  wrote; `name="evil; rm -rf /"` fails validation before quoting even
  matters.
- Inside the sandbox, commands run under a Landlock allowlist (where the
  kernel supports it): the `/proc`, `/sys`, `/dev`, `/run` trees bound in
  for functionality are read-only, so even root inside the sandbox cannot
  write to the host through a bind mount.

## Verifying all of this yourself

```console
$ cortex demo            # the whole guarantee in ~2s, no root, no docker
$ cortex verify --self   # every exercisable template's round-trip, proved
$ cortex templates show <id>   # any template's full contract, undo included
```

The engine's own test suite (`cargo test --workspace`, 150+ tests) includes
the guarantee suite — undo-refuses-drift, force-rescues-contents,
inverse-must-verify — run unprivileged and as root in CI, plus the
conformance and policy-gate jobs. See `.github/workflows/ci.yml`; every job
there guards a property this document claims.
