//! Recording-fake tests: body record before label, and no label-only park.
#![allow(clippy::unwrap_used)]

use super::*;
use crate::operator_decision::cli::IssueState;

#[derive(Default)]
struct Fake {
    calls: Vec<String>,
    labels: Vec<String>,
    body: String,
    fail_body: bool,
    fail_label: bool,
    fail_remove: bool,
}

impl ParkForge for Fake {
    fn view(&mut self, _n: u64) -> Result<IssueState, String> {
        self.calls.push("view".into());
        Ok(IssueState {
            body: self.body.clone(),
            labels: self.labels.clone(),
        })
    }
    fn state(&mut self, _n: u64) -> Result<String, String> {
        Ok("open".into())
    }
    fn set_body(&mut self, _n: u64, body: &str) -> Result<(), String> {
        self.calls.push("set_body".into());
        if self.fail_body {
            return Err("422".into());
        }
        self.body = body.to_string();
        Ok(())
    }
    fn add_labels(&mut self, _n: u64, l: &[String]) -> Result<(), String> {
        self.calls.push(format!("add:{}", l.join(",")));
        if self.fail_label {
            return Err("403".into());
        }
        Ok(())
    }
    fn remove_label(&mut self, _n: u64, l: &str) -> Result<(), String> {
        self.calls.push(format!("remove:{l}"));
        if self.fail_remove {
            Err("403".into())
        } else {
            Ok(())
        }
    }
}

fn with_issue() -> Fake {
    Fake {
        labels: vec!["loom:issue".into()],
        ..Fake::default()
    }
}

fn no_label_calls(f: &Fake) -> bool {
    !f.calls
        .iter()
        .any(|c| c.starts_with("add:") || c.starts_with("remove:"))
}

#[test]
fn quarantine_writes_body_record_before_label() {
    let mut f = with_issue();
    quarantine_park_via(&mut f, 7).unwrap();
    assert_eq!(f.calls, ["view", "set_body", "add:loom:blocked", "remove:loom:issue"]);
    assert!(f.body.contains("insta-crash quarantine"), "{}", f.body);
    assert!(f.body.contains("by=daemon"), "{}", f.body);
}

#[test]
fn quarantine_failed_body_write_applies_no_label() {
    let mut f = with_issue();
    f.fail_body = true;
    assert!(quarantine_park_via(&mut f, 7).is_err());
    assert!(no_label_calls(&f), "{:?}", f.calls);
}

#[test]
fn hold_writes_body_record_before_label() {
    let mut f = with_issue();
    prless_hold_park_via(&mut f, 8, true).unwrap();
    assert_eq!(f.calls, ["view", "set_body", "add:loom:blocked", "remove:loom:issue"]);
    assert!(f.body.contains("pr-less hold"), "{}", f.body);
    assert!(f.body.contains("by=daemon"), "{}", f.body);
}

#[test]
fn hold_add_only_fallback_skips_removal_and_does_not_rewrite_body() {
    let mut f = with_issue();
    f.fail_label = true;
    assert!(prless_hold_park_via(&mut f, 8, true).is_err());
    f.fail_label = false;
    f.calls.clear();
    prless_hold_park_via(&mut f, 8, false).unwrap();
    assert_eq!(f.calls, ["view", "add:loom:blocked"], "record already landed");
}

#[test]
fn hold_failed_body_write_applies_no_label() {
    let mut f = with_issue();
    f.fail_body = true;
    assert!(prless_hold_park_via(&mut f, 8, true).is_err());
    assert!(no_label_calls(&f), "{:?}", f.calls);
}

#[test]
fn hold_remove_failure_after_label_is_reported_but_label_is_applied() {
    let mut f = with_issue();
    f.fail_remove = true;
    assert!(prless_hold_park_via(&mut f, 8, true).is_err());
    assert!(f.calls.contains(&"add:loom:blocked".to_string()));
}
