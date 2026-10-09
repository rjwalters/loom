//! The CI-run log (#10737): head resolution at the cutoff, the meaning of
//! "last completed run", observation-time leaks and persistence.

use crate::eta::ci_log::{append, compact, fresh, load, log_path, pr_of_ref, CiLog, CiRecord};
use crate::eta::loop_features::FileSnapshot;
use chrono::{DateTime, Duration, TimeZone, Utc};

fn t(h: i64) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 10, 1, 0, 0, 0).unwrap() + Duration::hours(h)
}

fn head(repo: &str, pr: u32, known: i64, sha: Option<&str>) -> FileSnapshot {
    FileSnapshot {
        repo: repo.into(),
        pr,
        known_at: t(known),
        files: vec![],
        head_sha: sha.map(str::to_string),
        complete: true,
        additions: None,
        deletions: None,
        listed: None,
    }
}

fn run(sha: &str, id: u64, attempt: u32, done: i64, known: i64, c: Option<&str>) -> CiRecord {
    CiRecord {
        repo: "o/r".into(),
        head_sha: sha.into(),
        run_id: id,
        run_attempt: attempt,
        workflow: "ci".into(),
        completed_at: t(done),
        known_at: t(known),
        conclusion: c.map(str::to_string),
        git_ref: Some("refs/pull/7/merge".into()),
        pr: Some(7),
    }
}

/// `(at hours, failed)` of the observations, or `None` when unknown.
fn seen(log: &CiLog, repo: &str, pr: u32, as_of: i64) -> Option<Vec<(i64, bool)>> {
    log.observations(repo, pr, t(as_of)).map(|v| {
        v.iter()
            .map(|o| ((o.at - t(0)).num_hours(), o.failed))
            .collect()
    })
}

#[test]
fn an_unknown_head_leaves_ci_unknown() {
    let log = CiLog::new(&[run("a", 1, 1, 1, 1, Some("success"))], &[]);
    assert_eq!(seen(&log, "o/r", 7, 10), None);
    // A head first read after the cutoff is not the head at the cutoff.
    let log = CiLog::new(&[run("a", 1, 1, 1, 1, Some("success"))], &[head("o/r", 7, 5, Some("a"))]);
    assert_eq!(seen(&log, "o/r", 7, 5), None);
    assert_eq!(seen(&log, "o/r", 7, 6), Some(vec![(1, false)]));
    // A read that named no single head is ambiguous: unknown from then on.
    let log = CiLog::new(
        &[run("a", 1, 1, 1, 1, Some("success"))],
        &[head("o/r", 7, 2, Some("a")), head("o/r", 7, 4, None)],
    );
    assert_eq!(seen(&log, "o/r", 7, 3), Some(vec![(1, false)]));
    assert_eq!(seen(&log, "o/r", 7, 9), None);
}

#[test]
fn only_runs_of_the_heads_pr_repo_and_cutoff_count() {
    let mut other = run("a", 5, 1, 1, 1, Some("failure"));
    other.repo = "x/y".into();
    let log = CiLog::new(
        &[
            run("a", 1, 1, 1, 1, Some("success")),
            run("old", 2, 1, 2, 2, Some("failure")),
            other,
        ],
        &[
            head("o/r", 7, 0, Some("old")),
            head("o/r", 7, 3, Some("a")),
            head("x/y", 7, 0, Some("zzz")),
        ],
    );
    // Stale head `old` is the head before hour 3; its failure counts then.
    assert_eq!(seen(&log, "o/r", 7, 3), Some(vec![(2, true)]));
    // After the push only the new head's run is the subject's.
    assert_eq!(seen(&log, "o/r", 7, 9), Some(vec![(1, false)]));
    // Same number in another repo never sees o/r's runs or a sibling's.
    assert_eq!(seen(&log, "x/y", 7, 9), Some(vec![]));
    assert_eq!(seen(&log, "o/r", 8, 9), None);
    // Repo and sha compare case-insensitively.
    let log =
        CiLog::new(&[run("ABC", 1, 1, 1, 1, Some("success"))], &[head("O/R", 7, 0, Some("abc"))]);
    assert_eq!(seen(&log, "o/r", 7, 9), Some(vec![(1, false)]));
}

