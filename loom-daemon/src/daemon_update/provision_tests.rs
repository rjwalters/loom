//! Tests for [`super::install_to`]: the write-then-rename install (#10708).
//! What it keeps and records (#10983) is tested in `provision/txn_tests.rs`.
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

/// What an install over an existing binary leaves beside it (#10983): the
/// binary, its record, and the binary it replaced. Nothing else, so a stray
/// temp file still shows up by name.
#[cfg(unix)]
fn upgraded() -> Vec<String> {
    vec![
        "loom-daemon".to_string(),
        "loom-daemon.install-state.json".to_string(),
        "loom-daemon.previous".to_string(),
    ]
}

/// The same for a first-ever install: there is nothing to keep.
#[cfg(unix)]
fn first_install() -> Vec<String> {
    vec![
        "loom-daemon".to_string(),
        "loom-daemon.install-state.json".to_string(),
    ]
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
    assert_eq!(entries(dir.path()), upgraded());
    assert_eq!(std::fs::read(dir.path().join("loom-daemon.previous")).unwrap(), b"old binary");
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
    assert_eq!(entries(dir.path()), first_install());
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

/// The temp file is written in full and then the install cannot finish
/// (here: `dest` is a non-empty directory, which can be neither kept as a
/// previous binary nor renamed over). The temp file must be removed and
/// `dest` left exactly as it was.
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
    assert_eq!(entries(dir.path()), upgraded());
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
    // Which install ran last decides whether a previous binary was kept; what
    // must never survive is a staging file.
    let residue: Vec<String> = entries(dir.path())
        .into_iter()
        .filter(|n| n.starts_with('.'))
        .collect();
    assert_eq!(residue, Vec::<String>::new());
}

/// A bare relative destination (no directory component) stages its temp
/// file in the current directory's sense of "same directory", i.e. `.`.
#[test]
fn temp_dir_of_a_bare_file_name_is_dot() {
    assert_eq!(super::staging_dir(Path::new("loom-daemon")), Path::new("."));
    assert_eq!(super::staging_dir(Path::new("/opt/bin/loom-daemon")), Path::new("/opt/bin"));
}

#[cfg(unix)]
#[test]
fn a_symlinked_dest_is_replaced_by_a_regular_file_not_followed() {
    let dir = tmpdir();
    let staging = tmpdir();
    let real = staging.path().join("real");
    let dest = dir.path().join("loom-daemon");
    let fresh = staging.path().join("fresh");
    write(&real, b"real old");
    set_mode(&real, 0o600);
    std::os::unix::fs::symlink(&real, &dest).unwrap();
    write(&fresh, b"new bytes");

    assert!(install_to(&fresh, &dest));

    let md = std::fs::symlink_metadata(&dest).unwrap();
    assert!(md.file_type().is_file(), "dest must be a regular file, not a symlink");
    assert_eq!(std::fs::read(&dest).unwrap(), b"new bytes");
    assert_eq!(mode(&dest), 0o755);
    assert_eq!(std::fs::read(&real).unwrap(), b"real old");
    assert_eq!(mode(&real), 0o600);
}

#[cfg(unix)]
fn age(p: &Path, secs: u64) {
    let t = std::time::SystemTime::now() - std::time::Duration::from_secs(secs);
    std::fs::File::open(p).unwrap().set_modified(t).unwrap();
}

#[cfg(unix)]
const STALE_NAME: &str = ".loom-daemon.loom-install.123.4.567";

#[cfg(unix)]
fn fresh_source() -> (tempfile::TempDir, std::path::PathBuf) {
    let s = tmpdir();
    let f = s.path().join("fresh");
    write(&f, b"new");
    (s, f)
}

#[cfg(unix)]
#[test]
fn a_stale_owned_staging_file_is_swept_by_the_next_install() {
    let dir = tmpdir();
    let (_s, fresh) = fresh_source();
    let dest = dir.path().join("loom-daemon");
    let stale = dir.path().join(STALE_NAME);
    write(&stale, b"partial");
    age(&stale, 2 * 3600);

    assert!(install_to(&fresh, &dest));

    assert_eq!(entries(dir.path()), first_install());
}

/// Age a symlink itself (not its target), so only the file-type check can
/// keep the sweep away from it.
#[cfg(unix)]
fn age_link(p: &Path, secs: u64) {
    use std::os::unix::ffi::OsStrExt;
    let then = (std::time::SystemTime::now() - std::time::Duration::from_secs(secs))
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let tv = libc::timeval {
        tv_sec: libc::time_t::try_from(then).unwrap(),
        tv_usec: 0,
    };
    let c = std::ffi::CString::new(p.as_os_str().as_bytes()).unwrap();
    // SAFETY: `c` is a valid NUL-terminated path and `times` points at two
    // initialised timevals for the duration of the call.
    let rc = unsafe { libc::lutimes(c.as_ptr(), [tv, tv].as_ptr()) };
    assert_eq!(rc, 0, "lutimes failed: {}", std::io::Error::last_os_error());
    let md = std::fs::symlink_metadata(p).unwrap();
    assert!(md.modified().unwrap().elapsed().unwrap().as_secs() >= secs - 60);
}

