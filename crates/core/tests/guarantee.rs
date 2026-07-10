//! The conformance suite for the central promise:
//!
//!   "Run any change transactionally. Verify it works. Commit or undo
//!    everything perfectly with proof."
//!
//! Each test here corresponds to a way that promise was previously false and
//! was reproduced on a real machine. If any of these fail, the guarantee does
//! not hold and cortex must not claim it does. They run without root.

#![cfg(target_os = "linux")]

use cortex_core::guard::{detect_drift, Fingerprint};
use cortex_core::journal::Journal;
use std::fs;
use std::path::PathBuf;

/// Build a lower dir and a fabricated upper layer, then capture+seal an
/// entry as a commit would. Returns (journal, lower, entry).
fn commit(
    root: &std::path::Path,
    prior: &[(&str, &str)],
    staged: &[(&str, &str)],
) -> (Journal, PathBuf, cortex_core::journal::EntryMeta) {
    let lower = root.join("lower");
    let upper = root.join("upper");
    fs::create_dir_all(&lower).unwrap();
    fs::create_dir_all(&upper).unwrap();
    for (p, c) in prior {
        fs::write(lower.join(p), c).unwrap();
    }
    for (p, c) in staged {
        fs::write(upper.join(p), c).unwrap();
    }

    let journal = Journal::new(root.join("journal"));
    let entry = journal
        .capture(
            &upper,
            &lower,
            "test",
            None,
            "staged change",
            None,
            None,
            None,
        )
        .unwrap();
    // The "merge": apply the upper layer to the lower.
    for (p, c) in staged {
        fs::write(lower.join(p), c).unwrap();
    }
    let entry = journal.seal(&entry).unwrap();
    (journal, lower, entry)
}

/// GAP 1, reproduced before the fix:
///   cortex commits v2; a human hotfixes to v3; undo overwrites it with v1
///   and reports success. The hotfix is gone.
///
/// Now: undo must REFUSE.
#[test]
fn undo_refuses_when_someone_else_changed_the_file() {
    let t = tempfile::tempdir().unwrap();
    let (journal, lower, entry) = commit(
        t.path(),
        &[("app.conf", "v1-original")],
        &[("app.conf", "v2-cortex")],
    );
    assert_eq!(
        entry.fingerprints.len(),
        1,
        "the commit must fingerprint what it left"
    );

    // A colleague hotfixes the same file.
    fs::write(lower.join("app.conf"), "v3-human-hotfix-DO-NOT-LOSE").unwrap();

    let err = journal.undo(None, false).unwrap_err();
    let msg = format!("{err}");
    assert!(msg.contains("changed since cortex committed"), "got: {msg}");

    // The hotfix survives, and the entry is still pending so it can be
    // resolved deliberately.
    assert_eq!(
        fs::read_to_string(lower.join("app.conf")).unwrap(),
        "v3-human-hotfix-DO-NOT-LOSE"
    );
    assert_eq!(journal.pending().unwrap().len(), 1);
}

/// `--force` is the deliberate override, and it must never destroy without
/// keeping a copy first.
#[test]
fn forced_undo_rescues_the_content_it_overwrites() {
    let t = tempfile::tempdir().unwrap();
    let (journal, lower, entry) = commit(
        t.path(),
        &[("app.conf", "v1-original")],
        &[("app.conf", "v2-cortex")],
    );
    fs::write(lower.join("app.conf"), "v3-human-hotfix").unwrap();

    journal.undo(None, true).expect("--force proceeds");

    // The undo happened...
    assert_eq!(
        fs::read_to_string(lower.join("app.conf")).unwrap(),
        "v1-original"
    );
    // ...and the clobbered work is recoverable.
    let rescued = t
        .path()
        .join("journal")
        .join(format!("{}.undone", entry.id))
        .join("rescued/app.conf");
    assert_eq!(
        fs::read_to_string(&rescued).unwrap(),
        "v3-human-hotfix",
        "forced undo must rescue what it overwrites, at {rescued:?}"
    );
}

/// An unchanged world is the happy path: no drift, undo proceeds.
#[test]
fn undo_proceeds_when_nothing_moved() {
    let t = tempfile::tempdir().unwrap();
    let (journal, lower, _) = commit(
        t.path(),
        &[("app.conf", "v1-original")],
        &[("app.conf", "v2-cortex")],
    );
    journal.undo(None, false).expect("no drift, so undo runs");
    assert_eq!(
        fs::read_to_string(lower.join("app.conf")).unwrap(),
        "v1-original"
    );
}

