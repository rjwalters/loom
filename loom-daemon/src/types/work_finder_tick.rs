//! The work finder's per-tick wire summary (Issue #4761), moved out of
//! `types.rs` (frozen by the file-size ratchet) when #9288 added its `plan`
//! block.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::{DispatchPlanContext, ReadyQueueRow};

/// One work-finder tick's dispatch/skip tally, stamped with the wall-clock
/// time it completed and the dynamic cap it ran under (Issue #4761).
///
/// A serializable projection of [`crate::work_finder::TickReport`] — the
/// counters an operator reads off the `work_finder: tick — …` log line, made
/// queryable over IPC so `loom-daemon health` can render the same one-line
/// dispatch summary without log scraping. Deliberately a *separate* type from
/// `TickReport`: that struct is the loop's internal per-tick accumulator and
/// is free to change shape, whereas this is a wire contract.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorkFinderTickSummary {
    /// When the tick completed.
    pub at: DateTime<Utc>,
    /// The dynamic concurrency cap this tick ran under.
    pub max_concurrent: usize,
    /// Ready `loom:issue` rows the source returned this tick.
    pub seen: usize,
    /// Issues for which a new sweep was dispatched this tick.
    pub dispatched: usize,
    /// Issues skipped for carrying a park/skip label.
    pub skipped_labeled: usize,
    /// Issues skipped because a live sweep already exists for them.
    pub skipped_in_flight: usize,
    /// Issues skipped for an insta-crash quarantine.
    pub skipped_quarantined: usize,
    /// Issues skipped because their workspace is missing
    /// `.claude/commands/loom/sweep.md` (Issue #4027 guard 2.4, quarantined
    /// at the work-finder level by #6440) — incremented once per gated
    /// workspace per tick, not once per candidate.
    /// `#[serde(default)]` keeps pre-#6440 wire data / older clients
    /// compatible (an absent field parses as `0`).
    #[serde(default)]
    pub skipped_workspace_commands_missing: usize,
    /// Issues skipped because they already have an open linked PR.
    pub skipped_pr_open: usize,
    /// Issues skipped because a peer host advertised a live soft claim.
    pub skipped_peer_claim: usize,
    /// Issues skipped inside a per-issue dispatch-backoff window.
    pub skipped_backoff: usize,
    /// The subset of [`Self::skipped_backoff`] whose window was armed
    /// specifically by the open-PR guard (#4123) refusing dispatch, rather
    /// than a real dispatch failure (Issue #7606). `#[serde(default)]` keeps
    /// pre-#7606 wire data / older clients compatible (an absent field
    /// parses as `0`).
    #[serde(default)]
    pub skipped_pr_open_backoff: usize,
    /// Issues skipped inside a no-op re-dispatch cooldown window (Issue
    /// #6670) — a sweep self-reported "no actionable delta this pass" via
    /// `RecordNoopRelease`. `#[serde(default)]` keeps pre-#6670 wire data /
    /// older clients compatible (an absent field parses as `0`).
    #[serde(default)]
    pub skipped_noop_cooldown: usize,
    /// Issues skipped because a **hard-exclusion rule** applies (Issue #7528) —
    /// either the candidate itself carries a hard-exclusion label (`external`
    /// today) or a previous sweep declined on one and the reaper's decline
    /// cooldown has not elapsed. `#[serde(default)]` keeps pre-#7528 wire data
    /// / older clients compatible (an absent field parses as `0`).
    #[serde(default)]
    pub skipped_declined: usize,
    /// Issues skipped for self-declaring a `<!-- loom:recheck-interval=<value>
    /// -->` marker (Issue #6685) still within its window. `#[serde(default)]`
    /// keeps pre-#6685 wire data / older clients compatible (an absent field
    /// parses as `0`).
    #[serde(default)]
    pub skipped_recheck_interval: usize,
    /// Issues skipped because their host-affinity constraint (#7456 —
    /// `loom:host:<id>` label / `<!-- loom:requires-host=<id> -->` body
    /// marker) does not name this host. `#[serde(default)]` keeps pre-#7456
    /// wire data / older clients compatible (an absent field parses as `0`).
    #[serde(default)]
    pub skipped_host_constraint: usize,
    /// Issues deferred because the concurrency cap was reached.
    pub deferred_capacity: usize,
    /// Issues deferred because the per-tick admission ramp cap was reached.
    pub deferred_ramp_cap: usize,
    /// Issues deferred because the saturation admission brake held new
    /// admissions this tick (Issue #4903) — the host was already at/over the
    /// configured load-per-core hold threshold. Distinct from
    /// [`Self::deferred_capacity`]: the cap was not reached, the *host* was.
    /// `#[serde(default)]` keeps pre-#4903 wire data / older clients compatible.
    #[serde(default)]
    pub deferred_saturation: usize,
    /// Issues deferred because the build back-off (Issue #9410) held new
    /// unstarred issue builds: review + merge debt was high.
    /// `#[serde(default)]` keeps pre-#9410 wire data compatible.
    #[serde(default)]
    pub deferred_build_backoff: usize,
    /// Dispatch attempts that returned an error.
    pub errors: usize,
    /// Whether any workspace was gated by the main-health halt this tick.
    pub halted: bool,
    /// Whether the saturation admission brake was engaged for this tick (Issue
    /// #4903). `true` even when nothing was deferred (an empty backlog on a
    /// saturated host), so a consumer can tell "held, nothing waiting" from
    /// "not held". `#[serde(default)]` keeps pre-#4903 wire data compatible.
    #[serde(default)]
    pub saturation_held: bool,
    /// Whether the build back-off (Issue #9410) was engaged for this tick,
    /// even with nothing deferred. `#[serde(default)]` keeps pre-#9410 wire
    /// data compatible.
    #[serde(default)]
    pub build_backoff_held: bool,
    /// Cumulative cross-host dispatch collisions observed by this tick's
    /// dispatcher(s) (Issue #4085, Phase 0 of #4028) — dispatches whose
    /// pre-flip label read showed a peer host already claimed the issue.
    /// Mirrors [`crate::work_finder::TickReport::collisions`], which was
    /// already logged on the per-tick `work_finder: tick — …` line but never
    /// reached this wire-carried summary, so `loom-daemon status` /
    /// `GetDaemonStatus` could not see it without log scraping (Issue #5302).
    /// `#[serde(default)]` keeps pre-#5302 wire data / older clients
    /// compatible (an absent field parses as `0`).
    #[serde(default)]
    pub collisions: u64,
    /// Every ready issue this tick saw, in dispatch order, with what happened
    /// to it (Issue #8852). Empty for a single-workspace tick and for a
    /// pre-#8852 wire payload.
    #[serde(default)]
    pub queue: Vec<ReadyQueueRow>,
    /// Repos whose ready-issue listing failed on this tick: their backlog is
    /// missing from [`Self::queue`], which is then incomplete, not empty.
    #[serde(default)]
    pub listing_failed: Vec<String>,
    /// The dispatch plan this tick's rows were annotated with (Issue #9288):
    /// slots, tick interval, shard posture, scope and key ordering. `None`
    /// for a single-workspace tick (which records no rows) and for a
    /// pre-#9288 payload.
    #[serde(default)]
    pub plan: Option<DispatchPlanContext>,
}

