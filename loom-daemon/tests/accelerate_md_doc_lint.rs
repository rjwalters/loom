//! Doc-lint for `defaults/.claude/commands/loom/accelerate.md` (#13663).
//!
//! CONTRACT: `/loom:accelerate` is session-only. It must never call the daemon
//! dispatch tool and must not install, restart or reconfigure a daemon or hook.
//! Absence of these tokens is the contract, so the assertions are negative
//! greps (no prose-existence pins).

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::fs;

const ACCELERATE_MD: &str = "../defaults/.claude/commands/loom/accelerate.md";

#[test]
fn accelerate_md_never_dispatches_through_the_daemon() {
    let body = fs::read_to_string(ACCELERATE_MD).expect("accelerate.md must exist");
    for forbidden in [
        "dispatch_sweep",
        "loom-daemon start",
        "loom-daemon-start",
        "loom-daemon stop",
        "loom-daemon install",
        "loom-daemon restart",
        "launchctl",
        "systemctl",
        "install-loom",
        "loom update",
    ] {
        assert!(
            !body.contains(forbidden),
            "accelerate.md must stay session-only but mentions `{forbidden}`"
        );
    }
}

#[test]
fn accelerate_md_pins_no_daemon_on_every_sweep_invocation() {
    let body = fs::read_to_string(ACCELERATE_MD).expect("accelerate.md must exist");
    for line in body.lines().filter(|l| l.contains("/loom:sweep all")) {
        if line.contains("--dry-run") || line.contains("--no-daemon") {
            assert!(
                line.contains("--no-daemon"),
                "every sweep invocation in accelerate.md must carry --no-daemon: {line}"
            );
        }
    }
    assert!(body.contains("--no-daemon"), "accelerate.md must pass --no-daemon");
}
