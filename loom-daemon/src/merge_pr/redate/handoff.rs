//! Re-date budget exhaustion hands off to a Doctor rebase before
//! `loom:operator` (#10388, item 4).
//!
//! An exhausted re-date chain (#9590) means tree-identical no-op commits
//! cannot out-race the base branch: each CI run is stale again by the time it
//! finishes. A human was the only exit, but what the human did was almost
//! always mechanical — rebase and push. A **real rebase** is a new tree: it
//! starts a fresh re-date chain and gives CI the latest base, which a re-date
//! never can. So the first response to exhaustion is a Doctor handoff:
//! `loom:pr` → `loom:changes-requested` plus a marker comment telling Doctor
//! "no code change, rebase onto the current base". Only after
//! [`HandoffConfig::max`] handoffs that still ended exhausted does the PR go
//! to `loom:operator`, exactly as before.
//!
//! The two labels are never applied together: Doctor stands down on
//! `loom:operator`, so a combined state would strand the PR.
//!
//! The count is durable forge state (trusted marker comments, counted across
//! every head of the PR), so no process owns it and a restart cannot reset it.

use serde_json::Value;
use std::path::Path;

/// Default handoffs before `loom:operator`.
pub const DEFAULT_MAX: u32 = 2;
/// Upper clamp: the bound is the point.
pub const MAX_MAX: u32 = 10;
/// Env override (beats config). `0` restores pre-#10388 direct escalation.
pub const ENV: &str = "LOOM_REDATE_DOCTOR_HANDOFFS";
/// Config key.
pub const CONFIG_KEY: &str = "champion.redateDoctorHandoffs";
/// The label that routes the PR to Doctor.
pub const DOCTOR_LABEL: &str = "loom:changes-requested";
/// The approval label the handoff withdraws.
pub const APPROVED_LABEL: &str = "loom:pr";

/// How many Doctor handoffs precede an operator hold.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HandoffConfig {
    pub max: u32,
}

impl Default for HandoffConfig {
    fn default() -> Self {
        Self { max: DEFAULT_MAX }
    }
}

impl HandoffConfig {
    /// env > config > default; an unparseable value falls through. `0` is
    /// valid (no handoffs).
    #[must_use]
    pub fn resolve(env: Option<&str>, config: &Value) -> Self {
        let max = env
            .and_then(|s| s.trim().parse::<u64>().ok())
            .or_else(|| {
                crate::config_resolver::get_path(config, CONFIG_KEY).and_then(Value::as_u64)
            })
            .map_or(DEFAULT_MAX, |n| u32::try_from(n.min(u64::from(MAX_MAX))).unwrap_or(MAX_MAX));
        Self { max }
    }

    /// From the process env and the effective config at `root`.
    #[must_use]
    pub fn for_root(root: &Path) -> Self {
        let effective = crate::config_resolver::resolve_effective_config(root);
        Self::resolve(std::env::var(ENV).ok().as_deref(), &effective)
    }
}

/// The handoff marker for `head`, the `n`-th handoff on this PR.
#[must_use]
pub fn marker(head: &str, n: u32) -> String {
    format!("<!-- loom:stale-check-doctor-handoff head={head} n={n} -->")
}

/// Handoffs recorded on the PR, from TRUSTED comment bodies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HandoffState {
    /// Highest handoff number recorded on any head.
    pub done: u32,
    /// The handoff number recorded for the current head, if any.
    pub this_head: Option<u32>,
}

/// Parse every handoff marker out of `bodies` (trusted comments only).
#[must_use]
pub fn state(bodies: &str, head: &str) -> HandoffState {
    const PREFIX: &str = "<!-- loom:stale-check-doctor-handoff head=";
    let mut st = HandoffState {
        done: 0,
        this_head: None,
    };
    for rest in bodies.split(PREFIX).skip(1) {
        let Some((inner, _)) = rest.split_once(" -->") else {
            continue;
        };
        let Some((h, n)) = inner.split_once(" n=") else {
            continue;
        };
        let Ok(n) = n.parse::<u32>() else { continue };
        st.done = st.done.max(n);
        if h == head {
            st.this_head = Some(st.this_head.map_or(n, |m| m.max(n)));
        }
    }
    st
}

/// What an exhausted chain should do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HandoffDecision {
    /// Post handoff `n` and route to Doctor.
    HandOff { n: u32 },
    /// This head was already handed off (e.g. the label write failed, or the
    /// PR was re-approved with no push): re-assert the routing, post nothing.
    Reassert { n: u32 },
    /// `done` handoffs were spent and the guard still blocks: operator hold.
    Escalate { done: u32 },
}

/// Pure decision.
#[must_use]
pub fn decide(st: &HandoffState, cfg: &HandoffConfig) -> HandoffDecision {
    if let Some(n) = st.this_head {
        if n <= cfg.max {
            return HandoffDecision::Reassert { n };
        }
    }
    if st.done < cfg.max {
        HandoffDecision::HandOff { n: st.done + 1 }
    } else {
        HandoffDecision::Escalate { done: st.done }
    }
}

/// The handoff comment.
#[must_use]
pub fn comment_body(
    pr: &str,
    head: &str,
    n: u32,
    cfg: &HandoffConfig,
    spent: u32,
    budget: u32,
) -> String {
    let short = &head[..head.len().min(7)];
    let max = cfg.max;
    format!(
        "{}\n**Re-date budget exhausted: handing off to Doctor for a real rebase (#10388), {n} of {max}**\n\n\
PR #{pr} is still blocked at head `{short}` by the required-check freshness guard (#8248) after \
{spent} of {budget} automated tree-identical re-dates (#9590): the base branch moves faster than \
CI can re-date the checks. Another no-op commit cannot win that race. A real rebase can — it is a \
new tree, so it starts a fresh re-date chain and CI tests the latest base.\n\n\
**Doctor:** no code change is requested. Rebase this branch onto the current tip of its base \
branch, resolve any conflicts, push with `--force-with-lease`, and return the PR to \
`loom:review-requested`.\n\n\
Moved `{APPROVED_LABEL}` → `{DOCTOR_LABEL}`. If the PR is still exhausted after {max} handoff(s), \
it escalates to `loom:operator` for a human.",
        marker(head, n)
    )
}

#[cfg(test)]
#[path = "handoff_tests.rs"]
mod tests;
