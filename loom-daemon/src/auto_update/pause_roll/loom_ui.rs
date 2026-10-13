//! Pause-roll failures on loom-ui's Alerts page (#11295).
//!
//! - `version.pause-failed`, key = the version the host is stuck on: an H4
//!   pause failed (H7 `pause-failed`) and the host stays on that version until
//!   a later roll takes. Resolved by the first start on another version (not
//!   by the pause that succeeds: the daemon exits right after it, before the
//!   outbox could send a `resolved`).
//! - `version.config-rejected`, key `pauseRoll`: the
//!   `autonomous.autoUpdate.pauseRoll.*` tuning was rejected and the defaults
//!   used. Evaluated at every roll start and at daemon start; resolved by the
//!   first evaluation that accepts it.
//!
//! Both live in one [`Ledger`], so a roll (which restarts the daemon) never
//! leaves either open.

use std::path::Path;
use std::sync::{Mutex, OnceLock};

use crate::loom_ui_alerts::ledger::Ledger;
use crate::loom_ui_alerts::{Alert, Severity, State};

/// Rule for a failed pause.
pub const RULE_PAUSE_FAILED: &str = "version.pause-failed";
/// Rule for rejected pause-roll tuning.
pub const RULE_CONFIG_REJECTED: &str = "version.config-rejected";
const KEY_CONFIG: &str = "pauseRoll";

fn ledger() -> &'static Mutex<Ledger> {
    static LEDGER: OnceLock<Mutex<Ledger>> = OnceLock::new();
    LEDGER.get_or_init(|| Mutex::new(Ledger::load("pause_roll")))
}

fn with<R>(f: impl FnOnce(&mut Ledger) -> R) -> R {
    f(&mut ledger()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner))
}

/// The open `config-rejected` alert for `rejected`, if any.
#[must_use]
pub fn config_alert(rejected: Option<&str>) -> Option<Alert> {
    rejected.map(|why| {
        Alert::new(
            RULE_CONFIG_REJECTED,
            KEY_CONFIG,
            "Pause-roll tuning was rejected; rolls use the defaults",
            Severity::Warning,
            State::Open,
        )
        .with_detail(why)
    })
}

/// Record whether the pause-roll tuning was rejected.
pub fn config_checked(rejected: Option<&str>) {
    let current = config_alert(rejected).into_iter().collect();
    with(|l| l.reconcile_where(current, |rule, _| rule == RULE_CONFIG_REJECTED));
}

/// An H4 pause on `from_version` failed with `why`.
pub fn pause_failed(from_version: &str, why: &str) {
    with(|l| {
        l.open(
            Alert::new(
                RULE_PAUSE_FAILED,
                from_version,
                format!(
                    "A version roll could not pause this host's agents; it stays on {from_version}"
                ),
                Severity::Warning,
                State::Open,
            )
            .with_detail(why),
        )
    });
}

/// Resolve every `pause-failed` incident not about `running` in `ledger`.
pub fn resolve_rolled(ledger: &mut Ledger, running: &str) -> usize {
    ledger.reconcile_where(Vec::new(), |rule, key| rule == RULE_PAUSE_FAILED && key != running)
}

/// At daemon start: a pause that failed on another version is over (this
/// process runs a different one), and the tuning is checked once.
pub fn startup(root: &Path) {
    with(|l| resolve_rolled(l, env!("CARGO_PKG_VERSION")));
    let (_, rejected) = super::PauseRollTuning::resolve(root);
    config_checked(rejected.as_deref());
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::sync::Arc;

    type Seen = Arc<Mutex<Vec<(String, String, State)>>>;

    fn ledger() -> (Ledger, Seen) {
        let seen: Seen = Arc::default();
        let sink = seen.clone();
        let l = Ledger::at(None, move |a: Alert| {
            sink.lock().unwrap().push((a.rule, a.key, a.state));
        });
        (l, seen)
    }

    #[test]
    fn a_start_on_another_version_resolves_the_failed_pause() {
        let (mut l, seen) = ledger();
        l.open(Alert::new(RULE_PAUSE_FAILED, "0.19.1", "t", Severity::Warning, State::Open));
        l.open(config_alert(Some("bad")).unwrap());
        assert_eq!(resolve_rolled(&mut l, "0.19.1"), 0);
        assert_eq!(resolve_rolled(&mut l, "0.19.2"), 1);
        let got = seen.lock().unwrap();
        assert_eq!(
            got.last().unwrap(),
            &(RULE_PAUSE_FAILED.into(), "0.19.1".into(), State::Resolved)
        );
        // The config incident is not this evaluation's to resolve.
        assert!(l.is_open(RULE_CONFIG_REJECTED, KEY_CONFIG));
    }

    #[test]
    fn accepted_tuning_resolves_a_rejection() {
        let (mut l, seen) = ledger();
        l.reconcile_where(
            config_alert(Some("pauseBudgetSecs=0"))
                .into_iter()
                .collect(),
            |r, _| r == RULE_CONFIG_REJECTED,
        );
        l.reconcile_where(config_alert(None).into_iter().collect(), |r, _| {
            r == RULE_CONFIG_REJECTED
        });
        let states: Vec<State> = seen.lock().unwrap().iter().map(|e| e.2).collect();
        assert_eq!(states, [State::Open, State::Resolved]);
    }
}
