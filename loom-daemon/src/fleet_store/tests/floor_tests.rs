//! Tests for the `loom_min_version` floor reader (#10711).

use std::collections::BTreeMap;

use serde_json::json;

use super::*;
use crate::fleet_store::test_support::snapshot_of;

const ROSTER_BODY: &str = "root: /srv/src\nrepos:\n  - name: app\n    fleet: true\n";

fn store(fleet_json: Option<&str>, repos_yml: Option<&str>) -> Snapshot {
    let mut files = BTreeMap::new();
    if let Some(f) = fleet_json {
        files.insert("fleet.json".to_string(), f.to_string());
    }
    if let Some(r) = repos_yml {
        files.insert("repos.yml".to_string(), r.to_string());
    }
    snapshot_of(&files)
}

fn roster_with(line: &str) -> String {
    format!("{line}\n{ROSTER_BODY}")
}

#[test]
fn absent_everywhere_is_absent() {
    assert_eq!(read(&store(None, Some(ROSTER_BODY))), FloorRead::Absent);
    assert_eq!(read(&store(None, None)), FloorRead::Absent);
    assert_eq!(read(&store(Some("{}"), Some(ROSTER_BODY))), FloorRead::Absent);
}

#[test]
fn quoted_floor_in_repos_yml_is_read() {
    let roster = roster_with("loom_min_version: \"0.19.830\"");
    assert_eq!(
        read(&store(None, Some(&roster))),
        FloorRead::Valid {
            version: "0.19.830".to_string(),
            source: "repos.yml"
        }
    );
}

#[test]
fn unquoted_and_single_quoted_floors_are_read_too() {
    for line in ["loom_min_version: 0.19.830", "loom_min_version: '0.19.830'"] {
        let roster = roster_with(line);
        assert_eq!(
            read(&store(None, Some(&roster))),
            FloorRead::Valid {
                version: "0.19.830".to_string(),
                source: "repos.yml"
            },
            "{line}"
        );
    }
}

#[test]
fn the_roster_itself_still_parses_with_the_key_present() {
    // The key is additive: `roster::parse` ignores it, unchanged.
    let roster = roster_with("loom_min_version: \"0.19.830\"");
    let parsed = crate::fleet_store::roster::parse(&roster, std::path::Path::new("/home/u"))
        .expect("roster still parses");
    assert_eq!(parsed.records.len(), 1);
}

#[test]
fn fleet_json_floor_is_read() {
    assert_eq!(
        read(&store(Some(r#"{"loom_min_version":"1.2.3"}"#), None)),
        FloorRead::Valid {
            version: "1.2.3".to_string(),
            source: "fleet.json"
        }
    );
}

#[test]
fn fleet_json_wins_when_both_sources_carry_the_key() {
    let roster = roster_with("loom_min_version: \"0.19.800\"");
    assert_eq!(
        read(&store(Some(r#"{"loom_min_version":"0.19.830"}"#), Some(&roster))),
        FloorRead::Valid {
            version: "0.19.830".to_string(),
            source: "fleet.json"
        }
    );
}

#[test]
fn a_fleet_json_without_the_key_falls_back_to_repos_yml() {
    let roster = roster_with("loom_min_version: \"0.19.800\"");
    assert_eq!(
        read(&store(Some(r#"{"other":1}"#), Some(&roster))),
        FloorRead::Valid {
            version: "0.19.800".to_string(),
            source: "repos.yml"
        }
    );
}

#[test]
fn malformed_values_are_malformed_never_absent() {
    for line in [
        "loom_min_version: 1.2",
        "loom_min_version: 12",
        "loom_min_version: true",
        "loom_min_version:",
        "loom_min_version: \"v0.19.830\"",
        "loom_min_version: \"0.19\"",
        "loom_min_version: \"0.19.830.1\"",
        "loom_min_version: \"0.19.830-rc1\"",
        "loom_min_version: \"\"",
        "loom_min_version: \"a.b.c\"",
    ] {
        let roster = roster_with(line);
        assert!(
            matches!(
                read(&store(None, Some(&roster))),
                FloorRead::Malformed {
                    source: "repos.yml",
                    ..
                }
            ),
            "{line}"
        );
    }
}

#[test]
fn malformed_fleet_json_is_malformed_and_does_not_fall_back() {
    let roster = roster_with("loom_min_version: \"0.19.800\"");
    for body in ["not json", "[1,2]", r#"{"loom_min_version":19}"#] {
        assert!(
            matches!(
                read(&store(Some(body), Some(&roster))),
                FloorRead::Malformed {
                    source: "fleet.json",
                    ..
                }
            ),
            "{body}"
        );
    }
}

#[test]
fn an_unparseable_repos_yml_is_malformed() {
    assert!(matches!(
        read(&store(None, Some("- just\n- a list\n"))),
        FloorRead::Malformed {
            source: "repos.yml",
            ..
        }
    ));
}

#[test]
fn validate_normalises_and_explains() {
    assert_eq!(validate(&json!(" 0.19.830 ")), Ok("0.19.830".to_string()));
    assert_eq!(validate(&json!("1.02.3")), Ok("1.2.3".to_string()));
    let err = validate(&json!(1.2)).unwrap_err();
    assert!(err.contains("quoted"), "{err}");
    let err = validate(&json!("0.19")).unwrap_err();
    assert!(err.contains("X.Y.Z"), "{err}");
}

#[test]
fn parse_triple_accepts_only_three_integers() {
    assert_eq!(parse_triple("0.19.830"), Some((0, 19, 830)));
    for bad in [
        "", "1", "1.2", "1.2.3.4", "1..3", "+1.2.3", "1.2.3 ", "v1.2.3",
    ] {
        assert_eq!(parse_triple(bad), None, "{bad}");
    }
}
