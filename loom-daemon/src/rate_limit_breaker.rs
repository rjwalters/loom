//! GitHub API rate-limit circuit breaker (Issue #4429).
//!
//! Every fleet host authenticates to the forge as the same identity, so the
//! REST/GraphQL budgets are a *shared, fixed* resource. When the budget is
//! exhausted, every `gh`-polling loop in the daemon (work-finder,
//! claim/quarantine reconciliation, epic supervisor) fails its calls with a
//! rate-limit error — and, before this module, kept firing the full per-tick
//! call pattern anyway (the 2026-07-29 incident: 3 workspaces × 3+ calls every
//! 60s for ~25 minutes, all failing, plus role sessions spawned straight into
//! the same wall).
//!
//! This breaker is deliberately simpler than [`crate::host_breaker`] (no
//! sustain counter — a single unambiguous rate-limit failure is proof enough):
//!
//! - Any gh call site that fails feeds its error text to
//!   [`global_observe_failure`]. A rate-limit signature trips the breaker.
//! - On trip, one `gh api rate_limit` probe (that endpoint does **not** count
//!   against the quota) learns the real reset epoch; the cooldown runs until
//!   the latest exhausted resource resets (clamped, with a config fallback
//!   when the probe itself fails).
//! - While cooling, the polling loops skip their tick *entirely* — zero gh
//!   calls, zero role spawns — and release automatically once the window
//!   resets ([`SharedRateLimitBreaker::observe_tick`]'s lazy release).
//!
//! Like the host breaker, the handle is process-global ([`register_global`])
//! so the IPC status path and every loop can consult it without threading an
//! `Arc` everywhere; unset reads as "not suppressed" (zero behavior change).

use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock};

use chrono::{DateTime, Duration, Utc};

// ============================================================================
// Configuration (env > config > default)
// ============================================================================

/// Env var toggling the rate-limit breaker. `0`/`false`/`no`/`off` disables;
/// truthy forces on. Overrides config. Defaults ON — a safety backstop,
/// mirroring [`crate::host_breaker::HOST_BREAKER_ENABLE_ENV`].
pub const RATE_LIMIT_BREAKER_ENABLE_ENV: &str = "LOOM_RATE_LIMIT_BREAKER";

/// Env var overriding the fallback cooldown (seconds) used when the reset
/// probe fails. A zero/invalid value falls through to config/default.
pub const RATE_LIMIT_BREAKER_FALLBACK_COOLDOWN_ENV: &str =
    "LOOM_RATE_LIMIT_BREAKER_FALLBACK_COOLDOWN_SECS";

/// Default: the breaker is enabled (a safety backstop — a disabled breaker is
/// byte-for-byte the pre-#4429 hammer-through-exhaustion behavior).
pub const DEFAULT_RATE_LIMIT_BREAKER_ENABLED: bool = true;

/// Default fallback cooldown when the `gh api rate_limit` probe cannot supply
/// a reset epoch: 15 minutes — a quarter of the primary-limit window, so a
/// probe-blind trip never parks the daemon for a full hour.
pub const DEFAULT_FALLBACK_COOLDOWN_SECS: u64 = 900;

/// Lower clamp on any computed cooldown: below this a release/re-trip cycle
/// would churn faster than the loops that consult it.
pub const MIN_COOLDOWN_SECS: i64 = 60;

/// Upper clamp on any computed cooldown: GitHub's primary windows are hourly,
/// so a reset epoch further out than this is a clock artifact, not a plan.
pub const MAX_COOLDOWN_SECS: i64 = 3600;

/// Resolved breaker parameters (env > config > default), captured once at
/// daemon startup and held by [`SharedRateLimitBreaker`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RateLimitBreakerConfig {
    /// Whether the breaker is active. When `false` it never suppresses.
    pub enabled: bool,
    /// Cooldown length when no probed reset epoch is available.
    pub fallback_cooldown_secs: u64,
}

