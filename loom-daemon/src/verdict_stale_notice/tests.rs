//! #9709: the stale-clear notice names an untrusted newer verdict marker
//! instead of asserting a head move, and changes nothing else.

use super::*;
use crate::comment_trust::TRUSTED_ASSOCIATIONS;
use serde_json::json;

const OLD: &str = "b84a60027aa2d0888d1cd36299bb02c2709adcf1";
const HEAD: &str = "a0d8beef89b364c5ba35834eef0e70b8552beabf";
const OTHER: &str = "3333333333333333333333333333333333333333";

fn c(login: &str, assoc: &str, body: &str) -> Value {
    json!({"user": {"login": login}, "author_association": assoc, "body": body})
}

fn marker(sha: &str, verdict: &str) -> String {
    format!("Reviewed.\n\n<!-- loom:verdict-sha sha={sha} verdict={verdict} -->")
}

/// The fleet's reader is trusted through the allowlist here (the fleet roster
/// is host state); the human admin is not listed.
fn policy() -> TrustPolicy {
    TrustPolicy::new(
        crate::forge_identity::FleetLogins::default(),
        None,
        vec!["loom-fleet-reader-1[bot]".to_string()],
    )
}

/// The klayout-tools#2571 shape: a trusted fleet approval at an earlier head,
/// prose from both, then the admin's fresh approval at the current head, read
/// as `CONTRIBUTOR` because their org membership is private.
fn klayout_listing() -> Vec<Value> {
    vec![
        c("rjwalters", "CONTRIBUTOR", "Doctor: resolving conflicts."),
        c("loom-fleet-reader-1[bot]", "NONE", &marker(OLD, "approved")),
        c("loom-fleet-dispatch[bot]", "NONE", "lease record"),
        c("rjwalters", "CONTRIBUTOR", "Doctor: merge-conflict resolved."),
        c("rjwalters", "CONTRIBUTOR", &marker(HEAD, "approved")),
        c("rjwalters", "CONTRIBUTOR", "follow-up prose"),
    ]
}

#[test]
fn the_klayout_fixture_names_the_dropped_admin_approval() {
    let u = untrusted_newer_marker(&policy(), &klayout_listing(), VerdictKind::Approved).unwrap();
    assert_eq!(
        u,
        UntrustedVerdictMarker {
            login: "rjwalters".into(),
            association: "CONTRIBUTOR".into(),
            sha: HEAD.into(),
        }
    );
    let b = body("loom:pr", OLD, HEAD, "", Some(&u), DAEMON_SOURCE);
    assert!(b.starts_with(&format!("{VERDICT_STALE_MARKER_PREFIX}{OLD} to={HEAD} -->\n")));
    assert!(b.contains("`rjwalters`"), "{b}");
    assert!(b.contains("author_association=CONTRIBUTOR"), "{b}");
    assert!(b.contains("forge.trustedCommenters"), "{b}");
    assert!(b.contains("ignored as untrusted"), "{b}");
    assert!(
        b.contains("which IS the current head"),
        "the dropped marker is AT the head: {b}"
    );
    // The untrue claim is exactly what #9709 removes.
    assert!(!b.contains("head SHA moved"), "{b}");
    assert!(b.contains("*Automated by loom-daemon claim reconciliation (#5686, #9709)*"));
}

#[test]
fn a_genuine_head_move_keeps_the_existing_wording_byte_for_byte() {
    let listing = vec![c(
        "loom-fleet-reader-1[bot]",
        "NONE",
        &marker(OLD, "approved"),
    )];
    assert_eq!(untrusted_newer_marker(&policy(), &listing, VerdictKind::Approved), None);
    let expected = format!(
        "<!-- loom:verdict-stale from={OLD} to={HEAD} -->\n\
         **Stale review verdict cleared — head SHA moved**\n\n\
         This PR's `loom:pr` verdict was rendered against `{OLD}`, but the current head is \
         `{HEAD}`. A review verdict is a statement about a specific tree, so it does not survive \
         a rebase, a force-push, or new commits.\n\n\
         - Verdict cleared: `loom:pr` (recorded for `{OLD}`)\n\
         - Returned to the review queue: `loom:review-requested` (current head `{HEAD}`)\n\n\
         Judge will re-evaluate the tree that is actually here now. No judgment about the new \
         tree is implied either way — the old verdict simply no longer describes it.\n\n\
         ---\n\
         *Automated by loom-daemon claim reconciliation (#5686)*"
    );
    assert_eq!(body("loom:pr", OLD, HEAD, "", None, DAEMON_SOURCE), expected);
    // The guard's copy differs ONLY in the footer.
    assert_eq!(
        body("loom:pr", OLD, HEAD, "", None, GUARD_SOURCE),
        expected.replace(DAEMON_SOURCE, GUARD_SOURCE)
    );
}

