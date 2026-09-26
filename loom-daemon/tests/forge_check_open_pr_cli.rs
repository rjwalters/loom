//! CLI contract for `loom-daemon forge check-open-pr <issue>` (#8551).
//!
//! The manual Builder claim path (`gh issue list --label=loom:issue` → `gh
//! issue edit N --add-label loom:building`) had no equivalent of the daemon's
//! #4123 open-linked-PR dispatch guard, and on 2026-09-21 a hand-claim on issue
//! #8413 re-did work PR #8462 had already shipped. This suite pins the exit
//! codes and output that pre-claim guard's callers (CLAUDE.md § "Builder
//! Workflow" step 0, `builder.md` § "Finding Work") depend on:
//!
//! | Exit | Meaning | stdout |
//! |---|---|---|
//! | `0` | open linked PR exists — DO NOT claim | the PR number |
//! | `1` | verified absence — safe to claim | empty |
//! | `5` | probe could not answer — fail closed | empty |
//!
//! The probe itself is exercised through the real
//! `worktree_ops::gh::probe_open_linked_pr` union (GraphQL closes-graph ∪ REST
//! issue timeline) via a mock `gh` on `LOOM_GH_BIN` — the same seam
//! `forge_cmd`'s auto-merge tests use — so this suite also proves the
//! subcommand reuses that one code path rather than re-asking the forge itself.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// One `closedByPullRequestsReferences` payload with the given nodes.
fn graphql_nodes(nodes: &str) -> String {
    format!(
        "{{\"data\":{{\"repository\":{{\"issue\":{{\
         \"closedByPullRequestsReferences\":{{\"nodes\":[{nodes}]}}}}}}}}}}"
    )
}

/// Write a mock `gh` that answers the three calls `probe_open_linked_pr` makes
/// — `repo view` (owner/name resolution), `api graphql` (closes-graph),
/// `api repos/…/timeline` (cross-reference union) — from env vars, so each
/// test drives one leg without a live forge.
fn write_mock_gh(dir: &Path) -> PathBuf {
    let path = dir.join("gh");
    std::fs::write(
        &path,
        r#"#!/bin/sh
case "$1" in
  repo)
    [ "${MOCK_REPO_RC:-0}" = 0 ] || exit "$MOCK_REPO_RC"
    printf '%s\n' "${MOCK_NWO:-rjwalters/loom}"
    ;;
  api)
    case "$2" in
      graphql)
        [ "${MOCK_GRAPHQL_RC:-0}" = 0 ] || exit "$MOCK_GRAPHQL_RC"
        printf '%s' "${MOCK_GRAPHQL_OUT:-}"
        ;;
      *)
        [ "${MOCK_TIMELINE_RC:-0}" = 0 ] || exit "$MOCK_TIMELINE_RC"
        printf '%s' "${MOCK_TIMELINE_OUT:-}"
        ;;
    esac
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

/// Run `loom-daemon forge check-open-pr <issue>` against the mock `gh`.
///
/// Every ambient influence the command reads is pinned: `LOOM_FORGE_TYPE` (so
/// the Gitea decline cannot fire from an operator's config), the private
/// defaults tier, and the machine-level workspace registry.
fn run(issue: u32, env: &[(&str, &str)]) -> Output {
    let dir = tempfile::tempdir().unwrap();
    let gh = write_mock_gh(dir.path());
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_loom-daemon"));
    cmd.args(["forge", "check-open-pr", &issue.to_string()])
        .current_dir(dir.path())
        .env("LOOM_GH_BIN", &gh)
        .env("LOOM_FORGE_TYPE", "github")
        .env("LOOM_CONFIG_DEFAULTS_FILE", "")
        .env("LOOM_WORKSPACES_PATH", dir.path().join("workspaces.json"));
    for (k, v) in env {
        cmd.env(k, v);
    }
    cmd.output().unwrap()
}

