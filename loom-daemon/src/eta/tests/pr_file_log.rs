//! The per-PR file-list log (#10550): parsing, the budgeted read plan,
//! change-only logging and persistence beside the fleet snapshots.

use crate::eta::loop_features::FileSnapshot;
use crate::eta::pr_file_log::{
    append, compact, load, log_path, parse_files, parse_page, plan, refresh, Candidate, ReadClock,
    MAX_LISTED_FILES,
};
use chrono::{DateTime, Duration, TimeZone, Utc};
use serde_json::{json, Value};
use std::cell::Cell;

fn t(h: i64) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 10, 1, 0, 0, 0).unwrap() + Duration::hours(h)
}

fn cand(pr: u32, updated: Option<i64>) -> Candidate {
    Candidate {
        repo: "o/r".into(),
        pr,
        updated_at: updated.map(t),
    }
}

fn body(files: &[&str]) -> Value {
    Value::Array(files.iter().map(|f| json!({ "filename": f })).collect())
}

/// A page as GitHub returns it: each entry names the head it was read at.
fn body_at(head: &str, files: &[&str]) -> Value {
    Value::Array(
        files
            .iter()
            .map(|f| {
                json!({
                    "filename": f,
                    "blob_url": format!("https://github.com/o/r/blob/{head}/{f}"),
                    "contents_url": format!("https://api.github.com/repos/o/r/contents/{f}?ref={head}"),
                })
            })
            .collect(),
    )
}

fn full_page(head: &str) -> Value {
    let full: Vec<String> = (0..MAX_LISTED_FILES).map(|i| format!("f{i}")).collect();
    let refs: Vec<&str> = full.iter().map(String::as_str).collect();
    body_at(head, &refs)
}

#[test]
fn a_page_parses_sorted_and_a_full_page_is_refused() {
    assert_eq!(parse_files(&body(&["b", "a", "a"])), Some(vec!["a".into(), "b".into()]));
    assert_eq!(parse_files(&json!({"message": "Not Found"})), None);
    assert_eq!(parse_files(&json!([{"sha": "x"}])), None);
    let full: Vec<String> = (0..MAX_LISTED_FILES).map(|i| format!("f{i}")).collect();
    let refs: Vec<&str> = full.iter().map(String::as_str).collect();
    assert_eq!(parse_files(&body(&refs)), None, "a possibly truncated list is not a list");
}

#[test]
fn the_plan_reads_never_read_first_then_the_stalest_within_budget() {
    let log = vec![
        FileSnapshot {
            repo: "o/r".into(),
            pr: 1,
            known_at: t(1),
            files: vec![],
            head_sha: None,
            complete: true,
            additions: None,
            deletions: None,
            listed: None,
        },
        FileSnapshot {
            repo: "O/R".into(),
            pr: 2,
            known_at: t(5),
            files: vec![],
            head_sha: None,
            complete: true,
            additions: None,
            deletions: None,
            listed: None,
        },
    ];
    let cands = [
        cand(1, Some(9)),
        cand(2, Some(9)),
        cand(3, None),
        cand(4, Some(0)),
    ];
    let clock = ReadClock::default();
    let picked: Vec<u32> = plan(&cands, &log, &clock, 3).iter().map(|c| c.pr).collect();
    assert_eq!(picked, vec![3, 4, 1], "never read, then stalest; budget 3 of 4");
    let all: Vec<u32> = plan(&cands, &log, &clock, 10)
        .iter()
        .map(|c| c.pr)
        .collect();
    assert_eq!(all, vec![3, 4, 1, 2]);
    assert!(plan(&cands, &log, &clock, 0).is_empty());
}

#[test]
fn a_pr_not_updated_since_its_last_read_is_left_alone() {
    let log = vec![FileSnapshot {
        repo: "o/r".into(),
        pr: 1,
        known_at: t(5),
        files: vec![],
        head_sha: None,
        complete: true,
        additions: None,
        deletions: None,
        listed: None,
    }];
    let clock = ReadClock::default();
    assert!(plan(&[cand(1, Some(5)), cand(1, Some(4))], &log, &clock, 9).is_empty());
    assert_eq!(plan(&[cand(1, Some(6))], &log, &clock, 9).len(), 1);
}