#[test]
fn a_run_ingested_late_never_reaches_an_earlier_cutoff() {
    let late = run("a", 1, 1, 2, 20, Some("failure"));
    let heads = [head("o/r", 7, 0, Some("a"))];
    let log = CiLog::new(&[late], &heads);
    assert_eq!(seen(&log, "o/r", 7, 10), Some(vec![]));
    assert_eq!(seen(&log, "o/r", 7, 21), Some(vec![(2, true)]));
    // Duplicate delivery keeps the earliest observation.
    let log = CiLog::new(
        &[
            run("a", 1, 1, 2, 20, Some("failure")),
            run("a", 1, 1, 2, 3, Some("failure")),
        ],
        &heads,
    );
    assert_eq!(seen(&log, "o/r", 7, 10), Some(vec![(2, true)]));
    assert_eq!(seen(&log, "o/r", 7, 10).unwrap().len(), 1);
}

#[test]
fn conclusions_map_to_outcomes_and_the_rest_are_ignored() {
    let heads = [head("o/r", 7, 0, Some("a"))];
    for (c, want) in [
        (Some("success"), Some(false)),
        (Some("FAILURE"), Some(true)),
        (Some("timed_out"), Some(true)),
        (Some("startup_failure"), Some(true)),
        (Some("cancelled"), None),
        (Some("neutral"), None),
        (Some("skipped"), None),
        (Some("action_required"), None),
        (Some("bogus"), None),
        (None, None),
    ] {
        let log = CiLog::new(&[run("a", 1, 1, 2, 2, c)], &heads);
        let got = seen(&log, "o/r", 7, 9).unwrap();
        assert_eq!(got.first().map(|o| o.1), want, "{c:?}");
    }
    // A later cancelled run does not replace the last completed outcome.
    let log = CiLog::new(
        &[
            run("a", 1, 1, 2, 2, Some("failure")),
            run("a", 2, 1, 4, 4, Some("cancelled")),
        ],
        &heads,
    );
    assert_eq!(seen(&log, "o/r", 7, 9), Some(vec![(2, true)]));
}

#[test]
fn ordering_ties_and_reruns_are_deterministic() {
    let heads = [head("o/r", 7, 0, Some("a"))];
    // Same completion instant: the higher (run_id, attempt) is last, whatever
    // the delivery order.
    let a = run("a", 1, 1, 2, 2, Some("failure"));
    let b = run("a", 2, 1, 2, 2, Some("success"));
    let fwd = CiLog::new(&[a.clone(), b.clone()], &heads);
    let rev = CiLog::new(&[b, a], &heads);
    assert_eq!(seen(&fwd, "o/r", 7, 9), seen(&rev, "o/r", 7, 9));
    assert_eq!(seen(&fwd, "o/r", 7, 9), Some(vec![(2, true), (2, false)]));
    // A rerun supersedes only once it completes and is known.
    let log = CiLog::new(
        &[
            run("a", 1, 1, 2, 2, Some("failure")),
            run("a", 1, 2, 6, 6, Some("success")),
        ],
        &heads,
    );
    assert_eq!(seen(&log, "o/r", 7, 5), Some(vec![(2, true)]));
    assert_eq!(seen(&log, "o/r", 7, 9), Some(vec![(2, true), (6, false)]));
}

#[test]
fn from_state_needs_a_head_and_a_finished_run() {
    use crate::eta::fleet_signoz_timeline::CiRunState;
    use crate::eta::fleet_signoz_timeline_rows::CiRunRow;
    let state = |head: Option<&str>, status: Option<&str>| CiRunState {
        run: CiRunRow {
            run_id: 9,
            run_attempt: 2,
            workflow: "ci".into(),
            git_ref: Some("refs/heads/x".into()),
            head_sha: head.map(str::to_string),
            status: status.map(str::to_string),
            conclusion: Some("success".into()),
            completed_at: t(1),
            duration_ms: None,
        },
        observed_at: t(3),
        jobs: vec![],
        duration_samples: vec![],
    };
    let r = CiRecord::from_state("O/R", &state(Some("AbC"), Some("completed"))).unwrap();
    assert_eq!((r.repo.as_str(), r.head_sha.as_str()), ("o/r", "abc"));
    assert_eq!((r.completed_at, r.known_at, r.run_attempt), (t(1), t(3), 2));
    // A branch ref names no PR.
    assert_eq!((r.git_ref.as_deref(), r.pr), (Some("refs/heads/x"), None));
    assert!(CiRecord::from_state("o/r", &state(None, None)).is_none());
    assert!(CiRecord::from_state("o/r", &state(Some("a"), Some("in_progress"))).is_none());
}

