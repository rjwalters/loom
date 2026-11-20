//! The fleet token-probe snapshot: the captain publishes its account probe,
//! workers apply it (issue #13942, deliverable 2 of #13893).
//!
//! Token exhaustion state is per host: each worker re-probes accounts on its
//! own schedule and keeps its own `.ranking`, so two hosts can disagree about
//! the same pool for a whole probe cadence (the 2026-11-14 evidence in
//! #13893: four workers, four different usable-account counts, 12 minutes
//! after an operator-bought reset). The channel this module rides is the
//! fleet-dispatch heartbeat the captain already answers: the captain reads
//! the ranking-refresh result it already writes (`.ranking`, refreshed by
//! `token_ranking_refresh`) into one snapshot, serves it as an optional field
//! on `HeartbeatResponse` (`fleet_dispatch::proto`), and every worker that
//! receives it stores it here as a sidecar beside the pool. No token
//! material ever moves — the snapshot carries exactly the fields `.ranking`
//! already holds, per account: name, status, 5h utilization, limit reset.
//!
//! # Why a sidecar, not a `.ranking` column
//!
//! The #9005 ruling, same as [`super::ranking_weekly`]:
//! `select::parse_ranking_line` splits on `splitn(4, '|')`, so a fifth
//! column is swallowed into `limit_reset` by every reader that has not been
//! upgraded — and `.ranking` is the *shared* machine-level pool, read by
//! every daemon on the host whatever its version. The fleet snapshot is
//! published fleet-wide but each host still owns its local `.ranking`, so a
//! fleet rollout must never depend on every reader understanding a new
//! column.
//!
//! # Freshness: newest probe wins
//!
//! The evidence readers in [`super::bad_tokens`] consult this sidecar *in
//! addition to* `.ranking` and prefer whichever probe is newer
//! ([`evidence_row`]): the sidecar carries its probe instant
//! (`probed_at`, from the captain's `.ranking` mtime), and a local `.ranking`
//! refreshed after that instant still wins — so a host that keeps its own
//! probe loop running (or an operator's manual `tokens check --ranking`)
//! behaves exactly as before, and the fleet snapshot only fills gaps and
//! converges fresh workers. A snapshot older than the mark it is weighed
//! against is ignored (the readers' `probed_at > marked_at` gate).
//!
//! # Absent, never fabricated
//!
//! Every read failure (file absent, unparseable, wrong schema) yields `None`
//! — "unknown", not a zero or an empty verdict — the same contract
//! [`super::ranking_weekly`] holds.

use std::path::Path;

use chrono::{DateTime, Utc};

use super::monitor;

/// Sidecar file name, beside `.ranking` in the pool directory.
pub const SIDECAR_FILE_NAME: &str = ".fleet_probe.json";
/// On-disk schema version of the sidecar payload.
const SIDECAR_SCHEMA: i64 = 1;

/// One account's probe verdict, as the captain publishes it: exactly the
/// fields a `.ranking` row already carries ([`super::select::parse_ranking_line`]).
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct TokenProbeAccount {
    /// Account name (the `.token` file stem).
    pub name: String,
    /// The probe's verdict (`available`, `exhausted`, `error`, …).
    pub status: String,
    /// 5h-window utilization (0..=1), when the probe measured one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub util_5h: Option<f64>,
    /// When the account's binding limit window resets (ISO-8601 text, the
    /// raw field `.ranking` holds), when the probe recorded one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit_reset: Option<String>,
}

/// The captain's whole-pool probe snapshot: every account, one probe instant.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct TokenProbeSnapshot {
    /// When the captain's probe ran (its `.ranking` mtime at publish time).
    /// The worker-side freshness currency: a newer local `.ranking` mtime
    /// still outranks the snapshot (see [`evidence_row`]).
    pub probed_at: DateTime<Utc>,
    /// One row per account, `.ranking` order.
    pub accounts: Vec<TokenProbeAccount>,
}

impl TokenProbeSnapshot {
    /// The row for `name`, when the snapshot carries one.
    #[must_use]
    pub fn account(&self, name: &str) -> Option<&TokenProbeAccount> {
        self.accounts.iter().find(|a| a.name == name)
    }
}

