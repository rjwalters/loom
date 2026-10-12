//! Tests for the per-PR re-date telemetry (#10163).

use super::*;

const SUBJ: &str = " (#8248 guard, automated by #8508)";

fn rec(sha: &str, pr: &str, at: &str) -> String {
    format!("{sha}\u{1f}chore: re-date required checks for PR #{pr}{SUBJ}\u{1f}{at}\u{1e}\n")
}

fn t(s: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
}

/// The 2026-10-04 shape: chain head #9832 re-dated three times in ~40 min.
fn three_redate_chain() -> String {
    [
        rec("c3", "9832", "2026-10-04T03:50:00+00:00"),
        rec("c2", "9832", "2026-10-04T03:25:00+00:00"),
        rec("c1", "9832", "2026-10-04T03:10:00+00:00"),
        rec("d1", "9900", "2026-10-04T03:30:00+00:00"),
    ]
    .concat()
}

#[test]
fn a_three_redate_chain_is_counted_and_flagged_stuck_while_unlanded() {
    let stats = chain_stats(&parse_timed_log(&three_redate_chain()), |_| None);
    assert_eq!(stats.len(), 2);
    let head = &stats[0];
    assert_eq!((head.pr.as_str(), head.redates), ("9832", 3));
    assert_eq!(head.first_redate_at, "2026-10-04T03:10:00Z");
    assert_eq!(head.last_redate_at, "2026-10-04T03:50:00Z");
    assert_eq!(head.last_sha, "c3");
    assert!(head.stuck && head.time_to_land_secs.is_none());
    let single = &stats[1];
    assert_eq!((single.pr.as_str(), single.redates, single.stuck), ("9900", 1, false));
}

#[test]
fn time_to_land_runs_from_the_first_redate_to_the_landing_merge() {
    let stats = chain_stats(&parse_timed_log(&three_redate_chain()), |sha| {
        (sha == "c3").then(|| t("2026-10-04T04:10:00Z"))
    });
    let head = &stats[0];
    assert_eq!(head.landed_at.as_deref(), Some("2026-10-04T04:10:00Z"));
    assert_eq!(head.time_to_land_secs, Some(3600));
    assert!(!head.stuck, "a landed PR is never stuck");
}

#[test]
fn records_that_are_not_automated_redates_or_have_no_time_are_dropped() {
    let raw = format!(
        "{}x1\u{1f}chore: re-date required checks for PR #1 (#8248 guard, operator release)\u{1f}2026-10-04T03:10:00+00:00\u{1e}\n\
         x2\u{1f}feat: unrelated\u{1f}2026-10-04T03:10:00+00:00\u{1e}\n\
         x3\u{1f}chore: re-date required checks for PR #2{SUBJ}\u{1f}not-a-time\u{1e}\n",
        rec("ok", "7", "2026-10-04T03:10:00+00:00")
    );
    let got = parse_timed_log(&raw);
    assert_eq!(got.len(), 1);
    assert_eq!((got[0].sha.as_str(), got[0].pr.as_str()), ("ok", "7"));
}

#[test]
fn log_args_are_read_only_and_anchored() {
    let args = timed_log_args("origin/main", t("2026-10-04T00:00:00Z"));
    assert_eq!(args[0], "log");
    assert!(args.iter().any(|a| a.starts_with("--grep=^chore: re-date")));
    let landing = landing_log_args("abc", "origin/main");
    assert!(
        landing.contains(&"--reverse".to_string()) && landing.contains(&"--merges".to_string())
    );
    assert_eq!(landing[1], "abc..origin/main");
}

#[test]
fn text_rendering_names_the_livelock() {
    let stats = chain_stats(&parse_timed_log(&three_redate_chain()), |_| None);
    let text = render_text(&stats);
    assert!(text.contains("#9832  NOT LANDED (livelock suspected)"), "{text}");
    assert_eq!(render_text(&[]), "");
}

// --- Against a real repository ---------------------------------------------

/// Run git in `dir` with a fixed identity and committer/author time `at`.
fn git(dir: &std::path::Path, at: DateTime<Utc>, args: &[&str]) -> String {
    let date = at.to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@example.invalid")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@example.invalid")
        .env("GIT_AUTHOR_DATE", &date)
        .env("GIT_COMMITTER_DATE", &date)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

#[test]
fn collect_sees_an_unlanded_pr_on_its_remote_branch_then_its_landing() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    let now = Utc::now();
    let hours = |h: i64| now - chrono::TimeDelta::hours(h);
    git(dir, hours(5), &["init", "-q", "-b", "main"]);
    git(dir, hours(5), &["commit", "-q", "--allow-empty", "-m", "base"]);
    git(dir, hours(5), &["update-ref", "refs/remotes/origin/main", "HEAD"]);
    git(dir, hours(5), &["checkout", "-q", "-b", "feature/issue-1"]);
    for h in [4, 3, 2] {
        let subject = format!("chore: re-date required checks for PR #42{SUBJ}");
        git(dir, hours(h), &["commit", "-q", "--allow-empty", "-m", &subject]);
    }
    git(dir, hours(2), &["update-ref", "refs/remotes/origin/feature/issue-1", "HEAD"]);

    // Only the PR's remote branch carries the re-dates: `origin/main` alone
    // would report nothing while the PR is livelocked.
    let stats = collect(dir, "origin/main", hours(24)).unwrap();
    assert_eq!(stats.len(), 1);
    assert_eq!((stats[0].pr.as_str(), stats[0].redates), ("42", 3));
    assert!(stats[0].stuck && stats[0].landed_at.is_none(), "{stats:?}");

    // Land it with a merge commit one hour after the last re-date.
    git(dir, hours(1), &["checkout", "-q", "main"]);
    git(
        dir,
        hours(1),
        &[
            "merge",
            "-q",
            "--no-ff",
            "-m",
            "Merge PR #42",
            "feature/issue-1",
        ],
    );
    git(dir, hours(1), &["update-ref", "refs/remotes/origin/main", "HEAD"]);
    let stats = collect(dir, "origin/main", hours(24)).unwrap();
    assert_eq!(stats.len(), 1);
    assert_eq!(stats[0].redates, 3, "one commit reachable from two refs counts once");
    assert!(!stats[0].stuck);
    assert_eq!(stats[0].time_to_land_secs, Some(3 * 3600));
}
