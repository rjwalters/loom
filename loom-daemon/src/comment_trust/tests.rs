use serde_json::json;

use super::*;
use crate::forge_identity::{FleetLogins, Identity, Roster};

fn app(slug: &str) -> Identity {
    Identity {
        app_id: format!("id-{slug}"),
        slug: Some(slug.to_string()),
        private_key_path: "/k.pem".into(),
    }
}

/// A configured fleet: writer `acme-writer`, reader `acme-reader-1`, legacy
/// `acme-old`.
fn policy(allow: &[&str]) -> TrustPolicy {
    let roster = Roster {
        writer: Some(app("acme-writer")),
        readers: vec![app("acme-reader-1")],
        legacy_logins: vec!["acme-old".into()],
    };
    TrustPolicy::new(
        FleetLogins::of(&roster),
        Some("acme-writer[bot]".into()),
        allow.iter().map(|s| (*s).to_string()).collect(),
    )
}

fn rest(login: &str, kind: &str, assoc: &str) -> Value {
    json!({"user": {"login": login, "type": kind}, "author_association": assoc, "body": "b"})
}

fn graphql(login: &str, assoc: &str) -> Value {
    json!({"author": {"login": login}, "authorAssociation": assoc, "body": "b"})
}

#[test]
fn insiders_are_trusted_by_association_contributors_never() {
    let p = policy(&[]);
    for assoc in ["OWNER", "MEMBER", "COLLABORATOR", "collaborator"] {
        assert!(p.trusts_json(&rest("anyone", "User", assoc)), "{assoc}");
        assert!(p.trusts_json(&graphql("anyone", assoc)), "{assoc}");
    }
    for assoc in [
        "CONTRIBUTOR",
        "FIRST_TIME_CONTRIBUTOR",
        "FIRST_TIMER",
        "NONE",
        "MANNEQUIN",
    ] {
        assert!(!p.trusts_json(&rest("drive-by", "User", assoc)), "{assoc}");
        assert!(!p.trusts_json(&graphql("drive-by", assoc)), "{assoc}");
    }
}

#[test]
fn every_roster_app_is_trusted_but_only_when_app_spelled() {
    let p = policy(&[]);
    for slug in [
        "acme-writer",
        "acme-reader-1",
        "acme-old",
        "loom-fleet-dispatch-2",
    ] {
        assert!(p.trusts_json(&rest(&format!("{slug}[bot]"), "Bot", "NONE")), "{slug}");
        assert!(p.trusts_json(&graphql(&format!("app/{slug}"), "CONTRIBUTOR")), "{slug}");
        // The bare slug is a user login: squattable, never the App.
        assert!(!p.trusts_json(&rest(slug, "User", "NONE")), "bare {slug}");
        assert!(!p.trusts_json(&graphql(slug, "CONTRIBUTOR")), "bare gh-json {slug}");
    }
    // A `Bot` type or `__typename` marks an App even without the suffix.
    assert!(p.trusts_json(&json!({"author": {"login": "acme-reader-1", "__typename": "Bot"}})));
}

#[test]
fn another_fleets_apps_are_not_ours() {
    let p = policy(&[]);
    for foreign in [
        "other-fleet-dispatch[bot]",
        "app/other-fleet-dispatch",
        "acme-writer-2[bot]",
        "loom-fleet-dispatch-evil[bot]",
        "github-actions[bot]",
    ] {
        assert!(!p.trusts_json(&rest(foreign, "Bot", "NONE")), "{foreign}");
    }
}

#[test]
fn self_and_allowlist_match_only_the_same_account_kind() {
    let p = policy(&["robb-bot", "helper-app[bot]"]);
    assert!(p.trusts_json(&rest("Robb-Bot", "User", "NONE")));
    assert!(!p.trusts_json(&rest("robb-bot[bot]", "Bot", "NONE")));
    assert!(p.trusts_json(&rest("helper-app[bot]", "Bot", "NONE")));
    assert!(p.trusts_json(&graphql("app/helper-app", "NONE")));
    assert!(!p.trusts_json(&rest("helper-app", "User", "NONE")));
    // Self: the writer App, not a user who took its name.
    let no_fleet =
        TrustPolicy::new(FleetLogins::single("unrelated"), Some("omega[bot]".into()), vec![]);
    assert!(no_fleet.trusts_json(&rest("omega[bot]", "Bot", "NONE")));
    assert!(!no_fleet.trusts_json(&rest("omega", "User", "NONE")));
}

