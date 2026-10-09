//! The label-flag timeline (#10245): the one label → flag definition, the
//! shared `(at, seq)` replay, knowability at a row instant, retention, and a
//! snapshot written before the timeline existed.

use super::fit_rows::{
    approve, cutoff, h, landed, open, row, secs, snapshot, OPERATOR, REPO, STAR,
};
use crate::eta::episodes::StageEpisode;
use crate::eta::episodes::{EpisodeInput, LabelEvent, PrEnd};
use crate::eta::fit::rows;
use crate::eta::flag_timeline::{flag_changes_from_input, flags_before, prune, FlagChange};
use crate::eta::fleet::FleetSnapshot;
use crate::eta::labels::{
    pr_flags, FLAG_BLOCKED, FLAG_CI_FAIL, FLAG_CONFLICT, FLAG_OP_HOLD, FLAG_SEQUENCED, FLAG_STARRED,
};
use crate::pr_latency::history::fixtures::{labeled, t, unlabeled};
use crate::pr_latency::REVIEW_REQUESTED;
use chrono::{DateTime, Duration, Utc};

fn labels(names: &[&str]) -> Vec<String> {
    names.iter().map(|s| (*s).to_string()).collect()
}

fn event(at: DateTime<Utc>, seq: u64, label: &str, added: bool) -> LabelEvent {
    LabelEvent {
        at,
        seq,
        label: label.to_string(),
        added,
    }
}

fn input(events: Vec<LabelEvent>, end: PrEnd) -> EpisodeInput {
    EpisodeInput {
        repo: REPO.to_string(),
        pr_number: 7,
        events,
        end,
    }
}

fn change(at: DateTime<Utc>, flags: u8) -> FlagChange {
    FlagChange {
        pr_number: 7,
        at,
        flags,
    }
}

#[test]
fn pr_flags_is_one_bit_per_flag() {
    let table: [(&str, u8); 10] = [
        ("loom:operator", FLAG_OP_HOLD),
        ("loom:operator-only", FLAG_OP_HOLD),
        ("loom:operator-decision", FLAG_OP_HOLD),
        ("loom:sequenced", FLAG_SEQUENCED),
        ("loom:operator-priority", FLAG_STARRED),
        ("loom:merge-conflict", FLAG_CONFLICT),
        ("loom:ci-failure", FLAG_CI_FAIL),
        ("loom:blocked", FLAG_BLOCKED),
        ("loom:pr", 0),
        ("loom:operator-mechanical", 0),
    ];
    for (label, want) in table {
        assert_eq!(pr_flags(&labels(&[label])), want, "{label}");
    }
    let bits = [
        FLAG_OP_HOLD,
        FLAG_SEQUENCED,
        FLAG_STARRED,
        FLAG_CONFLICT,
        FLAG_CI_FAIL,
        FLAG_BLOCKED,
    ];
    assert_eq!(bits, [1, 2, 4, 8, 16, 32], "bit i is the i-th flag");
    let every: Vec<&str> = table.iter().map(|(l, _)| *l).collect();
    assert_eq!(pr_flags(&labels(&every)), 0b11_1111);
    assert_eq!(pr_flags(&[]), 0);
}

#[test]
fn every_pr_gets_an_entry_at_its_first_label_event_then_one_per_change() {
    let events = vec![
        event(t(100), 0, REVIEW_REQUESTED, true),
        event(t(200), 1, STAR, true),
        // Re-applied while in force, and a label that is not a flag: no entry.
        event(t(300), 2, STAR, true),
        event(t(400), 3, "loom:pr", true),
        event(t(500), 4, STAR, false),
    ];
    assert_eq!(
        flag_changes_from_input(&input(events, PrEnd::Open), t(10_000)),
        vec![
            change(t(100), 0),
            change(t(200), FLAG_STARRED),
            change(t(500), 0)
        ]
    );
    // No label event, no entry.
    assert!(flag_changes_from_input(&input(Vec::new(), PrEnd::Open), t(10_000)).is_empty());
}

