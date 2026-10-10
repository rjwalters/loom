//! Tests for the deterministic `loom:blocked` release pass (#10556), against
//! the counting [`super::batch_tests::Fake`] evidence forge plus recording
//! fakes for [`ParkForge`] and [`ReleaseForge`].

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::{HashMap, HashSet};

use serde_json::{json, Value};

use super::batch::{ClosingRef, RefState};
use super::batch_tests::{fleet, row, Fake};
use super::budget::{Budget, Floor};
use super::release::{marker, run, Config, ReleaseForge, Report};
use super::release_gh::{gated, Mode};
use crate::comment_trust::TrustPolicy;
use crate::operator_decision::cli::IssueState;
use crate::park_record::apply::ParkForge;
use crate::park_record::{blockers, BlockerRef};

/// [`crate::park_record::render_park`] over local numbers — every park these
/// fixtures render is local unless a test writes a qualified one by hand.
fn render_park(local: &[u64], by: Option<&str>, at: Option<&str>, reason: Option<&str>) -> String {
    let refs: Vec<BlockerRef> = local.iter().copied().map(BlockerRef::local).collect();
    crate::park_record::render_park(&refs, by, at, reason)
}
use crate::sweep_registry::PRLESS_HOLD_COMMENT_MARKER;

// --- fakes --------------------------------------------------------------------

#[derive(Default)]
struct Park {
    items: HashMap<u64, IssueState>,
    /// Remove-label calls that fail before one succeeds.
    remove_failures: usize,
    writes: Vec<String>,
    views: usize,
}

impl ParkForge for Park {
    fn view(&mut self, number: u64) -> Result<IssueState, String> {
        self.views += 1;
        self.items
            .get(&number)
            .cloned()
            .ok_or_else(|| format!("no fixture for #{number}"))
    }
    fn state(&mut self, _repo: Option<&str>, _number: u64) -> Result<String, String> {
        panic!("the release pass never reads state through ParkForge")
    }
    fn set_body(&mut self, number: u64, body: &str) -> Result<(), String> {
        self.writes.push(format!("body #{number}"));
        self.items.get_mut(&number).unwrap().body = body.to_string();
        Ok(())
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
        if self.remove_failures > 0 {
            self.remove_failures -= 1;
            return Err("HTTP 502".to_string());
        }
        self.writes.push(format!("remove #{number} {label}"));
        self.items
            .get_mut(&number)
            .unwrap()
            .labels
            .retain(|l| l != label);
        Ok(())
    }
}

#[derive(Default)]
struct Extra {
    archived: bool,
    comments: HashMap<u64, Vec<Value>>,
    comments_fail: bool,
    events: HashMap<u64, Vec<String>>,
    posted: Vec<(u64, String)>,
    reads: usize,
}

impl ReleaseForge for Extra {
    fn archived(&mut self) -> Result<bool, String> {
        Ok(self.archived)
    }
    fn comments(&mut self, number: u64) -> Result<Vec<Value>, String> {
        self.reads += 1;
        if self.comments_fail {
            return Err("HTTP 502".to_string());
        }
        Ok(self.comments.get(&number).cloned().unwrap_or_default())
    }
    fn labeled_events(&mut self, number: u64) -> Result<Vec<String>, String> {
        self.reads += 1;
        Ok(self.events.get(&number).cloned().unwrap_or_default())
    }
    fn post_comment(&mut self, number: u64, _is_pr: bool, body: &str) -> Result<(), String> {
        self.posted.push((number, body.to_string()));
        // What the forge would now return: a comment by the fleet's own App.
        self.comments.entry(number).or_default().push(trusted(body));
        Ok(())
    }
}

fn trusted(body: &str) -> Value {
    json!({"body": body, "user": {"login": "someone"}, "author_association": "MEMBER"})
}

fn untrusted(body: &str) -> Value {
    json!({"body": body, "user": {"login": "drive-by"}, "author_association": "NONE"})
}

// --- fixtures -----------------------------------------------------------------

/// The release pass over fakes; shared with `release_telemetry_tests`.
pub(super) struct World {
    gather: Fake,
    park: Park,
    extra: Extra,
}

impl World {
    pub(super) fn new() -> Self {
        Self {
            gather: Fake::default(),
            park: Park::default(),
            extra: Extra::default(),
        }
    }