#[test]
fn a_missing_author_is_untrusted() {
    let p = policy(&["ghost"]);
    assert!(!p.trusts_json(&json!({"user": null, "author_association": "NONE"})));
    assert!(!p.trusts_json(&json!({"author": null, "authorAssociation": "NONE"})));
    assert!(
        !p.trusts_json(&json!({"body": "<!-- loom:verdict-sha sha=abc1234 verdict=approved -->"}))
    );
}

#[test]
fn listings_parse_single_concatenated_and_slurped_pages() {
    let p = policy(&[]);
    let one = rest("acme-writer[bot]", "Bot", "NONE");
    let two = rest("outsider", "User", "NONE");
    let single = serde_json::to_vec(&json!([one, two])).unwrap();
    let concat = format!("[{one}][{two}]");
    let slurp = serde_json::to_vec(&json!([[one], [two]])).unwrap();
    for bytes in [single, concat.into_bytes(), slurp] {
        assert_eq!(p.trusted_bodies(&bytes), Some(vec!["b".to_string()]));
    }
    // Judge #9566: nothing at all is a fetch that did not happen, not "no
    // comments" (an empty listing is `[]`).
    assert_eq!(p.trusted_bodies(b""), None);
    assert_eq!(p.trusted_bodies(b" \n"), None);
    assert_eq!(filter_document(&p, b""), None);
    assert_eq!(p.trusted_bodies(b"[]"), Some(vec![]));
    assert_eq!(p.trusted_bodies(b"{\"message\":\"Not Found\"}"), None);
    assert_eq!(p.trusted_bodies(b"not json"), None);
}

