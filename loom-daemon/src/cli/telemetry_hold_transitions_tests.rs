//! Unit tests for `telemetry-hold-transitions`: bound construction, row
//! parsing and aggregation — no live telemetry-reader (Issue #14094).
#![allow(clippy::unwrap_used)]

use chrono::{TimeZone, Utc};

use super::*;
use crate::telemetry::kinds::hold_transition::reason;

fn record(repo: &str, reason_code: &str, confidence_pct: Option<u8>) -> HoldTransitionRow {
    HoldTransitionRow {
        repo: repo.to_string(),
        reason_code: reason_code.to_string(),
        confidence_pct,
    }
}

const FIRST_ROW: &str =
    r#"{"repo":"o/one","reason_code":"merge-risk-self-release","confidence_pct":90}"#;
const SECOND_ROW: &str =
    r#"{"repo":"o/two","reason_code":"merge-risk-pass","confidence_pct":null}"#;

#[test]
fn since_and_until_are_required_and_flags_parse() {
    assert!(crate::cli::whole_cli_parse::try_parse_cli(&[
        "loom-daemon",
        "telemetry-hold-transitions",
    ])
    .is_err());
    assert!(crate::cli::whole_cli_parse::try_parse_cli(&[
        "loom-daemon",
        "telemetry-hold-transitions",
        "--since",
        "2026-11-03",
    ])
    .is_err());
    let cli = crate::cli::whole_cli_parse::try_parse_cli(&[
        "loom-daemon",
        "telemetry-hold-transitions",
        "--since",
        "2026-11-03",
        "--until",
        "2026-11-18",
        "--repo",
        "o/one",
        "--repo",
        "o/two",
        "--reason-code",
        "merge-risk-self-release",
        "--max-rows",
        "42",
        "--json",
    ])
    .unwrap();
    let Some(crate::cli::root::Commands::Telemetry(
        crate::cli::telemetry::TelemetryCommand::TelemetryHoldTransitions(args),
    )) = cli.command
    else {
        panic!("wrong subcommand");
    };
    assert_eq!(args.since, "2026-11-03");
    assert_eq!(args.until, "2026-11-18");
    assert_eq!(args.repos, vec!["o/one", "o/two"]);
    assert_eq!(args.reason_code.as_deref(), Some("merge-risk-self-release"));
    assert_eq!(args.max_rows, 42);
    assert!(args.json);
}

#[test]
fn bounds_accept_rfc3339_and_bare_dates() {
    let rfc = parse_bound("2026-11-03T09:30:00Z", false).unwrap();
    assert_eq!(rfc, Utc.with_ymd_and_hms(2026, 11, 3, 9, 30, 0).unwrap());
    let day_since = parse_bound("2026-11-03", false).unwrap();
    assert_eq!(day_since, Utc.with_ymd_and_hms(2026, 11, 3, 0, 0, 0).unwrap());
    // A date-only until means *through* that UTC day.
    let day_until = parse_bound("2026-11-18", true).unwrap();
    assert_eq!(day_until, Utc.with_ymd_and_hms(2026, 11, 19, 0, 0, 0).unwrap());
    assert!(parse_bound("not-a-date", false).is_err());
}

#[test]
fn run_refuses_an_inverted_or_empty_window_before_any_io() {
    let args = HoldTransitionsArgs {
        since: "2026-11-18".to_string(),
        until: "2026-11-03".to_string(),
        repos: vec![],
        reason_code: None,
        max_rows: DEFAULT_MAX_ROWS,
        json: true,
        reader_url: None,
        timeout: 30,
    };
    let err = format!("{}", args.run().unwrap_err());
    assert!(err.contains("--until must be after --since"), "{err}");
}

#[test]
fn run_refuses_bad_row_caps_and_repo_shapes_before_any_io() {
    let base = |max_rows: usize, repos: Vec<String>| HoldTransitionsArgs {
        since: "2026-11-03".to_string(),
        until: "2026-11-18".to_string(),
        repos,
        reason_code: None,
        max_rows,
        json: true,
        reader_url: None,
        timeout: 30,
    };
    let zero = format!("{}", base(0, vec![]).run().unwrap_err());
    assert!(zero.contains("--max-rows"), "{zero}");
    let huge = format!("{}", base(MAX_MAX_ROWS + 1, vec![]).run().unwrap_err());
    assert!(huge.contains("--max-rows"), "{huge}");
    let repo = format!("{}", base(10, vec!["just-a-name".to_string()]).run().unwrap_err());
    assert!(repo.contains("owner/repo"), "{repo}");
}