/// An OPEN closes-graph node: exit 0, and stdout is the bare PR number so
/// `PR=$(loom-daemon forge check-open-pr 42)` captures something usable.
#[test]
fn open_linked_pr_exits_zero_and_prints_the_pr_number() {
    let out = run(
        8413,
        &[("MOCK_GRAPHQL_OUT", &graphql_nodes(r#"{"number":8462,"state":"OPEN"}"#))],
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(0), "stderr: {stderr}");
    assert_eq!(stdout.trim(), "8462", "stdout: {stdout}");
    assert_eq!(stdout.trim().parse::<u32>().unwrap(), 8462);
    assert!(stderr.contains("do NOT claim"), "stderr: {stderr}");
}

/// Verified absence from BOTH legs: exit 1, empty stdout.
#[test]
fn no_linked_pr_exits_one_with_empty_stdout() {
    let out = run(
        42,
        &[
            ("MOCK_GRAPHQL_OUT", &graphql_nodes("")),
            ("MOCK_TIMELINE_OUT", ""),
        ],
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(1), "stderr: {stderr}");
    assert!(stdout.trim().is_empty(), "stdout: {stdout}");
    assert!(stderr.contains("safe to claim"), "stderr: {stderr}");
}

/// A MERGED node is not an open PR — the `state == "OPEN"` filter is the
/// load-bearing one (`includeClosedPrs:false` alone still returns merged PRs),
/// so a long-since-merged PR must not block every future claim.
#[test]
fn a_merged_linked_pr_is_not_treated_as_open() {
    let out = run(
        42,
        &[
            ("MOCK_GRAPHQL_OUT", &graphql_nodes(r#"{"number":100,"state":"MERGED"}"#)),
            ("MOCK_TIMELINE_OUT", ""),
        ],
    );
    assert_eq!(out.status.code(), Some(1), "stderr: {}", String::from_utf8_lossy(&out.stderr));
}

/// One `{number, body}` timeline candidate line, the post-`--jq` shape the
/// mock `gh` emits for the cross-reference leg (#8940).
fn timeline_candidate(number: u32, body: &str) -> String {
    format!("{}\n", serde_json::json!({ "number": number, "body": body }))
}

/// The timeline leg is consulted when the closes-graph says `NoneOpen`: a
/// `Part of #N` phase PR uses no closing keyword and is invisible to leg 1
/// (#7859/#8116). Reaching it here also proves the subcommand runs the real
/// two-transport union rather than only its first leg.
#[test]
fn a_part_of_phase_pr_is_found_through_the_timeline_leg() {
    let out = run(
        42,
        &[
            ("MOCK_GRAPHQL_OUT", &graphql_nodes("")),
            ("MOCK_TIMELINE_OUT", &timeline_candidate(7777, "Part of #42")),
        ],
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(out.status.code(), Some(0), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(stdout.trim(), "7777", "stdout: {stdout}");
}

/// AC (#8940): the timeline leg's candidate is only a *candidate*. A PR that
/// merely mentions `#N` — no closing keyword, no `Part of`/`Contributes to`
/// phrase — is a bare mention, so the verdict is a verified absence (exit 1,
/// safe to claim), identical to what `/loom:sweep`'s existing-PR probe returns
/// for the same issue. Before the phrase filter, one stand-down comment quoting
/// an issue number refused that issue's dispatch for as long as the mentioning
/// PR stayed open (#8322, 6.5 days).
#[test]
fn a_bare_mention_is_a_verified_absence_not_an_open_linked_pr() {
    let out = run(
        8322,
        &[
            ("MOCK_GRAPHQL_OUT", &graphql_nodes("")),
            (
                "MOCK_TIMELINE_OUT",
                &timeline_candidate(
                    8314,
                    "Filed #8322 to track it, standing down without pushing.\n\nCloses #8256",
                ),
            ),
        ],
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(1), "stderr: {stderr}");
    assert!(stdout.trim().is_empty(), "stdout: {stdout}");
    assert!(stderr.contains("safe to claim"), "stderr: {stderr}");
}

/// AC (#8940): the phrase filter applies to the TIMELINE leg only. A `Closes
/// #N` PR is still an open linked PR through the closes-graph leg, which is
/// unchanged — including when the timeline leg answers nothing at all.
#[test]
fn a_closing_keyword_pr_still_blocks_through_the_closes_graph_leg() {
    let out = run(
        8322,
        &[
            ("MOCK_GRAPHQL_OUT", &graphql_nodes(r#"{"number":8314,"state":"OPEN"}"#)),
            ("MOCK_TIMELINE_OUT", ""),
        ],
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(out.status.code(), Some(0), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(stdout.trim(), "8314", "stdout: {stdout}");
}

/// Both transports erroring is NOT a verified absence: exit 5, and the message
/// must never read as an all-clear. This is the edge case the guard exists for
/// — a rate-limited `gh` silently reporting "no open PR" would greenlight
/// exactly the duplicate claim #8551 reports.
#[test]
fn a_gh_error_fails_closed_rather_than_reporting_no_open_pr() {
    let out = run(42, &[("MOCK_GRAPHQL_RC", "1"), ("MOCK_TIMELINE_RC", "1")]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(5), "stderr: {stderr}");
    assert_ne!(
        out.status.code(),
        Some(1),
        "a probe failure must not reuse the verified-absence exit code"
    );
    assert!(stdout.trim().is_empty(), "stdout: {stdout}");
    assert!(
        !stderr.contains("safe to claim"),
        "a failed probe must never read as an all-clear: {stderr}"
    );
    assert!(stderr.contains("NOT a verified absence"), "stderr: {stderr}");
}

/// Unresolvable repository (`gh repo view` fails) is a probe failure too — the
/// question was never asked, so it cannot be answered "no".
#[test]
fn an_unresolvable_repo_fails_closed() {
    let out = run(42, &[("MOCK_REPO_RC", "1")]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(5), "stderr: {stderr}");
    assert!(!stderr.contains("safe to claim"), "stderr: {stderr}");
}

/// Unparseable GraphQL with an unparseable timeline is also a failure, not an
/// absence.
#[test]
fn unparseable_output_fails_closed() {
    let out = run(
        42,
        &[
            ("MOCK_GRAPHQL_OUT", "not json at all"),
            ("MOCK_TIMELINE_OUT", "also not a number"),
        ],
    );
    assert_eq!(out.status.code(), Some(5), "stderr: {}", String::from_utf8_lossy(&out.stderr));
}

/// Gitea declines (exit 3) instead of answering: both transports are GitHub
/// APIs, and a confident "no open PR" there would be a false all-clear.
#[test]
fn gitea_declines_rather_than_reporting_a_false_all_clear() {
    let out = run(42, &[("LOOM_FORGE_TYPE", "gitea")]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(3), "stderr: {stderr}");
    assert!(stderr.contains("GitHub-only"), "stderr: {stderr}");
    assert!(!stderr.contains("safe to claim"), "stderr: {stderr}");
}