#[test]
fn documents_filter_arrays_and_object_comment_fields() {
    let p = policy(&[]);
    let doc = json!({
        "state": "OPEN",
        "comments": [graphql("rjwalters", "OWNER"), graphql("outsider", "NONE")],
        "reviews": [graphql("outsider", "CONTRIBUTOR")],
    });
    let out = filter_document(&p, &serde_json::to_vec(&doc).unwrap()).unwrap();
    assert_eq!(out["state"], "OPEN");
    assert_eq!(out["comments"].as_array().unwrap().len(), 1);
    assert_eq!(out["reviews"].as_array().unwrap().len(), 0);
    let arr = filter_document(&p, br#"[{"user":{"login":"x"},"author_association":"NONE"}]"#);
    assert_eq!(arr, Some(json!([])));
    assert_eq!(filter_document(&p, b"42"), None);
}

/// Judge #9566: an object that is not a comment document is rejected, never
/// echoed back with outsider text still in it.
#[test]
fn objects_that_are_not_comment_documents_are_rejected() {
    let p = policy(&[]);
    assert!(
        filter_document(&p, br#"{"message":"Bad credentials"}"#).is_none(),
        "forge error object"
    );
    assert!(
        filter_document(&p, br#"{"comments":{"nodes":[{"author":{"login":"x"},"body":"b"}]}}"#)
            .is_none(),
        "GraphQL connection shape"
    );
    assert!(
        filter_document(&p, br#"{"data":{"repository":{"pullRequest":{"comments":[]}}}}"#)
            .is_none(),
        "nested GraphQL document"
    );
    assert!(
        filter_document(&p, br#"{"comments":[],"reviews":{"nodes":[]}}"#).is_none(),
        "every present key must be an array"
    );
    assert!(
        filter_document(&p, br#"{"reviews":[]}"#).is_some(),
        "reviews alone is a document"
    );
}

#[test]
fn the_allowlist_comes_from_config_and_a_malformed_value_widens_nothing() {
    let cfg = json!({"forge": {"trustedCommenters": [" robb-bot ", "", "x[bot]"]}});
    assert_eq!(allowlist_from_config(&cfg), vec!["robb-bot", "x[bot]"]);
    assert!(allowlist_from_config(&json!({"forge": {"trustedCommenters": "robb-bot"}})).is_empty());
    assert!(allowlist_from_config(&json!({})).is_empty());
}

/// Structural guard (#9548 AC): a Rust site that reads a `loom:verdict-sha`
/// marker out of forge comments must take its comments through
/// [`TrustPolicy`]. Every file that mentions the marker is listed here with
/// what it does with it; a new file must be added deliberately, by someone
/// who has checked that its comments are filtered (or that it only writes
/// the marker).
#[test]
fn verdict_sha_readers_go_through_the_trust_filter() {
    const REVIEWED: &[(&str, &str)] = &[
        // Reads: `fetch_comment_bodies` returns trusted bodies only.
        ("claim_reconciliation.rs", "reader via forge::fetch_comment_bodies"),
        (
            "claim_reconciliation/review_conflict.rs",
            "reader via forge::fetch_comment_bodies",
        ),
        // Writes the marker; scans only already-filtered bodies.
        ("claim_reconciliation/verdict_stale_comment.rs", "writer + filtered bodies"),
        ("claim_reconciliation/verdict_invalidation.rs", "writer"),
        // #9416: owns the re-anchor comment BODY (so it names the marker) and
        // decides equivalence. It reads no comment at all — every answer comes
        // from git objects or the forge's own compare endpoint, and its module
        // doc states outright that a marker is never evidence (#9548). So there
        // is no unfiltered read here either.
        ("verdict_equivalence/mod.rs", "writer of the marker body; never reads a comment"),
        ("verdict_equivalence/tests.rs", "test"),
        // #9772: mentions the marker in its module doc only — forge_comment
        // WRITES comments (and appends the dashboard footer); it never reads
        // comment bodies at all, so there is no unfiltered read to guard.
        ("forge_comment.rs", "doc mention only; writer, never a reader"),
        // Scoring context, never a control decision (High follow-up, #9548).
        ("jev_merge_risk.rs", "shadow scoring input, not control"),
        // Tests and fixtures.
        ("claim_reconciliation/tests.rs", "test"),
        ("claim_reconciliation/review_conflict_tests.rs", "test"),
        ("claim_reconciliation/verdict_dedup_tests.rs", "test"),
        ("claim_reconciliation/auto_merge_disarm.rs", "test fixture"),
        ("claim_reconciliation/trusted_comments_tests.rs", "test"),
        // #9709: reads the RAW listing deliberately, but only to NAME the
        // author of a marker the policy dropped, in the stale-clear notice.
        // The decision is made upstream from trusted markers only and nothing
        // here feeds it — attribution, never evidence.
        ("verdict_stale_notice.rs", "attribution of dropped markers; never control"),
        ("verdict_stale_notice/tests.rs", "test"),
        ("comment_trust.rs", "module docs"),
        ("comment_trust/tests.rs", "this test"),
    ];
    let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut offenders = Vec::new();
    let mut stack = vec![src.clone()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).unwrap().flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            if path.extension().and_then(|e| e.to_str()) != Some("rs") {
                continue;
            }
            let rel = path
                .strip_prefix(&src)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            let text = std::fs::read_to_string(&path).unwrap_or_default();
            let mentions =
                text.contains("loom:verdict-sha") || text.contains("VERDICT_MARKER_PREFIX");
            if mentions && !REVIEWED.iter().any(|(f, _)| *f == rel) {
                offenders.push(rel);
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "new verdict-sha marker sites must read comments through comment_trust::TrustPolicy \
         and be listed here: {offenders:?}"
    );
    // The reviewed reader really filters: its fetch goes through the policy.
    let forge = std::fs::read_to_string(src.join("claim_reconciliation.rs")).unwrap();
    assert!(
        forge.contains("TrustPolicy::for_root(root).trusted_bodies("),
        "fetch_comment_bodies must return trusted bodies only"
    );
}
