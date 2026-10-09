//! Which hold a PR is under, and who released it (#10958, Slice 1): the
//! classifier fixtures, the leak test (nothing at or after the cutoff moves
//! a row) and disk/in-memory parity.

use crate::eta::fleet_events::{EventKind, ItemKind, RawEvent, SOURCE_FORGE};
use crate::eta::hold_kind::{
    is_hold_label, HoldKind, HoldSpell, ReleaseBy, RepoHolds, SpellEnd, CLOSER_WINDOW_SEC,
    OPERATOR, OPERATOR_DECISION, OPERATOR_MECHANICAL, OPERATOR_ONLY,
};
use crate::eta::hold_marker_log::{
    self, parse_page, Coverage, HoldMarker, MarkerCursor, MarkerKind, MarkerSource, RepoCursor,
    MARKER_SCHEMA,
};
use chrono::{DateTime, Duration, TimeZone, Utc};
use serde_json::json;

const PR: u32 = 7;

fn t(m: i64) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 10, 1, 0, 0, 0).unwrap() + Duration::minutes(m)
}

fn ev(kind: EventKind, label: Option<&str>, at: i64, seq: u64) -> RawEvent {
    RawEvent::new(
        "o/r",
        PR,
        ItemKind::Pr,
        kind,
        label.map(str::to_string),
        t(at),
        SOURCE_FORGE,
        seq,
        t(at),
    )
}

fn add(label: &str, at: i64) -> RawEvent {
    ev(EventKind::LabelAdded, Some(label), at, u64::try_from(at).unwrap() * 10 + 1)
}

fn remove(label: &str, at: i64) -> RawEvent {
    ev(EventKind::LabelRemoved, Some(label), at, u64::try_from(at).unwrap() * 10 + 2)
}

fn marker(kind: MarkerKind, at: i64, head: Option<&str>) -> HoldMarker {
    HoldMarker {
        schema: MARKER_SCHEMA.into(),
        repo: "o/r".into(),
        pr: PR,
        thread: PR,
        kind,
        head: head.map(str::to_string),
        comment_id: u64::try_from(at).unwrap() + 1000,
        created_at: t(at),
        fetched_at: t(at),
        source: MarkerSource::Live,
    }
}

/// Covered from well before the fixtures through well after them.
fn covered() -> Coverage {
    Coverage {
        from: Some(t(-10_000)),
        through: Some(t(10_000)),
    }
}

fn holds(events: &[RawEvent], markers: &[HoldMarker]) -> RepoHolds {
    RepoHolds::new(events, markers, covered())
}

#[test]
fn the_kind_labels_are_hold_labels() {
    for l in [
        OPERATOR,
        OPERATOR_ONLY,
        OPERATOR_DECISION,
        OPERATOR_MECHANICAL,
        "loom:blocked",
    ] {
        assert!(is_hold_label(l), "{l}");
    }
    assert!(!is_hold_label("loom:pr"));
    assert!(!is_hold_label("loom:operator-priority"));
}

#[test]
fn label_only_holds_take_the_label_kind() {
    let h = holds(&[add(OPERATOR_ONLY, 0), add("loom:blocked", 1)], &[]);
    assert_eq!(h.hold_kind_at(PR, t(5)), Some(HoldKind::OperatorOnly));
    let h = holds(&[add("loom:blocked", 0)], &[]);
    assert_eq!(h.hold_kind_at(PR, t(5)), Some(HoldKind::Blocked));
    let h = holds(&[add(OPERATOR, 0), add(OPERATOR_DECISION, 2)], &[]);
    assert_eq!(h.hold_kind_at(PR, t(1)), Some(HoldKind::Operator));
    assert_eq!(h.hold_kind_at(PR, t(3)), Some(HoldKind::OperatorDecision));
    assert_eq!(h.hold_kind_at(PR, t(0)), None, "a label at the cutoff is not yet known");
    let h = holds(&[add("loom:pr", 0)], &[]);
    assert_eq!(h.hold_kind_at(PR, t(5)), None);
}

#[test]
fn critical_file_hold_released_by_a_human_with_no_closer() {
    let h = holds(
        &[add(OPERATOR, 10), remove(OPERATOR, 100)],
        &[marker(MarkerKind::CriticalFileHold, 9, Some("aa"))],
    );
    assert_eq!(h.hold_kind_at(PR, t(50)), Some(HoldKind::CriticalFile));
    let w = CLOSER_WINDOW_SEC / 60;
    // The closer window is still open: not yet decidable.
    let early = h.spells(PR, t(100 + w));
    assert_eq!(
        early[0].end,
        Some(SpellEnd::Released {
            at: t(100),
            by: ReleaseBy::Pending
        })
    );
    let s = &h.spells(PR, t(100 + w + 1))[0];
    assert_eq!((s.kind, s.label_kind), (HoldKind::CriticalFile, HoldKind::Operator));
    assert_eq!(s.head.as_deref(), Some("aa"));
    assert_eq!(
        s.end,
        Some(SpellEnd::Released {
            at: t(100),
            by: ReleaseBy::Human
        })
    );
    // A respected-release acknowledgement is still a human release.
    let h = holds(
        &[add(OPERATOR, 10), remove(OPERATOR, 100)],
        &[
            marker(MarkerKind::CriticalFileHold, 10, None),
            marker(MarkerKind::CriticalFileReleaseRespected, 105, None),
        ],
    );
    assert!(matches!(
        h.spells(PR, t(200))[0].end,
        Some(SpellEnd::Released {
            by: ReleaseBy::Human,
            ..
        })
    ));
}