    /// An open parked artifact whose body declares `declared`, listed and
    /// viewable, carrying `labels` plus `loom:blocked`.
    pub(super) fn parked(&mut self, number: u32, pr: bool, declared: &[u64], labels: &[&str]) {
        let body = format!(
            "Some work.\n\n{}\n",
            render_park(declared, Some("builder"), Some("2026-10-01T00:00:00Z"), None)
        );
        self.with_body(number, pr, &body, labels);
    }

    pub(super) fn with_body(&mut self, number: u32, pr: bool, body: &str, labels: &[&str]) {
        let mut all: Vec<&str> = vec!["loom:blocked"];
        all.extend_from_slice(labels);
        self.gather.rows.push(row(number, body, 0, pr, &all));
        self.park.items.insert(
            u64::from(number),
            IssueState {
                body: body.to_string(),
                labels: all.iter().map(|l| (*l).to_string()).collect(),
            },
        );
        if pr {
            self.gather
                .merge
                .insert(number, ("MERGEABLE".to_string(), "CLEAN".to_string()));
        }
    }

    pub(super) fn state(&mut self, number: i64, state: &str, is_pr: bool) {
        self.gather.states.insert(
            (None, number),
            RefState {
                state: state.to_string(),
                labels: Vec::new(),
                is_pr,
            },
        );
    }

    /// The budget probe's answer.
    pub(super) fn budget(&mut self, core_remaining: u64, graphql_remaining: u64) {
        self.gather.budget = Some(Budget {
            core_remaining,
            graphql_remaining,
        });
    }

    pub(super) fn run_with(&mut self, cfg: Config) -> Report {
        run(
            &mut self.gather,
            &mut self.park,
            &mut self.extra,
            &fleet(),
            &TrustPolicy::new(fleet(), None, Vec::new()),
            &cfg,
        )
    }

    pub(super) fn run(&mut self) -> Report {
        self.run_with(cfg())
    }

    fn labels(&self, number: u64) -> HashSet<String> {
        self.park.items[&number].labels.iter().cloned().collect()
    }

    fn no_writes(&self) -> bool {
        self.park.writes.is_empty() && self.extra.posted.is_empty()
    }
}

pub(super) fn cfg() -> Config {
    Config {
        dry_run: false,
        max_writes: 20,
        floor: Floor::default(),
    }
}

fn skipped(r: &Report, key: &str) -> usize {
    r.skipped.get(key).copied().unwrap_or(0)
}

// --- release ------------------------------------------------------------------

#[test]
fn all_declared_blockers_closed_releases_and_restores_prior_approval() {
    let mut w = World::new();
    w.parked(10, false, &[1, 2], &[]);
    w.state(1, "CLOSED", false);
    w.state(2, "CLOSED", false);
    w.extra.events.insert(
        10,
        vec![
            "loom:triage".into(),
            "loom:issue".into(),
            "loom:blocked".into(),
        ],
    );
    let r = w.run();
    assert_eq!(r.released.len(), 1, "{}", r.summary());
    assert_eq!(r.released[0].restored.as_deref(), Some("loom:issue"));
    assert!(r.released[0].applied && r.released[0].commented);
    // Comment first, then add the lane label, then drop loom:blocked.
    assert_eq!(w.extra.posted.len(), 1);
    assert!(w.extra.posted[0].1.contains(&marker(true, &[1, 2])));
    assert_eq!(w.park.writes, vec!["add #10 loom:issue", "remove #10 loom:blocked"]);
    assert!(w.labels(10).contains("loom:issue") && !w.labels(10).contains("loom:blocked"));
}

#[test]
fn never_approved_issue_is_released_without_loom_issue() {
    let mut w = World::new();
    w.parked(10, false, &[1], &[]);
    w.state(1, "CLOSED", false);
    let r = w.run();
    assert_eq!(r.released.len(), 1);
    assert_eq!(r.released[0].restored, None);
    assert_eq!(w.park.writes, vec!["remove #10 loom:blocked"]);
    assert!(w.extra.posted[0].1.contains("was not restored"));
}

