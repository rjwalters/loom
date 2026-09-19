//! Per-model-class utilization ingested from claude-monitor's `ranking.json`
//! (issue #8297).
//!
//! [`super::monitor`] already reads `accounts[].utilization` (account-wide,
//! `5h`/`7d`) and `accounts[].resets` from `ranking.json` but ignores the
//! sibling `accounts[].models` map — e.g. `{"fable": {"utilization": 0.91}}`
//! — confirmed populated on a live host in the same capture that motivated
//! #8242. This module is the ingest path for that map.
//!
//! # Design decision: report only, never gates selection
//!
//! Phase 1/2's `.bad_tokens` marks (#8090, #8241) are **terminal** signals —
//! a probe or a wrapper-observed rotation already concluded an account is
//! down for a class. claude-monitor's per-class utilization is **predictive**
//! — a fraction approaching 100% forecasts a future block, it is not one.
//! Mixing the two would need a threshold policy (how close to 100% is "down
//! enough to exclude?") this issue does not scope, and — per the "narrower,
//! never wider" fail-safe direction #8058 held throughout — a wrong threshold
//! would risk excluding an account the selector could still legitimately
//! hand out. So: this data is surfaced on the health/status surfaces
//! ([`crate::capacity::model_class`]) for an operator to read, and it is
//! never consulted by `select.rs`/`bad_tokens.rs`. Only an actual
//! probe-confirmed status change removes an account from the candidate pool.
//!
//! # Why a JSON sidecar, not a fifth `.ranking` column
//!
//! `select::parse_ranking_line` splits `.ranking` rows on `splitn(4, '|')`;
//! a naive fifth column would be silently swallowed into `limit_reset` by any
//! reader that has not been upgraded to expect it. Rather than version the
//! selector's hot-path format for data it must never consult, the per-class
//! map is written to its own versioned (`schema`) JSON file beside
//! `.ranking`, read only by the observability path
//! ([`crate::capacity::model_class::read_class_capacity_at`]).
//!
//! # Freshness is enforced on read, not on write
//!
//! The sidecar carries its own `written_at`. A reader older than
//! [`SIDECAR_FRESH_SECONDS`] — the same window `super::monitor::is_fresh`
//! applies to `ranking.json` itself — is treated as absent. This is what
//! keeps a stale snapshot from surviving a later run that falls back to
//! probing (which has no per-class data and therefore never rewrites this
//! file): the probe path does not need to know this sidecar exists at all.

use std::collections::BTreeMap;
use std::path::Path;

use chrono::{DateTime, Utc};

use super::monitor::{parse_iso8601, write_ranking_text, MonitorAccount};

const SIDECAR_FILE_NAME: &str = ".ranking.classes.json";
const SIDECAR_SCHEMA: i64 = 1;
/// Same freshness window as `ranking.json` itself (`super::monitor::is_fresh`).
const SIDECAR_FRESH_SECONDS: i64 = 600;

/// Per-class utilization for one `ranking.json` account entry
/// (`accounts[].models.<class>.utilization`), or empty when the entry has no
/// `models` object. Only classes claude-monitor actually reports appear as
/// keys — an absent class means "no data", and must never be coerced to
/// `0.0` (the issue's "Coverage" note: only the class in active use appears
/// today).
pub(super) fn ranking_row_class_utilization(entry: &serde_json::Value) -> BTreeMap<String, f64> {
    let mut out = BTreeMap::new();
    let Some(models) = entry.get("models").and_then(serde_json::Value::as_object) else {
        return out;
    };
    for (class, value) in models {
        let class = class.trim();
        if class.is_empty() {
            continue;
        }
        if let Some(util) = value.get("utilization").and_then(serde_json::Value::as_f64) {
            out.insert(class.to_string(), util);
        }
    }
    out
}

/// Write the sidecar carrying every account's non-empty
/// [`MonitorAccount::class_utilization`] map, atomically, beside `.ranking`
/// in `tokens_dir`. Always written (even when every map is empty) so a stale
/// prior sidecar from a since-vanished `models` entry cannot outlive its own
/// freshness window.
pub(super) fn write_class_utilization_sidecar(
    accounts: &[MonitorAccount],
    tokens_dir: &Path,
    now: DateTime<Utc>,
) -> std::io::Result<()> {
    let mut by_account = serde_json::Map::new();
    for a in accounts {
        if !a.class_utilization.is_empty() {
            by_account.insert(a.name.clone(), serde_json::json!(a.class_utilization));
        }
    }
    let payload = serde_json::json!({
        "schema": SIDECAR_SCHEMA,
        "written_at": now.format("%Y-%m-%dT%H:%M:%SZ").to_string(),
        "accounts": by_account,
    });
    write_ranking_text(&payload.to_string(), &tokens_dir.join(SIDECAR_FILE_NAME))
}

