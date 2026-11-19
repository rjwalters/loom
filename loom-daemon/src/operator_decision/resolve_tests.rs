use super::*;
use crate::comment_trust::TrustPolicy;
use crate::forge_identity::FleetLogins;
use crate::operator_decision::resolution::tests::body;
use serde_json::{json, Value};
use std::cell::RefCell;
use std::rc::Rc;

#[derive(Default)]
struct State {
    view: IssueView,
    comments: Vec<Value>,
    /// Author JSON stamped on a comment this fake posts.
    poster: Value,
    log: Vec<String>,
}

struct Fake(Rc<RefCell<State>>);

impl ResolveForge for Fake {
    fn repo(&mut self) -> Result<String, String> {
        Ok("Acme/Widgets".into())
    }
    fn view(&mut self, _: u64) -> Result<IssueView, String> {
        Ok(self.0.borrow().view.clone())
    }
    fn comments(&mut self, _: u64) -> Result<Vec<Value>, String> {
        Ok(self.0.borrow().comments.clone())
    }
    fn post_comment(&mut self, _: u64, b: &str) -> Result<(), String> {
        let mut s = self.0.borrow_mut();
        s.log.push("post".into());
        let mut c = s.poster.clone();
        c["body"] = json!(b);
        s.comments.push(c);
        Ok(())
    }
    fn remove_label(&mut self, _: u64, l: &str) -> Result<(), String> {
        let mut s = self.0.borrow_mut();
        s.log.push(format!("remove:{l}"));
        s.view.labels.retain(|x| x != l);
        Ok(())
    }
}

fn operator() -> Value {
    json!({"user": {"login": "rjwalters", "type": "User"}, "author_association": "OWNER"})
}
fn bot() -> Value {
    json!({"user": {"login": "loom-fleet-dispatch[bot]", "type": "Bot"}, "author_association": "NONE"})
}

fn state(poster: Value) -> Rc<RefCell<State>> {
    Rc::new(RefCell::new(State {
        view: IssueView {
            title: "hermit: drop the legacy cache".into(),
            body: body("w"),
            labels: vec!["loom:hermit".into(), DECISION_LABEL.into()],
        },
        poster,
        ..State::default()
    }))
}

fn policy() -> TrustPolicy {
    TrustPolicy::new(FleetLogins::of(&crate::forge_identity::Roster::default()), None, Vec::new())
}

fn req(option: &str) -> ResolveRequest {
    ResolveRequest {
        issue: 7,
        option: option.into(),
        effect: Effect::Proceed,
        release: false,
        dry_run: false,
    }
}

struct Out {
    code: i32,
    out: String,
    err: String,
}

fn run(s: &Rc<RefCell<State>>, r: &ResolveRequest, confirm: &mut dyn FnMut(&str) -> bool) -> Out {
    let (mut o, mut e) = (Vec::new(), Vec::new());
    let code = resolve(&mut Fake(s.clone()), &policy(), r, confirm, &mut o, &mut e);
    Out {
        code,
        out: String::from_utf8(o).unwrap(),
        err: String::from_utf8(e).unwrap(),
    }
}

fn yes(_: &str) -> bool {
    true
}

fn posts(s: &Rc<RefCell<State>>) -> usize {
    s.borrow().log.iter().filter(|l| *l == "post").count()
}

#[test]
fn dry_run_shows_the_review_and_touches_nothing() {
    let s = state(operator());
    let mut r = req("keep");
    r.dry_run = true;
    let o = run(&s, &r, &mut |_| panic!("a dry run asks nothing"));
    assert_eq!(o.code, 0, "{}", o.err);
    for k in [
        "OPTION=keep",
        "OPTION_LABEL=Option keep",
        "OPTION_WHY=w",
        "EFFECT=proceed",
        "DECISION_DIGEST=",
        "SCOPE_DIGEST=",
        "DRY_RUN=true",
    ] {
        assert!(o.out.contains(k), "{k} in {}", o.out);
    }
    assert!(s.borrow().log.is_empty());
}

#[test]
fn an_unconfirmed_review_records_nothing() {
    let s = state(operator());
    let o = run(&s, &req("keep"), &mut |_| false);
    assert_eq!(o.code, exit::REFUSED);
    assert!(s.borrow().log.is_empty());
}

