//! The replay reader without a store: the committed SQL is split as a reader
//! runs it, parameters bind, and query output assembles into a report in
//! which an uncovered host is `unknown`. The reconstruction itself is proven
//! against the fixture store in `tests/telemetry_replay_fixture_store.rs`.

use super::*;
use chrono::TimeZone;

fn params() -> ReplayParams {
    ReplayParams {
        as_of: Utc.with_ymd_and_hms(2026, 10, 4, 13, 0, 0).unwrap(),
        window_sec: DEFAULT_WINDOW_SEC,
        repo: String::new(),
    }
}

#[test]
fn the_committed_sql_splits_into_the_state_and_coverage_queries() {
    let (state, coverage) = state_and_coverage_queries(REPLAY_QUERIES).unwrap();
    for q in [&state, &coverage] {
        assert!(q.starts_with("WITH samples AS"), "{q}");
        // Knowable-at decides membership; no comment survives the split.
        assert!(q.contains("created_at < {t:DateTime64(3)}"), "{q}");
        assert!(!q.contains("--"), "{q}");
        assert!(!q.ends_with(';'));
    }
    assert!(state.contains("FROM live"), "{state}");
    // Queries 1-3 read the instant t only, whatever span is bound.
    for q in [&state, &coverage] {
        assert!(q.contains("t = {t:DateTime64(3)}"), "{q}");
    }
    assert!(state.contains("reporting_hosts"), "{state}");
    assert!(coverage.contains("FROM status WHERE t = {t:DateTime64(3)}) s"), "{coverage}");
    // No row caps: the only LIMIT in either query is the dedupe `LIMIT 1 BY`.
    for q in [&state, &coverage] {
        assert_eq!(q.matches("LIMIT").count(), q.matches("LIMIT 1 BY").count(), "{q}");
    }
}

#[test]
fn an_unexpected_sql_shape_is_refused_not_guessed() {
    assert!(state_and_coverage_queries("SELECT 1;")
        .unwrap_err()
        .contains("begin marker"));
    let short = format!("{PREFIX_BEGIN}\nWITH x AS (SELECT 1)\n{PREFIX_END}\nSELECT 1;");
    assert!(state_and_coverage_queries(&short)
        .unwrap_err()
        .contains("expected 10 statements"));
    // Query 1 no longer begins with the prefix.
    let moved = REPLAY_QUERIES.replacen(PREFIX_BEGIN, &format!("SELECT 9\n{PREFIX_BEGIN}"), 1);
    assert!(state_and_coverage_queries(&moved)
        .unwrap_err()
        .contains("does not begin with the replay prefix"));
}

#[test]
fn parameters_bind_the_instant_in_utc_the_window_and_the_repo_scope() {
    let mut p = params();
    p.as_of += chrono::Duration::milliseconds(250);
    p.repo = "rjwalters/loom".to_string();
    assert_eq!(
        p.params(),
        [
            ("t".to_string(), "2026-10-04 13:00:00.250".to_string()),
            ("window".to_string(), "3900".to_string()),
            ("repo".to_string(), "rjwalters/loom".to_string()),
            ("span".to_string(), "0".to_string()),
            ("step".to_string(), "300".to_string()),
        ]
    );
}

const STATE: &str = r#"{"repo":"rjwalters\/loom","issue":60,"stage":"review_wait","host":"h-readd","pr":600,"entered_at":"2026-10-04 11:00:00.000000000","reporting_hosts":2}
{"repo":"rjwalters/loom","issue":"3","stage":"sweep_doctor","host":"","pr":"0","entered_at":"2026-10-04 12:10:00.000000000","reporting_hosts":"1"}"#;

const COVERAGE: &str = r#"{"emitter":"h-gap","state":"broken_chain","covered":0,"anchor_as_of":"2026-10-04 12:00:00.000000000","last_as_of":"2026-10-04 12:10:00.000000000","anchor_age_sec":3600}
{"emitter":"h-readd","state":"complete","covered":true,"anchor_as_of":"2026-10-04 12:00:00.000000000","last_as_of":"2026-10-04 12:15:00.000000000","anchor_age_sec":"3600"}

{"emitter":"h-silent","state":"no_anchor","covered":0,"anchor_as_of":null,"last_as_of":null,"anchor_age_sec":null}"#;

#[test]
fn output_assembles_with_every_host_and_uncovered_hosts_unknown() {
    let replay = assemble(&params(), STATE, COVERAGE).unwrap();
    assert_eq!(replay.state.len(), 2);
    assert_eq!(replay.state[0].host, "h-readd");
    assert_eq!(replay.state[1].issue, 3);
    assert_eq!(replay.state[1].reporting_hosts, 1);
    let verdicts: Vec<_> = replay
        .hosts
        .iter()
        .map(|h| (h.emitter.as_str(), h.verdict(), h.state.as_str()))
        .collect();
    assert_eq!(
        verdicts,
        [
            ("h-gap", "unknown", "broken_chain"),
            ("h-readd", "covered", "complete"),
            ("h-silent", "unknown", "no_anchor"),
        ]
    );
    assert_eq!(replay.hosts[1].anchor_age_sec, Some(3600));
    assert_eq!(replay.hosts[2].anchor_as_of, None);

    let text = render(&replay);
    assert!(text.contains("hosts: 3 (1 covered)"), "{text}");
    assert!(text.contains("h-silent") && text.contains("unknown  no_anchor"), "{text}");
    assert!(text.contains("rjwalters/loom#60"), "{text}");
    assert!(text.contains("holder=h-readd pr=#600"), "{text}");
    assert!(text.contains("holder=- pr=-"), "{text}");
}

#[test]
fn one_export_holding_both_queries_splits_by_shape() {
    let mixed = format!("{COVERAGE}\n{STATE}");
    assert_eq!(
        assemble_export(&params(), &mixed).unwrap(),
        assemble(&params(), STATE, COVERAGE).unwrap()
    );
}

#[test]
fn a_row_missing_a_selected_column_is_refused_with_its_line() {
    let err = assemble(&params(), "{\"repo\":\"a/b\"}", "").unwrap_err();
    assert!(err.starts_with("state:") && err.contains("issue"), "{err}");
    let err = assemble(&params(), "", "\n{\"emitter\":\"h\"}").unwrap_err();
    assert!(err.starts_with("coverage:") && err.contains("state"), "{err}");
    let err = assemble(&params(), "nope", "").unwrap_err();
    assert!(err.contains("line 1: not JSON"), "{err}");
}

#[test]
fn an_empty_store_is_an_empty_report_not_an_error() {
    let replay = assemble(&params(), "", "").unwrap();
    assert!(replay.hosts.is_empty() && replay.state.is_empty());
    assert!(render(&replay).contains("hosts: 0 (0 covered)"));
}
