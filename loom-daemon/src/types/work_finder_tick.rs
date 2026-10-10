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
    /// Whether the build back-off (Issue #9410) was engaged for any repo this
    /// tick (#10624: the hold is per repo), even with nothing deferred. The
    /// `BUILD-BACKOFF-HELD` tag and the `build_backoff_held` tick result key on
    /// [`deferred_build_backoff`](Self::deferred_build_backoff) instead.
    /// `#[serde(default)]` keeps pre-#9410 wire data compatible.
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
    /// Repos whose ready-issue listing returned only part of its queue on
    /// this tick (#11139: a later page failed, the page cap, a mid-walk
    /// change). Their rows are in [`Self::queue`], but not all of them: a
    /// consumer must not read a missing row as "left the queue".
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub listing_incomplete: Vec<String>,
    /// The dispatch plan this tick's rows were annotated with (Issue #9288):
    /// slots, tick interval, shard posture, scope and key ordering. `None`
    /// for a single-workspace tick (which records no rows) and for a
    /// pre-#9288 payload.
    #[serde(default)]
    pub plan: Option<DispatchPlanContext>,
    /// The terms of this tick's concurrency cap (Issue #10214): the
    /// configured `maxConcurrent` and the disk / RAM headroom that may hold
    /// it lower, so a consumer can say *what* limits the cap. `None` for a
    /// pre-#10214 payload and before the first tick.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cap: Option<CapView>,
    /// How the starred-at cache answered since this process started: the
    /// in-process cache, the restart store, a loom-ui intent, or a timeline
    /// read (known / none / failed). `None` for a payload that predates it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub starred_at_cache: Option<StarredAtCacheTally>,
}

/// Outcome tally of the work finder's starred-at lookups, cumulative since
/// the daemon started. The `read_*` rows are the forge timeline reads that
/// remain; `read_none` vs `read_known` is the split that says whether
/// unknown starred-ats (retried every 10 minutes) dominate them.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct StarredAtCacheTally {
    /// A starred issue the in-process cache already answered.
    pub mem_hit: u64,
    /// An in-process miss answered by the restart store.
    pub disk_hit: u64,
    /// An in-process miss answered by a loom-ui intent's `requested_at`.
    pub intent_hit: u64,
    /// A timeline read that found the starred-at.
    pub read_known: u64,
    /// A timeline read that found no star event (unknown; retried later).
    pub read_none: u64,
    /// A timeline read that failed (unknown; retried later).
    pub read_err: u64,
}

impl StarredAtCacheTally {
    /// Add `other`'s counts to these.
    pub fn add(&mut self, other: &Self) {
        self.mem_hit += other.mem_hit;
        self.disk_hit += other.disk_hit;
        self.intent_hit += other.intent_hit;
        self.read_known += other.read_known;
        self.read_none += other.read_none;
        self.read_err += other.read_err;
    }

    /// How many lookups went to the forge.
    #[must_use]
    pub fn reads(&self) -> u64 {
        self.read_known + self.read_none + self.read_err
    }
}

/// What holds a tick's effective concurrency cap where it is (Issue #10214).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CapLimiter {
    /// The configured `maxConcurrent`: an operator setting, not a shortage.
    Configured,
    /// Disk headroom on the worktree volume (`free GB / per-worktree GB`).
    Disk,
    /// Available-RAM headroom.
    Ram,
    /// A limiter this client does not know (forward compatibility).
    #[serde(other)]
    Unknown,
}

impl CapLimiter {
    /// The kebab-case wire name (identical to the serde form).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Configured => "configured",
            Self::Disk => "disk",
            Self::Ram => "ram",
            Self::Unknown => "unknown",
        }
    }
}

/// The terms of one tick's concurrency cap (Issue #10214): `min(configured,
/// disk headroom, ram headroom)`. A headroom term is `None` when it was
/// unmeasurable (the work finder then skips that clamp).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CapView {
    /// The configured `maxConcurrent`.
    #[serde(default)]
    pub configured: usize,
    /// Disk headroom: how many worktrees the scratch volume can hold.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disk: Option<usize>,
    /// RAM headroom: how many sweeps available memory can hold.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ram: Option<usize>,
}

impl CapView {
    /// From the work finder's raw terms, where `usize::MAX` means "not
    /// measured, not clamping".
    #[must_use]
    pub fn from_terms(configured: usize, disk: usize, ram: usize) -> Self {
        let known = |n: usize| (n != usize::MAX).then_some(n);
        Self {
            configured,
            disk: known(disk),
            ram: known(ram),
        }
    }

