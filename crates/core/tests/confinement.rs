//! Confinement is real: a command inside a transaction can do its job, but
//! cannot write to the host through a bind mount.
//!
//! Needs root (chroot + overlay + landlock) and a kernel with Landlock. Skips
//! cleanly otherwise, the same discipline as the other root tests.

#![cfg(target_os = "linux")]

use cortex_core::transaction::Transaction;
use std::fs;
use std::path::Path;
use std::process::Command;

fn is_root() -> bool {
    Command::new("id")
        .arg("-u")
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim() == "0")
        .unwrap_or(false)
}

fn has_landlock() -> bool {
    fs::read_to_string("/sys/kernel/security/lsm")
        .map(|s| s.contains("landlock"))
        .unwrap_or(false)
}

fn open(root: &Path) -> Transaction {
    let upper = root.join("upper");
    let work = root.join("work");
    let merged = root.join("merged");
    // Lower = the host root, exactly as production does, so the chrooted
    // /bin/sh and the rest of the system actually resolve. Writes still land
    // in the upper layer and are discarded when the transaction drops.
    let mut tx =
        Transaction::new(&[Path::new("/")], &upper, &work, &merged).expect("overlay mount failed");
    tx.bind_system_dirs().expect("bind system dirs");
    tx
}

/// A legitimate command still works under confinement: it can write to the
/// sandbox's own filesystem, which is what every real operation does.
#[test]
fn a_normal_write_inside_the_sandbox_succeeds() {
    if !is_root() {
        eprintln!("skipping a_normal_write_inside_the_sandbox_succeeds: requires root");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let tx = open(tmp.path());

    // Writing under /etc inside the sandbox is captured by the overlay and
    // must be allowed by the confinement.
    let out = tx
        .run_in_root("echo hello > /etc/cortex-confine-test && cat /etc/cortex-confine-test")
        .unwrap();
    assert!(
        out.status.success(),
        "a normal sandbox write must succeed under confinement; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "hello");
}

/// The escape that confinement exists to stop: a command in the sandbox
/// cannot write to a host tree bound in for functionality. On a kernel with
/// Landlock the write is denied; the host file is never touched.
#[test]
fn writing_to_a_bound_host_tree_is_blocked() {
    if !is_root() {
        eprintln!("skipping writing_to_a_bound_host_tree_is_blocked: requires root");
        return;
    }
    if !has_landlock() {
        eprintln!("skipping writing_to_a_bound_host_tree_is_blocked: no Landlock in this kernel");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let tx = open(tmp.path());

    // /run is bound in from the host. A command that tries to create a file
    // there is attempting to affect the host through the bind mount.
    let out = tx
        .run_in_root("echo pwned > /run/cortex-escape-attempt 2>&1")
        .unwrap();

    // The write must fail (landlock denies it), and crucially the host file
    // must not exist.
    assert!(
        !out.status.success(),
        "writing to a bound host tree must be denied by confinement"
    );
    assert!(
        !Path::new("/run/cortex-escape-attempt").exists(),
        "the escape attempt reached the host filesystem — confinement did not hold"
    );
}
