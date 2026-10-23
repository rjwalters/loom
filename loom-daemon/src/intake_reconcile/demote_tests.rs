//! Tests for the demotion of issues opened past intake (#11820): the pure
//! selector and "opened with" test, and the step against a scripted forge.

use super::*;
use std::collections::BTreeMap;

fn now() -> DateTime<Utc> {
    "2026-10-13T12:00:00Z".parse().unwrap()
}

fn at(s: &str) -> DateTime<Utc> {
    s.parse().unwrap()
}

/// An issue opened by `alice` at `created`.
fn row(n: u32, created: &str, labels: &[&str]) -> IntakeRow {
    IntakeRow {
        number: n,
        created_at: Some(at(created)),
        labels: labels.iter().map(|s| (*s).to_string()).collect(),
        author: Some("alice".into()),
    }
}

fn ev(label: &str, actor: &str, when: &str) -> LabelEvent {
    LabelEvent {
        label: label.into(),
        actor: Some(actor.into()),
        created_at: Some(at(when)),
    }
}

fn numbers(rows: &[&IntakeRow]) -> Vec<u32> {
    rows.iter().map(|r| r.number).collect()
}

// ===== the selector =====

#[test]
fn only_issues_carrying_nothing_past_intake_are_candidates() {
    let rows = vec![
        row(1, "2026-10-13T08:00:00Z", &["loom:issue"]),
        row(2, "2026-10-13T08:00:00Z", &["loom:curated", "bug"]),
        row(3, "2026-10-13T08:00:00Z", &["loom:issue", "loom:curated", "loom:important"]),
        row(4, "2026-10-13T08:00:00Z", &["loom:issue", "loom:building"]),
        row(5, "2026-10-13T08:00:00Z", &["loom:curated", "loom:curating"]),
        row(6, "2026-10-13T08:00:00Z", &["loom:issue", "loom:architect"]),
        row(7, "2026-10-13T08:00:00Z", &["loom:issue", "loom:blocked"]),
        row(8, "2026-10-13T08:00:00Z", &["loom:epic", "loom:issue"]),
        row(9, "2026-10-13T08:00:00Z", &["loom:triage"]),
        row(10, "2026-10-13T08:00:00Z", &[]),
        row(11, "2026-10-13T08:00:00Z", &["loom:issue", "loom:triage"]),
        row(12, "2026-10-13T08:00:00Z", &["loom:very-important", "loom:curated"]),
    ];
    let picked = select_demote_candidates(&rows, now(), DEFAULT_WINDOW_SECS, 50, |_| false);
    assert_eq!(numbers(&picked), [1, 2, 3, 11, 12]);
}

#[test]
fn the_grace_and_the_window_bound_the_candidates_and_the_cap_takes_the_oldest() {
    let rows = vec![
        row(1, "2026-10-13T11:59:00Z", &["loom:issue"]), // 60 s old: the filer's grace
        row(2, "2026-10-12T11:00:00Z", &["loom:issue"]), // 25 h old: past the window
        row(3, "2026-10-13T10:00:00Z", &["loom:issue"]),
        row(4, "2026-10-13T09:00:00Z", &["loom:curated"]),
        row(5, "2026-10-13T11:00:00Z", &["loom:issue"]),
    ];
    let all = select_demote_candidates(&rows, now(), DEFAULT_WINDOW_SECS, 50, |_| false);
    assert_eq!(numbers(&all), [4, 3, 5]);
    let capped = select_demote_candidates(&rows, now(), DEFAULT_WINDOW_SECS, 2, |_| false);
    assert_eq!(numbers(&capped), [4, 3]);
    // A skipped (remembered) promotion does not take a cap slot.
    let skipped = select_demote_candidates(&rows, now(), DEFAULT_WINDOW_SECS, 2, |n| n == 4);
    assert_eq!(numbers(&skipped), [3, 5]);
    // No creation time: the window is unknown, never a candidate.
    let mut undated = row(6, "2026-10-13T10:00:00Z", &["loom:issue"]);
    undated.created_at = None;
    let undated_rows = [undated];
    assert!(
        select_demote_candidates(&undated_rows, now(), DEFAULT_WINDOW_SECS, 5, |_| false)
            .is_empty()
    );
}

// ===== "opened with" =====