/// Regression coverage for #10837 (loom-ui#1695): a starred, triage-only issue
/// carrying three identical-reason park records whose same-repo blockers are
/// all CLOSED is released, with no `loom:issue` invented and the star kept.
#[test]
fn starred_triage_only_issue_with_three_identical_reason_parks_is_released() {
    let mut w = World::new();
    let reason = Some("needs slice 1a, finished-item mode and compact layout");
    let records: Vec<String> = [1689u64, 1692, 1693]
        .iter()
        .map(|b| render_park(&[*b], Some("guide"), Some("2026-10-05T21:24:30Z"), reason))
        .collect();
    let body = format!("Work.\n\n{}\n", records.join("\n"));
    w.with_body(1695, false, &body, &["loom:triage", "loom:operator-priority"]);
    for b in [1689, 1692, 1693] {
        w.state(b, "CLOSED", false);
    }
    w.extra
        .events
        .insert(1695, vec!["loom:triage".into(), "loom:blocked".into()]);
    let r = w.run();
    assert_eq!(r.released.len(), 1, "{}", r.summary());
    assert_eq!(r.released[0].restored, None);
    assert!(r.released[0].applied && r.released[0].commented);
    assert_eq!(w.park.writes, vec!["remove #1695 loom:blocked"]);
    let l = w.labels(1695);
    assert!(!l.contains("loom:blocked") && !l.contains("loom:issue"));
    assert!(l.contains("loom:triage") && l.contains("loom:operator-priority"));
}

#[test]
fn merged_pr_blocker_counts_as_resolved() {
    let mut w = World::new();
    w.parked(10, false, &[5], &[]);
    w.state(5, "MERGED", true);
    let r = w.run();
    assert_eq!(r.released.len(), 1, "{}", r.summary());
}

#[test]
fn closed_unmerged_pr_blocker_is_skipped() {
    let mut w = World::new();
    w.parked(10, false, &[5], &[]);
    w.state(5, "CLOSED", true);
    let r = w.run();
    assert_eq!(skipped(&r, "closed-unmerged-pr"), 1);
    assert!(r.released.is_empty() && w.no_writes());
}

#[test]
fn parked_pr_restores_its_latest_review_label() {
    let mut w = World::new();
    w.parked(20, true, &[1], &[]);
    w.parked(21, true, &[1], &[]);
    w.state(1, "CLOSED", false);
    w.extra.events.insert(
        20,
        vec![
            "loom:review-requested".into(),
            "loom:changes-requested".into(),
        ],
    );
    w.extra.events.insert(
        21,
        vec![
            "loom:changes-requested".into(),
            "loom:review-requested".into(),
        ],
    );
    let r = w.run();
    assert_eq!(r.released.len(), 2, "{}", r.summary());
    assert!(w.labels(20).contains("loom:changes-requested"));
    assert!(w.labels(21).contains("loom:review-requested"));
    assert!(!w.labels(20).contains("loom:blocked") && !w.labels(21).contains("loom:blocked"));
}

#[test]
fn pr_with_empty_history_defaults_to_review_requested() {
    let mut w = World::new();
    w.parked(20, true, &[1], &[]);
    w.state(1, "CLOSED", false);
    let r = w.run();
    assert_eq!(r.released[0].restored.as_deref(), Some("loom:review-requested"));
}

// --- re-park ------------------------------------------------------------------

#[test]
fn mixed_blockers_repark_to_the_open_records_and_keep_the_label() {
    let mut w = World::new();
    w.parked(10, false, &[1, 3], &[]);
    w.state(1, "CLOSED", false);
    w.state(3, "OPEN", false);
    let r = w.run();
    assert_eq!(r.reparked.len(), 1, "{}", r.summary());
    assert!(r.released.is_empty());
    assert_eq!(w.park.writes, vec!["body #10"]);
    assert_eq!(blockers(&w.park.items[&10].body), vec![BlockerRef::local(3)]);
    assert!(w.park.items[&10].body.starts_with("Some work."));
    assert!(w.labels(10).contains("loom:blocked"));
    assert!(w.extra.posted[0].1.contains(&marker(false, &[1])));
    // A re-park needs no closing-PR GraphQL.
    assert!(w.gather.closing_calls.is_empty());
}

