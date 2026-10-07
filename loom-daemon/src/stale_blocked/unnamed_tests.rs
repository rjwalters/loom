//! Fake-forge tests for the `loom:blocked-unnamed` queue pass (#10558).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::HashMap;

use serde_json::{json, Value};

use super::batch_tests::{fleet, row, Fake};
use super::budget::Floor;
use super::release::{Config, ReleaseForge};
use super::unnamed::{run, Report, REVIEW_MARKER, UNNAMED_LABEL};
use crate::comment_trust::TrustPolicy;
use crate::operator_decision::cli::IssueState;
use crate::park_record::apply::ParkForge;
use crate::sweep_registry::{PRLESS_HOLD_COMMENT_MARKER, QUARANTINE_COMMENT_MARKER};

#[derive(Default)]
struct Park {
    items: HashMap<u64, IssueState>,
    writes: Vec<String>,
}

impl ParkForge for Park {
    fn view(&mut self, number: u64) -> Result<IssueState, String> {
        self.items
            .get(&number)
            .cloned()
            .ok_or_else(|| "no fixture".to_string())
    }
    fn state(&mut self, _: Option<&str>, _: u64) -> Result<String, String> {
        panic!("never read")
    }
    fn set_body(&mut self, _: u64, _: &str) -> Result<(), String> {
        panic!("the queue pass never edits a body")
    }
    fn add_labels(&mut self, number: u64, labels: &[String]) -> Result<(), String> {
        self.writes
            .push(format!("add #{number} {}", labels.join(",")));
        self.items
            .get_mut(&number)
            .unwrap()
            .labels
            .extend(labels.iter().cloned());
        Ok(())
    }
    fn remove_label(&mut self, number: u64, label: &str) -> Result<(), String> {
        self.writes.push(format!("remove #{number} {label}"));
        if let Some(i) = self.items.get_mut(&number) {
            i.labels.retain(|l| l != label);
        }
        Ok(())
    }
}

#[derive(Default)]
struct Extra {
    archived: bool,
    comments: HashMap<u64, Vec<Value>>,
    /// `created_at` of the newest `loom:blocked` labeled event.
    blocked_at: HashMap<u64, String>,
}

impl ReleaseForge for Extra {
    fn archived(&mut self) -> Result<bool, String> {
        Ok(self.archived)
    }
    fn comments(&mut self, number: u64) -> Result<Vec<Value>, String> {
        Ok(self.comments.get(&number).cloned().unwrap_or_default())
    }
    fn labeled_events(&mut self, _: u64) -> Result<Vec<String>, String> {
        panic!("never read")
    }
    fn last_labeled_at(&mut self, number: u64, label: &str) -> Result<Option<String>, String> {
        assert_eq!(label, "loom:blocked");
        Ok(self.blocked_at.get(&number).cloned())
    }
    fn post_comment(&mut self, _: u64, _: bool, _: &str) -> Result<(), String> {
        panic!("the queue pass never comments")
    }
}

fn trusted(body: &str) -> Value {
    json!({"body": body, "user": {"login": "someone"}, "author_association": "MEMBER"})
}

/// Curator's outcome comment, posted at `at`.
fn review(at: &str, association: &str) -> Value {
    json!({
        "body": format!("Kept: waiting on the vendor.\n\n{REVIEW_MARKER} outcome=kept -->"),
        "user": {"login": "someone"},
        "author_association": association,
        "created_at": at,
    })
}

/// A reason record from an earlier hold, older than the current label.
fn stale_kept_body() -> String {
    crate::park_record::render_park(
        &[],
        Some("curator"),
        Some("2026-01-01T00:00:00Z"),
        Some("waiting on the vendor"),
    )
}

struct World {
    gather: Fake,
    park: Park,
    extra: Extra,
}

impl World {
    fn new() -> Self {
        Self {
            gather: Fake::default(),
            park: Park::default(),
            extra: Extra::default(),
        }
    }

    fn add(&mut self, number: u32, body: &str, labels: &[&str]) {
        let mut all = vec!["loom:blocked"];
        all.extend_from_slice(labels);
        self.gather.rows.push(row(number, body, 0, false, &all));
        if labels.contains(&UNNAMED_LABEL) {
            self.gather.unnamed.push(row(number, body, 0, false, &all));
        }
        self.park.items.insert(
            u64::from(number),
            IssueState {
                body: body.to_string(),
                labels: all.iter().map(|l| (*l).to_string()).collect(),
            },
        );
    }

