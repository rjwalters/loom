//! Tests for `park-record apply` (#10152): the refusals, the body-before-label
//! ordering, idempotence, that a park written this way is visible to
//! `check-stale-blocked`, and that no role prompt still applies `loom:blocked`
//! with a bare label edit.

use super::*;
use crate::dep_recheck::{extract, premise};
use crate::stale_blocked::{self, Evidence, Verdict};
use std::collections::HashMap;

const AT: &str = "2026-10-04T03:00:00Z";

#[derive(Default)]
struct Fake {
    state: IssueState,
    blockers: HashMap<u64, &'static str>,
    calls: Vec<String>,
    fail_body: bool,
}

impl Fake {
    fn new(body: &str, labels: &[&str]) -> Self {
        Self {
            state: IssueState {
                body: body.to_string(),
                labels: labels.iter().map(|l| (*l).to_string()).collect(),
            },
            ..Self::default()
        }
    }
    fn blocker(mut self, n: u64, state: &'static str) -> Self {
        self.blockers.insert(n, state);
        self
    }
    fn mutations(&self) -> Vec<&String> {
        self.calls
            .iter()
            .filter(|c| !c.starts_with("view") && !c.starts_with("state"))
            .collect()
    }
}

impl ParkForge for Fake {
    fn view(&mut self, number: u64) -> Result<IssueState, String> {
        self.calls.push(format!("view {number}"));
        Ok(self.state.clone())
    }
    fn state(&mut self, number: u64) -> Result<String, String> {
        self.calls.push(format!("state {number}"));
        self.blockers
            .get(&number)
            .map(|s| (*s).to_string())
            .ok_or_else(|| "404".to_string())
    }
    fn set_body(&mut self, number: u64, body: &str) -> Result<(), String> {
        self.calls.push(format!("set_body {number}"));
        if self.fail_body {
            return Err("boom".into());
        }
        self.state.body = body.to_string();
        Ok(())
    }
    fn add_labels(&mut self, number: u64, labels: &[String]) -> Result<(), String> {
        self.calls
            .push(format!("add_labels {number} {}", labels.join(",")));
        self.state.labels.extend(labels.iter().cloned());
        Ok(())
    }
    fn remove_label(&mut self, number: u64, label: &str) -> Result<(), String> {
        self.calls.push(format!("remove_label {number} {label}"));
        self.state.labels.retain(|l| l != label);
        Ok(())
    }
}

fn req(blocked_by: &[u64], reason: Option<&str>) -> ApplyRequest {
    ApplyRequest {
        number: 10,
        blocked_by: blocked_by.to_vec(),
        reason: reason.map(str::to_string),
        by: Some("builder".to_string()),
        at: AT.to_string(),
        remove_labels: vec!["loom:building".to_string()],
        dry_run: false,
    }
}

fn run(f: &mut Fake, r: &ApplyRequest) -> (i32, String, String) {
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let code = apply(f, r, &mut out, &mut err);
    (code, String::from_utf8(out).unwrap(), String::from_utf8(err).unwrap())
}

#[test]
fn no_blocker_and_no_reason_is_refused_before_any_forge_call() {
    let mut f = Fake::new("body", &["loom:building"]);
    let (code, _, err) = run(&mut f, &req(&[], None));
    assert_eq!(code, exit::REFUSED);
    assert!(f.calls.is_empty(), "{:?}", f.calls);
    assert!(err.contains("--reason"), "{err}");

    let (code, _, _) = run(&mut f, &req(&[], Some("   ")));
    assert_eq!(code, exit::REFUSED, "a blank reason is not a reason");
    assert!(f.calls.is_empty());
}

#[test]
fn a_self_block_is_refused() {
    let mut f = Fake::new("", &[]);
    assert_eq!(run(&mut f, &req(&[10], None)).0, exit::REFUSED);
    assert!(f.calls.is_empty());
}

#[test]
fn body_is_written_before_the_label_and_building_is_dropped_last() {
    let mut f = Fake::new("## Summary\n\nDo it.\n", &["loom:building"]).blocker(11, "open");
    let (code, out, err) = run(&mut f, &req(&[11], Some("needs the API")));
    assert_eq!(code, exit::OK, "{err}");
    assert_eq!(
        f.mutations(),
        vec![
            "set_body 10",
            "add_labels 10 loom:blocked",
            "remove_label 10 loom:building"
        ]
    );
    assert_eq!(park_record_blockers(&f.state.body), vec![11]);
    assert!(f
        .state
        .body
        .starts_with("## Summary\n\nDo it.\n\n<!-- loom:park Blocked by: #11 by=builder"));
    assert!(out.contains("APPLIED=true"));
    assert_eq!(f.state.labels, vec!["loom:blocked".to_string()]);
}

fn park_record_blockers(body: &str) -> Vec<u64> {
    super::super::blockers(body)
}

#[test]
fn a_failed_body_write_applies_no_label() {
    let mut f = Fake::new("b", &["loom:building"]).blocker(11, "open");
    f.fail_body = true;
    let (code, _, err) = run(&mut f, &req(&[11], None));
    assert_eq!(code, exit::FORGE);
    assert_eq!(f.mutations(), vec!["set_body 10"]);
    assert!(err.contains("no label applied"), "{err}");
}

