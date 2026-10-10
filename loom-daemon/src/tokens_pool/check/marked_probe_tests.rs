//! Tests for [`super`] (issue #8972): an explicit `--source probe` really
//! probes a bad-marked account, and every row reports `probed` plus the
//! standing mark separately from the live result.
//!
//! Every credential string here is an obviously fake fixture, written into a
//! tempdir and sent only to an in-process stub. No assertion message prints
//! one.

use std::cell::RefCell;
use std::collections::{BTreeSet, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};

use super::super::{
    discover_tokens, discover_tokens_for, format_table, run_check, AccountResult, CheckOptions,
    ProbeError, ProbeReport, ProbeResponse, ProbeTransport, Source, REPROBE_AUTH_DEAD_REASON,
};
use super::BadMark;
use crate::tokens_pool::bad_tokens::BadReasonClass;
use crate::tokens_pool::monitor_dir::test_env::MonitorDirEnvGuard;

const FAKE_TOKEN: &str = "sk-ant-oat01-fake-fixture-not-a-credential";
const MARKED_AT: &str = "2026-09-01T00:00:00Z";
const AUTH_REASON: &str = "auth-dead: 401 Invalid bearer token";

/// A stub transport that serves canned responses **and records every call's
/// headers**, so a test can assert that a request did (or did not) happen and
/// which credential it carried.
struct RecordingTransport {
    responses: RefCell<VecDeque<Result<ProbeResponse, ProbeError>>>,
    calls: RefCell<Vec<Vec<(String, String)>>>,
}

impl RecordingTransport {
    fn new(responses: Vec<Result<ProbeResponse, ProbeError>>) -> Self {
        Self {
            responses: RefCell::new(responses.into()),
            calls: RefCell::new(Vec::new()),
        }
    }

    fn call_count(&self) -> usize {
        self.calls.borrow().len()
    }

    /// Whether call `index` carried `token` as its bearer credential.
    fn call_carried(&self, index: usize, token: &str) -> bool {
        let expected = format!("Bearer {token}");
        self.calls.borrow().get(index).is_some_and(|headers| {
            headers
                .iter()
                .any(|(name, value)| name == "authorization" && *value == expected)
        })
    }
}

impl ProbeTransport for RecordingTransport {
    fn post(
        &self,
        _url: &str,
        headers: &[(String, String)],
        _body: &str,
        _timeout: f64,
    ) -> Result<ProbeResponse, ProbeError> {
        self.calls.borrow_mut().push(headers.to_vec());
        self.responses
            .borrow_mut()
            .pop_front()
            .unwrap_or_else(|| Err(ProbeError::Request("stub exhausted".into())))
    }
}

fn healthy() -> Result<ProbeResponse, ProbeError> {
    Ok(ProbeResponse {
        status: 200,
        headers: vec![
            ("anthropic-ratelimit-unified-5h-utilization".into(), "0.35".into()),
            ("anthropic-ratelimit-unified-7d-utilization".into(), "0.25".into()),
        ],
    })
}

fn unauthorized() -> Result<ProbeResponse, ProbeError> {
    Ok(ProbeResponse {
        status: 401,
        headers: Vec::new(),
    })
}

/// A pool holding one account, `agent-auth`, with a non-empty `.token` file
/// and a standing auth-class mark.
fn auth_marked_pool() -> tempfile::TempDir {
    let tmp = tempfile::tempdir().unwrap();
    fs::write(tmp.path().join("agent-auth.token"), format!("{FAKE_TOKEN}\n")).unwrap();
    fs::write(
        tmp.path().join(".bad_tokens"),
        format!("{MARKED_AT} agent-auth {AUTH_REASON}\n"),
    )
    .unwrap();
    tmp
}

fn opts(source: Source, write_ranking: bool) -> CheckOptions<'static> {
    CheckOptions {
        source,
        write_ranking,
        stagger: false,
        ..Default::default()
    }
}

