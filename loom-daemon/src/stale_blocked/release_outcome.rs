//! Observable outcomes of the `loom:blocked` release tick (#10763).
//!
//! Before this module every refusal in [`super::release_gh::maybe_run`] was
//! silent, so a fleet where the pass never acted was undiagnosable without host
//! logs. Now every per-root tick yields exactly one [`Outcome`], classified by
//! pure functions (the tick itself is `cfg!(test)`-disabled), logged
//! edge-triggered for the quiet ones, and tallied per workspace in
//! [`RepoCounters`]. The tallies reach the fleet two ways:
//!
//! - `host.health`'s `managed_repos[].stale_blocked_release`
//!   ([`crate::telemetry::StaleBlockedReleaseCounters`]), sampled by the
//!   observability collector, so every host's per-repo releases, re-parks and
//!   last outcome are visible fleet-wide;
//! - `<root>/.loom/status/stale-blocked-release.json` (the gitignored status
//!   dir), for an operator on the host itself.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex, PoisonError};

use serde::Serialize;

use super::release::Report;
use super::release_gh::Mode;

/// Where the per-workspace counters are persisted, relative to the root.
pub const STATUS_FILE: &str = ".loom/status/stale-blocked-release.json";

/// What one tick did for one workspace.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    /// A live pass ran.
    Ran,
    /// `LOOM_RELEASE_STALE_BLOCKED=off`.
    SkippedOff,
    /// Neither the work finder nor the role runner runs for this workspace
    /// on this host, so the host takes no part in its automation.
    NotServed,
    /// The shared GitHub rate-limit breaker is suppressing forge calls.
    RateLimited,
    /// Inside the cadence window.
    NotDue,
    /// Another host owns this workspace's shard (or the roster is yielding).
    SkippedShard,
    /// The root is outside the forge-write scope (#9548).
    DeniedScope,
    /// `LOOM_RELEASE_STALE_BLOCKED=dry-run`: planned, wrote nothing.
    DryRun,
    /// The repository is archived (#10562): read-only, nothing evaluated.
    Archived,
    /// The pass ran but its listing/probe failed or looked anomalous.
    EnumerateError,
}

impl Outcome {
    /// Stable snake_case key (log field and counter key).
    #[must_use]
    pub const fn key(self) -> &'static str {
        match self {
            Self::Ran => "ran",
            Self::SkippedOff => "skipped_off",
            Self::NotServed => "not_served",
            Self::RateLimited => "rate_limited",
            Self::NotDue => "not_due",
            Self::SkippedShard => "skipped_shard",
            Self::DeniedScope => "denied_scope",
            Self::DryRun => "dry_run",
            Self::Archived => "archived",
            Self::EnumerateError => "enumerate_error",
        }
    }

    /// Quiet outcomes repeat every poll; log them only on a change.
    #[must_use]
    pub const fn edge_triggered(self) -> bool {
        matches!(
            self,
            Self::NotDue
                | Self::SkippedShard
                | Self::SkippedOff
                | Self::NotServed
                | Self::RateLimited
                | Self::Archived
        )
    }
}

/// The pre-pass gates in tick order: `Some(outcome)` when one refuses.
#[must_use]
pub fn classify_gate(mode: Mode, due: bool, owned: bool) -> Option<Outcome> {
    if mode == Mode::Off {
        Some(Outcome::SkippedOff)
    } else if !due {
        Some(Outcome::NotDue)
    } else if !owned {
        Some(Outcome::SkippedShard)
    } else {
        None
    }
}

/// The outcome of a pass that did run (or was refused by the scope gate inside it).
#[must_use]
pub fn classify_report(dry_run: bool, report: &Report) -> Outcome {
    match &report.enumerate_error {
        Some(e) if e.contains("outside the forge-write scope") => Outcome::DeniedScope,
        Some(_) => Outcome::EnumerateError,
        None if report.archived => Outcome::Archived,
        None if dry_run => Outcome::DryRun,
        None => Outcome::Ran,
    }
}

