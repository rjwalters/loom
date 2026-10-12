//! A per-workspace hold on a role's tap after it failed to launch the same
//! deterministic way `N` times in a row (#11453).
//!
//! The ordered preference walk ([`super::resolve`]) passes a tap over only
//! when its credential source is dry ([`super::availability`]). A tap whose
//! launch fails *deterministically* — a toolless guarded binding (#8448), a
//! provider auth refusal, `spawn-*.sh` exiting 78 — still has a healthy pool,
//! so before this module the walk chose it again on every tick, in every
//! workspace, indefinitely, spending a seat's quota each time (#8505).
//!
//! This generalizes the Codex sandbox hold ([`super::sandbox_hold`], #10003)
//! with two differences that #10170 taught:
//!
//! - **The key is `(workspace root, role, tap)`**, never host-wide. A broken
//!   binding in one stale workspace must not starve every other workspace.
//! - **It trips on `N` identical failures** ([`threshold`],
//!   `LOOM_LAUNCH_FAILURE_HOLD_THRESHOLD`, default [`DEFAULT_THRESHOLD`]),
//!   not on one. The signature is the tick's `loom.failure_class`; a success,
//!   a transient failure or a different class resets the streak.
//!
//! Which classes count is decided by the caller against an explicit
//! allowlist (`role_runner::launch::failure_class::is_deterministic`).
//! Everything else is transient and keeps today's retry-next-tick behaviour,
//! so a misclassification fails open to the status quo.
//!
//! While a hold is live, the **role-tick** walk (and only that walk:
//! [`super::DispatchContext::role_tick`]) treats the tap as unavailable for
//! that role in that workspace and falls through `rolePreference`. The role
//! itself is never benched: it still ticks every interval, on the next tap or
//! as a cheap pre-spawn skip when every tap is held.
//!
//! It clears itself:
//!
//! - **on expiry** after [`hold_secs`] (`LOOM_LAUNCH_FAILURE_HOLD_SECS`,
//!   default [`DEFAULT_HOLD_SECS`]). The streak is kept, so the first tick
//!   after expiry re-tests the tap and one more identical failure re-arms at
//!   once: steady state is one probe launch per cooldown;
//! - **on a successful tick** on the key;
//! - **when the configuration changes**: the hold stores a [`fingerprint`] of
//!   the effective `runtimes` block and the tap's resolved model profile, and
//!   a mismatch on read drops it;
//! - **on a daemon restart** (the table lives in daemon memory).
//!
//! `LOOM_LAUNCH_FAILURE_HOLD_SECS=0` disables the feature entirely: nothing is
//! counted, nothing is consulted. Operator pins never reach the walk, so a
//! pinned role is never re-routed.

use super::resolve::Tap;
use serde_json::Value;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

/// Default cooldown: one probe launch per half hour per `(root, role, tap)`.
pub const DEFAULT_HOLD_SECS: u64 = 30 * 60;

/// Environment override for [`DEFAULT_HOLD_SECS`]; `0` disables the feature.
pub const HOLD_SECS_ENV: &str = "LOOM_LAUNCH_FAILURE_HOLD_SECS";

/// Default number of consecutive identical deterministic failures that trip.
pub const DEFAULT_THRESHOLD: u32 = 2;

/// Environment override for [`DEFAULT_THRESHOLD`]. Values below 1 read as 1.
pub const THRESHOLD_ENV: &str = "LOOM_LAUNCH_FAILURE_HOLD_THRESHOLD";

/// The wire name a held tap's skip reason carries as its `source`, so the
/// preference marker renders it as `unavailable(launch-failure-hold: …)`.
pub const SKIP_SOURCE: &str = "launch-failure-hold";

/// What a hold is keyed on. `role` is the canonical role name; `tap` is the
/// tap's `runtime[:profile]` rendering ([`Tap`]'s `Display`).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Key {
    pub root: PathBuf,
    pub role: String,
    pub tap: String,
}

impl Key {
    #[must_use]
    pub fn new(root: &Path, role: &str, tap: &Tap) -> Self {
        Self {
            root: root.to_path_buf(),
            role: role.to_string(),
            tap: tap.to_string(),
        }
    }
}

/// One live hold.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchFailureHold {
    /// The failure class that tripped it.
    pub class: String,
    /// Consecutive identical failures when it was (re-)armed.
    pub streak: u32,
    /// Epoch seconds the hold was armed.
    pub armed_at: u64,
    /// Epoch seconds at which the hold stops applying.
    pub until: u64,
    /// [`fingerprint`] of the configuration the failures happened under.
    pub fingerprint: u64,
}

