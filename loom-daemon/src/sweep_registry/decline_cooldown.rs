//! Hard-exclusion **decline** cooldown (Issue #7528).
//!
//! # The gap
//!
//! The reaper's checkpoint-less clean-exit path ("orphaned-claim recovery",
//! #3823b) restores `loom:building -> loom:issue` for **any** sweep that exits
//! 0 without ever writing a checkpoint. That is correct for the case it was
//! written for — a self-skip / no-work exit whose claim would otherwise be
//! stranded — but it makes no distinction for *why* the sweep exited, and one
//! of those reasons is permanent: a Curator/Builder that declines immediately
//! on a hard-exclusion label rule
//! ([`crate::hard_exclusion::HARD_EXCLUSION_LABELS`], `external` today).
//!
//! Restoring `loom:issue` for that shape puts the issue straight back into the
//! candidate pool, where the very next tick re-dispatches it, it declines
//! again for the identical reason, and the loop repeats — 23 dispatches in
//! roughly one hour on rjwalters/kicad-tools#5197, each burning ~90s of a
//! token account's session budget, fastest exactly when the pool is thinnest.
//! The existing insta-crash/no-progress quarantine
//! ([`super::quarantine`]) does eventually catch it via the `no_progress`
//! verdict, but only after three wasted dispatches and only for one
//! quarantine TTL before the loop resumes.
//!
//! # The mechanism
//!
//! [`crate::work_finder`]'s candidate filter now drops hard-excluded issues
//! before they are ever dispatched, which is the primary fix. This module is
//! the **backstop** for every path that filter does not cover — a label added
//! *after* dispatch, an explicit `dispatch_sweep`/CLI dispatch, a watchdog or
//! reaper-driven resume — plus the observability the loop was missing.
//!
//! On a checkpoint-less clean exit the reaper probes the issue's labels once
//! and, when it finds a hard-exclusion label, calls
//! [`SweepRegistry::record_decline`]: the claim is still restored exactly as
//! before (leaving a stranded `loom:building` would trade one bug for the very
//! bug #3823b fixed), but a cooldown window is armed so the work finder skips
//! the candidate instead of re-offering it next tick, and the decline is
//! counted so N consecutive declines emit one WARN naming the issue and the
//! rule.
//!
//! # Deliberate design choices
//!
//! - **Inferred, not self-reported.** Unlike [`super::noop_cooldown`] (a
//!   call-through-only signal from the sweep itself), the discriminator here is
//!   a *fact on the forge* — the issue carries a hard-exclusion label — so the
//!   reaper can detect it deterministically without any cooperation from a
//!   markdown-prompted agent session. A signal channel that depends on the
//!   declining agent remembering to call a CLI is exactly as reliable as the
//!   role prompt that produced the loop in the first place.
//! - **Never clears on dispatch.** [`super::noop_cooldown`] clears its record
//!   on any fresh dispatch; this one must not, because the decline is recorded
//!   *after* the dispatch that produced it — clearing on dispatch would reset
//!   the consecutive tally on every cycle and the WARN at N would never fire.
//!   The record is cleared only by evidence the rule no longer applies: a
//!   checkpoint-less clean exit that is NOT a decline, or a run that made real
//!   checkpoint progress.
//! - **Not fleet-broadcast.** [`super::noop_cooldown`]'s window has to be
//!   advertised over the peer-claim channel (#7477) because "this sweep found
//!   nothing to do" is private knowledge. A hard-exclusion label is not: every
//!   host in the fleet reads the same labels off the same forge and reaches the
//!   same verdict in its own candidate filter, so there is nothing to share.
//! - **Not a config-weakenable exclusion.** The cooldown *duration* is
//!   configurable (env > config > default, like every sibling); the label list
//!   it keys on is not — see [`crate::hard_exclusion`].

use super::*;

/// Env var toggling the hard-exclusion decline cooldown (Issue #7528).
/// `0`/`false`/`no`/`off` disables; `1`/`true`/`yes`/`on` forces on. Overrides
/// config. Defaults ON, like [`super::noop_cooldown::NOOP_COOLDOWN_ENABLE_ENV`]
/// — it is a dispatch-efficiency backstop that never holds an issue for longer
/// than [`DeclineCooldownConfig::cooldown`].
pub const DECLINE_COOLDOWN_ENABLE_ENV: &str = "LOOM_WORK_FINDER_DECLINE_COOLDOWN";

