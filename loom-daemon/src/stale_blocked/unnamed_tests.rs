//! Fake-forge tests for the `loom:blocked-unnamed` queue pass (#10558).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::HashMap;

use serde_json::{json, Value};

use super::batch_tests::{fleet, row, Fake};
use super::budget::Floor;
use super::release::{Config, ReleaseForge};
use super::unnamed::{run, Report, UNNAMED_LABEL};
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
    fn post_comment(&mut self, _: u64, _: bool, _: &str) -> Result<(), String> {
        panic!("the queue pass never comments")
    }
}

fn trusted(body: &str) -> Value {
    json!({"body": body, "user": {"login": "someone"}, "author_association": "MEMBER"})
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
