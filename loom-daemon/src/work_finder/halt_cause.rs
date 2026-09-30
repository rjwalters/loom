//! Closed hold-cause vocabulary for `workspace_halted` rows (Issue #9017).
//!
//! The multi-workspace tick folds several independent per-root and
//! daemon-global hold signals into one `halted: &[bool]`, so a
//! `workspace_halted` row could previously say only "repo dispatch held
//! (red main, gate, token pool, drain or breaker)" — never which one. This
//! seam module owns the named-cause counterpart: a small closed vocabulary
//! plus the per-root fold that computes it. It lives in its own file for the
//! same file-size-ratchet reason `tick_report.rs` and `pool_preflight.rs`
//! were split out of the frozen `work_finder.rs`.
//!
//! The bool fold stays authoritative for *routing* (a root is held or it is
//! not); the cause is the *attribution* layered on top. Deriving the bool as
//! `cause.is_some()` keeps the two in lockstep by construction.
//!
//! `token_pool` and `preflight_advisory` are deliberately distinct values:
//! `pool_preflight::preflight_held_per_root` folds the #7708 token-pool
//! exhaustion hold and the #5030 claude-wrapper pre-flight advisory into one
//! bool, and its cause-carrying counterpart
//! ([`crate::work_finder::pool_preflight::preflight_held_causes_per_root`])
//! splits them back out — an operator's remedy for a dead pool (add/wait for
//! accounts) is different from a broken `.mcp.json` (fix the workspace).

use std::path::PathBuf;

use super::ready_queue::{self, TickQueueRow};
use super::WorkItem;
use crate::main_health_gate::WorkspaceHealthStates;
use crate::types::QueueDisposition;

/// Why one workspace's dispatch is held this tick. A closed vocabulary: the
/// `detail` a `workspace_halted` row carries on public views is one of these
/// tokens verbatim, so no free-form text ever reaches the wire from this
/// path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HaltCause {
    /// The root's `main` is verified-red (`is_halted`, #3930).
    MainRed,
    /// A build-gate run against the root is in flight and the
    /// `suppress_dispatch_during_gate` knob is on (#4084).
    GatePending,
    /// The resolved token pool has no spawnable accounts (#7708), or a peer
    /// host reported the same pool unspawnable (#8001).
    TokenPool,
    /// The root's claude-wrapper pre-flight advisory is tripped (broken
    /// `.mcp.json`, repeated crash deaths — #5030).
    PreflightAdvisory,
    /// A daemon-global scheduled drain is in progress (#4090).
    Drain,
    /// The host-distress breaker is suppressing dispatch globally.
    Breaker,
}

impl HaltCause {
    /// Every halt cause, in [`Self::as_str`] wire-token order.
    pub const ALL: [Self; 6] = [
        Self::MainRed,
        Self::GatePending,
        Self::TokenPool,
        Self::PreflightAdvisory,
        Self::Drain,
        Self::Breaker,
    ];

    /// The cause whose [`Self::as_str`] is `raw`, or `None` when `raw` is not
    /// in the closed vocabulary. The validation half of the disposition-span
    /// exporter's (#9673) structured-extraction rule: a `workspace_halted`
    /// row's `detail` may reach a span attribute only through here, so no
    /// free-form text ever does.
    #[must_use]
    pub fn from_wire(raw: &str) -> Option<Self> {
        Self::ALL
            .iter()
            .copied()
            .find(|cause| cause.as_str() == raw)
    }

    /// The wire token carried in a `workspace_halted` row's `detail`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::MainRed => "main_red",
            Self::GatePending => "gate_pending",
            Self::TokenPool => "token_pool",
            Self::PreflightAdvisory => "preflight_advisory",
            Self::Drain => "drain",
            Self::Breaker => "breaker",
        }
    }
}

/// The per-root cause fold — the named-cause counterpart of
/// [`super::dispatch_held_per_root_with_preflight`] plus the daemon-global
/// `draining` / breaker terms the production loop OR's on top (#9017).
///
/// `preflight_causes` is parallel to `roots`, as computed by
/// [`crate::work_finder::pool_preflight::preflight_held_causes_per_root`];
/// a missing entry defaults to *not held*, mirroring the bool fold's
/// `preflight_held.get(i).unwrap_or(false)`.
///
/// **Precedence** — when several causes are true at once for the same root,
/// the first of these wins, so the row always names one cause:
///
/// 1. `main_red` — the most root-specific, operator-actionable cause; a red
///    `main` is why the queue usually looks at this row at all.
/// 2. `gate_pending` — transient and per-root; it clears on its own when the
///    gate run finishes.
/// 3. `token_pool` — per-root (or pool-shared); outranks the advisory below
///    because a pool hold allows no recovery probe, mirroring the existing
///    "a pool hold outranks a #5030 recovery probe" rule.
/// 4. `preflight_advisory` — per-root, and already probe-driven, so it is the
///    least urgent of the per-root holds.
/// 5. `drain` — daemon-global operator intent.
/// 6. `breaker` — daemon-global automatic suppression; the least specific
///    cause names the least specific holds.
///
/// `None` means the root is not held. `cause.is_some()` is byte-for-byte the
/// bool fold's `true`, which is how the production loop derives its `halted`
/// slice from this function — one source of truth, so the slice the tick
/// routes on and the causes its rows record can never disagree.
#[must_use]
pub fn causes_per_root(
    health_states: &WorkspaceHealthStates,
    roots: &[PathBuf],
    suppress_dispatch_during_gate: bool,
    preflight_causes: &[Option<HaltCause>],
    draining: bool,
    breaker_suppressed: bool,
) -> Vec<Option<HaltCause>> {
    roots
        .iter()
        .enumerate()
        .map(|(i, root)| {
            if health_states.is_halted(root) {
                return Some(HaltCause::MainRed);
            }
            if suppress_dispatch_during_gate && health_states.is_gate_in_flight(root) {
                return Some(HaltCause::GatePending);
            }
            if let Some(cause) = preflight_causes.get(i).copied().flatten() {
                return Some(cause);
            }
            if draining {
                return Some(HaltCause::Drain);
            }
            breaker_suppressed.then_some(HaltCause::Breaker)
        })
        .collect()
}

/// Record every `ready` item of held workspace `idx` as a `workspace_halted`
/// row naming its cause (#9017). `causes` is optional and parallel to the
/// tick's `halted` slice; a missing entry (or a legacy no-cause caller passing
/// `None`) records a cause-less row, byte-for-byte the pre-#9017 behaviour.
/// Lives here rather than inline in the tick loop for the same file-size
/// ratchet reason as the rest of this module.
pub fn record_halted(
    rows: &mut Vec<TickQueueRow>,
    ready: &[WorkItem],
    idx: usize,
    workspace_priority: u32,
    red: bool,
    causes: Option<&[Option<HaltCause>]>,
) {
    let cause = causes.and_then(|c| c.get(idx).copied()).flatten();
    for item in ready {
        let key = ready_queue::key_of(idx, workspace_priority, item, red);
        let detail = cause.map(|c| c.as_str().to_string());
        ready_queue::record_skip(rows, key, item, QueueDisposition::WorkspaceHalted, detail);
    }
}

#[cfg(test)]
#[path = "halt_cause_tests.rs"]
mod tests;
