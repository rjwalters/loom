//! Tests for [`super::install_to`]: the write-then-rename install (#10708).
//!
//! Every test works in its own tempdir; none touches a real daemon
//! destination.

use std::path::Path;

use super::install_to;

fn tmpdir() -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix("loom-provision-")
        .tempdir()
        .unwrap()
}

/// Every entry in `dir`, sorted, so a stray temp file shows up by name.
fn entries(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

#[cfg(unix)]
fn mode(p: &Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p).unwrap().permissions().mode() & 0o7777
}

#[cfg(unix)]
fn set_mode(p: &Path, m: u32) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(p, std::fs::Permissions::from_mode(m)).unwrap();
}

/// Permission checks are bypassed for root, so the read-only-directory
/// injection proves nothing there.
#[cfg(unix)]
fn running_as_root() -> bool {
    // SAFETY: geteuid has no preconditions and cannot fail.
    unsafe { libc::geteuid() == 0 }
}

fn write(p: &Path, body: &[u8]) {
    std::fs::write(p, body).unwrap();
}

#[cfg(unix)]
#[test]
fn installs_over_an_existing_file_with_mode_755_and_no_residue() {
    let dir = tmpdir();
    let staging = tmpdir();
    let dest = dir.path().join("loom-daemon");
    let fresh = staging.path().join("fresh");
    write(&dest, b"old binary");
    set_mode(&dest, 0o600);
    write(&fresh, b"new binary, longer than the old one");
    set_mode(&fresh, 0o644);

    assert!(install_to(&fresh, &dest));

    assert_eq!(std::fs::read(&dest).unwrap(), b"new binary, longer than the old one");
    assert_eq!(mode(&dest), 0o755);
    assert_eq!(entries(dir.path()), vec!["loom-daemon".to_string()]);
    // The source is copied, not moved.
    assert_eq!(std::fs::read(&fresh).unwrap(), b"new binary, longer than the old one");
}

#[cfg(unix)]
#[test]
fn installs_when_the_destination_does_not_exist_yet() {
    let dir = tmpdir();
    let staging = tmpdir();
    let dest = dir.path().join("loom-daemon");
    let fresh = staging.path().join("fresh");
    write(&fresh, b"first install");

    assert!(install_to(&fresh, &dest));

    assert_eq!(std::fs::read(&dest).unwrap(), b"first install");
    assert_eq!(mode(&dest), 0o755);
    assert_eq!(entries(dir.path()), vec!["loom-daemon".to_string()]);
}

/// The destination is replaced by a NEW inode (rename of the staged file),
/// and that inode is the staged temp file itself, not a rewrite of the old
/// one. An open handle on the old inode keeps reading the old bytes, the way
/// a process executing it keeps running.
#[cfg(unix)]
#[test]
fn the_old_inode_survives_untouched_for_an_open_reader() {
    use std::io::Read;
    use std::os::unix::fs::MetadataExt;

    let dir = tmpdir();
    let dest = dir.path().join("loom-daemon");
    let fresh = dir.path().join("fresh");
    write(&dest, b"old bytes");
    write(&fresh, b"new bytes!");
    let before = std::fs::metadata(&dest).unwrap().ino();
    let mut held = std::fs::File::open(&dest).unwrap();

    assert!(install_to(&fresh, &dest));

    assert_ne!(before, std::fs::metadata(&dest).unwrap().ino());
    let mut old = String::new();
    held.read_to_string(&mut old).unwrap();
    assert_eq!(old, "old bytes", "the old inode was rewritten in place");
    assert_eq!(std::fs::read(&dest).unwrap(), b"new bytes!");
}

/// A missing source fails before anything is created: `dest` is
/// byte-identical and no temp file is left behind.
#[cfg(unix)]
#[test]
fn a_missing_source_leaves_dest_intact_and_no_temp_file() {
    let dir = tmpdir();
    let dest = dir.path().join("loom-daemon");
    write(&dest, b"previous good binary");
    set_mode(&dest, 0o755);

    assert!(!install_to(&dir.path().join("does-not-exist"), &dest));

    assert_eq!(std::fs::read(&dest).unwrap(), b"previous good binary");
    assert_eq!(mode(&dest), 0o755);
    assert_eq!(entries(dir.path()), vec!["loom-daemon".to_string()]);
}

/// A read-only directory with a still-writable `dest`: the staged temp file
/// cannot be created, so the install must FAIL. It must not fall back to
/// rewriting `dest` in place (which `cp -f` would happily do here), because
/// an in-place rewrite is the partial-binary hazard this function exists to
/// rule out.
#[cfg(unix)]
#[test]
fn a_read_only_directory_fails_without_touching_dest() {
    if running_as_root() {
        return;
    }
    let dir = tmpdir();
    let staging = tmpdir();
    let dest = dir.path().join("loom-daemon");
    let fresh = staging.path().join("fresh");
    write(&dest, b"previous good binary");
    set_mode(&dest, 0o755);
    write(&fresh, b"replacement");
    set_mode(dir.path(), 0o555);

    let ok = install_to(&fresh, &dest);

    set_mode(dir.path(), 0o755);
    assert!(!ok, "install must fail rather than rewrite dest in place");
    assert_eq!(std::fs::read(&dest).unwrap(), b"previous good binary");
    assert_eq!(entries(dir.path()), vec!["loom-daemon".to_string()]);
}