impl LaunchFailureHold {
    /// The `detail` a passed-over tap carries into the preference marker:
    /// the class, the arm time and the seconds remaining.
    #[must_use]
    pub fn describe(&self, now: u64) -> String {
        let armed = i64::try_from(self.armed_at)
            .ok()
            .and_then(|secs| chrono::DateTime::from_timestamp(secs, 0))
            .map_or_else(
                || self.armed_at.to_string(),
                |at| at.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            );
        format!(
            "{} x{} armed={armed} remaining={}s #11453",
            self.class,
            self.streak,
            self.until.saturating_sub(now)
        )
    }
}

/// What a recorded deterministic failure did to its key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Recorded {
    /// The streak grew but is still below the threshold.
    Counted { streak: u32 },
    /// The streak reached the threshold and the hold was (re-)armed.
    Armed(LaunchFailureHold),
    /// The feature is disabled (`secs == 0`); nothing was recorded.
    Disabled,
}

/// Why a live hold stopped applying.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClearReason {
    Expired,
    Success,
    TransientFailure,
    ConfigChanged,
}

impl ClearReason {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Expired => "cooldown expired; the next tick re-tests the tap",
            Self::Success => "a tick on the tap succeeded",
            Self::TransientFailure => "a tick on the tap failed transiently",
            Self::ConfigChanged => "the runtimes configuration changed",
        }
    }
}

/// The answer to "is this key held right now?".
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Lookup {
    /// No hold applies.
    None,
    /// A hold is live.
    Live(LaunchFailureHold),
    /// A hold was live until this read; it is gone now.
    Cleared(ClearReason, LaunchFailureHold),
}

#[derive(Debug, Clone)]
struct Entry {
    class: String,
    streak: u32,
    hold: Option<LaunchFailureHold>,
}

/// The hold table. A plain value type so its rules are unit-testable without
/// touching the process-global instance the daemon uses.
#[derive(Debug, Default)]
pub struct LaunchFailureHolds {
    entries: HashMap<Key, Entry>,
}

impl LaunchFailureHolds {
    /// Count one deterministic failure of `class` on `key`, arming the hold
    /// when the streak of identical failures reaches `threshold`.
    pub fn record_failure(
        &mut self,
        key: &Key,
        class: &str,
        fingerprint: u64,
        now: u64,
        threshold: u32,
        secs: u64,
    ) -> Recorded {
        if secs == 0 {
            self.entries.remove(key);
            return Recorded::Disabled;
        }
        let entry = self.entries.entry(key.clone()).or_insert_with(|| Entry {
            class: class.to_string(),
            streak: 0,
            hold: None,
        });
        if entry.class != class {
            *entry = Entry {
                class: class.to_string(),
                streak: 0,
                hold: None,
            };
        }
        entry.streak = entry.streak.saturating_add(1);
        if entry.streak < threshold.max(1) {
            return Recorded::Counted {
                streak: entry.streak,
            };
        }
        let hold = LaunchFailureHold {
            class: class.to_string(),
            streak: entry.streak,
            armed_at: now,
            until: now.saturating_add(secs),
            fingerprint,
        };
        entry.hold = Some(hold.clone());
        Recorded::Armed(hold)
    }

    /// Reset `key`'s streak: its tick succeeded (`Success`) or failed
    /// transiently (`TransientFailure`). Returns the hold that was live, if
    /// any, so the caller can log the clear once.
    pub fn reset(&mut self, key: &Key, now: u64) -> Option<LaunchFailureHold> {
        self.entries
            .remove(key)
            .and_then(|entry| entry.hold)
            .filter(|hold| hold.until > now)
    }

    /// `key`'s hold at `now`. `fingerprint` is evaluated only when a hold
    /// exists. An expired hold is removed on read (the streak is kept, so one
    /// more identical failure re-arms); a fingerprint mismatch drops the
    /// hold and the streak.
    pub fn lookup(&mut self, key: &Key, fingerprint: impl FnOnce() -> u64, now: u64) -> Lookup {
        let Some(entry) = self.entries.get_mut(key) else {
            return Lookup::None;
        };
        let Some(hold) = entry.hold.clone() else {
            return Lookup::None;
        };
        if hold.until <= now {
            entry.hold = None;
            return Lookup::Cleared(ClearReason::Expired, hold);
        }
        if hold.fingerprint != fingerprint() {
            self.entries.remove(key);
            return Lookup::Cleared(ClearReason::ConfigChanged, hold);
        }
        Lookup::Live(hold)
    }

