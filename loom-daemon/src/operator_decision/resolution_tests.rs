use super::*;
use crate::operator_decision::render::compose_body;
use crate::operator_decision::{Decision, DecisionOption};

const TITLE: &str = "hermit: drop the legacy cache";
const REPORT: &str = "Remove it.\n<!-- loom:premise-check exists=yes deliberate=yes reversal=yes verdict=operator-decision -->\n";

pub(crate) fn opt(id: &str, why: &str) -> DecisionOption {
    DecisionOption {
        id: id.into(),
        label: format!("Option {id}"),
        why: Some(why.into()),
    }
}

pub(crate) fn decision(why_a: &str) -> Decision {
    Decision {
        question: "Reverse the documented cache design?".into(),
        context: "ctx".into(),
        options: vec![opt("keep", why_a), opt("drop", "why drop")],
        recommended: Some("keep".into()),
        deadline: None,
        context_links: vec![],
    }
}

pub(crate) fn body(why_a: &str) -> String {
    compose_body(REPORT, &decision(why_a))
}

fn cur(body: &str) -> Current {
    current("Acme/Widgets", 7, TITLE, body).unwrap()
}

fn res(c: &Current, option: &str, effect: Effect) -> Resolution {
    let o = c.decision.options.iter().find(|o| o.id == option).unwrap();
    Resolution {
        repo: c.repo.clone(),
        issue: c.issue,
        decision: c.decision_digest.clone(),
        scope: c.scope_digest.clone(),
        option: option.into(),
        option_digest: digest_option(o),
        effect,
    }
}

fn comment(r: &Resolution, operator: bool) -> Candidate {
    Candidate {
        body: render_comment(r, "Option"),
        operator,
    }
}

fn ev(c: Current, candidates: Vec<Candidate>) -> Evidence {
    Evidence {
        current: Ok(c),
        candidates,
    }
}

#[test]
fn no_resolution_is_pending() {
    let a = verify(&ev(cur(&body("w")), vec![]));
    assert_eq!(a.status, Status::Pending);
    assert!(!a.status.approves());
}

#[test]
fn a_verified_operator_approval_resolves() {
    let c = cur(&body("w"));
    let a = verify(&ev(c.clone(), vec![comment(&res(&c, "keep", Effect::Proceed), true)]));
    assert!(a.status.approves(), "{:?}", a.status);
    assert!(matches!(a.status, Status::Resolved(ref r) if r.option == "keep"));
}

#[test]
fn a_resolution_from_a_non_operator_is_ignored_and_counted() {
    let c = cur(&body("w"));
    let a = verify(&ev(c.clone(), vec![comment(&res(&c, "keep", Effect::Proceed), false)]));
    assert_eq!(a.status, Status::Pending);
    assert_eq!(a.ignored_unauthorized, 1);
}

#[test]
fn a_marker_quoted_mid_comment_is_prose() {
    let c = cur(&body("w"));
    let quoted = format!("I approve:\n{}", render_marker(&res(&c, "keep", Effect::Proceed)));
    let a = verify(&ev(
        c,
        vec![Candidate {
            body: quoted,
            operator: true,
        }],
    ));
    assert_eq!(a.status, Status::Pending);
}

#[test]
fn a_changed_option_under_the_same_id_is_stale() {
    let old = cur(&body("w"));
    let r = res(&old, "keep", Effect::Proceed);
    // Same ids, different meaning: a superseding decision.
    let new = cur(&body("a different why"));
    let a = verify(&ev(new, vec![comment(&r, true)]));
    assert!(matches!(a.status, Status::Stale(_)), "{:?}", a.status);
}

#[test]
fn editing_the_proposal_needs_renewed_approval() {
    let b = body("w");
    let c = cur(&b);
    let r = res(&c, "keep", Effect::Proceed);
    let edited = b.replace("Remove it.", "Remove it, and the sibling cache too.");
    let a = verify(&ev(cur(&edited), vec![comment(&r, true)]));
    assert!(
        matches!(a.status, Status::Stale(ref w) if w.contains("scope")),
        "{:?}",
        a.status
    );
}

#[test]
fn a_title_edit_changes_the_scope() {
    let b = body("w");
    let r = res(&cur(&b), "keep", Effect::Proceed);
    let retitled = current("acme/widgets", 7, "hermit: drop ALL caches", &b).unwrap();
    let a = verify(&ev(retitled, vec![comment(&r, true)]));
    assert!(matches!(a.status, Status::Stale(_)));
}

#[test]
fn routine_bookkeeping_does_not_invalidate_the_resolution() {
    let b = body("w");
    let c = cur(&b);
    let r = res(&c, "keep", Effect::Proceed);
    // A queue transition rewrites estimate/points/dashboard markers.
    let later = format!(
        "{b}\n<!-- loom:estimate lines=460 files=9 -->\n<!-- loom:points=5 -->\n<!-- loom:dashboard-link -->\n"
    );
    let a = verify(&ev(cur(&later), vec![comment(&r, true)]));
    assert!(a.status.approves(), "{:?}", a.status);
}

