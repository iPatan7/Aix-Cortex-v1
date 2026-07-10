//! Integration tests for the OverlayFS transaction engine.
//!
//! Mounting kernel OverlayFS needs CAP_SYS_ADMIN, so these tests skip
//! (with a message) when not run as root:
//!   sudo -E cargo test -p cortex-core --test transaction

#![cfg(target_os = "linux")]

use cortex_core::transaction::Transaction;
use std::fs;
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
}

fn setup(root: &Path) -> Dirs {
    let dirs = Dirs {
        lower: root.join("lower"),
        upper: root.join("upper"),
        work: root.join("work"),
        merged: root.join("merged"),
    };
    fs::create_dir_all(&dirs.lower).unwrap();
    fs::write(dirs.lower.join("base.txt"), "from lower").unwrap();
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
fn commit_keeps_file_on_merged_mount() {
    if !is_root() {
        eprintln!("skipping commit_keeps_file_on_merged_mount: requires root");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let dirs = setup(tmp.path());

    let tx = open(&dirs);
    fs::write(dirs.merged.join("tx.txt"), "committed").unwrap();
    tx.commit(false).unwrap();

    // Remount the same layers: the committed file must be visible on merged.
    let tx2 = open(&dirs);
    assert_eq!(
        fs::read_to_string(dirs.merged.join("tx.txt")).unwrap(),
        "committed"
    );
    assert_eq!(
        fs::read_to_string(dirs.merged.join("base.txt")).unwrap(),
        "from lower"
    );
    drop(tx2);
}

#[test]
fn rollback_discards_file() {
    if !is_root() {
        eprintln!("skipping rollback_discards_file: requires root");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let dirs = setup(tmp.path());

    let tx = open(&dirs);
    fs::write(dirs.merged.join("tx.txt"), "discard me").unwrap();
    assert!(dirs.merged.join("tx.txt").exists());
    tx.rollback().unwrap();

    // Remount: the rolled-back file must be gone, the base intact.
    let tx2 = open(&dirs);
    assert!(!dirs.merged.join("tx.txt").exists());
    assert_eq!(
        fs::read_to_string(dirs.merged.join("base.txt")).unwrap(),
        "from lower"
    );
    drop(tx2);
}

/// GAP 7: opening a transaction with its upper layer on tmpfs would send every
/// written byte to RAM, and a large install can OOM the host. It must be
/// refused by default, with an override for the small-write case.
#[test]
fn tmpfs_upper_is_refused_by_default() {
    // /dev/shm is tmpfs on essentially every Linux host and is world-writable,
    // so this runs without root. Skip cleanly if it is somehow absent.
    let shm = std::path::Path::new("/dev/shm");
    if !shm.is_dir() {
        eprintln!("skipping tmpfs_upper_is_refused_by_default: no /dev/shm");
        return;
    }
    let base = shm.join(format!("cortex-tmpfs-test-{}", std::process::id()));
    let _ = fs::create_dir_all(&base);
    let lower = base.join("lower");
    fs::create_dir_all(&lower).unwrap();
    fs::write(lower.join("x"), "y").unwrap();

    let result = Transaction::new(
        &[lower.as_path()],
        &base.join("upper"),
        &base.join("work"),
        &base.join("merged"),
    );
    let msg = match result {
        Ok(_) => panic!("tmpfs upper should have been refused"),
        Err(e) => format!("{e:#}"),
    };
    // The distinctive phrase from preflight_upper's refusal, not just the word
    // "tmpfs" (which the mount path itself contains).
    assert!(
        msg.contains("would go to RAM"),
        "expected the tmpfs preflight refusal, got: {msg}"
    );

    // The override lets a caller opt in for a deliberately small write.
    std::env::set_var("CORTEX_ALLOW_TMPFS_UPPER", "1");
    // (mount itself needs root, so we only assert the preflight no longer
    // refuses; a non-root mount will fail later with EPERM, which is fine.)
    let result = Transaction::new(
        &[lower.as_path()],
        &base.join("upper2"),
        &base.join("work2"),
        &base.join("merged2"),
    );
    std::env::remove_var("CORTEX_ALLOW_TMPFS_UPPER");
    if let Err(e) = &result {
        assert!(
            !format!("{e:#}").contains("would go to RAM"),
            "override should bypass the tmpfs preflight refusal"
        );
    }
    let _ = fs::remove_dir_all(&base);
}
