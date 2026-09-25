//! Tests for the in-place re-run remedy (#8914).
//!
//! [`evaluate`] and [`classify_post_error`] are pure and tested directly.
//! [`rerun_in_place_with`] is driven against a stub `gh` passed as a plain
//! argument (never `LOOM_GH_BIN`, which races across parallel test threads),
//! because the property that matters most — nothing is pushed, and a 403 only
//! falls back to the push when it is NOT "already running" — is a property
//! of the call sequence.

use super::*;
use std::fs;
use std::io::Write;

const BASE_TIP: &str = "2026-09-25T14:00:00Z";
const BEFORE: &str = "2026-09-25T13:00:00Z";
const AFTER: &str = "2026-09-25T14:05:00Z";

fn ts(s: &str) -> DateTime<Utc> {
    s.parse().expect("test timestamp parses")
}

fn run(
    name: &str,
    status: &str,
    conclusion: Option<&str>,
    started: Option<&str>,
    run_id: Option<u64>,
) -> CheckRun {
    CheckRun {
        name: name.to_string(),
        status: status.to_string(),
        conclusion: conclusion.map(String::from),
        started_at: started.map(ts),
        actions_run_id: run_id,
    }
}

fn ctx(names: &[&str]) -> Vec<String> {
    names.iter().map(|n| (*n).to_string()).collect()
}

// --- evaluate -------------------------------------------------------------

#[test]
fn evaluate_collects_every_stale_required_run_not_just_the_first() {
    let runs = vec![
        run("A", "completed", Some("success"), Some(BEFORE), Some(1)),
        run("B", "completed", Some("success"), Some(BEFORE), Some(2)),
        run("C", "completed", Some("success"), Some(BEFORE), Some(1)),
    ];
    let ev = evaluate(ts(BASE_TIP), &ctx(&["A", "B", "C"]), &runs);
    assert_eq!(ev.stale_runs, BTreeSet::from([1, 2]), "one entry per workflow run");
    assert!(!ev.is_fresh());
}

#[test]
fn evaluate_is_fresh_when_every_required_green_started_after_the_tip() {
    let runs = vec![
        run("A", "completed", Some("success"), Some(AFTER), Some(1)),
        // Non-required stale green is not merge evidence.
        run("Informational", "completed", Some("success"), Some(BEFORE), Some(1)),
    ];
    let ev = evaluate(ts(BASE_TIP), &ctx(&["A"]), &runs);
    assert!(ev.is_fresh(), "{ev:?}");
}

#[test]
fn evaluate_treats_a_queued_rerun_beside_the_old_green_as_pending_not_stale() {
    // Right after a whole-run re-run the new attempt may be queued with no
    // started_at while the old green run is still listed; the old run must
    // not be read as stale (which would re-POST forever) nor as fresh.
    let runs = vec![
        run("A", "completed", Some("success"), Some(BEFORE), Some(1)),
        run("A", "queued", None, None, Some(1)),
    ];
    let ev = evaluate(ts(BASE_TIP), &ctx(&["A"]), &runs);
    assert_eq!(ev.pending, ctx(&["A"]));
    assert!(ev.stale_runs.is_empty());
    assert!(!ev.is_fresh());
}

#[test]
fn evaluate_flags_a_stale_check_from_another_app_as_unrerunnable() {
    let runs = vec![run(
        "External",
        "completed",
        Some("success"),
        Some(BEFORE),
        None,
    )];
    let ev = evaluate(ts(BASE_TIP), &ctx(&["External"]), &runs);
    assert_eq!(ev.stale_unrerunnable, ctx(&["External"]));
    assert!(ev.stale_runs.is_empty());
}

#[test]
fn evaluate_reports_red_with_its_run_and_ignores_skipped_and_absent() {
    let runs = vec![
        run("A", "completed", Some("failure"), Some(AFTER), Some(7)),
        run("S", "completed", Some("skipped"), Some(BEFORE), Some(7)),
    ];
    let ev = evaluate(ts(BASE_TIP), &ctx(&["A", "S", "Absent"]), &runs);
    assert_eq!(
        ev.red,
        vec![Red {
            check: "A".to_string(),
            conclusion: "failure".to_string(),
            run_id: Some(7),
            started_at: Some(ts(AFTER)),
        }]
    );
    assert!(ev.stale_runs.is_empty(), "skipped is never stale green");
}

