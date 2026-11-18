//! Unit tests for `telemetry-hold-transitions`: bound construction, row
//! parsing and aggregation — no live telemetry-reader (Issue #14094).
#![allow(clippy::unwrap_used)]

use chrono::{TimeZone, Utc};

use super::*;
use crate::telemetry::kinds::hold_transition::reason;
use crate::telemetry::provenance::Provenance;

fn record(
    repo: &str,
    pr: u32,
    reason_code: &str,
    confidence_pct: Option<u8>,
) -> HoldTransitionRecord {
    let at = Utc.with_ymd_and_hms(2026, 11, 10, 12, 0, 0).unwrap();
    HoldTransitionRecord {
        repo: repo.to_string(),
        pr,
        transition: "decision".to_string(),
        reason_code: reason_code.to_string(),
        trigger: "champion_role".to_string(),
        confirmed: true,
        episode_id: None,
        entered_at: None,
        entered_at_lower_bound: false,
        confidence_pct,
        event_at: at,
        observed_at: at,
        loom: Provenance {
            version: "0.19.1".to_string(),
            revision: "9d8e226ce0123456789abcdef0123456789abcde".to_string(),
            tree_state: "clean".to_string(),
            complete: true,
        },
    }
}

fn row(record: HoldTransitionRecord, ts_ns: i64) -> Row {
    Row { record, ts_ns }
}

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
    assert!(sql.contains("attributes_string['loom.kind'] = 'hold.transition'"), "{sql}");
    assert!(sql.contains("timestamp >= fromUnixTimestamp64Nano(1000)"), "{sql}");
    assert!(sql.contains("timestamp < fromUnixTimestamp64Nano(2000)"), "{sql}");
    assert!(
        sql.contains("(attributes_string['loom.repo'] = {repo0:String} OR attributes_string['loom.repo'] = {repo1:String})"),
        "{sql}"
    );
    assert!(
        sql.contains("attributes_string['loom.hold.reason_code'] = {reason_code:String}"),
        "{sql}"
    );
    assert!(sql.ends_with(" ORDER BY ts_ns ASC LIMIT 101"), "{sql}");
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
    assert!(!sql.contains("loom.repo"), "{sql}");
    assert!(!sql.contains("reason_code"), "{sql}");
    assert!(sql.ends_with(" ORDER BY ts_ns ASC LIMIT 6"), "{sql}");
    assert!(params.is_empty());
}

#[test]
fn rows_parse_from_ts_ns_and_body() {
    // In the store the body column is a JSON *string* (JSONEachRow escapes
    // it), so the fixture double-encodes.
    let good = format!(
        r#"{{"ts_ns": 116000000000, "body": {}}}"#,
        serde_json::to_string(
            &serde_json::to_string(&record("o/r", 42, reason::MERGE_RISK_SELF_RELEASE, Some(93)))
                .unwrap()
        )
        .unwrap()
    );
    let parsed = parse_row(&good).unwrap();
    assert_eq!(parsed.ts_ns, 116_000_000_000);
    assert_eq!(parsed.record.repo, "o/r");
    assert_eq!(parsed.record.confidence_pct, Some(93));

    // A record with no confidence (null / absent) parses as None.
    let bare = record("o/r", 7, reason::MERGE_RISK_PASS, None);
    let good = format!(
        r#"{{"ts_ns": 1, "body": {}}}"#,
        serde_json::to_string(&serde_json::to_string(&bare).unwrap()).unwrap()
    );
    assert_eq!(parse_row(&good).unwrap().record.confidence_pct, None);

    assert!(parse_row("not json").is_err());
    assert!(parse_row(r#"{"body": "{}"}"#).is_err());
    assert!(parse_row(r#"{"ts_ns": 1}"#).is_err());
    assert!(parse_row(r#"{"ts_ns": 1, "body": "not a record"}"#).is_err());
}

#[test]
fn aggregation_rolls_up_per_repo_and_overall() {
    let rows = vec![
        row(record("o/a", 1, reason::MERGE_RISK_SELF_RELEASE, Some(80)), 1),
        row(record("o/a", 2, reason::MERGE_RISK_SELF_RELEASE, Some(90)), 2),
        row(record("o/a", 3, reason::MERGE_RISK_PASS, Some(99)), 3),
        row(record("o/b", 4, reason::MERGE_RISK_SELF_RELEASE, Some(70)), 4),
        // A null-confidence self-release still counts; it carries no value.
        row(record("o/b", 5, reason::MERGE_RISK_SELF_RELEASE, None), 5),
        row(record("o/c", 6, "add", None), 6),
    ];
    let mut outcome = Outcome {
        since: "s".to_string(),
        until: "u".to_string(),
        repos: vec![],
        reason_code: None,
        ..Outcome::default()
    };
    aggregate(&rows, 2, &mut outcome);
    assert_eq!(outcome.total_records, 6);
    assert_eq!(outcome.unparseable_records, 2);

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
    aggregate(&[], 0, &mut outcome);
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
