//! The per-PR file-list log (#10550): parsing, the budgeted read plan,
//! change-only logging and persistence beside the fleet snapshots.

use crate::eta::loop_features::FileSnapshot;
use crate::eta::pr_file_log::{
    append, compact, load, log_path, parse_files, plan, refresh, Candidate, ReadClock,
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
        },
        FileSnapshot {
            repo: "O/R".into(),
            pr: 2,
            known_at: t(5),
            files: vec![],
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
fn a_truncated_list_is_marked_read_but_never_logged() {
    let full: Vec<String> = (0..MAX_LISTED_FILES).map(|i| format!("f{i}")).collect();
    let refs: Vec<&str> = full.iter().map(String::as_str).collect();
    let mut clock = ReadClock::default();
    let out = refresh(&[cand(1, None)], &[], &mut clock, 9, || t(1), |_| Some(body(&refs)));
    assert!(out.is_empty());
    assert!(plan(&[cand(1, Some(0))], &[], &clock, 9).is_empty());
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
    };
    let b = FileSnapshot {
        repo: "o/r".into(),
        pr: 2,
        known_at: t(2),
        files: vec![],
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
