//! #9548 (H12), Judge #9593: an outsider's `loom:premise-check` record must be
//! invisible to the gate, the same record from a trusted author must count,
//! and an author the forge did not report is untrusted.

use super::{strip_markers, trusted_inputs};
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