impl WorkFinderTickSummary {
    /// The single-line skip-reason summary `loom-daemon health` renders —
    /// only the non-zero terms, so a clean tick reads `12 seen, 2 dispatched`
    /// rather than a wall of zeros.
    #[must_use]
    pub fn reason_summary(&self) -> String {
        let mut parts = vec![
            format!("{} seen", self.seen),
            format!("{} dispatched", self.dispatched),
        ];
        for (n, label) in [
            (self.skipped_labeled, "labeled-skip"),
            (self.skipped_in_flight, "in-flight-skip"),
            (self.skipped_quarantined, "quarantine-skip"),
            (self.skipped_workspace_commands_missing, "workspace-commands-missing-skip"),
            (self.skipped_pr_open, "pr-open-skip"),
            (self.skipped_peer_claim, "peer-claim-skip"),
            (self.skipped_backoff, "backoff-skip"),
            (self.skipped_pr_open_backoff, "pr-open-backoff"),
            (self.skipped_declined, "declined-skip"),
            (self.skipped_host_constraint, "host-constraint-skip"),
            (self.deferred_capacity, "deferred-capacity"),
            (self.deferred_ramp_cap, "deferred-ramp"),
            (self.deferred_saturation, "deferred-saturation"),
            (self.deferred_build_backoff, "deferred-build-backoff"),
            (self.errors, "error"),
        ] {
            if n > 0 {
                parts.push(format!("{n} {label}"));
            }
        }
        if self.collisions > 0 {
            parts.push(format!("{} cross-host-collision(s)", self.collisions));
        }
        if self.halted {
            parts.push("HALTED".to_string());
        }
        if self.saturation_held {
            parts.push("SATURATION-HELD".to_string());
        }
        if self.build_backoff_held {
            parts.push("BUILD-BACKOFF-HELD".to_string());
        }
        parts.join(", ")
    }
}