#[test]
fn opened_with_needs_the_author_at_creation_for_every_carried_label() {
    let created = "2026-10-13T10:00:00Z";
    let issue = row(1, created, &["loom:issue"]);
    let curated = row(2, created, &["loom:curated"]);
    let both = row(3, created, &["loom:issue", "loom:curated", "bug"]);

    // The author's label at creation (GitHub stamps it at created_at).
    let at_open = [ev("loom:issue", "alice", created)];
    assert_eq!(opened_with(&issue, &at_open), Some(vec!["loom:issue".into()]));
    // Within the grace, by the author, and the login compared case-blind.
    let soon = [ev("loom:curated", "Alice", "2026-10-13T10:01:30Z")];
    assert_eq!(opened_with(&curated, &soon), Some(vec!["loom:curated".into()]));
    let both_open = [
        ev("bug", "alice", created),
        ev("loom:issue", "alice", created),
        ev("loom:curated", "alice", created),
    ];
    assert_eq!(
        opened_with(&both, &both_open),
        Some(vec!["loom:issue".into(), "loom:curated".into()])
    );

    // A promotion: another actor, or the author after the grace.
    assert_eq!(opened_with(&issue, &[ev("loom:issue", "champion[bot]", created)]), None);
    assert_eq!(opened_with(&issue, &[ev("loom:issue", "alice", "2026-10-13T10:05:00Z")]), None);
    // Opened with it, removed, then promoted later: the latest event rules.
    let relabelled = [
        ev("loom:issue", "alice", created),
        ev("loom:issue", "rjwalters", "2026-10-13T11:00:00Z"),
    ];
    assert_eq!(opened_with(&issue, &relabelled), None);
    // Both carried, one promoted: left alone.
    let half = [
        ev("loom:curated", "alice", created),
        ev("loom:issue", "champion[bot]", "2026-10-13T11:00:00Z"),
    ];
    assert_eq!(opened_with(&both, &half), None);
    // Missing facts never demote on a guess.
    assert_eq!(opened_with(&issue, &[]), None);
    let mut anonymous = issue.clone();
    anonymous.author = None;
    assert_eq!(opened_with(&anonymous, &at_open), None);
}

#[test]
fn parse_label_events_reads_the_jq_lines() {
    let events = parse_label_events(
        "loom:issue\talice\t2026-10-13T10:00:00Z\nbug\t\t2026-10-13T10:00:00Z\n\t\t\n",
    );
    assert_eq!(events.len(), 2);
    assert_eq!(events[0], ev("loom:issue", "alice", "2026-10-13T10:00:00Z"));
    assert_eq!(events[1].actor, None);
}

// ===== the step =====

/// A scripted forge recording every call.
#[derive(Default)]
struct Fake {
    events: BTreeMap<u32, Vec<LabelEvent>>,
    current: BTreeMap<u32, Option<Vec<String>>>,
    add_fails: bool,
    remove_fails: bool,
    calls: Vec<String>,
}

impl DemoteForge<str> for Fake {
    fn current_labels(&mut self, _: &str, number: u32) -> Result<Option<Vec<String>>> {
        self.calls.push(format!("reread {number}"));
        self.current
            .get(&number)
            .cloned()
            .ok_or_else(|| anyhow!("HTTP 502"))
    }
    fn add_triage(&mut self, _: &str, number: u32) -> Option<Vec<String>> {
        self.calls.push(format!("triage {number}"));
        (!self.add_fails).then(Vec::new)
    }
    fn label_events(&mut self, _: &str, number: u32) -> Result<Vec<LabelEvent>> {
        self.calls.push(format!("events {number}"));
        self.events
            .get(&number)
            .cloned()
            .ok_or_else(|| anyhow!("shed"))
    }
    fn remove_label(&mut self, _: &str, number: u32, label: &str) -> bool {
        self.calls.push(format!("unlabel {number} {label}"));
        !self.remove_fails
    }
    fn comment(&mut self, _: &str, number: u32, body: &str) -> bool {
        assert_eq!(body, DEMOTE_COMMENT);
        self.calls.push(format!("comment {number}"));
        true
    }
}

fn labels(names: &[&str]) -> Option<Vec<String>> {
    Some(names.iter().map(|s| (*s).to_string()).collect())
}

const CREATED: &str = "2026-10-13T10:00:00Z";

#[test]
fn an_issue_opened_with_either_label_is_demoted_with_one_comment() {
    let rows = [
        row(1, CREATED, &["loom:issue"]),
        row(2, CREATED, &["loom:curated", "bug"]),
        row(3, CREATED, &["loom:issue", "loom:curated"]),
    ];
    let mut forge = Fake::default();
    forge
        .events
        .insert(1, vec![ev("loom:issue", "alice", CREATED)]);
    forge
        .events
        .insert(2, vec![ev("loom:curated", "alice", CREATED)]);
    forge.events.insert(
        3,
        vec![
            ev("loom:issue", "alice", CREATED),
            ev("loom:curated", "alice", CREATED),
        ],
    );
    forge.current.insert(1, labels(&["loom:issue"]));
    forge.current.insert(2, labels(&["loom:curated", "bug"]));
    forge
        .current
        .insert(3, labels(&["loom:issue", "loom:curated"]));
    let picked: Vec<&IntakeRow> = rows.iter().collect();
    let out = demote_step(&mut forge, "r", "demote-test-both", "r", &picked);
    assert_eq!(
        out,
        DemoteOutcome {
            demoted: 3,
            reads: 3,
            stopped: false
        }
    );
    assert_eq!(
        forge.calls,
        [
            "events 1",
            "reread 1",
            "triage 1",
            "unlabel 1 loom:issue",
            "comment 1",
            "events 2",
            "reread 2",
            "triage 2",
            "unlabel 2 loom:curated",
            "comment 2",
            "events 3",
            "reread 3",
            "triage 3",
            "unlabel 3 loom:issue",
            "unlabel 3 loom:curated",
            "comment 3",
        ]
    );
}