/// Read the sidecar [`write_class_utilization_sidecar`] writes:
/// `{name -> {class -> utilization}}`. Empty when the file is absent,
/// unreadable, not valid JSON, an unsupported schema, or stale (`written_at`
/// older than [`SIDECAR_FRESH_SECONDS`]) — the same fail-closed posture
/// `super::monitor::is_fresh` takes on `ranking.json` itself.
#[must_use]
pub fn read_class_utilization_sidecar(
    tokens_dir: &Path,
    now: DateTime<Utc>,
) -> BTreeMap<String, BTreeMap<String, f64>> {
    let path = tokens_dir.join(SIDECAR_FILE_NAME);
    let Ok(raw) = std::fs::read_to_string(&path) else {
        return BTreeMap::new();
    };
    let Ok(data) = serde_json::from_str::<serde_json::Value>(&raw) else {
        return BTreeMap::new();
    };
    if data.get("schema").and_then(serde_json::Value::as_i64) != Some(SIDECAR_SCHEMA) {
        return BTreeMap::new();
    }
    let fresh = parse_iso8601(data.get("written_at").and_then(|v| v.as_str()))
        .is_some_and(|dt| (0..SIDECAR_FRESH_SECONDS).contains(&(now - dt).num_seconds()));
    if !fresh {
        return BTreeMap::new();
    }
    let Some(accounts) = data.get("accounts").and_then(serde_json::Value::as_object) else {
        return BTreeMap::new();
    };
    let mut out = BTreeMap::new();
    for (name, classes) in accounts {
        let Some(classes_obj) = classes.as_object() else {
            continue;
        };
        let mut per_class = BTreeMap::new();
        for (class, util) in classes_obj {
            if let Some(u) = util.as_f64() {
                per_class.insert(class.clone(), u);
            }
        }
        if !per_class.is_empty() {
            out.insert(name.clone(), per_class);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn account(name: &str, class_utilization: &[(&str, f64)]) -> MonitorAccount {
        MonitorAccount {
            name: name.to_string(),
            status: "available".to_string(),
            util_7d: None,
            util_5h: None,
            reset_7d: None,
            reset_5h: None,
            class_utilization: class_utilization
                .iter()
                .map(|(c, u)| ((*c).to_string(), *u))
                .collect(),
        }
    }

    #[test]
    fn ranking_row_class_utilization_reads_present_classes_only() {
        let entry = serde_json::json!({
            "models": {"fable": {"utilization": 0.91}},
        });
        let got = ranking_row_class_utilization(&entry);
        assert_eq!(got.get("fable"), Some(&0.91));
        // Absent class stays absent, never coerced to a fabricated 0.0.
        assert_eq!(got.get("opus"), None);
    }

    #[test]
    fn ranking_row_class_utilization_is_empty_without_a_models_object() {
        let entry = serde_json::json!({"status": "available"});
        assert!(ranking_row_class_utilization(&entry).is_empty());
    }

    #[test]
    fn sidecar_round_trips_a_fresh_write() {
        let dir = tempfile::tempdir().unwrap();
        let now = Utc::now();
        let accounts = vec![
            account("agent-18", &[("fable", 0.91)]),
            account("agent-2", &[]),
        ];
        write_class_utilization_sidecar(&accounts, dir.path(), now).unwrap();

        let read = read_class_utilization_sidecar(dir.path(), now);
        assert_eq!(read.get("agent-18").and_then(|m| m.get("fable")), Some(&0.91));
        // An account with no class-scoped data is omitted, not fabricated as
        // an empty-but-present entry.
        assert!(!read.contains_key("agent-2"));
    }

    #[test]
    fn sidecar_stale_write_reads_as_absent() {
        let dir = tempfile::tempdir().unwrap();
        let written_at = Utc::now() - chrono::Duration::seconds(SIDECAR_FRESH_SECONDS + 60);
        let accounts = vec![account("agent-18", &[("fable", 0.91)])];
        write_class_utilization_sidecar(&accounts, dir.path(), written_at).unwrap();

        // Reading "now" against a `written_at` far in the past must fail
        // closed — a probe-fallback run leaves this file untouched, and a
        // stale snapshot must not survive to be reported as current.
        let read = read_class_utilization_sidecar(dir.path(), Utc::now());
        assert!(read.is_empty());
    }

    #[test]
    fn sidecar_missing_file_reads_as_empty() {
        let dir = tempfile::tempdir().unwrap();
        assert!(read_class_utilization_sidecar(dir.path(), Utc::now()).is_empty());
    }
}