fn only_row(report: &ProbeReport) -> &AccountResult {
    assert_eq!(report.accounts.len(), 1, "expected exactly one row");
    &report.accounts[0]
}

fn auth_mark() -> BadMark {
    BadMark {
        class: BadReasonClass::Auth,
        reason: AUTH_REASON.to_string(),
        marked_at: MARKED_AT.to_string(),
    }
}

/// `Source::Auto` with no claude-monitor data at all: the probe fall-through.
fn run_auto_fall_through(tokens_dir: &Path, transport: &RecordingTransport) -> ProbeReport {
    let empty_monitor = tempfile::tempdir().unwrap();
    let guard = MonitorDirEnvGuard::legacy(empty_monitor.path());
    let report = run_check(tokens_dir, &opts(Source::Auto, false), transport);
    drop(guard);
    report
}

// ---- discovery ---------------------------------------------------------

/// Rewrite of `discover_does_not_probe_auth_blocked_accounts`: the default
/// discovery (what `auto` and the overdue re-probe use) still hands an
/// auth-marked account an empty token; only the explicit-probe discovery
/// hands back the live one.
#[test]
fn discover_hands_an_auth_marked_account_its_token_only_for_an_explicit_probe() {
    let tmp = auth_marked_pool();

    let periodic = discover_tokens(tmp.path());
    assert_eq!(periodic.len(), 1);
    assert!(
        periodic[0].1.is_empty(),
        "the periodic paths must not probe an auth-marked account"
    );
    assert!(discover_tokens_for(tmp.path(), false)[0].1.is_empty());

    let explicit = discover_tokens_for(tmp.path(), true);
    assert_eq!(explicit.len(), 1);
    assert!(explicit[0].1 == FAKE_TOKEN, "an explicit probe must get the live credential");
}

// ---- Source::Probe really probes ---------------------------------------

#[test]
fn probe_source_sends_exactly_one_request_for_an_auth_marked_account() {
    let tmp = auth_marked_pool();
    let t = RecordingTransport::new(vec![healthy()]);

    let report = run_check(tmp.path(), &opts(Source::Probe, false), &t);

    assert_eq!(t.call_count(), 1, "exactly one request for the marked account");
    assert!(
        t.call_carried(0, FAKE_TOKEN),
        "the request must carry the account's own credential"
    );
    assert!(only_row(&report).probed);
}

/// Rewrite of `run_check_surfaces_the_blocking_reason_for_a_bad_account`: the
/// reason is still surfaced in the row and the table — now as the standing
/// mark beside a live result, not instead of one.
#[test]
fn a_stale_mark_is_reported_beside_a_healthy_live_result() {
    let tmp = auth_marked_pool();
    let t = RecordingTransport::new(vec![healthy()]);

    let report = run_check(tmp.path(), &opts(Source::Probe, false), &t);
    let row = only_row(&report);

    assert!(row.probed);
    assert_eq!(row.probe_status.as_deref(), Some("available"), "the live verdict");
    assert!((row.s5h_utilization.unwrap() - 0.35).abs() < 1e-9);
    assert!((row.s7d_utilization.unwrap() - 0.25).abs() < 1e-9);
    assert_eq!(row.error, None, "a healthy live probe reports no error");
    assert_eq!(row.status, "blocked", "status describes selectability; the mark still stands");
    assert_eq!(row.bad_mark, Some(auth_mark()));

    let json = row.to_json();
    assert_eq!(json["probed"], true);
    assert_eq!(json["probe_status"], "available");
    assert_eq!(json["5h_utilization"], 0.35);
    assert_eq!(json["7d_utilization"], 0.25);
    assert_eq!(json["bad_mark"]["class"], "auth");
    assert_eq!(json["bad_mark"]["reason"], AUTH_REASON);
    assert_eq!(json["bad_mark"]["marked_at"], MARKED_AT);

    // Not indistinguishable from a healthy unmarked account ...
    let unmarked = tempfile::tempdir().unwrap();
    fs::write(unmarked.path().join("agent-auth.token"), FAKE_TOKEN).unwrap();
    let healthy_report = run_check(
        unmarked.path(),
        &opts(Source::Probe, false),
        &RecordingTransport::new(vec![healthy()]),
    );
    assert_ne!(json, only_row(&healthy_report).to_json());
    assert_eq!(only_row(&healthy_report).status, "available");
    assert_eq!(only_row(&healthy_report).bad_mark, None);
    // ... nor from a confirmed-dead one.
    let dead_report = run_check(
        tmp.path(),
        &opts(Source::Probe, false),
        &RecordingTransport::new(vec![unauthorized()]),
    );
    assert_ne!(json, only_row(&dead_report).to_json());

    let table = format_table(&report);
    assert!(
        table.contains(AUTH_REASON),
        "table does not surface the recorded reason: {table}"
    );
}