#[test]
fn nothing_resolved_is_still_blocked() {
    let mut w = World::new();
    w.parked(10, false, &[3], &[]);
    w.state(3, "OPEN", false);
    let r = w.run();
    assert_eq!(r.still_blocked, 1);
    assert!(w.no_writes());
    assert!(w.gather.closing_calls.is_empty(), "steady state spends no GraphQL");
}

// --- skips --------------------------------------------------------------------

#[test]
fn prose_only_and_unstated_parks_are_skipped() {
    let mut w = World::new();
    w.with_body(10, false, "Blocked by #1", &[]);
    w.with_body(11, false, &render_park(&[], Some("x"), None, Some("operator")), &[]);
    w.state(1, "CLOSED", false);
    let r = w.run();
    assert_eq!(skipped(&r, "no-park-record"), 1);
    assert_eq!(skipped(&r, "unstated"), 1);
    assert!(w.no_writes() && w.gather.state_calls.is_empty());
}

#[test]
fn qualified_cross_repo_blocker_is_skipped() {
    let mut w = World::new();
    w.with_body(10, false, "<!-- loom:park Blocked by: other/repo#1 by=x -->", &[]);
    w.state(1, "CLOSED", false);
    let r = w.run();
    assert_eq!(skipped(&r, "cross-repo"), 1);
    assert!(w.no_writes() && w.gather.state_calls.is_empty());
}

/// #10443 x #10556: `blockers()` now returns qualified refs as their own
/// repo's artifacts. A local-only check must not let one vanish — a closed
/// local blocker beside an open cross-repo one is still a park, not a release.
#[test]
fn closed_local_blocker_beside_a_qualified_one_is_skipped_not_released() {
    for body in [
        // Distinct numbers, both orders.
        "<!-- loom:park Blocked by: #1 by=x -->\n<!-- loom:park Blocked by: other/repo#2 by=x -->",
        "<!-- loom:park Blocked by: other/repo#2, #1 by=x -->",
        // Same number: the qualified `#1` is another repo's, never local #1.
        "<!-- loom:park Blocked by: #1, other/repo#1 by=x -->",
    ] {
        let mut w = World::new();
        w.with_body(10, false, body, &[]);
        w.state(1, "CLOSED", false);
        w.state(2, "CLOSED", false);
        let r = w.run();
        assert_eq!(skipped(&r, "cross-repo"), 1, "{body}: {}", r.summary());
        assert!(r.released.is_empty() && r.reparked.is_empty(), "{body}");
        assert!(w.no_writes() && w.gather.state_calls.is_empty(), "{body}");
    }
}

#[test]
fn permanent_block_in_body_or_any_comment_is_skipped() {
    let mut w = World::new();
    let body = format!(
        "{}\n<!-- loom:permanent-block ruled by operator -->\n",
        render_park(&[1], None, None, None)
    );
    w.with_body(10, false, &body, &[]);
    w.parked(11, false, &[1], &[]);
    w.state(1, "CLOSED", false);
    // From an untrusted author too: it can only prevent a write.
    w.extra
        .comments
        .insert(11, vec![untrusted("<!-- loom:permanent-block -->")]);
    let r = w.run();
    assert_eq!(skipped(&r, "permanent"), 2, "{}", r.summary());
    assert!(w.no_writes());
}

#[test]
fn open_closing_pr_on_an_issue_is_skipped() {
    let mut w = World::new();
    w.parked(10, false, &[1], &[]);
    w.state(1, "CLOSED", false);
    w.gather.closing.insert(
        10,
        vec![ClosingRef {
            number: 50,
            state: "OPEN".to_string(),
        }],
    );
    let r = w.run();
    assert_eq!(skipped(&r, "open-closing-pr"), 1, "{}", r.summary());
    assert!(w.no_writes());
}

#[test]
fn another_open_prose_reference_vetoes_a_release() {
    let mut w = World::new();
    let body = format!("Depends on #7\n\n{}\n", render_park(&[1], None, None, None));
    w.with_body(10, false, &body, &[]);
    w.state(1, "CLOSED", false);
    w.state(7, "OPEN", false);
    let r = w.run();
    assert_eq!(skipped(&r, "other-open-reference"), 1, "{}", r.summary());
    assert!(w.no_writes());
}