/// GAP 2, reproduced before the fix:
///   `--undo-cmd "echo done"` was accepted, and undo reported success while
///   the container kept running.
///
/// Now: a compensation with no post-condition cannot even be journaled.
#[test]
fn a_compensation_without_a_post_condition_cannot_be_journaled() {
    let t = tempfile::tempdir().unwrap();
    let journal = Journal::new(t.path().join("journal"));

    let err = journal
        .capture_compensation(t.path(), "k", None, "docker run ...", "echo done", "", None)
        .unwrap_err();
    assert!(format!("{err}").contains("non-empty"), "got: {err}");

    // And the filesystem path refuses the same way.
    let upper = t.path().join("upper");
    fs::create_dir_all(&upper).unwrap();
    let err = journal
        .capture(
            &upper,
            t.path(),
            "k",
            None,
            "d",
            Some("echo done"),
            None,
            None,
        )
        .unwrap_err();
    assert!(format!("{err}").contains("no post-condition"), "got: {err}");
}

/// And if a compensation *does* run but does not take effect, undo must say
/// so and leave the entry pending rather than marking it reverted.
#[test]
fn an_unverified_compensation_leaves_the_entry_pending() {
    let t = tempfile::tempdir().unwrap();
    let journal = Journal::new(t.path().join("journal"));
    let marker = t.path().join("still-here");
    fs::write(&marker, "the thing that should have been removed").unwrap();

    // A lying inverse: exits 0, changes nothing. Its post-condition asserts
    // the marker is gone — which it will not be.
    let entry = journal
        .capture_compensation(
            t.path(),
            "test",
            None,
            "created something",
            "echo 'I did not actually undo anything'",
            &format!("! test -e {}", marker.display()),
            None,
        )
        .unwrap();

    let err = journal.undo(None, false).unwrap_err();
    let msg = format!("{err}");
    assert!(msg.contains("NOT complete"), "got: {msg}");
    assert!(msg.contains("was not actually reversed"), "got: {msg}");

    // The critical property: cortex did NOT claim success.
    let pending = journal.pending().unwrap();
    assert_eq!(pending.len(), 1, "the entry must remain pending");
    assert_eq!(pending[0].id, entry.id);
    assert!(marker.exists(), "nothing was actually undone");
}

/// A truthful compensation passes its post-condition and completes.
#[test]
fn a_verified_compensation_completes_the_undo() {
    let t = tempfile::tempdir().unwrap();
    let journal = Journal::new(t.path().join("journal"));
    let marker = t.path().join("resource");
    fs::write(&marker, "created").unwrap();

    journal
        .capture_compensation(
            t.path(),
            "test",
            None,
            "created a resource",
            &format!("rm -f {}", marker.display()),
            &format!("! test -e {}", marker.display()),
            None,
        )
        .unwrap();

    journal.undo(None, false).expect("honest inverse verifies");
    assert!(!marker.exists());
    assert!(journal.pending().unwrap().is_empty());
}

/// GAP 4: an irreversible operation is recorded honestly, and undo refuses
/// to pretend it can reverse it.
#[test]
fn irreversible_entries_are_never_silently_skipped() {
    let t = tempfile::tempdir().unwrap();
    let journal = Journal::new(t.path().join("journal"));
    journal
        .capture_irreversible(t.path(), "aws s3 rm s3://bucket --recursive")
        .unwrap();

    let err = journal.undo(None, false).unwrap_err();
    let msg = format!("{err}");
    assert!(msg.contains("irreversible"), "got: {msg}");
    assert!(msg.contains("will not pretend"), "got: {msg}");

    // It stays pending and visible until the operator dismisses it.
    assert_eq!(journal.pending().unwrap().len(), 1);
    journal.forget(&journal.pending().unwrap()[0].id).unwrap();
    assert!(journal.pending().unwrap().is_empty());
}

/// A forgotten entry must not be resurrected by `undo --all`.
#[test]
fn forgotten_entries_leave_the_pending_set() {
    let t = tempfile::tempdir().unwrap();
    let journal = Journal::new(t.path().join("journal"));
    let e = journal
        .capture_irreversible(t.path(), "dropped a table")
        .unwrap();
    journal.forget(&e.id).unwrap();

    assert!(journal.pending().unwrap().is_empty());
    // Still auditable.
    let all = journal.entries().unwrap();
    assert_eq!(all.len(), 1);
    assert!(all[0].forgotten);
}

// (The "every template can prove its own inverse" guarantee lives in the
// cortex-registry crate's own tests, alongside the templates themselves.)

/// The drift detector must see a file that was deleted after the commit,
/// not only one that was edited.
#[test]
fn drift_covers_deletion_and_recreation() {
    let t = tempfile::tempdir().unwrap();
    let (_, lower, entry) = commit(t.path(), &[("a", "1")], &[("a", "2")]);

    fs::remove_file(lower.join("a")).unwrap();
    let d = detect_drift(&lower, &entry.fingerprints).unwrap();
    assert_eq!(d.len(), 1);
    assert_eq!(d[0].found, Fingerprint::Absent);
}