impl Default for RateLimitBreakerConfig {
    fn default() -> Self {
        Self {
            enabled: DEFAULT_RATE_LIMIT_BREAKER_ENABLED,
            fallback_cooldown_secs: DEFAULT_FALLBACK_COOLDOWN_SECS,
        }
    }
}

/// The raw config half read from `.loom/config.json →
/// autonomous.rateLimitBreaker` (each field `None` when absent/malformed),
/// before env/default resolution.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RateLimitBreakerConfigFile {
    pub enabled: Option<bool>,
    pub fallback_cooldown_secs: Option<u64>,
}

/// Read `.loom/config.json → autonomous.rateLimitBreaker`, soft-failing every
/// field to `None` on a missing file, malformed JSON, or a missing block.
/// Mirrors [`crate::host_breaker::read_host_breaker_config`].
#[must_use]
pub fn read_rate_limit_breaker_config(repo_root: &Path) -> RateLimitBreakerConfigFile {
    let effective = crate::config_resolver::resolve_effective_config(repo_root);
    let Some(autonomous) = crate::config_resolver::get_path(&effective, "autonomous") else {
        return RateLimitBreakerConfigFile::default();
    };
    let rl = autonomous.get("rateLimitBreaker");
    RateLimitBreakerConfigFile {
        enabled: rl
            .and_then(|r| r.get("enabled"))
            .and_then(serde_json::Value::as_bool),
        fallback_cooldown_secs: rl
            .and_then(|r| r.get("fallbackCooldownSecs"))
            .and_then(serde_json::Value::as_u64)
            .filter(|&n| n > 0),
    }
}

/// Env override for [`RATE_LIMIT_BREAKER_ENABLE_ENV`].
fn env_enabled() -> Option<bool> {
    match std::env::var(RATE_LIMIT_BREAKER_ENABLE_ENV) {
        Ok(v) => {
            Some(matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on"))
        }
        Err(_) => None,
    }
}

fn env_fallback_cooldown_secs() -> Option<u64> {
    std::env::var(RATE_LIMIT_BREAKER_FALLBACK_COOLDOWN_ENV)
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|&n| n > 0)
}

/// Resolve the full [`RateLimitBreakerConfig`] with precedence **env > config
/// > default** for every field independently.
#[must_use]
pub fn resolve_config(file: &RateLimitBreakerConfigFile) -> RateLimitBreakerConfig {
    RateLimitBreakerConfig {
        enabled: env_enabled()
            .or(file.enabled)
            .unwrap_or(DEFAULT_RATE_LIMIT_BREAKER_ENABLED),
        fallback_cooldown_secs: env_fallback_cooldown_secs()
            .or(file.fallback_cooldown_secs)
            .unwrap_or(DEFAULT_FALLBACK_COOLDOWN_SECS),
    }
}

/// Convenience: read `repo_root`'s config and resolve it end-to-end.
#[must_use]
pub fn resolve_config_for(repo_root: &Path) -> RateLimitBreakerConfig {
    resolve_config(&read_rate_limit_breaker_config(repo_root))
}

// ============================================================================
// Pure classification + cooldown computation
// ============================================================================

/// Substrings (matched case-insensitively) that identify a gh failure as a
/// rate-limit rejection rather than an auth/network/JSON problem. Kept as a
/// table so new phrasings are one-line additions, mirroring
/// `sweep_registry::exhaustion_signatures`. Sources:
///
/// - GraphQL (`gh issue list`, `gh pr list`):
///   `GraphQL: API rate limit already exceeded for user ID …`
/// - REST (`gh api …`): `HTTP 403: API rate limit exceeded for …`
/// - Secondary limits: `You have exceeded a secondary rate limit …` /
///   abuse-detection phrasing on older deployments.
const RATE_LIMIT_SIGNATURES: &[&str] = &[
    "api rate limit exceeded",
    "api rate limit already exceeded",
    "secondary rate limit",
    "abuse detection mechanism",
    "was submitted too quickly",
];