#[cfg(unix)]
#[test]
fn the_sweep_leaves_everything_that_does_not_qualify() {
    let dir = tmpdir();
    let (_s, fresh) = fresh_source();
    let dest = dir.path().join("loom-daemon");
    let old = 2 * 3600;

    // The one entry that qualifies, so the selection below is non-empty.
    let stale = dir.path().join(STALE_NAME);
    write(&stale, b"partial");
    age(&stale, old);
    // Fresh matching file (a concurrent install's live temp file).
    let recent = dir.path().join(".loom-daemon.loom-install.1.1.1");
    write(&recent, b"live");
    // Stale file for a different binary.
    let other = dir.path().join(".other.loom-install.1.1.1");
    write(&other, b"x");
    age(&other, old);
    // Stale file with a non-matching suffix.
    let suffix = dir.path().join(".loom-daemon.loom-install.abc");
    write(&suffix, b"x");
    age(&suffix, old);
    let suffix2 = dir.path().join(".loom-daemon.loom-install.1.2.3.4");
    write(&suffix2, b"x");
    age(&suffix2, old);
    // Stale matching-named directory: only the file-type check excludes it.
    let adir = dir.path().join(".loom-daemon.loom-install.2.2.2");
    std::fs::create_dir(&adir).unwrap();
    age(&adir, old);
    // Stale matching-named symlink (the link itself is aged, so only the
    // file-type check excludes it; removing that check unlinks it).
    let target = _s.path().join("target");
    write(&target, b"t");
    let link = dir.path().join(".loom-daemon.loom-install.3.3.3");
    std::os::unix::fs::symlink(&target, &link).unwrap();
    age_link(&link, old);

    // The selection is exactly the one stale regular file: the aged
    // directory and aged symlink are rejected on file type alone.
    assert_eq!(super::stale_staging_entries(dir.path(), &dest), vec![stale.clone()]);

    assert!(install_to(&fresh, &dest));

    assert_eq!(
        entries(dir.path()),
        vec![
            ".loom-daemon.loom-install.1.1.1".to_string(),
            ".loom-daemon.loom-install.2.2.2".to_string(),
            ".loom-daemon.loom-install.3.3.3".to_string(),
            ".loom-daemon.loom-install.abc".to_string(),
            ".loom-daemon.loom-install.1.2.3.4".to_string(),
            ".other.loom-install.1.1.1".to_string(),
            "loom-daemon".to_string(),
            "loom-daemon.install-state.json".to_string(),
        ]
        .into_iter()
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>()
    );
    assert!(std::fs::symlink_metadata(&link)
        .unwrap()
        .file_type()
        .is_symlink());
    assert!(target.exists());
}

#[cfg(unix)]
#[test]
fn a_sweep_failure_does_not_change_the_install_result() {
    // A matching-named stale entry the sweep cannot remove does not change
    // the install's result. A directory is used because it is the one such
    // entry a test can build portably; it is rejected by the file-type check
    // before `remove_file` is reached. A real `remove_file` error is ignored
    // by construction (only `.is_ok()` is counted), and a non-removable
    // regular file cannot be made here without also making the staging
    // directory unwritable, which would fail the install itself.
    let dir = tmpdir();
    let (_s, fresh) = fresh_source();
    let dest = dir.path().join("loom-daemon");
    let adir = dir.path().join(STALE_NAME);
    std::fs::create_dir(&adir).unwrap();
    age(&adir, 2 * 3600);

    assert!(install_to(&fresh, &dest));
    assert_eq!(std::fs::read(&dest).unwrap(), b"new");
    assert!(adir.is_dir());
}

#[cfg(unix)]
#[test]
fn staging_names_are_matched_on_raw_bytes() {
    use super::is_staging_name;
    assert!(is_staging_name(b".loom-daemon.loom-install.1.22.333", b"loom-daemon"));
    assert!(!is_staging_name(b"loom-daemon", b"loom-daemon"));
    assert!(!is_staging_name(b".loom-daemon.loom-install.1.2", b"loom-daemon"));
    assert!(!is_staging_name(b".loom-daemon.loom-install.1..2", b"loom-daemon"));
    assert!(!is_staging_name(b".a.b.loom-install.1.2.3", b"a"));
    // Two non-UTF-8 basenames differing only in invalid bytes: a lossy
    // conversion maps both to U+FFFD; byte matching keeps them apart.
    assert!(is_staging_name(b".x\xff.loom-install.1.2.3", b"x\xff"));
    assert!(!is_staging_name(b".x\xfe.loom-install.1.2.3", b"x\xff"));
    assert!(!is_staging_name(b".x\xff.loom-install.1.2.3", b"x\xfe"));
}

/// The machine-level path tells the script which `loom-daemon` performs its
/// `install-binary` calls: this running binary, which is known to execute on
/// this host (#10983). An operator's own pin is left alone.
#[test]
fn the_provision_command_names_the_running_binary_as_the_install_helper() {
    let cmd = super::provision_command(
        Path::new("/x/provision-daemon.sh"),
        Path::new("/x/new"),
        Path::new("/x/repo"),
        Path::new("/x/sink"),
    );
    let set = cmd
        .get_envs()
        .find(|(k, _)| *k == std::ffi::OsStr::new(super::INSTALL_HELPER_ENV))
        .and_then(|(_, v)| v.map(std::path::PathBuf::from));
    if std::env::var_os(super::INSTALL_HELPER_ENV).is_some() {
        assert_eq!(set, None, "an ambient pin must not be overridden");
    } else {
        assert_eq!(set, super::selfrepl::running_binary());
        assert!(set.is_some(), "a test binary always knows its own path");
    }
    let args: Vec<_> = cmd.get_args().collect();
    assert_eq!(args.len(), 7);
    assert_eq!(args[0], "-c");
    assert_eq!(args[3], "/x/provision-daemon.sh");
    assert_eq!(args[6], "/x/sink");
}
