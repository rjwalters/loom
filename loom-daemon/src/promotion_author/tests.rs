//! Unit tests for the #10827 promotion author gate.

use super::*;
use crate::forge_identity::{FleetLogins, Roster};
use serde_json::json;

fn policy() -> TrustPolicy {
    TrustPolicy::new(FleetLogins::of(&Roster::default()), None, vec!["partner".to_string()])
}

fn issue(login: &str, kind: &str, assoc: &str) -> Value {
    json!({"user": {"login": login, "type": kind}, "author_association": assoc, "number": 7})
}

#[test]
fn trusted_authors_pass_by_association_and_allowlist() {
    let p = policy();
    assert_eq!(classify(&p, Some(&issue("rjwalters", "User", "OWNER"))), Gate::Trusted);
    assert_eq!(classify(&p, Some(&issue("dev", "User", "COLLABORATOR"))), Gate::Trusted);
    assert_eq!(classify(&p, Some(&issue("partner", "User", "NONE"))), Gate::Trusted);
}

#[test]
fn untrusted_authors_are_held_with_their_login() {
    let p = policy();
    let gate = classify(&p, Some(&issue("drive-by", "User", "NONE")));
    assert_eq!(gate, Gate::Untrusted(Some("drive-by".to_string())));
    assert_eq!(gate.token(), "UNTRUSTED drive-by");
    assert!(matches!(
        classify(&p, Some(&issue("outside[bot]", "Bot", "NONE"))),
        Gate::Untrusted(_)
    ));
    assert!(matches!(
        classify(&p, Some(&issue("c", "User", "CONTRIBUTOR"))),
        Gate::Untrusted(_)
    ));
}

#[test]
fn a_deleted_author_is_untrusted_not_trusted() {
    let p = policy();
    assert_eq!(classify(&p, Some(&json!({"number": 7}))), Gate::Untrusted(None));
}

#[test]
fn unreadable_evidence_is_unknown_never_trusted() {
    let p = policy();
    let gate = classify(&p, None);
    assert!(matches!(gate, Gate::Unknown(_)));
    assert!(gate.token().starts_with("UNKNOWN "));
    let mut pr = issue("rjwalters", "User", "OWNER");
    pr["pull_request"] = json!({});
    assert!(matches!(classify(&p, Some(&pr)), Gate::Unknown(_)));
}

#[test]
fn only_the_trusted_gate_has_no_notice_and_retries_render_identically() {
    let held = Gate::Untrusted(Some("drive-by".to_string()));
    assert!(notice_body(&Gate::Trusted).is_empty());
    let body = notice_body(&held);
    assert!(body.starts_with(NOTICE_MARKER));
    assert!(body.contains("drive-by"));
    assert_eq!(body, notice_body(&held), "a retry posts the same, marker-anchored text");
    assert!(notice_body(&Gate::Unknown("x".into())).contains("could not be verified"));
}
