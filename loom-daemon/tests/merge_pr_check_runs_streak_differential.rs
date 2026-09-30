//! Differential test: the Rust port of `merge-pr.sh`'s persistent-404 streak
//! classification (#6389, an #8191 slice) must agree with the retired shell
//! logic, byte for byte, on a shared corpus (`defaults/docs/verification-
//! recipes.md` §6).
//!
//! # The frozen shell side
//!
//! The retired shell block is small enough to keep frozen inline (rather than
//! as a separate fixture file, like `merge-pr-checks-failure-retired.sh`):
//! given `attempt1_rc`, `attempt2_rc` and the running streak, it is exactly
//! the `[[ "$attempt1_rc" -eq NF && "$attempt2_rc" -eq NF ]]` gate that used
//! to live inside `_wait_for_checks_then_sync_merge`, before this slice moved
//! it to `loom-daemon merge-pr check-runs-streak`. It is ALSO, verbatim, the
//! `_iteration_verdict` mirror `test-merge-pr-check-runs-404-fallback.sh`
//! already carries as its own regression lock — this test exercises the same
//! rule against the REAL Rust binary rather than a second shell copy.
//!
//! # Coverage floor
//!
//! The test fails unless both verdicts (PROCEED and PENDING) were reached, and
//! at least one case exercises a streak reset (a confirmed 404 following an
//! unconfirmed failure).

use std::path::PathBuf;
use std::process::Command;

const THRESHOLD: u64 = 2;
const NOT_FOUND_RC: i32 = 44;

