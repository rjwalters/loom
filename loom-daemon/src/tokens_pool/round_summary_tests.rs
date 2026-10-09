//! Issue #10744: what a traced `tokens check` run reports per account.

use std::cell::RefCell;
use std::fs;
use std::path::{Path, PathBuf};

use chrono::Utc;

use super::*;
use crate::tokens_pool::check::{
    run_check_traced, CheckOptions, ProbeError, ProbeResponse, ProbeTransport, Source,
};

/// Canned responses in order; records whether each request carried an
/// `x-api-key` header (a metered API-key probe).
struct Stub {
    responses: RefCell<Vec<ProbeResponse>>,
    api_key_requests: RefCell<Vec<bool>>,
}

impl Stub {
    fn new(statuses: &[u16]) -> Self {
        Self {
            responses: RefCell::new(
                statuses
                    .iter()
                    .map(|&status| ProbeResponse {
                        status,
                        headers: Vec::new(),
                    })
                    .collect(),
            ),
            api_key_requests: RefCell::new(Vec::new()),
        }
    }
}

impl ProbeTransport for Stub {
    fn post(
        &self,
        _url: &str,
        headers: &[(String, String)],
        _body: &str,
        _timeout: f64,
    ) -> Result<ProbeResponse, ProbeError> {
        self.api_key_requests
            .borrow_mut()
            .push(headers.iter().any(|(k, _)| k == "x-api-key"));
        let mut queue = self.responses.borrow_mut();
        if queue.is_empty() {
            return Err(ProbeError::Request("stub exhausted".into()));
        }
        Ok(queue.remove(0))
    }
}

fn opts(source: Source) -> CheckOptions<'static> {
    CheckOptions {
        source,
        write_ranking: false,
        stagger: false,
        ..Default::default()
    }
}

fn entry<'a>(summary: &'a RoundSummary, name: &str) -> &'a TokenRankingAccount {
    summary
        .accounts
        .iter()
        .find(|a| a.account == name)
        .unwrap_or_else(|| panic!("{name} in {summary:?}"))
}

/// A probed pool: an OAuth account, an API-key account, a credential with no
/// Claude shape, an auth-dead `.bad_tokens` entry, and a codex account.
fn probe_pool(dir: &Path) {
    fs::write(dir.join("a-oauth.token"), "sk-ant-oat01-aaa\n").unwrap();
    fs::write(dir.join("b-key.token"), "sk-ant-api03-bbb\n").unwrap();
    fs::write(dir.join("c-shape.token"), "eyJhbGciOi.not-claude\n").unwrap();
    fs::write(dir.join("d-dead.token"), "sk-ant-oat01-ddd\n").unwrap();
    fs::write(dir.join("e-codex.token"), "codex-secret\n").unwrap();
    fs::write(
        dir.join("index.json"),
        serde_json::json!({
            "version": 3,
            "accounts": [
                {"env_index": 5, "name": "e-codex", "provider": "codex", "upstream_id": "monitor-pk:9", "email": "e@x.com", "file": "e-codex.token", "source": "monitor-db", "materialized": false},
            ],
        })
        .to_string(),
    )
    .unwrap();
    crate::tokens_pool::bad_tokens::mark_bad_in_dir(dir, "d-dead", "auth-dead: 401").unwrap();
}

