//! Signed operator-decision records (#10827): canonical bytes, the published
//! test vector, strict parsing, binding, keys, freshness and replay.

use aws_lc_rs::signature::{Ed25519KeyPair, KeyPair};
use base64::Engine as _;
use serde_json::json;

use super::*;
use crate::fleet_store::decision_signers::SignerKey;

/// Test-only seeds: `sha256("loom-10827-test-vector-key-a")` and `…-b`.
/// Published in `defaults/docs/comment-trust.md`; never a real key.
const SEED_A: &str = "05c3574eb3a78c83d2719f2755c0f1cac6fdec94fe8694b70a26dface35cbc08";
const SEED_B: &str = "a32e30ebcba5e1e0805bd9674f4eab8c313c156d434dcba6fbe0be1d8cdb7b42";
const PUB_A: &str = "4hMOQPTWOkE461Va0/ykByEMayRT+DM/4or9N+OA86Q=";
const PUB_B: &str = "vJWRze8CQnI5DrfEnTRNrKMhfPefAmHJ+yyXAnKDK1U=";

/// The published vector, signed with OpenSSL 4.0.3 (`openssl pkeyutl -sign
/// -rawin`), an implementation independent of the verifier under test.
const VECTOR_MARKER: &str = "<!-- loom:operator-decision v1 repo=acme/widgets issue=42 \
     decision=approve by=octo-admin at=2026-10-08T12:00:00Z key=test-a \
     sig=ed25519:rRAbMpmWIg/OA7RxZswtB15XMruYCHRmGxR4MbdiuJTrh1+AkHRUvpuvi/cR4TPAxCnFJiFG94rgLYB69sZ1CA== -->";
const VECTOR_BYTES: &str = "loom:operator-decision v1\nrepo=acme/widgets\nissue=42\n\
     decision=approve\nby=octo-admin\nat=2026-10-08T12:00:00Z\nkey=test-a\n";

fn key(id: &str, b64: &str, active: bool) -> SignerKey {
    SignerKey {
        id: id.to_string(),
        public_key: decode_canonical::<32>(b64).unwrap(),
        active,
    }
}

fn signers(keys: Vec<SignerKey>) -> Signers {
    Signers {
        keys,
        state: "loaded".into(),
    }
}

fn both_active() -> Signers {
    signers(vec![key("test-a", PUB_A, true), key("test-b", PUB_B, true)])
}

fn admins(logins: &[&str]) -> Admins {
    Admins {
        logins: logins.iter().map(|s| (*s).to_string()).collect(),
        state: "loaded".into(),
    }
}

fn at(s: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
}

fn now() -> DateTime<Utc> {
    at("2026-10-08T13:00:00Z")
}

fn here() -> Location {
    Location {
        repo: "acme/widgets".into(),
        issue: 42,
    }
}

fn ctx<'a>(s: &'a Signers, a: &'a Admins) -> Context<'a> {
    Context {
        signers: s,
        admins: a,
        now: now(),
        max_age: chrono::Duration::seconds(DEFAULT_MAX_AGE_SECS),
    }
}

/// Sign a record with a test seed (aws-lc), as the dashboard would.
fn signed(seed: &str, f: [&str; 6]) -> String {
    let [repo, issue, decision, by, at, key] = f;
    let msg = format!(
        "{SIGNED_DOMAIN}\nrepo={repo}\nissue={issue}\ndecision={decision}\nby={by}\nat={at}\nkey={key}\n"
    );
    let pair = Ed25519KeyPair::from_seed_unchecked(&hex::decode(seed).unwrap()).unwrap();
    let sig = base64::engine::general_purpose::STANDARD.encode(pair.sign(msg.as_bytes()).as_ref());
    format!(
        "<!-- loom:operator-decision v1 repo={repo} issue={issue} decision={decision} by={by} \
         at={at} key={key} sig=ed25519:{sig} -->"
    )
}

fn approve_at(t: &str) -> String {
    signed(SEED_A, ["acme/widgets", "42", "approve", "octo-admin", t, "test-a"])
}

fn check(line: &str) -> Result<VerifiedDecision, Rejection> {
    let (s, a) = (both_active(), admins(&["octo-admin"]));
    verify(line, &here(), &ctx(&s, &a))
}