/// Run the frozen `_iteration_verdict` gate: `<streak_out> <verdict>`, verdict
/// in `success | still-pending | proceed-to-merge`. Mirrors
/// `test-merge-pr-check-runs-404-fallback.sh`'s own shell function verbatim.
fn run_frozen_shell(attempt1_rc: i32, attempt2_rc: i32, streak_in: u64) -> (u64, String) {
    let driver = format!(
        r#"set -euo pipefail
attempt1_rc={attempt1_rc}
attempt2_rc={attempt2_rc}
streak_in={streak_in}
threshold={THRESHOLD}
NF={NOT_FOUND_RC}

fetch_rc="$attempt1_rc"
[[ "$attempt1_rc" -ne 0 ]] && fetch_rc="$attempt2_rc"

if [[ "$fetch_rc" -eq 0 ]]; then
    echo "0 success"
    exit 0
fi

if [[ "$attempt1_rc" -eq "$NF" && "$attempt2_rc" -eq "$NF" ]]; then
    streak_out=$(( streak_in + 1 ))
else
    streak_out=0
fi

if [[ "$streak_out" -ge "$threshold" ]]; then
    echo "$streak_out proceed-to-merge"
else
    echo "$streak_out still-pending"
fi
"#
    );
    let out = Command::new("bash")
        .args(["-c", &driver])
        .output()
        .expect("bash ran the frozen shell side");
    assert!(
        out.status.success(),
        "frozen shell side failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let mut parts = stdout.splitn(2, ' ');
    let streak_out: u64 = parts.next().unwrap().parse().expect("streak is numeric");
    let verdict = parts.next().unwrap().to_string();
    (streak_out, verdict)
}

/// Run the ported subcommand exactly as the live `merge-pr.sh` invokes it
/// (only ever on the failure path — `attempt1_rc` and/or `attempt2_rc`
/// nonzero), and map its sentinel onto the frozen shell's vocabulary.
fn run_rust_cli(attempt1_rc: i32, attempt2_rc: i32, streak_in: u64) -> (u64, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_loom-daemon"))
        .args([
            "merge-pr",
            "check-runs-streak",
            "--attempt1-rc",
            &attempt1_rc.to_string(),
            "--attempt2-rc",
            &attempt2_rc.to_string(),
            "--streak",
            &streak_in.to_string(),
            "--threshold",
            &THRESHOLD.to_string(),
            "--not-found-rc",
            &NOT_FOUND_RC.to_string(),
        ])
        .output()
        .expect("loom-daemon ran");
    assert!(
        out.status.success(),
        "loom-daemon merge-pr check-runs-streak failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let mut fields = stdout.split(' ');
    assert_eq!(
        fields.next(),
        Some("LOOM-CHECK-RUNS-STREAK"),
        "unexpected sentinel in {stdout:?}"
    );
    let verdict = match fields.next() {
        Some("PROCEED") => "proceed-to-merge",
        Some("PENDING") => "still-pending",
        other => panic!("unrecognized verdict token {other:?} in {stdout:?}"),
    };
    let streak_out: u64 = fields.next().unwrap().parse().expect("streak is numeric");
    (streak_out, verdict.to_string())
}

#[test]
fn frozen_shell_and_rust_cli_agree_on_every_case() {
    // (attempt1_rc, attempt2_rc, streak_in) — the shell only ever calls the
    // ported verb once `fetch_rc != 0` (i.e. attempt1_rc != 0), so every case
    // here keeps that invariant; the "success" branch of the frozen gate is
    // exercised only to document that this slice deliberately never reaches
    // it (see the module docs on `check_runs_streak` for the fail-direction
    // note), not because the live CLI is ever called that way.
    let cases: &[(i32, i32, u64)] = &[
        // persistent 404: crosses the threshold on the second confirmed poll
        (NOT_FOUND_RC, NOT_FOUND_RC, 0),
        (NOT_FOUND_RC, NOT_FOUND_RC, 1),
        // transient 5xx, repeated: never crosses, streak stays at 0
        (1, 1, 0),
        (1, 1, 0),
        // a confirmed 404 immediately at the threshold boundary
        (NOT_FOUND_RC, NOT_FOUND_RC, 5),
        // mixed: attempt1 confirms 404, the retry is a DIFFERENT failure —
        // NOT confirmed, streak resets even though it was nonzero coming in
        (NOT_FOUND_RC, 1, 3),
        (1, NOT_FOUND_RC, 2),
        // both attempts a different transient failure than not-found
        (2, 2, 4),
        // one attempt already recovered (rc 0) alongside a 404 retry —
        // fetch_rc is 0 only when attempt1_rc is 0; every case here keeps
        // attempt1_rc nonzero (the live delegation invariant), so this is a
        // confirmed-404-adjacent shape instead: attempt1 not-found, attempt2
        // a totally different code
        (NOT_FOUND_RC, 500, 0),
    ];

    let mut saw_proceed = false;
    let mut saw_pending = false;
    let mut saw_reset_from_nonzero = false;
    for &(a1, a2, streak_in) in cases {
        let shell = run_frozen_shell(a1, a2, streak_in);
        let rust = run_rust_cli(a1, a2, streak_in);
        assert_eq!(
            shell, rust,
            "divergence on attempt1_rc={a1} attempt2_rc={a2} streak_in={streak_in}"
        );
        match shell.1.as_str() {
            "proceed-to-merge" => saw_proceed = true,
            "still-pending" => saw_pending = true,
            other => panic!("frozen shell returned an unexpected verdict {other:?}"),
        }
        if streak_in > 0 && shell.0 == 0 {
            saw_reset_from_nonzero = true;
        }
    }

    assert!(saw_proceed, "corpus never reached the PROCEED/proceed-to-merge verdict");
    assert!(saw_pending, "corpus never reached the PENDING/still-pending verdict");
    assert!(
        saw_reset_from_nonzero,
        "corpus never exercised a streak reset from a nonzero starting streak"
    );
}

/// `loom-daemon` must exist at the path the differential relies on — a
/// misconfigured test run that silently skipped the CLI side would report
/// false agreement.
#[test]
fn the_cli_binary_under_test_exists() {
    let path = PathBuf::from(env!("CARGO_BIN_EXE_loom-daemon"));
    assert!(
        path.exists(),
        "CARGO_BIN_EXE_loom-daemon did not resolve to a real file: {path:?}"
    );
}
