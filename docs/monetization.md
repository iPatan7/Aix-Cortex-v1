# Monetization: open core

The wedge is the reversibility guarantee. It is the *free* thing — never charge
for the safety primitive, or you lose the adoption that makes everything else
worth buying. The business is what teams and enterprises need *around* a fleet
of machines each running the free CLI.

## The line

Everything that makes one machine's changes safe is OSS (Apache-2.0). Everything
that makes an *organization's* changes governable, auditable, and fleet-wide is
the commercial tier.

## Free — the CLI (this repo)

The transactional engine, all commands, the registry, the policy gate, the
conformance suite. This is what a DevOps engineer installs, runs, and tells
their team about. It must be genuinely excellent on its own; the paid tiers are
not a crippled-free upsell.

Wins: adoption, the HN post, trust. `cortex verify --self` is the marketing —
skeptics run it themselves and it holds.

## Team — $ per seat/host

The journal is already a **signed, tamper-evident, per-machine audit log** of
every change. Aggregate that across a team and you have a product:

- **Central audit.** Every change on every host, who ran it, whether it was
  reversed, streamed to one place. You already produce the receipts.
- **Shared policy.** Author `/etc/cortex/policy.toml` once, distribute and
  enforce it across hosts. The gate already exists; this is distribution +
  drift detection on the policy itself.
- **Fleet status/undo.** "Undo that config change on all 40 web hosts" as one
  command, as a distributed transaction: commit everywhere or roll back
  everywhere.

This is the natural extension of the primitive and the thing nobody else has.

## Enterprise — $$$ contract

The journal is an accident of good design: it is **compliance evidence**.

- **Compliance export.** "Prove to your auditor that every production change in
  the last year was reviewed, verified, and reversible." SOC2 / ISO / FedRAMP
  evidence generation is a budget line at every company over ~200 people. You
  already have the hash-chained, signed ledger; this packages it.
- **SSO / RBAC.** Policy scoped to identity, not just uid. Approval workflows
  (two-person rule for irreversible ops) backed by the org's IdP.
- **Air-gapped / on-prem control plane.** The daemon (see roadmap) as a
  self-hosted fleet server.

## Why this order

Undo gets you *adopted* by engineers. The signed "every change reviewed and
reversible" ledger gets you *bought* by their CISO. The compliance angle is
worth more than the undo — and you have already built 80% of it. Lead with the
guarantee to win the engineer; sell the ledger to win the org.

## What not to do

- Don't charge for the guarantee. It is the wedge; keep it free and excellent.
- Don't ship a crippled free tier. The paid value is *organizational*, not
  per-machine — a single engineer should never hit a paywall doing their job.
- Don't build the enterprise features before the free CLI has users. The
  fastest way back to a 133k-line unfocused repo is to build fleet management
  for a product nobody runs yet.
