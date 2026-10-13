//! Fleet-sync failures on loom-ui's Alerts page (#11295).
//!
//! - `fleet.floor-malformed` (key `loom_min_version`): the store's floor
//!   cannot be read as `X.Y.Z`. Opens on the first such pass (a typo in the
//!   store does not fix itself), resolves on the first pass that reads a valid
//!   floor or none.
//! - `fleet.sync-error` (key `fleet-store`): the config or roster half of a
//!   pass failed, or the floor could not be read from the snapshot at all,
//!   in [`ERROR_AFTER`] passes in a row (one forge blip is not an alert).
//!   Once open it stays open until a pass with no error.
//!
//! Evaluated after every pass, startup and timer, and kept in a
//! [`Ledger`] so a restart resolves what the previous process left open.

use std::sync::{Mutex, OnceLock};

use super::FleetSyncStatus;
use crate::loom_ui_alerts::ledger::Ledger;
use crate::loom_ui_alerts::{Alert, Severity, State};

/// Rule for a malformed `loom_min_version`.
pub const RULE_FLOOR: &str = "fleet.floor-malformed";
/// Rule for a failing sync pass.
pub const RULE_SYNC: &str = "fleet.sync-error";
/// Erroring passes in a row before `fleet.sync-error` opens.
pub const ERROR_AFTER: u32 = 3;

const KEY_FLOOR: &str = "loom_min_version";
const KEY_SYNC: &str = "fleet-store";

/// The pass-to-pass state: the ledger and the current error streak.
pub(super) struct Watch {
    ledger: Ledger,
    streak: u32,
}

impl Watch {
    pub(super) fn new(ledger: Ledger) -> Self {
        Self { ledger, streak: 0 }
    }

    /// Fold one pass in, posting its edges.
    pub(super) fn observe(&mut self, status: &FleetSyncStatus) {
        let mut current = Vec::new();
        let floor_error = status.floor.error.as_deref();
        let malformed = floor_error.filter(|e| is_malformed(e));
        if let Some(why) = malformed {
            current.push(
                Alert::new(
                    RULE_FLOOR,
                    KEY_FLOOR,
                    format!("The fleet store's `{KEY_FLOOR}` is malformed"),
                    Severity::Warning,
                    State::Open,
                )
                .with_detail(format!("{} @ {}: {why}", status.repo, status.reference)),
            );
        }
        let errors: Vec<String> = [
            status
                .config
                .error
                .as_deref()
                .map(|e| format!("config: {e}")),
            status
                .roster
                .error
                .as_deref()
                .map(|e| format!("roster: {e}")),
            floor_error
                .filter(|e| !is_malformed(e))
                .map(|e| format!("floor: {e}")),
        ]
        .into_iter()
        .flatten()
        .collect();
        self.streak = if errors.is_empty() {
            0
        } else {
            self.streak.saturating_add(1)
        };
        let held = self.ledger.is_open(RULE_SYNC, KEY_SYNC);
        if !errors.is_empty() && (self.streak >= ERROR_AFTER || held) {
            current.push(
                Alert::new(
                    RULE_SYNC,
                    KEY_SYNC,
                    format!("Fleet-store sync from {} is failing", status.repo),
                    Severity::Warning,
                    State::Open,
                )
                .with_detail(format!(
                    "{} failing pass(es) in a row ({} pass at {}): {}",
                    self.streak,
                    status.pass,
                    status.at.to_rfc3339(),
                    errors.join("; ")
                )),
            );
        }
        self.ledger.reconcile(current);
    }
}

/// Whether a floor error is a malformed value, not a failed read
/// ([`super::resolve_floor`] words the two differently).
fn is_malformed(error: &str) -> bool {
    error.contains(&format!("malformed `{KEY_FLOOR}`"))
}

