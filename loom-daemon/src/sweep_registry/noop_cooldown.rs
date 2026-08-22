//! No-op re-dispatch cooldown (Issue #6670).
//!
//! # The gap
//!
//! The insta-crash quarantine ([`super::quarantine`]) and the per-issue
//! dispatch backoff ([`super::dispatch`]) both bound how often a *failing*
//! issue is re-dispatched. Neither covers a sweep that runs to a clean,
//! **successful** conclusion and finds nothing to do: a phase checkpoint was
//! written, the `loom:building` claim was released cleanly back to
//! `loom:issue`, and zero forge/issue mutation happened — a standing/tracking
//! issue with no pending content change is the canonical shape. That sweep
//! looks identical to a normal, healthy completion from the reaper's point of
//! view, so it trips neither existing brake, and the work finder re-offers
//! the same candidate on the very next tick — burning a dispatch slot and a
//! full agent session purely to re-confirm "still nothing to do" (observed:
//! ~20 claim/release cycles over 4 hours on one issue).
//!
//! # The mechanism
//!
//! This module gives a sweep a way to signal that outcome back to the daemon
//! — "no actionable delta this pass" — and lets the work finder apply a
//! cooldown before re-offering the same candidate, **independent of and
//! non-interfering with** both the insta-crash quarantine and the dispatch
//! backoff (a distinct in-memory map, a distinct config block, a distinct
//! work-finder skip-set).
//!
//! Unlike the quarantine's crash *tally* (N consecutive insta-crashes before
//! quarantine engages) this is a single-shot arm: a caller that itself
//! concluded "no actionable delta" has already assessed the ENTIRE candidate
//! — repeating that assessment inside the daemon would just duplicate the
//! sweep-side re-derivation logic issue #6670 explicitly says is not the
//! daemon's job. The daemon takes the sweep's self-report at face value.
//!
//! There is deliberately no daemon-automatic path that infers this state
//! (e.g. from a checkpoint field): the signal is call-through only, mirroring
//! [`super::dispatch::record_dispatch_failure`]'s IPC-reachable sibling
//! (`RecordDispatchFailure`) rather than the reaper's automatic
//! terminal-outcome classification. A sweep (or the `/loom:sweep` orchestrator
//! driving it) that reaches "no actionable delta, releasing the claim" calls
//! `loom-daemon noop-cooldown record --issue <N>` before exiting, exactly the
//! way `build-gate.sh` calls `loom-daemon dispatch-backoff record` on a step
//! timeout (Issue #6192).

use super::*;

/// Env var toggling the no-op re-dispatch cooldown (Issue #6670). `0`/`false`/
/// `no`/`off` disables; `1`/`true`/`yes`/`on` forces on. Overrides config.
/// Defaults ON — like quarantine/dispatch-backoff it is a dispatch-efficiency
/// backstop, and (like dispatch backoff, unlike quarantine) it never blocks an
/// issue for longer than [`NoopCooldownConfig::cooldown`].
pub const NOOP_COOLDOWN_ENABLE_ENV: &str = "LOOM_WORK_FINDER_NOOP_COOLDOWN";

/// Env var overriding the no-op cooldown duration, in seconds (Issue #6670). A
/// zero/invalid value falls through to config/default.
pub const NOOP_COOLDOWN_SECS_ENV: &str = "LOOM_WORK_FINDER_NOOP_COOLDOWN_SECS";

/// Default no-op cooldown duration (#6670): one hour. Matches
/// [`super::quarantine::DEFAULT_QUARANTINE_TTL_SECS`] deliberately — the
/// observed incident's own re-verification cadence (an external ref that had
/// not moved in an hour) makes an hour a reasonable default "come back later"
/// window without a repo-specific tuning pass.
pub const DEFAULT_NOOP_COOLDOWN_SECS: u64 = 3600;

/// Resolved no-op-cooldown parameters (Issue #6670), set on the registry at
/// construction so the work finder can enforce them without a per-tick config
/// read — mirrors [`QuarantineConfig`] / [`DispatchBackoffConfig`]'s shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NoopCooldownConfig {
    /// Whether the no-op cooldown is active. When `false`, recording a no-op
    /// release is a no-op (no state written, no cooldown armed) — byte-for-byte
    /// the pre-#6670 path.
    pub enabled: bool,
    /// How long a recorded no-op release holds the issue out of dispatch.
    pub cooldown: Duration,
}

impl Default for NoopCooldownConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            cooldown: Duration::from_secs(DEFAULT_NOOP_COOLDOWN_SECS),
        }
    }
}

