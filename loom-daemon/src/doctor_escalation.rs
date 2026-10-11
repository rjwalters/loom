//! Mixed-sweep Doctor retry policy (#11359): after K Judge rejections of
//! completed GLM Doctor attempts, the next Doctor leaves the GLM tap and goes
//! to the existing `sweep.escalation` Claude ladder.
//!
//! This module owns only the *decision state*; it never launches anything and
//! never invents a model ladder. The state lives in the sweep checkpoint
//! (`doctor_escalation`) so it survives a kill-and-resume. The overall Doctor
//! cycle cap (`sweep.max_doctor_cycles`) stays authoritative: [`route`] checks
//! it before the latch, so K can never buy an extra cycle.
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Config key (under `.loom/config.json`).
pub const CONFIG_KEY: &str = "sweep.glmDoctorRejectionsBeforeEscalation";
/// K when the key is absent.
pub const DEFAULT_K: u32 = 2;

/// Parse K. Absent means [`DEFAULT_K`]; anything but a positive integer is an
/// error naming the key (a silently-ignored typo would strand a PR on GLM).
///
/// # Errors
/// A diagnostic naming [`CONFIG_KEY`] and the offending value.
pub fn parse_k(config: &Value) -> Result<u32, String> {
    let Some(value) = crate::config_resolver::get_path(config, CONFIG_KEY) else {
        return Ok(DEFAULT_K);
    };
    match value.as_u64() {
        Some(k) if (1..=u64::from(u32::MAX)).contains(&k) => Ok(u32::try_from(k).unwrap_or(DEFAULT_K)),
        _ => Err(format!(
            "{CONFIG_KEY} must be a positive integer (got {value}); omit it for the default of {DEFAULT_K}"
        )),
    }
}

/// `loom-daemon validate` findings for this key.
#[must_use]
pub fn check_config(config: &Value) -> Vec<String> {
    parse_k(config).err().into_iter().collect()
}

/// Whether an attempt ran on GLM, from the actual launch report's runtime and
/// model profile (never from configuration: an empty GLM pool falls back).
#[must_use]
pub fn is_glm(runtime: &str, profile: Option<&str>) -> bool {
    let has = |s: &str| s.to_ascii_lowercase().contains("glm");
    runtime != "claude" && (has(runtime) || profile.is_some_and(has))
}

/// Persisted under the checkpoint's `doctor_escalation` key. Absent fields in a
/// legacy checkpoint deserialize to zero/false.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct State {
    /// The PR this history belongs to; another PR's history never applies.
    #[serde(default)]
    pub pr: u64,
    /// Head SHA the most recent completed Doctor attempt produced.
    #[serde(default)]
    pub last_doctor_head: Option<String>,
    /// Whether that attempt actually ran on GLM.
    #[serde(default)]
    pub last_doctor_glm: bool,
    /// Qualifying rejections so far.
    #[serde(default)]
    pub rejections: u32,
    /// Head whose rejection was last counted (replay identity).
    #[serde(default)]
    pub last_rejection_head: Option<String>,
    /// Latched: every later Doctor of this PR goes to Claude.
    #[serde(default)]
    pub escalated: bool,
    #[serde(default)]
    pub reason: Option<String>,
}

/// A completed Doctor attempt produced `head` on the given runtime.
pub fn record_doctor(state: &mut State, pr: u64, head: &str, glm: bool) {
    if state.pr != pr {
        *state = State {
            pr,
            ..State::default()
        };
    }
    state.last_doctor_head = Some(head.to_owned());
    state.last_doctor_glm = glm;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Rejection {
    Counted { count: u32, escalated: bool },
    Replay,
    NotQualifying(&'static str),
}

/// A Judge rejection of `head`. Counts only when `head` is exactly what the
/// last completed GLM Doctor attempt produced and has not been counted yet.
pub fn record_rejection(state: &mut State, pr: u64, head: &str, k: u32) -> Rejection {
    if state.pr != pr || state.last_doctor_head.is_none() {
        return Rejection::NotQualifying("no-completed-doctor-attempt");
    }
    if state.last_doctor_head.as_deref() != Some(head) {
        return Rejection::NotQualifying("stale-or-unrelated-head");
    }
    if !state.last_doctor_glm {
        return Rejection::NotQualifying("attempt-not-on-glm");
    }
    if state.last_rejection_head.as_deref() == Some(head) {
        return Rejection::Replay;
    }
    state.rejections += 1;
    state.last_rejection_head = Some(head.to_owned());
    if state.rejections >= k && !state.escalated {
        state.escalated = true;
        state.reason = Some(format!(
            "{} Judge rejection(s) of completed GLM Doctor attempt(s) reached {CONFIG_KEY}={k}",
            state.rejections
        ));
    }
    Rejection::Counted {
        count: state.rejections,
        escalated: state.escalated,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Route {
    /// Use the Doctor's configured role preference.
    Preference,
    /// Bypass the GLM preference; resolve through the Claude escalation ladder.
    Claude { reason: String },
    /// `sweep.max_doctor_cycles` has no cycle left; K never extends it.
    CapExhausted,
}

/// Route the Doctor of `attempt` (first Doctor cycle = attempt 2) for `pr`
/// under the effective cycle `cap`.
#[must_use]
pub fn route(state: &State, pr: u64, attempt: u32, cap: u32) -> Route {
    if attempt.saturating_sub(1) > cap {
        return Route::CapExhausted;
    }
    match (&state.reason, state.pr == pr && state.escalated) {
        (Some(reason), true) => Route::Claude {
            reason: reason.clone(),
        },
        _ => Route::Preference,
    }
}

#[cfg(test)]
mod tests;
