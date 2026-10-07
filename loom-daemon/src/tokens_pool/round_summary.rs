//! What one `tokens check` run actually did, per account (issue #10744).
//!
//! The daemon's token-ranking refresh loop does not probe in-process: it runs
//! `loom-daemon tokens check --ranking` as a child and, before this module,
//! learned only the child's exit status. The per-account facts the
//! `token_ranking.refresh` record needs — was a request sent, with which kind
//! of credential, and what came back — exist only in the child.
//!
//! So the child records them in a [`RoundTrace`] while [`super::check`] runs,
//! folds the trace and the report into a [`RoundSummary`] ([`summarize`]), and
//! writes it as JSON to the path in [`ROUND_SUMMARY_FILE_ENV`] when the parent
//! set one ([`write_summary_if_requested`]). The parent reads it back
//! ([`read_summary`]) and emits the record in the long-lived daemon, where the
//! OTLP exporter lives.
//!
//! The path rides in an environment variable rather than a flag so that an
//! older binary on disk (a rollback) silently ignores it instead of failing
//! every refresh on an unknown argument.
//!
//! No token value, or anything derived from one beyond the closed-set
//! [`CredentialKind`], is ever stored in a trace or written to a summary.

use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

use super::account_registry::AccountProvider;
use super::check::ProbeReport;
use crate::telemetry::kinds::token_ranking_refresh::{
    AccountOutcome, CredentialKind, RankingSource, TokenRankingAccount,
};

/// Environment variable naming the file a `tokens check` child writes its
/// [`RoundSummary`] to. Set only by the daemon's refresh loop.
pub const ROUND_SUMMARY_FILE_ENV: &str = "LOOM_TOKEN_RANKING_SUMMARY_FILE";

/// Whether [`super::check`] sends a request for this account: a Claude account
/// whose token has a Claude credential shape. Mirrors `dispatch_probe`, which
/// short-circuits every other case before the network.
#[must_use]
pub fn would_probe(token: &str, provider: AccountProvider) -> bool {
    provider == AccountProvider::Claude && super::bootstrap::has_claude_credential_shape(token)
}

/// What [`super::check::run_check_traced`] saw while it ran. Interior
/// mutability so the monitor re-probe hook (a `Fn`) can record into it.
#[derive(Debug, Default)]
pub struct RoundTrace {
    monitor_served: Cell<bool>,
    /// Accounts a request was sent for, with the credential kind it used.
    probed: RefCell<BTreeMap<String, CredentialKind>>,
    /// The status each probe actually returned, by account. Kept apart from
    /// the report because a monitor re-probe that errors leaves the ranking row
    /// as it was, so the report alone cannot say what the probe saw.
    results: RefCell<BTreeMap<String, String>>,
    /// Every account the run looked at, with its provider.
    seen: RefCell<BTreeMap<String, AccountProvider>>,
}

impl RoundTrace {
    /// Record that the run handled `name`. `token` is classified and dropped.
    pub fn record(&self, name: &str, token: &str, provider: AccountProvider) {
        self.seen.borrow_mut().insert(name.to_string(), provider);
        if would_probe(token, provider) {
            self.probed
                .borrow_mut()
                .insert(name.to_string(), CredentialKind::of_token(token));
        }
    }

    /// Record the status the probe for `name` returned.
    pub fn record_result(&self, name: &str, status: &str) {
        self.results
            .borrow_mut()
            .insert(name.to_string(), status.to_string());
    }

    /// A fresh claude-monitor `ranking.json` served the report.
    pub fn mark_monitor_served(&self) {
        self.monitor_served.set(true);
    }
}

/// The child's account of one run, as the parent reads it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoundSummary {
    /// Where the ranking came from.
    pub source: RankingSource,
    /// One entry per account in the report.
    pub accounts: Vec<TokenRankingAccount>,
}

impl RoundSummary {
    /// Requests sent to the provider.
    #[must_use]
    pub fn probed_count(&self) -> u32 {
        u32::try_from(self.accounts.iter().filter(|a| a.probed).count()).unwrap_or(u32::MAX)
    }