    fn run_cfg(&mut self, max_writes: usize, dry_run: bool) -> Report {
        run(
            &mut self.gather,
            &mut self.park,
            &mut self.extra,
            &fleet(),
            &TrustPolicy::new(fleet(), None, Vec::new()),
            &Config {
                dry_run,
                max_writes,
                floor: Floor::default(),
            },
        )
    }

    fn run(&mut self) -> Report {
        self.run_cfg(20, false)
    }
}

fn skipped(r: &Report, k: &str) -> usize {
    r.skipped.get(k).copied().unwrap_or(0)
}

#[test]
fn an_undocumented_issue_is_queued_once_and_a_rerun_writes_nothing() {
    let mut w = World::new();
    w.add(5, "No blocker recorded.", &[]);
    let r = w.run();
    assert_eq!(r.queued, vec![5]);
    assert_eq!(w.park.writes, vec![format!("add #5 {UNNAMED_LABEL}")]);
    // The forge now lists the label; a second identical tick is silent.
    let labels = w.park.items[&5].labels.clone();
    let l: Vec<&str> = labels.iter().map(String::as_str).collect();
    w.gather.rows.clear();
    w.gather.unnamed.clear();
    w.park.writes.clear();
    w.add(5, "No blocker recorded.", &l[1..]);
    let r2 = w.run();
    assert!(r2.queued.is_empty() && r2.cleared.is_empty());
    assert_eq!(r2.already_queued, 1);
    assert!(w.park.writes.is_empty());
}

#[test]
fn legacy_daemon_hold_comments_are_skipped_and_counted() {
    for marker in [QUARANTINE_COMMENT_MARKER, PRLESS_HOLD_COMMENT_MARKER] {
        let mut w = World::new();
        w.add(5, "No blocker recorded.", &[]);
        w.extra
            .comments
            .insert(5, vec![trusted(&format!("{marker}\nheld"))]);
        let r = w.run();
        assert_eq!(skipped(&r, "daemon-hold"), 1, "{marker}");
        assert!(w.park.writes.is_empty());
    }
}

#[test]
fn an_old_legacy_hold_does_not_veto_a_later_bare_reblock() {
    for marker in [QUARANTINE_COMMENT_MARKER, PRLESS_HOLD_COMMENT_MARKER] {
        let mut w = World::new();
        w.add(5, "No blocker recorded.", &[]);
        w.extra.comments.insert(
            5,
            vec![json!({
                "body": format!("{marker}\nheld"),
                "user": {"login": "someone"},
                "author_association": "MEMBER",
                "created_at": "2026-01-01T00:00:00Z",
            })],
        );
        w.extra
            .blocked_at
            .insert(5, "2026-10-01T00:00:00Z".to_string());
        let r = w.run();
        assert_eq!(skipped(&r, "daemon-hold"), 0, "{marker}");
        assert_eq!(r.queued, vec![5], "{marker}");
    }
}

#[test]
fn a_legacy_hold_posted_after_the_label_still_vetoes() {
    let mut w = World::new();
    w.add(5, "No blocker recorded.", &[]);
    w.extra.comments.insert(
        5,
        vec![json!({
            "body": format!("{QUARANTINE_COMMENT_MARKER}\nheld"),
            "user": {"login": "someone"},
            "author_association": "MEMBER",
            "created_at": "2026-10-02T00:00:00Z",
        })],
    );
    w.extra
        .blocked_at
        .insert(5, "2026-10-01T00:00:00Z".to_string());
    let r = w.run();
    assert_eq!(skipped(&r, "daemon-hold"), 1);
    assert!(w.park.writes.is_empty());
}

#[test]
fn permanent_operator_and_claimed_issues_are_skipped() {
    let mut w = World::new();
    w.add(1, "x\n<!-- loom:permanent-block -->", &[]);
    w.add(2, "x", &["loom:operator"]);
    w.add(3, "x", &["loom:operator-only"]);
    w.add(4, "x", &["loom:operator-decision"]);
    w.add(5, "x", &["loom:building"]);
    w.add(6, "x", &["loom:curating"]);
    w.add(7, "x", &[]);
    w.extra.comments.insert(
        7,
        vec![json!({
            "body": "<!-- loom:permanent-block -->",
            "user": {"login": "x"},
            "author_association": "NONE"
        })],
    );
    let r = w.run();
    assert_eq!(skipped(&r, "permanent"), 2);
    assert_eq!(skipped(&r, "operator-hold"), 3);
    assert_eq!(skipped(&r, "active-claim"), 2);
    assert!(r.queued.is_empty());
    assert!(w.park.writes.is_empty());
}

