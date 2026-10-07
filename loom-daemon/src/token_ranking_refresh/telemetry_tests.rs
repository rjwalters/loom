//! Issue #10744: every refresh round emits exactly one `token_ranking.refresh`
//! record, whatever happened to it.

use std::fs;
use std::path::{Path, PathBuf};

use super::*;
use crate::observability::ops::capture::capture;
use crate::telemetry::kinds::token_ranking_refresh::{
    AccountOutcome, CredentialKind, TokenRankingAccount,
};
use crate::token_ranking_refresh::ScriptRankingRefreshRunner;

fn account(name: &str, kind: CredentialKind, probed: bool) -> TokenRankingAccount {
    TokenRankingAccount {
        account: name.to_string(),
        provider: "claude".to_string(),
        status: "available".to_string(),
        outcome: AccountOutcome::Ok,
        credential_kind: kind,
        probed,
    }
}

fn summary() -> RoundSummary {
    RoundSummary {
        source: RankingSource::Probe,
        accounts: vec![
            account("acct-oauth", CredentialKind::Oauth, true),
            account("acct-key", CredentialKind::ApiKey, true),
        ],
    }
}

/// A runner returning a fixed outcome and summary.
struct Scripted {
    outcome: RefreshOutcome,
    summary: Option<RoundSummary>,
}

impl RankingRefreshRunner for Scripted {
    fn refresh(&mut self) -> RefreshOutcome {
        self.outcome.clone()
    }
    fn take_summary(&mut self) -> Option<RoundSummary> {
        self.summary.take()
    }
}

/// The one record `f` emitted.
fn one_record(f: impl FnOnce()) -> TokenRankingRefreshRecord {
    let ((), captured) = capture(f);
    let [TelemetryRecord::TokenRankingRefresh(record)] = captured.records.as_slice() else {
        panic!("one token_ranking.refresh record, got {:?}", captured.records);
    };
    record.clone()
}

#[test]
fn a_successful_round_emits_one_record_with_its_accounts() {
    let mut runner = Scripted {
        outcome: RefreshOutcome::Success,
        summary: Some(summary()),
    };
    let record = one_record(|| {
        let outcome = refresh_and_record(Path::new("/repo-a"), &mut runner);
        assert_eq!(outcome, RefreshOutcome::Success);
    });
    assert_eq!(record.outcome, RoundOutcome::Success);
    assert_eq!(record.workspace, "/repo-a");
    assert_eq!(record.source, RankingSource::Probe);
    assert_eq!(record.accounts, summary().accounts);
    assert_eq!((record.probed_count, record.api_key_probe_count), (2, 1));
    assert_eq!(record.failure_class, None);
    assert!(record.has_provenance());
    assert_eq!(record.round_id.len(), 32);
}

#[test]
fn an_api_key_probe_is_warned_about_by_account_name() {
    let mut runner = Scripted {
        outcome: RefreshOutcome::Success,
        summary: Some(summary()),
    };
    let logs = crate::test_log_capture::capture_logs(|| {
        let _ = capture(|| refresh_and_record(Path::new("/repo-a"), &mut runner));
    });
    let warns: Vec<_> = logs
        .iter()
        .filter(|(level, m)| *level == log::Level::Warn && m.contains("API key"))
        .collect();
    assert_eq!(warns.len(), 1, "{logs:?}");
    assert!(warns[0].1.contains("acct-key") && !warns[0].1.contains("acct-oauth"));
}

#[test]
fn a_failed_round_still_emits_one_record() {
    let mut runner = Scripted {
        outcome: RefreshOutcome::Failure("`/bin/x` timed out after 120s".to_string()),
        summary: None,
    };
    let record = one_record(|| {
        let _ = refresh_and_record(Path::new("/repo-a"), &mut runner);
    });
    assert_eq!(record.outcome, RoundOutcome::Failure);
    assert_eq!(record.failure_class.as_deref(), Some("timeout"));
    assert_eq!(record.source, RankingSource::Unknown);
    assert!(record.accounts.is_empty());
}

#[test]
fn disabled_and_panicked_rounds_each_emit_one_record() {
    let disabled = one_record(|| record_disabled(Path::new("/repo-off")));
    assert_eq!(disabled.outcome, RoundOutcome::Disabled);
    assert_eq!(disabled.probed_count, 0);

    let panicked = one_record(|| record_panic(Path::new("/repo-a"), Utc::now()));
    assert_eq!(panicked.outcome, RoundOutcome::Failure);
    assert_eq!(panicked.failure_class.as_deref(), Some("panic"));
}

#[test]
fn failure_classes_match_the_runner_reasons() {
    assert_eq!(failure_class("`/b` timed out after 120s"), "timeout");
    assert_eq!(
        failure_class("`/b` exited with exit status: 1: tokens directory missing"),
        "nonzero_exit"
    );
    assert_eq!(failure_class("could not spawn `/b`: No such file"), "spawn_error");
    assert_eq!(failure_class("could not poll `/b`: oops"), "poll_error");
    assert_eq!(failure_class("could not create probe output file: denied"), "error");
}

#[test]
fn the_round_id_is_derived_not_random() {
    let at = Utc::now();
    let build = |ws: &str| {
        record(
            Path::new(ws),
            RoundEnd::Success,
            None,
            "host-1",
            at,
            Duration::ZERO,
            Provenance::current(),
        )
        .round_id
    };
    assert_eq!(build("/repo-a"), build("/repo-a"));
    assert_ne!(build("/repo-a"), build("/repo-b"));
}

fn write_fake_bin(dir: &Path, body: &str) -> PathBuf {
    let path = dir.join("fake-daemon.sh");
    fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    }
    path
}

/// End to end through the real runner: the child writes its summary to the
/// path the parent hands it, and the record carries those accounts.
#[test]
fn the_real_runner_reads_the_childs_summary_file() {
    let tmp = tempfile::tempdir().unwrap();
    let json = serde_json::to_string(&summary()).unwrap();
    fs::write(tmp.path().join("summary.json"), &json).unwrap();
    let src = tmp.path().join("summary.json");
    let bin = write_fake_bin(
        tmp.path(),
        &format!("cp '{}' \"$LOOM_TOKEN_RANKING_SUMMARY_FILE\"; exit 0", src.display()),
    );
    let mut runner = ScriptRankingRefreshRunner::new(tmp.path().to_path_buf()).with_bin(bin);
    let record = one_record(|| {
        let _ = refresh_and_record(tmp.path(), &mut runner);
    });
    assert_eq!(record.outcome, RoundOutcome::Success);
    assert_eq!(record.accounts, summary().accounts);
    assert_eq!(record.api_key_probe_count, 1);
}

/// A child that writes no summary (an older binary) and fails still yields a
/// failure record with no fabricated accounts.
#[test]
fn the_real_runner_without_a_summary_reports_unknown_source() {
    let tmp = tempfile::tempdir().unwrap();
    let bin = write_fake_bin(tmp.path(), "echo no tokens; exit 1");
    let mut runner = ScriptRankingRefreshRunner::new(tmp.path().to_path_buf()).with_bin(bin);
    let record = one_record(|| {
        let _ = refresh_and_record(tmp.path(), &mut runner);
    });
    assert_eq!(record.outcome, RoundOutcome::Failure);
    assert_eq!(record.failure_class.as_deref(), Some("nonzero_exit"));
    assert_eq!(record.source, RankingSource::Unknown);
    assert!(record.accounts.is_empty());
}