/// Env var overriding the decline cooldown duration, in seconds (Issue #7528).
/// A zero/invalid value falls through to config/default.
pub const DECLINE_COOLDOWN_SECS_ENV: &str = "LOOM_WORK_FINDER_DECLINE_COOLDOWN_SECS";

/// Env var overriding how many consecutive declines of the same issue emit the
/// WARN (Issue #7528). A zero/invalid value falls through to config/default.
pub const DECLINE_WARN_THRESHOLD_ENV: &str = "LOOM_WORK_FINDER_DECLINE_WARN_THRESHOLD";

/// Default decline cooldown duration (#7528): six hours.
///
/// Deliberately **longer** than [`super::noop_cooldown::DEFAULT_NOOP_COOLDOWN_SECS`]
/// (one hour). A no-op release means "nothing to do *yet*" — a genuinely
/// transient state worth re-checking hourly. A hard-exclusion decline means a
/// label is present that only a maintainer can remove, so re-checking it
/// hourly is 24 wasted agent sessions a day for a state that changes on human
/// timescales. Six hours keeps the issue visibly re-evaluated within a working
/// day without paying tick-cadence prices for it.
pub const DEFAULT_DECLINE_COOLDOWN_SECS: u64 = 21_600;

/// Default consecutive-decline count that emits the WARN (#7528). Matches
/// [`super::quarantine::DEFAULT_QUARANTINE_THRESHOLD`]: three identical
/// outcomes is the repo's established "this is a pattern, not a blip"
/// threshold.
pub const DEFAULT_DECLINE_WARN_THRESHOLD: u32 = 3;

/// Resolved decline-cooldown parameters (Issue #7528), set on the registry at
/// construction so the work finder can enforce them without a per-tick config
/// read — mirrors [`NoopCooldownConfig`]'s shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeclineCooldownConfig {
    /// Whether the decline cooldown is active. When `false`, recording a
    /// decline is a no-op (no state written, no cooldown armed, no WARN) —
    /// byte-for-byte the pre-#7528 path.
    pub enabled: bool,
    /// How long a recorded decline holds the issue out of dispatch.
    pub cooldown: Duration,
    /// How many consecutive declines of one issue emit the WARN.
    pub warn_threshold: u32,
}

impl Default for DeclineCooldownConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            cooldown: Duration::from_secs(DEFAULT_DECLINE_COOLDOWN_SECS),
            warn_threshold: DEFAULT_DECLINE_WARN_THRESHOLD,
        }
    }
}

/// Per-issue decline bookkeeping (Issue #7528).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DeclineCooldownState {
    /// When this cooldown window was (most recently) armed.
    pub(crate) recorded_at: DateTime<Utc>,
    /// The instant at which the next dispatch attempt becomes allowed.
    pub(crate) until: DateTime<Utc>,
    /// Consecutive declines recorded for this issue. Deliberately NOT cleared
    /// by a fresh dispatch (see the module doc): only
    /// [`SweepRegistry::clear_decline_cooldown`] resets it, and that is called
    /// on positive evidence the rule no longer applies.
    pub(crate) consecutive: u32,
    /// The hard-exclusion label the sweep declined on, quoted in the WARN.
    pub(crate) rule: String,
}

/// The subset of `.loom/config.json → autonomous.workFinder.declineCooldown`
/// this module consumes (Issue #7528). Every field is `Option` so an absent
/// key falls through to the env-var / built-in-default resolution —
/// precedence **env > config > default**, mirroring
/// [`NoopCooldownFileConfig`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DeclineCooldownFileConfig {
    /// `autonomous.workFinder.declineCooldown.enabled`.
    pub enabled: Option<bool>,
    /// `autonomous.workFinder.declineCooldown.cooldownSecs` (zero/invalid dropped).
    pub cooldown_secs: Option<u64>,
    /// `autonomous.workFinder.declineCooldown.warnThreshold` (zero/invalid dropped).
    pub warn_threshold: Option<u32>,
}