#[test]
fn seeds_derive_the_published_public_keys() {
    for (seed, public) in [(SEED_A, PUB_A), (SEED_B, PUB_B)] {
        let pair = Ed25519KeyPair::from_seed_unchecked(&hex::decode(seed).unwrap()).unwrap();
        let got = base64::engine::general_purpose::STANDARD.encode(pair.public_key().as_ref());
        assert_eq!(got, public);
    }
}

#[test]
fn canonical_bytes_are_pinned() {
    let bytes = canonical_bytes(
        "acme/widgets",
        42,
        OperatorDecision::Approve,
        "octo-admin",
        "2026-10-08T12:00:00Z",
        "test-a",
    );
    assert_eq!(bytes, VECTOR_BYTES);
    assert!(bytes.ends_with("key=test-a\n"), "terminal newline is signed");
}

#[test]
fn published_vector_from_an_independent_implementation_verifies() {
    let v = check(VECTOR_MARKER).expect("the OpenSSL-signed vector verifies");
    assert_eq!(v.decision, OperatorDecision::Approve);
    assert_eq!((v.by.as_str(), v.key.as_str(), v.issue), ("octo-admin", "test-a", 42));
    assert_eq!(VECTOR_MARKER, approve_at("2026-10-08T12:00:00Z"), "Ed25519 is deterministic");
}

#[test]
fn rfc8032_test_1_verifies_through_the_same_primitive() {
    let public: [u8; 32] =
        hex::decode("d75a980182b10ab7d54bfed3c964073a0ee172f3daa62325af021a68f707511a")
            .unwrap()
            .try_into()
            .unwrap();
    let sig: [u8; 64] = hex::decode(
        "e5564300c360ac729086e2cc806e828a84877f1eb8e5d974d873e065224901555fb8821590a33bacc61e39701cf9b46bd25bf5f0595bbe24655141438e7a100b",
    )
    .unwrap()
    .try_into()
    .unwrap();
    assert!(signature_ok(&public, b"", &sig));
    assert!(!signature_ok(&public, b"x", &sig));
}

#[test]
fn every_single_field_mutation_of_the_vector_fails() {
    let mutations = [
        ("repo=acme/widgets", "repo=acme/gadgets", Rejection::WrongRepo),
        ("issue=42", "issue=43", Rejection::WrongIssue),
        ("decision=approve", "decision=reject", Rejection::BadSignature),
        ("decision=approve", "decision=defer", Rejection::BadSignature),
        ("by=octo-admin", "by=octo-admim", Rejection::BadSignature),
        ("at=2026-10-08T12:00:00Z", "at=2026-10-08T12:00:01Z", Rejection::BadSignature),
        ("key=test-a", "key=test-b", Rejection::BadSignature),
        ("sig=ed25519:rRAb", "sig=ed25519:rRAc", Rejection::BadSignature),
    ];
    for (from, to, want) in mutations {
        let line = VECTOR_MARKER.replacen(from, to, 1);
        assert_ne!(line, VECTOR_MARKER, "{from}");
        assert_eq!(check(&line), Err(want), "{from} -> {to}");
    }
    // The same mutations against a location that matches them still fail on
    // the signature: binding is not the only defence.
    let line = VECTOR_MARKER.replacen("issue=42", "issue=43", 1);
    let (s, a) = (both_active(), admins(&["octo-admin"]));
    let there = Location {
        repo: "acme/widgets".into(),
        issue: 43,
    };
    assert_eq!(verify(&line, &there, &ctx(&s, &a)), Err(Rejection::BadSignature));
}

#[test]
fn cross_repo_and_cross_issue_records_are_prose() {
    let other_repo = signed(
        SEED_A,
        [
            "acme/other",
            "42",
            "approve",
            "octo-admin",
            "2026-10-08T12:00:00Z",
            "test-a",
        ],
    );
    assert_eq!(check(&other_repo), Err(Rejection::WrongRepo));
    let other_issue = signed(
        SEED_A,
        [
            "acme/widgets",
            "7",
            "approve",
            "octo-admin",
            "2026-10-08T12:00:00Z",
            "test-a",
        ],
    );
    assert_eq!(check(&other_issue), Err(Rejection::WrongIssue));
}

