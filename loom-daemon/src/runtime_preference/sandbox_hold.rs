//! A short, host-wide hold on a runtime whose sandbox just proved it cannot
//! run a tool call (#10003).
//!
//! The ordered preference walk ([`super::resolve`]) chooses a tap from
//! [`super::availability`], which reads credential pools only. A Codex tick
//! whose every shell command bubblewrap refused (#9979) leaves the account
//! pool perfectly healthy, so before this module the walk chose Codex again
//! on the very next tick, in every workspace, indefinitely. That is why
//! `rolePreference: ["codex","claude"]` never fell through.
//!
//! The sandbox is a property of the **host**: the session containers' Docker
//! seccomp / AppArmor profile and the host kernel's user-namespace policy.
//! It is not a property of an account or a workspace. So the hold is keyed
//! by runtime alone and kept in daemon memory. One no-op tick in any
//! workspace routes every workspace's next tick to the next tap.
//!
//! It is deliberately **self-healing and short**:
//!
//! - It ages out after [`hold_secs`] (`LOOM_RUNTIME_SANDBOX_HOLD_SECS`, default
//!   [`DEFAULT_HOLD_SECS`]). The first tick after that re-tests the sandbox
//!   on the preferred runtime and re-arms the hold if it is still broken. A
//!   repaired sandbox is therefore picked up within one hold window with no
//!   operator action.
//! - A tick on that runtime that did run a command clears it at once.
//! - A daemon restart forgets it, which costs one re-test tick.
//! - `LOOM_RUNTIME_SANDBOX_HOLD_SECS=0` disables it. Ticks are still reported
//!   as failures, but nothing falls through.
//!
//! It never overrides an operator pin: pins never reach the preference walk,
//! which is the only reader.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

/// Default hold length: long enough that a fleet-wide sandbox outage costs
/// one wasted tick per host per half hour, short enough that a repaired
/// sandbox is back in use well within a role's normal cadence.
pub const DEFAULT_HOLD_SECS: u64 = 30 * 60;

/// Environment override for [`DEFAULT_HOLD_SECS`]; `0` disables the hold.
pub const HOLD_SECS_ENV: &str = "LOOM_RUNTIME_SANDBOX_HOLD_SECS";

/// One live hold.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxHold {
    /// Epoch seconds at which the hold stops applying.
    pub until: u64,
    /// What the no-op looked like, from the tick that armed the hold.
    pub detail: String,
}

impl SandboxHold {
    /// The `detail` a passed-over tap carries into the preference marker and
    /// the role log.
    #[must_use]
    pub fn describe(&self, runtime: &str, now: u64) -> String {
        format!(
            "{runtime} runtime sandbox unavailable — its last tick ran no tool call ({}); \
             held for {}s more, then re-tested (#10003)",
            self.detail,
            self.until.saturating_sub(now)
        )
    }
}

/// The hold table. A plain value type so its rules are unit-testable without
/// touching the process-global instance the daemon uses.
#[derive(Debug, Default)]
pub struct SandboxHolds {
    holds: HashMap<String, SandboxHold>,
}

impl SandboxHolds {
    /// Arm (or re-arm) `runtime`'s hold for `secs` from `now`. `secs == 0`
    /// is a no-op: the operator disabled the hold.
    pub fn arm(&mut self, runtime: &str, detail: &str, now: u64, secs: u64) {
        if secs == 0 {
            return;
        }
        self.holds.insert(
            runtime.to_string(),
            SandboxHold {
                until: now.saturating_add(secs),
                detail: detail.to_string(),
            },
        );
    }

    /// Drop `runtime`'s hold. Returns whether one was live.
    pub fn clear(&mut self, runtime: &str) -> bool {
        self.holds.remove(runtime).is_some()
    }

    /// `runtime`'s hold if it is still live at `now`. An expired hold is
    /// removed on read, so the table never grows past the runtime count.
    pub fn active(&mut self, runtime: &str, now: u64) -> Option<SandboxHold> {
        match self.holds.get(runtime) {
            Some(hold) if hold.until > now => Some(hold.clone()),
            Some(_) => {
                self.holds.remove(runtime);
                None
            }
            None => None,
        }
    }
}