#[test]
fn a_confirmed_dead_mark_reports_the_live_401_and_appends_nothing() {
    let tmp = auth_marked_pool();
    let bad_path = tmp.path().join(".bad_tokens");
    let before = fs::read(&bad_path).unwrap();
    let t = RecordingTransport::new(vec![unauthorized()]);

    let report = run_check(tmp.path(), &opts(Source::Probe, true), &t);
    let row = only_row(&report);

    assert_eq!(t.call_count(), 1);
    assert!(row.probed);
    assert_eq!(row.error.as_deref(), Some("auth_401"), "the live result, not the stored reason");
    assert_eq!(row.probe_status.as_deref(), Some("blocked"));
    assert_eq!(row.status, "blocked");
    assert_eq!(row.bad_mark, Some(auth_mark()));
    assert_eq!(fs::read(&bad_path).unwrap(), before, ".bad_tokens must gain no line");
}

// ---- the periodic paths still skip, and say so --------------------------

#[test]
#[serial_test::serial]
fn auto_fall_through_does_not_probe_and_reports_probed_false() {
    let tmp = auth_marked_pool();
    let t = RecordingTransport::new(vec![]);

    let report = run_auto_fall_through(tmp.path(), &t);
    let row = only_row(&report);

    assert_eq!(t.call_count(), 0, "auto must not probe a permanently-marked account");
    assert!(!row.probed);
    assert_eq!(row.status, "blocked");
    assert_eq!(row.probe_status, None, "no request, so no live verdict");
    assert_eq!(row.bad_mark, Some(auth_mark()));

    let json = row.to_json();
    assert_eq!(json["probed"], false, "JSON alone must show this is not a measurement");
    assert_eq!(json["bad_mark"]["reason"], AUTH_REASON);
    assert_eq!(json["bad_mark"]["marked_at"], MARKED_AT);
    assert!(json.get("probe_status").is_none());
    // The pre-#8972 `error` echo is kept for existing consumers.
    assert_eq!(json["error"], format!("auth: {AUTH_REASON}"));
}

/// A monitor-served run makes no request for a marked account either; its row
/// carries `probed: false` and the standing mark.
#[test]
#[serial_test::serial]
fn a_monitor_served_row_reports_probed_false_and_the_standing_mark() {
    let tmp = tempfile::tempdir().unwrap();
    let (tokens_dir, monitor_dir) = monitor_fixture(tmp.path());
    fs::write(
        tokens_dir.join(".bad_tokens"),
        format!("{MARKED_AT} acct-1 {REPROBE_AUTH_DEAD_REASON}\n"),
    )
    .unwrap();
    let t = RecordingTransport::new(vec![]);

    let guard = MonitorDirEnvGuard::legacy(&monitor_dir);
    let report = run_check(&tokens_dir, &opts(Source::Monitor, false), &t);
    drop(guard);
    let row = only_row(&report);

    assert_eq!(t.call_count(), 0);
    assert_eq!(row.to_json()["probed"], false);
    assert_eq!(row.bad_mark.as_ref().map(|m| m.class), Some(BadReasonClass::Auth));
    assert_eq!(row.bad_mark.as_ref().map(|m| m.marked_at.as_str()), Some(MARKED_AT));
    let table = format_table(&report);
    assert!(!table.contains("probed at"), "nothing was probed: {table}");
    assert!(table.contains("not probed"), "{table}");
}

