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