#[test]
fn evaluate_fails_closed_on_green_without_started_at() {
    let runs = vec![run("A", "completed", Some("success"), None, Some(1))];
    let ev = evaluate(ts(BASE_TIP), &ctx(&["A"]), &runs);
    assert!(ev.unknown.is_some());
    assert!(!ev.is_fresh());
}

// --- classify_post_error --------------------------------------------------

#[test]
fn already_running_is_a_wait_even_though_it_is_a_403() {
    assert_eq!(
        classify_post_error(
            "gh api -X failed: gh: The workflow run containing this job is already running (HTTP 403)"
        ),
        PostResult::AlreadyRunning
    );
}

#[test]
fn missing_actions_write_is_refused() {
    let e = "gh api -X failed: gh: Resource not accessible by integration (HTTP 403)";
    assert_eq!(classify_post_error(e), PostResult::Refused(e.to_string()));
    let old =
        "gh: Unable to retry this workflow run because it was created over a month ago (HTTP 403)";
    assert!(matches!(classify_post_error(old), PostResult::Refused(_)));
    assert!(matches!(
        classify_post_error("gh: Not Found (HTTP 404)"),
        PostResult::Refused(_)
    ));
}

#[test]
fn transient_failures_are_errors_not_refusals() {
    // A 5xx must not fall back to the push: that would spend the verdict on a
    // forge hiccup the next pass would ride out.
    assert!(matches!(
        classify_post_error("gh: Server Error (HTTP 502)"),
        PostResult::Error(_)
    ));
}

// --- Stubbed forge I/O ------------------------------------------------------

