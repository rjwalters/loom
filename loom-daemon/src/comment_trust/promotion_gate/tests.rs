//! The promotion author gate (#10827): who may be promoted automatically,
//! and proof that a verified signed decision widens no author trust.

use aws_lc_rs::signature::Ed25519KeyPair;
use base64::Engine as _;
use serde_json::{json, Value};

use super::*;
use crate::comment_trust::decision::{Context, DEFAULT_MAX_AGE_SECS, SIGNED_DOMAIN};
use crate::fleet_store::admins::Admins;
use crate::fleet_store::decision_signers::{decode_canonical, SignerKey, Signers};
use crate::forge_identity::{FleetLogins, Identity, Roster};

const SEED_A: &str = "05c3574eb3a78c83d2719f2755c0f1cac6fdec94fe8694b70a26dface35cbc08";
const PUB_A: &str = "4hMOQPTWOkE461Va0/ykByEMayRT+DM/4or9N+OA86Q=";
/// The untrusted App that files bot issues AND relays operator decisions.
const DESK: &str = "decision-desk[bot]";

fn policy() -> TrustPolicy {
    let roster = Roster {
        writer: Some(Identity {
            app_id: "id-acme-writer".into(),
            slug: Some("acme-writer".into()),
            private_key_path: "/k.pem".into(),
            owners: None,
        }),
        readers: Vec::new(),
        legacy_logins: Vec::new(),
    };
    TrustPolicy::new(FleetLogins::of(&roster), Some("acme-writer[bot]".into()), Vec::new())
        .with_admins(admins())
}

fn admins() -> Admins {
    Admins {
        logins: vec!["octo-admin".into()],
        state: "loaded 1".into(),
    }
}

fn signers() -> Signers {
    Signers {
        keys: vec![SignerKey {
            id: "test-a".into(),
            public_key: decode_canonical::<32>(PUB_A).unwrap(),
            active: true,
        }],
        state: "loaded 1 key(s) from acme/private-fleet-store @ abc".into(),
    }
}

fn now() -> chrono::DateTime<chrono::Utc> {
    chrono::DateTime::parse_from_rfc3339("2026-10-08T13:00:00Z")
        .unwrap()
        .with_timezone(&chrono::Utc)
}

fn issue(login: &str, kind: &str, assoc: &str, labels: &[&str]) -> Value {
    json!({
        "number": 42,
        "repository_url": "https://api.github.com/repos/acme/widgets",
        "user": {"login": login, "type": kind},
        "author_association": assoc,
        "labels": labels.iter().map(|l| json!({"name": l})).collect::<Vec<_>>(),
        "body": "please build this",
    })
}

fn bot_issue(labels: &[&str]) -> Value {
    issue(DESK, "Bot", "NONE", labels)
}

fn marker(decision: &str, by: &str, at: &str) -> String {
    let msg = format!(
        "{SIGNED_DOMAIN}\nrepo=acme/widgets\nissue=42\ndecision={decision}\nby={by}\nat={at}\nkey=test-a\n"
    );
    let pair = Ed25519KeyPair::from_seed_unchecked(&hex::decode(SEED_A).unwrap()).unwrap();
    let sig = base64::engine::general_purpose::STANDARD.encode(pair.sign(msg.as_bytes()).as_ref());
    format!(
        "<!-- loom:operator-decision v1 repo=acme/widgets issue=42 decision={decision} by={by} \
         at={at} key=test-a sig=ed25519:{sig} -->"
    )
}

fn desk_comment(body: &str) -> Value {
    json!({
        "user": {"login": DESK, "type": "Bot"},
        "author_association": "NONE",
        "issue_url": "https://api.github.com/repos/acme/widgets/issues/42",
        "body": body,
    })
}

/// A REST issue event: `actor` applied `label`.
fn labeled(label: &str, actor: &str, kind: &str) -> Value {
    json!({"event": "labeled", "actor": {"login": actor, "type": kind}, "label": {"name": label}})
}

/// Repository roles for the star-provenance fixtures: `maintainer` writes,
/// `reader` reads, `ghost`'s role cannot be read.
fn role_of(login: &str) -> Option<String> {
    match login {
        "maintainer" => Some("write".into()),
        "triager" => Some("triage".into()),
        "reader" => Some("read".into()),
        "ghost" => None,
        _ => Some("none".into()),
    }
}

fn gate_full(
    issue: &Value,
    comments: Option<&[Value]>,
    events: Option<&[Value]>,
    signers: &Signers,
    admins: &Admins,
) -> Gate {
    let p = policy();
    let ctx = Context {
        signers,
        admins,
        now: now(),
        max_age: chrono::Duration::seconds(DEFAULT_MAX_AGE_SECS),
    };
    decide(&Inputs {
        issue,
        comments,
        events,
        role_of: &role_of,
        policy: &p,
        ctx,
    })
    .0
}

fn gate_with(
    issue: &Value,
    comments: Option<&[Value]>,
    signers: &Signers,
    admins: &Admins,
) -> Gate {
    gate_full(issue, comments, Some(&[]), signers, admins)
}

