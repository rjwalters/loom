//! Unit tests for the #9548 High-slice record helpers.

use super::*;
use crate::forge_identity::FleetLogins;
use serde_json::json;

fn policy() -> TrustPolicy {
    TrustPolicy::new(FleetLogins::of(&crate::forge_identity::Roster::default()), None, Vec::new())
}

fn rec(login: &str, kind: &str, assoc: &str) -> Value {
    json!({"user": {"login": login, "type": kind}, "author_association": assoc, "body": "b"})
}

#[test]
fn records_parse_ndjson_arrays_and_pages_but_not_scalars() {
    let one = rec("a", "User", "OWNER");
    let ndjson = format!("{one}\n{one}\n");
    assert_eq!(parse_records(ndjson.as_bytes()).unwrap().len(), 2);
    assert_eq!(
        parse_records(format!("[{one}][{one}]").as_bytes())
            .unwrap()
            .len(),
        2
    );
    assert_eq!(parse_records(b"").unwrap().len(), 0, "a jq filter that matched nothing");
    assert!(parse_records(b"\"2026-01-01T00:00:00Z\"").is_none());
    assert!(parse_records(b"not json").is_none());
}

#[test]
fn only_trusted_records_and_ndjson_lines_survive() {
    let p = policy();
    let lines = [
        rec("loom-fleet-dispatch[bot]", "Bot", "NONE"),
        rec("rjwalters", "User", "OWNER"),
        rec("drive-by", "User", "NONE"),
        rec("loom-fleet-dispatch", "User", "CONTRIBUTOR"),
        rec("other-fleet[bot]", "Bot", "NONE"),
        json!({"body": "no author at all"}),
    ]
    .iter()
    .map(Value::to_string)
    .collect::<Vec<_>>()
    .join("\n");
    assert_eq!(p.trusted_records(lines.as_bytes()).unwrap().len(), 2);
    let kept = String::from_utf8(p.trusted_ndjson(lines.as_bytes())).unwrap();
    assert_eq!(kept.lines().count(), 2, "{kept}");
    assert!(kept.contains("loom-fleet-dispatch[bot]") && kept.contains("rjwalters"));
}

#[test]
fn anchored_markers_must_start_the_comment() {
    assert!(anchored("  <!-- loom:lease host=a sweep=b -->\nprose", "<!-- loom:lease host="));
    assert!(!anchored("see <!-- loom:lease host=a sweep=b -->", "<!-- loom:lease host="));
}

#[test]
fn max_timestamp_picks_the_latest_parseable_value() {
    let rs = [
        json!({"t": "2026-01-01T00:00:00Z"}),
        json!({"t": "garbage"}),
        json!({"t": "2026-03-01T00:00:00Z"}),
    ];
    assert_eq!(max_timestamp(&rs, "t").unwrap().to_rfc3339(), "2026-03-01T00:00:00+00:00");
    assert!(max_timestamp(&[], "t").is_none());
}

fn graph(nodes: &[Value]) -> String {
    json!({"data": {"repository": {"issue": {"closedByPullRequestsReferences": {"nodes": nodes}}}}})
        .to_string()
}

fn node(n: u32, cross: bool, login: &str, typename: &str, assoc: &str) -> Value {
    json!({"number": n, "state": "OPEN", "isCrossRepository": cross,
           "authorAssociation": assoc, "author": {"login": login, "__typename": typename}})
}

#[test]
fn h14_an_untrusted_fork_pr_is_not_a_linked_pr() {
    let p = policy();
    let out = p.drop_untrusted_fork_prs(&graph(&[
        node(1, true, "drive-by", "User", "NONE"),
        node(2, true, "loom-fleet-dispatch", "User", "CONTRIBUTOR"),
        node(3, true, "maintainer", "User", "COLLABORATOR"),
        node(4, false, "anyone", "User", "NONE"),
        node(5, true, "loom-fleet-dispatch", "Bot", "NONE"),
        json!({"number": 6, "state": "OPEN"}),
    ]));
    let v: Value = serde_json::from_str(&out).unwrap();
    let kept: Vec<u64> = v
        .pointer("/data/repository/issue/closedByPullRequestsReferences/nodes")
        .unwrap()
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|n| n["number"].as_u64())
        .collect();
    // Fork PRs by an outsider or a bare-slug squatter go; an insider's fork PR,
    // any same-repo branch, the fleet App's PR and a node with no author
    // fields (fail toward "a PR exists") stay.
    assert_eq!(kept, vec![3, 4, 5, 6]);
    assert_eq!(p.drop_untrusted_fork_prs("not json"), "not json");
}

#[test]
fn h14_the_timeline_leg_drops_known_untrusted_authors_only() {
    let p = policy();
    let lines = [
        json!({"number": 1, "body": "Closes #9", "user": {"login": "drive-by", "type": "User"}, "author_association": "NONE"}),
        json!({"number": 2, "body": "Part of #9", "user": {"login": "loom-fleet-dispatch[bot]", "type": "Bot"}, "author_association": "NONE"}),
        json!({"number": 3, "body": "Closes #9"}),
    ]
    .iter()
    .map(Value::to_string)
    .collect::<Vec<_>>()
    .join("\n");
    let kept = p.drop_untrusted_timeline_prs(&lines);
    assert!(!kept.contains("drive-by"));
    assert_eq!(kept.lines().count(), 2);
}

#[test]
fn h15_a_body_counts_only_from_a_trusted_author() {
    let dir = tempfile::tempdir().unwrap();
    let body = |login: &str, assoc: &str| {
        json!({"body": "<!-- loom:capability=root -->", "user": {"login": login, "type": "User"}, "author_association": assoc})
            .to_string()
    };
    assert!(trusted_body(dir.path(), body("drive-by", "NONE").as_bytes()).is_none());
    assert!(trusted_body(dir.path(), body("rjwalters", "OWNER").as_bytes()).is_some());
    assert!(trusted_body(dir.path(), b"not json").is_none());
}

#[test]
fn the_fleet_author_fixture_is_trusted() {
    let line = with_fleet_author(r#"{"id":1,"body":"x"}"#);
    let v: Value = serde_json::from_str(&line).unwrap();
    assert!(policy().trusts_json(&v), "{line}");
    assert_eq!(with_fleet_author("not json"), "not json");
}