#[test]
fn records_verifies_then_releases_in_that_order() {
    let s = state(operator());
    let mut r = req("keep");
    r.release = true;
    let mut asked = None;
    let o = run(&s, &r, &mut |e| {
        asked = Some(e.to_string());
        true
    });
    assert_eq!(o.code, 0, "{}", o.err);
    assert_eq!(asked.as_deref(), Some("keep"), "the operator confirms the option id");
    assert!(o.out.contains("RECORDED=true") && o.out.contains("VERIFIED=true"));
    assert!(o.out.contains("RELEASED=true"));
    assert_eq!(s.borrow().log, ["post", &format!("remove:{DECISION_LABEL}")]);
}

#[test]
fn a_retry_keeps_a_single_effective_receipt() {
    let s = state(operator());
    assert_eq!(run(&s, &req("keep"), &mut yes).code, 0);
    let again = run(&s, &req("keep"), &mut yes);
    assert_eq!(again.code, 0, "{}", again.err);
    assert!(again.out.contains("RECORDED=already"));
    assert_eq!(posts(&s), 1);
}

#[test]
fn a_different_standing_resolution_is_refused_not_stacked() {
    let s = state(operator());
    assert_eq!(run(&s, &req("keep"), &mut yes).code, 0);
    let o = run(&s, &req("drop"), &mut yes);
    assert_eq!(o.code, exit::REFUSED, "{}", o.err);
    assert_eq!(posts(&s), 1);
}

#[test]
fn a_scope_change_between_review_and_recording_is_refused() {
    let s = state(operator());
    let racer = s.clone();
    let o = run(&s, &req("keep"), &mut |_| {
        let mut st = racer.borrow_mut();
        st.view.body = st.view.body.replace("Remove it.", "Remove it and more.");
        true
    });
    assert_eq!(o.code, exit::REFUSED);
    assert!(o.err.contains("changed"), "{}", o.err);
    assert_eq!(posts(&s), 0);
}

#[test]
fn a_label_or_comment_change_between_review_and_recording_is_not_a_scope_change() {
    let s = state(operator());
    let racer = s.clone();
    let o = run(&s, &req("keep"), &mut |_| {
        let mut st = racer.borrow_mut();
        st.view.labels.push("loom:important".into());
        st.comments
            .push(json!({"body": "hello", "user": {"login": "x", "type": "User"}}));
        true
    });
    assert_eq!(o.code, 0, "{}", o.err);
}

#[test]
fn an_option_that_is_not_published_is_refused() {
    let s = state(operator());
    let o = run(&s, &req("ghost"), &mut yes);
    assert_eq!(o.code, exit::REFUSED);
    assert!(o.err.contains("keep, drop"), "{}", o.err);
    assert!(s.borrow().log.is_empty());
}

#[test]
fn an_unreadable_decision_is_refused_and_names_why() {
    let s = state(operator());
    s.borrow_mut().view.body = "just prose".into();
    let o = run(&s, &req("keep"), &mut yes);
    assert_eq!(o.code, exit::REFUSED);
    assert!(o.err.contains("decision"), "{}", o.err);
    assert!(s.borrow().log.is_empty());
}

#[test]
fn a_posting_account_the_forge_does_not_call_an_operator_never_releases() {
    let s = state(bot());
    let mut r = req("keep");
    r.release = true;
    let o = run(&s, &r, &mut yes);
    assert_eq!(o.code, UNVERIFIED, "{}", o.err);
    assert!(s.borrow().log.iter().all(|l| !l.starts_with("remove:")), "{:?}", s.borrow().log);
    assert!(s.borrow().view.labels.iter().any(|l| l == DECISION_LABEL));
}

#[test]
fn release_is_only_for_a_proceed_resolution() {
    let s = state(operator());
    let mut r = req("drop");
    r.effect = Effect::Reject;
    r.release = true;
    assert_eq!(run(&s, &r, &mut yes).code, exit::USAGE);
    assert!(s.borrow().log.is_empty());
}

#[test]
fn a_non_proceed_resolution_is_recorded_without_release() {
    let s = state(operator());
    let mut r = req("drop");
    r.effect = Effect::Defer;
    let o = run(&s, &r, &mut yes);
    assert_eq!(o.code, 0, "{}", o.err);
    assert!(s.borrow().view.labels.iter().any(|l| l == DECISION_LABEL));
}