/// Per-issue no-op-cooldown bookkeeping (Issue #6670).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NoopCooldownState {
    /// When this cooldown window was (most recently) armed.
    pub(crate) recorded_at: DateTime<Utc>,
    /// The instant at which the next dispatch attempt becomes allowed.
    pub(crate) until: DateTime<Utc>,
    /// Consecutive no-op releases recorded for this issue without an
    /// intervening real dispatch (cleared by [`SweepRegistry::clear_noop_cooldown`]
    /// — including the automatic clear on a successful non-no-op dispatch, see
    /// [`SweepRegistry::dispatch`]). Observability only; never affects the
    /// (flat, non-exponential) cooldown duration.
    pub(crate) consecutive: u32,
    /// Free-form context supplied by the recording caller, logged verbatim.
    pub(crate) reason: Option<String>,
}

/// The subset of `.loom/config.json → autonomous.workFinder.noopCooldown`
/// this module consumes (Issue #6670). Mirrors [`DispatchBackoffFileConfig`]'s
/// shape: every field is `Option` so an absent key falls through to the
/// env-var / built-in-default resolution — precedence **env > config >
/// default**.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NoopCooldownFileConfig {
    /// `autonomous.workFinder.noopCooldown.enabled`.
    pub enabled: Option<bool>,
    /// `autonomous.workFinder.noopCooldown.cooldownSecs` (zero/invalid dropped).
    pub cooldown_secs: Option<u64>,
}

/// Read `.loom/config.json → autonomous.workFinder.noopCooldown` (Issue
/// #6670), soft-failing every field to `None` on a missing file, malformed
/// JSON, or an absent block — mirrors [`read_dispatch_backoff_file_config`].
#[must_use]
pub fn read_noop_cooldown_file_config(repo_root: &Path) -> NoopCooldownFileConfig {
    let effective = crate::config_resolver::resolve_effective_config(repo_root);
    let Some(c) =
        crate::config_resolver::get_path(&effective, "autonomous.workFinder.noopCooldown")
    else {
        return NoopCooldownFileConfig::default();
    };
    NoopCooldownFileConfig {
        enabled: c.get("enabled").and_then(serde_json::Value::as_bool),
        cooldown_secs: c
            .get("cooldownSecs")
            .and_then(serde_json::Value::as_u64)
            .filter(|&s| s > 0),
    }
}

/// Resolve the full [`NoopCooldownConfig`] for `repo_root` with precedence
/// **env > config > default** for every knob (Issue #6670), mirroring
/// [`resolve_dispatch_backoff_config`].
#[must_use]
pub fn resolve_noop_cooldown_config(repo_root: &Path) -> NoopCooldownConfig {
    let file = read_noop_cooldown_file_config(repo_root);

    let enabled = if let Ok(v) = std::env::var(NOOP_COOLDOWN_ENABLE_ENV) {
        matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on")
    } else {
        file.enabled.unwrap_or(true)
    };

    let cooldown_secs = std::env::var(NOOP_COOLDOWN_SECS_ENV)
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|&s| s > 0)
        .or(file.cooldown_secs)
        .unwrap_or(DEFAULT_NOOP_COOLDOWN_SECS);

    NoopCooldownConfig {
        enabled,
        cooldown: Duration::from_secs(cooldown_secs),
    }
}

impl SweepRegistry {
    /// Set the no-op-cooldown parameters (Issue #6670). `main.rs` and the
    /// workspace pool call this once at provision time with the resolved
    /// env > config > default value, mirroring
    /// [`Self::set_dispatch_backoff_config`].
    pub fn set_noop_cooldown_config(&mut self, config: NoopCooldownConfig) {
        self.noop_cooldown_config = config;
    }

    /// Read-only accessor for the no-op-cooldown parameters (Issue #6670).
    #[must_use]
    pub fn noop_cooldown_config(&self) -> NoopCooldownConfig {
        self.noop_cooldown_config
    }