#[test]
fn a_probe_run_reports_outcome_credential_kind_and_probed_per_account() {
    let tmp = tempfile::tempdir().unwrap();
    probe_pool(tmp.path());
    // a-oauth then b-key are the only accounts that reach the network.
    let stub = Stub::new(&[200, 429]);
    let trace = RoundTrace::default();
    let report = run_check_traced(tmp.path(), &opts(Source::Probe), &stub, &trace);
    let summary = summarize(tmp.path(), &report, &trace);

    assert_eq!(summary.source, RankingSource::Probe);
    assert_eq!(*stub.api_key_requests.borrow(), [false, true]);

    let a = entry(&summary, "a-oauth");
    assert_eq!(
        (a.outcome, a.credential_kind, a.probed),
        (AccountOutcome::Ok, CredentialKind::Oauth, true)
    );
    let b = entry(&summary, "b-key");
    assert_eq!(
        (b.outcome, b.credential_kind, b.probed),
        (AccountOutcome::RateLimited, CredentialKind::ApiKey, true)
    );
    let c = entry(&summary, "c-shape");
    assert_eq!(
        (c.outcome, c.credential_kind, c.probed),
        (AccountOutcome::AuthDead, CredentialKind::Unknown, false)
    );
    let d = entry(&summary, "d-dead");
    assert_eq!(
        (d.outcome, d.credential_kind, d.probed),
        (AccountOutcome::AuthDead, CredentialKind::Oauth, false)
    );
    let e = entry(&summary, "e-codex");
    assert_eq!(
        (e.outcome, e.credential_kind, e.probed, e.provider.as_str()),
        (AccountOutcome::Unsupported, CredentialKind::Unknown, false, "codex")
    );

    assert_eq!(summary.probed_count(), 2);
    assert_eq!(summary.api_key_probe_count(), 1);
    assert_eq!(summary.api_key_probed_accounts(), ["b-key"]);
    assert_eq!(probed_names(&trace).len(), 2);
}

#[test]
fn a_summary_never_carries_token_material() {
    let tmp = tempfile::tempdir().unwrap();
    probe_pool(tmp.path());
    let stub = Stub::new(&[200, 200]);
    let trace = RoundTrace::default();
    let report = run_check_traced(tmp.path(), &opts(Source::Probe), &stub, &trace);
    let json = serde_json::to_string(&summarize(tmp.path(), &report, &trace)).unwrap();
    for secret in ["sk-ant", "aaa", "bbb", "ddd", "eyJ", "codex-secret"] {
        assert!(!json.contains(secret), "{secret} leaked into {json}");
    }
}

/// A fresh claude-monitor `ranking.json` naming one `exhausted` account whose
/// 7d reset is `reset_days` from now, plus its `.token` file.
fn monitor_fixture(root: &Path, reset_days: i64) -> (PathBuf, PathBuf) {
    let tokens_dir = root.join("tokens");
    let monitor_dir = root.join("monitor");
    fs::create_dir_all(&tokens_dir).unwrap();
    fs::create_dir_all(&monitor_dir).unwrap();
    fs::write(tokens_dir.join("acct-m.token"), "sk-ant-oat01-mmm\n").unwrap();
    fs::write(
        tokens_dir.join("index.json"),
        serde_json::json!({
            "version": 2,
            "accounts": [{"name": "acct-m", "email": "m@example.com"}],
        })
        .to_string(),
    )
    .unwrap();
    let reset = (Utc::now() + chrono::Duration::days(reset_days))
        .format("%Y-%m-%dT%H:%M:%SZ")
        .to_string();
    fs::write(
        monitor_dir.join("ranking.json"),
        serde_json::json!({
            "schema": 1,
            "generated_at": Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string(),
            "accounts": [{
                "email": "m@example.com",
                "status": "exhausted",
                "utilization": {"5h": 0.0, "7d": 1.0},
                "resets": {"7d": reset},
            }],
        })
        .to_string(),
    )
    .unwrap();
    (tokens_dir, monitor_dir)
}

fn monitor_run(reset_days: i64, statuses: &[u16]) -> (RoundSummary, usize) {
    let tmp = tempfile::tempdir().unwrap();
    let (tokens_dir, monitor_dir) = monitor_fixture(tmp.path(), reset_days);
    let stub = Stub::new(statuses);
    let trace = RoundTrace::default();
    std::env::set_var("LOOM_CLAUDE_MONITOR_DIR", &monitor_dir);
    let report = run_check_traced(&tokens_dir, &opts(Source::Auto), &stub, &trace);
    std::env::remove_var("LOOM_CLAUDE_MONITOR_DIR");
    let requests = stub.api_key_requests.borrow().len();
    (summarize(&tokens_dir, &report, &trace), requests)
}