/// Build the snapshot from the captain's own pool: every parseable
/// `.ranking` row, stamped with the file's mtime as the probe instant (the
/// same freshness fact [`super::bad_tokens`]'s readers already use).
/// `None` when `.ranking` is unreadable — there is nothing to publish, and
/// the heartbeat answer omits the field so workers keep their own evidence.
#[must_use]
pub fn snapshot_from_tokens_dir(tokens_dir: &Path) -> Option<TokenProbeSnapshot> {
    let ranking_path = tokens_dir.join(".ranking");
    let meta = std::fs::metadata(&ranking_path).ok()?;
    let modified = meta.modified().ok()?;
    let probed_at: DateTime<Utc> = modified.into();
    let text = std::fs::read_to_string(&ranking_path).ok()?;
    let accounts: Vec<TokenProbeAccount> = text
        .lines()
        .filter_map(super::select::parse_ranking_line)
        .map(|row| TokenProbeAccount {
            name: row.name,
            status: row.status,
            util_5h: row.util_5h,
            limit_reset: row.limit_reset,
        })
        .collect();
    Some(TokenProbeSnapshot {
        probed_at,
        accounts,
    })
}

/// The sidecar payload: the snapshot plus a schema stamp.
#[derive(serde::Serialize, serde::Deserialize)]
struct SidecarFile {
    schema: i64,
    probed_at: DateTime<Utc>,
    accounts: Vec<TokenProbeAccount>,
}

/// Store `snapshot` beside the pool as the fleet sidecar, atomically
/// (temp file + rename, the writer every other pool bookkeeping file uses).
///
/// # Errors
/// Propagates the filesystem error (the caller logs and moves on — a worker
/// that cannot persist a snapshot keeps its local evidence and retries on
/// the next heartbeat).
pub fn write_sidecar(tokens_dir: &Path, snapshot: &TokenProbeSnapshot) -> std::io::Result<()> {
    let payload = SidecarFile {
        schema: SIDECAR_SCHEMA,
        probed_at: snapshot.probed_at,
        accounts: snapshot.accounts.clone(),
    };
    let text = serde_json::to_string(&payload)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    monitor::write_ranking_text(&text, &tokens_dir.join(SIDECAR_FILE_NAME))
}

/// Read the fleet sidecar [`write_sidecar`] wrote. `None` when the file is
/// absent, unreadable, not valid JSON, or not this schema — "unknown", never
/// a fabricated verdict.
#[must_use]
pub fn read_sidecar(tokens_dir: &Path) -> Option<TokenProbeSnapshot> {
    let raw = std::fs::read_to_string(tokens_dir.join(SIDECAR_FILE_NAME)).ok()?;
    let file: SidecarFile = serde_json::from_str(&raw).ok()?;
    if file.schema != SIDECAR_SCHEMA {
        return None;
    }
    Some(TokenProbeSnapshot {
        probed_at: file.probed_at,
        accounts: file.accounts,
    })
}

/// What [`apply_snapshot`] did with an incoming snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApplyOutcome {
    /// Stored: the incoming probe is newer than the one on disk.
    Applied,
    /// Nothing written: the sidecar already holds this exact probe cycle.
    Unchanged,
    /// Nothing written: the incoming probe is older than the one on disk
    /// (a stale answer racing a newer heartbeat, or a captain rollback) —
    /// or the write itself failed, in which case the local evidence simply
    /// stands and the next heartbeat retries.
    Stale,
}

/// A worker's heartbeat answer carried `snapshot`: store it as this host's
/// fleet sidecar when it is not older than what is already there, and emit
/// the per-host applied-snapshot-age gauge (`loom.tokens.fleet_probe_age`,
/// sample-time minus the captain's `probed_at`) either way — the drift
/// signal the rollout check reads. Signed on purpose: a small negative value
/// is clock skew between the captain and this host, not a wrapped counter.
///
/// Never propagates errors: a worker that cannot write the sidecar keeps
/// its local evidence and tries again on the next heartbeat.
pub fn apply_snapshot(
    tokens_dir: &Path,
    snapshot: &TokenProbeSnapshot,
    now: DateTime<Utc>,
) -> ApplyOutcome {
    let outcome = match read_sidecar(tokens_dir) {
        Some(current) if current.probed_at > snapshot.probed_at => ApplyOutcome::Stale,
        Some(current) if current.probed_at == snapshot.probed_at => ApplyOutcome::Unchanged,
        _ => {
            if let Err(error) = write_sidecar(tokens_dir, snapshot) {
                log::warn!(
                    "tokens_pool: could not store the fleet token probe ({}): {error}; keeping \
                     local evidence",
                    tokens_dir.join(SIDECAR_FILE_NAME).display()
                );
                return ApplyOutcome::Stale;
            }
            ApplyOutcome::Applied
        }
    };
    crate::observability::ops::emit_metrics(vec![crate::telemetry::ops::MetricPoint::int(
        crate::telemetry::ops::MetricName::TokensFleetProbeAge,
        (now - snapshot.probed_at).num_seconds(),
    )]);
    outcome
}