#[test]
fn republishing_an_identical_decision_keeps_the_approval_but_a_changed_one_does_not() {
    let b = body("w");
    let r = res(&cur(&b), "keep", Effect::Proceed);
    let again = compose_body(&b, &decision("w"));
    assert!(verify(&ev(cur(&again), vec![comment(&r, true)]))
        .status
        .approves());
    let changed = compose_body(&b, &decision("changed"));
    assert!(!verify(&ev(cur(&changed), vec![comment(&r, true)]))
        .status
        .approves());
}

#[test]
fn another_issue_or_repo_cannot_reuse_a_resolution() {
    let b = body("w");
    let c = cur(&b);
    let mut other_issue = res(&c, "keep", Effect::Proceed);
    other_issue.issue = 8;
    let mut other_repo = res(&c, "keep", Effect::Proceed);
    other_repo.repo = "evil/widgets".into();
    for r in [other_issue, other_repo] {
        let a = verify(&ev(c.clone(), vec![comment(&r, true)]));
        assert!(matches!(a.status, Status::Stale(_)), "{:?}", a.status);
    }
}

#[test]
fn an_unknown_option_or_altered_option_digest_is_stale() {
    let c = cur(&body("w"));
    let mut ghost = res(&c, "keep", Effect::Proceed);
    ghost.option = "ghost".into();
    let mut forged = res(&c, "keep", Effect::Proceed);
    forged.option_digest = "0".repeat(64);
    for r in [ghost, forged] {
        let a = verify(&ev(c.clone(), vec![comment(&r, true)]));
        assert!(matches!(a.status, Status::Stale(_)), "{:?}", a.status);
    }
}

#[test]
fn a_malformed_operator_record_blocks() {
    let c = cur(&body("w"));
    let good = comment(&res(&c, "keep", Effect::Proceed), true);
    let bad = Candidate {
        body: format!("{MARKER} v=1 repo=acme/widgets issue=7 effect=proceed -->"),
        operator: true,
    };
    let a = verify(&ev(c, vec![good, bad]));
    assert!(matches!(a.status, Status::Malformed(_)), "{:?}", a.status);
}

#[test]
fn conflicting_operator_records_block_but_identical_retries_do_not() {
    let c = cur(&body("w"));
    let keep = comment(&res(&c, "keep", Effect::Proceed), true);
    let drop = comment(&res(&c, "drop", Effect::Proceed), true);
    let a = verify(&ev(c.clone(), vec![keep.clone(), drop]));
    assert!(matches!(a.status, Status::Conflict(_)), "{:?}", a.status);
    let a = verify(&ev(c, vec![keep.clone(), keep]));
    assert!(a.status.approves(), "a retry is one effective receipt");
}

#[test]
fn reject_defer_and_rescope_never_approve() {
    let c = cur(&body("w"));
    for effect in [Effect::Reject, Effect::Defer, Effect::Rescope] {
        let a = verify(&ev(c.clone(), vec![comment(&res(&c, "drop", effect), true)]));
        assert!(matches!(a.status, Status::Resolved(_)));
        assert!(!a.status.approves(), "{effect:?}");
        assert!(a.status.reason().is_some());
    }
}

#[test]
fn an_unreadable_decision_fails_closed_with_a_diagnostic() {
    let missing = current("a/b", 7, TITLE, "no decision here").unwrap_err();
    let bad = current("a/b", 7, TITLE, "```decision\n{not json\n```\n").unwrap_err();
    let one_option = {
        let mut d = decision("w");
        d.options.truncate(1);
        current("a/b", 7, TITLE, &compose_body(REPORT, &d)).unwrap_err()
    };
    for why in [missing, bad, one_option] {
        let a = verify(&Evidence {
            current: Err(why.clone()),
            candidates: vec![],
        });
        assert_eq!(a.status, Status::Unreadable(why));
        assert!(!a.status.approves());
    }
}

#[test]
fn marker_round_trips_and_rejects_typos() {
    let c = cur(&body("w"));
    let r = res(&c, "keep", Effect::Proceed);
    assert_eq!(parse_marker(&render_comment(&r, "x")).unwrap(), r);
    let m = render_marker(&r);
    assert!(parse_marker(&m.replace("effect=", "efect=")).is_err());
    assert!(parse_marker(&m.replace(" v=1", " v=2")).is_err());
    assert!(parse_marker(&format!("{m} ").replace(" -->", " extra=1 -->")).is_err());
    assert!(parse_marker(&m.replace("issue=7", "issue=0")).is_err());
    assert!(parse_marker(&format!("{m}\n{m}").replacen(" -->", " scope=x -->", 1)).is_err());
}

#[test]
fn option_ids_the_marker_cannot_carry_are_flagged() {
    assert!(id_is_markable("keep-it_2.0:a"));
    for bad in ["", "a b", "a-->b", "a=b", "é"] {
        assert!(!id_is_markable(bad), "{bad:?}");
    }
}
