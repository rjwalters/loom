//! Slice-2 tests (#9789): every coordination case against an in-memory
//! Gitea fake whose behaviour knobs reproduce the live observations and the
//! failure modes the matrix must distinguish — unknown vs unsupported vs
//! fail, partial pagination, injected vs service faults, scoped cleanup and
//! secret-free receipts. Offline: no network, no subprocess.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::cell::{Cell, RefCell};
use std::collections::HashMap;

use serde_json::{json, Value};

use super::cleanup::{cleanup, FaultInjector, INJECTED_PREFIX};
use super::coordination::Case;
use super::*;

const REPO: &str = "repos/qual-org/loomp-test/";
const NS_PREFIX: &str = "loomp-testrun: ";
const TOKEN: &str = "FAKE-WRITER-TOKEN-coordination";

#[derive(Default)]
struct Issue {
    number: u64,
    title: String,
    body: String,
    state: String,
    reason: Option<String>,
    labels: Vec<u64>,
    events: Vec<(u64, &'static str)>,
}

#[derive(Default)]
struct State {
    issues: Vec<Issue>,
    comments: Vec<(u64, u64, String)>,          // (id, issue, body)
    labels: Vec<(u64, String, String, String)>, // (id, name, color, description)
    next_id: u64,
}

/// An in-memory Gitea. Default knobs model a conforming server; each knob
/// reproduces one live observation or failure mode.
#[derive(Default)]
struct Mini {
    st: RefCell<State>,
    log: RefCell<Vec<(String, String)>>,
    /// `state_reason`: None = field absent (live gitea-1), Some(true) = honoured, Some(false) = null.
    reason: Option<bool>,
    comments_ignore_page: bool,
    sub_issues_status: u16,
    timeline_404: bool,
    timeline_drop_close: bool,
    rewrite_body: bool,
    search_lag: Cell<usize>,
    drop_label_add: bool,
    reverse_comments: bool,
    hide_from_list: Cell<Option<u64>>,
    wrong_title_on_read: bool,
    /// Fail the nth (1-based) GET whose path contains the substring.
    fail_get: Option<(&'static str, usize)>,
    fail_get_seen: Cell<usize>,
    /// Answer this status for METHOD + path substring.
    status: Option<(&'static str, &'static str, u16)>,
    /// Embed the run token in every response body (a reflecting server).
    echo_token: bool,
    login: Option<&'static str>,
}

fn conforming() -> Mini {
    Mini {
        reason: Some(true),
        sub_issues_status: 404,
        login: Some("probe-writer"),
        ..Default::default()
    }
}

impl Mini {
    fn seed(&self, title: &str, n: usize) {
        for _ in 0..n {
            self.create_issue(title, "seed");
        }
    }
    fn id(&self) -> u64 {
        let mut st = self.st.borrow_mut();
        st.next_id += 1;
        st.next_id
    }
    fn create_issue(&self, title: &str, body: &str) -> u64 {
        let mut st = self.st.borrow_mut();
        let number = st.issues.len() as u64 + 1;
        st.issues.push(Issue {
            number,
            title: title.into(),
            body: body.into(),
            state: "open".into(),
            ..Default::default()
        });
        number
    }
    fn label_json(&self, id: u64) -> Value {
        let st = self.st.borrow();
        let (_, name, color, desc) = st.labels.iter().find(|l| l.0 == id).unwrap();
        json!({"id": id, "name": name, "color": color, "description": desc})
    }
    fn issue_json(&self, n: u64) -> Value {
        let st = self.st.borrow();
        let i = &st.issues[(n - 1) as usize];
        let labels: Vec<Value> = i
            .labels
            .iter()
            .filter_map(|id| st.labels.iter().find(|l| l.0 == *id))
            .map(|(id, name, color, desc)| json!({"id": id, "name": name, "color": color, "description": desc}))
            .collect();
        let title = if self.wrong_title_on_read {
            "someone else's issue"
        } else {
            i.title.as_str()
        };
        let mut v = json!({"id": 100_000 + n, "number": n, "title": title, "body": i.body, "state": i.state, "labels": labels});
        match self.reason {
            Some(true) => v["state_reason"] = json!(i.reason),
            Some(false) => v["state_reason"] = Value::Null,
            None => {}
        }
        v
    }
    fn page(&self, items: Vec<Value>, q: &HashMap<String, String>, ignore: bool) -> String {
        if ignore {
            return Value::Array(items).to_string();
        }
        let page: usize = q.get("page").and_then(|p| p.parse().ok()).unwrap_or(1);
        let limit: usize = q
            .get("limit")
            .and_then(|p| p.parse().ok())
            .unwrap_or(50)
            .min(50);
        let slice: Vec<Value> = items
            .into_iter()
            .skip((page - 1) * limit)
            .take(limit)
            .collect();
        if slice.is_empty() {
            "null".into() // gitea-1's out-of-range answer
        } else {
            Value::Array(slice).to_string()
        }
    }
    fn answer(
        &self,
        method: &str,
        path: &str,
        body: Option<&str>,
    ) -> Result<(u16, String), String> {
        let (p, query) = path.split_once('?').unwrap_or((path, ""));
        let q: HashMap<String, String> = query
            .split('&')
            .filter_map(|kv| kv.split_once('='))
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        let req: Value = body
            .and_then(|b| serde_json::from_str(b).ok())
            .unwrap_or(Value::Null);
        if p == "version" {
            return Ok((200, r#"{"version":"28.0.0"}"#.into()));
        }
        if p == "user" {
            return Ok(match self.login {
                Some(l) => (200, json!({ "login": l }).to_string()),
                None => (401, "{}".into()),
            });
        }
        if p == "repos/issues/search" {
            if self.search_lag.get() > 0 {
                self.search_lag.set(self.search_lag.get() - 1);
                return Ok((200, "[]".into()));
            }
            let needle = q.get("q").cloned().unwrap_or_default();
            let hits: Vec<u64> = self
                .st
                .borrow()
                .issues
                .iter()
                .filter(|i| i.body.contains(&needle) || i.title.contains(&needle))
                .map(|i| i.number)
                .collect();
            return Ok((
                200,
                Value::Array(hits.into_iter().map(|n| self.issue_json(n)).collect()).to_string(),
            ));
        }
        let rest = p
            .strip_prefix(REPO)
            .ok_or_else(|| format!("fake: unrouted {path}"))?;
        let seg: Vec<&str> = rest.split('/').collect();
        let num = |s: &str| s.parse::<u64>().ok();
        match (method, seg.as_slice()) {
            ("POST", ["issues"]) => {
                let n = self.create_issue(
                    req["title"].as_str().unwrap_or(""),
                    req["body"].as_str().unwrap_or(""),
                );
                Ok((201, json!({"number": n, "title": req["title"]}).to_string()))
            }
            ("GET", ["issues"]) => {
                let open_only = q.get("state").map(String::as_str) == Some("open");
                let hidden = self.hide_from_list.get();
                let ns: Vec<u64> = self
                    .st
                    .borrow()
                    .issues
                    .iter()
                    .rev()
                    .filter(|i| !open_only || i.state == "open")
                    .filter(|i| Some(i.number) != hidden)
                    .map(|i| i.number)
                    .collect();
                Ok((
                    200,
                    self.page(ns.into_iter().map(|n| self.issue_json(n)).collect(), &q, false),
                ))
            }
            ("GET", ["issues", n]) => Ok((200, self.issue_json(num(n).unwrap()).to_string())),
            ("PATCH", ["issues", n]) => {
                let n = num(n).unwrap();
                let eid = self.id();
                let mut st = self.st.borrow_mut();
                let i = &mut st.issues[(n - 1) as usize];
                if let Some(b) = req["body"].as_str() {
                    i.body = if self.rewrite_body {
                        b.replace('"', "&quot;")
                    } else {
                        b.into()
                    };
                }
                if let Some(s) = req["state"].as_str() {
                    if s != i.state {
                        let closing = s == "closed";
                        if !(closing && self.timeline_drop_close) {
                            i.events
                                .push((eid, if closing { "close" } else { "reopen" }));
                        }
                    }
                    i.state = s.into();
                    i.reason = req["state_reason"].as_str().map(String::from);
                }
                Ok((201, "{}".into()))
            }
            ("POST", ["issues", n, "labels"]) => {
                if !self.drop_label_add {
                    let ids: Vec<u64> = req["labels"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .filter_map(Value::as_u64)
                        .collect();
                    self.st.borrow_mut().issues[(num(n).unwrap() - 1) as usize]
                        .labels
                        .extend(ids);
                }
                Ok((200, "[]".into()))
            }
            ("DELETE", ["issues", n, "labels", id]) => {
                let id = num(id).unwrap();
                self.st.borrow_mut().issues[(num(n).unwrap() - 1) as usize]
                    .labels
                    .retain(|l| *l != id);
                Ok((204, String::new()))
            }
            ("POST", ["issues", n, "comments"]) => {
                let (n, id) = (num(n).unwrap(), self.id());
                let mut st = self.st.borrow_mut();
                st.comments
                    .push((id, n, req["body"].as_str().unwrap_or("").into()));
                st.issues[(n - 1) as usize].events.push((id, "comment"));
                Ok((201, json!({ "id": id }).to_string()))
            }
            ("GET", ["issues", n, "comments"]) => {
                let n = num(n).unwrap();
                let mut rows: Vec<Value> = self
                    .st
                    .borrow()
                    .comments
                    .iter()
                    .filter(|c| c.1 == n)
                    .map(|c| json!({"id": c.0, "body": c.2, "updated_at": "2026-10-08T00:00:00Z"}))
                    .collect();
                if self.reverse_comments {
                    rows.reverse();
                }
                Ok((200, self.page(rows, &q, self.comments_ignore_page)))
            }
            ("PATCH", ["issues", "comments", id]) => {
                let id = num(id).unwrap();
                if let Some(c) = self.st.borrow_mut().comments.iter_mut().find(|c| c.0 == id) {
                    c.2 = req["body"].as_str().unwrap_or("").into();
                }
                Ok((200, "{}".into()))
            }
            ("DELETE", ["issues", "comments", id]) => {
                let id = num(id).unwrap();
                self.st.borrow_mut().comments.retain(|c| c.0 != id);
                Ok((204, String::new()))
            }
            ("GET", ["issues", _, "timeline"]) if self.timeline_404 => Ok((404, "{}".into())),
            ("GET", ["issues", n, "timeline"]) => {
                let rows: Vec<Value> = self.st.borrow().issues[(num(n).unwrap() - 1) as usize].events.iter().map(|(id, t)| json!({"id": id, "type": t, "created_at": "2026-10-08T00:00:00Z"})).collect();
                Ok((200, self.page(rows, &q, false)))
            }
            ("GET", ["issues", _, "sub_issues"]) => Ok((self.sub_issues_status, "[]".into())),
            ("POST", ["labels"]) => {
                let id = self.id();
                let color = req["color"]
                    .as_str()
                    .unwrap_or("")
                    .trim_start_matches('#')
                    .to_string();
                let name = req["name"].as_str().unwrap_or("").to_string();
                self.st.borrow_mut().labels.push((
                    id,
                    name,
                    color,
                    req["description"].as_str().unwrap_or("").into(),
                ));
                Ok((201, self.label_json(id).to_string()))
            }
            ("GET", ["labels"]) => {
                let rows: Vec<Value> = self
                    .st
                    .borrow()
                    .labels
                    .iter()
                    .map(|l| l.0)
                    .collect::<Vec<_>>()
                    .into_iter()
                    .map(|id| self.label_json(id))
                    .collect();
                Ok((200, self.page(rows, &q, false)))
            }
            ("PATCH", ["labels", id]) => {
                let id = num(id).unwrap();
                if let Some(l) = self.st.borrow_mut().labels.iter_mut().find(|l| l.0 == id) {
                    l.2 = req["color"]
                        .as_str()
                        .unwrap_or("")
                        .trim_start_matches('#')
                        .into();
                    l.3 = req["description"].as_str().unwrap_or("").into();
                }
                Ok((200, "{}".into()))
            }
            ("DELETE", ["labels", id]) => {
                let id = num(id).unwrap();
                self.st.borrow_mut().labels.retain(|l| l.0 != id);
                Ok((204, String::new()))
            }
            _ => Err(format!("fake: unrouted {method} {path}")),
        }
    }
}

impl ProbeHttp for Mini {
    fn request(
        &self,
        method: &str,
        path: &str,
        _token: &str,
        body: Option<&str>,
    ) -> Result<(u16, String), String> {
        self.log.borrow_mut().push((method.into(), path.into()));
        if let Some((m, sub, code)) = self.status {
            if method == m && path.contains(sub) {
                return Ok((code, format!("{{\"message\":\"{TOKEN}\"}}")));
            }
        }
        if let Some((sub, nth)) = self.fail_get {
            if method == "GET" && path.contains(sub) {
                self.fail_get_seen.set(self.fail_get_seen.get() + 1);
                if self.fail_get_seen.get() == nth {
                    return Err(format!("connection reset (token {TOKEN})"));
                }
            }
        }
        let (code, body) = self.answer(method, path, body)?;
        if self.echo_token && body.starts_with('{') && body.len() > 2 {
            return Ok((code, format!("{{\"echo\":\"{TOKEN}\",{}", &body[1..])));
        }
        Ok((code, body))
    }
}

fn cfg(live_write: bool, only: &[&str]) -> RunnerConfig {
    RunnerConfig {
        origin: "https://gitea.example.com".into(),
        repo: "qual-org/loomp-test".into(),
        writer_token: TOKEN.into(),
        readonly_token: None,
        run_ns: "loomp-testrun".into(),
        live_write,
        timeout: Duration::from_secs(5),
        only: only.iter().map(|s| s.to_string()).collect(),
    }
}

fn row_of(results: &[CaseResult], case: &str) -> CaseResult {
    let id = format!("forge-probe::coordination::{case}");
    results
        .iter()
        .find(|r| r.test_id == id)
        .cloned()
        .unwrap_or_else(|| panic!("no row {id}"))
}

/// Run one case on `http` and return its row.
fn one(http: &dyn ProbeHttp, case: &str) -> CaseResult {
    row_of(&run(&cfg(true, &[case]), http).unwrap(), case)
}

const CASES: [&str; 13] = [
    "issue-create",
    "comment-create",
    "issue-view-state",
    "issue-list",
    "issue-search",
    "issue-edit-labels",
    "issue-edit-body",
    "issue-close-with-reason",
    "label-sync-catalogue",
    "comment-list",
    "comment-edit-delete",
    "timeline-read",
    "epic-sub-issues",
];

#[test]
fn every_coordination_row_executes_on_a_conforming_server() {
    let http = conforming();
    http.seed("foreign", 12); // enough rows for a multi-page issue walk
    let results = run(&cfg(true, &[]), &http).unwrap();
    for case in CASES {
        let r = row_of(&results, case);
        let want = if case == "epic-sub-issues" {
            OUTCOME_UNSUPPORTED
        } else {
            OUTCOME_PASS
        };
        assert_eq!(r.outcome, want, "{case}: {r:?}");
        assert!(!r.injected_fault, "{case}");
        assert_eq!(r.actor.as_deref(), Some("probe-writer"), "{case}");
    }
    // Other profiles and the unsupported containment keep the verdict nonzero.
    let (ok, unknown, _, unsupported) = verdict(&results);
    assert!(!ok && unknown > 0 && unsupported >= 1);
}

#[test]
fn read_only_mode_writes_nothing_and_leaves_every_case_unknown() {
    let http = conforming();
    let results = run(&cfg(false, &[]), &http).unwrap();
    for case in CASES {
        let r = row_of(&results, case);
        assert_eq!(r.outcome, OUTCOME_UNKNOWN, "{case}");
        assert!(r.observed.contains("refused: live-write not opted in"), "{case}: {r:?}");
    }
    assert!(http.log.borrow().iter().all(|(m, _)| m == "GET"), "{:?}", http.log.borrow());
}

#[test]
fn case_selection_is_independent() {
    let http = conforming();
    let results = run(&cfg(true, &["issue-edit-body"]), &http).unwrap();
    assert_eq!(row_of(&results, "issue-edit-body").outcome, OUTCOME_PASS);
    for case in CASES.iter().filter(|c| **c != "issue-edit-body") {
        assert!(
            row_of(&results, case)
                .notes
                .iter()
                .any(|n| n.contains("deselected")),
            "{case}"
        );
    }
}

#[test]
fn a_page_two_failure_is_unknown_and_flagged_injected_only_when_injected() {
    // Injected by the runner: unknown, flagged.
    let http = conforming();
    http.seed("foreign", 12);
    let injector = FaultInjector {
        inner: &http,
        page: 2,
    };
    let r = one(&injector, "issue-list");
    assert_eq!(r.outcome, OUTCOME_UNKNOWN, "{r:?}");
    assert!(r.injected_fault && r.observed.contains(INJECTED_PREFIX), "{r:?}");
    assert!(r.observed.contains("partial pagination: page 2"), "{r:?}");

    // The same failure from the service itself: unknown, NOT flagged.
    let mut http = conforming();
    http.fail_get = Some(("page=2", 1));
    http.seed("foreign", 12);
    let r = one(&http, "issue-list");
    assert_eq!(r.outcome, OUTCOME_UNKNOWN, "{r:?}");
    assert!(!r.injected_fault, "{r:?}");
}

#[test]
fn a_list_that_fits_one_page_did_not_exercise_pagination() {
    let http = conforming();
    let r = one(&http, "issue-list");
    assert_eq!(r.outcome, OUTCOME_UNKNOWN, "{r:?}");
    assert!(r.notes.iter().any(|n| n.contains("not exercised")), "{r:?}");
}

#[test]
fn a_fixture_missing_from_a_complete_list_fails() {
    let http = conforming();
    http.seed("foreign", 12);
    http.hide_from_list.set(Some(13)); // the first fixture
    assert_eq!(one(&http, "issue-list").outcome, OUTCOME_FAIL);
}

#[test]
fn page_ignoring_endpoints_are_complete_only_under_the_limit() {
    let mut http = conforming();
    http.comments_ignore_page = true;
    let r = one(&http, "comment-list");
    assert_eq!(r.outcome, OUTCOME_PASS, "{r:?}");
    assert!(r.observed.contains("page param ignored"), "{r:?}");

    // A full page that repeats on page 2 cannot prove completeness.
    let view = ProbeEntryView {
        id: "x".into(),
        test_id: None,
        risk: "normal".into(),
        disposition: "required".into(),
        class: "read".into(),
    };
    struct Repeat;
    impl ProbeHttp for Repeat {
        fn request(
            &self,
            _: &str,
            _: &str,
            _: &str,
            _: Option<&str>,
        ) -> Result<(u16, String), String> {
            Ok((200, r#"[{"id":1},{"id":2}]"#.into()))
        }
    }
    let c = cfg(true, &[]);
    let case = Case {
        test_id: "t",
        view: &view,
        cfg: &c,
        http: &Repeat,
        server_version: "v",
    };
    let err = case
        .walk("repos/x/y/labels", 2, 10)
        .unwrap_err()
        .to_string();
    assert!(err.contains("completeness unprovable"), "{err}");
    let w = case.walk("repos/x/y/labels", 5, 10).unwrap();
    assert!(w.page_ignored && w.rows.len() == 2);
}

#[test]
fn close_reason_distinguishes_native_absence_from_a_wrong_answer() {
    let mut http = conforming();
    assert_eq!(one(&http, "issue-close-with-reason").outcome, OUTCOME_PASS);
    http = conforming();
    http.reason = None; // live gitea-1: the field is absent
    let r = one(&http, "issue-close-with-reason");
    assert_eq!(r.outcome, OUTCOME_UNSUPPORTED, "{r:?}");
    assert!(r.observed.contains("field absent"));
    http = conforming();
    http.reason = Some(false); // present but null
    assert_eq!(one(&http, "issue-close-with-reason").outcome, OUTCOME_FAIL);
}

#[test]
fn a_rewritten_body_fails_without_echoing_it() {
    let mut http = conforming();
    http.rewrite_body = true;
    let r = one(&http, "issue-edit-body");
    assert_eq!(r.outcome, OUTCOME_FAIL);
    assert!(
        r.observed.contains("first difference at byte") && !r.observed.contains("&quot;"),
        "{r:?}"
    );
}

#[test]
fn label_add_must_be_visible_on_read_back() {
    let mut http = conforming();
    http.drop_label_add = true;
    assert_eq!(one(&http, "issue-edit-labels").outcome, OUTCOME_FAIL);
    assert_eq!(one(&conforming(), "issue-edit-labels").outcome, OUTCOME_PASS);
}

#[test]
fn a_failed_re_list_after_delete_is_unknown_never_deleted() {
    let mut http = conforming();
    http.fail_get = Some(("labels?page=1", 2)); // the post-delete walk
    let r = one(&http, "label-sync-catalogue");
    assert_eq!(r.outcome, OUTCOME_UNKNOWN, "{r:?}");
    let mut http = conforming();
    http.fail_get = Some(("comments?page=1", 2));
    assert_eq!(one(&http, "comment-edit-delete").outcome, OUTCOME_UNKNOWN);
}

#[test]
fn comment_order_must_be_creation_order() {
    let mut http = conforming();
    http.reverse_comments = true;
    let r = one(&http, "comment-list");
    assert_eq!(r.outcome, OUTCOME_FAIL);
    assert!(r.observed.contains("BROKEN"));
}

#[test]
fn search_honours_a_bounded_eventual_window() {
    let http = conforming();
    http.search_lag.set(2);
    let r = one(&http, "issue-search");
    assert_eq!(r.outcome, OUTCOME_PASS, "{r:?}");
    assert!(r.observed.contains("after 3 read(s)"), "{r:?}");
    let http = conforming();
    http.search_lag.set(99);
    assert_eq!(one(&http, "issue-search").outcome, OUTCOME_FAIL);
}

#[test]
fn timeline_absence_is_unsupported_and_a_missing_event_fails() {
    let mut http = conforming();
    http.timeline_404 = true;
    assert_eq!(one(&http, "timeline-read").outcome, OUTCOME_UNSUPPORTED);
    let mut http = conforming();
    http.timeline_drop_close = true;
    assert_eq!(one(&http, "timeline-read").outcome, OUTCOME_FAIL);
}

#[test]
fn a_bare_2xx_on_sub_issues_is_unknown_not_a_pass() {
    let mut http = conforming();
    http.sub_issues_status = 200;
    let r = one(&http, "epic-sub-issues");
    assert_eq!(r.outcome, OUTCOME_UNKNOWN, "{r:?}");
    assert_eq!(one(&conforming(), "epic-sub-issues").outcome, OUTCOME_UNSUPPORTED);
}

#[test]
fn unproven_fixture_ownership_blocks_every_further_write() {
    let mut http = conforming();
    http.wrong_title_on_read = true;
    let r = one(&http, "comment-list");
    assert_eq!(r.outcome, OUTCOME_UNKNOWN, "{r:?}");
    let writes: Vec<_> = http
        .log
        .borrow()
        .iter()
        .filter(|(m, _)| m != "GET")
        .cloned()
        .collect();
    assert_eq!(writes.len(), 1, "only the fixture create may write: {writes:?}");
}

#[test]
fn denied_and_transient_answers_are_unknown() {
    for code in [403u16, 429, 503] {
        let mut http = conforming();
        http.status = Some(("PATCH", "/issues/", code));
        let r = one(&http, "issue-edit-body");
        assert_eq!(r.outcome, OUTCOME_UNKNOWN, "{code}: {r:?}");
    }
}

#[test]
fn receipts_never_carry_the_token_even_from_a_reflecting_server() {
    let mut http = conforming();
    http.echo_token = true;
    http.seed("foreign", 12);
    let mut results = run(&cfg(true, &[]), &http).unwrap();
    let mut http = conforming();
    http.status = Some(("POST", "/labels", 500));
    http.fail_get = Some(("comments", 1));
    results.extend(run(&cfg(true, &[]), &http).unwrap());
    let receipt = serde_json::to_string(&results).unwrap();
    assert!(!receipt.contains(TOKEN), "{receipt}");
}

#[test]
fn cleanup_touches_only_this_run_namespace() {
    let http = conforming();
    http.seed(&format!("{NS_PREFIX}fixture"), 2); // #1, #2: ours
    http.seed("loomp-testrun2: other run", 1); // #3
    http.seed("loomp-testru: prefix of ours", 1); // #4
    http.seed("unrelated", 1); // #5
    for name in [
        format!("{NS_PREFIX}labels ✓"),
        "loomp-testrun2: x".into(),
        "loom:issue".into(),
    ] {
        let id = http.id();
        http.st
            .borrow_mut()
            .labels
            .push((id, name, "00aabb".into(), String::new()));
    }
    let report = cleanup(&cfg(true, &[]), &http).unwrap();
    assert!(report.is_clean(), "{report:?}");
    assert_eq!(report.issues_closed, vec![2, 1]);
    assert_eq!(report.labels_deleted.len(), 1);
    let st = http.st.borrow();
    assert!(st.issues[2..].iter().all(|i| i.state == "open"));
    assert_eq!(st.labels.len(), 2);
    let foreign_writes = http
        .log
        .borrow()
        .iter()
        .filter(|(m, p)| {
            m != "GET"
                && ["issues/3", "issues/4", "issues/5"]
                    .iter()
                    .any(|f| p.ends_with(f))
        })
        .count();
    assert_eq!(foreign_writes, 0);
}

#[test]
fn cleanup_refuses_without_live_write_and_reports_an_incomplete_walk() {
    let http = conforming();
    assert!(cleanup(&cfg(false, &[]), &http).is_err());
    assert!(http.log.borrow().is_empty());
    let mut http = conforming();
    http.fail_get = Some(("issues?state=open", 1));
    let report = cleanup(&cfg(true, &[]), &http).unwrap();
    assert!(!report.is_clean());
    assert!(!serde_json::to_string(&report).unwrap().contains(TOKEN));
}

#[test]
fn a_second_run_on_fresh_resources_reproduces_the_outcomes() {
    let http = conforming();
    http.seed("foreign", 12);
    let outcomes = |ns: &str| {
        let mut c = cfg(true, &[]);
        c.run_ns = ns.into();
        let r = run(&c, &http).unwrap();
        let report = cleanup(&c, &http).unwrap();
        assert!(report.is_clean());
        r.into_iter()
            .map(|r| (r.test_id, r.outcome))
            .collect::<Vec<_>>()
    };
    let first = outcomes("run-a");
    let second = outcomes("run-b");
    assert_eq!(first, second);
    // Each cleanup closed only its own run's fixtures.
    let st = http.st.borrow();
    assert!(st
        .issues
        .iter()
        .filter(|i| i.title == "foreign")
        .all(|i| i.state == "open"));
}

/// #9945: `run_ns` is the full namespace. Cleanup's prefix must be exactly
/// the prefix of every disposable title, or a live run leaks its fixtures.
#[test]
fn cleanup_prefix_matches_disposable_titles() {
    let c = cfg(true, &[]);
    let prefix = super::cleanup::namespace_prefix(&c);
    assert_eq!(prefix, NS_PREFIX);
    assert!(issue_title(&c, "issue-create").starts_with(&prefix));
    assert!(!prefix.starts_with("loomp-loomp-"), "{prefix}");
}