    /// The effective cap.
    #[must_use]
    pub fn effective(&self) -> usize {
        self.configured
            .min(self.disk.unwrap_or(usize::MAX))
            .min(self.ram.unwrap_or(usize::MAX))
    }

    /// The term that binds. A headroom term binds only when it is strictly
    /// below the configured cap; disk wins a disk / RAM tie.
    #[must_use]
    pub fn limiter(&self) -> CapLimiter {
        let disk = self.disk.unwrap_or(usize::MAX);
        let ram = self.ram.unwrap_or(usize::MAX);
        if disk < self.configured && disk <= ram {
            CapLimiter::Disk
        } else if ram < self.configured {
            CapLimiter::Ram
        } else {
            CapLimiter::Configured
        }
    }

    /// Whether a resource shortage (disk or RAM), not the operator's
    /// setting, holds the cap below `maxConcurrent`.
    #[must_use]
    pub fn resource_limited(&self) -> bool {
        matches!(self.limiter(), CapLimiter::Disk | CapLimiter::Ram)
    }
}

/// Why a starred issue the work finder deferred is still waiting, and where
/// it stands (Issue #10214). Carried on a `no-capacity` landing row so the
/// status, the queue view and any escalation can say "queued #88 of 106
/// (cap 2, disk-limited)" instead of an undifferentiated "no capacity".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CapacityWait {
    /// The work finder's gate: `capacity`, `ramp`, `saturation`,
    /// `build-backoff`, `repo-cap`, `repo-slice` or `host-affinity`.
    pub gate: String,
    /// For the `capacity` gate: which cap term binds, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limiter: Option<CapLimiter>,
    /// 1-based position among the host's waiting starred issues.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub position: Option<u32>,
    /// How many starred issues are waiting on the host.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total: Option<u32>,
    /// The effective concurrency cap, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cap: Option<usize>,
    /// The configured `maxConcurrent`, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub configured_cap: Option<usize>,
}

impl CapacityWait {
    /// Whether this is a queue wait (a capacity-style gate that frees up as
    /// work ahead completes), as opposed to a host refusal (`host-affinity`),
    /// which no amount of waiting on this host resolves.
    #[must_use]
    pub fn queued(&self) -> bool {
        self.gate != "host-affinity"
    }

    /// The short phrase naming what limits it, e.g. `disk-limited`.
    #[must_use]
    pub fn limit_phrase(&self) -> &'static str {
        match (self.gate.as_str(), self.limiter) {
            ("capacity", Some(CapLimiter::Disk)) => "disk-limited",
            ("capacity", Some(CapLimiter::Ram)) => "ram-limited",
            ("capacity", Some(CapLimiter::Configured)) => "at the configured cap",
            ("capacity", _) => "concurrency cap full",
            ("ramp", _) => "per-tick admission ramp",
            ("saturation", _) => "host saturated",
            ("build-backoff", _) => "build back-off",
            ("repo-cap", _) => "per-repo cap",
            ("repo-slice", _) => "outside this host's repo slice",
            ("host-affinity", _) => "host affinity names another host",
            _ => "deferred",
        }
    }

    /// `queued #88 of 106 (cap 2, disk-limited)`; without a position,
    /// `waiting (cap 2, disk-limited)`; for a host refusal, `not for this
    /// host (host affinity names another host)`.
    #[must_use]
    pub fn summary(&self) -> String {
        let mut inner = Vec::new();
        if let Some(cap) = self.cap {
            inner.push(format!("cap {cap}"));
        }
        inner.push(self.limit_phrase().to_string());
        let inner = inner.join(", ");
        if !self.queued() {
            return format!("not for this host ({inner})");
        }
        match (self.position, self.total) {
            (Some(p), Some(t)) => format!("queued #{p} of {t} ({inner})"),
            _ => format!("waiting ({inner})"),
        }
    }
}

impl WorkFinderTickSummary {
    /// Repos whose backlog this tick does not hold whole: those in
    /// [`Self::listing_failed`] and those in [`Self::listing_incomplete`]
    /// (#11139). A consumer that reads a missing row as "left the queue" or
    /// "not held" uses this, not `listing_failed` alone.
    pub fn listing_not_whole(&self) -> impl Iterator<Item = &String> {
        self.listing_failed.iter().chain(&self.listing_incomplete)
    }

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
        if !self.listing_incomplete.is_empty() {
            parts.push(format!("{} listing-incomplete", self.listing_incomplete.len()));
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
        // Only when the back-off actually deferred something (#10624): with a
        // per-repo hold some repo is often engaged, and the bare flag would
        // tag nearly every tick.
        if self.build_backoff_held && self.deferred_build_backoff > 0 {
            parts.push("BUILD-BACKOFF-HELD".to_string());
        }
        parts.join(", ")
    }
}
