//! The release pass's SigNoz export (#10752): one `pass.summary` per pass,
//! one `pass.verdict` per artifact it decided, and the caller scope that
//! stamps every GitHub call the pass makes with `github.caller`.
//!
//! Before this, a pass left one local `log::info!` line, and its GitHub calls
//! were `invoke github` spans named by call site (`api.rest`,
//! `comment.post`), indistinguishable from every other caller's. The tick
//! now wraps the pass in an [`Observer`]:
//!
//! 1. [`Observer::start`] enters [`crate::gh_invocation::caller_scope`] under
//!    the pass's `forge_call_stats` caller name, so every span the pass
//!    produces carries `github.caller`, and a write also carries
//!    `github.number` and `github.repo`.
//! 2. [`Observer::finish`] ends the scope and emits the records through the
//!    OTLP-only ops sink. Without an OTLP exporter it builds nothing.
//!
//! # Verdict volume: changes plus a heartbeat
//!
//! A pass runs every five minutes per workspace and decides every
//! `loom:blocked` artifact again. rjwalters/loom alone carried 46 on
//! 2026-10-07, so one verdict per artifact per pass would be ~13k records a
//! day for one repository, nearly all repeating the previous one. So:
//!
//! - a verdict that **wrote** (released, re-parked or failed, mode `on`) is
//!   always emitted;
//! - any other verdict is emitted when it differs from the last one emitted
//!   for that artifact (verdict, reason, blocker states, mode), and otherwise
//!   once per heartbeat: [`HEARTBEAT_ENV`], default one hour; `0` emits
//!   every verdict of every pass.
//!
//! Every held artifact therefore appears in any window of an hour or more,
//! and each `pass.summary` keeps the exact per-pass counts, with
//! `verdicts_unchanged` saying how many verdicts it held back.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};

use super::release::{Report, Skip};
use super::release_items::{Item, ItemVerdict};
use crate::eta::Provenance;
use crate::gh_invocation::caller_scope::{self, Scope};
use crate::telemetry::kinds::pass::{
    BlockerState, GithubSpend, PassMode, PassOutcome, PassSummaryRecord, PassVerdictRecord,
    REPO_UNRESOLVED,
};
use crate::telemetry::TelemetryRecord;

/// Seconds between re-emissions of an unchanged verdict (`0` = every pass).
pub const HEARTBEAT_ENV: &str = "LOOM_RELEASE_STALE_BLOCKED_VERDICT_HEARTBEAT_SECS";
/// [`HEARTBEAT_ENV`]'s default.
pub const DEFAULT_HEARTBEAT_SECS: u64 = 3600;

/// A refusal's text is cut to this many characters.
const MAX_REFUSAL: usize = 240;

/// One observed pass: its caller scope and clock.
#[derive(Debug)]
#[must_use = "dropping the observer ends the pass's caller scope without emitting"]
pub struct Observer {
    scope: Scope,
    mechanism: &'static str,
    started_at: DateTime<Utc>,
    clock: Instant,
}

impl Observer {
    /// Start observing a pass named `mechanism` (its `forge_call_stats`
    /// caller): every GitHub call on this thread until [`Self::finish`]
    /// carries it as `github.caller`.
    pub fn start(mechanism: &'static str) -> Self {
        Self {
            scope: caller_scope::enter(mechanism),
            mechanism,
            started_at: Utc::now(),
            clock: Instant::now(),
        }
    }

    /// End the scope and emit the pass's records for the workspace at `root`.
    /// Nothing is built when no OTLP exporter runs.
    pub fn finish(self, root: &Path, report: &Report) {
        if !crate::observability::ops::spans_exported() {
            return;
        }
        let repo = repo_slug(root);
        let host = crate::observability::ops::global_ops_sink()
            .map_or_else(crate::sweep_registry::host_identity, |s| s.host_id().to_string());
        let heartbeat = Duration::from_secs(
            std::env::var(HEARTBEAT_ENV)
                .ok()
                .and_then(|v| v.trim().parse().ok())
                .unwrap_or(DEFAULT_HEARTBEAT_SECS),
        );
        for record in self.records(&repo, &host, report, heartbeat) {
            crate::observability::ops::emit_record(record);
        }
    }

    /// End the scope and build the records: the `pass.summary` first, then
    /// the `pass.verdict`s the heartbeat lets through, in decision order.
    pub(crate) fn records(
        self,
        repo: &str,
        host: &str,
        report: &Report,
        heartbeat: Duration,
    ) -> Vec<TelemetryRecord> {
        let elapsed = self.clock.elapsed();
        let spent = self.scope.finish();
        let ended_at = self.started_at
            + chrono::Duration::from_std(elapsed).unwrap_or_else(|_| chrono::Duration::zero());
        let mode = if report.dry_run {
            PassMode::DryRun
        } else {
            PassMode::On
        };
        let pass_id = crate::telemetry::trace::derived_hex(
            &[
                "loom.pass",
                self.mechanism,
                host,
                repo,
                &crate::telemetry::trace::instant(self.started_at),
            ],
            32,
        );
        let completed = !report.archived && report.enumerate_error.is_none();
        let emitted = with_seen(|seen| {
            select(&report.items, repo, mode, completed, seen, heartbeat, Instant::now())
        });
        let verdicts: Vec<TelemetryRecord> = emitted
            .iter()
            .map(|&i| {
                TelemetryRecord::PassVerdict(verdict_record(
                    &report.items[i],
                    &pass_id,
                    self.mechanism,
                    repo,
                    mode,
                    ended_at,
                ))
            })
            .collect();
        let summary = PassSummaryRecord {
            pass_id: pass_id.clone(),
            mechanism: self.mechanism.to_string(),
            repo: repo.to_string(),
            host: host.to_string(),
            mode,
            outcome: if report.archived {
                PassOutcome::Archived
            } else if report.enumerate_error.is_some() {
                PassOutcome::Refused
            } else {
                PassOutcome::Completed
            },
            refusal: report
                .enumerate_error
                .as_deref()
                .and_then(crate::forge_call_stats::sanitize)
                .map(|r| r.chars().take(MAX_REFUSAL).collect()),
            started_at: self.started_at,
            ended_at,
            duration_ms: u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX),
            examined: report.examined as u64,
            verdicts: verdict_counts(report),
            skipped: report
                .skipped
                .iter()
                .map(|(k, v)| (k.clone(), *v as u64))
                .collect(),
            write_cap_hit: report.skipped.contains_key(&Skip::WriteCap.key()),
            github: GithubSpend {
                calls: spent.calls,
                writes: spent.writes,
                not_modified: spent.not_modified,
            },
            verdicts_emitted: verdicts.len() as u64,
            verdicts_unchanged: (report.items.len() - verdicts.len()) as u64,
            loom: Provenance::current(),
        };
        let mut records = vec![TelemetryRecord::PassSummary(summary)];
        records.extend(verdicts);
        records
    }
}