    /// How many keys the table tracks (tests and diagnostics).
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the table tracks no key.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// A fingerprint of everything that, when an operator changes it, should
/// re-test a held tap at once: the effective `runtimes` block and the tap's
/// resolved model profile (a native runtime's profile resolves through
/// `runtimes.modelProfiles`, else the bundled set).
#[must_use]
pub fn fingerprint(config: &Value, tap: &Tap) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    config
        .get("runtimes")
        .map(Value::to_string)
        .hash(&mut hasher);
    tap.to_string().hash(&mut hasher);
    if crate::worker_spawn::is_native(&tap.runtime) || tap.model_profile.is_some() {
        crate::worker_spawn::profiles::lookup(tap.model_profile.as_deref(), config)
            .ok()
            .map(|(name, profile)| format!("{name}={profile:?}"))
            .hash(&mut hasher);
    }
    hasher.finish()
}

/// The configured cooldown, in seconds. `0` disables the feature.
#[must_use]
pub fn hold_secs() -> u64 {
    std::env::var(HOLD_SECS_ENV)
        .ok()
        .and_then(|value| value.trim().parse().ok())
        .unwrap_or(DEFAULT_HOLD_SECS)
}

/// The configured trip threshold (at least 1).
#[must_use]
pub fn threshold() -> u32 {
    std::env::var(THRESHOLD_ENV)
        .ok()
        .and_then(|value| value.trim().parse::<u32>().ok())
        .unwrap_or(DEFAULT_THRESHOLD)
        .max(1)
}

fn global() -> &'static Mutex<LaunchFailureHolds> {
    static HOLDS: OnceLock<Mutex<LaunchFailureHolds>> = OnceLock::new();
    HOLDS.get_or_init(|| Mutex::new(LaunchFailureHolds::default()))
}

fn with_global<T>(f: impl FnOnce(&mut LaunchFailureHolds) -> T) -> T {
    // A poisoned lock only means another thread panicked mid-update; the
    // table is still a valid map, so keep using it.
    let mut guard = global()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    f(&mut guard)
}

fn log_cleared(key: &Key, reason: ClearReason, hold: &LaunchFailureHold) {
    log::info!(
        "launch-failure hold cleared — root={} role={} tap={} class={}: {} (#11453)",
        key.root.display(),
        key.role,
        key.tap,
        hold.class,
        reason.as_str()
    );
}

/// Record one deterministic launch failure of `class` on `key`. Logs one
/// `WARN` when this failure arms the hold.
pub fn record_failure(key: &Key, class: &str, fingerprint: u64, now: u64) -> Recorded {
    let secs = hold_secs();
    let threshold = threshold();
    let recorded =
        with_global(|holds| holds.record_failure(key, class, fingerprint, now, threshold, secs));
    if let Recorded::Armed(hold) = &recorded {
        log::warn!(
            "launch-failure hold armed — root={} role={} tap={} class={} streak={} \
             cooldown={secs}s: this role's preference walk in this workspace skips the tap \
             and falls through rolePreference until the cooldown ends, a tick on it \
             succeeds, or the runtimes config changes. Remedy: fix the tap's launch (see \
             .loom/logs/role-{}.log), or set {HOLD_SECS_ENV}=0 to disable (#11453)",
            key.root.display(),
            key.role,
            key.tap,
            hold.class,
            hold.streak,
            key.role,
        );
    }
    recorded
}

/// Reset `key`'s streak after a successful tick (`Success`) or a transient
/// failure (`TransientFailure`), logging one `INFO` if a hold was live.
pub fn record_reset(key: &Key, reason: ClearReason, now: u64) {
    if hold_secs() == 0 {
        return;
    }
    if let Some(hold) = with_global(|holds| holds.reset(key, now)) {
        log_cleared(key, reason, &hold);
    }
}

/// `key`'s live hold at `now`, if any. Logs one `INFO` when this read is the
/// one that finds the hold expired or its configuration changed.
#[must_use]
pub fn active(key: &Key, fingerprint: impl FnOnce() -> u64, now: u64) -> Option<LaunchFailureHold> {
    if hold_secs() == 0 {
        return None;
    }
    match with_global(|holds| holds.lookup(key, fingerprint, now)) {
        Lookup::Live(hold) => Some(hold),
        Lookup::Cleared(reason, hold) => {
            log_cleared(key, reason, &hold);
            None
        }
        Lookup::None => None,
    }
}

/// Forget every key (test hygiene: the table is process-global).
#[cfg(test)]
pub fn reset_for_tests() {
    with_global(|holds| *holds = LaunchFailureHolds::default());
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests;