/// The temp file is written in full and then the rename fails (here: `dest`
/// is a non-empty directory). The temp file must be removed and `dest` left
/// exactly as it was.
#[cfg(unix)]
#[test]
fn a_failed_rename_removes_the_temp_file() {
    let dir = tmpdir();
    let staging = tmpdir();
    let dest = dir.path().join("loom-daemon");
    std::fs::create_dir(&dest).unwrap();
    write(&dest.join("keep"), b"inside");
    let fresh = staging.path().join("fresh");
    write(&fresh, b"replacement");

    assert!(!install_to(&fresh, &dest));

    assert!(dest.is_dir());
    assert_eq!(std::fs::read(dest.join("keep")).unwrap(), b"inside");
    assert_eq!(entries(dir.path()), vec!["loom-daemon".to_string()]);
}

/// Env var that turns [`sleeper_helper`] into a long sleep when this test
/// binary is re-executed as the "running daemon" below.
const SLEEPER_ENV: &str = "LOOM_PROVISION_TEST_SLEEPER";

/// Not a real test: a no-op normally, and a 30s sleep when re-executed by
/// [`replaces_a_currently_executing_binary`] with [`SLEEPER_ENV`] set.
#[test]
fn sleeper_helper() {
    if std::env::var_os(SLEEPER_ENV).is_some() {
        std::thread::sleep(std::time::Duration::from_secs(30));
    }
}

/// Replacing a binary that is executing right now. A copy of THIS test
/// binary is started from `dest` (sleeping in [`sleeper_helper`]) and then
/// replaced. The install succeeds, the running process survives, and the
/// path now holds the new bytes.
///
/// The test binary is used rather than a copy of `/bin/sleep`: on macOS a
/// copied Apple platform binary (arm64e) is killed at launch wherever it
/// lives, which would make this test fail for reasons unrelated to the
/// install. On macOS this is also the sharp case: rewriting a running,
/// code-signed image in place gets the process killed by the kernel.
#[cfg(unix)]
#[test]
fn replaces_a_currently_executing_binary() {
    let me = std::env::current_exe().unwrap();
    let dir = tmpdir();
    let dest = dir.path().join("loom-daemon");
    std::fs::copy(&me, &dest).unwrap();
    set_mode(&dest, 0o755);

    // A sibling test thread forking while our copy's write fd was open can
    // make the first exec fail with ETXTBSY; that is a test-harness race, not
    // the property under test, so retry briefly.
    let mut child = None;
    for _ in 0..50 {
        let spawned = std::process::Command::new(&dest)
            .args(["--exact", "daemon_update::provision::tests::sleeper_helper"])
            .env(SLEEPER_ENV, "1")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();
        match spawned {
            Ok(c) => {
                child = Some(c);
                break;
            }
            Err(e) if e.raw_os_error() == Some(libc::ETXTBSY) => {
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            Err(e) => panic!("could not run the copied test binary: {e}"),
        }
    }
    let mut child = child.expect("copied test binary stayed ETXTBSY");
    // Let it get properly under way (past exec, into the sleep).
    std::thread::sleep(std::time::Duration::from_millis(500));
    assert!(
        child.try_wait().unwrap().is_none(),
        "the copied test binary exited before the install; the test premise is broken"
    );

    let staging = tmpdir();
    let fresh = staging.path().join("fresh");
    write(&fresh, b"#!/bin/sh\necho new\n");
    let ok = install_to(&fresh, &dest);
    // Give a kernel-side kill (macOS code signing) time to land.
    std::thread::sleep(std::time::Duration::from_millis(500));
    let still_running = child.try_wait().unwrap().is_none();
    let _ = child.kill();
    let _ = child.wait();

    assert!(ok, "replacing an executing binary must succeed");
    assert!(still_running, "the running process must survive the replacement");
    assert_eq!(std::fs::read(&dest).unwrap(), b"#!/bin/sh\necho new\n");
    assert_eq!(mode(&dest), 0o755);
    assert_eq!(entries(dir.path()), vec!["loom-daemon".to_string()]);
}

/// Concurrent installs onto the same destination each stage their own
/// uniquely named temp file (`create_new`), so neither clobbers the other's
/// staging: both succeed, `dest` is one complete payload, nothing is left
/// behind.
#[cfg(unix)]
#[test]
fn concurrent_installs_never_share_a_temp_file() {
    let dir = tmpdir();
    let staging = tmpdir();
    let dest = dir.path().join("loom-daemon");
    let a = staging.path().join("a");
    let b = staging.path().join("b");
    let body_a = vec![b'a'; 256 * 1024];
    let body_b = vec![b'b'; 256 * 1024];
    write(&a, &body_a);
    write(&b, &body_b);

    let handles: Vec<_> = (0..8)
        .map(|i| {
            let src = if i % 2 == 0 { a.clone() } else { b.clone() };
            let dest = dest.clone();
            std::thread::spawn(move || install_to(&src, &dest))
        })
        .collect();
    for h in handles {
        assert!(h.join().unwrap());
    }

    let got = std::fs::read(&dest).unwrap();
    assert!(got == body_a || got == body_b, "dest is a mix of two installs");
    assert_eq!(entries(dir.path()), vec!["loom-daemon".to_string()]);
}

/// A bare relative destination (no directory component) stages its temp
/// file in the current directory's sense of "same directory", i.e. `.`.
#[test]
fn temp_dir_of_a_bare_file_name_is_dot() {
    assert_eq!(super::staging_dir(Path::new("loom-daemon")), Path::new("."));
    assert_eq!(super::staging_dir(Path::new("/opt/bin/loom-daemon")), Path::new("/opt/bin"));
}