#[test]
fn the_query_is_one_windowed_statement_and_user_text_is_bound() {
    let (sql, params) = build_query(
        1_000,
        2_000,
        &["o/one".to_string(), "o/two".to_string()],
        Some("merge-risk-self-release"),
        100,
    );
    assert!(sql.contains("FROM signoz_logs.distributed_logs_v2 "), "{sql}");
    assert!(sql.starts_with("SELECT attributes_string['loom.repo'] AS repo,"), "{sql}");
    assert!(
        sql.contains("attributes_string['loom.hold.reason_code'] AS reason_code"),
        "{sql}"
    );
    assert!(
        sql.contains("mapContains(attributes_number, 'loom.hold.confidence_pct')"),
        "{sql}"
    );
    assert!(sql.contains("attributes_string['loom.kind'] = 'hold.transition'"), "{sql}");
    // The store's timestamp is UInt64 nanoseconds: the generated bounds are
    // compared against the raw column, no DateTime64 conversion either way.
    assert!(sql.contains("timestamp >= 1000"), "{sql}");
    assert!(sql.contains("timestamp < 2000"), "{sql}");
    assert!(
        sql.contains("(attributes_string['loom.repo'] = {repo0:String} OR attributes_string['loom.repo'] = {repo1:String})"),
        "{sql}"
    );
    assert!(
        sql.contains("attributes_string['loom.hold.reason_code'] = {reason_code:String}"),
        "{sql}"
    );
    assert!(sql.ends_with(" ORDER BY timestamp ASC LIMIT 101 FORMAT JSONEachRow"), "{sql}");
    assert_eq!(
        params,
        vec![
            ("repo0".to_string(), "o/one".to_string()),
            ("repo1".to_string(), "o/two".to_string()),
            ("reason_code".to_string(), "merge-risk-self-release".to_string()),
        ]
    );

    // No filters: no bound params, no filter clauses, cap still max+1.
    let (sql, params) = build_query(1, 2, &[], None, 5);
    assert!(!sql.contains("{repo0:String}"), "{sql}");
    assert!(!sql.contains("{reason_code:String}"), "{sql}");
    assert!(sql.ends_with(" ORDER BY timestamp ASC LIMIT 6 FORMAT JSONEachRow"), "{sql}");
    assert!(params.is_empty());
}

#[test]
fn observed_projected_reader_line_decodes() {
    // Sanitized projection from the attended reader, with its exact JSONEachRow framing (#14569).
    let body =
        r#"{"repo":"2amlogic/2am","reason_code":"merge-risk-self-release","confidence_pct":93}"#;
    let rows = collect_rows(body, 10).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].repo, "2amlogic/2am");
    assert_eq!(rows[0].reason_code, reason::MERGE_RISK_SELF_RELEASE);
    assert_eq!(rows[0].confidence_pct, Some(93));
}

#[test]
fn row_fields_are_strict_and_confidence_is_optional() {
    assert_eq!(parse_row(SECOND_ROW).unwrap().confidence_pct, None);
    let without_confidence = r#"{"repo":"o/one","reason_code":"merge-risk-pass","timestamp":123}"#;
    assert_eq!(parse_row(without_confidence).unwrap().confidence_pct, None);

    for bad in [
        "not json",
        r#"{"timestamp":123}"#,
        r#"{"body":"{\"repo\":\"o/one\",\"reason_code\":\"merge-risk-pass\"}"}"#,
        r#"{"repo":"","reason_code":"merge-risk-pass"}"#,
        r#"{"repo":"o/one","reason_code":""}"#,
        r#"{"repo":42,"reason_code":"merge-risk-pass"}"#,
        r#"{"repo":"o/one","reason_code":"merge-risk-pass","confidence_pct":"93"}"#,
        r#"{"repo":"o/one","reason_code":"merge-risk-pass","confidence_pct":93.5}"#,
        r#"{"repo":"o/one","reason_code":"merge-risk-pass","confidence_pct":256}"#,
    ] {
        assert!(parse_row(bad).is_err(), "{bad}");
    }
}

#[test]
fn collect_rows_refuses_a_capped_window_counting_unparseable_lines() {
    // Cap+1 malformed lines count towards overflow just like valid lines.
    let body = ["not json"; 5].join("\n");
    let err = collect_rows(&body, 4).unwrap_err().to_string();
    assert!(err.contains("more than 4 records"), "{err}");

    let mixed = format!("{FIRST_ROW}\nnot json\n{SECOND_ROW}");
    let err = collect_rows(&mixed, 2).unwrap_err().to_string();
    assert!(err.contains("more than 2 records"), "{err}");
    // At exactly the cap, the malformed line still refuses the read.
    let err = collect_rows(&mixed, 3).unwrap_err().to_string();
    assert!(err.contains("did not decode"), "{err}");
}