#[test]
fn a_later_promotion_is_untouched_and_read_only_once() {
    let key = "demote-test-promotion";
    let rows = [
        row(1, CREATED, &["loom:issue"]),
        row(2, CREATED, &["loom:curated"]),
    ];
    let mut forge = Fake::default();
    forge
        .events
        .insert(1, vec![ev("loom:issue", "champion[bot]", "2026-10-13T11:00:00Z")]);
    forge
        .events
        .insert(2, vec![ev("loom:curated", "alice", "2026-10-13T10:30:00Z")]);
    let picked: Vec<&IntakeRow> = rows.iter().collect();
    let out = demote_step(&mut forge, "r", key, "r", &picked);
    assert_eq!(
        out,
        DemoteOutcome {
            demoted: 0,
            reads: 2,
            stopped: false
        }
    );
    assert_eq!(forge.calls, ["events 1", "events 2"], "no write at all");
    // Remembered: the next pass's selector skips both, so no read.
    assert!(candidates(key, &rows, now(), 50).is_empty());
    assert!(is_known_promotion(key, 1) && is_known_promotion(key, 2));
}

#[test]
fn a_failed_events_read_or_re_read_demotes_nothing_and_stops() {
    let rows = [
        row(1, CREATED, &["loom:issue"]),
        row(2, CREATED, &["loom:issue"]),
    ];
    let picked: Vec<&IntakeRow> = rows.iter().collect();
    let mut shed = Fake::default();
    let out = demote_step(&mut shed, "r", "demote-test-shed", "r", &picked);
    assert_eq!(
        out,
        DemoteOutcome {
            demoted: 0,
            reads: 1,
            stopped: true
        }
    );
    assert_eq!(shed.calls, ["events 1"]);
    // A failed read is not a remembered promotion.
    assert!(!is_known_promotion("demote-test-shed", 1));

    let mut blind = Fake::default();
    blind
        .events
        .insert(1, vec![ev("loom:issue", "alice", CREATED)]);
    let out = demote_step(&mut blind, "r", "demote-test-blind", "r", &picked);
    assert!(out.stopped && out.demoted == 0);
    assert_eq!(blind.calls, ["events 1", "reread 1"]);
}

#[test]
fn a_failed_write_posts_no_comment_and_stops() {
    let rows = [
        row(1, CREATED, &["loom:issue"]),
        row(2, CREATED, &["loom:issue"]),
    ];
    let picked: Vec<&IntakeRow> = rows.iter().collect();
    let script = |f: &mut Fake| {
        for n in [1, 2] {
            f.events.insert(n, vec![ev("loom:issue", "alice", CREATED)]);
            f.current.insert(n, labels(&["loom:issue"]));
        }
    };
    let mut add = Fake {
        add_fails: true,
        ..Fake::default()
    };
    script(&mut add);
    let out = demote_step(&mut add, "r", "demote-test-addfail", "r", &picked);
    assert!(out.stopped && out.demoted == 0);
    assert_eq!(add.calls, ["events 1", "reread 1", "triage 1"]);

    let mut remove = Fake {
        remove_fails: true,
        ..Fake::default()
    };
    script(&mut remove);
    let out = demote_step(&mut remove, "r", "demote-test-rmfail", "r", &picked);
    assert!(out.stopped && out.demoted == 0);
    assert_eq!(remove.calls, ["events 1", "reread 1", "triage 1", "unlabel 1 loom:issue"]);
}

#[test]
fn the_re_read_rules_and_a_part_done_demotion_only_fixes_labels() {
    let rows = [
        row(1, CREATED, &["loom:issue"]),
        row(2, CREATED, &["loom:issue"]),
        row(3, CREATED, &["loom:issue", "loom:triage"]),
    ];
    let mut forge = Fake::default();
    for n in [1, 2, 3] {
        forge
            .events
            .insert(n, vec![ev("loom:issue", "alice", CREATED)]);
    }
    // #1 was claimed since the listing; #2 was closed.
    forge
        .current
        .insert(1, labels(&["loom:issue", "loom:building"]));
    forge.current.insert(2, None);
    // #3: an earlier pass added loom:triage and then failed to remove.
    forge
        .current
        .insert(3, labels(&["loom:issue", "loom:triage"]));
    let picked: Vec<&IntakeRow> = rows.iter().collect();
    let out = demote_step(&mut forge, "r", "demote-test-reread", "r", &picked);
    assert_eq!(
        out,
        DemoteOutcome {
            demoted: 1,
            reads: 3,
            stopped: false
        }
    );
    assert_eq!(
        forge.calls,
        [
            "events 1",
            "reread 1",
            "events 2",
            "reread 2",
            "events 3",
            "reread 3",
            "unlabel 3 loom:issue"
        ]
    );
}

#[test]
fn a_demoted_issue_is_no_longer_a_candidate() {
    let rows = [
        row(1, CREATED, &["loom:triage"]),
        row(2, CREATED, &["loom:triage", "bug"]),
    ];
    assert!(candidates("demote-test-idem", &rows, now(), 50).is_empty());
}