/// Report `status` to loom-ui through the process's watch.
pub(super) fn report(status: &FleetSyncStatus) {
    static WATCH: OnceLock<Mutex<Watch>> = OnceLock::new();
    WATCH
        .get_or_init(|| Mutex::new(Watch::new(Ledger::load("fleet_sync"))))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .observe(status);
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::super::{resolve_floor, ConfigPass, FleetSyncStatus, RosterPass};
    use super::*;
    use crate::fleet_store::floor::FloorRead;
    use std::sync::Arc;

    type Seen = Arc<Mutex<Vec<(String, State)>>>;

    fn watch() -> (Watch, Seen) {
        let seen: Seen = Arc::default();
        let sink = seen.clone();
        let ledger = Ledger::at(None, move |a: Alert| sink.lock().unwrap().push((a.rule, a.state)));
        (Watch::new(ledger), seen)
    }

    fn status() -> FleetSyncStatus {
        let raw = serde_json::json!({
            "repo": "o/fleet", "reference": "main", "host": "h", "pass": "timer",
            "at": "2026-10-11T00:00:00Z", "intervalSecs": 300, "autoApply": false,
            "config": ConfigPass::default(), "roster": RosterPass::default(),
        });
        serde_json::from_value(raw).unwrap()
    }

    fn take(seen: &Seen) -> Vec<(String, State)> {
        std::mem::take(&mut *seen.lock().unwrap())
    }

    #[test]
    fn a_malformed_floor_opens_at_once_and_a_valid_one_resolves() {
        let (mut w, seen) = watch();
        let mut s = status();
        s.floor = resolve_floor(
            Ok(FloorRead::Malformed {
                source: "fleet.json",
                detail: "bad".into(),
            }),
            Some("0.19.1"),
        );
        w.observe(&s);
        assert_eq!(take(&seen), [(RULE_FLOOR.to_string(), State::Open)]);
        w.observe(&s);
        assert!(take(&seen).is_empty());
        s.floor = resolve_floor(
            Ok(FloorRead::Valid {
                version: "0.19.2".into(),
                source: "fleet.json",
            }),
            None,
        );
        w.observe(&s);
        assert_eq!(take(&seen), [(RULE_FLOOR.to_string(), State::Resolved)]);
    }

    #[test]
    fn sync_errors_open_after_three_passes_and_resolve_on_a_clean_one() {
        let (mut w, seen) = watch();
        let mut bad = status();
        bad.roster.error = Some("forge said 502".into());
        w.observe(&bad);
        w.observe(&bad);
        assert!(take(&seen).is_empty());
        w.observe(&bad);
        assert_eq!(take(&seen), [(RULE_SYNC.to_string(), State::Open)]);
        w.observe(&status());
        assert_eq!(take(&seen), [(RULE_SYNC.to_string(), State::Resolved)]);
    }

    #[test]
    fn an_unreadable_floor_is_a_sync_error_not_a_malformed_one() {
        let (mut w, seen) = watch();
        let mut s = status();
        s.floor = resolve_floor(Err("no snapshot".into()), None);
        for _ in 0..ERROR_AFTER {
            w.observe(&s);
        }
        assert_eq!(take(&seen), [(RULE_SYNC.to_string(), State::Open)]);
    }

    #[test]
    fn an_incident_inherited_from_a_restart_stays_open_through_the_streak() {
        // The previous process opened it; the first erroring pass of this one
        // must not resolve it only to reopen it two passes later.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fleet_sync.json");
        let mut bad = status();
        bad.config.error = Some("timeout".into());
        {
            let mut w = Watch::new(Ledger::at(Some(path.clone()), |_| {}));
            for _ in 0..ERROR_AFTER {
                w.observe(&bad);
            }
        }
        let seen: Seen = Arc::default();
        let sink = seen.clone();
        let mut w = Watch::new(Ledger::at(Some(path), move |a: Alert| {
            sink.lock().unwrap().push((a.rule, a.state));
        }));
        w.observe(&bad);
        assert!(take(&seen).is_empty());
        w.observe(&status());
        assert_eq!(take(&seen), [(RULE_SYNC.to_string(), State::Resolved)]);
    }
}
