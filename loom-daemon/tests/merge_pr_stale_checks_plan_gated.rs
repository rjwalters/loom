//! `loom-daemon merge-pr stale-checks` against a forge that gates rulesets
//! behind the repository's plan (#8844).
//!
//! # What broke
//!
//! On a PRIVATE repository owned by a GitHub Free account or org,
//! `GET /repos/{nwo}/rules/branches/{branch}` answers
//! `HTTP 403: Upgrade to GitHub Pro or make this repository public to enable
//! this feature.` The #8248 freshness guard treated that like any other lookup
//! failure and failed closed — so `merge-pr.sh` refused **every** merge on such
//! a repo, with no flag that helped (`--allow-unapproved` and
//! `--redate-stale-checks` address different conditions) and only a
//! `LOOM_DAEMON_BIN` shim as an escape.
//!
//! # The contract pinned here
//!
//! | rulesets endpoint says | required contexts | exit | stdout |
//! |---|---|---|---|
//! | plan-gated 403 | none from that source | 0 | the CLEAN sentinel (+ a `Warning:` on stderr) |
//! | any other 403/401/404/5xx/network error | undeterminable | 2 | the fail-closed refusal |
//!
//! The split is a *message* match, not a status-code match: a plan gate and a
//! missing token scope are both 403, and only the first one means "this
//! repository cannot have required checks".
//!
//! These run the REAL binary against a mock `gh` on `LOOM_GH_BIN` (the seam
//! `fetch::gh_bin` provides), because the bug was in the live fetch path — the
//! `--from-stdin` seam the rest of the suite drives never touches it.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// A mock `gh` answering the three `api` calls `live_inputs` makes, emitting
/// POST-`--jq` output (real `gh` applies the filter itself):
///
/// - `repos/…/commits/<base>` → `<sha>\t<committer date>`
/// - `repos/…/rules/branches/<base>` → one context per line, or the failure
///   named by `MOCK_RULES_RC` / `MOCK_RULES_STDERR`
/// - `graphql` → one classic context per line
/// - `repos/…/commits/<sha>/check-runs` → the projected JSON array
fn write_mock_gh(dir: &Path) -> PathBuf {
    let path = dir.join("gh");
    std::fs::write(
        &path,
        r#"#!/bin/sh
[ "$1" = "api" ] || exit 1
case "$2" in
  graphql)
    [ "${MOCK_CLASSIC_RC:-0}" = 0 ] || { printf '%s\n' "${MOCK_CLASSIC_STDERR:-}" >&2; exit "$MOCK_CLASSIC_RC"; }
    printf '%s' "${MOCK_CLASSIC_OUT:-}"
    ;;
  *rules/branches/*)
    [ "${MOCK_RULES_RC:-0}" = 0 ] || { printf '%s\n' "${MOCK_RULES_STDERR:-}" >&2; exit "$MOCK_RULES_RC"; }
    printf '%s' "${MOCK_RULES_OUT:-}"
    ;;
  *check-runs*)
    printf '%s' "${MOCK_RUNS_OUT:-[]}"
    ;;
  *commits/*)
    printf '%s\t%s\n' "${MOCK_TIP_SHA:-abc123}" "${MOCK_TIP_DATE:-2026-09-18T11:45:21Z}"
    ;;
  *) exit 1 ;;
esac
"#,
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&path).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&path, perms).unwrap();
    }
    path
}

/// The check-runs rollup: one green `File Size Ratchet` that started 22h
/// BEFORE the base tip — the #8248 incident's own evidence. Using the stale
/// payload throughout is deliberate: if the plan-gate path ever started
/// reporting phantom required contexts, these tests would go red instead of
/// passing for the wrong reason.
const STALE_GREEN_RUNS: &str = r#"[{"name":"File Size Ratchet","status":"completed","conclusion":"success","started_at":"2026-09-17T22:54:20Z"}]"#;

/// The exact stderr `gh` prints for the plan-gated 403.
const PLAN_GATED: &str =
    "gh: Upgrade to GitHub Pro or make this repository public to enable this feature. (HTTP 403)";

fn run(env: &[(&str, &str)]) -> Output {
    let dir = tempfile::tempdir().unwrap();
    let gh = write_mock_gh(dir.path());
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_loom-daemon"));
    cmd.args([
        "merge-pr",
        "stale-checks",
        "--pr",
        "8078",
        "--repo",
        "acme/private",
        "--head-sha",
        "deadbeef",
        "--base-ref",
        "main",
    ])
    .current_dir(dir.path())
    .env("LOOM_GH_BIN", &gh)
    .env("MOCK_RUNS_OUT", STALE_GREEN_RUNS);
    for (k, v) in env {
        cmd.env(k, v);
    }
    cmd.output().unwrap()
}

/// THE BUG: a plan-gated rulesets 403 is "this repository has no required
/// checks", so the guard passes — with the relaxation said out loud.
#[test]
fn plan_gated_rulesets_403_is_clean_with_a_warning() {
    let out = run(&[("MOCK_RULES_RC", "1"), ("MOCK_RULES_STDERR", PLAN_GATED)]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(0), "stdout: {stdout} stderr: {stderr}");
    assert_eq!(
        stdout.trim(),
        "LOOM-STALE-CHECKS-CLEAN",
        "stdout must carry the sentinel and NOTHING else — merge-pr.sh compares it \
         for exact equality; stdout: {stdout}"
    );
    assert!(stderr.contains("Warning:"), "the relaxation must be visible: {stderr}");
    assert!(stderr.contains("#8844"), "the warning must name the issue: {stderr}");
    assert!(
        stderr.contains("Upgrade to GitHub Pro"),
        "the warning must quote what the forge actually said: {stderr}"
    );
}

/// The same gate reaching us from the LEGACY source instead. Both APIs back
/// the same paid feature, so whichever one GitHub declines on plan grounds,
/// the conclusion is identical.
#[test]
fn plan_gated_classic_branch_protection_403_is_clean_with_a_warning() {
    let out = run(&[
        ("MOCK_CLASSIC_RC", "1"),
        ("MOCK_CLASSIC_STDERR", PLAN_GATED),
    ]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(0), "stdout: {stdout} stderr: {stderr}");
    assert_eq!(stdout.trim(), "LOOM-STALE-CHECKS-CLEAN");
    assert!(
        stderr.contains("classic branch-protection"),
        "the warning must name the gated source: {stderr}"
    );
}

/// Every other 403 still fails CLOSED. These are the ones a code-only match
/// would have swallowed — a missing token scope hides required checks just as
/// completely as a plan gate, but there they may genuinely exist.
#[test]
fn other_403s_still_fail_closed() {
    for stderr_text in [
        "gh: Resource not accessible by integration (HTTP 403)",
        "gh: Must have admin rights to Repository. (HTTP 403)",
        "gh: API rate limit exceeded for user ID 1234. (HTTP 403)",
        "gh: Bad credentials (HTTP 401)",
        "gh: Not Found (HTTP 404)",
        "gh: Server Error (HTTP 500)",
    ] {
        let out = run(&[("MOCK_RULES_RC", "1"), ("MOCK_RULES_STDERR", stderr_text)]);
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert_eq!(
            out.status.code(),
            Some(2),
            "must fail closed on {stderr_text}; stdout: {stdout}"
        );
        assert!(stdout.contains("Merge blocked"), "stdout: {stdout}");
        assert!(
            !stdout.contains("LOOM-STALE-CHECKS-CLEAN"),
            "a failed lookup must never emit the clean sentinel: {stdout}"
        );
    }
    // …and the same for the classic source.
    let out = run(&[
        ("MOCK_CLASSIC_RC", "1"),
        ("MOCK_CLASSIC_STDERR", "gh: Bad credentials (HTTP 401)"),
    ]);
    assert_eq!(out.status.code(), Some(2));
}

/// A plan gate does NOT disarm the guard: a required context that the
/// SURVIVING source still reports, with a stale green run, blocks as before.
/// (Classic branch protection and rulesets are gated together on a real Free
/// private repo; this asserts the per-source rule has no blast radius beyond
/// the source that was actually refused.)
#[test]
fn a_plan_gated_source_does_not_silence_the_other_ones_required_check() {
    let out = run(&[
        ("MOCK_RULES_RC", "1"),
        ("MOCK_RULES_STDERR", PLAN_GATED),
        ("MOCK_CLASSIC_OUT", "File Size Ratchet\n"),
    ]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(out.status.code(), Some(1), "stdout: {stdout}");
    assert!(stdout.contains("File Size Ratchet"), "stdout: {stdout}");
    assert!(stdout.contains("Merge blocked"), "stdout: {stdout}");
}

/// No gate anywhere: the ordinary path is untouched — an ungated ruleset
/// requiring the check still blocks on the incident's stale green run.
#[test]
fn the_ungated_path_is_unchanged() {
    let out = run(&[("MOCK_RULES_OUT", "File Size Ratchet\n")]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(out.status.code(), Some(1), "stdout: {stdout}");
    assert!(stdout.contains("File Size Ratchet"), "stdout: {stdout}");
}
