//! The `token_ranking.refresh` record for one workspace's round (issue
//! #10744).
//!
//! Every round of [`super::spawn_multi_token_ranking_refresh_task`] emits
//! exactly one record per registered workspace: a success, a failure (spawn
//! error, timeout, non-zero exit, or a panicked blocking task), or `disabled`.
//! The per-account entries come from the child's round summary
//! ([`crate::tokens_pool::round_summary`]); a child that did not write one
//! (it failed early, or is an older binary) yields `source: unknown` and no
//! entries rather than a fabricated row.
//!
//! Emission goes through the OTLP-only ops sink and cannot fail the loop: with
//! no exporter registered it does nothing, and a record whose provenance does
//! not validate is dropped with a warning.

use std::path::Path;
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};

use super::{RankingRefreshRunner, RefreshOutcome};
use crate::telemetry::kinds::token_ranking_refresh::{
    RankingSource, RoundOutcome, TokenRankingRefreshRecord,
};
use crate::telemetry::provenance::Provenance;
use crate::telemetry::TelemetryRecord;
use crate::tokens_pool::round_summary::RoundSummary;

/// A closed-set label for a failure, from the reasons
/// [`super::ScriptRankingRefreshRunner`] produces. The reason text itself is
/// never exported: it can carry the child's output.
#[must_use]
pub fn failure_class(reason: &str) -> &'static str {
    if reason.contains(" timed out after ") {
        "timeout"
    } else if reason.contains(" exited with ") {
        "nonzero_exit"
    } else if reason.starts_with("could not spawn") {
        "spawn_error"
    } else if reason.starts_with("could not poll") {
        "poll_error"
    } else {
        "error"
    }
}

/// How a round ended, as the record needs it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RoundEnd<'a> {
    /// The child exited 0.
    Success,
    /// The runner reported a failure with this reason.
    Failure(&'a str),
    /// The blocking task panicked.
    Panic,
    /// The workspace has the loop turned off.
    Disabled,
}

/// The exported form of a workspace root: its final path component
/// (`/home/alice/GitHub/loom` -> `loom`), never the absolute path.
///
/// An absolute path routinely embeds the operating user's name, which is
/// host-identifying in a way nothing else on this wire is; the final component
/// keeps the field's point (*which* workspace's round this was) at the repo
/// granularity `loom.repo` uses. Same rule as `daemon.preflight.advisory`'s
/// `workspace_root` (#8760). A path with no final component (`/`) falls back
/// to its lossy display form.
#[must_use]
pub fn workspace_label(workspace: &Path) -> String {
    workspace
        .file_name()
        .map_or_else(|| workspace.display().to_string(), |name| name.to_string_lossy().into_owned())
}

/// Build the record for one round. Pure: host and provenance are inputs.
///
/// `round_id` is derived from the full workspace path (so two workspaces that
/// share a basename on one host never collide); only the hash leaves the
/// host. The exported `workspace` is [`workspace_label`].
#[must_use]
pub fn record(
    workspace: &Path,
    end: RoundEnd<'_>,
    summary: Option<RoundSummary>,
    host_id: &str,
    started_at: DateTime<Utc>,
    duration: Duration,
    loom: Provenance,
) -> TokenRankingRefreshRecord {
    let workspace_path = workspace.display().to_string();
    let at = crate::telemetry::trace::instant(started_at);
    let (outcome, failure_class) = match end {
        RoundEnd::Success => (RoundOutcome::Success, None),
        RoundEnd::Failure(reason) => (RoundOutcome::Failure, Some(failure_class(reason))),
        RoundEnd::Panic => (RoundOutcome::Failure, Some("panic")),
        RoundEnd::Disabled => (RoundOutcome::Disabled, None),
    };
    let (source, probed_count, api_key_probe_count, accounts) = match summary {
        Some(s) => (s.source, s.probed_count(), s.api_key_probe_count(), s.accounts),
        None => (RankingSource::Unknown, 0, 0, Vec::new()),
    };
    TokenRankingRefreshRecord {
        round_id: crate::telemetry::trace::derived_hex(
            &["loom.token_ranking.refresh", host_id, &workspace_path, &at],
            32,
        ),
        started_at,
        workspace: workspace_label(workspace),
        outcome,
        failure_class: failure_class.map(str::to_string),
        source,
        probed_count,
        api_key_probe_count,
        accounts,
        duration_ms: u64::try_from(duration.as_millis()).unwrap_or(u64::MAX),
        loom,
    }
}

/// Build and emit one round's record through the OTLP-only ops sink.
pub fn emit(
    workspace: &Path,
    end: RoundEnd<'_>,
    summary: Option<RoundSummary>,
    started_at: DateTime<Utc>,
    duration: Duration,
) {
    let host_id = crate::observability::ops::global_ops_sink()
        .map_or_else(crate::sweep_registry::host_identity, |sink| sink.host_id().to_string());
    let record =
        record(workspace, end, summary, &host_id, started_at, duration, Provenance::current());
    if !record.has_provenance() {
        log::warn!(
            "token_ranking_refresh: dropped token_ranking.refresh record: invalid provenance"
        );
        return;
    }
    crate::observability::ops::emit_record(TelemetryRecord::TokenRankingRefresh(record));
}

/// Warn when a round sent a probe on an API-key credential: each one is
/// metered spend, direct to the provider. Probing is unchanged; this only
/// makes it visible in the daemon log as well as in the record.
fn warn_on_api_key_probes(workspace: &Path, summary: &RoundSummary) {
    let accounts = summary.api_key_probed_accounts();
    if accounts.is_empty() {
        return;
    }
    log::warn!(
        "token_ranking_refresh: {} probed {} account(s) holding an API key ({}); each probe is \
         a metered max_tokens=1 request to api.anthropic.com",
        workspace.display(),
        accounts.len(),
        accounts.join(", ")
    );
}

/// Run one refresh with `runner` and emit its record. Returns the outcome for
/// the loop's own logging.
pub fn refresh_and_record<R: RankingRefreshRunner + ?Sized>(
    workspace: &Path,
    runner: &mut R,
) -> RefreshOutcome {
    let started_at = Utc::now();
    let start = Instant::now();
    let outcome = runner.refresh();
    let summary = runner.take_summary();
    if let Some(summary) = &summary {
        warn_on_api_key_probes(workspace, summary);
    }
    let end = match &outcome {
        RefreshOutcome::Success => RoundEnd::Success,
        RefreshOutcome::Failure(reason) => RoundEnd::Failure(reason),
    };
    emit(workspace, end, summary, started_at, start.elapsed());
    outcome
}

/// Emit the record for a workspace the loop skipped because it is disabled.
pub fn record_disabled(workspace: &Path) {
    emit(workspace, RoundEnd::Disabled, None, Utc::now(), Duration::ZERO);
}

/// Emit the failure record for a round whose blocking task panicked.
pub fn record_panic(workspace: &Path, started_at: DateTime<Utc>) {
    let elapsed = (Utc::now() - started_at).to_std().unwrap_or_default();
    emit(workspace, RoundEnd::Panic, None, started_at, elapsed);
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
#[path = "telemetry_tests.rs"]
mod tests;