#[test]
fn a_documented_block_is_never_queued() {
    let mut w = World::new();
    let named = crate::park_record::render_park(
        &[crate::park_record::BlockerRef::local(9)],
        Some("curator"),
        None,
        None,
    );
    let reasoned = crate::park_record::render_park(&[], Some("curator"), None, Some("waiting"));
    w.add(1, &named, &[]);
    w.add(2, &reasoned, &[]);
    w.gather
        .states
        .insert((None, 9), super::batch_tests::state("OPEN"));
    let r = w.run();
    assert!(r.queued.is_empty());
    assert!(w.park.writes.is_empty());
}

#[test]
fn an_unevaluated_read_writes_nothing() {
    let mut w = World::new();
    // One comment on the row, with no fixture: the evidence read fails.
    w.gather.rows.push(row(5, "x", 1, false, &["loom:blocked"]));
    w.park.items.insert(
        5,
        IssueState {
            body: "x".into(),
            labels: vec!["loom:blocked".into()],
        },
    );
    let r = w.run();
    assert_eq!(r.unevaluated.len(), 1);
    assert!(w.park.writes.is_empty());
}

#[test]
fn the_label_clears_once_named_reasoned_or_unblocked() {
    let mut w = World::new();
    let named = crate::park_record::render_park(
        &[crate::park_record::BlockerRef::local(9)],
        Some("curator"),
        None,
        None,
    );
    let reasoned = crate::park_record::render_park(&[], Some("curator"), None, Some("waiting"));
    w.add(1, &named, &[UNNAMED_LABEL]);
    w.add(2, &reasoned, &[UNNAMED_LABEL]);
    // #3 lost loom:blocked: only the label listing knows it.
    w.gather
        .unnamed
        .push(row(3, "x", 0, false, &[UNNAMED_LABEL]));
    w.park.items.insert(
        3,
        IssueState {
            body: "x".into(),
            labels: vec![UNNAMED_LABEL.into()],
        },
    );
    // #4 is still undocumented and keeps its label.
    w.add(4, "x", &[UNNAMED_LABEL]);
    w.gather
        .states
        .insert((None, 9), super::batch_tests::state("OPEN"));
    let r = w.run();
    let mut cleared = r.cleared.clone();
    cleared.sort_unstable();
    assert_eq!(cleared, vec![1, 2, 3]);
    assert_eq!(r.already_queued, 1);
    assert!(!w.park.writes.iter().any(|x| x.contains("#4")));
}

#[test]
fn the_per_pass_cap_defers_the_rest() {
    let mut w = World::new();
    for n in 1..=3 {
        w.add(n, "x", &[]);
    }
    let r = w.run_cfg(2, false);
    assert_eq!(r.queued.len(), 2);
    assert_eq!(skipped(&r, "write-cap"), 1);
}

#[test]
fn dry_run_and_archived_write_nothing() {
    let mut w = World::new();
    w.add(5, "x", &[]);
    let r = w.run_cfg(20, true);
    assert_eq!(r.queued, vec![5]);
    assert!(w.park.writes.is_empty());
    w.extra.archived = true;
    let r = w.run();
    assert!(r.archived && r.queued.is_empty());
    assert!(w.park.writes.is_empty());
}

#[test]
fn prs_are_never_queued() {
    let mut w = World::new();
    w.gather.rows.push(row(8, "x", 0, true, &["loom:blocked"]));
    let r = w.run();
    assert!(r.queued.is_empty());
}

/// Judge (#10602): park records are append-only. A reason record older than
/// the latest `loom:blocked` application belongs to an earlier hold, so a bare
/// re-block is undocumented again and is queued; a current one is not.
#[test]
fn a_stale_reason_record_does_not_document_a_bare_re_block() {
    let mut w = World::new();
    let old = crate::park_record::render_park(
        &[],
        Some("curator"),
        Some("2026-01-01T00:00:00Z"),
        Some("waiting on a ruling"),
    );
    w.add(1, &old, &[]);
    w.extra.blocked_at.insert(1, "2026-10-06T00:00:00Z".into());
    // Same record, written seconds before its own label write: current.
    let cur = crate::park_record::render_park(
        &[],
        Some("curator"),
        Some("2026-10-06T00:00:00Z"),
        Some("waiting on a ruling"),
    );
    w.add(2, &cur, &[]);
    w.extra.blocked_at.insert(2, "2026-10-06T00:00:20Z".into());
    // Already queued and still stale: kept, not cleared.
    w.add(3, &old, &[UNNAMED_LABEL]);
    w.extra.blocked_at.insert(3, "2026-10-06T00:00:00Z".into());
    let r = w.run();
    assert_eq!(r.queued, vec![1]);
    assert_eq!(r.already_queued, 1);
    assert!(r.cleared.is_empty());
    assert_eq!(w.park.writes, vec![format!("add #1 {UNNAMED_LABEL}")]);
}