/// Whether `text` (typically a gh stderr tail or the `anyhow` error string
/// wrapping it) identifies a rate-limit rejection.
#[must_use]
pub fn indicates_rate_limit(text: &str) -> bool {
    let lowered = text.to_ascii_lowercase();
    RATE_LIMIT_SIGNATURES
        .iter()
        .any(|sig| lowered.contains(sig))
}

/// A point-in-time budget reading from `gh api rate_limit`, cached on the
/// breaker so `loom-daemon status` can show the last-known budget without a
/// live network call (the `types.rs` status-surface rule).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BudgetSnapshot {
    pub core_remaining: u64,
    pub core_reset: DateTime<Utc>,
    pub graphql_remaining: u64,
    pub graphql_reset: DateTime<Utc>,
    pub probed_at: DateTime<Utc>,
}

/// Compute when a cooldown started at `now` should release.
///
/// With a probed budget: the **latest reset among exhausted resources**
/// (`remaining == 0`), so a graphql-only exhaustion doesn't wait on core and
/// vice versa. With no exhausted resource (a secondary limit trips without
/// zeroing a primary bucket) or no budget at all: `now + fallback_secs`.
/// Always clamped to `[MIN_COOLDOWN_SECS, MAX_COOLDOWN_SECS]` from `now` so a
/// stale/absurd reset epoch can neither churn nor park the daemon.
#[must_use]
pub fn cooldown_until(
    budget: Option<&BudgetSnapshot>,
    now: DateTime<Utc>,
    fallback_secs: u64,
) -> DateTime<Utc> {
    let fallback =
        now + Duration::seconds(i64::try_from(fallback_secs).unwrap_or(MAX_COOLDOWN_SECS));
    let candidate = match budget {
        Some(b) => {
            let mut exhausted_resets = Vec::new();
            if b.core_remaining == 0 {
                exhausted_resets.push(b.core_reset);
            }
            if b.graphql_remaining == 0 {
                exhausted_resets.push(b.graphql_reset);
            }
            exhausted_resets.into_iter().max().unwrap_or(fallback)
        }
        None => fallback,
    };
    let min = now + Duration::seconds(MIN_COOLDOWN_SECS);
    let max = now + Duration::seconds(MAX_COOLDOWN_SECS);
    candidate.clamp(min, max)
}

// ============================================================================
// State + transitions
// ============================================================================

/// The breaker's phase. No `Open`-with-sustain distinction (unlike the host
/// breaker): a rate-limit failure is unambiguous, so Closed ⇄ Cooldown is the
/// whole machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BreakerPhase {
    /// Normal operation — polls and dispatch proceed.
    Closed,
    /// The shared API budget is exhausted; polling loops skip their ticks.
    Cooldown,
}

impl BreakerPhase {
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Closed => "closed",
            Self::Cooldown => "cooldown",
        }
    }
}

/// An active cooldown window.
#[derive(Debug, Clone, PartialEq, Eq)]
struct CooldownWindow {
    until: DateTime<Utc>,
    tripped_at: DateTime<Utc>,
    /// Which loop's failure tripped it (e.g. `"work_finder"`), for the log
    /// line and status surface.
    source: String,
}

#[derive(Debug, Default)]
struct BreakerRuntime {
    cooldown: Option<CooldownWindow>,
    trips_total: u64,
    last_budget: Option<BudgetSnapshot>,
}

/// What changed, for the caller to log/emit. Fires only on real phase edges —
/// never a per-tick stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Transition {
    pub kind: TransitionKind,
    pub reason: String,
    pub until: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransitionKind {
    /// Closed → Cooldown: a rate-limit failure was observed.
    Tripped,
    /// Cooldown → Closed: the window expired.
    Released,
}