#[test]
fn a_same_instant_remove_and_add_follows_the_at_seq_rule() {
    let base = || vec![event(t(100), 0, STAR, true)];
    // Removed then re-added in one edit: still starred, so no change.
    let mut kept = base();
    kept.push(event(t(200), 2, STAR, true));
    kept.push(event(t(200), 1, STAR, false));
    assert_eq!(
        flag_changes_from_input(&input(kept, PrEnd::Open), t(10_000)),
        vec![change(t(100), FLAG_STARRED)]
    );
    // Added then removed: unstarred at 200.
    let mut dropped = base();
    dropped.push(event(t(200), 2, STAR, false));
    dropped.push(event(t(200), 1, STAR, true));
    let mut reversed = dropped.clone();
    reversed.reverse();
    let want = vec![change(t(100), FLAG_STARRED), change(t(200), 0)];
    assert_eq!(flag_changes_from_input(&input(dropped, PrEnd::Open), t(10_000)), want);
    assert_eq!(
        flag_changes_from_input(&input(reversed, PrEnd::Open), t(10_000)),
        want,
        "input order does not matter"
    );
}

#[test]
fn nothing_at_or_after_the_merge_or_the_cut_is_a_change() {
    let events = vec![
        event(t(100), 0, REVIEW_REQUESTED, true),
        event(t(200), 1, OPERATOR, true),
        event(t(300), 2, STAR, true),
    ];
    let merged = input(events.clone(), PrEnd::Merged(t(300)));
    assert_eq!(
        flag_changes_from_input(&merged, t(10_000)),
        vec![change(t(100), 0), change(t(200), FLAG_OP_HOLD)]
    );
    assert_eq!(
        flag_changes_from_input(&input(events, PrEnd::Open), t(200)),
        vec![change(t(100), 0)],
        "only at < as_of"
    );
}

#[test]
fn a_flag_set_60s_before_t_is_not_in_force_at_t_and_one_set_121s_before_is() {
    let at = h(10.0);
    let lag = Duration::seconds(120);
    let changes = vec![
        change(h(1.0), 0),
        change(at - Duration::seconds(121), FLAG_STARRED),
        change(at - Duration::seconds(60), FLAG_STARRED | FLAG_CI_FAIL),
    ];
    assert_eq!(flags_before(&changes, at - lag), Some(FLAG_STARRED));
    assert_eq!(flags_before(&changes, h(1.0)), None);

    // End to end, through the snapshot and the row builder.
    let star_at = |offset: i64| {
        let mut events = vec![labeled(REVIEW_REQUESTED, secs(h(5.0)))];
        events.push(labeled(STAR, secs(at) - offset));
        events
    };
    let prs = vec![
        landed(90, h(1.0), h(2.0), h(3.0)),
        open(1, star_at(121)),
        open(2, star_at(60)),
    ];
    let a = rows::build(&[snapshot(REPO, &prs, cutoff() + Duration::hours(1))], cutoff());
    assert!(row(&a, REPO, 1, at).unwrap().inputs.starred);
    assert!(!row(&a, REPO, 2, at).unwrap().inputs.starred);
    assert!(
        row(&a, REPO, 2, at + Duration::minutes(30))
            .unwrap()
            .inputs
            .starred
    );
}

#[test]
fn pruning_keeps_the_state_in_force_at_the_floor() {
    let floor = t(1000);
    let other = FlagChange {
        pr_number: 8,
        at: t(100),
        flags: FLAG_BLOCKED,
    };
    let changes = vec![
        change(t(100), 0),
        change(t(500), FLAG_STARRED),
        change(t(1000), 0),
        change(t(1500), FLAG_CONFLICT),
        other,
    ];
    let mut kept = prune(changes, floor);
    kept.sort();
    assert_eq!(
        kept,
        vec![
            change(t(500), FLAG_STARRED),
            change(t(1000), 0),
            change(t(1500), FLAG_CONFLICT),
            other
        ]
    );

    // Through the snapshot: a long-lived PR keeps the flag it had at the floor.
    let day = |d: i64| d * 86_400;
    let retention = crate::eta::fleet::RETENTION_DAYS;
    let mut events = approve(t(0), t(3600));
    events.push(labeled(STAR, 7200));
    events.push(labeled(REVIEW_REQUESTED, day(retention + 5)));
    events.push(unlabeled("loom:pr", day(retention + 5)));
    let s = snapshot(REPO, &[open(5, events)], t(day(retention + 6)));
    let floor = s.as_of - Duration::days(retention);
    assert_eq!(
        s.flag_changes.iter().filter(|c| c.at < floor).count(),
        1,
        "{:?}",
        s.flag_changes
    );
    assert_eq!(flags_before(&s.flag_changes, floor), Some(FLAG_STARRED));
}

