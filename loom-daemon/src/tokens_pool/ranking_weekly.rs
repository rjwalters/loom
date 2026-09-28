//! Per-account weekly (7-day) utilization sidecar beside `.ranking`
//! (issue #9005).
//!
//! Every `tokens check --ranking` run already knows each Claude account's
//! rolling 7-day utilization ([`AccountResult::s7d_utilization`] — the native
//! probe's `anthropic-ratelimit-*-7d-utilization` header, or claude-monitor's
//! `ranking.json` `utilization.7d` on the monitor short-circuit), but `.ranking`
//! carries only the 5-hour axis. The telemetry collector exports that 5h axis
//! as `loom.tokens.usage_fraction`; this sidecar gives it the weekly axis
//! (`loom.tokens.usage_fraction_weekly`) so SigNoz can chart both.
//!
//! # Why a sidecar, not a fifth `.ranking` column
//!
//! Same reason as [`super::monitor_classes`]: `select::parse_ranking_line`
//! splits on `splitn(4, '|')`, so a fifth column is swallowed into
//! `limit_reset` by every reader that has not been upgraded — and `.ranking`
//! is usually the *shared* machine-level pool, read by every daemon on the
//! host whatever its version. `select_tests.rs`'s
//! `parse_ranking_line_has_no_room_for_a_fifth_per_class_column` pins that
//! ruling. The selector must never consult this data either, so it has no
//! business in the selector's hot-path format.
//!
//! # Freshness: paired with the `.ranking` it was written beside
//!
//! The sidecar is written right after `.ranking` by the same `--ranking` run
//! (see `cli::tokens_weekly_points`), so it describes the same probe. A reader
//! accepts it only while that is still true: when `.ranking` has been rewritten
//! after the sidecar (by a path that does not write this file), the sidecar is
//! describing an older probe and is treated as absent. No fixed age window —
//! the refresh cadence is configurable, and a gauge that flickered out every
//! time a refresh ran late would read as "unknown" for a healthy pool.
//!
//! # Absent, never zero
//!
//! An account whose probe returned no 7d reading (a failed probe, a legacy
//! monitor entry) is simply not written, and every read failure (file absent,
//! unparseable, wrong schema, stale) yields an empty map — the same "unknown,
//! not a fabricated value" contract `.ranking`'s `5h_util` holds.
//!
//! [`AccountResult::s7d_utilization`]: super::check::AccountResult::s7d_utilization

use std::collections::BTreeMap;
use std::path::Path;

use chrono::{DateTime, Utc};

use super::check::ProbeReport;

/// Sidecar file name, beside `.ranking` in the pool directory.
pub const SIDECAR_FILE_NAME: &str = ".ranking.weekly.json";
const SIDECAR_SCHEMA: i64 = 1;
/// Slack between the sidecar's `written_at` (whole seconds) and `.ranking`'s
/// mtime before the sidecar counts as describing an older probe. The pair is
/// written back to back by one process, so this only absorbs timestamp
/// truncation and filesystem mtime granularity.
const PAIRING_SLACK_SECONDS: i64 = 60;

/// Write the weekly sidecar for `report` into `tokens_dir`, atomically.
///
/// Always written — even when no account carries a 7d reading — so a sidecar
/// from an earlier run can never outlive the probe that replaced it.
/// `unsupported` rows are left out for the same reason `.ranking` omits them.
pub fn write_weekly_utilization_sidecar(
    report: &ProbeReport,
    tokens_dir: &Path,
    now: DateTime<Utc>,
) -> std::io::Result<()> {
    let mut by_account = serde_json::Map::new();
    for account in &report.accounts {
        if account.status == "unsupported" {
            continue;
        }
        if let Some(util) = account
            .s7d_utilization
            .filter(|u| u.is_finite() && *u >= 0.0)
        {
            by_account.insert(account.name.clone(), serde_json::json!(util));
        }
    }
    let payload = serde_json::json!({
        "schema": SIDECAR_SCHEMA,
        "written_at": now.format("%Y-%m-%dT%H:%M:%SZ").to_string(),
        "accounts": by_account,
    });
    super::monitor::write_ranking_text(&payload.to_string(), &tokens_dir.join(SIDECAR_FILE_NAME))
}

/// Read the sidecar [`write_weekly_utilization_sidecar`] writes:
/// `{account name -> 7d utilization (0..=1)}`.
///
/// Empty when the file is absent, unreadable, not valid JSON, an unsupported
/// schema, or older than the `.ranking` beside it (see the module doc). A
/// non-numeric, negative or non-finite value drops that one account rather
/// than the whole file.
#[must_use]
pub fn read_weekly_utilization_sidecar(tokens_dir: &Path) -> BTreeMap<String, f64> {
    let Ok(raw) = std::fs::read_to_string(tokens_dir.join(SIDECAR_FILE_NAME)) else {
        return BTreeMap::new();
    };
    let Ok(data) = serde_json::from_str::<serde_json::Value>(&raw) else {
        return BTreeMap::new();
    };
    if data.get("schema").and_then(serde_json::Value::as_i64) != Some(SIDECAR_SCHEMA) {
        return BTreeMap::new();
    }
    let Some(written_at) = data
        .get("written_at")
        .and_then(serde_json::Value::as_str)
        .and_then(|raw| DateTime::parse_from_rfc3339(raw).ok())
        .map(|dt| dt.with_timezone(&Utc))
    else {
        return BTreeMap::new();
    };
    if ranking_is_newer_than(tokens_dir, written_at) {
        return BTreeMap::new();
    }
    let Some(accounts) = data.get("accounts").and_then(serde_json::Value::as_object) else {
        return BTreeMap::new();
    };
    accounts
        .iter()
        .filter_map(|(name, util)| {
            let util = util.as_f64().filter(|u| u.is_finite() && *u >= 0.0)?;
            Some((name.clone(), util))
        })
        .collect()
}

/// Whether `.ranking` in `tokens_dir` was rewritten after a sidecar stamped
/// `written_at` (beyond [`PAIRING_SLACK_SECONDS`]). An unreadable `.ranking`
/// mtime is not evidence of a newer ranking, so it does not reject the sidecar
/// — the collector reads no Claude rows without a readable `.ranking` anyway.
fn ranking_is_newer_than(tokens_dir: &Path, written_at: DateTime<Utc>) -> bool {
    let Ok(modified) = std::fs::metadata(tokens_dir.join(".ranking")).and_then(|m| m.modified())
    else {
        return false;
    };
    let ranking_mtime: DateTime<Utc> = modified.into();
    (ranking_mtime - written_at).num_seconds() > PAIRING_SLACK_SECONDS
}

#[cfg(test)]
#[path = "ranking_weekly_tests.rs"]
mod tests;