/// A fake `gh` driven by files in `dir`:
/// - `head` — the PR head SHA `pulls/42` reports;
/// - `runs-before.json` / `runs-after.json` — the projected check-runs array,
///   served before / after a re-run POST has been accepted;
/// - `post-mode` — `ok`, `running`, `forbidden`, or `error`.
///
/// Every call's argv is appended to `argv.log`, one line per call.
fn stub(dir: &std::path::Path, before: &str, after: &str, post_mode: &str) -> std::path::PathBuf {
    fs::write(dir.join("head"), "headsha").unwrap();
    fs::write(dir.join("runs-before.json"), before).unwrap();
    fs::write(dir.join("runs-after.json"), after).unwrap();
    fs::write(dir.join("post-mode"), post_mode).unwrap();
    let d = dir.display();
    let script = format!(
        r#"#!/usr/bin/env bash
set -uo pipefail
shift # drop "api"
echo "$*" >> "{d}/argv.log"
PATH_ARG=""
for arg in "$@"; do
  case "$arg" in repos/*|graphql) PATH_ARG="$arg" ;; esac
done
case "$PATH_ARG" in
  repos/o/r/pulls/42) printf '%s\tmain\n' "$(cat "{d}/head")" ;;
  repos/o/r/commits/main) printf 'tipsha\t{BASE_TIP}\n' ;;
  repos/o/r/rules/branches/main) printf 'A\nB\n' ;;
  graphql) : ;;
  repos/o/r/commits/*/check-runs)
    if [ -e "{d}/posted" ]; then cat "{d}/runs-after.json"; else cat "{d}/runs-before.json"; fi ;;
  repos/o/r/actions/runs/*/rerun)
    case "$(cat "{d}/post-mode")" in
      ok) touch "{d}/posted"; echo '{{}}' ;;
      running) echo "gh: The workflow run containing this job is already running (HTTP 403)" >&2; exit 1 ;;
      forbidden) echo "gh: Resource not accessible by integration (HTTP 403)" >&2; exit 1 ;;
      *) echo "gh: Server Error (HTTP 502)" >&2; exit 1 ;;
    esac ;;
  *) echo "stub gh: unexpected call: $*" >&2; exit 2 ;;
esac
"#
    );
    let path = dir.join("gh");
    let mut f = fs::File::create(&path).unwrap();
    f.write_all(script.as_bytes()).unwrap();
    drop(f);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    }
    path
}

fn tmp_dir(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("loom-rerun-test-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn runs_json(started: &str, status: &str, conclusion: &str) -> String {
    let concl = if conclusion.is_empty() {
        "null".to_string()
    } else {
        format!("\"{conclusion}\"")
    };
    let started = if started.is_empty() {
        "null".to_string()
    } else {
        format!("\"{started}\"")
    };
    ["A", "B"]
        .iter()
        .map(|n| {
            format!(
                r#"{{"name":"{n}","status":"{status}","conclusion":{concl},"started_at":{started},"app":"github-actions","details_url":"https://github.com/o/r/actions/runs/555/job/1"}}"#
            )
        })
        .collect::<Vec<_>>()
        .join(",")
}

fn arr(s: &str) -> String {
    format!("[{s}]")
}

fn go(dir: &std::path::Path, gh: &std::path::Path, wait_ms: u64) -> (RerunOutcome, String) {
    let out = rerun_in_place_with(
        gh.to_str().unwrap(),
        "o/r",
        "42",
        "headsha",
        Duration::from_millis(wait_ms),
        Duration::ZERO,
    );
    let log = fs::read_to_string(dir.join("argv.log")).unwrap_or_default();
    (out, log)
}

fn assert_never_pushes(log: &str) {
    assert!(
        !log.contains("git/commits") && !log.contains("git/refs"),
        "the in-place remedy must never create a commit or move a ref: {log}"
    );
}

#[test]
fn reruns_the_whole_run_once_and_reports_fresh_without_pushing() {
    let dir = tmp_dir("fresh");
    let before = arr(&runs_json(BEFORE, "completed", "success"));
    let after = arr(&runs_json(AFTER, "completed", "success"));
    let gh = stub(&dir, &before, &after, "ok");
    let (out, log) = go(&dir, &gh, 60_000);
    assert_eq!(out, RerunOutcome::Fresh { reran: vec![555] });
    assert_eq!(
        log.matches("actions/runs/555/rerun").count(),
        1,
        "two stale jobs in ONE workflow run are one whole-run re-run: {log}"
    );
    assert!(!log.contains("actions/jobs/"), "never per-job re-runs: {log}");
    assert_never_pushes(&log);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn already_running_waits_and_reports_pending_without_pushing() {
    let dir = tmp_dir("running");
    let before = arr(&runs_json(BEFORE, "completed", "success"));
    let gh = stub(&dir, &before, &before, "running");
    let (out, log) = go(&dir, &gh, 0);
    match out {
        RerunOutcome::Pending { reran, waiting_on } => {
            assert!(reran.is_empty());
            assert_eq!(waiting_on, vec!["workflow run 555".to_string()]);
        }
        other => panic!("already-running must be a wait, got {other:?}"),
    }
    assert_never_pushes(&log);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn missing_permission_is_refused_so_the_caller_can_fall_back_to_the_push() {
    let dir = tmp_dir("forbidden");
    let before = arr(&runs_json(BEFORE, "completed", "success"));
    let gh = stub(&dir, &before, &before, "forbidden");
    let (out, log) = go(&dir, &gh, 60_000);
    assert!(
        matches!(&out, RerunOutcome::Refused(why) if why.contains("not accessible by integration")),
        "{out:?}"
    );
    assert_never_pushes(&log);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn a_transient_post_failure_leaves_the_refusal_standing() {
    let dir = tmp_dir("error");
    let before = arr(&runs_json(BEFORE, "completed", "success"));
    let gh = stub(&dir, &before, &before, "error");
    let (out, _) = go(&dir, &gh, 60_000);
    assert!(matches!(out, RerunOutcome::Failed(_)), "{out:?}");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn pending_after_the_rerun_reports_pending_when_the_budget_is_spent() {
    let dir = tmp_dir("pending");
    let before = arr(&runs_json(BEFORE, "completed", "success"));
    let after = arr(&runs_json("", "queued", ""));
    let gh = stub(&dir, &before, &after, "ok");
    let (out, log) = go(&dir, &gh, 0);
    assert_eq!(
        out,
        RerunOutcome::Pending {
            reran: vec![555],
            waiting_on: vec!["workflow run 555".to_string()],
        }
    );
    assert_never_pushes(&log);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn a_red_rerun_is_a_failure_not_a_retry() {
    let dir = tmp_dir("red");
    let before = arr(&runs_json(BEFORE, "completed", "success"));
    // Started after the POST (the loop compares against the wall clock at
    // POST time), i.e. the re-run's own verdict.
    let after = arr(&runs_json("2099-01-01T00:00:00Z", "completed", "failure"));
    let gh = stub(&dir, &before, &after, "ok");
    let (out, log) = go(&dir, &gh, 60_000);
    assert!(
        matches!(&out, RerunOutcome::Failed(why) if why.contains("concluded failure")),
        "{out:?}"
    );
    assert_eq!(log.matches("/rerun").count(), 1, "{log}");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn a_moved_head_is_head_moved_before_anything_is_rerun() {
    let dir = tmp_dir("moved");
    let before = arr(&runs_json(BEFORE, "completed", "success"));
    let gh = stub(&dir, &before, &before, "ok");
    fs::write(dir.join("head"), "othersha").unwrap();
    let (out, log) = go(&dir, &gh, 60_000);
    assert_eq!(
        out,
        RerunOutcome::HeadMoved {
            current: "othersha".to_string()
        }
    );
    assert!(!log.contains("/rerun"), "{log}");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn a_stale_check_from_another_app_is_not_applicable() {
    let dir = tmp_dir("external");
    let before = arr(&format!(
        r#"{{"name":"A","status":"completed","conclusion":"success","started_at":"{BEFORE}","app":"some-ci","details_url":"https://ci.example/1"}}"#
    ));
    let gh = stub(&dir, &before, &before, "ok");
    let (out, log) = go(&dir, &gh, 60_000);
    assert!(matches!(out, RerunOutcome::NotApplicable(_)), "{out:?}");
    assert!(!log.contains("/rerun"), "{log}");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn already_fresh_evidence_reports_fresh_with_nothing_rerun() {
    let dir = tmp_dir("already-fresh");
    let fresh = arr(&runs_json(AFTER, "completed", "success"));
    let gh = stub(&dir, &fresh, &fresh, "ok");
    let (out, log) = go(&dir, &gh, 60_000);
    assert_eq!(out, RerunOutcome::Fresh { reran: vec![] });
    assert!(!log.contains("/rerun"), "{log}");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn actions_run_id_parses_only_github_actions_job_urls() {
    use super::super::stale_checks::fetch::actions_run_id;
    assert_eq!(
        actions_run_id(
            Some("github-actions"),
            Some("https://github.com/o/r/actions/runs/36159146111/job/108151066150")
        ),
        Some(36_159_146_111)
    );
    assert_eq!(
        actions_run_id(Some("other-app"), Some("https://github.com/o/r/actions/runs/1/job/2")),
        None
    );
    assert_eq!(actions_run_id(Some("github-actions"), Some("https://x/y")), None);
    assert_eq!(actions_run_id(Some("github-actions"), None), None);
}

#[test]
fn a_red_run_that_predates_the_rerun_is_not_read_as_its_verdict() {
    // Right after the POST the old attempt's red check run can still be the
    // latest listed; it is not the re-run's result. Later passes must keep
    // waiting (Pending once the budget is spent), never report Failed.
    let dir = tmp_dir("old-red");
    let before = arr(&format!(
        r#"{},{{"name":"B","status":"completed","conclusion":"failure","started_at":"{BEFORE}","app":"github-actions","details_url":"https://github.com/o/r/actions/runs/555/job/2"}}"#,
        runs_json(BEFORE, "completed", "success")
            .split("},{")
            .next()
            .map(|a| format!("{a}}}"))
            .unwrap()
    ));
    let gh = stub(&dir, &before, &before, "ok");
    let (out, log) = go(&dir, &gh, 300);
    assert!(matches!(out, RerunOutcome::Pending { .. }), "{out:?}");
    assert_never_pushes(&log);
    let _ = fs::remove_dir_all(&dir);
}
