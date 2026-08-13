//! Regression test for issue #6127: `loom-daemon clean --deep` unlinked the
//! backing file of a **currently running** service binary.
//!
//! The scheduled fleet clean runs
//! `loom-daemon clean --workspace <repo> --deep --safe -y` every few hours.
//! `--deep` removes `target/` and `node_modules/` from the primary checkout
//! wholesale, and `--safe` does not narrow that (#4890 gates only worktrees and
//! branches). A service whose launchd/systemd `program` is
//! `<repo>/target/release/<bin>` therefore has its binary deleted out from
//! under it. Nothing fails at delete time — the kernel keeps the running
//! process alive on the unlinked inode (`/proc/<pid>/exe` reads
//! `… (deleted)`) — so the damage is invisible until the next restart, where
//! the supervisor cannot exec a missing path.
//!
//! The confirmed repro on `loom-worker-1`: `safehoused` ran for ~3 days on a
//! deleted `target/release/safehoused` after a routine 4-hourly pass.
//!
//! `deep_clean.rs`'s pre-existing `exe_is_inside_artifacts` gate only ever
//! compared against `std::env::current_exe()` — it protects loom-daemon's *own*
//! binary and nothing else, and the CLI `--deep` path (the one the incident
//! logs show running) never consulted it at all. So the test below deliberately
//! runs a **different** process out of `target/release/`.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

/// Initialize a minimal git repo plus the `.loom/` marker directory
/// `resolve_repo_root` requires to treat the directory as a Loom repo root.
fn init_repo(dir: &Path) {
    let run = |args: &[&str]| {
        let status = Command::new("git")
            .args(args)
            .current_dir(dir)
            .status()
            .expect("git command must spawn");
        assert!(status.success(), "git {args:?} failed");
    };
    run(&["init", "--quiet"]);
    run(&["config", "user.email", "test@example.com"]);
    run(&["config", "user.name", "Test"]);
    run(&["commit", "--allow-empty", "--quiet", "-m", "initial"]);
    std::fs::create_dir_all(dir.join(".loom")).unwrap();
}

/// A long-lived system binary to stand in for a service executable. Copying a
/// real one (rather than compiling a fixture) keeps the test fast and makes the
/// spawned process indistinguishable, to the detector, from a real service.
fn host_sleep_binary() -> PathBuf {
    for candidate in ["/bin/sleep", "/usr/bin/sleep"] {
        let path = PathBuf::from(candidate);
        if path.is_file() {
            return path;
        }
    }
    panic!("no `sleep` binary found — this test needs one to stand in for a service");
}

/// Copy `sleep` into `<repo>/target/release/<name>` and run it, so a live
/// process's executable image lives inside the directory `--deep` deletes.
fn spawn_service_from_target(repo: &Path, name: &str) -> (Child, PathBuf) {
    let release = repo.join("target").join("release");
    std::fs::create_dir_all(&release).unwrap();
    let program = release.join(name);
    std::fs::copy(host_sleep_binary(), &program).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    // Some bulk so a partially-successful removal is still detectable.
    std::fs::write(release.join("filler.bin"), vec![0u8; 4096]).unwrap();

    // Retried on ETXTBSY: a concurrent test thread forking while our write fd
    // was briefly open leaves the child holding it, and Linux then refuses to
    // exec the file. A harness race, not a property of the code under test.
    for _ in 0..100 {
        match Command::new(&program)
            .arg("300")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        {
            Ok(child) => {
                // Let the kernel publish the exec'd image (/proc/<pid>/exe).
                std::thread::sleep(std::time::Duration::from_millis(250));
                return (child, program);
            }
            Err(e) if e.kind() == std::io::ErrorKind::ExecutableFileBusy => {
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            Err(e) => panic!("the stand-in service must spawn: {e}"),
        }
    }
    panic!("the stand-in service never became executable");
}

fn run_clean_deep(repo: &Path) -> (bool, String) {
    let output = Command::new(env!("CARGO_BIN_EXE_loom-daemon"))
        .args([
            "clean",
            "--deep",
            "--worktrees-only",
            "--force",
            "--workspace",
            repo.to_str().unwrap(),
        ])
        .stdin(Stdio::null())
        .output()
        .expect("loom-daemon clean must spawn");
    let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&output.stderr));
    (output.status.success(), text)
}

/// The issue's core acceptance criterion: a binary running as a **different**
/// process than loom-daemon itself, out of `<repo>/target/release/`, survives
/// `clean --deep`.
#[test]
fn deep_clean_does_not_delete_a_running_services_binary() {
    let dir = tempfile::tempdir().unwrap();
    init_repo(dir.path());
    let (mut service, program) = spawn_service_from_target(dir.path(), "loom-test-service");

    let (success, output) = run_clean_deep(dir.path());

    let program_survived = program.is_file();
    let target_survived = dir.path().join("target").is_dir();
    let _ = service.kill();
    let _ = service.wait();

    assert!(success, "clean must not error out; output:\n{output}");
    assert!(
        program_survived,
        "clean --deep unlinked the backing file of a live process ({}) — the exact \
         silent-outage hazard in #6127; output:\n{output}",
        program.display()
    );
    assert!(
        target_survived,
        "the whole build-artifact directory must be left alone while it backs a live \
         process; output:\n{output}"
    );
    assert!(
        output.contains("live process") || output.contains("running program"),
        "the operator must be told why the reclaim was skipped, not left guessing why \
         disk was not freed; output:\n{output}"
    );
}

/// The no-regression half: with nothing running out of `target/`, `--deep`
/// still reclaims exactly as before. Runs with `--safe` to pin the documented
/// meaning of that flag — it narrows worktrees and branches only, never
/// build-artifact removal (#4890).
#[test]
fn deep_clean_safe_still_reclaims_when_nothing_is_running_from_target() {
    let dir = tempfile::tempdir().unwrap();
    init_repo(dir.path());
    let release = dir.path().join("target").join("release");
    std::fs::create_dir_all(&release).unwrap();
    std::fs::write(release.join("filler.bin"), vec![0u8; 4096]).unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_loom-daemon"))
        .args([
            "clean",
            "--deep",
            "--safe",
            "--worktrees-only",
            "--force",
            "--workspace",
            dir.path().to_str().unwrap(),
        ])
        .stdin(Stdio::null())
        .output()
        .expect("loom-daemon clean must spawn");
    let stdout = String::from_utf8_lossy(&output.stdout);

    assert!(output.status.success(), "clean must succeed; stdout:\n{stdout}");
    assert!(
        !dir.path().join("target").exists(),
        "the normal reclaim path must be unchanged when nothing is executing from \
         target/; stdout:\n{stdout}"
    );
    assert!(
        stdout.contains("Removed target/"),
        "the reclaim must still be reported; stdout:\n{stdout}"
    );
}