    /// Record a **no-op release** for `issue` (Issue #6670): a sweep concluded
    /// "no actionable delta this pass" — its own checkpoint was written, the
    /// `loom:building` claim was released cleanly back to `loom:issue`, and it
    /// made zero forge/issue mutation — and reports that back so the work
    /// finder does not immediately re-offer the same candidate. Arms (or
    /// refreshes) a flat cooldown window; unlike
    /// [`Self::record_dispatch_failure`] this never grows exponentially — a
    /// caller that determines "still nothing to do" on every pass is expected
    /// to keep reporting it, and each report just re-arms the same window.
    ///
    /// A no-op when the mechanism is disabled
    /// (`autonomous.workFinder.noopCooldown.enabled: false` /
    /// [`NOOP_COOLDOWN_ENABLE_ENV`]) — no state is written, mirroring
    /// [`Self::record_dispatch_failure`]'s disabled-path contract.
    pub(crate) fn record_noop_release(&mut self, issue: u32, reason: Option<String>) {
        if !self.noop_cooldown_config.enabled {
            return;
        }
        let now = Utc::now();
        let consecutive = self
            .noop_cooldown
            .get(&issue)
            .map_or(1, |prev| prev.consecutive.saturating_add(1));
        let until = now
            + chrono::Duration::from_std(self.noop_cooldown_config.cooldown)
                .unwrap_or_else(|_| chrono::Duration::zero());
        log::info!(
            "sweep_registry: issue #{issue} no-op-release cooldown armed — {consecutive} \
             consecutive no-op release(s), next dispatch allowed in {}s (#6670){}",
            self.noop_cooldown_config.cooldown.as_secs(),
            reason
                .as_deref()
                .map(|r| format!(": {r}"))
                .unwrap_or_default()
        );
        self.noop_cooldown.insert(
            issue,
            NoopCooldownState {
                recorded_at: now,
                until,
                consecutive,
                reason,
            },
        );
    }

    /// Clear `issue`'s no-op-cooldown record (Issue #6670) — called on any
    /// dispatch that actually starts a sweep, so a genuine content change
    /// before the cooldown elapses is never suppressed (a fresh dispatch is
    /// itself proof the candidate is live again). Returns `true` when a record
    /// existed. Mirrors [`Self::clear_dispatch_backoff`].
    pub(crate) fn clear_noop_cooldown(&mut self, issue: u32) -> bool {
        self.noop_cooldown.remove(&issue).is_some()
    }

    /// Remaining no-op cooldown for `issue` at `now` (Issue #6670), or `None`
    /// when it may be dispatched immediately. `Some(Duration::ZERO)` is never
    /// returned — an elapsed window reads as `None`. Mirrors
    /// [`Self::dispatch_backoff_remaining`].
    #[must_use]
    pub fn noop_cooldown_remaining(&self, issue: u32, now: DateTime<Utc>) -> Option<Duration> {
        if !self.noop_cooldown_config.enabled {
            return None;
        }
        let state = self.noop_cooldown.get(&issue)?;
        let remaining = state.until - now;
        if remaining <= chrono::Duration::zero() {
            return None;
        }
        remaining.to_std().ok().filter(|d| !d.is_zero())
    }

    /// Consecutive no-op releases recorded for `issue` (Issue #6670). `0` when
    /// no release is on record. Test/inspection helper, mirroring
    /// [`Self::dispatch_failure_count`].
    #[must_use]
    pub fn noop_release_count(&self, issue: u32) -> u32 {
        self.noop_cooldown.get(&issue).map_or(0, |s| s.consecutive)
    }

    /// Every issue whose no-op cooldown is still in effect at `now` (Issue
    /// #6670) — the set the work finder skips *before* the capacity gate,
    /// mirroring [`Self::quarantined_issues`] / [`Self::dispatch_backoff_issues`].
    #[must_use]
    pub fn noop_cooldown_issues(&self, now: DateTime<Utc>) -> HashSet<u32> {
        if !self.noop_cooldown_config.enabled {
            return HashSet::new();
        }
        self.noop_cooldown
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
        assert_eq!(reg.noop_release_count(4242), 0);
        assert!(reg.noop_cooldown_remaining(4242, Utc::now()).is_none());

        reg.record_noop_release(4242, Some("no delta since last survey".into()));

        assert_eq!(reg.noop_release_count(4242), 1);
        let remaining = reg.noop_cooldown_remaining(4242, Utc::now());
        assert!(remaining.is_some_and(|d| d.as_secs() > 0));
        assert!(reg.noop_cooldown_issues(Utc::now()).contains(&4242));
    }

    #[test]
    fn repeated_records_accumulate_consecutive_but_stay_flat() {
        let mut reg = test_registry();
        reg.record_noop_release(99, None);
        reg.record_noop_release(99, None);
        reg.record_noop_release(99, None);
        assert_eq!(reg.noop_release_count(99), 3);
        // Flat, non-exponential: the window is always exactly the configured
        // cooldown from the most recent record, never longer.
        let remaining = reg.noop_cooldown_remaining(99, Utc::now()).unwrap();
        assert!(remaining.as_secs() <= reg.noop_cooldown_config().cooldown.as_secs());
    }

    #[test]
    fn cooldown_expires_after_ttl() {
        let mut reg = test_registry();
        reg.record_noop_release(7, None);
        let past_ttl = Utc::now() + chrono::Duration::hours(2);
        assert!(reg.noop_cooldown_remaining(7, past_ttl).is_none());
        assert!(!reg.noop_cooldown_issues(past_ttl).contains(&7));
    }

