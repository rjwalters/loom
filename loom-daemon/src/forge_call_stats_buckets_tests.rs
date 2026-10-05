#![allow(clippy::unwrap_used)]
//! W1: per-bucket rollups and sink-line compatibility in both directions.

use super::*;
use crate::forge_call_stats::{CallAttribution, Pool};
use serde::Deserialize;

/// A line exactly as a pre-W1 binary wrote it.
const PRE_W1: &str = r#"{"t":1900000000,"c":"claim.pr_view","p":"graphql","o":"ok","op":"unknown","pv":"github","og":"github.com","rp":"acme/widget","ir":"writer"}"#;

/// The pre-W1 `SinkLine` shape, as an older reader deserializes.
#[derive(Debug, Deserialize, PartialEq)]
struct OldSinkLine {
    t: i64,
    c: String,
    p: Pool,
    o: Outcome,
    #[serde(default)]
    rem: Option<u64>,
    #[serde(default)]
    usd: Option<u64>,
    #[serde(default)]
    rst: Option<i64>,
    #[serde(default)]
    op: Option<String>,
    #[serde(default)]
    pv: Option<String>,
    #[serde(default)]
    og: Option<String>,
    #[serde(default)]
    rp: Option<String>,
    #[serde(default)]
    ir: Option<String>,
}

fn w1_line(t: i64, caller: &str, o: Outcome, at: CallAttribution) -> String {
    serde_json::to_string(&SinkLine {
        t,
        c: caller.to_string(),
        p: Pool::Core,
        o,
        rem: Some(4000),
        usd: Some(1000),
        rst: Some(1_900_003_600),
        op: Some("unknown".into()),
        pv: Some("github".into()),
        og: Some("github.com".into()),
        rp: Some("acme/widget".into()),
        ir: Some("reader".into()),
        at,
    })
    .unwrap()
}

fn attributed(pg: Option<u32>) -> CallAttribution {
    CallAttribution {
        ro: Some("target".into()),
        ca: Some("app-42".into()),
        co: Some("acme".into()),
        tk: Some("reader".into()),
        rr: Some("core".into()),
        pg,
        pu: None,
        rd: Some(true),
    }
}

#[test]
fn a_pre_w1_line_parses_with_no_attribution() {
    let line: SinkLine = serde_json::from_str(PRE_W1).unwrap();
    assert_eq!(line.at, CallAttribution::default());
    assert_eq!(line.rp.as_deref(), Some("acme/widget"));
    let agg = aggregate_lines([PRE_W1].into_iter(), 0, GroupBy::Bucket);
    assert_eq!(agg.groups.len(), 1);
    assert_eq!(agg.groups[0].key, ["unknown", "-", "graphql", "-"]);
    assert_eq!((agg.groups[0].charged, agg.no_account), (1, 1));
}

#[test]
fn a_w1_line_parses_under_the_old_shape() {
    let raw = w1_line(1_900_000_000, "claim.pr_get", Outcome::Ok, attributed(Some(3)));
    for key in [
        "\"ro\"", "\"ca\"", "\"co\"", "\"tk\"", "\"rr\"", "\"pg\"", "\"rd\"",
    ] {
        assert!(raw.contains(key), "{key} in {raw}");
    }
    assert!(!raw.contains("\"pu\""), "absent fields are not written: {raw}");
    let old: OldSinkLine = serde_json::from_str(&raw).unwrap();
    assert_eq!(
        (old.c.as_str(), old.rem, old.rst),
        ("claim.pr_get", Some(4000), Some(1_900_003_600))
    );
    assert_eq!(old.ir.as_deref(), Some("reader"));
    let back: SinkLine = serde_json::from_str(&raw).unwrap();
    assert_eq!(back.at, attributed(Some(3)));
}

#[test]
fn charged_counts_ok_pages_and_never_a_304_or_a_limited_call() {
    let lines = [
        w1_line(1_900_000_000, "a", Outcome::Ok, attributed(Some(3))),
        w1_line(1_900_000_001, "a", Outcome::Ok, attributed(None)),
        w1_line(1_900_000_002, "a", Outcome::NotModified, attributed(None)),
        w1_line(1_900_000_003, "a", Outcome::RateLimited, attributed(None)),
        w1_line(1_900_000_004, "a", Outcome::Error, attributed(None)),
        w1_line(1_800_000_000, "a", Outcome::Ok, attributed(None)),
    ];
    let agg = aggregate_lines(lines.iter().map(String::as_str), 1_900_000_000, GroupBy::Bucket);
    assert_eq!(agg.lines, 5, "the old line is outside the window");
    let g = &agg.groups[0];
    assert_eq!(g.key, ["app-42", "acme", "core", "1900003600"]);
    assert_eq!((g.rows, g.charged, g.not_modified, g.rate_limited, g.error), (5, 4, 1, 1, 1));
    assert_eq!(agg.cwd_route_disagree, 5);

    let by_role = aggregate_lines(lines.iter().map(String::as_str), 0, GroupBy::Role);
    assert_eq!(by_role.groups[0].key, ["reader"]);
    let by_repo = aggregate_lines(lines.iter().map(String::as_str), 0, GroupBy::Repo);
    assert_eq!(by_repo.groups[0].key, ["acme/widget"]);
}

#[test]
fn group_by_parses_only_its_four_values() {
    for (v, want) in [
        ("bucket", GroupBy::Bucket),
        ("caller", GroupBy::Caller),
        ("role", GroupBy::Role),
        ("repo", GroupBy::Repo),
    ] {
        assert_eq!(GroupBy::parse(v), Ok(want));
    }
    assert!(GroupBy::parse("owner").is_err());
}