#[test]
fn collect_rows_refuses_any_unparseable_row_with_its_count_and_first_reason() {
    let body = format!("{FIRST_ROW}\nnot json\n{}\n{SECOND_ROW}", r#"{"repo":"o/one"}"#);
    let err = collect_rows(&body, 10).unwrap_err().to_string();
    assert!(err.contains("2 fetched row(s)"), "{err}");
    assert!(err.contains("first: malformed row"), "{err}");
    assert!(err.contains("partial total"), "{err}");

    let rows = collect_rows(&format!("\n {FIRST_ROW}\n\n{SECOND_ROW}\n"), 2).unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].repo, "o/one");
    assert_eq!(rows[1].repo, "o/two");
}

#[test]
fn aggregation_rolls_up_per_repo_and_overall() {
    let rows = vec![
        record("o/a", reason::MERGE_RISK_SELF_RELEASE, Some(80)),
        record("o/a", reason::MERGE_RISK_SELF_RELEASE, Some(90)),
        record("o/a", reason::MERGE_RISK_PASS, Some(99)),
        record("o/b", reason::MERGE_RISK_SELF_RELEASE, Some(70)),
        // A null-confidence self-release still counts; it carries no value.
        record("o/b", reason::MERGE_RISK_SELF_RELEASE, None),
        record("o/c", "add", None),
    ];
    let mut outcome = Outcome {
        since: "s".to_string(),
        until: "u".to_string(),
        repos: vec![],
        reason_code: None,
        ..Outcome::default()
    };
    aggregate(&rows, &mut outcome);
    assert_eq!(outcome.total_records, 6);

    let a = &outcome.by_repo["o/a"];
    assert_eq!(a.records, 3);
    assert_eq!(a.by_reason_code[reason::MERGE_RISK_SELF_RELEASE], 2);
    assert_eq!(a.by_reason_code[reason::MERGE_RISK_PASS], 1);
    // Even count: median is the mean of the two middle values.
    assert_eq!(a.self_release.count, 2);
    assert_eq!(a.self_release.values, vec![80, 90]);
    assert_eq!(a.self_release.min, Some(80));
    assert_eq!(a.self_release.max, Some(90));
    assert_eq!(a.self_release.median, Some(85.0));

    let b = &outcome.by_repo["o/b"];
    // A null confidence still counts as a self-release record, but carries
    // no value (reported absent, never zero).
    assert_eq!(b.records, 2);
    assert_eq!(b.self_release.count, 2);
    assert_eq!(b.self_release.values, vec![70]);
    assert_eq!(b.self_release.min, Some(70));
    assert_eq!(b.self_release.median, Some(70.0));
    assert_eq!(b.self_release.max, Some(70));

    // Odd count median; the null-confidence self-release adds count but no
    // value.
    let overall = &outcome.overall_self_release;
    assert_eq!(overall.count, 4);
    assert_eq!(overall.values, vec![70, 80, 90]);
    assert_eq!(overall.median, Some(80.0));
    assert_eq!(overall.min, Some(70));
    assert_eq!(overall.max, Some(90));
}

#[test]
fn an_empty_window_aggregates_to_stated_zeroes() {
    let mut outcome = Outcome::default();
    aggregate(&[], &mut outcome);
    assert_eq!(outcome.total_records, 0);
    assert!(outcome.by_repo.is_empty());
    assert_eq!(outcome.overall_self_release, ConfidenceStats::default());
}

#[test]
fn endpoint_resolution_fails_closed_naming_the_variable() {
    let empty = serde_json::Value::Null;
    // Everything set: the flag wins.
    assert_eq!(
        resolve_endpoint(Some("http://flag",), Some("http://env"), &empty).unwrap(),
        "http://flag"
    );
    // No flag: env wins over config.
    assert_eq!(resolve_endpoint(None, Some("http://env"), &empty).unwrap(), "http://env");
    // No flag or env: config answers.
    let config = serde_json::json!({"telemetry": {"signoz": {"endpoint": "http://cfg"}}});
    assert_eq!(resolve_endpoint(None, None, &config).unwrap(), "http://cfg");
    // Nothing anywhere: refuse, naming TELEMETRY_READER_URL.
    let err = format!("{}", resolve_endpoint(None, None, &empty).unwrap_err());
    assert!(err.contains("TELEMETRY_READER_URL"), "{err}");
    let blank_env = "   ".to_string();
    let err = format!("{}", resolve_endpoint(None, Some(blank_env.as_str()), &empty).unwrap_err());
    assert!(err.contains("TELEMETRY_READER_URL"), "{err}");
}