fn global() -> &'static Mutex<SandboxHolds> {
    static HOLDS: OnceLock<Mutex<SandboxHolds>> = OnceLock::new();
    HOLDS.get_or_init(|| Mutex::new(SandboxHolds::default()))
}

fn with_global<T>(f: impl FnOnce(&mut SandboxHolds) -> T) -> T {
    // A poisoned lock only means another thread panicked mid-update; the
    // table is still a valid map, so keep using it rather than failing a
    // tick over it.
    let mut guard = global()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    f(&mut guard)
}

/// The configured hold length, in seconds.
#[must_use]
pub fn hold_secs() -> u64 {
    std::env::var(HOLD_SECS_ENV)
        .ok()
        .and_then(|value| value.trim().parse().ok())
        .unwrap_or(DEFAULT_HOLD_SECS)
}

/// Arm the host-wide hold for `runtime` (see the module doc).
pub fn arm(runtime: &str, detail: &str, now: u64) {
    let secs = hold_secs();
    with_global(|holds| holds.arm(runtime, detail, now, secs));
}

/// Clear `runtime`'s hold: a tick on it just ran a tool call. Returns whether
/// one was live.
pub fn clear(runtime: &str) -> bool {
    with_global(|holds| holds.clear(runtime))
}

/// `runtime`'s live hold at `now`, if any.
#[must_use]
pub fn active(runtime: &str, now: u64) -> Option<SandboxHold> {
    with_global(|holds| holds.active(runtime, now))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_armed_hold_applies_until_it_ages_out() {
        let mut holds = SandboxHolds::default();
        holds.arm("codex", "shape=exec-denied execs=1 denied=1 succeeded=0", 1_000, 60);
        let live = holds
            .active("codex", 1_059)
            .expect("live inside the window");
        assert_eq!(live.until, 1_060);
        assert!(live.describe("codex", 1_059).contains("held for 1s more"));
        assert_eq!(holds.active("codex", 1_060), None, "expires at `until`");
        assert_eq!(holds.active("codex", 1_000), None, "an expired hold is dropped on read");
    }

    #[test]
    fn a_hold_is_per_runtime() {
        let mut holds = SandboxHolds::default();
        holds.arm("codex", "x", 0, 60);
        assert!(holds.active("codex", 1).is_some());
        assert_eq!(holds.active("claude", 1), None);
    }

    #[test]
    fn a_zero_length_hold_is_disabled() {
        let mut holds = SandboxHolds::default();
        holds.arm("codex", "x", 0, 0);
        assert_eq!(holds.active("codex", 0), None);
    }

    #[test]
    fn clear_drops_a_live_hold_and_reports_it() {
        let mut holds = SandboxHolds::default();
        holds.arm("codex", "x", 0, 60);
        assert!(holds.clear("codex"));
        assert!(!holds.clear("codex"));
        assert_eq!(holds.active("codex", 1), None);
    }

    #[test]
    fn re_arming_extends_the_window() {
        let mut holds = SandboxHolds::default();
        holds.arm("codex", "first", 0, 60);
        holds.arm("codex", "second", 50, 60);
        let live = holds.active("codex", 100).unwrap();
        assert_eq!(live.until, 110);
        assert_eq!(live.detail, "second");
    }

    #[test]
    #[serial_test::serial]
    fn hold_secs_reads_the_env_override_and_defaults_otherwise() {
        std::env::remove_var(HOLD_SECS_ENV);
        assert_eq!(hold_secs(), DEFAULT_HOLD_SECS);
        std::env::set_var(HOLD_SECS_ENV, "0");
        assert_eq!(hold_secs(), 0);
        std::env::set_var(HOLD_SECS_ENV, "not-a-number");
        assert_eq!(hold_secs(), DEFAULT_HOLD_SECS);
        std::env::remove_var(HOLD_SECS_ENV);
    }
}