/// One `available` account served from a fresh claude-monitor `ranking.json`.
fn monitor_fixture(root: &Path) -> (PathBuf, PathBuf) {
    let tokens_dir = root.join("tokens");
    let monitor_dir = root.join("monitor");
    fs::create_dir_all(&tokens_dir).unwrap();
    fs::create_dir_all(&monitor_dir).unwrap();
    fs::write(tokens_dir.join("acct-1.token"), format!("{FAKE_TOKEN}\n")).unwrap();
    fs::write(
        tokens_dir.join("index.json"),
        serde_json::json!({
            "version": 2,
            "accounts": [{"name": "acct-1", "email": "one@example.com"}],
        })
        .to_string(),
    )
    .unwrap();
    fs::write(
        monitor_dir.join("ranking.json"),
        serde_json::json!({
            "schema": 1,
            "generated_at": chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string(),
            "accounts": [{
                "email": "one@example.com",
                "status": "available",
                "utilization": {"5h": 0.1, "7d": 0.2},
                "resets": {},
            }],
        })
        .to_string(),
    )
    .unwrap();
    (tokens_dir, monitor_dir)
}

// ---- JSON shape is additive ---------------------------------------------

#[test]
fn probed_is_on_every_row_and_the_existing_keys_are_unchanged() {
    let tmp = tempfile::tempdir().unwrap();
    fs::write(tmp.path().join("agent-1.token"), FAKE_TOKEN).unwrap();
    fs::write(tmp.path().join("agent-2.token"), FAKE_TOKEN).unwrap();
    fs::write(tmp.path().join(".bad_tokens"), format!("{MARKED_AT} agent-2 {AUTH_REASON}\n"))
        .unwrap();
    let t = RecordingTransport::new(vec![healthy(), unauthorized()]);

    let report = run_check(tmp.path(), &opts(Source::Probe, false), &t);
    let json = report.to_json();
    let rows = json["accounts"].as_array().unwrap();
    assert_eq!(rows.len(), 2);
    for row in rows {
        assert!(row["probed"].is_boolean(), "`probed` missing from {}", row["name"]);
    }

    let plain = rows.iter().find(|r| r["name"] == "agent-1").unwrap();
    let keys: BTreeSet<&str> = plain
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        keys,
        BTreeSet::from([
            "name",
            "status",
            "5h_utilization",
            "7d_utilization",
            "7d_reset",
            "5h_reset",
            "limit_reset",
            "reset_overdue",
            "probed",
        ]),
        "an unmarked healthy row gains only `probed`"
    );
    assert!(plain["name"].is_string());
    assert!(plain["status"].is_string());
    assert!(plain["5h_utilization"].is_number());
    assert!(plain["7d_utilization"].is_number());
    assert!(plain["reset_overdue"].is_boolean());

    let marked = rows.iter().find(|r| r["name"] == "agent-2").unwrap();
    assert!(marked["error"].is_string(), "`error` keeps its name and type");
    assert!(marked["bad_mark"].is_object());

    // A row nobody probed says so.
    assert_eq!(AccountResult::new("agent-3", "available").to_json()["probed"], false);
    // And no JSON field carries the credential.
    assert!(!json.to_string().contains(FAKE_TOKEN));
}

// ---- reporting only: selection state is untouched -----------------------