/// #10161: a current body daemon-hold record is skipped structurally; a
/// released one (older than the label) is not, and once a body record exists
/// its legacy comment no longer vetoes either.
#[test]
fn only_a_current_body_daemon_hold_is_skipped() {
    use crate::sweep_registry::park_hold::{render_hold_record, PRLESS_HOLD_REASON};
    let mut w = World::new();
    w.add(1, &render_hold_record(PRLESS_HOLD_REASON, "2026-10-06T00:00:00Z"), &[]);
    w.extra.blocked_at.insert(1, "2026-10-06T00:00:05Z".into());
    w.add(2, &render_hold_record(PRLESS_HOLD_REASON, "2026-01-01T00:00:00Z"), &[]);
    w.extra.blocked_at.insert(2, "2026-10-06T00:00:00Z".into());
    w.extra
        .comments
        .insert(2, vec![trusted(&format!("{PRLESS_HOLD_COMMENT_MARKER}\nheld"))]);
    let r = w.run();
    assert_eq!(skipped(&r, "daemon-hold"), 1);
    assert_eq!(r.queued, vec![2]);
}

/// #9274: an unticked `## Dependencies` checklist (even all-unparseable) is
/// `Unticked`, a cited dependency, never queued as unnamed.
#[test]
fn an_unticked_checklist_issue_is_not_queued_as_unnamed() {
    let mut w = World::new();
    w.add(1, "## Dependencies\n\n- [ ] vendor sign-off on the pinout\n", &[]);
    w.add(2, "## Dependencies\n\n- [ ] vendor sign-off on the pinout\n", &[UNNAMED_LABEL]);
    let r = w.run();
    assert!(r.queued.is_empty());
    assert_eq!(r.cleared, vec![2]);
}

/// Judge (#10602) re-queue loop: Curator keeps the hold with the reason the
/// stale record already states, so `park-record apply` writes nothing and the
/// record still predates the label. Its trusted review comment, posted after
/// the label, stops the next tick from queueing the block again.
#[test]
fn keep_with_the_same_reason_is_not_re_queued_next_tick() {
    let body = stale_kept_body();
    let mut w = World::new();
    w.add(1, &body, &[]);
    w.extra.blocked_at.insert(1, "2026-10-06T00:00:00Z".into());
    let r = w.run();
    assert_eq!(r.queued, vec![1]);

    // Curator "kept": same reason, so the body is unchanged (no fresh `at=`).
    assert!(crate::park_record::apply::compose_body(
        &body,
        &[],
        Some("curator"),
        "2026-10-06T01:00:00Z",
        Some("waiting on the vendor"),
    )
    .is_none());
    // It removes the queue label and posts its outcome comment.
    w.gather.rows.clear();
    w.gather.unnamed.clear();
    w.park.writes.clear();
    w.add(1, &body, &[]);
    w.extra
        .comments
        .insert(1, vec![review("2026-10-06T01:00:00Z", "MEMBER")]);

    let r = w.run();
    assert!(r.queued.is_empty() && r.cleared.is_empty(), "{}", r.summary());
    assert_eq!(skipped(&r, "reviewed"), 1);
    assert!(r.summary().contains("reviewed=1"), "{}", r.summary());
    assert!(w.park.writes.is_empty(), "{:?}", w.park.writes);
}

/// A review comment counts only for the block it reviewed: one older than the
/// latest `loom:blocked` label (an earlier block), or an untrusted one, does
/// not suppress queueing.
#[test]
fn an_older_or_untrusted_review_does_not_suppress_queueing() {
    let body = stale_kept_body();
    let mut w = World::new();
    w.add(1, &body, &[]);
    w.extra.blocked_at.insert(1, "2026-10-06T00:00:00Z".into());
    w.extra
        .comments
        .insert(1, vec![review("2026-09-01T00:00:00Z", "MEMBER")]);
    w.add(2, &body, &[]);
    w.extra.blocked_at.insert(2, "2026-10-06T00:00:00Z".into());
    w.extra
        .comments
        .insert(2, vec![review("2026-10-06T01:00:00Z", "NONE")]);
    let r = w.run();
    let mut queued = r.queued.clone();
    queued.sort_unstable();
    assert_eq!(queued, vec![1, 2]);
    assert_eq!(skipped(&r, "reviewed"), 0);
}