impl TransitionKind {
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Tripped => "tripped",
            Self::Released => "released",
        }
    }
}

/// A point-in-time snapshot for `loom-daemon status` and event payloads. Maps
/// to [`crate::types::RateLimitBreakerStatus`] via [`Self::into_status`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RateLimitSnapshot {
    pub enabled: bool,
    pub phase: BreakerPhase,
    pub suppressed: bool,
    pub source: Option<String>,
    pub tripped_at: Option<DateTime<Utc>>,
    pub cooldown_until: Option<DateTime<Utc>>,
    pub trips_total: u64,
    pub core_remaining: Option<u64>,
    pub graphql_remaining: Option<u64>,
    pub budget_probed_at: Option<DateTime<Utc>>,
}

impl RateLimitSnapshot {
    /// Convert to the wire/status type in [`crate::types`].
    #[must_use]
    pub fn into_status(self) -> crate::types::RateLimitBreakerStatus {
        crate::types::RateLimitBreakerStatus {
            enabled: self.enabled,
            phase: self.phase.as_str().to_string(),
            suppressed: self.suppressed,
            source: self.source,
            tripped_at: self.tripped_at,
            cooldown_until: self.cooldown_until,
            trips_total: self.trips_total,
            core_remaining: self.core_remaining,
            graphql_remaining: self.graphql_remaining,
            budget_probed_at: self.budget_probed_at,
        }
    }
}

// ============================================================================
// Shared runtime handle + process-global registration
// ============================================================================

/// Thread-safe breaker handle: gh error arms feed failures in via
/// [`observe_failure`](Self::observe_failure); polling loops call
/// [`observe_tick`](Self::observe_tick) (lazy release) then
/// [`is_suppressed`](Self::is_suppressed) before doing any forge work.
#[derive(Debug)]
pub struct SharedRateLimitBreaker {
    config: RateLimitBreakerConfig,
    inner: Mutex<BreakerRuntime>,
}

// Allow expect_used: a poisoned mutex means another thread panicked while
// holding it — unrecoverable, matching the crash-on-poison policy used across
// ipc.rs / host_breaker.rs.
#[allow(clippy::expect_used)]
impl SharedRateLimitBreaker {
    #[must_use]
    pub fn new(config: RateLimitBreakerConfig) -> Self {
        Self {
            config,
            inner: Mutex::new(BreakerRuntime::default()),
        }
    }

    #[must_use]
    pub fn config(&self) -> RateLimitBreakerConfig {
        self.config
    }

    /// Fold one observed gh failure into the breaker. Returns the `Tripped`
    /// transition when `error_text` carries a rate-limit signature and the
    /// breaker was Closed; `None` when the text is unrelated, the breaker is
    /// disabled, or a cooldown is already running (re-trips while cooling are
    /// absorbed silently — the window already covers them).
    ///
    /// `budget` is the caller-supplied probe result ([`forge::probe_budget`]);
    /// passing it in keeps the network side effect outside the lock and makes
    /// the trip path fully testable.
    pub fn observe_failure(
        &self,
        error_text: &str,
        source: &str,
        budget: Option<BudgetSnapshot>,
        now: DateTime<Utc>,
    ) -> Option<Transition> {
        if !self.config.enabled || !indicates_rate_limit(error_text) {
            return None;
        }
        let mut guard = self
            .inner
            .lock()
            .expect("rate-limit breaker mutex poisoned");
        if let Some(b) = budget {
            guard.last_budget = Some(b);
        }
        if guard.cooldown.is_some() {
            return None;
        }
        let until =
            cooldown_until(guard.last_budget.as_ref(), now, self.config.fallback_cooldown_secs);
        guard.cooldown = Some(CooldownWindow {
            until,
            tripped_at: now,
            source: source.to_string(),
        });
        guard.trips_total += 1;
        Some(Transition {
            kind: TransitionKind::Tripped,
            reason: format!(
                "{source} hit the shared API rate limit — suppressing forge polling until {until} \
                 (trip #{})",
                guard.trips_total
            ),
            until: Some(until),
        })
    }

