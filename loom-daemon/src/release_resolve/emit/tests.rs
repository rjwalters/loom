//! Tests for the `--resolve-json` object (epic #7810, PR 5).

use super::*;

#[test]
fn every_field_is_present_on_both_outcomes() {
    // "No field is ever omitted, so a consumer can rely on the shape" — the
    // shell's own words. A missing key is a different failure for a consumer
    // than a null one.
    for r in [
        Resolution::Unresolved("nothing here".into()),
        Resolution::Resolved(Box::new(Resolved {
            tag: "v0.19.24".into(),
            version: "0.19.24".into(),
            ..Default::default()
        })),
    ] {
        let v = to_json(&r);
        let obj = v.as_object().expect("an object");
        assert_eq!(obj.len(), FIELDS.len(), "{v}");
        for f in FIELDS {
            assert!(obj.contains_key(f), "missing {f} in {v}");
        }
    }
}

#[test]
fn an_unresolved_outcome_reports_ok_false_and_its_reason() {
    let v = to_json(&Resolution::Unresolved("no Releases yet".into()));
    assert_eq!(v["ok"], serde_json::json!(false));
    assert_eq!(v["reason"], serde_json::json!("no Releases yet"));
    assert_eq!(v["version"], serde_json::Value::Null);
}

#[test]
fn a_resolved_outcome_reports_ok_true_and_a_null_reason() {
    let v = to_json(&Resolution::Resolved(Box::new(Resolved {
        tag: "v0.19.24".into(),
        version: "0.19.24".into(),
        repo: "rjwalters/loom".into(),
        target: "aarch64-apple-darwin".into(),
        ..Default::default()
    })));
    assert_eq!(v["ok"], serde_json::json!(true));
    assert_eq!(v["reason"], serde_json::Value::Null);
    assert_eq!(v["tag"], serde_json::json!("v0.19.24"));
    assert_eq!(v["version"], serde_json::json!("0.19.24"));
}

#[test]
fn an_undetermined_optional_field_is_null_not_an_empty_string() {
    // The distinction auto_update reasons about: a null published_at means an
    // older `gh` did not report it; an empty string would look like a value.
    let v = to_json(&Resolution::Resolved(Box::new(Resolved {
        tag: "v1".into(),
        version: "1.0.0".into(),
        published_at: None,
        asset_sha256: None,
        ..Default::default()
    })));
    assert_eq!(v["published_at"], serde_json::Value::Null);
    assert_eq!(v["asset_sha256"], serde_json::Value::Null);
}

#[test]
fn a_determined_optional_field_is_its_value() {
    let v = to_json(&Resolution::Resolved(Box::new(Resolved {
        tag: "v1".into(),
        version: "1.0.0".into(),
        published_at: Some("2026-09-16T00:00:00Z".into()),
        asset_sha256: Some("deadbeef".into()),
        ..Default::default()
    })));
    assert_eq!(v["published_at"], serde_json::json!("2026-09-16T00:00:00Z"));
    assert_eq!(v["asset_sha256"], serde_json::json!("deadbeef"));
}

#[test]
fn the_literal_unknown_commit_survives_as_a_value() {
    // Distinct from null: the binary answered but named no commit.
    // `auto_update::parse_resolve_json` special-cases exactly this string.
    let v = to_json(&Resolution::Resolved(Box::new(Resolved {
        tag: "v1".into(),
        version: "1.0.0".into(),
        installed_commit: Some("unknown".into()),
        ..Default::default()
    })));
    assert_eq!(v["installed_commit"], serde_json::json!("unknown"));
}

#[test]
fn the_installed_binary_path_renders_as_a_string() {
    let v = to_json(&Resolution::Resolved(Box::new(Resolved {
        tag: "v1".into(),
        version: "1.0.0".into(),
        installed_bin: Some(std::path::PathBuf::from("/usr/local/bin/loom-daemon")),
        ..Default::default()
    })));
    assert_eq!(v["installed_bin"], serde_json::json!("/usr/local/bin/loom-daemon"));
}

#[test]
fn the_object_is_one_line_of_valid_json() {
    // stdout is reserved for exactly one JSON object; a pretty-printed or
    // multi-object stream would break the consumer that reads one line.
    let s = to_json(&Resolution::Unresolved("x".into())).to_string();
    assert!(!s.contains('\n'), "{s}");
    serde_json::from_str::<serde_json::Value>(&s).expect("valid JSON");
}
