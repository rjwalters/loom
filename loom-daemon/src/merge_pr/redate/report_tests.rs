//! Tests for `merge-pr redate-report` (#9746), over fixture `git log` output.

use super::*;
use crate::merge_pr::redate::attribution::Attribution;
use crate::merge_pr::redate::commit_message_with;

const SUBJ: &str = " (#8248 guard, automated by #8508)";

/// One record exactly as `git log --format=%H%x1f%s%x1f%(trailers:only,unfold)%x1e`
/// prints it (git ends the trailer block with a newline, and starts the next
/// record on a fresh line).
fn record(sha: &str, pr: &str, trailers: &[&str]) -> String {
    let mut t = trailers.join("\n");
    if !t.is_empty() {
        t.push('\n');
    }
    format!("{sha}\u{1f}chore: re-date required checks for PR #{pr}{SUBJ}\u{1f}{t}\u{1e}\n")
}

fn clause4(pr: &str, sha: &str) -> String {
    record(
        sha,
        pr,
        &[
            "Stale-Check: Structural Checks (Role Prompt Prefix Ratchet)",
            "Stale-Clause: the base move and this PR both touch this check's coupled inputs",
            "Coupled-Base-Path: CLAUDE.md",
            "Coupled-PR-Path: defaults/docs/eta.md",
        ],
    )
}

fn fixture() -> String {
    [
        clause4("9743", "a1"),
        clause4("9743", "a2"),
        clause4("9721", "a3"),
        record(
            "b1",
            "9726",
            &[
                "Stale-Check: Rust Tests",
                "Stale-Clause: time rule (#8248 started_at fallback)",
                "Coupled-Base-Path: none",
                "Coupled-PR-Path: none",
            ],
        ),
        // Pre-#9746 history: no trailers at all.
        record("c1", "9698", &[]),
        record("c2", "9698", &[]),
        // Matched the --grep pre-filter but is not the exact subject.
        "d1\u{1f}chore: re-date required checks for PR #1 (by hand)\u{1f}\u{1e}\n".to_string(),
        // Matched --grep only through a body line: not a re-date at all.
        "f1\u{1f}docs: explain re-dates\u{1f}\u{1e}\n".to_string(),
    ]
    .concat()
}

#[test]
fn parse_keeps_only_exact_redate_subjects_and_reads_trailers() {
    let commits = parse_log(&fixture());
    assert_eq!(commits.len(), 6, "{commits:?}");
    assert_eq!(commits[0].sha, "a1");
    assert_eq!(commits[0].pr, "9743");
    let key = commits[0].attribution.as_ref().expect("attributed");
    assert_eq!(key.check, "Structural Checks (Role Prompt Prefix Ratchet)");
    assert_eq!(key.base_path, "CLAUDE.md");
    assert_eq!(key.pr_path, "defaults/docs/eta.md");
    assert!(commits[4].attribution.is_none());
    assert!(commits.iter().all(|c| c.sha != "d1"));
}

#[test]
fn aggregate_counts_check_by_path_most_frequent_first() {
    let r = aggregate(
        &parse_log(&fixture()),
        count_other_redates(&fixture()),
        "origin/main",
        "2026-09-29T00:00:00Z",
    );
    assert_eq!((r.total, r.attributed, r.untrailered), (6, 4, 2));
    assert_eq!(r.other_redate_subjects, 1, "the other-subject re-date is counted apart");
    assert_eq!(r.rows.len(), 2);
    assert_eq!(r.rows[0].count, 3);
    assert_eq!(r.rows[0].key.base_path, "CLAUDE.md");
    assert_eq!(r.rows[0].prs, vec!["9721", "9743"], "distinct PRs, ascending");
    assert_eq!(r.rows[1].key.check, "Rust Tests");
    assert_eq!(r.by_check[0].name, "Structural Checks (Role Prompt Prefix Ratchet)");
    assert_eq!(r.by_check[0].count, 3);
    assert_eq!(r.by_clause.len(), 2);
}

#[test]
fn text_and_json_carry_the_attribution_and_the_untrailered_count() {
    let r = aggregate(&parse_log(&fixture()), count_other_redates(&fixture()), "origin/main", "S");
    let text = render_text(&r);
    assert!(text.contains("6 (4 attributed, 2 untrailered)"), "{text}");
    assert!(text.contains("base: CLAUDE.md  |  pr: defaults/docs/eta.md"), "{text}");
    assert!(text.contains("#9721 #9743"), "{text}");
    assert!(text.contains("plus 1 other re-date commit(s)"), "{text}");
    let json = serde_json::to_value(&r).expect("serializes");
    assert_eq!(json["untrailered"], 2);
    assert_eq!(json["rows"][0]["check"], "Structural Checks (Role Prompt Prefix Ratchet)");
    assert_eq!(json["rows"][0]["count"], 3);
}

#[test]
fn an_empty_window_says_so() {
    let r = aggregate(&parse_log(""), 0, "origin/main", "S");
    assert_eq!(r.total, 0);
    assert!(render_text(&r).contains("No attributed re-dates"));
}

#[test]
fn missing_path_trailers_read_as_none() {
    let raw = record("e1", "5", &["Stale-Check: Lint"]);
    let c = parse_log(&raw);
    let key = c[0].attribution.as_ref().expect("attributed");
    assert_eq!((key.clause.as_str(), key.base_path.as_str()), ("none", "none"));
}

/// End to end against a real repository: the producer's message, committed and
/// read back through the report's own `git log` invocation.
#[test]
fn the_report_reads_back_what_commit_message_with_writes() {
    let dir = std::env::temp_dir().join(format!("loom-redate-report-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("tmp dir");
    let git = |args: &[&str]| {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(&dir)
            .args([
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "-c",
                "commit.gpgsign=false",
            ])
            .args(args)
            .output()
            .expect("git runs");
        assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    };
    git(&["init", "-q", "-b", "main"]);
    let a = Attribution {
        check: "Structural Checks (Role Prompt Prefix Ratchet)".into(),
        clause: "the base move and this PR both touch this check's coupled inputs".into(),
        base_path: Some("CLAUDE.md".into()),
        pr_path: Some("defaults/docs/eta.md".into()),
    };
    git(&[
        "commit",
        "-q",
        "--allow-empty",
        "-m",
        &commit_message_with("9743", Some(&a)),
    ]);
    git(&[
        "commit",
        "-q",
        "--allow-empty",
        "-m",
        &commit_message_with("9698", None),
    ]);
    git(&["commit", "-q", "--allow-empty", "-m", "feat: unrelated"]);

    let since = chrono::Utc::now() - chrono::TimeDelta::hours(1);
    let raw = run_git_log(&dir, "main", since).expect("git log");
    let r = aggregate(&parse_log(&raw), count_other_redates(&raw), "main", "S");
    assert_eq!((r.total, r.attributed, r.untrailered), (2, 1, 1), "{raw:?}");
    assert_eq!(r.rows[0].key.pr_path, "defaults/docs/eta.md");
    assert_eq!(r.rows[0].prs, vec!["9743"]);
    let _ = std::fs::remove_dir_all(&dir);
}
