//! Push alert when the fleet goes DEGRADED (#10164).
//!
//! `loom-daemon health` already knows when the token pool has zero healthy
//! accounts, when dispatch is HALTED and when roles fail persistently — but
//! only when a human runs it. On 2026-10-04 all three held for hours and
//! nothing reached the operator. This module closes that gap with a daemon
//! thread ([`task`]) that evaluates the same facts on a timer and pushes a
//! de-duplicated alert.
//!
//! - [`classify`] (pure) maps a [`crate::types::DaemonStatusReport`] to
//!   [`Condition`]s, naming the cause and the fix ([`causes`]).
//! - [`state::AlertState`] (pure, injected clock) turns condition snapshots
//!   into `Started` / `Reminder` / `Cleared` transitions: debounced, one alert
//!   per edge, at most one reminder per interval, persisted across restarts.
//! - [`capacity`] (pure, #10214) adds the host-level capacity asks: disk or
//!   RAM headroom holding the cap below `maxConcurrent`, and a starred
//!   backlog more than 3x the cap.
//! - [`task`] delivers each transition to independent sinks: the event bus
//!   (relayed to Matrix by Safehouse) and the loom-ui inbox.
//!
//! **No forge calls anywhere in this path**: the whole point is that the alert
//! still arrives while `gh` is rate-limited. `loom-daemon health` output and
//! exit code are untouched.

use std::path::Path;
use std::time::Duration;

use chrono::{DateTime, Utc};

use crate::health::summarize_role_ticks;
use crate::types::DaemonStatusReport;

pub mod capacity;
pub mod causes;
pub mod eta_emit;
pub mod outputs;
pub mod state;
pub mod task;

#[cfg(test)]
mod capacity_tests;
#[cfg(test)]
mod tests;

pub use causes::TokenCause;

/// Condition keys (stable; used in the inbox mail key and persisted state).
pub const KEY_TOKENS: &str = "tokens-zero-healthy";
pub const KEY_DISPATCH: &str = "dispatch-halted";
pub const KEY_ROLES: &str = "roles-persistent";

/// One degraded fleet condition currently observed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Condition {
    /// Stable identity (`KEY_*`).
    pub key: String,
    /// What is wrong, one or two lines.
    pub headline: String,
    /// What to do about it.
    pub fix: String,
    /// Deliver to the inbox as `critical` (evaluator severity), else `normal`.
    pub critical: bool,
}

/// Evaluate a status report. `token_cause` is the best-effort reason the pool
/// is empty of healthy accounts (see [`causes::token_cause`]); it is only
/// consulted when the pool has zero healthy accounts. `window` is the role-tick
/// look-back (the same window `health --since` uses).
#[must_use]
pub fn classify(
    status: &DaemonStatusReport,
    now: DateTime<Utc>,
    window: Duration,
    token_cause: TokenCause,
) -> Vec<Condition> {
    let mut out = Vec::new();

    let cap = &status.capacity;
    let tokens_zero = cap.healthy_accounts == 0;
    if tokens_zero {
        let (why, fix) = causes::token_text(token_cause);
        out.push(Condition {
            key: KEY_TOKENS.to_string(),
            critical: false,
            headline: format!(
                "Token pool has ZERO healthy accounts ({}/{} healthy, {} exhausted): {why}. \
                 Every dispatch dies at token selection.",
                cap.healthy_accounts, cap.total_accounts, cap.exhausted_accounts
            ),
            fix: fix.to_string(),
        });
    }

    let tick_halted = status
        .last_work_finder_tick
        .as_ref()
        .is_some_and(|t| t.halted);
    if status.main_health_gate_halted {
        out.push(Condition {
            key: KEY_DISPATCH.to_string(),
            critical: false,
            headline: "Dispatch is HALTED by the main-health gate (main is red).".to_string(),
            fix: "Fix or revert the commit that broke main; dispatch resumes on its own."
                .to_string(),
        });
    } else if tick_halted && !tokens_zero {
        out.push(Condition {
            key: KEY_DISPATCH.to_string(),
            critical: false,
            headline: "Dispatch is HALTED: the last work-finder tick halted.".to_string(),
            fix: "Run `loom-daemon health` and read the dispatch section for the halt reason."
                .to_string(),
        });
    }

    let since =
        now - chrono::Duration::from_std(window).unwrap_or_else(|_| chrono::Duration::zero());
    let summary = summarize_role_ticks(&status.role_tick_records, since);
    if !summary.persistent.is_empty() {
        let mut names = Vec::new();
        let mut fixes: Vec<&'static str> = Vec::new();
        for f in &summary.persistent {
            names.push(f.role.clone());
            let fix = causes::role_fix(f.detail.as_deref());
            if !fixes.contains(&fix) {
                fixes.push(fix);
            }
        }
        names.sort();
        names.dedup();
        out.push(Condition {
            key: KEY_ROLES.to_string(),
            critical: false,
            headline: format!(
                "{} role(s) have PERSISTENT failures: {}.",
                names.len(),
                names.join(", ")
            ),
            fix: fixes.join(" "),
        });
    }

    // #10214: a resource-limited cap and an outgrown starred backlog.
    if let Some(tick) = &status.last_work_finder_tick {
        out.extend(capacity::conditions(tick));
    }
    out
}

