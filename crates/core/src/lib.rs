//! Cortex transactional execution engine.
//!
//! The guarantee, in one sentence: **run a change in an OverlayFS sandbox,
//! prove it worked, then commit or undo it — with proof, and never silent
//! data loss.**
//!
//! - [`transaction`] — the OverlayFS sandbox: lower/upper/work/merged, private
//!   mount namespace, commit-by-merge. Refuses a tmpfs upper layer (GAP 7).
//! - [`journal`] — the persistent undo log. Captures an inverse *and* a
//!   post-condition before committing, fingerprints what it left behind, and
//!   at undo time checks drift, runs the compensation, and **proves it
//!   worked** before marking anything undone.
//! - [`guard`] — content-addressed drift detection and rescue: undo refuses to
//!   overwrite work that changed since the commit.
//! - [`ui`] — terminal output: colour that disappears when piped.
//!
//! Everything here is engine. Orchestration (which template, which policy,
//! which command) lives in the CLI, so the engine stays testable in isolation.

pub mod guard;
pub mod journal;
pub mod transaction;
pub mod ui;