#[test]
fn a_healthy_probe_of_a_marked_account_changes_neither_bad_tokens_nor_its_ranking_row() {
    let tmp = auth_marked_pool();
    fs::write(tmp.path().join("agent-ok.token"), FAKE_TOKEN).unwrap();
    let bad_path = tmp.path().join(".bad_tokens");
    let before = fs::read(&bad_path).unwrap();
    // Sorted-name order: agent-auth, then agent-ok.
    let t = RecordingTransport::new(vec![healthy(), healthy()]);

    run_check(tmp.path(), &opts(Source::Probe, true), &t);

    assert_eq!(fs::read(&bad_path).unwrap(), before, ".bad_tokens must be byte-identical");
    let ranking = fs::read_to_string(tmp.path().join(".ranking")).unwrap();
    let lines: Vec<&str> = ranking.lines().collect();
    assert!(
        lines.contains(&"agent-auth|blocked"),
        ".ranking must keep the marked account blocked: {ranking}"
    );
    assert!(
        lines.iter().any(|l| l.starts_with("agent-ok|available|")),
        "the unmarked account is ranked as before: {ranking}"
    );
    assert!(!ranking.contains(FAKE_TOKEN));
}

/// A self-clearing `exhaustion` mark was already probed before #8972 and its
/// status is the live one (#7522) — it only gains the reported mark.
#[test]
fn an_exhaustion_mark_keeps_its_live_status_and_gains_only_the_reported_mark() {
    let tmp = tempfile::tempdir().unwrap();
    fs::write(tmp.path().join("agent-1.token"), FAKE_TOKEN).unwrap();
    let marked = (chrono::Utc::now() - chrono::Duration::seconds(600))
        .format("%Y-%m-%dT%H:%M:%SZ")
        .to_string();
    fs::write(
        tmp.path().join(".bad_tokens"),
        format!("{marked} agent-1 exhausted: hit your session limit\n"),
    )
    .unwrap();
    let t = RecordingTransport::new(vec![healthy()]);

    let report = run_check(tmp.path(), &opts(Source::Probe, true), &t);
    let row = only_row(&report);

    assert_eq!(row.status, "available");
    assert_eq!(row.probe_status, None);
    assert!(row.probed);
    let mark = row
        .bad_mark
        .as_ref()
        .expect("the standing mark is reported");
    assert_eq!(mark.class, BadReasonClass::Exhaustion);
    assert_eq!(mark.marked_at, marked);
    assert_eq!(
        fs::read_to_string(tmp.path().join(".ranking")).unwrap(),
        "agent-1|available|0.35\n"
    );
    assert!(format_table(&report).contains(&marked));
}

// ---- mark age -----------------------------------------------------------

#[test]
fn the_recorded_timestamp_is_surfaced_in_json_and_table() {
    let tmp = auth_marked_pool();
    let report = run_check(
        tmp.path(),
        &opts(Source::Probe, false),
        &RecordingTransport::new(vec![healthy()]),
    );

    assert_eq!(only_row(&report).to_json()["bad_mark"]["marked_at"], MARKED_AT);
    assert!(format_table(&report).contains(&format!("recorded {MARKED_AT}")));
}

#[test]
fn a_malformed_timestamp_is_reported_as_the_raw_text_it_was() {
    let tmp = tempfile::tempdir().unwrap();
    fs::write(tmp.path().join("agent-odd.token"), FAKE_TOKEN).unwrap();
    fs::write(tmp.path().join(".bad_tokens"), "last-tuesday agent-odd quota wobble\n").unwrap();
    let t = RecordingTransport::new(vec![healthy()]);

    let report = run_check(tmp.path(), &opts(Source::Probe, false), &t);
    let row = only_row(&report);

    assert_eq!(
        t.call_count(),
        1,
        "a malformed-timestamp mark is probed under --source probe too"
    );
    assert_eq!(row.status, "blocked");
    let json = row.to_json();
    assert_eq!(json["bad_mark"]["class"], "malformed-timestamp");
    assert_eq!(
        json["bad_mark"]["marked_at"], "last-tuesday",
        "raw text, not a fabricated instant"
    );
    let table = format_table(&report);
    assert!(table.contains("unparseable timestamp \"last-tuesday\""), "{table}");
}