/// Read `.loom/config.json → autonomous.workFinder.declineCooldown` (Issue
/// #7528), soft-failing every field to `None` on a missing file, malformed
/// JSON, or an absent block — mirrors [`read_noop_cooldown_file_config`].
#[must_use]
pub fn read_decline_cooldown_file_config(repo_root: &Path) -> DeclineCooldownFileConfig {
    let effective = crate::config_resolver::resolve_effective_config(repo_root);
    let Some(c) =
        crate::config_resolver::get_path(&effective, "autonomous.workFinder.declineCooldown")
    else {
        return DeclineCooldownFileConfig::default();
    };
    DeclineCooldownFileConfig {
        enabled: c.get("enabled").and_then(serde_json::Value::as_bool),
        cooldown_secs: c
            .get("cooldownSecs")
            .and_then(serde_json::Value::as_u64)
            .filter(|&s| s > 0),
        warn_threshold: c
            .get("warnThreshold")
            .and_then(serde_json::Value::as_u64)
            .filter(|&s| s > 0)
            .and_then(|s| u32::try_from(s).ok()),
    }
}

/// Resolve the full [`DeclineCooldownConfig`] for `repo_root` with precedence
/// **env > config > default** for every knob (Issue #7528), mirroring
/// [`resolve_noop_cooldown_config`].
#[must_use]
pub fn resolve_decline_cooldown_config(repo_root: &Path) -> DeclineCooldownConfig {
    let file = read_decline_cooldown_file_config(repo_root);

    let enabled = if let Ok(v) = std::env::var(DECLINE_COOLDOWN_ENABLE_ENV) {
        matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on")
    } else {
        file.enabled.unwrap_or(true)
    };

    let cooldown_secs = std::env::var(DECLINE_COOLDOWN_SECS_ENV)
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|&s| s > 0)
        .or(file.cooldown_secs)
        .unwrap_or(DEFAULT_DECLINE_COOLDOWN_SECS);

    let warn_threshold = std::env::var(DECLINE_WARN_THRESHOLD_ENV)
        .ok()
        .and_then(|v| v.trim().parse::<u32>().ok())
        .filter(|&s| s > 0)
        .or(file.warn_threshold)
        .unwrap_or(DEFAULT_DECLINE_WARN_THRESHOLD);

    DeclineCooldownConfig {
        enabled,
        cooldown: Duration::from_secs(cooldown_secs),
        warn_threshold,
    }
}

impl SweepRegistry {
    /// Set the decline-cooldown parameters (Issue #7528). `daemon_service.rs`
    /// and the workspace pool call this once at provision time with the
    /// resolved env > config > default value, mirroring
    /// [`Self::set_noop_cooldown_config`].
    pub fn set_decline_cooldown_config(&mut self, config: DeclineCooldownConfig) {
        self.decline_cooldown_config = config;
    }

    /// Read-only accessor for the decline-cooldown parameters (Issue #7528).
    #[must_use]
    pub fn decline_cooldown_config(&self) -> DeclineCooldownConfig {
        self.decline_cooldown_config
    }

    /// Record that a sweep for `issue` **declined on the hard-exclusion rule
    /// `rule`** (Issue #7528) and arm (or re-arm) the cooldown window that
    /// keeps the work finder from re-offering it next tick.
    ///
    /// Called from [`Self::reap_once`]'s checkpoint-less clean-exit path when
    /// the issue is verified to carry a
    /// [`crate::hard_exclusion::HARD_EXCLUSION_LABELS`] entry. The claim
    /// restore itself is unchanged — this only adds the brake.
    ///
    /// Emits exactly one WARN naming the issue and the rule, on the pass where
    /// the consecutive count reaches
    /// [`DeclineCooldownConfig::warn_threshold`]; every other pass logs at
    /// INFO. A no-op when the mechanism is disabled, mirroring
    /// [`Self::record_noop_release`]'s disabled-path contract.
    pub(crate) fn record_decline(&mut self, issue: u32, rule: &str) {
        if !self.decline_cooldown_config.enabled {
            return;
        }
        let now = Utc::now();
        let consecutive = self
            .decline_cooldown
            .get(&issue)
            .map_or(1, |prev| prev.consecutive.saturating_add(1));
        let until = now
            + chrono::Duration::from_std(self.decline_cooldown_config.cooldown)
                .unwrap_or_else(|_| chrono::Duration::zero());
        let secs = self.decline_cooldown_config.cooldown.as_secs();
        if consecutive == self.decline_cooldown_config.warn_threshold {
            log::warn!(
                "sweep_registry: issue #{issue} has now been declined {consecutive} times in a \
                 row on the hard-exclusion rule `{rule}` — a maintainer must remove `{rule}` \
                 (or close the issue) before any sweep can build it; holding it out of dispatch \
                 for {secs}s at a time until then (#7528)"
            );
        } else {
            log::info!(
                "sweep_registry: issue #{issue} declined on the hard-exclusion rule `{rule}` \
                 ({consecutive} consecutive) — decline cooldown armed, next dispatch allowed in \
                 {secs}s (#7528)"
            );
        }
        self.decline_cooldown.insert(
            issue,
            DeclineCooldownState {
                recorded_at: now,
                until,
                consecutive,
                rule: rule.to_string(),
            },
        );
    }

