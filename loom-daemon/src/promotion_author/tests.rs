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
    assert!(notice_body(&Gate::Trusted, None).is_empty());
    let body = notice_body(&held, None);
    assert!(body.starts_with(NOTICE_MARKER));
    assert!(body.contains("drive-by"));
    assert_eq!(body, notice_body(&held, None), "a retry posts the same, marker-anchored text");
    assert!(notice_body(&Gate::Unknown("x".into()), None).contains("could not be verified"));
}

#[test]
fn a_stamped_notice_round_trips_and_a_legacy_one_has_no_stamp() {
    let held = Gate::Untrusted(Some("drive-by".to_string()));
    let stamp = Stamp {
        rev: revision("t", "b"),
        trust: "0123456789abcdef".to_string(),
    };
    let body = notice_body(&held, Some(&stamp));
    assert!(body.starts_with("<!-- loom:promotion-author-gate rev="));
    assert_eq!(Stamp::parse(&body), Some(stamp));
    assert_eq!(Stamp::parse(&notice_body(&held, None)), None);
    assert_eq!(Stamp::parse("no marker here"), None);
}

#[test]
fn the_revision_moves_with_an_edit_and_ignores_trailing_newlines() {
    assert_eq!(revision("t", "b"), revision("t", "b\n"));
    assert_ne!(revision("t", "b"), revision("t", "b2"));
    assert_ne!(revision("t", "b"), revision("t2", "b"));
}

const TRACKER_BODY: &str = "x\n\nThis issue is opened, updated and closed by \
                            `.github/workflows/ci-daily.yml`. It closes itself.";

fn tracker(login: &str, title: &str, body: &str) -> Value {
    let mut v = issue(login, "Bot", "NONE");
    v["title"] = json!(title);
    v["body"] = json!(body);
    v
}

#[test]
fn a_ci_daily_tracker_from_the_actions_bot_passes() {
    let v = tracker("github-actions[bot]", "[ci-daily] build is red", TRACKER_BODY);
    assert_eq!(classify(&policy(), Some(&v)), Gate::Trusted);
}

#[test]
fn the_tracker_exception_needs_login_prefix_and_body_sentence() {
    let p = policy();
    let t = "[ci-daily] build is red";
    for v in [
        tracker("github-actions[bot]", "[ci-daily]", TRACKER_BODY),
        tracker("github-actions[bot]", "ci-daily build is red", TRACKER_BODY),
        tracker("github-actions[bot]", t, "no sentence"),
        tracker("github-actions[bot]", t, ""),
        tracker("other[bot]", t, TRACKER_BODY),
        tracker("github-actions", t, TRACKER_BODY),
        tracker("drive-by", t, TRACKER_BODY),
    ] {
        assert!(matches!(classify(&p, Some(&v)), Gate::Untrusted(_)), "{v}");
    }
    let mut no_fields = issue("github-actions[bot]", "Bot", "NONE");
    assert!(matches!(classify(&p, Some(&no_fields)), Gate::Untrusted(_)));
    no_fields["pull_request"] = json!({});
    no_fields["title"] = json!(t);
    no_fields["body"] = json!(TRACKER_BODY);
    assert!(matches!(classify(&p, Some(&no_fields)), Gate::Unknown(_)), "a PR stays unknown");
}