    /// Lazy release: called by polling loops each tick; returns the
    /// `Released` transition on the tick after the window expires.
    pub fn observe_tick(&self, now: DateTime<Utc>) -> Option<Transition> {
        if !self.config.enabled {
            return None;
        }
        let mut guard = self
            .inner
            .lock()
            .expect("rate-limit breaker mutex poisoned");
        match &guard.cooldown {
            Some(window) if now >= window.until => {
                let reason = format!(
                    "rate-limit cooldown (tripped by {} at {}) expired — resuming forge polling",
                    window.source, window.tripped_at
                );
                guard.cooldown = None;
                Some(Transition {
                    kind: TransitionKind::Released,
                    reason,
                    until: None,
                })
            }
            _ => None,
        }
    }

    /// Whether forge polling should be skipped right now.
    #[must_use]
    pub fn is_suppressed(&self, now: DateTime<Utc>) -> bool {
        if !self.config.enabled {
            return false;
        }
        let guard = self
            .inner
            .lock()
            .expect("rate-limit breaker mutex poisoned");
        guard.cooldown.as_ref().is_some_and(|w| now < w.until)
    }

    /// A snapshot for the status surface.
    #[must_use]
    pub fn snapshot(&self, now: DateTime<Utc>) -> RateLimitSnapshot {
        let guard = self
            .inner
            .lock()
            .expect("rate-limit breaker mutex poisoned");
        let active = guard.cooldown.as_ref().filter(|w| now < w.until);
        RateLimitSnapshot {
            enabled: self.config.enabled,
            phase: if active.is_some() {
                BreakerPhase::Cooldown
            } else {
                BreakerPhase::Closed
            },
            suppressed: self.config.enabled && active.is_some(),
            source: active.map(|w| w.source.clone()),
            tripped_at: active.map(|w| w.tripped_at),
            cooldown_until: active.map(|w| w.until),
            trips_total: guard.trips_total,
            core_remaining: guard.last_budget.map(|b| b.core_remaining),
            graphql_remaining: guard.last_budget.map(|b| b.graphql_remaining),
            budget_probed_at: guard.last_budget.map(|b| b.probed_at),
        }
    }
}

/// Process-global breaker handle, mirroring
/// [`crate::host_breaker`]'s registration shape. Registered once at daemon
/// startup (before the startup reconciliation passes — every gh-polling
/// consumer starts after it). Unset reads as "not suppressed".
static GLOBAL: OnceLock<Arc<SharedRateLimitBreaker>> = OnceLock::new();

/// Register the process-global breaker. Idempotent: first registration wins.
pub fn register_global(breaker: Arc<SharedRateLimitBreaker>) {
    let _ = GLOBAL.set(breaker);
}

/// The process-global breaker handle, if one has been registered.
#[must_use]
pub fn global() -> Option<Arc<SharedRateLimitBreaker>> {
    GLOBAL.get().cloned()
}

/// Whether the process-global breaker is currently suppressing forge polling.
/// `false` when no breaker is registered (zero behavior change).
#[must_use]
pub fn global_is_suppressed() -> bool {
    GLOBAL.get().is_some_and(|b| b.is_suppressed(Utc::now()))
}

/// A snapshot of the process-global breaker, or `None` when unregistered.
#[must_use]
pub fn global_snapshot() -> Option<RateLimitSnapshot> {
    GLOBAL.get().map(|b| b.snapshot(Utc::now()))
}