/// The id formula without flag lines: the samples' and the episodes'.
fn id_without_flags(s: &FleetSnapshot) -> String {
    let instant = crate::telemetry::trace::instant;
    let mut lines: Vec<String> = s
        .samples
        .iter()
        .map(|x| {
            format!(
                "{}|{}|{}|{}|{}|{}|{}|{}|{}",
                x.repo.to_ascii_lowercase(),
                x.stage.as_str(),
                x.pr_number,
                instant(x.entered_at),
                instant(x.observed_at),
                x.duration_sec,
                u8::from(x.censored),
                x.verdict.as_deref().unwrap_or(""),
                x.attempt.map(|a| a.to_string()).unwrap_or_default(),
            )
        })
        .collect();
    lines.extend(s.episodes.iter().map(StageEpisode::digest_line));
    let at = instant(s.as_of);
    let mut parts: Vec<&str> = vec!["loom.eta.fleet.snapshot", REPO, &at];
    parts.extend(lines.iter().map(String::as_str));
    crate::telemetry::trace::derived_hex(&parts, 16)
}

fn pre_timeline_snapshot() -> FleetSnapshot {
    let prs = vec![
        landed(90, h(1.0), h(2.0), h(3.0)),
        open(1, approve(h(5.0), h(9.0))),
    ];
    let mut s = snapshot(REPO, &prs, cutoff() + Duration::hours(1));
    assert!(!s.flag_changes.is_empty(), "a fresh snapshot has a timeline");
    s.flag_changes.clear();
    // A pre-#10245 file has no #10500 merges either.
    s.merges.clear();
    s.merge(&[], s.as_of);
    s
}

#[test]
fn a_snapshot_without_flag_changes_keeps_its_id_and_round_trips_byte_identically() {
    let old = pre_timeline_snapshot();
    assert_eq!(old.snapshot_id, id_without_flags(&old));
    let text = serde_json::to_string_pretty(&old).unwrap();
    assert!(!text.contains("flag_changes"), "{text}");
    assert!(!text.contains("\"merges\""), "{text}");
    let parsed: FleetSnapshot = serde_json::from_str(&text).unwrap();
    assert_eq!(parsed, old);
    assert_eq!(serde_json::to_string_pretty(&parsed).unwrap(), text);

    let dir = tempfile::tempdir().unwrap();
    let path = crate::eta::fleet::snapshot_path(dir.path(), REPO);
    crate::eta::fleet::write(&path, &old).unwrap();
    let bytes = std::fs::read(&path).unwrap();
    crate::eta::fleet::write(&path, &crate::eta::fleet::read(&path).unwrap()).unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), bytes);

    // With the timeline the id moves (flag lines are digested), and a re-read
    // PR replaces its changes rather than duplicating them.
    let with = snapshot(REPO, &[landed(90, h(1.0), h(2.0), h(3.0))], old.as_of);
    assert_ne!(with.snapshot_id, id_without_flags(&with));
    let mut reread = with.clone();
    reread.merge(&[landed(90, h(1.0), h(2.0), h(3.0))], old.as_of);
    assert_eq!(reread, with);
}

#[test]
fn a_pre_timeline_snapshots_rows_are_dropped_and_counted() {
    let old = pre_timeline_snapshot();
    let mut fresh = old.clone();
    fresh.merge(
        &[
            landed(90, h(1.0), h(2.0), h(3.0)),
            open(1, approve(h(5.0), h(9.0))),
        ],
        old.as_of,
    );
    assert!(!fresh.flag_changes.is_empty());
    let with = rows::build(&[fresh], cutoff());
    let without = rows::build(&[old], cutoff());
    assert!(!with.rows.is_empty());
    assert!(without.rows.is_empty(), "never zero-flagged");
    assert_eq!(
        without.stats.rows_dropped_no_flags,
        with.rows.len() + with.stats.rows_dropped_missing
    );
    assert_eq!(with.stats.rows_dropped_no_flags, 0);
    // The dwells do not read flags.
    assert_eq!(without.dwells, with.dwells);
}