#[test]
fn an_untrusted_marker_older_than_the_trusted_one_is_not_named() {
    let listing = vec![
        c("drive-by", "NONE", &marker(OTHER, "approved")),
        c("loom-fleet-reader-1[bot]", "NONE", &marker(OLD, "approved")),
    ];
    assert_eq!(untrusted_newer_marker(&policy(), &listing, VerdictKind::Approved), None);
}

#[test]
fn an_untrusted_marker_repeating_the_trusted_sha_is_not_named() {
    let listing = vec![
        c("loom-fleet-reader-1[bot]", "NONE", &marker(OLD, "approved")),
        c("drive-by", "NONE", &marker(OLD, "approved")),
    ];
    assert_eq!(untrusted_newer_marker(&policy(), &listing, VerdictKind::Approved), None);
}

#[test]
fn only_markers_for_the_held_verdict_kind_count() {
    let listing = vec![
        c("loom-fleet-reader-1[bot]", "NONE", &marker(OLD, "approved")),
        c("rjwalters", "CONTRIBUTOR", &marker(HEAD, "changes-requested")),
    ];
    assert_eq!(untrusted_newer_marker(&policy(), &listing, VerdictKind::Approved), None);
    let u = untrusted_newer_marker(&policy(), &listing, VerdictKind::ChangesRequested).unwrap();
    assert_eq!(u.login, "rjwalters");
}

#[test]
fn the_newest_of_several_untrusted_markers_is_named() {
    let mut listing = klayout_listing();
    listing.push(c("someone-else", "NONE", &marker(OTHER, "approved")));
    let u = untrusted_newer_marker(&policy(), &listing, VerdictKind::Approved).unwrap();
    assert_eq!((u.login.as_str(), u.sha.as_str()), ("someone-else", OTHER));
    let b = body("loom:pr", OLD, HEAD, "", Some(&u), GUARD_SOURCE);
    assert!(!b.contains("which IS the current head"), "OTHER is not the head: {b}");
    assert!(b.contains("*Automated by verdict-staleness-guard.sh (#5686, #9709)*"));
}

#[test]
fn listing_the_login_is_the_workaround_and_trust_is_otherwise_unchanged() {
    // The workaround the notice names really works: once allowlisted, the
    // admin's marker is trusted, so nothing is dropped.
    let listed = TrustPolicy::new(
        crate::forge_identity::FleetLogins::default(),
        None,
        vec!["loom-fleet-reader-1[bot]".into(), "rjwalters".into()],
    );
    assert_eq!(untrusted_newer_marker(&listed, &klayout_listing(), VerdictKind::Approved), None);
    // ...and #9709 widened nothing: CONTRIBUTOR is still not trusted.
    assert_eq!(TRUSTED_ASSOCIATIONS, &["OWNER", "MEMBER", "COLLABORATOR"]);
    assert!(!policy().trusts_json(&c("rjwalters", "CONTRIBUTOR", "x")));
}

#[test]
fn a_disarm_line_survives_in_both_wordings() {
    let u = untrusted_newer_marker(&policy(), &klayout_listing(), VerdictKind::Approved).unwrap();
    for b in [
        body("loom:pr", OLD, HEAD, "\n- Disarmed.", None, DAEMON_SOURCE),
        body("loom:pr", OLD, HEAD, "\n- Disarmed.", Some(&u), DAEMON_SOURCE),
    ] {
        assert!(b.contains("- Disarmed."), "{b}");
    }
}

#[test]
fn the_log_note_names_the_login_or_is_empty() {
    assert_eq!(log_note(None), "");
    let u = untrusted_newer_marker(&policy(), &klayout_listing(), VerdictKind::Approved).unwrap();
    let note = log_note(Some(&u));
    assert!(note.contains("`rjwalters`"), "{note}");
    assert!(note.contains("author_association=CONTRIBUTOR"), "{note}");
    assert!(note.contains("forge.trustedCommenters"), "{note}");
}

#[test]
fn a_deleted_author_is_named_ghost() {
    let listing = vec![
        c("loom-fleet-reader-1[bot]", "NONE", &marker(OLD, "approved")),
        json!({"user": null, "body": marker(HEAD, "approved")}),
    ];
    let u = untrusted_newer_marker(&policy(), &listing, VerdictKind::Approved).unwrap();
    assert_eq!((u.login.as_str(), u.association.as_str()), ("ghost", "unknown"));
}

#[test]
fn only_the_two_terminal_verdict_labels_name_a_kind() {
    assert_eq!(kind_for_label("loom:pr"), Some(VerdictKind::Approved));
    assert_eq!(kind_for_label("loom:changes-requested"), Some(VerdictKind::ChangesRequested));
    assert_eq!(kind_for_label("loom:review-requested"), None);
}
