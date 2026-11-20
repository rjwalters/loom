//! The issue lease chain on a tools-only host, against the real `loom-tools`
//! (#14488, design `docs/design/loom-tools-cli-boundary.md` §15).
//!
//! `defaults/scripts/tests/test-loom-tools-lease.sh` owns the scenarios: it puts
//! only `loom-tools` and a controlled stub forge on PATH (no `loom-daemon`) and
//! runs the real publish, deferred retry, renew, release, co-occupancy and
//! fence paths. It prints SKIP and exits 0 when no `loom-tools` is built, which
//! is what a bare shell CI leg has; this test hands it the binary cargo just
//! built so the chain is exercised on every `cargo test`.

use std::path::PathBuf;
use std::process::Command;

#[test]
fn the_issue_lease_chain_runs_on_a_tools_only_host() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..");
    let out = Command::new("bash")
        .arg(root.join("defaults/scripts/tests/test-loom-tools-lease.sh"))
        .env("LOOM_TOOLS_TEST_BIN", env!("CARGO_BIN_EXE_loom-tools"))
        .output()
        .expect("run test-loom-tools-lease.sh");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{stdout}\n{}", String::from_utf8_lossy(&out.stderr));
    assert!(
        !stdout.contains("SKIP"),
        "the suite must run against the built binary:\n{stdout}"
    );
    assert!(stdout.contains("ALL PASSED"), "{stdout}");
}