    /// Of those, requests that used an API-key credential.
    #[must_use]
    pub fn api_key_probe_count(&self) -> u32 {
        let n = self
            .accounts
            .iter()
            .filter(|a| a.probed && a.credential_kind == CredentialKind::ApiKey)
            .count();
        u32::try_from(n).unwrap_or(u32::MAX)
    }

    /// Names of the accounts probed with an API key, for the warn log.
    #[must_use]
    pub fn api_key_probed_accounts(&self) -> Vec<&str> {
        self.accounts
            .iter()
            .filter(|a| a.probed && a.credential_kind == CredentialKind::ApiKey)
            .map(|a| a.account.as_str())
            .collect()
    }
}

/// Classify the credential in `<tokens_dir>/<name>.token` without keeping it.
fn credential_kind_of_file(tokens_dir: &Path, name: &str) -> CredentialKind {
    std::fs::read_to_string(tokens_dir.join(format!("{name}.token")))
        .map_or(CredentialKind::Unknown, |t| CredentialKind::of_token(t.trim()))
}

/// Fold `report` and what `trace` saw into a [`RoundSummary`].
///
/// An account the trace never saw on a monitor-served run is a fresh monitor
/// row used as-is: `skipped_fresh`, not probed. Its credential kind is read
/// from its `.token` file (if any) so the summary still says what it holds.
#[must_use]
pub fn summarize(tokens_dir: &Path, report: &ProbeReport, trace: &RoundTrace) -> RoundSummary {
    let monitor = trace.monitor_served.get();
    let probed = trace.probed.borrow();
    let seen = trace.seen.borrow();
    let results = trace.results.borrow();
    let accounts = report
        .accounts
        .iter()
        .map(|a| {
            let provider = seen.get(&a.name).copied();
            let outcome = if monitor && provider.is_none() {
                AccountOutcome::SkippedFresh
            } else {
                // A request that was sent reports what it returned, not the
                // (possibly conservatively kept) ranking status.
                let status = results.get(&a.name).unwrap_or(&a.status);
                AccountOutcome::from_status(status)
            };
            let provider = provider.unwrap_or(AccountProvider::Claude);
            let credential_kind = match probed.get(&a.name) {
                Some(kind) => *kind,
                None if provider == AccountProvider::Claude => {
                    credential_kind_of_file(tokens_dir, &a.name)
                }
                None => CredentialKind::Unknown,
            };
            TokenRankingAccount {
                account: a.name.clone(),
                provider: provider.to_string(),
                status: a.status.clone(),
                outcome,
                credential_kind,
                probed: probed.contains_key(&a.name),
            }
        })
        .collect();
    RoundSummary {
        source: if monitor {
            RankingSource::Monitor
        } else {
            RankingSource::Probe
        },
        accounts,
    }
}

/// When [`ROUND_SUMMARY_FILE_ENV`] is set, write the run's summary there.
/// Best-effort: a write failure is a warning on stderr, never an error, so the
/// refresh itself is unaffected.
pub fn write_summary_if_requested(tokens_dir: &Path, report: &ProbeReport, trace: &RoundTrace) {
    let Some(path) = std::env::var_os(ROUND_SUMMARY_FILE_ENV).filter(|p| !p.is_empty()) else {
        return;
    };
    let summary = summarize(tokens_dir, report, trace);
    let written = serde_json::to_vec(&summary)
        .map_err(|e| e.to_string())
        .and_then(|bytes| std::fs::write(&path, bytes).map_err(|e| e.to_string()));
    if let Err(e) = written {
        eprintln!("WARNING could not write the round summary: {e}");
    }
}

/// Read a summary the child wrote. `None` when absent (an older child, or a
/// child that failed before writing) or unparseable.
#[must_use]
pub fn read_summary(path: &Path) -> Option<RoundSummary> {
    let bytes = std::fs::read(path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// Names in `trace` that a request was sent for (tests).
#[cfg(test)]
pub(crate) fn probed_names(trace: &RoundTrace) -> std::collections::BTreeSet<String> {
    trace.probed.borrow().keys().cloned().collect()
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
#[path = "round_summary_tests.rs"]
mod tests;