/// #9274: a resolved park blocker beside an unticked `## Dependencies` box
/// whose ref closed is NOT released: the box is unmet until a human ticks it.
#[test]
fn unticked_checklist_box_vetoes_a_release() {
    let mut w = World::new();
    let body = format!(
        "## Dependencies\n\n- [ ] #8: hardware bring-up signed off\n\n{}\n",
        render_park(&[1], Some("builder"), Some("2026-10-01T00:00:00Z"), None)
    );
    w.with_body(10, false, &body, &[]);
    w.state(1, "CLOSED", false);
    w.state(8, "CLOSED", false);
    let r = w.run();
    assert_eq!(skipped(&r, "unticked-checklist"), 1, "{}", r.summary());
    assert!(w.no_writes());
}

/// The same veto for an unchecked line no parser reads.
#[test]
fn unparseable_unticked_checklist_line_vetoes_a_release() {
    let mut w = World::new();
    let body = format!(
        "## Dependencies\n\n- [ ] vendor confirms the pinout\n\n{}\n",
        render_park(&[1], Some("builder"), Some("2026-10-01T00:00:00Z"), None)
    );
    w.with_body(10, false, &body, &[]);
    w.state(1, "CLOSED", false);
    let r = w.run();
    assert_eq!(skipped(&r, "unticked-checklist"), 1, "{}", r.summary());
    assert!(w.no_writes());
}

#[test]
fn superseded_pr_is_skipped() {
    let mut w = World::new();
    w.parked(20, true, &[1], &[]);
    w.state(1, "CLOSED", false);
    w.gather
        .merge
        .insert(20, ("CONFLICTING".to_string(), "DIRTY".to_string()));
    let r = w.run();
    assert_eq!(skipped(&r, "superseded"), 1, "{}", r.summary());
    assert!(w.no_writes());
}

#[test]
fn operator_labels_are_holds_but_the_star_is_not() {
    let mut w = World::new();
    w.parked(10, false, &[1], &["loom:operator-only", "loom:operator-decision"]);
    w.parked(11, false, &[1], &["loom:operator"]);
    w.parked(12, false, &[1], &["loom:operator-priority"]);
    w.state(1, "CLOSED", false);
    let r = w.run();
    assert_eq!(skipped(&r, "operator-hold"), 2);
    assert_eq!(r.released.len(), 1);
    assert_eq!(r.released[0].number, 12);
}

#[test]
fn trusted_prless_hold_comment_is_skipped() {
    let mut w = World::new();
    w.parked(10, false, &[1], &[]);
    w.state(1, "CLOSED", false);
    w.extra
        .comments
        .insert(10, vec![trusted(&format!("{PRLESS_HOLD_COMMENT_MARKER}\nHeld."))]);
    let r = w.run();
    assert_eq!(skipped(&r, "daemon-hold"), 1);
    assert!(w.no_writes());
}

// --- fail safe ------------------------------------------------------------------

#[test]
fn failed_evidence_read_is_unevaluated_with_zero_writes() {
    let mut w = World::new();
    // One comment on the listing row, and no fixture for it: phase 2 fails.
    let body = render_park(&[1], None, None, None);
    w.gather
        .rows
        .push(row(10, &body, 1, false, &["loom:blocked"]));
    w.park.items.insert(
        10,
        IssueState {
            body,
            labels: vec!["loom:blocked".into()],
        },
    );
    w.state(1, "CLOSED", false);
    let r = w.run();
    assert_eq!(r.unevaluated.len(), 1, "{}", r.summary());
    assert!(r.released.is_empty() && w.no_writes());
}

#[test]
fn missing_blocker_state_and_failed_comment_read_write_nothing() {
    let mut w = World::new();
    w.parked(10, false, &[404], &[]);
    w.parked(11, false, &[1], &[]);
    w.state(1, "CLOSED", false);
    w.extra.comments_fail = true;
    let r = w.run();
    assert_eq!(r.unevaluated.len(), 2, "{}", r.summary());
    assert!(w.no_writes());
}

#[test]
fn budget_refusal_reads_nothing_and_writes_nothing() {
    let mut w = World::new();
    w.parked(10, false, &[1], &[]);
    w.state(1, "CLOSED", false);
    w.gather.budget = Some(Budget {
        core_remaining: 10,
        graphql_remaining: 10,
    });
    let r = w.run();
    assert_eq!(r.unevaluated.len(), 1);
    assert!(r.cost.budget_refused.is_some());
    assert!(w.gather.state_calls.is_empty() && w.no_writes() && w.park.views == 0);
}

