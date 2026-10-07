// #10661 N2: on an operator `accounts session` command, the first SIGINT is
// recorded (and forwarded to `docker`); a second one must kill the command,
// so an operator can always force-quit. The handler is process-global, so
// the check runs in a child copy of this test binary.
#![allow(clippy::expect_used)]
#![allow(clippy::unwrap_used)]

use loom_daemon::tokens_pool::operator_interrupt;
use std::os::unix::process::ExitStatusExt;
use std::process::Command;

const CHILD_ENV: &str = "LOOM_TEST_OPERATOR_INTERRUPT_CHILD";
const TEST: &str = "a_second_signal_kills_the_operator_command";

#[test]
fn a_second_signal_kills_the_operator_command() {
    if std::env::var_os(CHILD_ENV).is_some() {
        operator_interrupt::install().unwrap();
        // SAFETY: raising a signal at this process, whose handler is installed.
        unsafe { libc::raise(libc::SIGINT) };
        assert_eq!(operator_interrupt::pending(), Some(libc::SIGINT), "the first is recorded");
        println!("survived-first");
        // SAFETY: as above; this one must terminate the process.
        unsafe { libc::raise(libc::SIGINT) };
        println!("survived-second");
        std::process::exit(0);
    }
    let out = Command::new(std::env::current_exe().unwrap())
        .args([TEST, "--exact", "--nocapture", "--test-threads=1"])
        .env(CHILD_ENV, "1")
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("survived-first"), "{stdout}");
    assert!(!stdout.contains("survived-second"), "{stdout}");
    assert_eq!(out.status.signal(), Some(libc::SIGINT), "{:?}", out.status);
}

const GROUP_CHILD_ENV: &str = "LOOM_TEST_OPERATOR_INTERRUPT_GROUP_CHILD";
const GROUP_TEST: &str = "a_second_signal_also_kills_a_docker_group_that_ignores_the_first";

/// The `docker` child may ignore the forwarded SIGINT (or not have been sent
/// it yet). The second signal must not leave it running behind the CLI.
#[test]
fn a_second_signal_also_kills_a_docker_group_that_ignores_the_first() {
    let pidfile = std::env::var_os("LOOM_TEST_GROUP_PIDFILE");
    if std::env::var_os(GROUP_CHILD_ENV).is_some() {
        let pidfile = std::path::PathBuf::from(pidfile.unwrap());
        operator_interrupt::install().unwrap();
        let watch = pidfile.clone();
        std::thread::spawn(move || {
            // Once the "docker" child is up: one Ctrl-C (forwarded, ignored),
            // then a second one inside the 10 s forward grace.
            while !watch.exists() {
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            // SAFETY: raising a signal at this process, whose handler is installed.
            unsafe { libc::raise(libc::SIGINT) };
            std::thread::sleep(std::time::Duration::from_millis(500));
            // SAFETY: as above; this one must kill the group, then the process.
            unsafe { libc::raise(libc::SIGINT) };
        });
        let script = format!("trap '' INT; echo $$ > '{}'; exec sleep 60", pidfile.display());
        let _ = loom_daemon::tokens_pool::docker_cli::run_bounded(
            "sh",
            &["-c", &script],
            std::time::Duration::from_secs(120),
        );
        println!("survived-second");
        std::process::exit(0);
    }
    let dir = tempfile::tempdir().unwrap();
    let pidfile = dir.path().join("group.pid");
    let out = Command::new(std::env::current_exe().unwrap())
        .args([GROUP_TEST, "--exact", "--nocapture", "--test-threads=1"])
        .env(GROUP_CHILD_ENV, "1")
        .env("LOOM_TEST_GROUP_PIDFILE", &pidfile)
        .output()
        .unwrap();
    assert_eq!(out.status.signal(), Some(libc::SIGINT), "{:?}", out.status);
    assert!(!String::from_utf8_lossy(&out.stdout).contains("survived-second"));
    let group: i32 = std::fs::read_to_string(&pidfile)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    // Gone (an orphaned zombie is reaped by init within moments).
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    // SAFETY: signal 0 only checks for existence of the group.
    while unsafe { libc::kill(-group, 0) } == 0 {
        assert!(
            std::time::Instant::now() < deadline,
            "the docker child group {group} is still running after the second signal"
        );
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}