// ---- the table tells the three cases apart ------------------------------

#[test]
#[serial_test::serial]
fn the_table_distinguishes_stale_confirmed_and_unprobed() {
    let tmp = auth_marked_pool();
    let table_for = |response| {
        let t = RecordingTransport::new(vec![response]);
        table_body(&run_check(tmp.path(), &opts(Source::Probe, false), &t))
    };
    let stale = table_for(healthy());
    let confirmed = table_for(unauthorized());
    let unprobed = table_body(&run_auto_fall_through(tmp.path(), &RecordingTransport::new(vec![])));

    assert_ne!(stale, confirmed);
    assert_ne!(stale, unprobed);
    assert_ne!(confirmed, unprobed);

    assert!(stale.contains("tokens unblock agent-auth"), "{stale}");
    assert!(stale.contains("probed live: available"), "{stale}");
    assert!(confirmed.contains("probed live: blocked, auth_401"), "{confirmed}");
    assert!(confirmed.contains("confirms"), "{confirmed}");
    assert!(!confirmed.contains("tokens unblock"), "{confirmed}");
    assert!(
        unprobed.contains(&format!("not probed — bad-mark recorded {MARKED_AT}")),
        "{unprobed}"
    );
    for table in [&stale, &confirmed, &unprobed] {
        assert!(table.contains(AUTH_REASON), "{table}");
        assert!(!table.contains(FAKE_TOKEN));
    }
}

/// The account rows of a table: everything after the header (whose first line
/// carries a wall-clock `ranked_at` that differs run to run).
fn table_body(report: &ProbeReport) -> String {
    let table = format_table(report);
    table.lines().skip(1).collect::<Vec<_>>().join("\n")
}

#[test]
#[serial_test::serial]
fn the_header_claims_a_probe_only_when_a_request_was_made() {
    let tmp = auth_marked_pool();
    let probed = run_check(
        tmp.path(),
        &opts(Source::Probe, false),
        &RecordingTransport::new(vec![healthy()]),
    );
    assert!(format_table(&probed).starts_with("Token pool ranking (probed at "));

    let unprobed = run_auto_fall_through(tmp.path(), &RecordingTransport::new(vec![]));
    let header = format_table(&unprobed);
    assert!(header.starts_with("Token pool ranking (ranked at "), "{header}");
    assert!(header.contains("no account was probed"), "{header}");
}

// ---- nothing to probe with ----------------------------------------------

#[test]
fn an_empty_token_file_for_a_marked_account_is_reported_unprobed_not_dropped() {
    let tmp = tempfile::tempdir().unwrap();
    fs::write(tmp.path().join("agent-auth.token"), "\n").unwrap();
    fs::write(
        tmp.path().join(".bad_tokens"),
        format!("{MARKED_AT} agent-auth {AUTH_REASON}\n"),
    )
    .unwrap();
    let t = RecordingTransport::new(vec![]);

    let report = run_check(tmp.path(), &opts(Source::Probe, false), &t);
    let row = only_row(&report);

    assert_eq!(t.call_count(), 0, "there is nothing to probe with");
    assert!(!row.probed);
    assert_eq!(row.status, "blocked");
    assert_eq!(row.probe_status, None);
    assert_eq!(row.bad_mark, Some(auth_mark()));
    assert_eq!(row.to_json()["probed"], false);
}

/// An unmarked account with an empty `.token` file is still skipped entirely,
/// as before.
#[test]
fn an_empty_token_file_for_an_unmarked_account_is_still_skipped() {
    let tmp = tempfile::tempdir().unwrap();
    fs::write(tmp.path().join("agent-empty.token"), "\n").unwrap();
    assert!(discover_tokens_for(tmp.path(), true).is_empty());
    assert!(discover_tokens_for(tmp.path(), false).is_empty());
}