#[test]
fn breaker_and_archived_repo_stop_before_the_listing() {
    let mut w = World::new();
    w.parked(10, false, &[1], &[]);
    w.state(1, "CLOSED", false);
    w.gather.list_fails = true;
    w.gather.breaker = true;
    let r = w.run();
    assert!(r.enumerate_error.unwrap().contains("breaker"));
    w.extra.archived = true;
    let r = w.run();
    assert!(r.archived && r.enumerate_error.is_none());
    assert!(w.no_writes() && w.gather.state_calls.is_empty());
}

#[test]
fn concurrent_edit_between_plan_and_write_aborts() {
    let mut w = World::new();
    w.parked(10, false, &[1], &[]);
    w.state(1, "CLOSED", false);
    // Someone added a second blocker after the listing was read.
    w.park.items.get_mut(&10).unwrap().body = render_park(&[1, 9], None, None, None);
    let r = w.run();
    assert_eq!(skipped(&r, "concurrent-edit"), 1);
    assert!(w.no_writes());
}

/// The body the evidence was read from, then `extra` appended before the
/// final view. Only park record #1 (closed) is declared; #9 is open.
fn edited_after_evidence(extra: &str) -> (World, Report) {
    let mut w = World::new();
    w.parked(10, false, &[1], &[]);
    w.state(1, "CLOSED", false);
    w.state(9, "OPEN", false);
    w.park.items.get_mut(&10).unwrap().body.push_str(extra);
    let r = w.run();
    (w, r)
}

#[test]
fn an_open_prose_dependency_added_after_the_evidence_aborts() {
    let (w, r) = edited_after_evidence("\nDepends on #9\n");
    assert_eq!(skipped(&r, "concurrent-edit"), 1, "{}", r.summary());
    assert!(r.released.is_empty() && r.reparked.is_empty() && w.no_writes());
}

#[test]
fn an_unchecked_dependencies_box_added_after_the_evidence_aborts() {
    let (w, r) = edited_after_evidence("\n## Dependencies\n\n- [ ] #9\n");
    assert_eq!(skipped(&r, "concurrent-edit"), 1, "{}", r.summary());
    assert!(r.released.is_empty() && r.reparked.is_empty() && w.no_writes());
}

#[test]
fn a_reparked_body_edited_after_the_plan_aborts() {
    let mut w = World::new();
    w.parked(10, false, &[1, 3], &[]);
    w.state(1, "CLOSED", false);
    w.state(3, "OPEN", false);
    w.park
        .items
        .get_mut(&10)
        .unwrap()
        .body
        .push_str("\nNew context.\n");
    let r = w.run();
    assert_eq!(skipped(&r, "concurrent-edit"), 1, "{}", r.summary());
    assert!(r.reparked.is_empty() && w.no_writes());
}

#[test]
fn line_endings_and_trailing_whitespace_are_not_an_edit() {
    let mut w = World::new();
    w.parked(10, false, &[1], &[]);
    w.state(1, "CLOSED", false);
    let item = w.park.items.get_mut(&10).unwrap();
    item.body = format!("{}  \r\n\r\n", item.body.trim_end().replace('\n', "\r\n"));
    let r = w.run();
    assert_eq!(r.released.len(), 1, "{}", r.summary());
}

#[test]
fn a_qualified_blocker_added_between_plan_and_write_aborts() {
    let mut w = World::new();
    w.parked(10, false, &[1], &[]);
    w.state(1, "CLOSED", false);
    // The fresh body keeps local #1 and gains a cross-repo blocker: the
    // local-only view is unchanged, so only the qualified ref can veto.
    w.park.items.get_mut(&10).unwrap().body = format!(
        "{}\n<!-- loom:park Blocked by: other/repo#1 by=x -->\n",
        render_park(&[1], Some("builder"), Some("2026-10-01T00:00:00Z"), None)
    );
    let r = w.run();
    assert_eq!(skipped(&r, "concurrent-edit"), 1, "{}", r.summary());
    assert!(r.released.is_empty() && w.no_writes());
}

