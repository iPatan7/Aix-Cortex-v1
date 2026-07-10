//! Integration tests for the saga-style undo journal.
//!
//! Overlay mounts and whiteout mknod need root, so these tests skip
//! (with a message) when not run as root:
//!   sudo -E cargo test -p cortex-core --test journal

#![cfg(target_os = "linux")]

use cortex_core::journal::{staged_changes, Journal};
use cortex_core::transaction::Transaction;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

fn is_root() -> bool {
    Command::new("id")
        .arg("-u")
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim() == "0")
        .unwrap_or(false)
}

struct Dirs {
    lower: PathBuf,
    upper: PathBuf,
    work: PathBuf,
    merged: PathBuf,
    journal: PathBuf,
}

fn setup(root: &Path) -> Dirs {
    let dirs = Dirs {
        lower: root.join("lower"),
        upper: root.join("upper"),
        work: root.join("work"),
        merged: root.join("merged"),
        journal: root.join("journal"),
    };
    fs::create_dir_all(&dirs.lower).unwrap();
    fs::write(dirs.lower.join("base.txt"), "original").unwrap();
    fs::write(dirs.lower.join("doomed.txt"), "delete me").unwrap();
    dirs
}

fn open(dirs: &Dirs) -> Transaction {
    Transaction::new(
        &[dirs.lower.as_path()],
        &dirs.upper,
        &dirs.work,
        &dirs.merged,
    )
    .expect("overlay mount failed")
}