/// Per-workspace tallies since this daemon started.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct RepoCounters {
    /// Ticks per [`Outcome::key`].
    pub ticks: BTreeMap<String, u64>,
    /// Artifacts actually released (applied writes only, never dry-run plans).
    pub released: u64,
    /// Artifacts actually re-parked.
    pub reparked: u64,
    pub last_outcome: Option<String>,
    /// Unix seconds of the last tick that was not `not_due`.
    pub last_active_unix: Option<u64>,
    pub last_error: Option<String>,
}

impl RepoCounters {
    /// Fold one tick in.
    pub fn apply(&mut self, outcome: Outcome, report: Option<&Report>, now_unix: u64) {
        *self.ticks.entry(outcome.key().to_string()).or_default() += 1;
        self.last_outcome = Some(outcome.key().to_string());
        if outcome != Outcome::NotDue {
            self.last_active_unix = Some(now_unix);
        }
        if let Some(r) = report {
            if outcome == Outcome::Ran {
                self.released += r.released.iter().filter(|a| a.applied).count() as u64;
                self.reparked += r.reparked.iter().filter(|a| a.applied).count() as u64;
            }
            if matches!(outcome, Outcome::EnumerateError | Outcome::DeniedScope) {
                self.last_error.clone_from(&r.enumerate_error);
            }
        }
    }

    /// The wire form carried on `host.health` (#10763).
    #[must_use]
    pub fn to_telemetry(&self) -> crate::telemetry::StaleBlockedReleaseCounters {
        crate::telemetry::StaleBlockedReleaseCounters {
            released: self.released,
            reparked: self.reparked,
            last_outcome: self.last_outcome.clone(),
            ticks: self.ticks.clone(),
        }
    }
}