#[test]
fn a_pass_logs_only_changed_lists_stamped_at_the_read() {
    let mut clock = ReadClock::default();
    let calls = Cell::new(0);
    let fetch = |c: &Candidate| {
        calls.set(calls.get() + 1);
        match c.pr {
            1 => Some(body(&["a", "b"])),
            2 => None,
            _ => Some(body(&[])),
        }
    };
    let first = refresh(
        &[cand(1, None), cand(2, None), cand(3, None)],
        &[],
        &mut clock,
        9,
        || t(10),
        fetch,
    );
    assert_eq!(calls.get(), 3);
    let prs: Vec<u32> = first.iter().map(|s| s.pr).collect();
    assert_eq!(prs, vec![1, 3], "a failed read logs nothing");
    assert!(first.iter().all(|s| s.known_at == t(10)));
    assert_eq!(first[0].files, vec!["a".to_string(), "b".to_string()]);

    // Later: #1 updated but the same list; #2 retried and answers.
    let log = first.clone();
    let second = refresh(
        &[cand(1, Some(11)), cand(2, None)],
        &log,
        &mut clock,
        9,
        || t(12),
        |c| {
            Some(if c.pr == 1 {
                body(&["b", "a"])
            } else {
                body(&["c"])
            })
        },
    );
    assert_eq!(second.len(), 1);
    assert_eq!((second[0].pr, second[0].known_at), (2, t(12)));
    // The unchanged read still advanced the clock: not re-read until updated again.
    assert!(plan(&[cand(1, Some(11))], &log, &clock, 9).is_empty());
    // A changed list is a new snapshot; the old one stays for earlier rows.
    let third =
        refresh(&[cand(1, Some(13))], &log, &mut clock, 9, || t(14), |_| Some(body(&["a"])));
    assert_eq!(third.len(), 1);
    assert_eq!(third[0].files, vec!["a".to_string()]);
}

#[test]
fn a_truncated_list_logs_an_incomplete_observation_not_its_paths() {
    let mut clock = ReadClock::default();
    let out = refresh(&[cand(1, None)], &[], &mut clock, 9, || t(1), |_| Some(full_page("h1")));
    assert_eq!(out.len(), 1);
    assert!(!out[0].complete, "a full page is unknown, not a list");
    assert!(out[0].files.is_empty());
    assert_eq!((out[0].known_at, out[0].head_sha.as_deref()), (t(1), Some("h1")));
    // Marked read: not re-read until updated.
    assert!(plan(&[cand(1, Some(0))], &out, &clock, 9).is_empty());
}

#[test]
fn the_page_names_its_head_and_a_mixed_head_page_is_incomplete() {
    let page = parse_page(&body_at("abc", &["b", "a"])).unwrap();
    assert_eq!(page.files, Some(vec!["a".into(), "b".into()]));
    assert_eq!(page.head_sha.as_deref(), Some("abc"));
    // A push landed mid-read: the entries disagree on the head.
    let mut rows = body_at("h1", &["a"]).as_array().unwrap().clone();
    rows.extend(body_at("h2", &["b"]).as_array().unwrap().clone());
    let mixed = parse_page(&Value::Array(rows.clone())).unwrap();
    assert_eq!((mixed.files, mixed.head_sha), (None, None));
    let mut clock = ReadClock::default();
    let out = refresh(
        &[cand(1, None)],
        &[],
        &mut clock,
        9,
        || t(1),
        |_| Some(Value::Array(rows.clone())),
    );
    assert_eq!(out.len(), 1);
    assert!(!out[0].complete, "a head race is logged unknown");
}

#[test]
fn distinct_heads_with_the_same_paths_keep_their_identity_history() {
    let mut clock = ReadClock::default();
    let first =
        refresh(&[cand(1, None)], &[], &mut clock, 9, || t(1), |_| Some(body_at("h1", &["a"])));
    let second = refresh(
        &[cand(1, Some(2))],
        &first,
        &mut clock,
        9,
        || t(3),
        |_| Some(body_at("h2", &["a"])),
    );
    assert_eq!(second.len(), 1, "a new head is a change even with equal paths");
    assert_eq!(second[0].head_sha.as_deref(), Some("h2"));
    let log: Vec<_> = first.iter().chain(&second).cloned().collect();
    let third = refresh(
        &[cand(1, Some(4))],
        &log,
        &mut clock,
        9,
        || t(5),
        |_| Some(body_at("h2", &["a"])),
    );
    assert!(third.is_empty(), "the same head and paths append nothing");
}

#[test]
fn complete_then_truncated_then_complete_survives_a_restart() {
    use crate::eta::loop_features::{loop_features, LoopInputs};
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    // Each pass is a fresh process: a new clock, the log read back from disk.
    let pass = |updated: i64, at: i64, page: Value| {
        let mut clock = ReadClock::default();
        let log = load(root);
        let fresh = refresh(
            &[cand(1, Some(updated))],
            &log,
            &mut clock,
            9,
            || t(at),
            |_| Some(page.clone()),
        );
        append(root, &fresh).unwrap();
        fresh.len()
    };
    assert_eq!(pass(0, 1, body_at("h1", &["a.rs"])), 1);
    assert_eq!(pass(2, 3, full_page("h2")), 1, "the grown list is logged unknown");
    // Not updated since: a restarted reader does not re-read it.
    assert!(plan(&[cand(1, Some(2))], &load(root), &ReadClock::default(), 9).is_empty());
    assert_eq!(pass(4, 5, body_at("h3", &["b.rs"])), 1);

    let log = load(root);
    assert_eq!(log.iter().map(|s| s.complete).collect::<Vec<_>>(), vec![true, false, true]);
    // The consumer: #1 alone in the repo, so its overlap is known exactly
    // when its own list is.
    let known = |h: i64| {
        loop_features(
            &LoopInputs {
                repo: "o/r",
                pr: 1,
                own: &[],
                repo_episodes: &[],
                files: Some(&log),
                ci: None,
            },
            t(h),
        )
        .overlap_prs
        .is_some()
    };
    assert!(known(2), "before the truncated read the older complete list serves");
    assert!(!known(4), "after it the older list is not served as current");
    assert!(known(6), "a later complete read restores it");
}