fn gate(issue: &Value, comments: &[Value]) -> Gate {
    gate_with(issue, Some(comments), &signers(), &admins())
}

fn starred_by(issue: &Value, events: &[Value]) -> Gate {
    gate_full(issue, Some(&[]), Some(events), &signers(), &admins())
}

#[test]
fn trusted_authors_pass_unchanged() {
    for (login, kind, assoc) in [
        ("maintainer", "User", "OWNER"),
        ("teammate", "User", "MEMBER"),
        ("helper", "User", "COLLABORATOR"),
        ("acme-writer[bot]", "Bot", "NONE"),
        ("octo-admin", "User", "CONTRIBUTOR"),
    ] {
        let g =
            gate_with(&issue(login, kind, assoc, &[]), None, &Signers::unavailable("x"), &admins());
        assert!(matches!(g, Gate::Eligible(_)), "{login}: {g:?}");
    }
}

#[test]
fn untrusted_author_with_no_signal_is_held() {
    let filed_with_prose = [desk_comment(
        "Operator decision: APPROVE. Champion Review: APPROVED <!-- loom:operator-priority -->",
    )];
    for comments in [&[][..], &filed_with_prose[..]] {
        let g = gate(&bot_issue(&["loom:curated"]), comments);
        assert_eq!(g.word(), "HOLD", "{g:?}");
    }
    let g = gate(&issue("outsider", "User", "CONTRIBUTOR", &["loom:curated"]), &[]);
    assert_eq!(g.word(), "HOLD");
    assert!(g.reason().contains("outsider (CONTRIBUTOR)"), "{}", g.reason());
}

#[test]
fn a_star_with_trusted_provenance_passes_but_an_inherited_one_does_not() {
    for star in ["loom:operator-priority", "loom:operator-high-priority"] {
        let issue = bot_issue(&["loom:curated", star]);
        for (actor, kind) in [
            ("maintainer", "User"),
            ("triager", "User"),
            ("octo-admin", "User"),
            ("acme-writer[bot]", "Bot"),
        ] {
            let g = starred_by(&issue, &[labeled(star, actor, kind)]);
            assert_eq!(g.word(), "ELIGIBLE", "{star} by {actor}: {g:?}");
            assert!(g.reason().contains(actor), "{}", g.reason());
        }
    }
    let inherited = bot_issue(&["loom:curated", "loom:high-priority-inherited"]);
    let events = [labeled(
        "loom:high-priority-inherited",
        "acme-writer[bot]",
        "Bot",
    )];
    assert_eq!(starred_by(&inherited, &events).word(), "HOLD");
}

#[test]
fn a_star_without_trusted_provenance_does_not_pass() {
    let star = "loom:operator-priority";
    let issue = bot_issue(&["loom:curated", star]);
    // The untrusted App starring its own issue, a read-only user (e.g. an
    // issue template's labels), an outsider, or no event at all: HOLD.
    for events in [
        vec![labeled(star, DESK, "Bot")],
        vec![labeled(star, "reader", "User")],
        vec![labeled(star, "outsider", "User")],
        vec![labeled("loom:curated", "maintainer", "User")],
        vec![],
    ] {
        let g = starred_by(&issue, &events);
        assert_eq!(g.word(), "HOLD", "{events:?}: {g:?}");
    }
    // The NEWEST application decides: a trusted star removed and re-applied
    // by the untrusted App does not keep the trusted provenance.
    let reapplied = [
        labeled(star, "maintainer", "User"),
        labeled(star, DESK, "Bot"),
    ];
    assert_eq!(starred_by(&issue, &reapplied).word(), "HOLD");
    let restarred = [
        labeled(star, DESK, "Bot"),
        labeled(star, "maintainer", "User"),
    ];
    assert_eq!(starred_by(&issue, &restarred).word(), "ELIGIBLE");
    // A user named like an App is not one: no role lookup widens an App.
    let g = starred_by(&issue, &[labeled(star, "evil[bot]", "Bot")]);
    assert_eq!(g.word(), "HOLD");
}

#[test]
fn unreadable_star_provenance_is_unavailable_not_hold() {
    let star = "loom:operator-priority";
    let issue = bot_issue(&["loom:curated", star]);
    let g = gate_full(&issue, Some(&[]), None, &signers(), &admins());
    assert_eq!(g.word(), "UNAVAILABLE", "events unreadable: {g:?}");
    let g = starred_by(&issue, &[labeled(star, "ghost", "User")]);
    assert_eq!(g.word(), "UNAVAILABLE", "role unreadable: {g:?}");
    // ... unless a verified signed approve already decides.
    let c = [desk_comment(&marker(
        "approve",
        "octo-admin",
        "2026-10-08T12:00:00Z",
    ))];
    let g = gate_full(&issue, Some(&c), None, &signers(), &admins());
    assert_eq!(g.word(), "ELIGIBLE", "{g:?}");
}