    /// Clear `issue`'s decline record (Issue #7528). Returns `true` when a
    /// record existed.
    ///
    /// Called only on **positive evidence the rule no longer applies**: a
    /// checkpoint-less clean exit that was NOT a hard-exclusion decline, or a
    /// run that made real checkpoint progress. Deliberately NOT called from
    /// [`Self::dispatch`] — see the module doc: the decline is recorded after
    /// the dispatch that produced it, so a dispatch-time clear would reset the
    /// consecutive tally every cycle and the threshold WARN could never fire.
    pub(crate) fn clear_decline_cooldown(&mut self, issue: u32) -> bool {
        self.decline_cooldown.remove(&issue).is_some()
    }

    /// Remaining decline cooldown for `issue` at `now` (Issue #7528), or
    /// `None` when it may be dispatched immediately. `Some(Duration::ZERO)` is
    /// never returned — an elapsed window reads as `None`. Mirrors
    /// [`Self::noop_cooldown_remaining`].
    #[must_use]
    pub fn decline_cooldown_remaining(&self, issue: u32, now: DateTime<Utc>) -> Option<Duration> {
        if !self.decline_cooldown_config.enabled {
            return None;
        }
        let state = self.decline_cooldown.get(&issue)?;
        let remaining = state.until - now;
        if remaining <= chrono::Duration::zero() {
            return None;
        }
        remaining.to_std().ok().filter(|d| !d.is_zero())
    }

    /// Consecutive declines recorded for `issue` (Issue #7528). `0` when no
    /// decline is on record. Test/inspection helper, mirroring
    /// [`Self::noop_release_count`].
    #[must_use]
    pub fn decline_count(&self, issue: u32) -> u32 {
        self.decline_cooldown
            .get(&issue)
            .map_or(0, |s| s.consecutive)
    }

    /// The hard-exclusion rule `issue` last declined on (Issue #7528), or
    /// `None` when no decline is on record.
    #[must_use]
    pub fn decline_rule(&self, issue: u32) -> Option<String> {
        self.decline_cooldown.get(&issue).map(|s| s.rule.clone())
    }