#[test]
#[serial_test::serial]
fn a_monitor_served_row_is_skipped_fresh_and_not_probed() {
    let (summary, requests) = monitor_run(3, &[]);
    assert_eq!(requests, 0);
    assert_eq!(summary.source, RankingSource::Monitor);
    let m = entry(&summary, "acct-m");
    assert_eq!(
        (m.outcome, m.credential_kind, m.probed, m.status.as_str()),
        (AccountOutcome::SkippedFresh, CredentialKind::Oauth, false, "exhausted")
    );
    assert_eq!(summary.probed_count(), 0);
}

#[test]
#[serial_test::serial]
fn an_overdue_monitor_row_that_is_reprobed_counts_as_probed() {
    let (summary, requests) = monitor_run(-8, &[200]);
    assert_eq!(requests, 1);
    assert_eq!(summary.source, RankingSource::Monitor);
    let m = entry(&summary, "acct-m");
    assert_eq!((m.outcome, m.probed), (AccountOutcome::Ok, true));
    assert_eq!(summary.probed_count(), 1);
    assert_eq!(summary.api_key_probe_count(), 0);
}

#[test]
#[serial_test::serial]
fn an_overdue_monitor_row_whose_reprobe_errors_reports_error_but_stays_exhausted() {
    let (summary, requests) = monitor_run(-8, &[500]);
    assert_eq!(requests, 1);
    let m = entry(&summary, "acct-m");
    assert_eq!(
        (m.outcome, m.probed, m.status.as_str()),
        (AccountOutcome::Error, true, "exhausted")
    );
    assert_eq!(summary.probed_count(), 1);
}

#[test]
#[serial_test::serial]
fn the_summary_file_is_written_only_when_requested_and_reads_back() {
    let tmp = tempfile::tempdir().unwrap();
    probe_pool(tmp.path());
    let stub = Stub::new(&[200, 200]);
    let trace = RoundTrace::default();
    let report = run_check_traced(tmp.path(), &opts(Source::Probe), &stub, &trace);
    let out = tmp.path().join("summary.json");

    std::env::remove_var(ROUND_SUMMARY_FILE_ENV);
    write_summary_if_requested(tmp.path(), &report, &trace);
    assert!(!out.exists());

    std::env::set_var(ROUND_SUMMARY_FILE_ENV, &out);
    write_summary_if_requested(tmp.path(), &report, &trace);
    std::env::remove_var(ROUND_SUMMARY_FILE_ENV);

    let read = read_summary(&out).expect("summary written");
    assert_eq!(read, summarize(tmp.path(), &report, &trace));
    assert_eq!(read_summary(&tmp.path().join("missing.json")), None);
}

#[test]
fn credential_kind_and_outcome_classification() {
    assert_eq!(CredentialKind::of_token("sk-ant-oat01-x"), CredentialKind::Oauth);
    assert_eq!(CredentialKind::of_token("sk-ant-api03-x"), CredentialKind::ApiKey);
    assert_eq!(CredentialKind::of_token("eyJ"), CredentialKind::Unknown);
    assert_eq!(CredentialKind::of_token(""), CredentialKind::Unknown);
    for (status, outcome) in [
        ("available", AccountOutcome::Ok),
        ("rate_limited", AccountOutcome::RateLimited),
        ("exhausted", AccountOutcome::RateLimited),
        ("blocked", AccountOutcome::AuthDead),
        ("skipped", AccountOutcome::SkippedFresh),
        ("unsupported", AccountOutcome::Unsupported),
        ("error", AccountOutcome::Error),
    ] {
        assert_eq!(AccountOutcome::from_status(status), outcome, "{status}");
    }
    assert!(would_probe("sk-ant-api03-x", AccountProvider::Claude));
    assert!(!would_probe("", AccountProvider::Claude));
    assert!(!would_probe("sk-ant-oat01-x", AccountProvider::Codex));
}