// --- idempotence, caps, dry run -------------------------------------------------

#[test]
fn rerun_after_a_failed_label_edit_does_not_repost_the_comment() {
    let mut w = World::new();
    w.parked(10, false, &[1], &[]);
    w.state(1, "CLOSED", false);
    w.park.remove_failures = 1;
    let first = w.run();
    assert_eq!(first.failed.len(), 1);
    assert_eq!(w.extra.posted.len(), 1);
    assert!(w.labels(10).contains("loom:blocked"));

    let second = w.run();
    assert_eq!(second.released.len(), 1, "{}", second.summary());
    assert!(!second.released[0].commented);
    assert_eq!(w.extra.posted.len(), 1, "no duplicate audit comment");
    assert!(!w.labels(10).contains("loom:blocked"));

    // A third pass over a stale listing finds the label gone: no writes.
    let before = w.park.writes.len();
    let third = w.run();
    assert_eq!(skipped(&third, "concurrent-edit"), 1);
    assert_eq!(w.park.writes.len(), before);
    assert_eq!(w.extra.posted.len(), 1);
}

#[test]
fn an_untrusted_marker_does_not_suppress_the_audit_comment() {
    let mut w = World::new();
    w.parked(10, false, &[1], &[]);
    w.state(1, "CLOSED", false);
    w.extra
        .comments
        .insert(10, vec![untrusted(&marker(true, &[1]))]);
    let r = w.run();
    assert!(r.released[0].commented);
    assert_eq!(w.extra.posted.len(), 1);
}

#[test]
fn write_cap_defers_the_rest_to_the_next_pass() {
    let mut w = World::new();
    w.parked(10, false, &[1], &[]);
    w.parked(11, false, &[1], &[]);
    w.state(1, "CLOSED", false);
    let r = w.run_with(Config {
        max_writes: 1,
        ..cfg()
    });
    assert_eq!(r.released.len(), 1);
    assert_eq!(skipped(&r, "write-cap"), 1);
    assert_eq!(w.extra.posted.len(), 1);
}

#[test]
fn dry_run_plans_with_zero_writes() {
    let mut w = World::new();
    w.parked(10, false, &[1], &[]);
    w.parked(11, false, &[1, 3], &[]);
    w.state(1, "CLOSED", false);
    w.state(3, "OPEN", false);
    let r = w.run_with(Config {
        dry_run: true,
        ..cfg()
    });
    assert_eq!((r.released.len(), r.reparked.len()), (1, 1));
    assert!(!r.released[0].applied && !r.reparked[0].applied);
    assert!(w.no_writes());
    let json = serde_json::to_value(&r).unwrap();
    assert_eq!(json["released"].as_array().unwrap().len(), 1);
    assert!(r.summary().contains("dry-run"));
}

// --- incident shapes (#10837) ---------------------------------------------------

/// The three Guide records loom-ui#1695 carried, verbatim: identical `reason=`.
const LOOM_UI_1695_RECORDS: &str = "\
<!-- loom:park Blocked by: #1689 by=guide at=2026-10-05T21:24:30Z reason=\"needs slice 1a, finished-item mode and compact layout\" -->
<!-- loom:park Blocked by: #1692 by=guide at=2026-10-05T21:24:30Z reason=\"needs slice 1a, finished-item mode and compact layout\" -->
<!-- loom:park Blocked by: #1693 by=guide at=2026-10-05T21:24:30Z reason=\"needs slice 1a, finished-item mode and compact layout\" -->";

#[test]
fn loom_ui_1695_shape_releases_without_inventing_approval() {
    // Starred, triage-only lane (never carried `loom:issue`), every blocker closed.
    let mut w = World::new();
    let body = format!("Card layout work.\n\n{LOOM_UI_1695_RECORDS}\n");
    w.with_body(1695, false, &body, &["loom:triage", "loom:operator-priority"]);
    for b in [1689, 1692, 1693] {
        w.state(b, "CLOSED", false);
    }
    let lane = ["loom:triage", "loom:blocked", "loom:operator-priority"];
    w.extra.events.insert(1695, lane.map(String::from).to_vec());
    let r = w.run();
    assert_eq!(r.released.len(), 1, "{}", r.summary());
    assert_eq!(r.released[0].resolved, vec![1689, 1692, 1693]);
    assert_eq!(r.released[0].restored, None);
    assert_eq!(w.park.writes, vec!["remove #1695 loom:blocked"]);
    let labels = w.labels(1695);
    assert!(labels.contains("loom:triage") && !labels.contains("loom:issue"));
}