#[test]
fn merge_risk_hold_cleared_by_champion() {
    let h = holds(
        &[add(OPERATOR, 10), remove(OPERATOR, 100)],
        &[
            marker(MarkerKind::MergeRiskHold, 10, None),
            marker(MarkerKind::MergeRiskHoldCleared, 101, None),
        ],
    );
    let s = &h.spells(PR, t(500))[0];
    assert_eq!(s.kind, HoldKind::MergeRisk);
    assert_eq!(
        s.end,
        Some(SpellEnd::Released {
            at: t(100),
            by: ReleaseBy::Champion
        })
    );
    // A cleared notice long after the release (posted at merge) does not make
    // a human release Champion's.
    let h = holds(
        &[add(OPERATOR, 10), remove(OPERATOR, 100)],
        &[
            marker(MarkerKind::MergeRiskHold, 10, None),
            marker(MarkerKind::MergeRiskHoldCleared, 400, None),
        ],
    );
    assert!(matches!(
        h.spells(PR, t(500))[0].end,
        Some(SpellEnd::Released {
            by: ReleaseBy::Human,
            ..
        })
    ));
}

#[test]
fn ac_hold_names_the_kind() {
    let h = holds(&[add(OPERATOR, 10)], &[marker(MarkerKind::AcHold, 12, Some("ab"))]);
    let s = h.hold_at(PR, t(20)).unwrap();
    assert_eq!((s.kind, s.head.as_deref()), (HoldKind::AcHold, Some("ab")));
}

#[test]
fn an_untrusted_marker_is_ignored_end_to_end() {
    let page = json!([{
        "id": 5,
        "issue_url": "https://api.github.com/repos/o/r/issues/7",
        "html_url": "https://github.com/o/r/pull/7#issuecomment-5",
        "body": "<!-- champion:merge-risk-hold -->",
        "created_at": "2026-10-01T00:12:00Z",
        "updated_at": "2026-10-01T00:12:00Z",
        "author_association": "NONE",
        "user": {"login": "drive-by", "type": "User"},
    }]);
    let trusts = |v: &serde_json::Value| v["author_association"] != "NONE";
    let p = parse_page("o/r", &page, &trusts, t(13), MarkerSource::Live).unwrap();
    assert!(p.markers.is_empty());
    let h = holds(&[add(OPERATOR, 10)], &p.markers);
    assert_eq!(h.hold_kind_at(PR, t(20)), Some(HoldKind::Operator));
}

#[test]
fn a_rehold_at_a_new_head_takes_the_newer_marker() {
    // Within one spell: re-armed at a new head.
    let h = holds(
        &[add(OPERATOR, 10)],
        &[
            marker(MarkerKind::MergeRiskHold, 10, Some("a1")),
            marker(MarkerKind::CriticalFileHold, 50, Some("b2")),
        ],
    );
    let s = h.hold_at(PR, t(60)).unwrap();
    assert_eq!((s.kind, s.head.as_deref()), (HoldKind::CriticalFile, Some("b2")));
    assert_eq!(h.hold_at(PR, t(40)).unwrap().head.as_deref(), Some("a1"));
    // Across spells: a released hold's marker does not carry into the next.
    let h = holds(
        &[add(OPERATOR, 10), remove(OPERATOR, 100), add(OPERATOR, 300)],
        &[marker(MarkerKind::MergeRiskHold, 10, Some("a1"))],
    );
    let spells = h.spells(PR, t(400));
    assert_eq!(spells.len(), 2);
    assert_eq!(spells[1].kind, HoldKind::Operator);
    assert_eq!(spells[1].start, t(300));
}

#[test]
fn a_human_putting_the_hold_back_after_a_release_marker_is_the_label_kind() {
    let h = holds(
        &[add(OPERATOR, 10)],
        &[
            marker(MarkerKind::CriticalFileHold, 10, None),
            marker(MarkerKind::CriticalFileReleaseRespected, 40, None),
        ],
    );
    assert_eq!(h.hold_kind_at(PR, t(50)), Some(HoldKind::Operator));
}

#[test]
fn merge_or_close_while_held_ends_the_spell() {
    let h = holds(
        &[add(OPERATOR, 10), ev(EventKind::Merged, None, 20, 999)],
        &[marker(MarkerKind::MergeRiskHold, 10, None)],
    );
    let spells = h.spells(PR, t(30));
    assert_eq!(spells.len(), 1);
    assert_eq!(spells[0].end, Some(SpellEnd::Merged { at: t(20) }));
    assert_eq!(h.hold_kind_at(PR, t(30)), None);
    let h = holds(&[add(OPERATOR, 10), ev(EventKind::Closed, None, 20, 999)], &[]);
    assert_eq!(h.spells(PR, t(30))[0].end, Some(SpellEnd::Closed { at: t(20) }));
}