/// One account's probe evidence, whichever probe is freshest: the local
/// `.ranking` row (probe instant = file mtime) or the fleet sidecar's row
/// (probe instant = the captain's `probed_at`). The sidecar is consulted
/// only when it is **strictly newer** — a locally refreshed `.ranking`
/// outranks a same-instant snapshot, so a host that probes its own pool
/// keeps byte-for-byte today's behaviour (#13942's fail-open contract).
///
/// `None` when neither source has a row for the account.
#[must_use]
pub(crate) fn evidence_row(tokens_dir: &Path, name: &str) -> Option<EvidenceRow> {
    let local = ranking_row(tokens_dir, name);
    let fleet = read_sidecar(tokens_dir).and_then(|snapshot| {
        snapshot.account(name).map(|account| EvidenceRow {
            status: account.status.clone(),
            util_5h: account.util_5h,
            limit_reset: account.limit_reset.clone(),
            // The captain's probe instant as it recorded it — not this
            // file's local mtime (the worker received it up to one
            // heartbeat later), so the newest-probe comparison weighs the
            // probes against each other, not the file writes.
            probed_at: snapshot.probed_at.timestamp(),
        })
    });
    match (local, fleet) {
        (Some(local), Some(fleet)) => (fleet.probed_at > local.probed_at)
            .then_some(fleet)
            .or(Some(local)),
        (local, fleet) => local.or(fleet),
    }
}

/// The local `.ranking` row for `name`, with the file's mtime as the probe
/// instant — the exact freshness fact [`super::bad_tokens`] read directly
/// before #13942.
fn ranking_row(tokens_dir: &Path, name: &str) -> Option<EvidenceRow> {
    let ranking_path = tokens_dir.join(".ranking");
    let meta = std::fs::metadata(&ranking_path).ok()?;
    let probed_at = meta
        .modified()
        .ok()
        .and_then(|m| m.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_secs() as i64);
    let text = std::fs::read_to_string(&ranking_path).ok()?;
    let row = text
        .lines()
        .filter_map(super::select::parse_ranking_line)
        .find(|r| r.name == name)?;
    Some(EvidenceRow {
        status: row.status,
        util_5h: row.util_5h,
        limit_reset: row.limit_reset,
        probed_at,
    })
}

/// One account's probe evidence from either source (see [`evidence_row`]).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct EvidenceRow {
    pub status: String,
    pub util_5h: Option<f64>,
    pub limit_reset: Option<String>,
    /// Unix seconds of the probe this row came from.
    pub probed_at: i64,
}

/// The captain-side per-account, per-cycle record (#13942): one point per
/// account per distinct probe instant, labelled with the account and its
/// verdict — what the captain published, queryable per cycle. Emitted by the
/// fleet-dispatch server when it hands a heartbeat a cycle it has not yet
/// recorded (see `Server::token_probe_snapshot`).
#[must_use]
pub fn published_cycle_points(
    snapshot: &TokenProbeSnapshot,
) -> Vec<crate::telemetry::ops::MetricPoint> {
    snapshot
        .accounts
        .iter()
        .map(|account| {
            crate::telemetry::ops::MetricPoint::int(
                crate::telemetry::ops::MetricName::TokensFleetProbeAccounts,
                1,
            )
            .label("account", account.name.clone())
            .label("state", account.status.clone())
        })
        .collect()
}

#[cfg(test)]
#[path = "fleet_probe_tests.rs"]
mod tests;
