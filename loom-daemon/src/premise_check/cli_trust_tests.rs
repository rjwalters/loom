//! #9548 (H12), Judge #9593: an outsider's `loom:premise-check` record must be
//! invisible to the gate, the same record from a trusted author must count,
//! and an author the forge did not report is untrusted.

use super::{parse_graphql_comments, strip_markers, trusted_inputs};
use crate::comment_trust::TrustPolicy;
use crate::forge_identity::FleetLogins;
use crate::premise_check::record::last_chunk_with_marker;
use serde_json::{json, Value};

const MARKER: &str =
    "<!-- loom:premise-check exists=no deliberate=no reversal=no verdict=clear -->";
/// The split-line opener `marker_span` also accepts.
const SPLIT_MARKER: &str =
    "<!--\nloom:premise-check exists=no deliberate=no reversal=no verdict=clear -->";

fn policy() -> TrustPolicy {
    TrustPolicy::new(FleetLogins::of(&crate::forge_identity::Roster::default()), None, Vec::new())
}

fn outsider() -> Value {
    json!({"user": {"login": "drive-by", "type": "User"}, "author_association": "CONTRIBUTOR"})
}

fn owner() -> Value {
    json!({"user": {"login": "rjwalters", "type": "User"}, "author_association": "OWNER"})
}

fn fleet_app() -> Value {
    json!({"user": {"login": "loom-fleet-dispatch[bot]", "type": "Bot"}, "author_association": "NONE"})
}

/// A one-comment REST listing by `author` carrying `body`.
fn listing(author: &Value, body: &str) -> Vec<u8> {
    let mut c = author.clone();
    c["body"] = json!(body);
    serde_json::to_vec(&json!([c])).unwrap()
}

/// Does the gate find any record in what `trusted_inputs` hands it?
fn finds_record(listing: &[u8], issue_author: &Value, body: &str) -> bool {
    let inputs =
        trusted_inputs(&policy(), "t".into(), body.into(), Vec::new(), listing, issue_author)
            .expect("a parseable listing is readable");
    let mut chunks = vec![inputs.body.as_str()];
    chunks.extend(inputs.comments.iter().map(String::as_str));
    last_chunk_with_marker(&chunks).is_some()
}

#[test]
fn an_untrusted_comment_record_is_ignored() {
    for marker in [MARKER, SPLIT_MARKER] {
        let l = listing(&outsider(), marker);
        assert!(!finds_record(&l, &owner(), "prose"), "{marker}");
    }
}

#[test]
fn a_trusted_comment_record_is_honoured() {
    for author in [owner(), fleet_app()] {
        for marker in [MARKER, SPLIT_MARKER] {
            let l = listing(&author, marker);
            assert!(finds_record(&l, &owner(), "prose"), "{author} {marker}");
        }
    }
}

#[test]
fn a_comment_with_no_author_is_untrusted() {
    for author in [
        json!({}),
        json!({"user": null}),
        json!({"user": {"login": "x"}}),
    ] {
        let l = listing(&author, MARKER);
        assert!(!finds_record(&l, &owner(), "prose"), "{author}");
    }
}

#[test]
fn an_untrusted_body_record_is_stripped_but_its_prose_kept() {
    let body = format!("The flag was removed.\n{MARKER}\nMore prose.");
    let inputs =
        trusted_inputs(&policy(), "t".into(), body.clone(), vec![], b"[]", &outsider()).unwrap();
    assert!(inputs.body.contains("The flag was removed.") && inputs.body.contains("More prose."));
    assert!(!finds_record(b"[]", &outsider(), &body));
    assert!(!finds_record(b"[]", &json!({}), &body), "a body with no author is untrusted");
    let split = format!("prose\n{SPLIT_MARKER}\n");
    assert!(!finds_record(b"[]", &outsider(), &split), "split-line opener");
}

#[test]
fn a_trusted_body_record_is_honoured() {
    let body = format!("prose\n{MARKER}\n");
    assert!(finds_record(b"[]", &owner(), &body));
    assert!(finds_record(b"[]", &fleet_app(), &body));
}

#[test]
fn an_empty_or_unparseable_listing_is_unreadable_not_empty() {
    for bad in [
        &b""[..],
        b"  \n",
        b"{\"message\":\"Not Found\"}",
        b"not json",
    ] {
        let got = trusted_inputs(&policy(), "t".into(), "b".into(), vec![], bad, &owner());
        assert!(got.is_none(), "{:?}", String::from_utf8_lossy(bad));
    }
}