#[test]
fn an_uncovered_spell_is_label_kind_and_marker_unknown() {
    let markers = [marker(MarkerKind::MergeRiskHold, 10, None)];
    let not_yet = Coverage {
        from: Some(t(0)),
        through: None,
    };
    let h = RepoHolds::new(&[add(OPERATOR, 10)], &markers, not_yet);
    let s = h.hold_at(PR, t(20)).unwrap();
    assert_eq!((s.kind, s.marker_known), (HoldKind::Operator, false));
    // A release the log does not cover stays pending.
    let h = RepoHolds::new(&[add(OPERATOR, 10), remove(OPERATOR, 20)], &markers, not_yet);
    assert!(matches!(
        h.spells(PR, t(500))[0].end,
        Some(SpellEnd::Released {
            by: ReleaseBy::Pending,
            ..
        })
    ));
    let h = holds(&[add(OPERATOR, 10)], &markers);
    assert!(h.hold_at(PR, t(20)).unwrap().marker_known);
}

/// The fixture timeline the leak test perturbs.
fn base() -> (Vec<RawEvent>, Vec<HoldMarker>) {
    (
        vec![
            add(OPERATOR, 10),
            remove(OPERATOR, 100),
            add(OPERATOR_ONLY, 200),
            add(OPERATOR, 250),
            remove(OPERATOR_ONLY, 300),
            remove(OPERATOR, 300),
            add("loom:blocked", 400),
        ],
        vec![
            marker(MarkerKind::MergeRiskHold, 9, Some("a")),
            marker(MarkerKind::MergeRiskHoldCleared, 105, None),
            marker(MarkerKind::CriticalFileHold, 210, Some("b")),
        ],
    )
}

fn spells_at(events: &[RawEvent], markers: &[HoldMarker], cutoff: DateTime<Utc>) -> Vec<HoldSpell> {
    holds(events, markers).spells(PR, cutoff)
}

#[test]
fn nothing_at_or_after_the_cutoff_changes_a_row() {
    let (events, markers) = base();
    for c in [
        5, 10, 11, 50, 100, 101, 104, 105, 106, 116, 130, 205, 211, 299, 300, 301, 330, 401,
    ] {
        let cutoff = t(c);
        let before = spells_at(&events, &markers, cutoff);
        // Perturb with every kind of fact, all knowable only at or after
        // the cutoff: labels, merges, closers, hold markers, a backfilled
        // marker whose `created_at` is after the cutoff.
        let mut e2 = events.clone();
        let mut m2 = markers.clone();
        for (i, d) in [0, 1, 7, 60].into_iter().enumerate() {
            let at = c + d;
            let seq = 50_000 + u64::try_from(i).unwrap();
            e2.push(ev(EventKind::LabelRemoved, Some(OPERATOR), at, seq));
            e2.push(ev(EventKind::LabelAdded, Some(OPERATOR_DECISION), at, seq + 10));
            e2.push(ev(EventKind::Merged, None, at, seq + 20));
            for k in MarkerKind::ALL {
                let mut m = marker(k, at, Some("ff"));
                m.comment_id += 7_000 + u64::try_from(i).unwrap() * 10;
                m.source = MarkerSource::Backfill;
                m.fetched_at = t(c - 1_000);
                m2.push(m);
            }
        }
        assert_eq!(spells_at(&e2, &m2, cutoff), before, "cutoff {c}");
    }
}

#[test]
fn the_disk_read_matches_the_in_memory_inputs() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let (events, markers) = base();
    let path = crate::eta::fleet_events::events_path(root, "o/r");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let lines: String = events
        .iter()
        .map(|e| serde_json::to_string(e).unwrap() + "\n")
        .collect();
    std::fs::write(&path, lines).unwrap();
    hold_marker_log::append(root, &markers).unwrap();
    // Another repo's rows never leak into this one.
    let mut other = marker(MarkerKind::CriticalFileHold, 15, None);
    other.repo = "p/q".into();
    hold_marker_log::append(root, &[other]).unwrap();
    let mut cursor = MarkerCursor::default();
    let mut rc = RepoCursor::start(t(10_000));
    rc.backfill_from = t(-10_000);
    rc.caught_up_at = Some(t(10_000));
    cursor.repos.insert("o/r".into(), rc);
    cursor.write(root).unwrap();
    let disk = RepoHolds::load(root, "O/R");
    let mem = holds(&events, &markers);
    for c in [20, 150, 250, 350, 500] {
        assert_eq!(disk.spells(PR, t(c)), mem.spells(PR, t(c)), "cutoff {c}");
    }
    assert_eq!(disk.hold_kind_at(PR, t(20)), Some(HoldKind::MergeRisk));
}
