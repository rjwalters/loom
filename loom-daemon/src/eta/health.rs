//! Local ETA pipeline health state (#10391): two small files the daemon
//! writes and `loom-daemon eta doctor` (a separate process) reads.
//!
//! - `.loom/state/eta/health/fit-check.json`: the last `eta.fit` record,
//!   byte-identical to its OTLP body.
//! - `.loom/state/eta/health/refresh-cycle.json`: the last fleet refresh
//!   tick, written on **every** tick, stand-down included.
//!
//! Both are written atomically and live outside `fit_dir`, so the coefficient
//! file retention never touches them. A failed write is logged and swallowed:
//! health state never costs a fit or a refresh.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::telemetry::kinds::eta_fit::EtaFitRecord;

/// `<root>/.loom/state/eta/health`.
#[must_use]
pub fn dir(root: &Path) -> PathBuf {
    root.join(".loom").join("state").join("eta").join("health")
}

/// The last `eta.fit` record.
#[must_use]
pub fn fit_check_path(root: &Path) -> PathBuf {
    dir(root).join("fit-check.json")
}

/// The last refresh tick.
#[must_use]
pub fn refresh_cycle_path(root: &Path) -> PathBuf {
    dir(root).join("refresh-cycle.json")
}

/// One repo in the last refreshing tick.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RefreshRepo {
    /// `owner/repo`.
    pub repo: String,
    /// The stop reason (`StopReason::as_str`).
    pub stop_reason: String,
    /// The published snapshot's `as_of`, when there is one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub as_of: Option<DateTime<Utc>>,
}

/// The last fleet refresh tick.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RefreshCycleState {
    /// When the tick started.
    pub started_at: DateTime<Utc>,
    /// `captain`, `no_captain` or `stand_down`.
    pub gate: String,
    /// The declared captain, on `stand_down`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub captain: Option<String>,
    /// The configured `intervalSecs`.
    pub interval_secs: u64,
    /// Repos per stop reason (empty on a stand-down tick).
    pub stop_reasons: BTreeMap<String, u64>,
    /// Every repo of the tick (empty on a stand-down tick).
    pub repos: Vec<RefreshRepo>,
}

/// Write `text` to `path` through a temp file and a rename.
///
/// # Errors
///
/// The directory could not be created, or the write or rename failed.
pub fn write_atomic(path: &Path, text: &str) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, path)
}

/// Persist the last `eta.fit` record; a failure is logged.
pub fn write_fit_check(root: &Path, body: &str) {
    if let Err(e) = write_atomic(&fit_check_path(root), body) {
        log::warn!("eta health: writing fit-check.json failed: {e}");
    }
}

/// Persist the last refresh tick; a failure is logged.
pub fn write_refresh_cycle(root: &Path, state: &RefreshCycleState) {
    let Ok(text) = serde_json::to_string_pretty(state) else {
        return;
    };
    if let Err(e) = write_atomic(&refresh_cycle_path(root), &text) {
        log::warn!("eta health: writing refresh-cycle.json failed: {e}");
    }
}

/// The last `eta.fit` record, if one was written and parses.
#[must_use]
pub fn read_fit_check(root: &Path) -> Option<EtaFitRecord> {
    serde_json::from_str(&std::fs::read_to_string(fit_check_path(root)).ok()?).ok()
}

/// The last refresh tick, if one was written and parses.
#[must_use]
pub fn read_refresh_cycle(root: &Path) -> Option<RefreshCycleState> {
    serde_json::from_str(&std::fs::read_to_string(refresh_cycle_path(root)).ok()?).ok()
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn refresh_cycle_round_trips_atomically_and_leaves_no_tmp() {
        let dir = tempfile::tempdir().unwrap();
        let state = RefreshCycleState {
            started_at: Utc.with_ymd_and_hms(2026, 10, 5, 1, 0, 0).unwrap(),
            gate: "stand_down".into(),
            captain: Some("robb-studio".into()),
            interval_secs: 3600,
            stop_reasons: BTreeMap::new(),
            repos: Vec::new(),
        };
        write_refresh_cycle(dir.path(), &state);
        write_refresh_cycle(dir.path(), &state);
        assert_eq!(read_refresh_cycle(dir.path()), Some(state));
        let names: Vec<_> = std::fs::read_dir(super::dir(dir.path()))
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, vec![std::ffi::OsString::from("refresh-cycle.json")]);
        assert_eq!(read_fit_check(dir.path()), None);
    }
}