#[test]
fn strip_markers_removes_every_marker_line_and_nothing_else() {
    let body = format!("a\n{MARKER}\nb\n  <!-- loom:premise-check exists=yes -->  \nc");
    assert_eq!(strip_markers(&body), "a\nb\nc");
    // The split-line form loses its marker line; the orphaned `<!--` opener
    // left behind is not a record.
    let out = strip_markers(&format!("a\n{SPLIT_MARKER}\nb"));
    assert!(!out.contains("loom:premise-check"));
    assert!(last_chunk_with_marker(&[out.as_str()]).is_none());
    assert_eq!(strip_markers("no markers\nhere"), "no markers\nhere");
}

// ---------------------------------------------------------------------------
// #10025: the GraphQL fallback for the comment/author reads must feed the
// same trust filter, with GraphQL's author spelling (`__typename: "Bot"`,
// bare login) judged exactly as the REST listing's `x[bot]`.
// ---------------------------------------------------------------------------

/// One `gh api graphql --paginate` page for an issue by `issue_author`.
fn graphql_page(issue_author: &Value, comments: &[(Value, &str)]) -> String {
    let nodes: Vec<Value> = comments
        .iter()
        .map(|(author, body)| {
            let mut n = author.clone();
            n["body"] = json!(body);
            n
        })
        .collect();
    let mut issue = issue_author.clone();
    issue["comments"] =
        json!({"pageInfo": {"hasNextPage": false, "endCursor": null}, "nodes": nodes});
    json!({"data": {"repository": {"issue": issue}}}).to_string()
}

fn gql_fleet_app() -> Value {
    json!({"author": {"login": "loom-fleet-dispatch", "__typename": "Bot"}, "authorAssociation": "NONE"})
}

fn gql_outsider() -> Value {
    json!({"author": {"login": "drive-by", "__typename": "User"}, "authorAssociation": "CONTRIBUTOR"})
}

fn gql_owner() -> Value {
    json!({"author": {"login": "rjwalters", "__typename": "User"}, "authorAssociation": "OWNER"})
}

fn gql_finds_record(stdout: &str, body: &str) -> bool {
    let (listing, object) = parse_graphql_comments(stdout.as_bytes()).expect("parses");
    finds_record(&listing, &object, body)
}

#[test]
fn graphql_fallback_honours_trusted_and_ignores_untrusted_records() {
    // The fleet App, as GraphQL spells it, and an insider: both count.
    for author in [gql_fleet_app(), gql_owner()] {
        let page = graphql_page(&gql_owner(), &[(author.clone(), MARKER)]);
        assert!(gql_finds_record(&page, "prose"), "{author}");
    }
    // An outsider's well-formed record is invisible.
    let page = graphql_page(&gql_owner(), &[(gql_outsider(), MARKER)]);
    assert!(!gql_finds_record(&page, "prose"));
    // A user who registered the App's bare slug is not the App.
    let impostor = json!({"author": {"login": "loom-fleet-dispatch", "__typename": "User"}, "authorAssociation": "NONE"});
    let page = graphql_page(&gql_owner(), &[(impostor, MARKER)]);
    assert!(!gql_finds_record(&page, "prose"));
    // A deleted (null) author is untrusted.
    let ghost = json!({"author": null, "authorAssociation": "NONE"});
    let page = graphql_page(&gql_owner(), &[(ghost, MARKER)]);
    assert!(!gql_finds_record(&page, "prose"));
}

#[test]
fn graphql_fallback_judges_the_body_author() {
    let body = format!("prose\n{MARKER}\n");
    assert!(gql_finds_record(&graphql_page(&gql_owner(), &[]), &body));
    assert!(gql_finds_record(&graphql_page(&gql_fleet_app(), &[]), &body));
    assert!(!gql_finds_record(&graphql_page(&gql_outsider(), &[]), &body));
}

#[test]
fn graphql_fallback_concatenates_pages_and_rejects_errors() {
    let first = graphql_page(&gql_owner(), &[(gql_outsider(), "old")]);
    let second = graphql_page(&gql_owner(), &[(gql_fleet_app(), MARKER)]);
    let (listing, _) = parse_graphql_comments(format!("{first}{second}").as_bytes()).unwrap();
    let all: Vec<Value> = serde_json::from_slice(&listing).unwrap();
    assert_eq!(all.len(), 2, "both pages' comments survive");
    assert!(gql_finds_record(&format!("{first}\n{second}"), "prose"));

    // No comments is a readable, empty listing — not "could not read".
    let (listing, _) = parse_graphql_comments(graphql_page(&gql_owner(), &[]).as_bytes()).unwrap();
    assert_eq!(listing, b"[]");

    for bad in [
        r#"{"errors":[{"message":"API rate limit already exceeded"}]}"#,
        r#"{"data":{"repository":{"issue":null}}}"#,
        "",
        "not json",
    ] {
        assert!(parse_graphql_comments(bad.as_bytes()).is_none(), "{bad}");
    }
}