#[test]
fn undo_restores_the_exact_prior_state() {
    if !is_root() {
        eprintln!("skipping undo_restores_the_exact_prior_state: requires root");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let dirs = setup(tmp.path());

    // The mode-restoration half of the test needs a known starting mode.
    fs::set_permissions(
        dirs.lower.join("doomed.txt"),
        fs::Permissions::from_mode(0o640),
    )
    .unwrap();
    let keeper = dirs.lower.join("keeper.txt");
    fs::write(&keeper, "keep me").unwrap();
    fs::set_permissions(&keeper, fs::Permissions::from_mode(0o644)).unwrap();

    // Stage a modify, a create, a delete, and a chmod inside the transaction.
    let tx = open(&dirs);
    fs::write(dirs.merged.join("base.txt"), "changed").unwrap();
    fs::write(dirs.merged.join("new.txt"), "created").unwrap();
    fs::remove_file(dirs.merged.join("doomed.txt")).unwrap();
    fs::set_permissions(
        dirs.merged.join("keeper.txt"),
        fs::Permissions::from_mode(0o600),
    )
    .unwrap();

    // Arm the inverse before the merge (saga order), then commit.
    let journal = Journal::new(&dirs.journal);
    let entry = journal
        .capture(
            &dirs.upper,
            &dirs.lower,
            "test",
            None,
            "modify+create+delete+chmod",
            None,
            None,
            None,
        )
        .unwrap();
    assert_eq!(entry.changes, 4);
    tx.commit(true).unwrap();

    assert_eq!(
        fs::read_to_string(dirs.lower.join("base.txt")).unwrap(),
        "changed"
    );
    assert_eq!(
        fs::read_to_string(dirs.lower.join("new.txt")).unwrap(),
        "created"
    );
    assert!(!dirs.lower.join("doomed.txt").exists());
    assert_eq!(
        fs::metadata(&keeper).unwrap().permissions().mode() & 0o7777,
        0o600
    );

    // Undo (LIFO latest) must restore the prior state exactly: contents,
    // presence, and permissions.
    let undone = journal.undo(None, false).unwrap();
    assert_eq!(undone.id, entry.id);
    assert_eq!(
        fs::read_to_string(dirs.lower.join("base.txt")).unwrap(),
        "original"
    );
    assert!(!dirs.lower.join("new.txt").exists());
    assert_eq!(
        fs::read_to_string(dirs.lower.join("doomed.txt")).unwrap(),
        "delete me"
    );
    let doomed_mode = fs::metadata(dirs.lower.join("doomed.txt"))
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(
        doomed_mode & 0o7777,
        0o640,
        "restored file keeps its prior mode"
    );
    assert_eq!(
        fs::metadata(&keeper).unwrap().permissions().mode() & 0o7777,
        0o644,
        "chmod-only change is undone"
    );

    // The entry is audited as undone, and a second undo has nothing to do.
    assert!(journal.entries().unwrap()[0].undone);
    assert!(journal.undo(None, false).is_err());
}

/// The journal is plain file manipulation for modifications and deletions —
/// no overlay mount, no whiteout mknod — so this runs without root. It
/// fabricates upper layers by hand instead of mounting them.
#[test]
fn capture_undo_and_lifo_order_without_root() {
    let tmp = tempfile::tempdir().unwrap();
    let lower = tmp.path().join("lower");
    fs::create_dir_all(lower.join("etc")).unwrap();
    fs::write(lower.join("etc/app.conf"), "port 80").unwrap();
    let journal = Journal::new(tmp.path().join("journal"));

    // Commit 1: modify etc/app.conf.
    let upper1 = tmp.path().join("upper1");
    fs::create_dir_all(upper1.join("etc")).unwrap();
    fs::write(upper1.join("etc/app.conf"), "port 8080").unwrap();
    assert_eq!(staged_changes(&upper1, &lower).unwrap().len(), 1);
    let e1 = journal
        .capture(
            &upper1,
            &lower,
            "safe-config",
            Some("app"),
            "port 8080",
            None,
            None,
            None,
        )
        .unwrap();
    assert_eq!(e1.changes, 1);
    assert_eq!(e1.sample, vec!["etc/app.conf".to_string()]);
    fs::write(lower.join("etc/app.conf"), "port 8080").unwrap(); // the "merge"

    // Commit 2: modify it again.
    let upper2 = tmp.path().join("upper2");
    fs::create_dir_all(upper2.join("etc")).unwrap();
    fs::write(upper2.join("etc/app.conf"), "port 9090").unwrap();
    let e2 = journal
        .capture(
            &upper2,
            &lower,
            "safe-config",
            Some("app"),
            "port 9090",
            None,
            None,
            None,
        )
        .unwrap();
    assert!(
        e2.id > e1.id,
        "ids must be monotonic even within one second: {} then {}",
        e1.id,
        e2.id
    );
    fs::write(lower.join("etc/app.conf"), "port 9090").unwrap();

    // LIFO: undoing the older entry while the newer is pending is refused
    // without force.
    let err = journal.undo(Some(&e1.id), false).unwrap_err().to_string();
    assert!(err.contains("newer"), "unexpected error: {err}");

    // Undo newest-first walks the state back exactly.
    assert_eq!(journal.undo(None, false).unwrap().id, e2.id);
    assert_eq!(
        fs::read_to_string(lower.join("etc/app.conf")).unwrap(),
        "port 8080"
    );
    assert_eq!(journal.undo(None, false).unwrap().id, e1.id);
    assert_eq!(
        fs::read_to_string(lower.join("etc/app.conf")).unwrap(),
        "port 80"
    );
    assert!(journal.latest_pending().unwrap().is_none());
}

/// A chmod copies the file up with identical bytes; the journal must still
/// see it, capture the prior mode, and restore it on undo. (This was the
/// reported undo bug: metadata-only changes were invisible to the capture,
/// so they were committed but never reverted.)
#[test]
fn permission_only_change_is_captured_and_undone() {
    let tmp = tempfile::tempdir().unwrap();
    let lower = tmp.path().join("lower");
    fs::create_dir_all(&lower).unwrap();
    let conf = lower.join("app.conf");
    fs::write(&conf, "secret").unwrap();
    fs::set_permissions(&conf, fs::Permissions::from_mode(0o644)).unwrap();

    // Fabricated upper layer: same bytes, tightened mode (what a copy-up
    // of `chmod 600` looks like).
    let upper = tmp.path().join("upper");
    fs::create_dir_all(&upper).unwrap();
    fs::write(upper.join("app.conf"), "secret").unwrap();
    fs::set_permissions(upper.join("app.conf"), fs::Permissions::from_mode(0o600)).unwrap();

    let changes = staged_changes(&upper, &lower).unwrap();
    assert_eq!(changes.len(), 1, "mode-only change must be visible");

    let journal = Journal::new(tmp.path().join("journal"));
    journal
        .capture(
            &upper,
            &lower,
            "safe-file-edit",
            None,
            "chmod 600 app.conf",
            None,
            None,
            None,
        )
        .unwrap();
    fs::set_permissions(&conf, fs::Permissions::from_mode(0o600)).unwrap(); // the "merge"

    journal.undo(None, false).unwrap();
    assert_eq!(
        fs::metadata(&conf).unwrap().permissions().mode() & 0o7777,
        0o644
    );
    assert_eq!(fs::read_to_string(&conf).unwrap(), "secret");
}

#[test]
fn identical_rewrite_is_detected_as_no_effect() {
    if !is_root() {
        eprintln!("skipping identical_rewrite_is_detected_as_no_effect: requires root");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let dirs = setup(tmp.path());

    // Rewriting a file with identical bytes copies it up to the upper layer
    // (as `sed -i` with a non-matching pattern does), but it is not an
    // effective change and must not count as one.
    let tx = open(&dirs);
    fs::write(dirs.merged.join("base.txt"), "original").unwrap();
    assert!(dirs.upper.join("base.txt").exists(), "expected copy-up");
    assert!(staged_changes(&dirs.upper, &dirs.lower).unwrap().is_empty());

    fs::write(dirs.merged.join("base.txt"), "different").unwrap();
    assert_eq!(staged_changes(&dirs.upper, &dirs.lower).unwrap().len(), 1);
    tx.rollback().unwrap();
}