#[test]
fn the_log_round_trips_beside_the_fleet_snapshots_and_compacts() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    assert!(load(root).is_empty());
    let a = FileSnapshot {
        repo: "o/r".into(),
        pr: 1,
        known_at: t(1),
        files: vec!["x".into()],
        head_sha: None,
        complete: true,
        additions: None,
        deletions: None,
        listed: None,
    };
    let b = FileSnapshot {
        repo: "o/r".into(),
        pr: 2,
        known_at: t(2),
        files: vec![],
        head_sha: None,
        complete: true,
        additions: None,
        deletions: None,
        listed: None,
    };
    append(root, std::slice::from_ref(&a)).unwrap();
    append(root, std::slice::from_ref(&b)).unwrap();
    std::fs::OpenOptions::new()
        .append(true)
        .open(log_path(root))
        .and_then(|mut f| std::io::Write::write_all(&mut f, b"not json\n"))
        .unwrap();
    assert_eq!(load(root), vec![a, b], "a bad line is skipped");
    // Small logs are never rewritten.
    compact(root, t(100_000)).unwrap();
    assert_eq!(load(root).len(), 2);
    // Its extension keeps the snapshot listing to snapshots alone.
    assert!(crate::eta::fleet::load_all(root).is_empty());
}

fn stat_body(entries: &[(&str, u64, u64)]) -> Value {
    Value::Array(
        entries
            .iter()
            .map(|(f, a, d)| json!({ "filename": f, "additions": a, "deletions": d }))
            .collect(),
    )
}

#[test]
fn a_whole_page_sums_the_per_file_diff_stat_and_records_listed() {
    let page = parse_page(&stat_body(&[("a.rs", 6, 26), ("b.rs", 4, 0)])).unwrap();
    assert_eq!(page.additions, Some(10));
    assert_eq!(page.deletions, Some(26));
    assert_eq!(page.listed, 2);
    // An entry without the stat makes the totals unknown, not partial.
    let mixed = parse_page(&json!([
        { "filename": "a.rs", "additions": 1, "deletions": 1 },
        { "filename": "b.rs" }
    ]))
    .unwrap();
    assert_eq!((mixed.additions, mixed.deletions, mixed.listed), (None, None, 2));
}

#[test]
fn a_full_page_keeps_listed_but_no_paths_or_partial_totals() {
    let rows: Vec<Value> = (0..MAX_LISTED_FILES)
        .map(|i| json!({ "filename": format!("f{i}"), "additions": 1, "deletions": 1 }))
        .collect();
    let page = parse_page(&Value::Array(rows)).unwrap();
    assert!(page.files.is_none());
    assert_eq!((page.additions, page.deletions), (None, None));
    assert_eq!(page.listed as usize, MAX_LISTED_FILES);
}

#[test]
fn a_changed_stat_with_equal_paths_is_logged_and_old_lines_parse_as_unknown() {
    let fetch_a = |_: &Candidate| Some(stat_body(&[("a.rs", 1, 1)]));
    let fetch_b = |_: &Candidate| Some(stat_body(&[("a.rs", 9, 1)]));
    let mut clock = ReadClock::default();
    let first = refresh(&[cand(1, None)], &[], &mut clock, 6, || t(1), fetch_a);
    assert_eq!(first.len(), 1);
    assert_eq!(first[0].additions, Some(1));
    assert_eq!(first[0].listed, Some(1));
    let same = refresh(&[cand(1, Some(2))], &first, &mut clock, 6, || t(3), fetch_a);
    assert!(same.is_empty());
    let changed = refresh(&[cand(1, Some(4))], &first, &mut clock, 6, || t(5), fetch_b);
    assert_eq!(changed.len(), 1);
    assert_eq!(changed[0].additions, Some(9));

    let old: FileSnapshot = serde_json::from_str(
        r#"{"repo":"o/r","pr":1,"known_at":"2026-10-07T00:00:00Z","files":["a.rs"]}"#,
    )
    .unwrap();
    assert_eq!((old.additions, old.deletions, old.listed), (None, None, None));
    assert!(old.complete);
}
