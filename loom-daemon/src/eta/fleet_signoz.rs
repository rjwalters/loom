//! Fleet-wide, SigNoz-sourced **in-sweep** history (#9758) — the second half
//! of #9343's fleet history.
//!
//! # Why
//!
//! [`super::fleet`]'s forge snapshot covers the human-gated stages a PR label
//! timeline can see. It cannot see inside a sweep, so at `historyScope = fleet`
//! `finish-v1` had no `sweep.curator` / `sweep.builder` samples and no in-sweep
//! merge share, and refused. Every fleet host already exports its
//! `sweep.outcome` records to SigNoz; this module turns those records, read
//! back, into one cached snapshot per repo that every host can hold
//! byte-identically.
//!
//! # Shape (mirrors [`super::fleet`])
//!
//! - **Purity.** Nothing here reads the network. [`parse_row`] takes one row of
//!   text a reader already fetched ([`super::fleet_signoz_refresh`] does the
//!   fetching, from the CLI or the daemon's refresh task) and
//!   [`SignozSnapshot::stage_samples`] hands the estimator an ordinary
//!   [`StageSamples`] value with `scope = fleet`.
//! - **Determinism.** Outcomes are deduplicated and stored in a canonical
//!   order, and the id is derived from `(repo, since, as_of, every outcome)`.
//!   No fetch time, build stamp or reading host is in the file.
//! - **Attribution.** Every sample keeps the **real** host that recorded the
//!   sweep (unlike a forge sample's [`super::fleet::FORGE_HOST`]) and the
//!   [`SampleSource::SignozOutcome`] source, which a `SweepOutcome` filter
//!   admits and a `StageJournal` filter does not ([`SampleSource::admits`]).
//!
//! # Record identity and deduplication
//!
//! Delivery is at least once (`telemetry-replay.md` § "Identity and dedupe"),
//! so a record can appear more than once: retried exports, overlapping pages,
//! a re-export by `observability backfill`. Two identities are applied, in
//! this order:
//!
//! 1. **Delivery identity** — `loom.record_id`, the content-derived id every
//!    exported log record carries. Equal ids are the same envelope; one is
//!    kept.
//! 2. **Logical identity** — `(host.id, loom.sweep_id)`. Two *different*
//!    envelopes for one sweep on one host (a re-serialisation by a newer
//!    build changes the content hash) still describe one sweep; the one that
//!    became knowable **first** is kept (ties by record id), which is also the
//!    only choice that is correct for every replay instant.
//!
//! `historyScope = augment` applies the logical identity across sources too:
//! a sweep this host journalled locally *and* exported is counted once, from
//! the local journal ([`super::fleet::apply_scope`]).
//!
//! # Knowable-at (the leak-freedom contract, #10196)
//!
//! A sample's `observed_at` is the row's **knowable-at** column,
//! `observed_timestamp` — never the event time (`timestamp`).
//! `telemetry-replay.md` makes that the replay rule and records its current
//! limitation, which this reader inherits rather than hides: today the
//! exporter fills `observed_timestamp` with the producer's own `emitted_at`, so
//! it is a **lower bound** on when the row actually became readable, and a
//! replay over this snapshot is point-in-time correct only up to the export
//! latency. When the collector-side receive stamp lands it arrives in the same
//! column and this reader needs no change.
//!
//! Admission rule (conservative): a row with no knowable-at, or one whose
//! knowable-at precedes its own event time (impossible for a genuine
//! observation), is **rejected**, never back-filled from the event time.
//!
//! # What a record contributes
//!
//! The same conversion as this host's own journal
//! ([`StageSamples::push_outcome_facts`]): phase durations and, for a
//! successful sweep, whether it merged itself — **but no Judge verdicts**. A
//! `sweep.outcome`'s verdicts are themselves reconstructed from the forge
//! label timeline (#8222), which the forge snapshot already carries, so
//! taking them here would count every fleet verdict twice.
//!
//! A row without `loom.phase_durations` is rejected rather than read as "no
//! phases": the exporter omits that attribute both when a sweep had none and
//! when the list was invalid or oversized, and only the first reading would
//! be honest. Nothing is ever inferred from `loom.phase` spans.