/// The workspace's `owner/repo` (`LOOM_REPO`, else the `origin` remote, the
/// slug the pass's own forges use), or [`REPO_UNRESOLVED`]. A local git read,
/// never a forge call.
fn repo_slug(root: &Path) -> String {
    let repo = std::env::var("LOOM_REPO")
        .ok()
        .filter(|r| !r.trim().is_empty());
    crate::forge_etag_store::resolve_target(Some(root), repo.as_deref())
        .repo
        .unwrap_or_else(|| REPO_UNRESOLVED.to_string())
}

/// Artifacts per verdict, every verdict present.
fn verdict_counts(report: &Report) -> BTreeMap<String, u64> {
    let mut counts: BTreeMap<String, u64> = ItemVerdict::ALL
        .iter()
        .map(|v| (v.as_str().to_string(), 0))
        .collect();
    for item in &report.items {
        *counts.entry(item.verdict.as_str().to_string()).or_default() += 1;
    }
    counts
}

fn verdict_record(
    item: &Item,
    pass_id: &str,
    mechanism: &str,
    repo: &str,
    mode: PassMode,
    at: DateTime<Utc>,
) -> PassVerdictRecord {
    PassVerdictRecord {
        pass_id: pass_id.to_string(),
        mechanism: mechanism.to_string(),
        role: None,
        repo: repo.to_string(),
        number: item.number,
        artifact: item.artifact.to_string(),
        verdict: item.verdict.as_str().to_string(),
        reason: item.reason.clone(),
        detail: item.detail.clone(),
        blockers: item
            .blockers
            .iter()
            .map(|b| BlockerState {
                reference: b.reference.clone(),
                state: b.state.to_string(),
            })
            .collect(),
        labels_added: item.labels_added.clone(),
        labels_removed: item.labels_removed.clone(),
        mode,
        applied: item.applied,
        at,
    }
}

/// What a verdict says, for "has it changed since it was last emitted?".
fn fingerprint(item: &Item, mode: PassMode) -> String {
    let blockers: Vec<String> = item
        .blockers
        .iter()
        .map(|b| format!("{}={}", b.reference, b.state))
        .collect();
    format!(
        "{}|{}|{}|{}",
        item.verdict.as_str(),
        item.reason.as_deref().unwrap_or_default(),
        blockers.join(","),
        mode.as_str()
    )
}

/// The last verdict emitted per `(repo, number)`, and when.
type Seen = HashMap<(String, u64), (String, Instant)>;

/// The indexes of `items` to emit (see the module docs), updating `seen`.
/// After a `completed` pass, entries for artifacts of `repo` the pass no
/// longer listed are dropped, so the map tracks only what is still blocked.
fn select(
    items: &[Item],
    repo: &str,
    mode: PassMode,
    completed: bool,
    seen: &mut Seen,
    heartbeat: Duration,
    now: Instant,
) -> Vec<usize> {
    let mut out = Vec::new();
    for (i, item) in items.iter().enumerate() {
        let key = (repo.to_string(), item.number);
        let print = fingerprint(item, mode);
        let wrote = mode == PassMode::On
            && matches!(
                item.verdict,
                ItemVerdict::Released | ItemVerdict::Reparked | ItemVerdict::Failed
            );
        let due = match seen.get(&key) {
            None => true,
            Some((last, at)) => {
                *last != print || heartbeat.is_zero() || now.duration_since(*at) >= heartbeat
            }
        };
        if wrote || due {
            seen.insert(key, (print, now));
            out.push(i);
        }
    }
    if completed {
        let listed: HashSet<u64> = items.iter().map(|i| i.number).collect();
        seen.retain(|(r, n), _| r != repo || listed.contains(n));
    }
    out
}

#[cfg(not(test))]
fn with_seen<R>(f: impl FnOnce(&mut Seen) -> R) -> R {
    use std::sync::{Mutex, OnceLock};
    static SEEN: OnceLock<Mutex<Seen>> = OnceLock::new();
    let lock = SEEN.get_or_init(|| Mutex::new(HashMap::new()));
    let mut guard = lock
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    f(&mut guard)
}

/// Test builds: per thread, so parallel tests never share emission state.
#[cfg(test)]
fn with_seen<R>(f: impl FnOnce(&mut Seen) -> R) -> R {
    thread_local! {
        static SEEN: std::cell::RefCell<Seen> = std::cell::RefCell::new(HashMap::new());
    }
    SEEN.with(|s| f(&mut s.borrow_mut()))
}

#[cfg(test)]
#[path = "release_telemetry_tests.rs"]
mod tests;