/// One-call hook for gh error arms: classify `error_text`, and on a
/// rate-limit signature probe the real reset epoch and trip the global
/// breaker. Logs the trip (`warn`) itself so sync call sites without an event
/// bus need nothing else; returns the transition so bus-holding callers can
/// additionally emit [`emit_transition_event`].
///
/// The probe runs at most once per trip (re-trips while cooling return before
/// probing), so this adds zero API load in the steady state — and
/// `gh api rate_limit` itself does not count against the quota.
pub fn global_observe_failure(error_text: &str, source: &str) -> Option<Transition> {
    let breaker = GLOBAL.get()?;
    if !breaker.config().enabled || !indicates_rate_limit(error_text) {
        return None;
    }
    let now = Utc::now();
    // Cheap pre-check so the probe runs only for a genuinely fresh trip.
    if breaker.is_suppressed(now) {
        return None;
    }
    let budget = forge::probe_budget(Path::new("gh"), now);
    let transition = breaker.observe_failure(error_text, source, budget, now)?;
    log::warn!("rate_limit_breaker: {} — {}", transition.kind.as_str(), transition.reason);
    Some(transition)
}

/// Emit the state-change `daemon.rate_limit_breaker.state` event for a
/// transition (the trip/release log line is handled by
/// [`global_observe_failure`] / the loop's release logging). Fire-and-forget,
/// matching [`crate::host_breaker::emit_transition_event`].
pub fn emit_transition_event(event_bus: &Arc<crate::event_bus::EventBus>, transition: &Transition) {
    if let Err(e) = event_bus.publish_generic(
        "daemon.rate_limit_breaker.state",
        serde_json::json!({
            "transition": transition.kind.as_str(),
            "reason": transition.reason,
            "until": transition.until,
        }),
    ) {
        log::debug!("rate_limit_breaker: state event not delivered: {e}");
    }
}

// ============================================================================
// Forge glue (probe)
// ============================================================================

/// `gh api rate_limit` glue. Not unit-tested directly (mirrors the other
/// `forge` modules); the parse half is the tested [`parse_budget`].
pub mod forge {
    use super::{parse_budget, BudgetSnapshot};
    use chrono::{DateTime, Utc};
    use std::path::Path;
    use std::process::Command;