#[test]
fn strict_parsing_rejects_every_alternative_spelling() {
    let good = VECTOR_MARKER;
    let cases: Vec<(String, Rejection)> = vec![
        (good.replacen(" v1 ", " v2 ", 1), Rejection::UnknownVersion),
        (good.replacen(" v1 ", " V1 ", 1), Rejection::Malformed),
        (
            good.replacen("decision=approve", "decision=merge", 1),
            Rejection::UnknownDecision,
        ),
        (
            good.replacen("decision=approve", "decision=Approve", 1),
            Rejection::UnknownDecision,
        ),
        // duplicate / reordered / unknown fields
        (good.replacen(" key=test-a", " key=test-a key=test-a", 1), Rejection::Malformed),
        (
            good.replacen("issue=42 decision=approve", "decision=approve issue=42", 1),
            Rejection::Malformed,
        ),
        (good.replacen(" key=test-a", " note=x key=test-a", 1), Rejection::Malformed),
        (good.replacen(" by=", " by_=", 1), Rejection::Malformed),
        // whitespace and trailing data
        (good.replacen(" issue=", "  issue=", 1), Rejection::Malformed),
        (good.replacen(" issue=", "\tissue=", 1), Rejection::Malformed),
        (good.replacen(" -->", "  -->", 1), Rejection::Malformed),
        (format!("{good} trailing"), Rejection::Malformed),
        (good.replacen(" -->", " extra -->", 1), Rejection::Malformed),
        // non-canonical repo / login / issue / timestamp / base64
        (good.replacen("repo=acme/widgets", "repo=Acme/widgets", 1), Rejection::Malformed),
        (good.replacen("by=octo-admin", "by=Octo-admin", 1), Rejection::Malformed),
        (good.replacen("by=octo-admin", "by=octo-admin[bot]", 1), Rejection::Malformed),
        (good.replacen("issue=42", "issue=042", 1), Rejection::Malformed),
        (good.replacen("issue=42", "issue=0", 1), Rejection::Malformed),
        (good.replacen("issue=42", "issue=+42", 1), Rejection::Malformed),
        (good.replacen("12:00:00Z", "12:00:00.000Z", 1), Rejection::Malformed),
        (good.replacen("12:00:00Z", "12:00:00+00:00", 1), Rejection::Malformed),
        (good.replacen("2026-10-08T", "2026-10-08 ", 1), Rejection::Malformed),
        (good.replacen("2026-10-08T", "2026-13-08T", 1), Rejection::Malformed),
        (good.replacen("CA== -->", "CA -->", 1), Rejection::Malformed),
        (good.replacen("rRAbMpmWIg/", "rRAbMpmWIg_", 1), Rejection::Malformed),
        (good.replacen("sig=ed25519:", "sig=ed448:", 1), Rejection::Malformed),
        (good.replacen("sig=ed25519:rRAb", "sig=ed25519:", 1), Rejection::Malformed),
        (good.replacen("key=test-a", "key=Test-A", 1), Rejection::Malformed),
    ];
    for (line, want) in cases {
        assert_eq!(check(&line), Err(want), "{line}");
    }
}

#[test]
fn keys_rotate_and_revocation_fails_closed() {
    let by_b = signed(
        SEED_B,
        [
            "acme/widgets",
            "42",
            "approve",
            "octo-admin",
            "2026-10-08T12:00:00Z",
            "test-b",
        ],
    );
    // Two simultaneously active keys: each verifies only its own markers.
    assert!(check(VECTOR_MARKER).is_ok());
    assert!(check(&by_b).is_ok());
    let claims_a = by_b.replacen("key=test-b", "key=test-a", 1);
    assert_eq!(check(&claims_a), Err(Rejection::BadSignature), "no try-every-key");
    let a = admins(&["octo-admin"]);
    let revoked = signers(vec![key("test-a", PUB_A, false), key("test-b", PUB_B, true)]);
    assert_eq!(verify(VECTOR_MARKER, &here(), &ctx(&revoked, &a)), Err(Rejection::UnknownKey));
    let removed = signers(vec![key("test-b", PUB_B, true)]);
    assert_eq!(verify(VECTOR_MARKER, &here(), &ctx(&removed, &a)), Err(Rejection::UnknownKey));
    let unknown = signed(
        SEED_A,
        [
            "acme/widgets",
            "42",
            "approve",
            "octo-admin",
            "2026-10-08T12:00:00Z",
            "test-z",
        ],
    );
    assert_eq!(check(&unknown), Err(Rejection::UnknownKey));
    let gone = Signers::unavailable("fleet store unreachable");
    assert_eq!(
        verify(VECTOR_MARKER, &here(), &ctx(&gone, &a)),
        Err(Rejection::SignersUnavailable)
    );
}

