# Launch assets — v0.3.0

**Tagline:** Cortex: transactional Linux changes with real undo.

## Why use Cortex (the pitch, one paragraph)

Every tool will happily change your server. Almost none can take it back.
`terraform plan` predicts, `ansible --check` simulates, NixOS rolls back if
you first rewrite your world in Nix. Cortex runs the change **for real, in a
sandbox**, proves it worked, and only then commits — recording an inverse
*and a post-condition that proves the inverse worked*. Undo is refused if
someone else touched the same files (drift detection), and `cortex verify
--self` proves the whole guarantee on your own machine. One 2.4 MB static
binary, offline deterministic planner, no model, no network.

## 15–30 second demo script (recordable)

```
# 1. (2s) The guarantee, no setup:
cortex demo

# 2. (8s) Plain English, real change, full plan shown first:
sudo cortex run nginx on port 8080
curl -s localhost:8080 | head -2

# 3. (5s) Two steps in one sentence:
sudo cortex try "install htop and open port 8080" --plan

# 4. (8s) Undo by name, with proof:
sudo cortex status
sudo cortex undo cortex-nginx     # runs the inverse AND checks it worked

# 5. (3s) Don't trust it — check it:
cortex verify --self
```

## X / Twitter post (ready to copy)

```
Cortex v0.3.0 is ready.

A CLI that runs Linux/DevOps changes transactionally:
sandbox → verify → commit, or undo with proof.

The undo is self-checkable. Drift detection is real. 2.4 MB static binary.

34 built-in templates. Natural commands:
→ cortex run nginx on port 8080
→ cortex try "install htop and open port 8080"   (yes, both — in order)
→ cortex undo cortex-nginx

Install:
curl -sSL https://raw.githubusercontent.com/iPatan7/Aix-Cortex-v1/main/scripts/install.sh | sh

Safety you can trust. Undo you can prove.
Repo: https://github.com/iPatan7/Aix-Cortex-v1

Try it and tell me what you break (it will undo cleanly).
```

## Hacker News (Show HN)

**Title:** Show HN: Cortex – transactional Linux changes with provable undo

**Text:**

I got tired of "rollback" being a promise instead of a property. Cortex is a
small CLI (2.4 MB static binary, Rust) that runs a change inside an
OverlayFS sandbox, verifies it actually took effect, and only then merges it
into the filesystem — after journaling an inverse *and a post-condition that
proves the inverse worked*.

Three design rules came out of real bugs:

1. An inverse must prove itself. `echo done` exits 0; an early version
   accepted it as an undo command and reported a "successful rollback"
   while the container kept running. Now undo runs the inverse and then
   *checks* its post-condition.
2. Nothing generated writes inverses. Reversible ops come from a registry
   of human-written (forward, verify, inverse, verify) quadruples — 34
   built-ins, plus your own in TOML. The planner (offline, deterministic,
   no model) only selects and fills parameters.
3. Undo refuses to destroy other people's work. Commits fingerprint what
   they leave behind; if anyone touched those paths since, undo stops.
   `--force` proceeds but rescues the current contents first.

Natural commands work offline: `cortex run nginx on port 8080`,
`cortex try "install htop and open port 8080"` (composes, runs in order,
unwinds newest-first), `cortex undo cortex-nginx`.

The part I'd most like scrutiny on: `cortex verify --self` runs every
exercisable template through its full cycle on your machine and prints why
the rest were skipped — and CI includes a mutation test that sabotages an
inverse and asserts the suite goes red.

It will not undo the outside world (emails, S3 deletes) and ships no
database migration workflow, because those undos would be lies — that's in
docs/safety.md.

## Reddit (r/selfhosted, r/linuxadmin, r/devops)

**Title:** I built a CLI that makes Linux changes transactional — sandbox,
verify, commit, and an undo it can actually prove

**Body:**

The pitch in one command:

    $ cortex demo

That runs a real config change, has a "colleague" hotfix the same file,
shows undo *refusing* to clobber the hotfix, then a clean verified undo —
in ~2 seconds, no root, no docker.

For real work: `sudo cortex run nginx on port 8080` plans (you see the
exact commands, the undo, and the proof for both, before anything runs),
executes in an OverlayFS sandbox, verifies something is actually listening,
commits, and journals a verified inverse. `sudo cortex undo` reverses it
and *checks* it reversed. `cortex status` shows what's applied, undoable,
or blocked by drift.

34 built-in templates (docker/podman/compose + volumes/env, nginx incl.
TLS, apt/dnf, users/SSH keys, ufw, sysctl, swap, sshd hardening, backups,
git deploys, /etc/hosts), natural-language composition ("install htop and
open port 8080"), and TOML user templates that are held to the same rule:
no inverse post-condition, no load.

2.4 MB static binary, x86_64 + aarch64, Apache-2.0. Homelabbers: this is
aimed at exactly the "I'll just quickly change this on the box" moment.

Install:

    curl -sSL https://raw.githubusercontent.com/iPatan7/Aix-Cortex-v1/main/scripts/install.sh | sh

Break it and tell me — undo failures are the bug reports I want most.

## Positioning / FAQ ammunition

- **vs terraform/ansible:** they predict or simulate; cortex executes for
  real in a sandbox and verifies effects, then keeps a *proved* undo.
- **vs NixOS:** atomic rollback, but only after you rewrite your world in
  Nix. Cortex works on the mutable Debian/Fedora box you already have.
- **vs docker:** isolates but never merges back. Cortex merges back — with
  a receipt.
- **vs `btrfs/zfs snapshot`:** whole-filesystem time travel, no semantics.
  Cortex knows *what* it changed, verifies the change worked, refuses undo
  on drift, and handles state that isn't filesystem (containers, systemd,
  firewall) via verified compensations.
- **"An LLM wrote your undo?"** No. Nothing generated writes inverses;
  models (opt-in only) select from human-written templates. That rule
  exists because the alternative was tried and produced a lying rollback.
