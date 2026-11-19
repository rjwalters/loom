//! #14399: the premise gate's view of an operator's resolution. One
//! synthetic deliberate reversal with two ranked options, driven through the
//! same `decide_with` that Curator preflight and Champion promotion reach via
//! `premise-check`.

use super::*;
use crate::comment_trust::TrustPolicy;
use crate::forge_identity::FleetLogins;
use crate::operator_decision::render::compose_body;
use crate::operator_decision::resolution::{
    candidates, current, digest_option, render_comment, Effect, Evidence, Resolution,
};
use crate::operator_decision::{Decision as Ranked, DecisionOption};
use serde_json::{json, Value};

const TITLE: &str = "hermit: drop the legacy cache";
const REPORT: &str = "Remove it.\n\
<!-- loom:premise-check exists=yes deliberate=yes reversal=yes verdict=operator-decision -->\n\
premise-evidence: Cargo.toml:1 — documented as intended\npremise-protected: none\n";

fn decision() -> Ranked {
    let o = |id: &str| DecisionOption {
        id: id.into(),
        label: format!("Option {id}"),
        why: Some(format!("why {id}")),
    };
    Ranked {
        question: "Reverse it?".into(),
        context: "ctx".into(),
        options: vec![o("keep"), o("drop")],
        recommended: Some("keep".into()),
        deadline: None,
        context_links: vec![],
    }
}

fn root() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("Cargo.toml"), "").unwrap();
    dir
}

fn policy() -> TrustPolicy {
    TrustPolicy::new(FleetLogins::of(&crate::forge_identity::Roster::default()), None, Vec::new())
}

fn rest(login: &str, kind: &str, assoc: &str, body: &str) -> Value {
    json!({"user": {"login": login, "type": kind}, "author_association": assoc, "body": body})
}

fn graphql(login: &str, kind: &str, assoc: &str, body: &str) -> Value {
    json!({"author": {"login": login, "__typename": kind}, "authorAssociation": assoc, "body": body})
}

fn resolution_for(body: &str, option: &str, effect: Effect) -> Resolution {
    let c = current("acme/widgets", 7, TITLE, body).unwrap();
    let o = c.decision.options.iter().find(|o| o.id == option).unwrap();
    Resolution {
        repo: c.repo.clone(),
        issue: 7,
        decision: c.decision_digest.clone(),
        scope: c.scope_digest.clone(),
        option: option.into(),
        option_digest: digest_option(o),
        effect,
    }
}

fn issue_body() -> String {
    compose_body(REPORT, &decision())
}

/// The gate's inputs as the forge flow builds them: `comments` is the raw
/// listing (REST or GraphQL shape).
fn gate(body: &str, labels: &[&str], listing: &[Value]) -> Decision {
    let evidence = Evidence {
        current: current("acme/widgets", 7, TITLE, body),
        candidates: candidates(&policy(), listing),
    };
    decide_with(
        &Inputs {
            title: TITLE.into(),
            body: body.into(),
            labels: labels.iter().map(ToString::to_string).collect(),
            comments: vec![],
            resolution: Some(evidence),
        },
        root().path(),
        ReversalPolicy::Always,
    )
}

fn approval(body: &str) -> String {
    render_comment(&resolution_for(body, "keep", Effect::Proceed), "Option keep")
}

#[test]
fn unresolved_reversal_routes_to_the_operator() {
    let d = gate(&issue_body(), &["loom:hermit"], &[]);
    assert_eq!(d.exit_code, exit::ROUTE_OPERATOR);
    assert_eq!(d.resolution.unwrap().status.token(), "pending");
}

#[test]
fn a_verified_operator_approval_proceeds_and_leaves_the_record_intact() {
    let b = issue_body();
    for c in [
        rest("rjwalters", "User", "OWNER", &approval(&b)),
        graphql("rjwalters", "User", "MEMBER", &approval(&b)),
    ] {
        let d = gate(&b, &["loom:hermit"], &[c]);
        assert_eq!(d.exit_code, exit::PROCEED, "{:?}", d.resolution);
        assert_eq!(d.outcome, Outcome::Proceed);
        // The original premise and its reversal claim stay auditable.
        let rec = d.record.unwrap();
        assert!(rec.deliberate && rec.reversal && rec.verdict == record::Verdict::OperatorDecision);
    }
}

#[test]
fn the_same_approval_does_not_clear_a_reversal_rewritten_to_clear() {
    let b = issue_body().replace("verdict=operator-decision", "verdict=clear");
    let listing = [rest("rjwalters", "User", "OWNER", &approval(&issue_body()))];
    let d = gate(&b, &["loom:hermit"], &listing);
    assert_eq!(d.exit_code, exit::RECORD_MALFORMED);
    assert!(d.resolution.is_none(), "an invalid premise never reaches the resolution");
}

#[test]
fn authors_without_operator_authority_leave_the_gate_closed() {
    let b = issue_body();
    let a = approval(&b);
    let cases = [
        rest("loom-fleet-dispatch[bot]", "Bot", "NONE", &a),
        graphql("loom-fleet-dispatch", "Bot", "NONE", &a),
        rest("drive-by", "User", "CONTRIBUTOR", &a),
        graphql("drive-by", "User", "NONE", &a),
        json!({"body": a, "author": null}),
        json!({"body": a, "user": {"login": "rjwalters", "type": "User"}}),
    ];
    for c in cases {
        let d = gate(&b, &["loom:hermit"], std::slice::from_ref(&c));
        assert_eq!(d.exit_code, exit::ROUTE_OPERATOR, "{c}");
    }
}

