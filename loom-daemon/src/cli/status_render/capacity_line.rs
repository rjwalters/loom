//! The Token-capacity block's below-cap diagnosis line (Issue #9131).
//!
//! Below the dispatch cap, `status` used to print "not capacity-bound … the
//! limiter is work availability" unconditionally (modulo the #4903 brake and
//! #4386 pre-flight substitutions). On a drain-paused host that line sat a
//! few rows above `Drain: DRAINING` while `loom-daemon queue` reported every
//! ready row `blocked: repo dispatch held (…)` and the tick `HALTED` — a host
//! with a full queue described as short of work.
//!
//! [`below_cap_line`] is the single line-choice used by both ranking branches
//! of `print_status_human`. Before attributing idleness to work availability
//! it consults the dispatch holds the status snapshot already exposes, using
//! the same vocabulary as the queue's `HaltCause` (`work_finder::halt_cause`):
//!
//! - **host-wide**: `drain` (`report.draining` — never `drain_note`, which
//!   outlives the drain) and `breaker` (`host_breaker.suppressed`);
//! - **workspace-scoped**: `main_red` (`per_repo[].health_gate_halted`, else
//!   the daemon workspace's `main_health_gate_halted`), `token_pool`
//!   (`pool_exhaustion_holds`), and `preflight_advisory`.
//!
//! A scoped hold never reads as "all dispatch stopped": the line names the
//! affected scope and keeps "work availability" for the unheld workspaces —
//! unless every registered workspace is main-red, which is host-wide in
//! effect. Known gap: the queue's `gate_pending`, `write_scope`, `ci_billing`,
//! `install_incompatible` and `daemon_too_old` causes have no projection on
//! the status wire; all are workspace-scoped, so omitting them can only leave
//! the "unheld workspaces" clause slightly optimistic, never misname the host.

use loom_daemon::types::DaemonStatusReport;

/// The line to print below the dispatch cap, or `None` to print nothing (the
/// pre-flight advisory case with no other hold — #4386: the warning at the top
/// of `status` already names the cause). `resources` is the branch's
/// "not …" tail, e.g. `"disk/RAM/CPU"`.
pub(crate) fn below_cap_line(
    report: &DaemonStatusReport,
    dispatch_cap: usize,
    resources: &str,
) -> Option<String> {
    // #4903: the saturation brake already carries its own full diagnosis.
    if let Some(note) = super::saturation_hold_note(report, dispatch_cap) {
        return Some(note);
    }
    let n = report.in_flight.len();
    let mut host_wide = host_wide_holds(report);
    let mut scoped = Vec::new();

    let total_repos = report.per_repo.len();
    let mut red = report
        .per_repo
        .iter()
        .filter(|r| r.health_gate_halted)
        .count();
    if red == 0 && report.main_health_gate_halted {
        red = 1;
    }
    if red > 0 && red >= total_repos {
        host_wide.push(format!(
            "main-health gate halted (main verified red in every workspace, {red} of {red})"
        ));
    } else if red > 0 {
        scoped.push(format!("main-health gate halted ({red} of {total_repos} workspace(s))"));
    }
    if !report.pool_exhaustion_holds.is_empty() {
        scoped.push(format!(
            "token pool exhausted ({} pool(s); workspaces resolving to them)",
            report.pool_exhaustion_holds.len()
        ));
    }
    if report.preflight_advisory_active && !(host_wide.is_empty() && scoped.is_empty()) {
        scoped.push("pre-flight advisory (see warning above)".to_string());
    }

    if !host_wide.is_empty() {
        let mut holds = host_wide.join("; ");
        if !scoped.is_empty() {
            holds.push_str("; also ");
            holds.push_str(&scoped.join("; "));
        }
        return Some(format!(
            "  not capacity-bound ({n} in flight, cap {dispatch_cap}) — but dispatch is HELD: \
             {holds}. The limiter is the hold, not work availability."
        ));
    }
    if !scoped.is_empty() {
        return Some(format!(
            "  not capacity-bound ({n} in flight, cap {dispatch_cap}) — dispatch held for some \
             workspaces: {}. Unheld workspaces are limited by work availability, not {resources}.",
            scoped.join("; ")
        ));
    }
    if report.preflight_advisory_active {
        return None;
    }
    Some(format!(
        "  not capacity-bound ({n} in flight, cap {dispatch_cap} — the limiter is work \
         availability, not {resources})"
    ))
}

/// Holds that pause new dispatch for every workspace on this host.
fn host_wide_holds(report: &DaemonStatusReport) -> Vec<String> {
    let mut holds = Vec::new();
    if report.draining {
        holds.push(
            "drain (`restart --drain` armed — new dispatch paused until in-flight reaches 0)"
                .to_string(),
        );
    }
    if let Some(hb) = report.host_breaker.as_ref().filter(|hb| hb.suppressed) {
        let phase = if hb.phase == "cooldown" {
            "COOLING DOWN"
        } else {
            "OPEN"
        };
        holds.push(format!("host breaker {phase}"));
    }
    holds
}

#[cfg(test)]
#[path = "capacity_line_tests.rs"]
mod tests;
