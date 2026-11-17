use super::*;
use crate::merge_pr::reconcile::LeaseState;
use std::cell::RefCell;

struct Fake {
    live: String,
    labels: String,
    lease: LeaseState,
    runnable: bool,
    stack_ok: bool,
    calls: RefCell<Vec<String>>,
}

impl Fake {
    fn new() -> Self {
        Fake {
            live: "[]".into(),
            labels: String::new(),
            lease: LeaseState::NoneFresh,
            runnable: true,
            stack_ok: true,
            calls: RefCell::default(),
        }
    }
    fn calls(&self) -> Vec<String> {
        self.calls.borrow().clone()
    }
}

impl Host for Fake {
    fn list_children(&self, _: &str, _: &str) -> String {
        self.calls.borrow_mut().push("list".into());
        self.live.clone()
    }
    fn issue_labels(&self, _: &str, _: &str) -> String {
        self.labels.clone()
    }
    fn lease(&self, _: &str) -> LeaseState {
        self.lease
    }
    fn comment(&self, _: &str, pr: &str, _: &str) -> bool {
        self.calls.borrow_mut().push(format!("comment {pr}"));
        true
    }
    fn script_runnable(&self, _: &str) -> bool {
        self.runnable
    }
    fn reconcile_stack(&self, _: &str, pr: &str, p: &str) -> bool {
        self.calls.borrow_mut().push(format!("stack {pr} {p}"));
        self.stack_ok
    }
    fn timestamp(&self) -> String {
        "T".into()
    }
}

fn inputs(parent: &str, snap: Option<&str>) -> Inputs {
    Inputs {
        repo: "o/r".into(),
        parent_branch: parent.into(),
        snapshot: snap.map(Into::into),
        script_dir: "d".into(),
        roll_hint: String::new(),
    }
}

const ONE: &str = r#"[{"number":7,"headRefName":"feature/issue-9"}]"#;

#[test]
fn non_stacked_parent_does_nothing_and_never_queries() {
    let h = Fake::new();
    assert!(run(&inputs("fix/x", None), &h).is_empty());
    assert!(h.calls().is_empty());
}

#[test]
fn stacked_parent_without_snapshot_queries_once() {
    let mut h = Fake::new();
    h.live = ONE.into();
    let out = run(&inputs("feature/issue-1", None), &h);
    assert_eq!(h.calls(), vec!["list", "stack 7 feature/issue-1"]);
    assert!(out.iter().any(|(l, _)| l == "SUCCESS"));
}

#[test]
fn snapshot_is_preferred_over_live_query() {
    let h = Fake::new();
    run(&inputs("feature/issue-1", Some(ONE)), &h);
    assert_eq!(h.calls(), vec!["stack 7 feature/issue-1"]);
}

#[test]
fn building_child_with_held_lease_defers_with_comment() {
    let mut h = Fake::new();
    h.labels = "loom:building".into();
    h.lease = LeaseState::Held;
    run(&inputs("feature/issue-1", Some(ONE)), &h);
    assert_eq!(h.calls(), vec!["comment 7"]);
}

#[test]
fn building_child_with_unknown_lease_still_defers() {
    let mut h = Fake::new();
    h.labels = "loom:building".into();
    h.lease = LeaseState::Unknown;
    run(&inputs("feature/issue-1", Some(ONE)), &h);
    assert_eq!(h.calls(), vec!["comment 7"]);
}

#[test]
fn missing_script_skips_everything() {
    let mut h = Fake::new();
    h.runnable = false;
    let out = run(&inputs("feature/issue-1", Some(ONE)), &h);
    assert!(h.calls().is_empty());
    assert!(out
        .iter()
        .any(|(l, t)| l == "WARNING" && t.contains("not found or not executable")));
}

#[test]
fn failed_reconcile_warns_with_manual_command() {
    let mut h = Fake::new();
    h.stack_ok = false;
    let out = run(&inputs("feature/issue-1", Some(ONE)), &h);
    assert!(out.iter().any(|(l, t)| l == "WARNING"
        && t.contains("./.loom/scripts/reconcile-stack.sh 7 feature/issue-1")));
}

#[test]
fn unreadable_snapshot_skips_with_warning() {
    let h = Fake::new();
    let out = run(&inputs("feature/issue-1", Some("not json")), &h);
    assert!(h.calls().is_empty());
    assert_eq!(out.len(), 1);
}