#[test]
fn signer_must_be_a_fleet_admin_independently_of_the_key() {
    let s = both_active();
    let not_admin = admins(&["someone-else"]);
    assert_eq!(verify(VECTOR_MARKER, &here(), &ctx(&s, &not_admin)), Err(Rejection::NotAdmin));
    let gone = Admins::unavailable("no file");
    assert_eq!(
        verify(VECTOR_MARKER, &here(), &ctx(&s, &gone)),
        Err(Rejection::AdminsUnavailable)
    );
    let mixed_case_roster = admins(&["Octo-Admin"]);
    assert!(verify(VECTOR_MARKER, &here(), &ctx(&s, &mixed_case_roster)).is_ok());
}

#[test]
fn freshness_window_and_future_skew() {
    // now = 13:00; default window 7 days; skew 5 minutes.
    assert!(check(&approve_at("2026-10-01T13:00:00Z")).is_ok(), "exactly at the window edge");
    assert_eq!(check(&approve_at("2026-10-01T12:59:59Z")), Err(Rejection::Expired));
    assert!(check(&approve_at("2026-10-08T13:05:00Z")).is_ok(), "inside the skew allowance");
    assert_eq!(check(&approve_at("2026-10-08T13:05:01Z")), Err(Rejection::FutureSkew));
    let (s, a) = (both_active(), admins(&["octo-admin"]));
    let mut short = ctx(&s, &a);
    short.max_age = chrono::Duration::minutes(30);
    assert_eq!(verify(VECTOR_MARKER, &here(), &short), Err(Rejection::Expired));
}

#[test]
fn window_config_is_bounded() {
    assert_eq!(max_age_from_config(&json!({})).num_seconds(), DEFAULT_MAX_AGE_SECS);
    let huge = json!({"forge": {"operatorDecisionMaxAgeSecs": 10_000_000_000_i64}});
    assert_eq!(max_age_from_config(&huge).num_seconds(), MAX_MAX_AGE_SECS);
    let negative = json!({"forge": {"operatorDecisionMaxAgeSecs": -5}});
    assert_eq!(max_age_from_config(&negative).num_seconds(), 0);
    let junk = json!({"forge": {"operatorDecisionMaxAgeSecs": "forever"}});
    assert_eq!(max_age_from_config(&junk).num_seconds(), DEFAULT_MAX_AGE_SECS);
}

fn comment(body: &str) -> Value {
    json!({
        "user": {"login": "decision-desk[bot]", "type": "Bot"},
        "author_association": "NONE",
        "issue_url": "https://api.github.com/repos/Acme/Widgets/issues/42",
        "body": body,
    })
}

fn newest_of(bodies: &[String]) -> Evaluation {
    let (s, a) = (both_active(), admins(&["octo-admin"]));
    let comments: Vec<Value> = bodies.iter().map(|b| comment(b)).collect();
    newest(&comments, &here(), &ctx(&s, &a))
}

#[test]
fn newest_valid_record_wins() {
    let older = approve_at("2026-10-08T10:00:00Z");
    let newer = signed(
        SEED_A,
        [
            "acme/widgets",
            "42",
            "reject",
            "octo-admin",
            "2026-10-08T11:00:00Z",
            "test-a",
        ],
    );
    for order in [
        [older.clone(), newer.clone()],
        [newer.clone(), older.clone()],
    ] {
        let e = newest_of(&order);
        assert_eq!(e.newest.unwrap().decision, OperatorDecision::Reject, "order-independent");
    }
}