#[test]
fn a_closed_blocker_is_refused_and_nothing_is_written() {
    let mut f = Fake::new("b", &["loom:building"])
        .blocker(11, "open")
        .blocker(12, "closed");
    let (code, _, err) = run(&mut f, &req(&[11, 12], None));
    assert_eq!(code, exit::REFUSED);
    assert!(f.mutations().is_empty(), "{:?}", f.calls);
    assert!(err.contains("#12"), "{err}");
    assert!(!err.contains("#11,"), "{err}");
}

#[test]
fn an_unreadable_blocker_is_a_forge_error_not_a_park() {
    let mut f = Fake::new("b", &[]);
    assert_eq!(run(&mut f, &req(&[99], None)).0, exit::FORGE);
    assert!(f.mutations().is_empty());
}

#[test]
fn reapplying_is_idempotent() {
    let mut f = Fake::new("b", &["loom:building"]).blocker(11, "open");
    assert_eq!(run(&mut f, &req(&[11], None)).0, exit::OK);
    let body = f.state.body.clone();
    f.calls.clear();
    let (code, out, _) = run(&mut f, &req(&[11], None));
    assert_eq!(code, exit::OK);
    assert!(f.mutations().is_empty(), "{:?}", f.calls);
    assert_eq!(f.state.body, body);
    assert!(out.contains("BODY_CHANGED=false"));
}

#[test]
fn only_undeclared_blockers_are_appended() {
    let existing = "b\n\n<!-- loom:park Blocked by: #11 by=curator -->\n";
    let mut f = Fake::new(existing, &[])
        .blocker(11, "open")
        .blocker(12, "open");
    assert_eq!(run(&mut f, &req(&[11, 12], None)).0, exit::OK);
    assert_eq!(f.state.body.matches("Blocked by: #11").count(), 1);
    assert_eq!(park_record_blockers(&f.state.body), vec![11, 12]);
}

#[test]
fn an_explicit_reason_without_a_blocker_writes_an_unstated_record_once() {
    let mut f = Fake::new("", &["loom:building"]);
    assert_eq!(run(&mut f, &req(&[], Some("operator"))).0, exit::OK);
    let records = parse(&f.state.body);
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].blocker, None);
    assert_eq!(records[0].reason.as_deref(), Some("operator"));
    assert!(f.state.labels.contains(&BLOCKED_LABEL.to_string()));

    f.calls.clear();
    assert_eq!(run(&mut f, &req(&[], Some("operator"))).0, exit::OK);
    assert!(f.mutations().is_empty(), "{:?}", f.calls);
}

#[test]
fn dry_run_mutates_nothing() {
    let mut f = Fake::new("b", &["loom:building"]).blocker(11, "open");
    let mut r = req(&[11], None);
    r.dry_run = true;
    let (code, out, _) = run(&mut f, &r);
    assert_eq!(code, exit::OK);
    assert!(f.mutations().is_empty());
    assert!(out.contains("DRY_RUN=true") && out.contains("LABELS_ADD=loom:blocked"));
    assert!(out.contains("<!-- loom:park Blocked by: #11"), "{out}");
}

/// AC: a park applied this way is declared — not PROSE-ONLY, not UNDOCUMENTED —
/// to `check-stale-blocked`, through its own extractor and classifier.
#[test]
fn an_applied_park_is_visible_to_check_stale_blocked() {
    let mut f = Fake::new("## Summary\n\nWork.\n", &["loom:building"]).blocker(11, "open");
    assert_eq!(run(&mut f, &req(&[11], Some("needs the API"))).0, exit::OK);

    let input = extract::Input {
        body: f.state.body.clone(),
        comments: Vec::new(),
    };
    assert_eq!(extract::extract(&input, "loom-bot"), "11");

    let e = Evidence {
        prose: vec![premise::Ref {
            number: 11,
            state: "OPEN".to_string(),
        }],
        declared: park_record_blockers(&input.body),
        ..Evidence::default()
    };
    assert_eq!(e.declared, vec![11]);
    assert!(!stale_blocked::undeclared(&e), "the park is declared, not prose-only");
    assert_eq!(stale_blocked::classify(&e), Verdict::StillBlocked);
}

/// AC: no role prompt applies `loom:blocked` with a bare label edit any more —
/// every park goes through `park-record apply`.
#[test]
fn no_role_prompt_adds_loom_blocked_with_a_bare_label_edit() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
    let bare = regex::Regex::new(r#"add-label[= ]+"?loom:blocked\b"#).unwrap();
    let mut offenders = Vec::new();
    for dir in [
        "defaults/.claude/commands/loom",
        "defaults/.agents/skills",
        "defaults/docs",
        "defaults/roles",
    ] {
        let mut stack = vec![root.join(dir)];
        while let Some(p) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&p) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                } else if path.extension().is_some_and(|x| x == "md") {
                    let text = std::fs::read_to_string(&path).unwrap_or_default();
                    for (i, line) in text.lines().enumerate() {
                        if bare.is_match(line) {
                            offenders.push(format!("{}:{}: {line}", path.display(), i + 1));
                        }
                    }
                }
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "apply loom:blocked with `loom-daemon park-record apply`, not a bare label edit \
         (#10152):\n{}",
        offenders.join("\n")
    );
}
