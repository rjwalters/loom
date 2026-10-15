//! The admission brake's starvation-message helpers (moved verbatim from the
//! parent under the file-size ratchet, #12090).

/// Render a duration in seconds as a compact `HhMMmSSs`-ish human string —
/// e.g. `65` → `1m05s`, `3725` → `1h02m`, `42` → `42s` (Issue #5715). Used
/// only in starvation log lines, where naming the elapsed hold duration is
/// the whole point (a bare tick/second count reads as noise next to a
/// multi-hour outage).
pub(super) fn format_hms(total_secs: i64) -> String {
    let total_secs = total_secs.max(0);
    let h = total_secs / 3600;
    let m = (total_secs % 3600) / 60;
    let s = total_secs % 60;
    if h > 0 {
        format!("{h}h{m:02}m")
    } else if m > 0 {
        format!("{m}m{s:02}s")
    } else {
        format!("{s}s")
    }
}

/// The role-agent attribution clause appended to the starvation messages
/// (Issue #6102). Empty when no role agent is in flight — the pre-#6102
/// wording is then already accurate.
///
/// # Why the messages needed this
///
/// The brake's guarantee is narrower than its own text implied. It samples
/// **host-wide** load, so a role agent's CPU absolutely pushes it over the hold
/// threshold — but the only thing it can withhold is a NEW *sweep* admission.
/// It has no authority over role-runner agents (they are admitted by
/// [`crate::role_runner`]'s own loops, bounded since #6102 by
/// `autonomous.roleRunner.maxConcurrent`, never by this brake or by
/// `autonomous.workFinder.maxConcurrent`), and it can shed nothing already
/// running. So "held, 0 sweeps in flight" does **not** imply an idle host, and
/// saying "nothing is running to relieve it" was wrong in exactly the case that
/// matters: a host loaded entirely by role ticks.
#[must_use]
pub(super) fn role_agent_clause(active_role_agents: usize) -> String {
    if active_role_agents == 0 {
        return String::new();
    }
    format!(
        " — NOTE: {active_role_agents} role-runner agent(s) ARE running; the brake cannot hold \
         those back (they are admitted outside it — bound them with \
         autonomous.roleRunner.maxConcurrent) and cannot shed load already in flight, so this \
         load may be role-driven rather than idle (#6102)"
    )
}
