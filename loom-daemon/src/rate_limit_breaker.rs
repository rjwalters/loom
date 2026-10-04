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
//!   when the probe itself fails). Since #8997 the trip lands *before* the
//!   probe, the probe runs under the failing call's credential context, and
//!   a reading that contradicts the failure is distrusted — see [`report`]
//!   and [`evidence`].
//! - While cooling, the polling loops skip their tick *entirely* — zero gh
//!   calls, zero role spawns — and release automatically once the window
//!   resets ([`SharedRateLimitBreaker::observe_tick`]'s lazy release).
//!
//! Like the host breaker, the handle is process-global ([`register_global`])
//! so the IPC status path and every loop can consult it without threading an
//! `Arc` everywhere; unset reads as "not suppressed" (zero behavior change).

use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock};

use chrono::{DateTime, Utc};

pub mod evidence;
pub mod report;
#[cfg(test)]
mod report_tests;

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
    /// The pools' consumed counts this GitHub window (`used`), when the
    /// response carried them (Issue #9855) — attribution for the trip: the
    /// daemon's own ledger vs the pool's total spend says who exhausted it.
    pub core_used: Option<u64>,
    pub graphql_used: Option<u64>,
    pub probed_at: DateTime<Utc>,
}

/// Compute when a cooldown started at `now` should release, from a probed
/// budget alone: the **latest reset among exhausted resources** whose reset is
/// still in the future, else `now + fallback_secs` (no budget, a secondary
/// limit, or a stale/untrustworthy reading — see [`evidence::from_probe`]).
/// Always clamped to `[MIN_COOLDOWN_SECS, MAX_COOLDOWN_SECS]` from `now`.
#[must_use]
pub fn cooldown_until(
    budget: Option<&BudgetSnapshot>,
    now: DateTime<Utc>,
    fallback_secs: u64,
) -> DateTime<Utc> {
    evidence::from_probe("", budget, now)
        .0
        .cooldown_until(now, fallback_secs)
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
    pub core_used: Option<u64>,
    pub graphql_used: Option<u64>,
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
            core_used: self.core_used,
            graphql_used: self.graphql_used,
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
        // Only THIS trip's reading counts (#8997): a previous trip's cached
        // budget carries a long-past reset and must never set a new window.
        let (ev, reading_trusted) = evidence::from_probe(error_text, budget.as_ref(), now);
        if let Some(b) = budget.filter(|_| reading_trusted) {
            self.lock().last_budget = Some(b);
        }
        self.trip(source, &ev, now)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, BreakerRuntime> {
        self.inner
            .lock()
            .expect("rate-limit breaker mutex poisoned")
    }

    fn trip_reason(source: &str, until: DateTime<Utc>, trips: u64) -> String {
        format!(
            "{source} hit the shared API rate limit — suppressing forge polling until {until} \
             (trip #{trips})"
        )
    }

    /// Closed → Cooldown with the release time `evidence` yields. `None` when
    /// disabled or already cooling (the running window covers the repeat).
    pub fn trip(
        &self,
        source: &str,
        evidence: &evidence::ResetEvidence,
        now: DateTime<Utc>,
    ) -> Option<Transition> {
        if !self.config.enabled {
            return None;
        }
        let mut guard = self.lock();
        if guard.cooldown.as_ref().is_some_and(|w| now < w.until) {
            return None;
        }
        let until = evidence.cooldown_until(now, self.config.fallback_cooldown_secs);
        guard.cooldown = Some(CooldownWindow {
            until,
            tripped_at: now,
            source: source.to_string(),
        });
        guard.trips_total += 1;
        Some(Transition {
            kind: TransitionKind::Tripped,
            reason: Self::trip_reason(source, until, guard.trips_total),
            until: Some(until),
        })
    }

    /// Re-derive the release time of the window tripped at `tripped_at` from
    /// later `evidence` (a contextual probe, #8997). A fallback keeps the
    /// provisional window; a trusted reset replaces it (clamped from
    /// `tripped_at`). `reading` is cached for status only when trusted.
    /// Returns the window's release time, or `None` when that window is no
    /// longer the current one (released or superseded).
    pub fn refine(
        &self,
        tripped_at: DateTime<Utc>,
        evidence: &evidence::ResetEvidence,
        reading: Option<BudgetSnapshot>,
    ) -> Option<DateTime<Utc>> {
        let mut guard = self.lock();
        if let Some(b) = reading {
            guard.last_budget = Some(b);
        }
        let fallback = self.config.fallback_cooldown_secs;
        let window = guard
            .cooldown
            .as_mut()
            .filter(|w| w.tripped_at == tripped_at)?;
        if evidence.is_trusted() {
            window.until = evidence.cooldown_until(tripped_at, fallback);
        }
        Some(window.until)
    }

    /// `transition` with its release time (and reason) updated to `until`.
    #[must_use]
    pub fn retitle(&self, mut transition: Transition, until: Option<DateTime<Utc>>) -> Transition {
        if let Some(until) = until {
            let guard = self.lock();
            let source = guard.cooldown.as_ref().map_or("?", |w| w.source.as_str());
            transition.reason = Self::trip_reason(source, until, guard.trips_total);
            transition.until = Some(until);
        }
        transition
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
            core_used: guard.last_budget.as_ref().and_then(|b| b.core_used),
            graphql_used: guard.last_budget.as_ref().and_then(|b| b.graphql_used),
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
/// rate-limit signature trip the global breaker, then probe the real reset
/// epoch in the caller ([`report::ProbeMode::Inline`]). Logs the trip (`warn`)
/// itself so sync call sites without an event bus need nothing else; returns
/// the transition so bus-holding callers can additionally emit
/// [`emit_transition_event`]. The pre-#8997 ambient-context form of
/// [`global_observe_failure_ctx`].
pub fn global_observe_failure(error_text: &str, source: &str) -> Option<Transition> {
    global_observe_failure_ctx(error_text, source, report::FailureContext::default())
}

/// [`global_observe_failure`] with the failing call's context (#8997), so
/// the probe reads the credential that was actually refused. The probe runs
/// at most once per trip (the trip lands first; repeats while cooling return
/// before probing), so this adds zero API load in the steady state.
pub fn global_observe_failure_ctx(
    error_text: &str,
    source: &str,
    ctx: report::FailureContext,
) -> Option<Transition> {
    let handle = report::BreakerHandle::global(report::ProbeMode::Inline)?;
    handle
        .report(error_text, source, ctx)
        .map(|reported| reported.transition)
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
    use super::report::FailureContext;
    use super::{parse_budget, parse_graphql_probe, BudgetSnapshot, GRAPHQL_PROBE_QUERY};
    use crate::cmd_out::CmdOutcome;
    use crate::gh_invocation::{AccessIntent, GhInvocation, GhTarget, Operation};
    use chrono::{DateTime, Utc};
    use std::time::Duration;

    /// Deadline for the probe. It runs inside a gh error arm, so a wedged
    /// `gh` must not hang that caller (#9985: it used to have no deadline).
    const PROBE_TIMEOUT: Duration = Duration::from_secs(30);

    /// Probe the live budget through the `gh` facade (which resolves the
    /// executable, honouring `LOOM_GH_BIN`). Returns `None` on any failure
    /// (spawn error, timeout, non-zero exit, unparseable JSON) — the caller
    /// falls back to the configured cooldown. `GET /rate_limit` does not
    /// count against the primary rate limit, so this probe is safe to run
    /// *during* exhaustion.
    #[must_use]
    pub fn probe_budget(now: DateTime<Utc>) -> Option<BudgetSnapshot> {
        probe_budget_ctx(&FailureContext::default(), now)
    }

    /// [`probe_budget`] under the failing call's context (#8997): same
    /// working directory (→ per-owner `GH_CONFIG_DIR`), same `gh` program,
    /// same explicit credential dir — so an exhausted App installation is
    /// measured instead of the host's ambient (often healthy user) token.
    #[must_use]
    pub fn probe_budget_ctx(ctx: &FailureContext, now: DateTime<Utc>) -> Option<BudgetSnapshot> {
        let core_body = run_probe(ctx, "api.rate_limit", &["api", "rate_limit"], false)?;
        // `/rate_limit`'s `.resources.graphql` can disagree with the bucket
        // the GraphQL endpoint actually enforces (#10038), so the graphql
        // figure comes from the GraphQL endpoint itself (`-i` for the
        // `X-RateLimit-*` fallback). A refused query still prints headers on
        // stdout, so a non-zero exit is tolerated here.
        let gql_out = run_probe(
            ctx,
            "api.graphql_rate_limit",
            &["api", "-i", "graphql", "-f", GRAPHQL_PROBE_QUERY],
            true,
        )?;
        parse_budget(&core_body, parse_graphql_probe(&gql_out, now), now)
    }

    fn run_probe(
        ctx: &FailureContext,
        op: &'static str,
        args: &[&str],
        tolerate_failure: bool,
    ) -> Option<String> {
        let mut inv = GhInvocation::new(
            Operation::new(op),
            AccessIntent::Read,
            GhTarget::None,
            PROBE_TIMEOUT,
        )
        .args(args.iter().copied())
        .gh_config_dir(ctx.config_dir.as_deref());
        if let Some(root) = &ctx.root {
            inv = inv.current_dir(root);
        }
        if let Some(program) = &ctx.program {
            inv = inv.program(program);
        }
        let outcome = inv.run();
        let CmdOutcome::Ran(output) = outcome else {
            return None;
        };
        if !output.status.success() && !tolerate_failure {
            log::debug!(
                "rate_limit_breaker: budget probe failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
            return None;
        }
        Some(String::from_utf8_lossy(&output.stdout).into_owned())
    }
}

/// GraphQL query whose `rateLimit` node reports the enforced GraphQL bucket.
pub const GRAPHQL_PROBE_QUERY: &str = "query=query{rateLimit{limit used remaining resetAt}}";

/// The enforced GraphQL bucket: `(remaining, reset, used)`.
pub type GraphqlReading = (u64, DateTime<Utc>, Option<u64>);

/// Parse the body of the `rateLimit` GraphQL query (pure).
#[must_use]
pub fn parse_graphql_rate_limit(body: &str) -> Option<GraphqlReading> {
    let json: serde_json::Value = serde_json::from_str(body).ok()?;
    let r = json.get("data")?.get("rateLimit")?;
    let remaining = r.get("remaining")?.as_u64()?;
    let reset = DateTime::parse_from_rfc3339(r.get("resetAt")?.as_str()?)
        .ok()?
        .with_timezone(&Utc);
    Some((remaining, reset, r.get("used").and_then(serde_json::Value::as_u64)))
}

/// Parse `X-RateLimit-{Remaining,Reset,Used}` from a `gh api -i` response
/// head (pure). Used when the refused query carries no `rateLimit` body.
#[must_use]
pub fn parse_graphql_headers(head: &str) -> Option<GraphqlReading> {
    let (mut remaining, mut reset, mut used) = (None, None, None);
    for line in head.lines() {
        let Some((k, v)) = line.split_once(':') else {
            continue;
        };
        let v = v.trim();
        match k.trim().to_ascii_lowercase().as_str() {
            "x-ratelimit-remaining" => remaining = v.parse::<u64>().ok(),
            "x-ratelimit-reset" => reset = v.parse::<i64>().ok(),
            "x-ratelimit-used" => used = v.parse::<u64>().ok(),
            _ => {}
        }
    }
    Some((remaining?, DateTime::from_timestamp(reset?, 0)?, used))
}

/// Interpret `gh api -i graphql` stdout (headers + blank line + body):
/// the `rateLimit` body wins, then the headers. `_now` is reserved for
/// future clock-relative handling.
#[must_use]
pub fn parse_graphql_probe(out: &str, _now: DateTime<Utc>) -> Option<GraphqlReading> {
    let normalized = out.replace("\r\n", "\n");
    let (head, body) = normalized
        .split_once("\n\n")
        .unwrap_or((normalized.as_str(), ""));
    parse_graphql_rate_limit(body.trim()).or_else(|| parse_graphql_headers(head))
}

/// Compose a [`BudgetSnapshot`]: `core` from the `gh api rate_limit` body,
/// `graphql` from the GraphQL endpoint ([`GraphqlReading`]); the body's own
/// `.resources.graphql` is deliberately ignored (#10038). When the graphql
/// reading is unknown the whole snapshot is `None` (caller uses the fallback
/// cooldown) rather than showing a figure that may be wrong.
#[must_use]
pub fn parse_budget(
    body: &str,
    graphql: Option<GraphqlReading>,
    now: DateTime<Utc>,
) -> Option<BudgetSnapshot> {
    let json: serde_json::Value = serde_json::from_str(body).ok()?;
    let core = json.get("resources")?.get("core")?;
    let core_remaining = core.get("remaining")?.as_u64()?;
    let core_reset = DateTime::from_timestamp(core.get("reset")?.as_i64()?, 0)?;
    let (graphql_remaining, graphql_reset, graphql_used) = graphql?;
    Some(BudgetSnapshot {
        core_remaining,
        core_reset,
        graphql_remaining,
        graphql_reset,
        core_used: core.get("used").and_then(serde_json::Value::as_u64),
        graphql_used,
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
            core_used: None,
            graphql_used: None,
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
        // A reset epoch in the past is a stale reading (#8997) → the
        // configured fallback, never an instant or floor-length release.
        let b = budget(0, -100, 4000, 0);
        assert_eq!(cooldown_until(Some(&b), t(0), 900), t(900));
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

    const CORE_BODY: &str = r#"{
        "resources": {
            "core": {"limit": 5000, "used": 278, "remaining": 4722, "reset": 1785352027},
            "graphql": {"limit": 5000, "used": 64, "remaining": 4936, "reset": 1785355000}
        }
    }"#;

    const GQL_BODY: &str = r#"{"data":{"rateLimit":{"limit":5000,"used":5208,"remaining":0,"resetAt":"2026-07-29T19:20:35Z"}}}"#;

    #[test]
    fn parse_graphql_rate_limit_reads_body() {
        let (rem, reset, used) = parse_graphql_rate_limit(GQL_BODY).unwrap();
        assert_eq!((rem, used), (0, Some(5208)));
        assert_eq!(reset, DateTime::parse_from_rfc3339("2026-07-29T19:20:35Z").unwrap());
    }

    #[test]
    fn parse_graphql_headers_fallback() {
        let out = "HTTP/2.0 403 Forbidden\r\nX-Ratelimit-Remaining: 0\r\nX-Ratelimit-Reset: 1785352835\r\nX-Ratelimit-Used: 5208\r\n\r\n{\"message\":\"API rate limit already exceeded\"}";
        let (rem, reset, used) = parse_graphql_probe(out, t(0)).unwrap();
        assert_eq!((rem, used), (0, Some(5208)));
        assert_eq!(reset, DateTime::from_timestamp(1_785_352_835, 0).unwrap());
    }

    #[test]
    fn parse_graphql_probe_missing_fields_is_unknown() {
        assert!(parse_graphql_probe("HTTP/2.0 502 Bad Gateway\n\noops", t(0)).is_none());
        assert!(parse_graphql_rate_limit(r#"{"data":{"rateLimit":{"remaining":1}}}"#).is_none());
        assert!(parse_graphql_rate_limit("").is_none());
    }

    #[test]
    fn graphql_endpoint_overrides_disagreeing_rate_limit_resource() {
        // /rate_limit says graphql used 64 (healthy); the endpoint says 0 left.
        let out = format!("HTTP/2.0 200 OK\n\n{GQL_BODY}");
        let gql = parse_graphql_probe(&out, t(0));
        let b = parse_budget(CORE_BODY, gql, t(0)).unwrap();
        assert_eq!(b.graphql_remaining, 0);
        assert_eq!(b.graphql_used, Some(5208));
        assert_eq!(b.core_used, Some(278));
        let reset = DateTime::parse_from_rfc3339("2026-07-29T19:20:35Z")
            .unwrap()
            .with_timezone(&Utc);
        assert_eq!(b.graphql_reset, reset);
        // Cooldown follows the GraphQL reset, not the 900s fallback.
        let until = cooldown_until(Some(&b), t(0), 900);
        assert_eq!(until, reset);
    }

    #[test]
    fn parse_budget_unknown_graphql_yields_none() {
        assert!(parse_budget(CORE_BODY, None, t(0)).is_none());
    }

    #[test]
    fn parse_budget_rejects_malformed_bodies() {
        let g = Some((1, t(10), None));
        assert!(parse_budget("", g, t(0)).is_none());
        assert!(parse_budget("not json", g, t(0)).is_none());
        assert!(parse_budget(r#"{"resources": {}}"#, g, t(0)).is_none());
    }
}