/// Resolved settings (`autonomous.fleetAlert.*`, env > config > default).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Settings {
    pub enabled: bool,
    pub reminder: Duration,
    pub debounce_ticks: u32,
    pub interval: Duration,
}

pub const ENABLED_ENV: &str = "LOOM_FLEET_ALERT";
pub const REMINDER_HOURS_ENV: &str = "LOOM_FLEET_ALERT_REMINDER_HOURS";
pub const DEBOUNCE_ENV: &str = "LOOM_FLEET_ALERT_DEBOUNCE_TICKS";
pub const INTERVAL_ENV: &str = "LOOM_FLEET_ALERT_INTERVAL_SECS";
pub const DEFAULT_REMINDER_HOURS: u64 = 6;
pub const DEFAULT_DEBOUNCE_TICKS: u32 = 3;
pub const DEFAULT_INTERVAL_SECS: u64 = 60;

impl Default for Settings {
    fn default() -> Self {
        Self {
            enabled: false,
            reminder: Duration::from_secs(DEFAULT_REMINDER_HOURS * 3600),
            debounce_ticks: DEFAULT_DEBOUNCE_TICKS,
            interval: Duration::from_secs(DEFAULT_INTERVAL_SECS),
        }
    }
}

fn env_u64(name: &str) -> Option<u64> {
    std::env::var(name)
        .ok()?
        .trim()
        .parse::<u64>()
        .ok()
        .filter(|n| *n > 0)
}

impl Settings {
    /// Resolve from an `autonomous.fleetAlert` block (or `None`) plus env.
    #[must_use]
    pub fn from_block(block: Option<&serde_json::Value>) -> Self {
        let d = Self::default();
        let cfg = |k: &str| {
            block
                .and_then(|b| b.get(k))
                .and_then(serde_json::Value::as_u64)
        };
        let enabled = match std::env::var(ENABLED_ENV).ok().as_deref().map(str::trim) {
            Some("1" | "true" | "on") => true,
            Some("0" | "false" | "off") => false,
            _ => block
                .and_then(|b| b.get("enabled"))
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(d.enabled),
        };
        let hours = env_u64(REMINDER_HOURS_ENV)
            .or_else(|| cfg("reminderHours").filter(|n| *n > 0))
            .unwrap_or(DEFAULT_REMINDER_HOURS);
        let debounce = env_u64(DEBOUNCE_ENV)
            .or_else(|| cfg("debounceTicks").filter(|n| *n > 0))
            .and_then(|n| u32::try_from(n).ok())
            .unwrap_or(DEFAULT_DEBOUNCE_TICKS);
        let interval = env_u64(INTERVAL_ENV)
            .or_else(|| cfg("intervalSecs").filter(|n| *n > 0))
            .unwrap_or(DEFAULT_INTERVAL_SECS);
        Self {
            enabled,
            reminder: Duration::from_secs(hours.saturating_mul(3600)),
            debounce_ticks: debounce,
            interval: Duration::from_secs(interval),
        }
    }

    /// Resolve for the workspace at `root` through the config resolver.
    #[must_use]
    pub fn resolve(root: &Path) -> Self {
        let effective = crate::config_resolver::resolve_effective_config(root);
        let block = crate::config_resolver::get_path(&effective, "autonomous")
            .and_then(|a| a.get("fleetAlert"))
            .cloned();
        Self::from_block(block.as_ref())
    }
}