#[test]
fn rest_and_graphql_agree_on_operator_authority() {
    let a = approval(&issue_body());
    let pairs = [
        (
            rest("rjwalters", "User", "OWNER", &a),
            graphql("rjwalters", "User", "OWNER", &a),
        ),
        (rest("x", "User", "COLLABORATOR", &a), graphql("x", "User", "COLLABORATOR", &a)),
        (rest("app[bot]", "Bot", "OWNER", &a), graphql("app", "Bot", "OWNER", &a)),
        (rest("x", "User", "CONTRIBUTOR", &a), graphql("x", "User", "CONTRIBUTOR", &a)),
    ];
    for (r, g) in pairs {
        let (r, g) = (candidates(&policy(), &[r]), candidates(&policy(), &[g]));
        assert_eq!(r[0].operator, g[0].operator);
    }
}

#[test]
fn a_changed_decision_changed_scope_or_other_issue_stays_pending() {
    let b = issue_body();
    let listing = [rest("rjwalters", "User", "OWNER", &approval(&b))];
    let mut changed_option = decision();
    changed_option.options[0].why = Some("now means something else".into());
    for body in [
        compose_body(&b, &changed_option),
        b.replace("Remove it.", "Remove everything."),
    ] {
        let d = gate(&body, &["loom:hermit"], &listing);
        assert_eq!(d.exit_code, exit::ROUTE_OPERATOR);
        assert_eq!(d.resolution.unwrap().status.token(), "stale");
    }
    // Same decision, another issue number.
    let evidence = Evidence {
        current: current("acme/widgets", 8, TITLE, &b),
        candidates: candidates(&policy(), &listing),
    };
    let d = decide_with(
        &Inputs {
            title: TITLE.into(),
            body: b,
            labels: vec!["loom:hermit".into()],
            comments: vec![],
            resolution: Some(evidence),
        },
        root().path(),
        ReversalPolicy::Always,
    );
    assert_eq!(d.exit_code, exit::ROUTE_OPERATOR);
}

#[test]
fn reject_defer_and_rescope_carry_no_proceed_approval() {
    let b = issue_body();
    for effect in [Effect::Reject, Effect::Defer, Effect::Rescope] {
        let c = render_comment(&resolution_for(&b, "drop", effect), "Option drop");
        let d = gate(&b, &["loom:hermit"], &[rest("rjwalters", "User", "OWNER", &c)]);
        assert_eq!(d.exit_code, exit::ROUTE_OPERATOR, "{effect:?}");
    }
}

#[test]
fn offline_reproduction_can_never_open_the_gate() {
    // `--body-file` / `--record-file`: no forge read, so no evidence. A
    // pasted approval comment is only text.
    let b = issue_body();
    let d = decide_with(
        &Inputs {
            title: TITLE.into(),
            body: b.clone(),
            labels: vec!["loom:hermit".into()],
            comments: vec![approval(&b), "approved, go ahead".into()],
            resolution: None,
        },
        root().path(),
        ReversalPolicy::Always,
    );
    assert_eq!(d.exit_code, exit::ROUTE_OPERATOR);
    assert_eq!(d.resolution.unwrap().status.token(), "unavailable");
}

#[test]
fn an_unreadable_decision_fails_closed() {
    let b = issue_body();
    let listing = [rest("rjwalters", "User", "OWNER", &approval(&b))];
    let prose_only = REPORT.to_string();
    let d = gate(&prose_only, &["loom:hermit"], &listing);
    assert_eq!(d.exit_code, exit::ROUTE_OPERATOR);
    assert_eq!(d.resolution.unwrap().status.token(), "unreadable");
}

#[test]
fn an_unpark_history_or_plain_comment_attests_nothing() {
    let b = issue_body();
    let listing = [
        rest("rjwalters", "User", "OWNER", "Approved. Going with `keep`."),
        rest("rjwalters", "User", "OWNER", "<!-- champion:proposal-unescalated -->"),
    ];
    let d = gate(&b, &["loom:hermit"], &listing);
    assert_eq!(d.exit_code, exit::ROUTE_OPERATOR);
}

#[test]
fn a_non_reversal_operator_decision_is_untouched() {
    let b = issue_body().replace("reversal=yes", "reversal=no");
    let d = gate(&b, &["loom:hermit"], &[]);
    assert!(d.resolution.is_none());
}

#[test]
fn the_hermit_policy_exception_is_preserved() {
    // `hermit.reversalNeedsOperator=protected-only`: a Hermit removal that
    // attests `premise-protected: none` is `verdict=clear` and never reaches
    // the resolution at all.
    let b = issue_body().replace("verdict=operator-decision", "verdict=clear");
    let d = decide_with(
        &Inputs {
            title: TITLE.into(),
            body: b,
            labels: vec!["loom:hermit".into()],
            comments: vec![],
            resolution: None,
        },
        root().path(),
        ReversalPolicy::ProtectedOnly,
    );
    assert_eq!(d.exit_code, exit::PROCEED);
    assert!(d.resolution.is_none());
}