static COUNTERS: LazyLock<Mutex<HashMap<PathBuf, RepoCounters>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static LAST_LOGGED: LazyLock<Mutex<HashMap<PathBuf, Outcome>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Snapshot of the counters for `root`; `None` before its first tick.
#[must_use]
pub fn counters(root: &Path) -> Option<RepoCounters> {
    COUNTERS
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .get(root)
        .cloned()
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// Record one tick: tally, log (edge-triggered for quiet outcomes), persist.
pub fn record(root: &Path, outcome: Outcome, report: Option<&Report>) {
    let snapshot = {
        let mut map = COUNTERS.lock().unwrap_or_else(PoisonError::into_inner);
        let c = map.entry(root.to_path_buf()).or_default();
        c.apply(outcome, report, now_unix());
        c.clone()
    };
    let changed = LAST_LOGGED
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .insert(root.to_path_buf(), outcome)
        != Some(outcome);
    let line = format!("stale_blocked_release: {} outcome={}", root.display(), outcome.key());
    let detail = report
        .and_then(|r| r.enumerate_error.as_deref())
        .map(|e| format!(": {e}"))
        .unwrap_or_default();
    match outcome {
        Outcome::DeniedScope | Outcome::EnumerateError if changed => {
            log::warn!("{line}{detail} (#10763)");
        }
        Outcome::DeniedScope | Outcome::EnumerateError => log::info!("{line}{detail} (#10763)"),
        o if !o.edge_triggered() => log::info!("{line} (#10763)"),
        _ if changed => log::info!("{line} (first tick in this state; repeats at DEBUG, #10763)"),
        _ => log::debug!("{line}"),
    }
    if outcome != Outcome::NotDue && (changed || report.is_some()) {
        persist(root, &snapshot);
    }
}

fn persist(root: &Path, counters: &RepoCounters) {
    let path = root.join(STATUS_FILE);
    let Ok(body) = serde_json::to_string_pretty(counters) else {
        return;
    };
    if let Some(dir) = path.parent() {
        if std::fs::create_dir_all(dir).is_err() {
            return;
        }
    }
    let tmp = path.with_extension("json.tmp");
    if std::fs::write(&tmp, body).is_ok() {
        let _ = std::fs::rename(&tmp, &path);
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::stale_blocked::release::Acted;

    fn acted(number: u64, applied: bool) -> Acted {
        Acted {
            kind: "issue",
            number,
            resolved: vec![],
            still_open: vec![],
            restored: None,
            commented: true,
            applied,
        }
    }

    #[test]
    fn gate_classification_follows_tick_order() {
        assert_eq!(classify_gate(Mode::Off, false, false), Some(Outcome::SkippedOff));
        assert_eq!(classify_gate(Mode::On, false, false), Some(Outcome::NotDue));
        assert_eq!(classify_gate(Mode::On, true, false), Some(Outcome::SkippedShard));
        assert_eq!(classify_gate(Mode::DryRun, true, true), None);
        assert_eq!(classify_gate(Mode::On, true, true), None);
    }

    #[test]
    fn report_classification() {
        let ok = Report::default();
        assert_eq!(classify_report(false, &ok), Outcome::Ran);
        assert_eq!(classify_report(true, &ok), Outcome::DryRun);
        let scope = Report {
            enumerate_error: Some("outside the forge-write scope (#9548)".into()),
            ..Report::default()
        };
        assert_eq!(classify_report(false, &scope), Outcome::DeniedScope);
        let err = Report {
            enumerate_error: Some("loom:blocked listing failed: x".into()),
            ..Report::default()
        };
        assert_eq!(classify_report(false, &err), Outcome::EnumerateError);
        let archived = Report {
            archived: true,
            ..Report::default()
        };
        assert_eq!(classify_report(false, &archived), Outcome::Archived);
    }

    #[test]
    fn counters_tally_applied_releases_and_reparks() {
        let report = Report {
            released: vec![acted(1, true), acted(2, true), acted(9, false)],
            reparked: vec![acted(3, true)],
            ..Report::default()
        };
        let mut c = RepoCounters::default();
        c.apply(Outcome::NotDue, None, 10);
        assert_eq!(c.last_active_unix, None);
        c.apply(Outcome::Ran, Some(&report), 20);
        c.apply(Outcome::Ran, Some(&report), 30);
        assert_eq!((c.released, c.reparked), (4, 2));
        assert_eq!(c.ticks["ran"], 2);
        assert_eq!(c.ticks["not_due"], 1);
        assert_eq!(c.last_active_unix, Some(30));
        let wire = c.to_telemetry();
        assert_eq!((wire.released, wire.reparked), (4, 2));
        assert_eq!(wire.last_outcome.as_deref(), Some("ran"));
    }

    #[test]
    fn dry_run_does_not_count_releases() {
        let mut c = RepoCounters::default();
        let report = Report {
            released: vec![acted(1, false)],
            ..Report::default()
        };
        c.apply(Outcome::DryRun, Some(&report), 5);
        assert_eq!(c.released, 0);
        assert_eq!(c.ticks["dry_run"], 1);
    }

    #[test]
    fn scope_denial_keeps_its_reason() {
        let mut c = RepoCounters::default();
        let report = Report {
            enumerate_error: Some("outside the forge-write scope (#9548)".into()),
            ..Report::default()
        };
        c.apply(Outcome::DeniedScope, Some(&report), 5);
        assert!(c.last_error.unwrap().contains("forge-write scope"));
    }

    #[test]
    fn record_persists_and_exposes_counters() {
        let dir = tempfile::tempdir().unwrap();
        assert!(counters(dir.path()).is_none());
        record(dir.path(), Outcome::SkippedShard, None);
        record(dir.path(), Outcome::SkippedShard, None);
        assert_eq!(counters(dir.path()).unwrap().ticks["skipped_shard"], 2);
        let body = std::fs::read_to_string(dir.path().join(STATUS_FILE)).unwrap();
        assert!(body.contains("skipped_shard"));
    }

    #[test]
    fn every_outcome_has_a_distinct_key() {
        let all = [
            Outcome::Ran,
            Outcome::SkippedOff,
            Outcome::NotServed,
            Outcome::RateLimited,
            Outcome::NotDue,
            Outcome::SkippedShard,
            Outcome::DeniedScope,
            Outcome::DryRun,
            Outcome::Archived,
            Outcome::EnumerateError,
        ];
        let keys: std::collections::BTreeSet<_> = all.iter().map(|o| o.key()).collect();
        assert_eq!(keys.len(), all.len());
    }
}