    #[test]
    fn clear_removes_the_record() {
        let mut reg = test_registry();
        reg.record_noop_release(55, None);
        assert!(reg.noop_cooldown_remaining(55, Utc::now()).is_some());
        assert!(reg.clear_noop_cooldown(55));
        assert!(reg.noop_cooldown_remaining(55, Utc::now()).is_none());
        assert!(!reg.clear_noop_cooldown(55), "second clear is a no-op");
    }

    #[test]
    fn disabled_mechanism_records_nothing() {
        let mut reg = test_registry();
        reg.set_noop_cooldown_config(NoopCooldownConfig {
            enabled: false,
            cooldown: Duration::from_secs(60),
        });
        reg.record_noop_release(1, None);
        assert_eq!(reg.noop_release_count(1), 0);
        assert!(reg.noop_cooldown_remaining(1, Utc::now()).is_none());
        assert!(reg.noop_cooldown_issues(Utc::now()).is_empty());
    }

    #[test]
    fn independent_of_quarantine_and_dispatch_backoff() {
        // #6670 AC: the new cooldown must not interfere with either existing
        // mechanism — an issue can be in all three states simultaneously and
        // each is tracked (and clearable) independently.
        let mut reg = test_registry();
        reg.seed_quarantine_for_test(321);
        reg.record_dispatch_failure(321);
        reg.record_noop_release(321, None);

        assert!(reg.is_quarantined(321));
        assert!(reg.dispatch_backoff_remaining(321, Utc::now()).is_some());
        assert!(reg.noop_cooldown_remaining(321, Utc::now()).is_some());

        // Clearing the no-op cooldown alone leaves the other two untouched.
        assert!(reg.clear_noop_cooldown(321));
        assert!(reg.is_quarantined(321));
        assert!(reg.dispatch_backoff_remaining(321, Utc::now()).is_some());
        assert!(reg.noop_cooldown_remaining(321, Utc::now()).is_none());
    }

    // --- Config resolution precedence (env > config > default) -------------

    #[test]
    #[serial]
    fn resolve_uses_shipped_defaults_with_no_env_or_file() {
        let dir = tempdir().unwrap();
        // Ensure a clean env for this process-serial test.
        std::env::remove_var(NOOP_COOLDOWN_ENABLE_ENV);
        std::env::remove_var(NOOP_COOLDOWN_SECS_ENV);
        let cfg = resolve_noop_cooldown_config(dir.path());
        assert!(cfg.enabled);
        assert_eq!(cfg.cooldown.as_secs(), DEFAULT_NOOP_COOLDOWN_SECS);
    }

    #[test]
    #[serial]
    fn resolve_file_config_overrides_default() {
        let dir = tempdir().unwrap();
        std::env::remove_var(NOOP_COOLDOWN_ENABLE_ENV);
        std::env::remove_var(NOOP_COOLDOWN_SECS_ENV);
        std::fs::create_dir_all(dir.path().join(".loom")).unwrap();
        std::fs::write(
            dir.path().join(".loom/config.json"),
            r#"{"autonomous":{"workFinder":{"noopCooldown":{"enabled":false,"cooldownSecs":120}}}}"#,
        )
        .unwrap();
        let cfg = resolve_noop_cooldown_config(dir.path());
        assert!(!cfg.enabled);
        assert_eq!(cfg.cooldown.as_secs(), 120);
    }

    #[test]
    #[serial]
    fn resolve_env_overrides_file() {
        let dir = tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".loom")).unwrap();
        std::fs::write(
            dir.path().join(".loom/config.json"),
            r#"{"autonomous":{"workFinder":{"noopCooldown":{"enabled":false,"cooldownSecs":120}}}}"#,
        )
        .unwrap();
        std::env::set_var(NOOP_COOLDOWN_ENABLE_ENV, "1");
        std::env::set_var(NOOP_COOLDOWN_SECS_ENV, "42");
        let cfg = resolve_noop_cooldown_config(dir.path());
        std::env::remove_var(NOOP_COOLDOWN_ENABLE_ENV);
        std::env::remove_var(NOOP_COOLDOWN_SECS_ENV);
        assert!(cfg.enabled);
        assert_eq!(cfg.cooldown.as_secs(), 42);
    }

    #[test]
    #[serial]
    fn zero_or_invalid_env_values_fall_through() {
        let dir = tempdir().unwrap();
        std::env::set_var(NOOP_COOLDOWN_SECS_ENV, "0");
        let cfg = resolve_noop_cooldown_config(dir.path());
        std::env::remove_var(NOOP_COOLDOWN_SECS_ENV);
        assert_eq!(cfg.cooldown.as_secs(), DEFAULT_NOOP_COOLDOWN_SECS);
    }
}