use super::explanation::HistoryScope;
use super::fleet::{self, RETENTION_DAYS};
use super::history::{OutcomeFacts, SampleSource, StageSamples};
// The row paging cursor is owned by the neutral SigNoz read client (#10196 R6).
use crate::signoz_read::RowCursor;
use crate::telemetry::{PhaseDuration, SweepResult};
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

/// Schema tag of one SigNoz snapshot file.
pub const SIGNOZ_SNAPSHOT_SCHEMA: &str = "eta-fleet-signoz-snapshot/v1";

/// Why a well-formed row was not admitted. A closed vocabulary: every
/// rejection is counted under exactly one of these.
pub mod reject {
    pub const MISSING_RECORD_ID: &str = "missing_record_id";
    pub const MISSING_HOST: &str = "missing_host";
    pub const MISSING_SWEEP_ID: &str = "missing_sweep_id";
    pub const REPO_MISMATCH: &str = "repo_mismatch";
    pub const UNKNOWN_RESULT: &str = "unknown_result";
    pub const MISSING_TOTAL_DURATION: &str = "missing_total_duration";
    pub const PHASES_ABSENT: &str = "phases_absent";
    pub const PHASES_INVALID: &str = "phases_invalid";
    pub const MISSING_EVENT_TIME: &str = "missing_event_time";
    pub const MISSING_KNOWABLE_TIME: &str = "missing_knowable_time";
    pub const KNOWABLE_BEFORE_EVENT: &str = "knowable_before_event";
}

/// One phase of one exported sweep.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct SignozPhase {
    /// Lifecycle phase name (`curator`, `builder`, `judge`, …).
    pub phase: String,
    /// Whole seconds, never negative.
    pub duration_sec: i64,
    /// 1-based attempt at this phase, when the producer recorded it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attempt: Option<u32>,
}

/// One admitted `sweep.outcome`, as read back from SigNoz.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignozOutcome {
    /// `loom.record_id`: the delivery identity.
    pub record_id: String,
    /// `host.id`: the host that ran and recorded the sweep.
    pub host: String,
    /// `loom.sweep_id`.
    pub sweep_id: String,
    /// `loom.result`.
    pub result: SweepResult,
    /// `loom.total_duration_sec`.
    pub total_duration_sec: i64,
    /// `loom.phase_durations`, in record order.
    pub phases: Vec<SignozPhase>,
    /// The record's event time (`timestamp`). Kept for forensics; it decides
    /// nothing.
    pub event_at: DateTime<Utc>,
    /// The record's knowable-at (`observed_timestamp`): the instant every
    /// sample it contributes is observed at.
    pub knowable_at: DateTime<Utc>,
}

impl SignozOutcome {
    /// `(host, sweep_id)`: the logical identity.
    #[must_use]
    pub fn logical_key(&self) -> (String, String) {
        (self.host.clone(), self.sweep_id.clone())
    }

    fn canonical_key(&self) -> (DateTime<Utc>, &str, &str, &str) {
        (self.knowable_at, &self.host, &self.sweep_id, &self.record_id)
    }

    fn digest_line(&self) -> String {
        let phases: Vec<String> = self
            .phases
            .iter()
            .map(|p| {
                format!(
                    "{}:{}:{}",
                    p.phase,
                    p.duration_sec,
                    p.attempt.map(|a| a.to_string()).unwrap_or_default()
                )
            })
            .collect();
        format!(
            "outcome|{}|{}|{}|{}|{}|{}|{}|{}",
            self.record_id,
            self.host,
            self.sweep_id,
            result_str(self.result),
            self.total_duration_sec,
            crate::telemetry::trace::instant(self.event_at),
            crate::telemetry::trace::instant(self.knowable_at),
            phases.join(","),
        )
    }

    fn phase_durations(&self) -> Vec<PhaseDuration> {
        self.phases
            .iter()
            .map(|p| {
                let mut entry = PhaseDuration::new(p.phase.clone(), p.duration_sec);
                entry.attempt = p.attempt;
                entry
            })
            .collect()
    }
}

fn result_str(result: SweepResult) -> &'static str {
    match result {
        SweepResult::Success => "success",
        SweepResult::Failure => "failure",
        SweepResult::Cancelled => "cancelled",
        SweepResult::Blocked => "blocked",
    }
}

/// What one row of the query's output parsed to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParsedRow {
    /// Admitted.
    Outcome(SignozOutcome),
    /// Well-formed but not admissible, for the named [`reject`] reason.
    Rejected(&'static str),
}