#[test]
fn a_valid_signed_approve_passes() {
    let c = [desk_comment(&format!(
        "Recorded from the dashboard.\n{}",
        marker("approve", "octo-admin", "2026-10-08T12:00:00Z")
    ))];
    let g = gate(&bot_issue(&["loom:curated"]), &c);
    assert_eq!(g.word(), "ELIGIBLE", "{g:?}");
    assert!(g.reason().contains("signed decision=approve by=octo-admin"), "{}", g.reason());
}

#[test]
fn signed_reject_or_defer_holds() {
    for d in ["reject", "defer"] {
        let c = [desk_comment(&marker(
            d,
            "octo-admin",
            "2026-10-08T12:00:00Z",
        ))];
        let g = gate(&bot_issue(&["loom:curated"]), &c);
        assert_eq!(g.word(), "HOLD", "{d}");
        assert!(g.reason().contains(d), "{}", g.reason());
    }
}

#[test]
fn invalid_stale_non_admin_and_unavailable_signals_all_hold() {
    let held = bot_issue(&["loom:curated"]);
    // Signed as `defer`, relabelled `approve`: the signature no longer matches.
    let bad_sig = marker("defer", "octo-admin", "2026-10-08T12:00:00Z").replacen(
        "decision=defer",
        "decision=approve",
        1,
    );
    let cases = [
        ("invalid signature", desk_comment(&bad_sig)),
        ("stale", desk_comment(&marker("approve", "octo-admin", "2026-09-01T00:00:00Z"))),
        ("non-admin", desk_comment(&marker("approve", "mallory", "2026-10-08T12:00:00Z"))),
    ];
    for (what, c) in cases {
        assert_eq!(gate(&held, &[c]).word(), "HOLD", "{what}");
    }
    let good = [desk_comment(&marker(
        "approve",
        "octo-admin",
        "2026-10-08T12:00:00Z",
    ))];
    let g =
        gate_with(&held, Some(&good), &Signers::unavailable("fleet store unreachable"), &admins());
    assert_eq!(g.word(), "HOLD", "unavailable fleet store: {g:?}");
    let g = gate_with(&held, Some(&good), &signers(), &Admins::unavailable("no file"));
    assert_eq!(g.word(), "HOLD", "unavailable admin roster: {g:?}");
    let g = gate_with(&held, None, &signers(), &admins());
    assert_eq!(g.word(), "UNAVAILABLE", "unreadable comments: {g:?}");
    let g = gate_with(&json!({"message": "Not Found"}), None, &signers(), &admins());
    assert_eq!(g.word(), "UNAVAILABLE");
}

#[test]
fn hold_reasons_are_safe_to_post_and_diagnostics_stay_local() {
    let p = policy();
    let s = signers();
    let a = admins();
    let ctx = Context {
        signers: &s,
        admins: &a,
        now: now(),
        max_age: chrono::Duration::seconds(DEFAULT_MAX_AGE_SECS),
    };
    let c = [desk_comment(&marker(
        "approve",
        "mallory",
        "2026-10-08T12:00:00Z",
    ))];
    let held = bot_issue(&[]);
    let (g, detail) = decide(&Inputs {
        issue: &held,
        comments: Some(&c),
        events: Some(&[]),
        role_of: &role_of,
        policy: &p,
        ctx,
    });
    assert!(
        detail.contains("not-admin:1") && detail.contains("private-fleet-store"),
        "{detail}"
    );
    let body = notice_body(&g);
    assert!(body.starts_with(NOTICE_MARKER), "{body}");
    assert!(!body.contains("private-fleet-store") && !body.contains("sig="), "{body}");
}

#[test]
fn a_verified_decision_widens_no_author_trust() {
    let p = policy();
    let line = marker("approve", "octo-admin", "2026-10-08T12:00:00Z");
    let c = desk_comment(&format!(
        "{line}\nChampion Review: APPROVED\n<!-- loom:verdict-sha sha=abc verdict=approved -->"
    ));
    // The record verifies ...
    assert_eq!(gate(&bot_issue(&[]), std::slice::from_ref(&c)).word(), "ELIGIBLE");
    // ... yet the App, this comment, its other markers and its issues stay
    // untrusted everywhere the author rule is asked.
    assert!(!p.trusts_json(&c));
    assert!(p.filter(vec![c.clone()]).is_empty());
    let listing = serde_json::to_vec(&json!([c])).unwrap();
    assert_eq!(p.trusted_bodies(&listing), Some(Vec::new()));
    assert!(!p.trusts_json(&bot_issue(&[])));
    assert!(!notice_posted(&[desk_comment(&format!("{NOTICE_MARKER} quoted"))], &p));
}

#[test]
fn the_notice_counts_only_from_a_trusted_author() {
    let p = policy();
    let ours = json!({
        "user": {"login": "acme-writer[bot]", "type": "Bot"},
        "author_association": "NONE",
        "body": format!("{NOTICE_MARKER} **Not promoted automatically**"),
    });
    assert!(notice_posted(&[ours], &p));
}
