//! Quarantine relapse escalation (vibesql#6639, #9605): **probation** plus a
//! **doubling TTL**.
//!
//! The insta-crash quarantine (#3939) used to flap forever on a persistent
//! breakage: 3 crashes -> 1h TTL -> release with a fresh 3-strike runway -> 3
//! more crashes -> repeat, each cycle wasting three dispatches. Two changes,
//! both confined to the relapse path:
//!
//! 1. **Probation.** A quarantine's generation marker
//!    (`SweepRegistry::quarantine_generations`) survives its TTL release; only
//!    a healthy outcome (checkpoint / clean exit) or an operator
//!    `quarantine clear` removes it. While the marker exists, the FIRST further
//!    insta-crash re-quarantines (effective threshold 1).
//! 2. **Escalation.** Generation N serves `ttl * 2^(N-1)`, capped at
//!    [`QuarantineConfig::ttl_max`] (default 24h, clamped up to `ttl`).
//!
//! A transient breakage keeps the exact old behavior: quarantine -> TTL -> one
//! healthy run -> ladder fully reset. Generation state is in-memory like the
//! quarantine itself, so a daemon restart resets the ladder.
//!
//! A sibling file rather than more code in `quarantine.rs` purely because that
//! file is at its file-size ratchet (`.loom/docs/file-size-policy.md`); the
//! tally/apply/release mechanics stay there and call the helpers below.

use super::*;

/// Env var overriding the ceiling on the escalated quarantine TTL, in seconds
/// (vibesql#6639). A zero/invalid value falls through to config/default.
pub const QUARANTINE_TTL_MAX_ENV: &str = "LOOM_WORK_FINDER_QUARANTINE_TTL_MAX_SECS";

/// Default ceiling on the escalated quarantine TTL (vibesql#6639): a
/// persistently-broken issue's flap decays to at-most-daily retries instead of
/// cycling every base TTL forever.
pub const DEFAULT_QUARANTINE_TTL_MAX_SECS: u64 = 86_400;

/// The TTL, in seconds, a **generation-N** quarantine serves (vibesql#6639):
/// `min(ttl * 2^(N-1), ttl_max)` with saturating arithmetic, never below `ttl`
/// (a ceiling misconfigured under the base TTL must not shorten generation 1).
/// Generation 0 (no marker) is treated as generation 1.
#[must_use]
pub fn effective_quarantine_ttl_secs(config: &QuarantineConfig, generation: u32) -> u64 {
    let base = config.ttl.as_secs();
    let shift = generation.saturating_sub(1).min(63);
    base.saturating_mul(1u64 << shift)
        .min(config.ttl_max.as_secs())
        .max(base)
}

/// Resolve the escalation ceiling for `repo_root` with precedence
/// **env > config > default** (`autonomous.workFinder.quarantine.ttlMaxSecs`),
/// then clamp it up to `ttl_secs` — the ceiling may only extend the pause.
#[must_use]
pub fn resolve_quarantine_ttl_max(repo_root: &Path, ttl_secs: u64) -> Duration {
    let from_file = || {
        let effective = crate::config_resolver::resolve_effective_config(repo_root);
        crate::config_resolver::get_path(&effective, "autonomous.workFinder.quarantine.ttlMaxSecs")
            .and_then(serde_json::Value::as_u64)
            .filter(|&s| s > 0)
    };
    let secs = std::env::var(QUARANTINE_TTL_MAX_ENV)
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|&s| s > 0)
        .or_else(from_file)
        .unwrap_or(DEFAULT_QUARANTINE_TTL_MAX_SECS);
    Duration::from_secs(secs.max(ttl_secs))
}

impl SweepRegistry {
    /// The quarantine generation `issue` is serving (or last served, while on
    /// probation). `0` when the issue has no generation marker.
    #[must_use]
    pub fn quarantine_generation(&self, issue: u32) -> u32 {
        self.quarantine_generations
            .get(&issue)
            .copied()
            .unwrap_or(0)
    }

    /// The generation-escalated TTL `issue`'s quarantine serves.
    #[must_use]
    pub(crate) fn effective_quarantine_ttl(&self, issue: u32) -> Duration {
        Duration::from_secs(effective_quarantine_ttl_secs(
            &self.quarantine_config,
            self.quarantine_generation(issue),
        ))
    }

    /// Seconds left on a quarantine applied at `quarantined_at`, measured
    /// against its escalated TTL and clamped to `0` (the `quarantine list`
    /// surface).
    pub(crate) fn quarantine_ttl_remaining(
        &self,
        issue: u32,
        quarantined_at: DateTime<Utc>,
        now: DateTime<Utc>,
    ) -> u64 {
        let elapsed = (now - quarantined_at).to_std().unwrap_or_default();
        self.effective_quarantine_ttl(issue)
            .saturating_sub(elapsed)
            .as_secs()
    }

    /// Whether a quarantine applied at `quarantined_at` has served its
    /// escalated TTL as of `now`.
    pub(crate) fn quarantine_ttl_elapsed(
        &self,
        issue: u32,
        quarantined_at: DateTime<Utc>,
        now: DateTime<Utc>,
    ) -> bool {
        (now - quarantined_at).to_std().unwrap_or_default() >= self.effective_quarantine_ttl(issue)
    }

    /// Count one insta-crash for `issue` and return `(tally, threshold)`, where
    /// `threshold` is the effective quarantine threshold: `1` while the issue
    /// is on probation (it holds a generation marker from an earlier
    /// quarantine), otherwise [`QuarantineConfig::threshold`].
    pub(crate) fn tally_insta_crash(&mut self, issue: u32) -> (u32, u32) {
        let count = self.insta_crash_counts.entry(issue).or_insert(0);
        *count += 1;
        let threshold = if self.quarantine_generations.contains_key(&issue) {
            1
        } else {
            self.quarantine_config.threshold
        };
        (*count, threshold)
    }

    /// Place `issue` in quarantine now, advancing its generation.
    pub(crate) fn enter_quarantine(&mut self, issue: u32) {
        *self.quarantine_generations.entry(issue).or_insert(0) += 1;
        self.quarantined.insert(issue, Utc::now());
    }

    /// Reset `issue`'s consecutive insta-crash run AND its escalation ladder —
    /// the healthy-outcome and operator-clear path. (TTL expiry clears only the
    /// tally; the generation marker survives it as the probation.)
    pub(crate) fn reset_insta_crash_run(&mut self, issue: u32) {
        self.insta_crash_counts.remove(&issue);
        self.quarantine_generations.remove(&issue);
    }

    /// Sentence appended to the quarantine forge comment for generation >= 2,
    /// explaining the escalated pause; empty for a first quarantine.
    #[must_use]
    pub(crate) fn quarantine_escalation_note(&self, issue: u32) -> String {
        let generation = self.quarantine_generation(issue);
        if generation < 2 {
            return String::new();
        }
        format!(
            " This is quarantine **generation {generation}** for this issue (vibesql#6639): it \
             relapsed on its first dispatch after the previous quarantine released, so this pause \
             is escalated to {ttl}s (base TTL doubled per relapse, capped at {max}s). A single \
             healthy run or `loom-daemon quarantine clear {issue}` resets the ladder.",
            ttl = self.effective_quarantine_ttl(issue).as_secs(),
            max = self.quarantine_config.ttl_max.as_secs(),
        )
    }
}

#[cfg(test)]
#[path = "quarantine_escalation_tests.rs"]
mod tests;