/// Parse one JSONEachRow line of the outcomes query for `repo`.
///
/// `Err` means the **response** is invalid (not a JSON object, or the paging
/// columns are unreadable): the caller must fail the whole fetch, because a
/// reader cannot know what else in such a response is wrong. `Ok(Rejected)`
/// is one record that is readable but not admissible; it is counted and
/// skipped and the fetch continues.
///
/// # Errors
///
/// The line is not a JSON object, or its `knowable_time_ns` / `record_id`
/// paging columns are missing or of the wrong type.
pub fn parse_row(line: &str, repo: &str) -> Result<(RowCursor, ParsedRow), String> {
    let value: Value = serde_json::from_str(line).map_err(|e| format!("row is not JSON: {e}"))?;
    let Value::Object(row) = value else {
        return Err("row is not a JSON object".to_string());
    };
    let knowable_ns = match row.get("knowable_time_ns") {
        Some(v) => int(v).ok_or("knowable_time_ns is not an integer")?,
        None => return Err("row has no knowable_time_ns column".to_string()),
    };
    let record_id = match row.get("record_id") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Null) => String::new(),
        _ => return Err("row has no record_id column".to_string()),
    };
    let cursor = (knowable_ns, record_id.clone());
    Ok((cursor, admit(&row, record_id, knowable_ns, repo)))
}