    /// Probe the live budget. Returns `None` on any failure (spawn error,
    /// non-zero exit, unparseable JSON) — the caller falls back to the
    /// configured cooldown. `GET /rate_limit` does not count against the
    /// primary rate limit, so this probe is safe to run *during* exhaustion.
    #[must_use]
    pub fn probe_budget(gh_bin: &Path, now: DateTime<Utc>) -> Option<BudgetSnapshot> {
        let output = Command::new(gh_bin)
            .arg("api")
            .arg("rate_limit")
            .output()
            .ok()?;
        if !output.status.success() {
            log::debug!(
                "rate_limit_breaker: budget probe failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
            return None;
        }
        parse_budget(&String::from_utf8_lossy(&output.stdout), now)
    }
}

/// Parse the `gh api rate_limit` JSON body into a [`BudgetSnapshot`]. Split
/// from the `Command` glue for testability.
#[must_use]
pub fn parse_budget(body: &str, now: DateTime<Utc>) -> Option<BudgetSnapshot> {
    let json: serde_json::Value = serde_json::from_str(body).ok()?;
    let resources = json.get("resources")?;
    let read = |name: &str| -> Option<(u64, DateTime<Utc>)> {
        let r = resources.get(name)?;
        let remaining = r.get("remaining")?.as_u64()?;
        let reset_epoch = r.get("reset")?.as_i64()?;
        let reset = DateTime::from_timestamp(reset_epoch, 0)?;
        Some((remaining, reset))
    };
    let (core_remaining, core_reset) = read("core")?;
    let (graphql_remaining, graphql_reset) = read("graphql")?;
    Some(BudgetSnapshot {
        core_remaining,
        core_reset,
        graphql_remaining,
        graphql_reset,
        probed_at: now,
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn t(secs: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(1_785_352_000 + secs, 0).unwrap()
    }

    fn enabled_config() -> RateLimitBreakerConfig {
        RateLimitBreakerConfig {
            enabled: true,
            fallback_cooldown_secs: 900,
        }
    }

    // ===== classifier =====

    #[test]
    fn classifier_matches_graphql_primary_exhaustion() {
        assert!(indicates_rate_limit(
            "gh issue list --label loom:issue failed: GraphQL: API rate limit already exceeded \
             for user ID 2687775."
        ));
    }

    #[test]
    fn classifier_matches_rest_and_secondary_phrasings() {
        assert!(indicates_rate_limit("HTTP 403: API rate limit exceeded for 1.2.3.4"));
        assert!(indicates_rate_limit("You have exceeded a secondary rate limit."));
        assert!(indicates_rate_limit(
            "You have triggered an abuse detection mechanism. Please wait."
        ));
    }

    #[test]
    fn classifier_ignores_unrelated_failures() {
        assert!(!indicates_rate_limit("gh: Not Found (HTTP 404)"));
        assert!(!indicates_rate_limit("error connecting to api.github.com: timeout"));
        assert!(!indicates_rate_limit("HTTP 401: Requires authentication"));
        assert!(!indicates_rate_limit(""));
    }

    // ===== cooldown_until =====

    fn budget(core_rem: u64, core_reset: i64, gql_rem: u64, gql_reset: i64) -> BudgetSnapshot {
        BudgetSnapshot {
            core_remaining: core_rem,
            core_reset: t(core_reset),
            graphql_remaining: gql_rem,
            graphql_reset: t(gql_reset),
            probed_at: t(0),
        }
    }

    #[test]
    fn cooldown_uses_exhausted_resource_reset() {
        // graphql exhausted, core fine → graphql's reset wins.
        let b = budget(4000, 3000, 0, 700);
        assert_eq!(cooldown_until(Some(&b), t(0), 900), t(700));
    }

    #[test]
    fn cooldown_uses_latest_reset_when_both_exhausted() {
        let b = budget(0, 500, 0, 800);
        assert_eq!(cooldown_until(Some(&b), t(0), 900), t(800));
    }

    #[test]
    fn cooldown_falls_back_when_nothing_exhausted() {
        // Secondary limit: signature fired but both primaries show remaining.
        let b = budget(100, 500, 100, 800);
        assert_eq!(cooldown_until(Some(&b), t(0), 900), t(900));
    }

    #[test]
    fn cooldown_falls_back_without_budget_and_clamps() {
        assert_eq!(cooldown_until(None, t(0), 900), t(900));
        // Below the floor → clamped up.
        assert_eq!(cooldown_until(None, t(0), 5), t(MIN_COOLDOWN_SECS));
        // A reset epoch in the past → clamped up to the floor, not instant.
        let b = budget(0, -100, 4000, 0);
        assert_eq!(cooldown_until(Some(&b), t(0), 900), t(MIN_COOLDOWN_SECS));
        // An absurdly far reset → clamped to the ceiling.
        let b = budget(0, 90_000, 4000, 0);
        assert_eq!(cooldown_until(Some(&b), t(0), 900), t(MAX_COOLDOWN_SECS));
    }

    // ===== trip / release lifecycle =====

    #[test]
    fn trip_then_lazy_release() {
        let breaker = SharedRateLimitBreaker::new(enabled_config());
        assert!(!breaker.is_suppressed(t(0)));

        let transition = breaker
            .observe_failure(
                "GraphQL: API rate limit already exceeded for user ID 1",
                "work_finder",
                Some(budget(4000, 3000, 0, 700)),
                t(0),
            )
            .unwrap();
        assert_eq!(transition.kind, TransitionKind::Tripped);
        assert_eq!(transition.until, Some(t(700)));
        assert!(breaker.is_suppressed(t(1)));
        assert!(breaker.is_suppressed(t(699)));

        // Ticks inside the window: no transition, still suppressed.
        assert!(breaker.observe_tick(t(300)).is_none());

        // First tick at/after the window: released.
        let released = breaker.observe_tick(t(700)).unwrap();
        assert_eq!(released.kind, TransitionKind::Released);
        assert!(!breaker.is_suppressed(t(701)));
    }

    #[test]
    fn retrips_while_cooling_are_absorbed() {
        let breaker = SharedRateLimitBreaker::new(enabled_config());
        breaker
            .observe_failure("API rate limit exceeded", "work_finder", None, t(0))
            .unwrap();
        // A second failure during the window must not extend or re-log.
        assert!(breaker
            .observe_failure("API rate limit exceeded", "claim_reconciliation", None, t(10))
            .is_none());
        assert_eq!(breaker.snapshot(t(10)).trips_total, 1);
    }

    #[test]
    fn unrelated_errors_never_trip() {
        let breaker = SharedRateLimitBreaker::new(enabled_config());
        assert!(breaker
            .observe_failure("gh: Not Found (HTTP 404)", "work_finder", None, t(0))
            .is_none());
        assert!(!breaker.is_suppressed(t(1)));
    }

    #[test]
    fn disabled_breaker_never_suppresses() {
        let breaker = SharedRateLimitBreaker::new(RateLimitBreakerConfig {
            enabled: false,
            fallback_cooldown_secs: 900,
        });
        assert!(breaker
            .observe_failure("API rate limit exceeded", "work_finder", None, t(0))
            .is_none());
        assert!(!breaker.is_suppressed(t(1)));
        assert!(breaker.observe_tick(t(1)).is_none());
    }

    #[test]
    fn snapshot_reflects_cooldown_and_budget() {
        let breaker = SharedRateLimitBreaker::new(enabled_config());
        let snap = breaker.snapshot(t(0));
        assert_eq!(snap.phase, BreakerPhase::Closed);
        assert!(!snap.suppressed);
        assert_eq!(snap.trips_total, 0);

        breaker
            .observe_failure(
                "API rate limit exceeded",
                "epic_supervisor",
                Some(budget(4000, 3000, 0, 700)),
                t(0),
            )
            .unwrap();
        let snap = breaker.snapshot(t(1));
        assert_eq!(snap.phase, BreakerPhase::Cooldown);
        assert!(snap.suppressed);
        assert_eq!(snap.source.as_deref(), Some("epic_supervisor"));
        assert_eq!(snap.cooldown_until, Some(t(700)));
        assert_eq!(snap.graphql_remaining, Some(0));
        assert_eq!(snap.core_remaining, Some(4000));

        // After expiry the snapshot reads Closed even before a tick observes
        // the release (status must never show a stale cooldown).
        let snap = breaker.snapshot(t(701));
        assert_eq!(snap.phase, BreakerPhase::Closed);
        assert!(!snap.suppressed);
        assert_eq!(snap.trips_total, 1);
    }

    // ===== parse_budget =====

    #[test]
    fn parse_budget_reads_gh_api_rate_limit_body() {
        let body = r#"{
            "resources": {
                "core": {"limit": 5000, "used": 278, "remaining": 4722, "reset": 1785352027},
                "graphql": {"limit": 5000, "used": 5000, "remaining": 0, "reset": 1785352835}
            },
            "rate": {"limit": 5000, "remaining": 4722, "reset": 1785352027}
        }"#;
        let b = parse_budget(body, t(0)).unwrap();
        assert_eq!(b.core_remaining, 4722);
        assert_eq!(b.graphql_remaining, 0);
        assert_eq!(b.graphql_reset, DateTime::from_timestamp(1_785_352_835, 0).unwrap());
    }

    #[test]
    fn parse_budget_rejects_malformed_bodies() {
        assert!(parse_budget("", t(0)).is_none());
        assert!(parse_budget("not json", t(0)).is_none());
        assert!(parse_budget(r#"{"resources": {}}"#, t(0)).is_none());
    }
}
