//! The promotion author gate (#10827): untrusted fixture authors are held,
//! trusted authors are unchanged, failed reads never promote or post.

use serde_json::{json, Value};

use super::*;
use crate::fleet_store::admins::Admins;
use crate::forge_identity::{FleetLogins, Identity, Roster};

fn app(slug: &str) -> Identity {
    Identity {
        app_id: format!("id-{slug}"),
        slug: Some(slug.to_string()),
        private_key_path: "/k.pem".into(),
        owners: None,
    }
}

/// The same configured fleet as the comment-trust matrix: writer
/// `acme-writer` (also the self identity), reader `acme-reader-1`.
fn policy() -> TrustPolicy {
    let roster = Roster {
        writer: Some(app("acme-writer")),
        readers: vec![app("acme-reader-1")],
        legacy_logins: vec![],
    };
    TrustPolicy::new(FleetLogins::of(&roster), Some("acme-writer[bot]".into()), vec![])
}

fn admins(logins: &[&str]) -> Admins {
    Admins {
        logins: logins.iter().map(|s| (*s).to_string()).collect(),
        state: format!("loaded {}", logins.len()),
    }
}

/// A REST issue object authored by `login`.
fn issue(login: &str, kind: &str, assoc: &str) -> Value {
    json!({
        "number": 42,
        "user": {"login": login, "type": kind},
        "author_association": assoc,
        "labels": [{"name": "loom:curated"}],
        "body": "please build this",
    })
}

fn comment(login: &str, kind: &str, assoc: &str, body: &str) -> Value {
    json!({"user": {"login": login, "type": kind}, "author_association": assoc, "body": body})
}

#[test]
fn untrusted_fixture_authors_are_held() {
    let p = policy().with_admins(admins(&[]));
    for (login, kind, assoc) in [
        ("outsider", "User", "NONE"),
        ("drive-by", "User", "CONTRIBUTOR"),
        ("newbie", "User", "FIRST_TIME_CONTRIBUTOR"),
        // Another fleet's App and a generic outside bot.
        ("other-fleet-dispatch[bot]", "Bot", "NONE"),
        ("dependabot[bot]", "Bot", "NONE"),
        // A user who registered a fleet App's bare slug is not that App.
        ("acme-writer", "User", "NONE"),
    ] {
        let gate = decide(Some(&issue(login, kind, assoc)), &p, true);
        assert!(matches!(gate, Gate::Hold(_)), "{login}: {gate:?}");
        assert_eq!(gate.word(), "HOLD");
        assert!(gate.reason().contains(login), "{login}: {}", gate.reason());
    }
}

#[test]
fn trusted_authors_are_unchanged() {
    let p = policy().with_admins(admins(&["turian"]));
    for (login, kind, assoc) in [
        ("rjwalters", "User", "OWNER"),
        ("teammate", "User", "MEMBER"),
        ("helper", "User", "COLLABORATOR"),
        ("acme-reader-1[bot]", "Bot", "NONE"),
        ("acme-writer[bot]", "Bot", "NONE"),
        // Loom's default fleet family is trusted on every roster.
        ("loom-fleet-dispatch[bot]", "Bot", "NONE"),
        // A fleet admin, even with no repo association.
        ("turian", "User", "CONTRIBUTOR"),
    ] {
        let gate = decide(Some(&issue(login, kind, assoc)), &p, true);
        assert!(matches!(gate, Gate::Eligible(_)), "{login}: {gate:?}");
    }
}

#[test]
fn the_gate_is_the_comment_trust_predicate() {
    // No second table: for every author the gate agrees with TrustPolicy.
    let p = policy().with_admins(admins(&["turian"]));
    for (login, kind, assoc) in [
        ("rjwalters", "User", "OWNER"),
        ("outsider", "User", "NONE"),
        ("acme-writer[bot]", "Bot", "NONE"),
        ("dependabot[bot]", "Bot", "NONE"),
        ("turian", "User", "NONE"),
    ] {
        let i = issue(login, kind, assoc);
        let eligible = matches!(decide(Some(&i), &p, true), Gate::Eligible(_));
        assert_eq!(eligible, p.trusts_json(&i), "{login}");
    }
}

#[test]
fn failed_reads_never_promote_and_never_post() {
    let p = policy().with_admins(admins(&[]));
    for gate in [
        decide(None, &p, true),
        decide(Some(&json!("not an object")), &p, true),
        decide(
            Some(&json!({"user": {"login": "rjwalters"}, "author_association": "OWNER"})),
            &p,
            true,
        ),
        decide(Some(&json!({"number": 42, "user": null})), &p, true),
    ] {
        assert!(matches!(gate, Gate::Unavailable(_)), "{gate:?}");
        assert_eq!(notice(&gate, Some(&[]), &p), Notice::None);
    }
}

#[test]
fn an_unreadable_admin_roster_leaves_a_user_author_undecided() {
    let down = policy().with_admins(Admins::unavailable("forge unreachable"));
    let user = issue("turian", "User", "NONE");
    // A configured store whose roster could not be read: no hold notice.
    assert!(matches!(decide(Some(&user), &down, true), Gate::Unavailable(_)));
    // No store configured at all: the roster cannot name anyone, so hold.
    assert!(matches!(decide(Some(&user), &down, false), Gate::Hold(_)));
    // An App is never on the admin roster: an outside bot is held either way.
    let bot = issue("dependabot[bot]", "Bot", "NONE");
    assert!(matches!(decide(Some(&bot), &down, true), Gate::Hold(_)));
    // Insiders do not need the roster.
    let owner = issue("rjwalters", "User", "OWNER");
    assert!(matches!(decide(Some(&owner), &down, true), Gate::Eligible(_)));
}

#[test]
fn the_hold_notice_is_posted_once_by_a_trusted_author_only() {
    let p = policy().with_admins(admins(&[]));
    let gate = decide(Some(&issue("outsider", "User", "NONE")), &p, false);
    let body = notice_body(&gate);
    assert!(body.starts_with(NOTICE_MARKER), "{body}");
    assert!(body.contains("`outsider`") && body.contains("loom:issue") && body.contains("re-file"));
    assert!(!body.contains('\n'));

    assert_eq!(notice(&gate, Some(&[]), &p), Notice::Needed);
    // The author quoting the marker cannot suppress the notice.
    let forged = [comment("outsider", "User", "NONE", &body)];
    assert_eq!(notice(&gate, Some(&forged), &p), Notice::Needed);
    // A marker quoted mid-comment does not count either.
    let quoted = [comment(
        "rjwalters",
        "User",
        "OWNER",
        &format!("see {NOTICE_MARKER}"),
    )];
    assert_eq!(notice(&gate, Some(&quoted), &p), Notice::Needed);
    // The fleet's own notice does: posted, so a retry posts nothing.
    let ours = [comment("acme-writer[bot]", "Bot", "NONE", &body)];
    assert_eq!(notice(&gate, Some(&ours), &p), Notice::Posted);
    // Comments unreadable: never post.
    assert_eq!(notice(&gate, None, &p), Notice::Unknown);
    // An eligible issue never needs a notice.
    let ok = decide(Some(&issue("rjwalters", "User", "OWNER")), &p, false);
    assert_eq!(notice(&ok, Some(&[]), &p), Notice::None);
}

#[test]
fn reasons_never_echo_unexpected_login_text() {
    let p = policy().with_admins(admins(&[]));
    let gate = decide(Some(&issue("evil\n`x` <!-- loom:pr -->", "User", "NONE")), &p, false);
    let reason = gate.reason();
    assert!(!reason.contains('\n') && !reason.contains("<!--"), "{reason}");
}