#[test]
fn equal_timestamps_resolve_deterministically_to_the_conservative_decision() {
    let t = "2026-10-08T11:00:00Z";
    let approve = approve_at(t);
    let defer = signed(SEED_B, ["acme/widgets", "42", "defer", "octo-admin", t, "test-b"]);
    let reject = signed(SEED_A, ["acme/widgets", "42", "reject", "octo-admin", t, "test-a"]);
    for order in [
        vec![approve.clone(), defer.clone(), reject.clone()],
        vec![reject.clone(), approve.clone(), defer.clone()],
        vec![defer.clone(), reject.clone(), approve.clone()],
    ] {
        assert_eq!(newest_of(&order).newest.unwrap().decision, OperatorDecision::Reject);
    }
    assert_eq!(newest_of(&[approve, defer]).newest.unwrap().decision, OperatorDecision::Defer);
}

#[test]
fn a_newer_invalid_marker_cannot_shadow_an_older_valid_one() {
    let valid = approve_at("2026-10-08T10:00:00Z");
    let forged = signed(
        SEED_A,
        [
            "acme/widgets",
            "42",
            "reject",
            "octo-admin",
            "2026-10-08T12:30:00Z",
            "test-a",
        ],
    )
    .replacen("decision=reject", "decision=defer", 1);
    let wrong_issue = signed(
        SEED_A,
        [
            "acme/widgets",
            "9",
            "reject",
            "octo-admin",
            "2026-10-08T12:40:00Z",
            "test-a",
        ],
    );
    let not_admin = signed(
        SEED_A,
        [
            "acme/widgets",
            "42",
            "reject",
            "mallory",
            "2026-10-08T12:50:00Z",
            "test-a",
        ],
    );
    let e = newest_of(&[valid, forged, wrong_issue, not_admin]);
    let d = e
        .newest
        .clone()
        .expect("the older valid record still stands");
    assert_eq!(d.decision, OperatorDecision::Approve);
    assert_eq!(e.rejected_summary(), "wrong-issue:1,bad-signature:1,not-admin:1");
}

#[test]
fn expiry_returns_the_issue_to_no_signed_decision() {
    let e = newest_of(&[approve_at("2026-09-01T00:00:00Z")]);
    assert!(e.newest.is_none());
    assert_eq!(e.rejected_summary(), "expired:1");
}

#[test]
fn a_comment_carries_at_most_one_record_on_its_own_line() {
    let ok = approve_at("2026-10-08T10:00:00Z");
    let crlf = format!("Operator approved this from the dashboard.\r\n{ok}\r\n");
    assert!(newest_of(&[crlf]).newest.is_some(), "CRLF bodies are fine");
    let two = format!("{ok}\n{}", approve_at("2026-10-08T10:30:00Z"));
    let e = newest_of(&[two]);
    assert!(e.newest.is_none());
    assert_eq!(e.rejected_summary(), "ambiguous:1");
    let indented = format!("  {ok}");
    assert_eq!(newest_of(&[indented]).rejected_summary(), "malformed:1");
    let quoted = format!("> {ok}");
    let e = newest_of(&[quoted]);
    assert!(e.newest.is_none() && e.rejected.is_empty(), "a quoted marker is plain prose");
}

#[test]
fn a_comment_placed_elsewhere_counts_for_nothing() {
    let (s, a) = (both_active(), admins(&["octo-admin"]));
    let mut c = comment(&approve_at("2026-10-08T10:00:00Z"));
    c["issue_url"] = json!("https://api.github.com/repos/acme/widgets/issues/43");
    let e = newest(&[c], &here(), &ctx(&s, &a));
    assert!(e.newest.is_none());
    assert_eq!(e.rejected_summary(), "wrong-issue:1");
}

#[test]
fn locations_come_from_the_forge_urls() {
    let l =
        Location::from_issue_url("https://api.github.com/repos/Acme/Widgets/issues/42").unwrap();
    assert_eq!(l, here());
    assert!(
        Location::from_issue_url("https://api.github.com/repos/acme/widgets/pulls/42").is_none()
    );
    assert!(
        Location::from_issue_url("https://api.github.com/repos/acme/widgets/issues/42/x").is_none()
    );
    let issue =
        json!({"number": 42, "repository_url": "https://api.github.com/repos/acme/widgets"});
    assert_eq!(Location::from_issue_object(&issue), Some(here()));
}
