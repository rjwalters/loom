//! #11972: a Judge pass releases with its own skip reason. `ci-pending` (it
//! stood down because the head's CI was still running) is not recorded as a
//! no-verdict pass if CI settled before the release, but still is when CI is
//! stuck (everything pending past the exemption window or unstamped), so the
//! backstop keeps bounding a PR whose CI never finishes.

use super::*;
use loom_daemon::judge_backstop::SKIP_CI_PENDING;

const GREEN: &str = r#"{"total_count":1,"check_runs":[{"status":"completed","started_at":"2026-10-14T05:00:00Z"}]}"#;

/// One check run, `status`, started `started_at` (a JSON value).
fn one_run(status: &str, started_at: &str) -> String {
    format!(
        r#"{{"total_count":1,"check_runs":[{{"status":"{status}","started_at":{started_at}}}]}}"#
    )
}

/// A Judge claims the waiting PR whose head's check runs are `checks`, then
/// releases with `reason`. Returns its patched release and whether the next
/// Judge (another host) is held by the backstop.
fn release_at(checks: &str, reason: Option<&str>) -> (String, bool) {
    let dir = TempDir::new().unwrap();
    fake_gh(dir.path());
    waiting_pr(dir.path());
    fs::write(dir.path().join("checks"), checks).unwrap();
    let judge = on("host-a", "run-1");
    assert_eq!(ctx(dir.path(), judge.clone()).claim(), EXIT_HELD);
    assert_eq!(ctx(dir.path(), judge).release(reason, None), EXIT_HELD);
    let held = ctx(dir.path(), on("host-b", "run-2")).claim() == backstop::EXIT_BACKSTOP;
    (patched(dir.path()), held)
}

fn recorded(patch: &str) -> bool {
    let judge = on("host-a", "run-1");
    let record =
        judge.released_marker_with(&format!(" head={HEAD} verdict=none labels=review-requested"));
    if patch.contains(&record) {
        return true;
    }
    assert!(patch.contains(&format!("body={}", judge.released_marker())), "{patch}");
    false
}

/// Acceptance (#11972): a pass that skipped for pending CI writes no record
/// when CI completed (green) between its check and its release, nor while CI
/// is still young; the PR stays claimable.
#[test]
fn a_ci_pending_skip_is_not_recorded_once_ci_settled_or_while_it_is_young() {
    let young = stamp(Utc::now() - chrono::Duration::minutes(10));
    for checks in [
        GREEN.to_string(),
        one_run("in_progress", &format!("\"{young}\"")),
    ] {
        let (patch, held) = release_at(&checks, Some(SKIP_CI_PENDING));
        assert!(!recorded(&patch), "{checks}: {patch}");
        assert!(!held, "{checks}");
    }
}

/// #12008 review: CI that never settles (a run queued with no runner has no
/// `started_at`; one in progress past the window) is recorded even under
/// `ci-pending`, so the PR planner serving it (#12003) stays bounded.
#[test]
fn a_ci_pending_skip_on_stuck_ci_is_still_recorded() {
    let stale = stamp(Utc::now() - chrono::Duration::hours(7));
    for checks in [
        one_run("queued", "null"),
        one_run("in_progress", &format!("\"{stale}\"")),
    ] {
        let (patch, held) = release_at(&checks, Some(SKIP_CI_PENDING));
        assert!(recorded(&patch), "{checks}: {patch}");
        assert!(held, "{checks}");
    }
}

/// No reason (an older prompt), or one this binary does not know: the
/// default, unchanged: settled CI and stuck CI are recorded, young is not.
#[test]
fn without_a_ci_pending_reason_the_release_is_unchanged() {
    let young = stamp(Utc::now() - chrono::Duration::minutes(10));
    for reason in [None, Some("other")] {
        for (checks, want) in [
            (GREEN.to_string(), true),
            (one_run("queued", "null"), true),
            (one_run("in_progress", &format!("\"{young}\"")), false),
        ] {
            let (patch, held) = release_at(&checks, reason);
            assert_eq!(recorded(&patch), want, "{reason:?} {checks}: {patch}");
            assert_eq!(held, want, "{reason:?} {checks}");
        }
    }
}

/// The `lease pr release` CLI takes `--skip-reason`, and omits it by default.
#[test]
fn release_parses_an_optional_skip_reason() {
    use clap::Parser;
    #[derive(clap::Parser)]
    struct Cli {
        #[command(subcommand)]
        action: PrLeaseAction,
    }
    let parse = |args: &[&str]| match Cli::try_parse_from(args).unwrap().action {
        PrLeaseAction::Release(r) => (r.common.pr, r.skip_reason),
        _ => panic!("not a release"),
    };
    let base = [
        "lease",
        "release",
        "12",
        "--role",
        "judge",
        "--watch-pid",
        "7",
    ];
    assert_eq!(parse(&base), (12, None));
    let skipped: Vec<&str> = base
        .iter()
        .copied()
        .chain(["--skip-reason", "ci-pending"])
        .collect();
    assert_eq!(parse(&skipped), (12, Some("ci-pending".to_string())));
}

/// The Judge prompt's pending-CI skip releases its lease with the pass's own
/// reason.
#[test]
fn the_judge_prompt_releases_a_pending_ci_skip_with_its_reason() {
    let prompts = [
        ("judge.md", include_str!("../../../../defaults/.claude/commands/loom/judge.md")),
        (
            "loom-judge/SKILL.md",
            include_str!("../../../../defaults/.agents/skills/loom-judge/SKILL.md"),
        ),
    ];
    let flag = format!("--skip-reason {SKIP_CI_PENDING}");
    for (name, text) in prompts {
        let lines: Vec<&str> = text.lines().collect();
        let case = lines
            .iter()
            .position(|l| l.trim_start().starts_with("LOOM-CHECKS-TIMEOUT*)"))
            .unwrap_or_else(|| panic!("{name}: no pending-CI skip case"));
        let arm: Vec<&str> = lines[case..]
            .iter()
            .copied()
            .take_while(|l| l.trim() != ";;")
            .collect();
        assert!(
            arm.iter()
                .any(|l| l.contains("loom-daemon lease pr release") && l.contains(&flag)),
            "{name}: the pending-CI skip must release with `{flag}`: {arm:?}"
        );
    }
}