#[test]
fn the_log_round_trips_both_timestamps_and_dedupes() {
    let dir = tempfile::tempdir().unwrap();
    assert!(load(dir.path()).is_empty());
    let a = run("a", 1, 1, 2, 5, Some("failure"));
    let b = run("a", 2, 1, 3, 6, None);
    append(dir.path(), &[a.clone(), b.clone()]).unwrap();
    assert_eq!(load(dir.path()), vec![a.clone(), b.clone()]);
    let mut again = a.clone();
    again.known_at = t(50);
    let c = run("b", 3, 1, 4, 7, Some("success"));
    assert_eq!(fresh(&load(dir.path()), &[again, c.clone()]), vec![c.clone()]);
    // A garbled line is skipped, not fatal.
    std::fs::write(
        log_path(dir.path()),
        format!("{}\nnot json\n", serde_json::to_string(&a).unwrap()),
    )
    .unwrap();
    assert_eq!(load(dir.path()), vec![a]);
    // Small logs are not compacted.
    compact(dir.path(), t(100_000)).unwrap();
    assert_eq!(load(dir.path()).len(), 1);
}

#[test]
fn pr_refs_parse_strictly() {
    assert_eq!(pr_of_ref("refs/pull/12/merge"), Some(12));
    assert_eq!(pr_of_ref("refs/pull/12/head"), Some(12));
    assert_eq!(pr_of_ref("pull/12/merge"), Some(12));
    for bad in [
        "",
        "main",
        "refs/heads/main",
        "refs/pull/0/merge",
        "refs/pull/x/merge",
        "refs/pull/12",
        "refs/pull/12/other",
        "refs/pull/12/merge/x",
        "refs/pull//merge",
        "refs/pull/-1/merge",
    ] {
        assert_eq!(pr_of_ref(bad), None, "{bad}");
    }
}

#[test]
fn a_run_must_name_the_subject_pr_not_just_its_head() {
    let heads = [head("o/r", 7, 0, Some("a")), head("o/r", 8, 0, Some("a"))];
    let subject = run("a", 1, 1, 1, 1, Some("success"));
    let base = CiLog::new(std::slice::from_ref(&subject), &heads);
    assert_eq!(seen(&base, "o/r", 7, 20), Some(vec![(1, false)]));
    // Newer failing runs at the same SHA from a branch, another PR, a
    // missing ref and a malformed ref change nothing for PR 7.
    let mut branch = run("a", 2, 1, 5, 5, Some("failure"));
    (branch.git_ref, branch.pr) = (Some("refs/heads/feature/x".into()), None);
    let mut other = run("a", 3, 1, 6, 6, Some("failure"));
    (other.git_ref, other.pr) = (Some("refs/pull/8/merge".into()), Some(8));
    let mut missing = run("a", 4, 1, 7, 7, Some("failure"));
    (missing.git_ref, missing.pr) = (None, None);
    let mut garbled = run("a", 5, 1, 8, 8, Some("failure"));
    (garbled.git_ref, garbled.pr) = (Some("refs/pull/7/merge/x".into()), None);
    let log = CiLog::new(&[subject, branch, other.clone(), missing, garbled], &heads);
    assert_eq!(seen(&log, "o/r", 7, 20), Some(vec![(1, false)]));
    // The other PR still sees its own run.
    assert_eq!(seen(&log, "o/r", 8, 20), Some(vec![(6, true)]));
}

#[test]
fn old_lines_without_identity_load_but_never_count() {
    let dir = tempfile::tempdir().unwrap();
    let old = r#"{"repo":"o/r","head_sha":"a","run_id":1,"run_attempt":1,"workflow":"ci","completed_at":"2026-10-01T01:00:00Z","known_at":"2026-10-01T01:00:00Z","conclusion":"failure"}"#;
    std::fs::create_dir_all(log_path(dir.path()).parent().unwrap()).unwrap();
    std::fs::write(log_path(dir.path()), format!("{old}\n")).unwrap();
    let loaded = load(dir.path());
    assert_eq!(loaded.len(), 1);
    assert_eq!(loaded[0].pr, None);
    let log = CiLog::new(&loaded, &[head("o/r", 7, 0, Some("a"))]);
    assert_eq!(seen(&log, "o/r", 7, 20), Some(vec![]));
}