    /// Every issue whose decline cooldown is still in effect at `now` (Issue
    /// #7528) — the set the work finder skips *before* the capacity gate,
    /// mirroring [`Self::noop_cooldown_issues`].
    ///
    /// Not unioned with a peer-observed view (unlike `noop_cooldown_issues`,
    /// #7477): a hard-exclusion label is public forge state every host reads
    /// for itself in its own candidate filter, so there is no host-private
    /// window to advertise.
    #[must_use]
    pub fn decline_cooldown_issues(&self, now: DateTime<Utc>) -> HashSet<u32> {
        if !self.decline_cooldown_config.enabled {
            return HashSet::new();
        }
        self.decline_cooldown
            .iter()
            .filter(|(_, s)| s.until > now)
            .map(|(issue, _)| *issue)
            .collect()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::sweep_registry::{SweepRegistry, SweepRegistryConfig};
    use serial_test::serial;
    use tempfile::tempdir;

    fn test_registry() -> SweepRegistry {
        let dir = tempdir().unwrap();
        SweepRegistry::new(SweepRegistryConfig::new(dir.path().to_path_buf()))
    }

    #[test]
    fn record_arms_cooldown_and_is_reflected_in_skip_set() {
        let mut reg = test_registry();
        assert_eq!(reg.decline_count(5197), 0);
        assert!(reg.decline_cooldown_remaining(5197, Utc::now()).is_none());

        reg.record_decline(5197, "external");

        assert_eq!(reg.decline_count(5197), 1);
        assert_eq!(reg.decline_rule(5197).as_deref(), Some("external"));
        assert!(reg
            .decline_cooldown_remaining(5197, Utc::now())
            .is_some_and(|d| d.as_secs() > 0));
        assert!(reg.decline_cooldown_issues(Utc::now()).contains(&5197));
    }

    /// #7528 AC: an issue with `loom:issue` + an excluded label is dispatched
    /// **at most once per cooldown**, not once per tick. The registry half of
    /// that: a recorded decline keeps the issue in the skip set for the whole
    /// window and only leaves it when the window elapses.
    #[test]
    fn issue_is_held_for_the_whole_cooldown_then_becomes_eligible_again() {
        let mut reg = test_registry();
        reg.set_decline_cooldown_config(DeclineCooldownConfig {
            enabled: true,
            cooldown: Duration::from_secs(600),
            warn_threshold: 3,
        });
        reg.record_decline(42, "external");

        // Every "tick" inside the window sees the issue as skipped.
        for minutes in [0, 1, 5, 9] {
            let t = Utc::now() + chrono::Duration::minutes(minutes);
            assert!(
                reg.decline_cooldown_issues(t).contains(&42),
                "still inside the 600s window at +{minutes}m"
            );
        }
        let past = Utc::now() + chrono::Duration::minutes(11);
        assert!(!reg.decline_cooldown_issues(past).contains(&42));
        assert!(reg.decline_cooldown_remaining(42, past).is_none());
    }

    #[test]
    fn repeated_records_accumulate_consecutive_but_stay_flat() {
        let mut reg = test_registry();
        reg.record_decline(99, "external");
        reg.record_decline(99, "external");
        reg.record_decline(99, "external");
        assert_eq!(reg.decline_count(99), 3);
        // Flat, non-exponential: the window is always exactly the configured
        // cooldown from the most recent record, never longer.
        let remaining = reg.decline_cooldown_remaining(99, Utc::now()).unwrap();
        assert!(remaining.as_secs() <= reg.decline_cooldown_config().cooldown.as_secs());
    }

    #[test]
    fn clear_removes_the_record() {
        let mut reg = test_registry();
        reg.record_decline(55, "external");
        assert!(reg.decline_cooldown_remaining(55, Utc::now()).is_some());
        assert!(reg.clear_decline_cooldown(55));
        assert_eq!(reg.decline_count(55), 0);
        assert!(reg.decline_cooldown_remaining(55, Utc::now()).is_none());
        assert!(!reg.clear_decline_cooldown(55), "second clear is a no-op");
    }

    #[test]
    fn disabled_mechanism_records_nothing() {
        let mut reg = test_registry();
        reg.set_decline_cooldown_config(DeclineCooldownConfig {
            enabled: false,
            cooldown: Duration::from_secs(60),
            warn_threshold: 3,
        });
        reg.record_decline(1, "external");
        assert_eq!(reg.decline_count(1), 0);
        assert!(reg.decline_cooldown_remaining(1, Utc::now()).is_none());
        assert!(reg.decline_cooldown_issues(Utc::now()).is_empty());
    }

    /// The three existing brakes and this one are independent: an issue can be
    /// in all four states at once and clearing one leaves the others alone.
    /// Mirrors `noop_cooldown`'s own independence test.
    #[test]
    fn independent_of_quarantine_backoff_and_noop_cooldown() {
        let mut reg = test_registry();
        reg.seed_quarantine_for_test(321);
        reg.record_dispatch_failure(321);
        reg.record_noop_release(321, None);
        reg.record_decline(321, "external");

        assert!(reg.is_quarantined(321));
        assert!(reg.dispatch_backoff_remaining(321, Utc::now()).is_some());
        assert!(reg.noop_cooldown_remaining(321, Utc::now()).is_some());
        assert!(reg.decline_cooldown_remaining(321, Utc::now()).is_some());

        assert!(reg.clear_decline_cooldown(321));
        assert!(reg.is_quarantined(321));
        assert!(reg.dispatch_backoff_remaining(321, Utc::now()).is_some());
        assert!(reg.noop_cooldown_remaining(321, Utc::now()).is_some());
        assert!(reg.decline_cooldown_remaining(321, Utc::now()).is_none());
    }

    /// The WARN fires on the pass that reaches the threshold, and the record
    /// keeps accruing past it (so a long-lived excluded issue does not reset
    /// itself and re-warn every three passes).
    #[test]
    fn consecutive_tally_crosses_the_warn_threshold_exactly_once() {
        let mut reg = test_registry();
        reg.set_decline_cooldown_config(DeclineCooldownConfig {
            enabled: true,
            cooldown: Duration::from_secs(60),
            warn_threshold: 2,
        });
        reg.record_decline(7, "external");
        assert_eq!(reg.decline_count(7), 1);
        reg.record_decline(7, "external"); // == threshold: the WARN pass
        assert_eq!(reg.decline_count(7), 2);
        reg.record_decline(7, "external");
        assert_eq!(reg.decline_count(7), 3, "tally keeps growing past the WARN");
    }

    // --- Config resolution precedence (env > config > default) -------------

    #[test]
    #[serial]
    fn resolve_uses_shipped_defaults_with_no_env_or_file() {
        let dir = tempdir().unwrap();
        std::env::remove_var(DECLINE_COOLDOWN_ENABLE_ENV);
        std::env::remove_var(DECLINE_COOLDOWN_SECS_ENV);
        std::env::remove_var(DECLINE_WARN_THRESHOLD_ENV);
        let cfg = resolve_decline_cooldown_config(dir.path());
        assert!(cfg.enabled);
        assert_eq!(cfg.cooldown.as_secs(), DEFAULT_DECLINE_COOLDOWN_SECS);
        assert_eq!(cfg.warn_threshold, DEFAULT_DECLINE_WARN_THRESHOLD);
    }

    #[test]
    #[serial]
    fn resolve_file_config_overrides_default() {
        let dir = tempdir().unwrap();
        std::env::remove_var(DECLINE_COOLDOWN_ENABLE_ENV);
        std::env::remove_var(DECLINE_COOLDOWN_SECS_ENV);
        std::env::remove_var(DECLINE_WARN_THRESHOLD_ENV);
        std::fs::create_dir_all(dir.path().join(".loom")).unwrap();
        std::fs::write(
            dir.path().join(".loom/config.json"),
            r#"{"autonomous":{"workFinder":{"declineCooldown":{"enabled":false,"cooldownSecs":120,"warnThreshold":9}}}}"#,
        )
        .unwrap();
        let cfg = resolve_decline_cooldown_config(dir.path());
        assert!(!cfg.enabled);
        assert_eq!(cfg.cooldown.as_secs(), 120);
        assert_eq!(cfg.warn_threshold, 9);
    }

    #[test]
    #[serial]
    fn resolve_env_overrides_file() {
        let dir = tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".loom")).unwrap();
        std::fs::write(
            dir.path().join(".loom/config.json"),
            r#"{"autonomous":{"workFinder":{"declineCooldown":{"enabled":false,"cooldownSecs":120,"warnThreshold":9}}}}"#,
        )
        .unwrap();
        std::env::set_var(DECLINE_COOLDOWN_ENABLE_ENV, "1");
        std::env::set_var(DECLINE_COOLDOWN_SECS_ENV, "42");
        std::env::set_var(DECLINE_WARN_THRESHOLD_ENV, "4");
        let cfg = resolve_decline_cooldown_config(dir.path());
        std::env::remove_var(DECLINE_COOLDOWN_ENABLE_ENV);
        std::env::remove_var(DECLINE_COOLDOWN_SECS_ENV);
        std::env::remove_var(DECLINE_WARN_THRESHOLD_ENV);
        assert!(cfg.enabled);
        assert_eq!(cfg.cooldown.as_secs(), 42);
        assert_eq!(cfg.warn_threshold, 4);
    }

    #[test]
    #[serial]
    fn zero_or_invalid_env_values_fall_through() {
        let dir = tempdir().unwrap();
        std::env::set_var(DECLINE_COOLDOWN_SECS_ENV, "0");
        std::env::set_var(DECLINE_WARN_THRESHOLD_ENV, "not-a-number");
        let cfg = resolve_decline_cooldown_config(dir.path());
        std::env::remove_var(DECLINE_COOLDOWN_SECS_ENV);
        std::env::remove_var(DECLINE_WARN_THRESHOLD_ENV);
        assert_eq!(cfg.cooldown.as_secs(), DEFAULT_DECLINE_COOLDOWN_SECS);
        assert_eq!(cfg.warn_threshold, DEFAULT_DECLINE_WARN_THRESHOLD);
    }
}