fn admit(
    row: &serde_json::Map<String, Value>,
    record_id: String,
    knowable_ns: i64,
    repo: &str,
) -> ParsedRow {
    use ParsedRow::Rejected;
    let text = |key: &str| {
        row.get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    if record_id.trim().is_empty() {
        return Rejected(reject::MISSING_RECORD_ID);
    }
    let Some(host) = text("host_id") else {
        return Rejected(reject::MISSING_HOST);
    };
    let Some(sweep_id) = text("sweep_id") else {
        return Rejected(reject::MISSING_SWEEP_ID);
    };
    if !text("repo").is_some_and(|r| r.eq_ignore_ascii_case(repo)) {
        return Rejected(reject::REPO_MISMATCH);
    }
    let result = match text("result").as_deref() {
        Some("success") => SweepResult::Success,
        Some("failure") => SweepResult::Failure,
        Some("cancelled") => SweepResult::Cancelled,
        Some("blocked") => SweepResult::Blocked,
        _ => return Rejected(reject::UNKNOWN_RESULT),
    };
    let Some(total_duration_sec) = row
        .get("total_duration_sec")
        .and_then(int)
        .filter(|t| *t >= 0)
    else {
        return Rejected(reject::MISSING_TOTAL_DURATION);
    };
    let phases = match row.get("phase_durations") {
        None | Some(Value::Null) => return Rejected(reject::PHASES_ABSENT),
        Some(v) => match phases(v) {
            Some(phases) => phases,
            None => return Rejected(reject::PHASES_INVALID),
        },
    };
    let Some(event_at) = row
        .get("event_time_ns")
        .and_then(int)
        .filter(|ns| *ns > 0)
        .map(DateTime::from_timestamp_nanos)
    else {
        return Rejected(reject::MISSING_EVENT_TIME);
    };
    if knowable_ns <= 0 {
        return Rejected(reject::MISSING_KNOWABLE_TIME);
    }
    let knowable_at = DateTime::from_timestamp_nanos(knowable_ns);
    if knowable_at < event_at {
        return Rejected(reject::KNOWABLE_BEFORE_EVENT);
    }
    ParsedRow::Outcome(SignozOutcome {
        record_id,
        host,
        sweep_id,
        result,
        total_duration_sec,
        phases,
        event_at,
        knowable_at,
    })
}

/// An integer that ClickHouse may render as a JSON number or, for 64-bit
/// columns, a quoted string.
fn int(value: &Value) -> Option<i64> {
    match value {
        Value::Number(n) => n.as_i64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

/// `loom.phase_durations`: a JSON array of `{phase, duration_sec, attempt?}`,
/// either inline or as the JSON-encoded string SigNoz stores attributes as.
/// `None` on any malformed entry — a partly readable list would silently
/// shorten the sweep.
fn phases(value: &Value) -> Option<Vec<SignozPhase>> {
    let parsed;
    let list = match value {
        Value::String(s) => {
            parsed = serde_json::from_str::<Value>(s).ok()?;
            parsed.as_array()?
        }
        Value::Array(list) => list,
        _ => return None,
    };
    list.iter()
        .map(|entry| {
            let phase = entry
                .get("phase")?
                .as_str()
                .filter(|p| !p.trim().is_empty())?;
            let duration_sec = int(entry.get("duration_sec")?).filter(|d| *d >= 0)?;
            let attempt = match entry.get("attempt") {
                None | Some(Value::Null) => None,
                Some(v) => Some(u32::try_from(int(v)?).ok().filter(|a| *a > 0)?),
            };
            Some(SignozPhase {
                phase: phase.to_string(),
                duration_sec,
                attempt,
            })
        })
        .collect()
}

/// What [`SignozSnapshot::build`] dropped while canonicalising.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BuildStats {
    /// Repeats of an already-held `record_id`.
    pub duplicate_records: usize,
    /// Distinct records for an already-held `(host, sweep_id)`.
    pub duplicate_sweeps: usize,
    /// Knowable outside `[since, as_of]`.
    pub outside_window: usize,
}

/// A fleet-wide in-sweep history for one repo, as of one instant.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignozSnapshot {
    /// Always [`SIGNOZ_SNAPSHOT_SCHEMA`].
    pub schema: String,
    /// Derived from `(repo, since, as_of, every outcome)`, never random.
    pub snapshot_id: String,
    /// `owner/repo`.
    pub repo: String,
    /// The oldest knowable-at the snapshot covers.
    pub since: DateTime<Utc>,
    /// The instant the snapshot describes: nothing in it became knowable
    /// after this.
    pub as_of: DateTime<Utc>,
    /// Every admitted outcome, deduplicated, in canonical order.
    pub outcomes: Vec<SignozOutcome>,
}

impl SignozSnapshot {
    /// The window a snapshot as of `as_of` covers: [`RETENTION_DAYS`] back,
    /// the same horizon the forge snapshot keeps, so a backtest replaying an
    /// instant one window ago still has a whole window behind it.
    #[must_use]
    pub fn window_start(as_of: DateTime<Utc>) -> DateTime<Utc> {
        as_of
            .checked_sub_signed(Duration::days(RETENTION_DAYS))
            .unwrap_or(DateTime::<Utc>::MIN_UTC)
    }

    /// Canonicalise `outcomes` into a sealed snapshot of `repo` covering
    /// `[since, as_of]`. Input order does not matter.
    #[must_use]
    pub fn build(
        repo: &str,
        since: DateTime<Utc>,
        as_of: DateTime<Utc>,
        mut outcomes: Vec<SignozOutcome>,
    ) -> (Self, BuildStats) {
        let mut stats = BuildStats::default();
        let before = outcomes.len();
        outcomes.retain(|o| o.knowable_at >= since && o.knowable_at <= as_of);
        stats.outside_window = before - outcomes.len();
        // Earliest knowable first, so "keep the first" keeps the earliest.
        outcomes.sort_by(|a, b| {
            (a.knowable_at, &a.record_id, a.digest_line()).cmp(&(
                b.knowable_at,
                &b.record_id,
                b.digest_line(),
            ))
        });
        let mut records = BTreeSet::new();
        let mut sweeps = BTreeSet::new();
        let mut kept = Vec::with_capacity(outcomes.len());
        for outcome in outcomes {
            if !records.insert(outcome.record_id.clone()) {
                stats.duplicate_records += 1;
                continue;
            }
            if !sweeps.insert(outcome.logical_key()) {
                stats.duplicate_sweeps += 1;
                continue;
            }
            kept.push(outcome);
        }
        let mut snapshot = SignozSnapshot {
            schema: SIGNOZ_SNAPSHOT_SCHEMA.to_string(),
            snapshot_id: String::new(),
            repo: repo.to_string(),
            since,
            as_of,
            outcomes: kept,
        };
        snapshot.seal();
        (snapshot, stats)
    }

    fn seal(&mut self) {
        self.schema = SIGNOZ_SNAPSHOT_SCHEMA.to_string();
        self.outcomes
            .sort_by(|a, b| a.canonical_key().cmp(&b.canonical_key()));
        let lines: Vec<String> = self
            .outcomes
            .iter()
            .map(SignozOutcome::digest_line)
            .collect();
        let repo = self.repo.to_ascii_lowercase();
        let since = crate::telemetry::trace::instant(self.since);
        let at = crate::telemetry::trace::instant(self.as_of);
        let mut parts: Vec<&str> = vec!["loom.eta.fleet.signoz", &repo, &since, &at];
        parts.extend(lines.iter().map(String::as_str));
        self.snapshot_id = crate::telemetry::trace::derived_hex(&parts, 16);
    }

    /// This snapshot as estimator input: `scope = Fleet`, every sample
    /// attributed to [`SampleSource::SignozOutcome`] and to the host that
    /// recorded it, observed at its knowable-at.
    ///
    /// An outcome whose `(host, sweep_id)` is in `exclude` contributes
    /// nothing — how `augment` keeps a locally journalled sweep from counting
    /// twice. Judge verdicts are never taken (see the module docs).
    #[must_use]
    pub fn stage_samples(&self, exclude: &BTreeSet<(String, String)>) -> StageSamples {
        let mut history = StageSamples {
            scope: HistoryScope::Fleet,
            ..StageSamples::default()
        };
        for outcome in &self.outcomes {
            if exclude.contains(&outcome.logical_key()) {
                continue;
            }
            let phases = outcome.phase_durations();
            let facts = OutcomeFacts {
                repo: Some(&self.repo),
                result: outcome.result,
                total_duration_sec: outcome.total_duration_sec,
                phase_durations: &phases,
                judge_verdicts: None,
            };
            history.push_outcome_facts(
                &facts,
                outcome.knowable_at,
                &outcome.host,
                SampleSource::SignozOutcome,
            );
        }
        history
    }

    /// Outcomes per recording host — the `show` census.
    #[must_use]
    pub fn counts_by_host(&self) -> BTreeMap<String, usize> {
        let mut counts = BTreeMap::new();
        for outcome in &self.outcomes {
            *counts.entry(outcome.host.clone()).or_insert(0) += 1;
        }
        counts
    }

    /// Completed stage samples per stage name — what tells an operator
    /// whether `finish` has enough evidence to answer.
    #[must_use]
    pub fn counts_by_stage(&self) -> BTreeMap<String, usize> {
        let mut counts = BTreeMap::new();
        for sample in &self.stage_samples(&BTreeSet::new()).stages {
            *counts.entry(sample.stage.as_str().to_string()).or_insert(0) += 1;
        }
        counts
    }
}

/// `<snapshot dir>/signoz`: beside the forge snapshots, in a subdirectory
/// [`fleet::load_all`] never lists (it reads `*.json` files only).
#[must_use]
pub fn signoz_dir(workspace_root: &Path) -> PathBuf {
    fleet::snapshot_dir(workspace_root).join("signoz")
}

/// The cached SigNoz snapshot path for `repo`.
#[must_use]
pub fn signoz_path(workspace_root: &Path, repo: &str) -> PathBuf {
    signoz_dir(workspace_root).join(format!("{}.json", fleet::snapshot_slug(repo)))
}

/// Read the snapshot at `path`. `None` when absent, unreadable or of another
/// schema — an unknown schema is a refusal, never a partial parse.
#[must_use]
pub fn read(path: &Path) -> Option<SignozSnapshot> {
    let text = std::fs::read_to_string(path).ok()?;
    let snapshot: SignozSnapshot = serde_json::from_str(&text).ok()?;
    (snapshot.schema == SIGNOZ_SNAPSHOT_SCHEMA).then_some(snapshot)
}

/// Write `snapshot` atomically (temp file + rename): a crash mid-write leaves
/// the previous snapshot in service, never half of a new one.
///
/// # Errors
///
/// The directory could not be created or the write/rename failed.
pub fn write(path: &Path, snapshot: &SignozSnapshot) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let text = serde_json::to_string_pretty(snapshot).map_err(std::io::Error::other)?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, format!("{text}\n"))?;
    std::fs::rename(&tmp, path)
}

/// Every cached SigNoz snapshot under `workspace_root`, by file name.
#[must_use]
pub fn load_all(workspace_root: &Path) -> Vec<SignozSnapshot> {
    let Ok(entries) = std::fs::read_dir(signoz_dir(workspace_root)) else {
        return Vec::new();
    };
    let mut paths: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "json"))
        .collect();
    paths.sort();
    paths.iter().filter_map(|p| read(p)).collect()
}