#[test]
fn records_quoted_in_a_code_fence_never_release_an_unrecorded_hold() {
    // #10837 itself: its body quotes #1695's records as evidence, and an
    // operator later parked it with no record of its own. Before the fix the
    // pass read the quotes as declarations and released the hold.
    let mut w = World::new();
    let body = format!("Reproduction:\n\n```\n{LOOM_UI_1695_RECORDS}\n```\n\nMore prose.\n");
    w.with_body(10837, false, &body, &["loom:curated", "loom:operator-priority"]);
    for b in [1689, 1692, 1693] {
        w.state(b, "CLOSED", false);
    }
    w.extra.events.insert(10837, vec!["loom:issue".into()]);
    let r = w.run();
    assert_eq!(skipped(&r, "no-park-record"), 1, "{}", r.summary());
    assert!(r.released.is_empty() && r.reparked.is_empty());
    assert!(w.no_writes());
    assert!(w.labels(10837).contains("loom:blocked"));
}

#[test]
fn quoted_or_indented_records_never_release_but_a_real_one_after_indented_backticks_does() {
    // #10837 review: indented / blockquoted markers are not records, so they
    // cannot release an unrecorded hold ...
    for body in [
        format!("Evidence:\n\n    {}\n", LOOM_UI_1695_RECORDS.replace('\n', "\n    ")),
        format!("> {}\n", LOOM_UI_1695_RECORDS.replace('\n', "\n> ")),
    ] {
        let mut w = World::new();
        w.with_body(10837, false, &body, &["loom:curated", "loom:operator-priority"]);
        for b in [1689, 1692, 1693] {
            w.state(b, "CLOSED", false);
        }
        w.extra.events.insert(10837, vec!["loom:issue".into()]);
        let r = w.run();
        assert_eq!(skipped(&r, "no-park-record"), 1, "{body}: {}", r.summary());
        assert!(r.released.is_empty() && w.no_writes(), "{body}");
    }
    // ... while a real record after a 4-space-indented ``` still releases.
    let mut w = World::new();
    let body = "Evidence:\n\n    ```\n\n<!-- loom:park Blocked by: #7 -->\n";
    w.with_body(10837, false, body, &["loom:curated"]);
    w.state(7, "CLOSED", false);
    w.extra.events.insert(10837, vec!["loom:issue".into()]);
    let r = w.run();
    assert_eq!(r.released.len(), 1, "{}", r.summary());
    assert_eq!(r.released[0].resolved, vec![7]);
}

// --- the tick's gate ------------------------------------------------------------

#[test]
fn mode_parses_default_on() {
    assert_eq!(Mode::parse(None), Mode::On);
    assert_eq!(Mode::parse(Some("")), Mode::On);
    assert_eq!(Mode::parse(Some("0")), Mode::Off);
    assert_eq!(Mode::parse(Some("off")), Mode::Off);
    assert_eq!(Mode::parse(Some(" FALSE ")), Mode::Off);
    assert_eq!(Mode::parse(Some("no")), Mode::Off);
    assert_eq!(Mode::parse(Some("1")), Mode::On);
    assert_eq!(Mode::parse(Some(" ON ")), Mode::On);
    assert_eq!(Mode::parse(Some("dry-run")), Mode::DryRun);
}

#[test]
fn env_off_and_unowned_shard_read_nothing() {
    let never = |_: bool| -> Report { panic!("the pass must not run") };
    assert!(gated(Mode::Off, true, never).is_none());
    assert!(gated(Mode::On, false, never).is_none());
    assert!(gated(Mode::DryRun, false, never).is_none());

    let mut w = World::new();
    w.parked(10, false, &[1], &[]);
    w.state(1, "CLOSED", false);
    let r = gated(Mode::DryRun, true, |dry_run| w.run_with(Config { dry_run, ..cfg() })).unwrap();
    assert!(r.dry_run && w.no_writes());
    assert_eq!(r.released.len(), 1);
}
