//! `metric.points` — the generic operational-metrics record kind (Issue #8860).
//!
//! Before this kind existed, a daemon loop that wanted a number in SigNoz had
//! to add a record kind of its own, an OTLP mapping arm, and native-backend
//! handling. `metric.points` is the one shared carrier: a batch of named
//! points, each a gauge or a delta counter, with a small allowlisted label
//! set. It is OTLP-only (the native HTTPS `/ingest` backend never receives
//! it — see `observability::tracing::native_envelopes`), so adding a metric
//! name here never needs a dashboard-backend change.
//!
//! # Fixed vocabulary
//!
//! [`MetricName`] is a closed enum, exactly like
//! [`SpanName`](super::trace::SpanName): free text can never become a metric
//! name, and each name fixes its own [`MetricKind`], unit and description, so
//! two emitters cannot disagree about whether `loom.dispatch.decisions` is a
//! gauge or a counter. Adding a metric means adding a variant (and, when it
//! needs a new label key, extending [`OPS_METRIC_LABEL_KEYS`] together with
//! the gateway collector's `keep_keys` — contract-tested).
//!
//! # Bounded labels
//!
//! [`bounded_labels`] keeps only [`OPS_METRIC_LABEL_KEYS`], drops values over
//! [`MAX_LABEL_VALUE_BYTES`] or carrying control characters, and caps the
//! count at [`MAX_LABELS_PER_POINT`]. The exporter applies it again at export
//! so a restored on-disk queue cannot bypass the policy.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Label keys a `metric.points` data point may carry. Low-cardinality by
/// construction — never an issue number, sha, sweep id or path. The gateway
/// collector's datapoint `keep_keys` must include every key (contract-tested).
pub const OPS_METRIC_LABEL_KEYS: &[&str] = &[
    "reason",
    "provider",
    "account",
    "model",
    "state",
    "resource",
    "task",
    // W1: per-bucket rate-limit gauges and `loom.forge.calls`.
    "owner",
    "caller",
    "op",
    "role",
    "cred_owner",
    "target_owner",
    // #10571: the App installation a bucket was minted under (one per
    // `(account, owner)`, so it adds no series).
    "installation",
    "outcome",
    "kind",
    "repo",
    // #10455: `loom.codex_session.state` (the session container's name).
    "container",
    // #10607: `loom.forge.calls` ingested from agent `gh` fronts — the agent
    // role (closed vocabulary), `-` on the daemon's own rows.
    "agent",
    // 2am#4367 P4: `loom.forge.calls`' route (gateway | direct_read |
    // direct | none), a closed vocabulary.
    "route",
    // #11906: `loom.dispatch.busy_slots{role, pool="step"}`, the step
    // engine's per-role slots beside the sweep total.
    "pool",
];

/// Span attribute keys the ops span names (`loom.dispatch.tick`,
/// `loom.dispatch.admission`) may carry, in
/// addition to the lifecycle allowlist in `trace::span::bounded_attributes`.
/// The gateway collector's span `keep_keys` must include every key
/// (contract-tested).
pub const OPS_SPAN_ATTRIBUTE_KEYS: &[&str] = &[
    "loom.dispatch.result",
    "loom.dispatch.seen",
    "loom.dispatch.dispatched",
    "loom.dispatch.errors",
    "loom.dispatch.max_concurrent",
    // #11649: a held tick's distinct halt causes, comma-joined.
    "loom.dispatch.halt_causes",
    // #11857: how many workspaces a hold gated this tick (`0` when none).
    "loom.dispatch.held_roots",
    // #11973: why a tick counted errors — one token of a fixed set, or
    // `mixed` (`work_finder::tick_report::error_kind`).
    "loom.dispatch.error_kind",
    // #11858: the held repos with their causes, the cap's binding term and
    // terms, and the open-PR pre-filter's skips (`ops::dispatch_facts`).
    "loom.dispatch.halted_repos",
    "loom.dispatch.cap_limiter",
    "loom.dispatch.cap_configured",
    "loom.dispatch.cap_disk",
    "loom.dispatch.cap_ram",
    "loom.dispatch.pr_open_prefiltered",
    // #12074: the step gather's cost and coverage. Emitted since #12074 but
    // missing here until #12256, so export dropped them.
    "loom.dispatch.gather_ms",
    "loom.dispatch.repos_gathered",
    "loom.dispatch.repos_skipped",
    "loom.dispatch.gather_truncated",
    // #12256: where the tick's wall time went (`ops::dispatch_facts`).
    "loom.dispatch.phase.listing_ms",
    "loom.dispatch.phase.filter_ms",
    "loom.dispatch.phase.draw_ms",
    "loom.dispatch.phase.spawn_ms",
    "loom.dispatch.spawn_attempts",
    "loom.dispatch.gather_concurrency",
    // #12306: the tick's discovery snapshot (`ops::dispatch_facts`).
    "loom.discovery.backend",
    "loom.discovery.repos",
    "loom.discovery.items",
    "loom.discovery.forge_reads",
    "loom.discovery.worklist_latency_ms",
    "loom.discovery.not_modified",
    "loom.discovery.reused_previous",
    "loom.discovery.stale_repos",
    "loom.discovery.oldest_watermark_age_secs",
    // #12298: the spawn half's concurrency and its sub-phases.
    "loom.dispatch.spawn_concurrency",
    "loom.dispatch.spawn_hold",
    "loom.dispatch.phase.spawn_resolve_ms",
    "loom.dispatch.phase.spawn_claim_ms",
    "loom.dispatch.phase.spawn_launch_ms",
    "loom.dispatch.phase.spawn_account_ms",
    // ---- Telemetry mega PR A (Issues #8908, #8931) ----------------------
    // `loom.runtime.usage` (#8908): one execution's exact token breakdown.
    "loom.tokens.input",
    "loom.tokens.output",
    "loom.tokens.cache_read",
    "loom.tokens.cache_write",
    "loom.tokens.total",
    // Per-model, per-attempt usage + USD (Issues #9204, #9303): one
    // `loom.runtime.usage` span per model, scoped `execution` | `attempt`.
    "loom.tokens.cache_write_5m",
    "loom.tokens.cache_write_1h",
    "gen_ai.usage.input_tokens",
    "gen_ai.usage.output_tokens",
    "gen_ai.usage.cache_read_input_tokens",
    "gen_ai.usage.cache_creation_input_tokens",
    "loom.cost.usd_estimate",
    "gen_ai.cost.usd_estimate",
    "loom.pricing.verified_on",
    "loom.pricing.source",
    "loom.usage.scope",
    // `loom.pool.hold` (#8931): one pool dispatch hold, armed to cleared.
    "loom.pool.hold.pool",
    "loom.pool.hold.post_mortem",
    "loom.pool.hold.accounts",
    // `loom.dispatch.admission` spans (Issue #8907).
    "loom.dispatch.admission_result",
    "loom.dispatch.reason",
    // `loom.dispatch.step` spans (Issue #11906), beside the two above.
    "loom.step.item",
    "loom.step.role",
    "loom.step.draw_attempt",
    "loom.step.age_source",
    "loom.step.resumed",
    // `loom.dispatch.disposition` spans (Issue #9222).
    "loom.queue.disposition",
    "loom.queue.state",
    "loom.queue.rank",
    "loom.queue.transition",
    "loom.queue.previous_disposition",
    "loom.queue.park_label",
    // Queue-position metadata on disposition + admission spans (Issue #9669).
    "loom.queue.candidate_rank",
    "loom.queue.total_candidates",
    "loom.queue.priority_score",
    "loom.queue.halt_cause",
    // Repo lockout weights on a pr-open-skip row's disposition/admission
    // spans (Issue #9674).
    "lockout.duration_seconds",
    "lockout.frozen_candidates_count",
    "lockout.frozen_points_sum",
    // `loom.ratelimit.trip` spans (Issues #10022, #12238): the tripping job,
    // the cooldown end, the tripping credential, and per pool its `used`,
    // this host's own calls on it (daemon and agent sessions apart) and the
    // unattributed rest (other hosts or processes on the same credential).
    "loom.ratelimit.source",
    "loom.ratelimit.cooldown_until",
    "github.account",
    "github.cred_owner",
    "github.installation",
    "github.ratelimit.core.used",
    "github.ratelimit.core.own",
    "github.ratelimit.core.own_daemon",
    "github.ratelimit.core.own_agent",
    "github.ratelimit.core.unattributed",
    "github.ratelimit.graphql.used",
    "github.ratelimit.graphql.own",
    "github.ratelimit.graphql.own_daemon",
    "github.ratelimit.graphql.own_agent",
    "github.ratelimit.graphql.unattributed",
    // `forge.reader.withdrawn` spans (W4-A): which reader App left which
    // (owner, resource) bucket, until when, and where that end came from.
    "forge.reader.app",
    "forge.reader.owner",
    "forge.reader.resource",
    "forge.reader.until",
    "forge.reader.source",
    "forge.reader.secondary",
    // `forge.reader.spill` spans (W4-B): one read-pool spill-latch
    // transition — repo, resource, home and target reader, mode, release.
    "forge.spill.owner_repo",
    "forge.spill.resource",
    "forge.spill.from",
    "forge.spill.to",
    "forge.spill.mode",
    "forge.spill.until",
    // `forge.read.shed` spans (W4-C): which deferred read, of which class,
    // for which (owner, resource) bucket, until when.
    "forge.read.op",
    "forge.read.class",
    "forge.read.app",
    "forge.read.owner",
    "forge.read.resource",
    "forge.read.until",
    // `loom.egress.request` spans (#11300): one proxied provider request.
    // Counts, classified codes and ids only, never a body or credential.
    "loom.egress.seat",
    "loom.egress.tap",
    "loom.egress.profile",
    "loom.egress.launch_id",
    "loom.egress.status",
    "loom.egress.latency_ms",
    "loom.egress.ttft_ms",
    "loom.egress.error_code",
    "loom.egress.stream",
    // `guard.cargo_target_dir.denied` spans (#11582): the forbidden root by
    // name (`/tmp`, `$TMPDIR`, `~/.cache`, `~`, `.loom/target-*`) and where the
    // value came from. Never the path or the command.
    "loom.guard.root",
    "loom.guard.source",
    // `build_gate.run` spans (#11690): one host-side run of
    // `buildGate.command` — stage, tier, outcome, duration and attempt.
    "loom.build_gate.stage",
    "loom.build_gate.tier",
    "loom.build_gate.outcome",
    "loom.build_gate.duration_ms",
    "loom.build_gate.attempt",
    // `main_red.autofile` spans (#11766): one change to a red `main`'s
    // auto-filed fix issue — the SHA, the action and the verdict's source.
    "loom.main_red.sha",
    "loom.main_red.action",
    "loom.main_red.source",
];

/// Longest label value kept, in bytes.
pub const MAX_LABEL_VALUE_BYTES: usize = 128;
/// Most labels kept on one point (`loom.forge.calls` carries eleven: #10607's
/// ten plus 2am#4367's `route`).
pub const MAX_LABELS_PER_POINT: usize = 11;
/// Most points kept in one record.
pub const MAX_POINTS_PER_RECORD: usize = 256;

/// How a metric aggregates. Fixed per [`MetricName`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MetricKind {
    /// A point-in-time reading (OTLP `Gauge`).
    Gauge,
    /// A count of events since the previous point from the same emitter (OTLP
    /// monotonic `Sum`, `Delta` temporality).
    DeltaCounter,
}

/// Every metric name `metric.points` can carry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum MetricName {
    /// Work-finder candidate outcomes for one tick, labelled `reason`.
    #[serde(rename = "loom.dispatch.decisions")]
    DispatchDecisions,
    /// Ready candidates the work finder saw in one tick.
    #[serde(rename = "loom.dispatch.candidates")]
    DispatchCandidates,
    /// The dynamic concurrency cap the tick ran under.
    #[serde(rename = "loom.dispatch.max_concurrent")]
    DispatchMaxConcurrent,
    /// The terms the tick's cap is the minimum of, labelled `kind`
    /// (`configured`, `disk`, `ram`; #11858). A headroom term that was not
    /// measured is not emitted.
    #[serde(rename = "loom.dispatch.cap_term")]
    DispatchCapTerm,
    /// The subset of `loom.dispatch.decisions{reason=pr_open}` the open-PR
    /// pre-filter skipped with no `dispatch()` attempt (#11858).
    #[serde(rename = "loom.dispatch.pr_open_prefiltered")]
    DispatchPrOpenPrefiltered,
    /// Age of the oldest waiting ready-queue row, labelled `state` (#8856).
    #[serde(rename = "loom.queue.oldest_wait")]
    QueueOldestWait,
    /// Waiting ready-queue rows older than the starvation threshold,
    /// labelled `state` (#8856).
    #[serde(rename = "loom.queue.starved")]
    QueueStarved,
    /// Starved rows per queue disposition, labelled `reason` (#8856).
    #[serde(rename = "loom.queue.starved.by_reason")]
    QueueStarvedByReason,
    /// Summed queue dwell of the issues dispatched in one tick (#8856).
    #[serde(rename = "loom.queue.dispatch_wait")]
    QueueDispatchWait,
    /// Issues contributing to `loom.queue.dispatch_wait` (#8856).
    #[serde(rename = "loom.queue.dispatch_wait.samples")]
    QueueDispatchWaitSamples,
    /// Memory available for new allocations without swapping.
    #[serde(rename = "loom.host.memory.available_bytes")]
    HostMemoryAvailableBytes,
    /// Physical memory installed.
    #[serde(rename = "loom.host.memory.total_bytes")]
    HostMemoryTotalBytes,
    /// Swap in use.
    #[serde(rename = "loom.host.swap.used_bytes")]
    HostSwapUsedBytes,
    /// Swap configured.
    #[serde(rename = "loom.host.swap.total_bytes")]
    HostSwapTotalBytes,
    /// Free space on the worktree-root volume.
    #[serde(rename = "loom.host.worktree_volume.free_bytes")]
    HostWorktreeVolumeFreeBytes,
    /// Capacity of the worktree-root volume.
    #[serde(rename = "loom.host.worktree_volume.total_bytes")]
    HostWorktreeVolumeTotalBytes,
    // Ready-queue depth (Issue #8852, phase 2) — one contiguous block.
    /// Ready issues on the last work-finder tick, labelled `state` and
    /// `reason` (the queue disposition). Never labelled by issue or repo.
    #[serde(rename = "loom.queue.issues")]
    QueueIssues,
    /// Repos whose ready-issue listing failed on the last tick: a non-zero
    /// value means `loom.queue.issues` is missing their backlog.
    #[serde(rename = "loom.queue.listing_failed_repos")]
    QueueListingFailedRepos,
    /// Repos whose ready-issue listing came back partial on the last tick
    /// (#11139: a later page failed, the page cap, a mid-walk change): a
    /// non-zero value means `loom.queue.issues` holds only part of their
    /// backlog.
    #[serde(rename = "loom.queue.listing_incomplete_repos")]
    QueueListingIncompleteRepos,
    /// `already_landed` rows on the last tick by evidence, labelled `reason`
    /// (`forge_closed`, `forge_closed_not_planned`, `journal_landed`,
    /// `unknown`; #11858).
    #[serde(rename = "loom.queue.already_landed")]
    QueueAlreadyLanded,
    // ---- Quota burn and pool state (Issue #8857) ------------------------
    /// Uncached input tokens consumed since the previous sample.
    #[serde(rename = "loom.llm.tokens.input")]
    LlmTokensInput,
    /// Output tokens produced since the previous sample.
    #[serde(rename = "loom.llm.tokens.output")]
    LlmTokensOutput,
    /// Cache-read input tokens since the previous sample.
    #[serde(rename = "loom.llm.tokens.cache_read")]
    LlmTokensCacheRead,
    /// Cache-write input tokens since the previous sample.
    #[serde(rename = "loom.llm.tokens.cache_write")]
    LlmTokensCacheWrite,
    /// Model API responses (distinct message ids) since the previous sample.
    #[serde(rename = "loom.llm.requests")]
    LlmRequests,
    /// Accounts in a provider's pool, labelled `state` = `usable`/`exhausted`.
    #[serde(rename = "loom.pool.accounts")]
    PoolAccounts,
    /// 1 when a provider's pool has exhausted accounts and none usable.
    #[serde(rename = "loom.pool.exhausted")]
    PoolExhausted,
    /// Accounts that became exhausted since the previous sample.
    #[serde(rename = "loom.pool.exhaustions")]
    PoolExhaustions,
    /// Seconds since the previous sample the pool read as exhausted.
    #[serde(rename = "loom.pool.exhausted_seconds")]
    PoolExhaustedSeconds,
    /// One pool account's state (Issue #11286): `1` for the state it is in,
    /// `0` otherwise, labelled `provider`, `account` and `state` =
    /// `usable`/`exhausted`.
    #[serde(rename = "loom.pool.account_state")]
    PoolAccountState,
    /// An API-key seat's declared plan token allowance per window (Issue
    /// #11286), labelled `provider` + `account`. Only where declared.
    #[serde(rename = "loom.pool.plan_token_limit")]
    PoolPlanTokenLimit,
    /// An API-key seat's declared plan window in seconds (Issue #11286),
    /// labelled `provider` + `account`. Only where declared.
    #[serde(rename = "loom.pool.plan_window_seconds")]
    PoolPlanWindowSeconds,
    // ---- Reason-classified account marks (Issue #8931) -------------------
    /// Pool accounts marked out of selection since the previous point,
    /// labelled `provider` and `reason` (a closed set — see
    /// `observability::ops::pool_marks::MarkReason`). Never an account name.
    #[serde(rename = "loom.pool.account_marks")]
    PoolAccountMarks,
    // ---- Worker turnaround and forge stage dwell (Issue #8929) ----------
    /// Seconds from an issue-sweep slot freeing to its refill, summed.
    #[serde(rename = "loom.dispatch.slot_turnaround")]
    DispatchSlotTurnaround,
    /// Refills counted in `loom.dispatch.slot_turnaround`.
    #[serde(rename = "loom.dispatch.slot_turnaround.samples")]
    DispatchSlotTurnaroundSamples,
    /// Concurrency slots left idle at the end of a work-finder tick.
    #[serde(rename = "loom.dispatch.idle_slots")]
    DispatchIdleSlots,
    /// Slot-seconds left idle while dispatchable ready work waited.
    #[serde(rename = "loom.dispatch.idle_slot_seconds")]
    DispatchIdleSlotSeconds,
    /// Busy agent slots, labelled `role` (#11847): `sweep` for the work
    /// finder's issue sweeps, the role name for role-runner agents.
    #[serde(rename = "loom.dispatch.busy_slots")]
    DispatchBusySlots,
    /// The role-runner agent ceiling (`autonomous.roleRunner.maxConcurrent`).
    #[serde(rename = "loom.dispatch.role_max_concurrent")]
    DispatchRoleMaxConcurrent,
    /// Role-runner agent slots idle: the ceiling minus agents in flight.
    #[serde(rename = "loom.dispatch.role_idle_slots")]
    DispatchRoleIdleSlots,
    /// Seconds items spent in a forge label stage, summed, labelled `state`.
    #[serde(rename = "loom.forge.stage_dwell")]
    ForgeStageDwell,
    /// Stage transitions counted in `loom.forge.stage_dwell`, labelled `state`.
    #[serde(rename = "loom.forge.stage_dwell.samples")]
    ForgeStageDwellSamples,
    /// Open items carrying a stage label, labelled `state`.
    #[serde(rename = "loom.forge.stage_items")]
    ForgeStageItems,
    /// Seconds since the oldest item of a `stage_items` state entered it,
    /// labelled `state`, `reason`, `repo` (#12027: `pr_refused`).
    #[serde(rename = "loom.forge.stage_oldest_age_seconds")]
    ForgeStageOldestAgeSeconds,
    /// Items the step tick could draw (weight > 0, after the live claims),
    /// labelled `repo`, sampled every work-finder tick (#11909).
    #[serde(rename = "loom.step.actionable")]
    StepActionable,
    /// Ready-queue rows dropped from a `loom.dispatch.disposition` export
    /// pass, labelled `reason` = `unresolved` (no forge slug) or `truncated`
    /// (over the per-call row cap) — Issue #9222.
    #[serde(rename = "loom.queue.disposition_rows_dropped")]
    QueueDispositionRowsDropped,
    // ---- GitHub rate limit (Issue #10022) --------------------------------
    /// Requests left in a GitHub rate-limit pool, labelled `resource` and
    /// `account` (the non-secret credential identity).
    #[serde(rename = "github.ratelimit.remaining")]
    GithubRateLimitRemaining,
    /// Requests spent this window in a pool, labelled `resource`/`account`.
    #[serde(rename = "github.ratelimit.used")]
    GithubRateLimitUsed,
    /// When a pool's window resets (Unix epoch seconds), `resource`/`account`.
    #[serde(rename = "github.ratelimit.reset")]
    GithubRateLimitReset,
    /// Job passes skipped because the rate-limit breaker was suppressing,
    /// labelled `reason` = the job (a closed set — see
    /// `observability::ops::ratelimit::Job`).
    #[serde(rename = "github.ratelimit.breaker_skips")]
    GithubRateLimitBreakerSkips,
    /// Requests the `gh` facade spent since the previous point (W1),
    /// labelled `caller`, `op`, `role`, `account`, `cred_owner`,
    /// `target_owner`, `resource` and `outcome`; a paginated call counts its
    /// pages when known.
    #[serde(rename = "loom.forge.calls")]
    ForgeCalls,
    /// The `gh` facade's named event counters since the previous point
    /// (`crate::forge_call_stats::counters`), labelled `reason` = the
    /// counter (`facade.cwd_route.disagree`, `repo_facts.redirected`,
    /// `repo_facts.resolver_disagree`): signals that are not forge calls.
    #[serde(rename = "loom.forge.facade.events")]
    ForgeFacadeEvents,
    // ---- Merge-chain re-date pressure (Issue #10163) ----------------------
    /// PRs with at least one #8508 re-date commit in the trailing window,
    /// labelled `state` = `landed` / `pending` / `stuck` (pending with at
    /// least the default re-date budget spent). Never labelled by PR.
    #[serde(rename = "loom.merge.redate_prs")]
    MergeRedatePrs,
    /// Most re-dates any one PR took in the trailing window, labelled `state`
    /// = `landed` / `pending`.
    #[serde(rename = "loom.merge.redates_max")]
    MergeRedatesMax,
    /// Longest first-re-date-to-landing time among PRs that landed in the
    /// trailing window.
    #[serde(rename = "loom.merge.time_to_land_max")]
    MergeTimeToLandMax,
    // ---- Egress proxy observe mode (Issue #11300) -------------------------
    /// Provider requests the egress proxy observed since the previous point,
    /// labelled `provider`, `account` (the seat), `model`, `role`, `outcome`
    /// (`ok` / `error`) and `reason` (a classified code on an error).
    #[serde(rename = "loom.egress.requests")]
    EgressRequests,
    /// Tokens the proxy read off provider responses, labelled as
    /// `loom.egress.requests` plus `kind` = `input` / `output` / `cache_read`
    /// / `cache_write`.
    #[serde(rename = "loom.egress.tokens")]
    EgressTokens,
    /// Summed request latency (send to last byte), seconds.
    #[serde(rename = "loom.egress.latency")]
    EgressLatency,
    /// Summed time to first response byte, seconds.
    #[serde(rename = "loom.egress.ttft")]
    EgressTtft,
    // ---- Long-running task liveness (Issue #10414) -----------------------
    /// 1 while a long-running daemon loop beat within its staleness window,
    /// 0 once it went silent or marked itself dead, labelled `task`
    /// (`crate::task_liveness`). Never labelled by repo or issue.
    #[serde(rename = "loom.daemon.task_alive")]
    DaemonTaskAlive,
    /// Faults a long-running loop survived or died of since the previous
    /// point, labelled `task` and `reason` = `panic` / `overrun` / `exit`.
    #[serde(rename = "loom.daemon.task_faults")]
    DaemonTaskFaults,
    // ---- IPC request latency (Issue #10765) -------------------------------
    /// Slowest IPC request answered in the interval, seconds from the request
    /// line being read to the response written, labelled `kind` (the
    /// request's wire `type` tag, or `invalid`).
    #[serde(rename = "loom.daemon.ipc.latency_max")]
    DaemonIpcLatencyMax,
    /// Summed IPC request latency since the previous point, by `kind`.
    #[serde(rename = "loom.daemon.ipc.latency")]
    DaemonIpcLatency,
    /// IPC requests answered since the previous point, by `kind`.
    #[serde(rename = "loom.daemon.ipc.requests")]
    DaemonIpcRequests,
    /// `DaemonStatus` builds finished since the previous point, by `outcome`
    /// (`ok` / `panic` / `join_error`). Concurrent status requests share one
    /// build (Issue #10861), so `requests / status_builds` is the coalescing
    /// ratio.
    #[serde(rename = "loom.daemon.ipc.status_builds")]
    DaemonIpcStatusBuilds,
    // ---- Codex session containers (Issue #10455) ---------------------------
    /// Per session-managed Codex account, one point per `state` ∈ `running`,
    /// `stopped`, `restarting`, `missing`, `stale_mounts`: 1 for the container's current
    /// state, 0 for the rest. Labelled `account` and `container`.
    #[serde(rename = "loom.codex_session.state")]
    CodexSessionState,
    /// Per session-managed Codex account, one point per `kind` ∈ `hold`,
    /// `drift_removal`: 1 while that on-disk record stands (an operator
    /// `stop`; the reconciler's fail-closed removal for a denied mount),
    /// else 0. Labelled `account` and `container` (#10600).
    #[serde(rename = "loom.codex_session.record")]
    CodexSessionRecord,
    /// Per session-managed Codex account with a drift verdict, one point per
    /// `kind` ∈ `missing`, `extra`, `denied`: how many workspace paths drift
    /// that way (`extra` excludes `denied`). Labelled `account` and
    /// `container` (#10600).
    #[serde(rename = "loom.codex_session.mount_drift")]
    CodexSessionMountDrift,
    // ---- Fleet gauges produced by the captain (W12) ----------------------
    /// Age of the captain's last production of a fleet gauge job, labelled
    /// `task` = the job (`observability::captain_gauges::Config::jobs`): on the captain its own,
    /// on a dispatcher the published `as_of` it last read.
    #[serde(rename = "loom.captain.gauge_age_seconds")]
    CaptainGaugeAgeSeconds,
    /// 1 while a dispatcher produces a fleet gauge job locally because the
    /// captain's data is stale or absent, 0 while it stands down; by `task` (the job).
    #[serde(rename = "loom.captain.gauge_fallback")]
    CaptainGaugeFallback,
    /// Peak memory (`memory.peak`) of one finished agent scope, by `repo`
    /// (#11094).
    #[serde(rename = "loom.agent_scope.peak_memory_bytes")]
    AgentScopePeakMemoryBytes,
    /// Processes OOM-killed inside finished agent scopes (`memory.events`
    /// `oom_kill`), by `repo` and `role` (#12241). One point per finished
    /// scope that had a kill.
    #[serde(rename = "loom.agent_scope.oom_kills")]
    AgentScopeOomKills,
    // ---- The GitHub state mirror (2am#4367 P5) ----------------------------
    /// Discovery listings and change-stream pulls that asked the mirror, by
    /// `caller`, `repo` and `outcome` (`hit`, `stale`, `unknown`, `error`,
    /// `skipped`; every non-`hit` listing fell back to the forge).
    #[serde(rename = "loom.forge.mirror.reads")]
    ForgeMirrorReads,
    /// Changes the mirror's change stream delivered, by `repo` and `kind`.
    #[serde(rename = "loom.forge.mirror.changes")]
    ForgeMirrorChanges,
    /// Summed seconds from a change at the mirror to the pull that saw it,
    /// by `repo` (mean = this / `loom.forge.mirror.changes`).
    #[serde(rename = "loom.forge.mirror.wake_latency")]
    ForgeMirrorWakeLatency,
    /// Stage-entry answers by `repo` and `kind`, the answering source
    /// (`mirror`, `events`, `created`, `none`), #11897: the step engine's stage-age coverage.
    #[serde(rename = "loom.stage_entry.lookups")]
    StageEntryLookups,
    /// Public-feed `completion` envelopes the narration sink built, by
    /// `kind` (the trigger: `sweep_exit`, `step`, `reconcile`) and `outcome`
    /// (`narrated`, or `seeded` for a fresh host's silent first pass), #12152.
    #[serde(rename = "loom.feed.completions")]
    FeedCompletions,
    /// Merge-reconciliation passes, by `outcome` (`ran`, `deferred` while the
    /// rate-limit breaker cools, `failed` when the forge read failed), #12152.
    #[serde(rename = "loom.feed.reconcile_passes")]
    FeedReconcilePasses,
}

impl MetricName {
    /// The OTLP metric name.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::DispatchDecisions => "loom.dispatch.decisions",
            Self::DispatchCandidates => "loom.dispatch.candidates",
            Self::DispatchMaxConcurrent => "loom.dispatch.max_concurrent",
            Self::DispatchCapTerm => "loom.dispatch.cap_term",
            Self::DispatchPrOpenPrefiltered => "loom.dispatch.pr_open_prefiltered",
            Self::QueueOldestWait => "loom.queue.oldest_wait",
            Self::QueueStarved => "loom.queue.starved",
            Self::QueueStarvedByReason => "loom.queue.starved.by_reason",
            Self::QueueDispatchWait => "loom.queue.dispatch_wait",
            Self::QueueDispatchWaitSamples => "loom.queue.dispatch_wait.samples",
            Self::HostMemoryAvailableBytes => "loom.host.memory.available_bytes",
            Self::HostMemoryTotalBytes => "loom.host.memory.total_bytes",
            Self::HostSwapUsedBytes => "loom.host.swap.used_bytes",
            Self::HostSwapTotalBytes => "loom.host.swap.total_bytes",
            Self::HostWorktreeVolumeFreeBytes => "loom.host.worktree_volume.free_bytes",
            Self::HostWorktreeVolumeTotalBytes => "loom.host.worktree_volume.total_bytes",
            Self::QueueIssues => "loom.queue.issues",
            Self::QueueListingFailedRepos => "loom.queue.listing_failed_repos",
            Self::QueueListingIncompleteRepos => "loom.queue.listing_incomplete_repos",
            Self::QueueAlreadyLanded => "loom.queue.already_landed",
            Self::LlmTokensInput => "loom.llm.tokens.input",
            Self::LlmTokensOutput => "loom.llm.tokens.output",
            Self::LlmTokensCacheRead => "loom.llm.tokens.cache_read",
            Self::LlmTokensCacheWrite => "loom.llm.tokens.cache_write",
            Self::LlmRequests => "loom.llm.requests",
            Self::PoolAccounts => "loom.pool.accounts",
            Self::PoolExhausted => "loom.pool.exhausted",
            Self::PoolExhaustions => "loom.pool.exhaustions",
            Self::PoolExhaustedSeconds => "loom.pool.exhausted_seconds",
            Self::PoolAccountState => "loom.pool.account_state",
            Self::PoolPlanTokenLimit => "loom.pool.plan_token_limit",
            Self::PoolPlanWindowSeconds => "loom.pool.plan_window_seconds",
            Self::PoolAccountMarks => "loom.pool.account_marks",
            Self::DispatchSlotTurnaround => "loom.dispatch.slot_turnaround",
            Self::DispatchSlotTurnaroundSamples => "loom.dispatch.slot_turnaround.samples",
            Self::DispatchIdleSlots => "loom.dispatch.idle_slots",
            Self::DispatchIdleSlotSeconds => "loom.dispatch.idle_slot_seconds",
            Self::DispatchBusySlots => "loom.dispatch.busy_slots",
            Self::DispatchRoleMaxConcurrent => "loom.dispatch.role_max_concurrent",
            Self::DispatchRoleIdleSlots => "loom.dispatch.role_idle_slots",
            Self::ForgeStageDwell => "loom.forge.stage_dwell",
            Self::ForgeStageDwellSamples => "loom.forge.stage_dwell.samples",
            Self::ForgeStageItems => "loom.forge.stage_items",
            Self::ForgeStageOldestAgeSeconds => "loom.forge.stage_oldest_age_seconds",
            Self::StepActionable => "loom.step.actionable",
            Self::QueueDispositionRowsDropped => "loom.queue.disposition_rows_dropped",
            Self::GithubRateLimitRemaining => "github.ratelimit.remaining",
            Self::GithubRateLimitUsed => "github.ratelimit.used",
            Self::GithubRateLimitReset => "github.ratelimit.reset",
            Self::GithubRateLimitBreakerSkips => "github.ratelimit.breaker_skips",
            Self::ForgeCalls => "loom.forge.calls",
            Self::ForgeFacadeEvents => "loom.forge.facade.events",
            Self::MergeRedatePrs => "loom.merge.redate_prs",
            Self::MergeRedatesMax => "loom.merge.redates_max",
            Self::MergeTimeToLandMax => "loom.merge.time_to_land_max",
            Self::EgressRequests => "loom.egress.requests",
            Self::EgressTokens => "loom.egress.tokens",
            Self::EgressLatency => "loom.egress.latency",
            Self::EgressTtft => "loom.egress.ttft",
            Self::DaemonTaskAlive => "loom.daemon.task_alive",
            Self::DaemonTaskFaults => "loom.daemon.task_faults",
            Self::DaemonIpcLatencyMax => "loom.daemon.ipc.latency_max",
            Self::DaemonIpcLatency => "loom.daemon.ipc.latency",
            Self::DaemonIpcRequests => "loom.daemon.ipc.requests",
            Self::DaemonIpcStatusBuilds => "loom.daemon.ipc.status_builds",
            Self::CodexSessionState => "loom.codex_session.state",
            Self::CodexSessionRecord => "loom.codex_session.record",
            Self::CodexSessionMountDrift => "loom.codex_session.mount_drift",
            Self::CaptainGaugeAgeSeconds => "loom.captain.gauge_age_seconds",
            Self::CaptainGaugeFallback => "loom.captain.gauge_fallback",
            Self::AgentScopePeakMemoryBytes => "loom.agent_scope.peak_memory_bytes",
            Self::AgentScopeOomKills => "loom.agent_scope.oom_kills",
            Self::ForgeMirrorReads => "loom.forge.mirror.reads",
            Self::ForgeMirrorChanges => "loom.forge.mirror.changes",
            Self::ForgeMirrorWakeLatency => "loom.forge.mirror.wake_latency",
            Self::StageEntryLookups => "loom.stage_entry.lookups",
            Self::FeedCompletions => "loom.feed.completions",
            Self::FeedReconcilePasses => "loom.feed.reconcile_passes",
        }
    }

    /// Gauge or delta counter.
    #[must_use]
    pub fn kind(self) -> MetricKind {
        match self {
            Self::DispatchDecisions
            | Self::DispatchPrOpenPrefiltered
            | Self::LlmTokensInput
            | Self::LlmTokensOutput
            | Self::LlmTokensCacheRead
            | Self::LlmTokensCacheWrite
            | Self::LlmRequests
            | Self::PoolExhaustions
            | Self::PoolExhaustedSeconds
            | Self::PoolAccountMarks
            | Self::QueueDispatchWait
            | Self::QueueDispatchWaitSamples
            | Self::DispatchSlotTurnaround
            | Self::DispatchSlotTurnaroundSamples
            | Self::DispatchIdleSlotSeconds
            | Self::ForgeStageDwell
            | Self::ForgeStageDwellSamples
            | Self::QueueDispositionRowsDropped
            | Self::GithubRateLimitBreakerSkips
            | Self::ForgeCalls
            | Self::ForgeFacadeEvents
            | Self::EgressRequests
            | Self::EgressTokens
            | Self::EgressLatency
            | Self::EgressTtft
            | Self::DaemonTaskFaults
            | Self::DaemonIpcLatency
            | Self::DaemonIpcRequests
            | Self::DaemonIpcStatusBuilds
            | Self::ForgeMirrorReads
            | Self::ForgeMirrorChanges
            | Self::ForgeMirrorWakeLatency
            | Self::StageEntryLookups
            | Self::FeedCompletions
            | Self::FeedReconcilePasses
            | Self::AgentScopeOomKills => MetricKind::DeltaCounter,
            _ => MetricKind::Gauge,
        }
    }

    /// UCUM unit string.
    #[must_use]
    pub fn unit(self) -> &'static str {
        match self {
            Self::DispatchDecisions
            | Self::DispatchCandidates
            | Self::QueueStarved
            | Self::QueueStarvedByReason
            | Self::QueueDispatchWaitSamples => "{issue}",
            Self::DispatchMaxConcurrent | Self::DispatchCapTerm => "{sweep}",
            Self::QueueIssues | Self::QueueAlreadyLanded | Self::DispatchPrOpenPrefiltered => {
                "{issue}"
            }
            Self::QueueListingFailedRepos | Self::QueueListingIncompleteRepos => "{repository}",
            Self::LlmTokensInput
            | Self::LlmTokensOutput
            | Self::LlmTokensCacheRead
            | Self::LlmTokensCacheWrite => "{token}",
            Self::LlmRequests => "{request}",
            Self::PoolAccounts | Self::PoolExhaustions | Self::PoolAccountMarks => "{account}",
            Self::PoolExhausted | Self::PoolAccountState => "1",
            Self::PoolExhaustedSeconds | Self::PoolPlanWindowSeconds => "s",
            Self::PoolPlanTokenLimit => "{token}",
            Self::QueueOldestWait | Self::QueueDispatchWait => "s",
            Self::DispatchSlotTurnaround
            | Self::DispatchIdleSlotSeconds
            | Self::ForgeStageDwell => "s",
            Self::DispatchSlotTurnaroundSamples
            | Self::DispatchIdleSlots
            | Self::DispatchBusySlots
            | Self::DispatchRoleMaxConcurrent
            | Self::DispatchRoleIdleSlots => "{slot}",
            Self::ForgeStageDwellSamples | Self::ForgeStageItems | Self::StepActionable => "{item}",
            Self::QueueDispositionRowsDropped => "{issue}",
            Self::GithubRateLimitRemaining | Self::GithubRateLimitUsed => "{request}",
            Self::GithubRateLimitReset | Self::ForgeStageOldestAgeSeconds => "s",
            Self::GithubRateLimitBreakerSkips => "{pass}",
            Self::ForgeCalls => "{request}",
            Self::ForgeFacadeEvents => "{event}",
            Self::MergeRedatePrs => "{pull_request}",
            Self::MergeRedatesMax => "{redate}",
            Self::MergeTimeToLandMax => "s",
            Self::EgressRequests => "{request}",
            Self::EgressTokens => "{token}",
            Self::EgressLatency | Self::EgressTtft => "s",
            Self::DaemonTaskAlive => "1",
            Self::DaemonTaskFaults => "{fault}",
            Self::DaemonIpcLatencyMax | Self::DaemonIpcLatency => "s",
            Self::DaemonIpcRequests => "{request}",
            Self::DaemonIpcStatusBuilds => "{build}",
            Self::CodexSessionState => "1",
            Self::CodexSessionRecord => "1",
            Self::CodexSessionMountDrift => "{path}",
            Self::CaptainGaugeAgeSeconds => "s",
            Self::CaptainGaugeFallback => "1",
            Self::ForgeMirrorReads => "{request}",
            Self::ForgeMirrorChanges => "{change}",
            Self::ForgeMirrorWakeLatency => "s",
            Self::StageEntryLookups => "{lookup}",
            Self::FeedCompletions => "{completion}",
            Self::FeedReconcilePasses => "{pass}",
            Self::AgentScopeOomKills => "{kill}",
            _ => "By",
        }
    }

    /// One-line description.
    #[must_use]
    pub fn description(self) -> &'static str {
        match self {
            Self::DispatchDecisions => "Work-finder candidate outcomes per tick, by reason.",
            Self::DispatchCandidates => "Ready candidates seen by one work-finder tick.",
            Self::DispatchMaxConcurrent => "Dynamic concurrency cap for the work-finder tick.",
            Self::DispatchCapTerm => "The configured, disk and RAM terms of the tick's cap.",
            Self::DispatchPrOpenPrefiltered => {
                "Open-PR skips the pre-filter made with no dispatch attempt."
            }
            Self::QueueOldestWait => "Age of the oldest waiting ready-queue issue, by state.",
            Self::QueueStarved => "Waiting ready-queue issues past the starvation threshold.",
            Self::QueueStarvedByReason => "Starved ready-queue issues by queue disposition.",
            Self::QueueDispatchWait => "Summed queue dwell of the issues dispatched per tick.",
            Self::QueueDispatchWaitSamples => "Issues counted in loom.queue.dispatch_wait.",
            Self::HostMemoryAvailableBytes => "Memory available without swapping.",
            Self::HostMemoryTotalBytes => "Physical memory installed.",
            Self::HostSwapUsedBytes => "Swap space in use.",
            Self::HostSwapTotalBytes => "Swap space configured.",
            Self::HostWorktreeVolumeFreeBytes => "Free space on the worktree-root volume.",
            Self::HostWorktreeVolumeTotalBytes => "Capacity of the worktree-root volume.",
            Self::QueueIssues => "Ready issues on the last work-finder tick, by state and reason.",
            Self::QueueListingFailedRepos => "Repos whose ready-issue listing failed last tick.",
            Self::QueueListingIncompleteRepos => {
                "Repos whose ready-issue listing came back partial last tick."
            }
            Self::QueueAlreadyLanded => "Already-landed ready issues last tick, by evidence.",
            Self::LlmTokensInput => "Uncached input tokens consumed, by provider and model.",
            Self::LlmTokensOutput => "Output tokens produced, by provider and model.",
            Self::LlmTokensCacheRead => "Cache-read input tokens, by provider and model.",
            Self::LlmTokensCacheWrite => "Cache-write input tokens, by provider and model.",
            Self::LlmRequests => "Model API responses, by provider and model.",
            Self::PoolAccounts => "Enabled pool accounts by provider and state.",
            Self::PoolExhausted => "1 when no account in the provider's pool is usable.",
            Self::PoolExhaustions => "Accounts that became exhausted since the last sample.",
            Self::PoolExhaustedSeconds => "Seconds the provider's pool read as exhausted.",
            Self::PoolAccountState => "1 for the state a pool account is in, else 0.",
            Self::PoolPlanTokenLimit => "Declared plan token allowance per window.",
            Self::PoolPlanWindowSeconds => "Declared plan quota window.",
            Self::PoolAccountMarks => {
                "Pool accounts marked out of selection, by provider and reason."
            }
            Self::DispatchSlotTurnaround => {
                "Seconds from an issue-sweep slot freeing to its refill."
            }
            Self::DispatchSlotTurnaroundSamples => {
                "Refills counted in loom.dispatch.slot_turnaround."
            }
            Self::DispatchIdleSlots => "Concurrency slots idle at the end of a work-finder tick.",
            Self::DispatchIdleSlotSeconds => "Slot-seconds left idle while ready work waited.",
            Self::DispatchBusySlots => {
                "Busy agent slots, by role (sweep, role-runner role, or step role with pool=step)."
            }
            Self::DispatchRoleMaxConcurrent => "The role-runner concurrent agent ceiling.",
            Self::DispatchRoleIdleSlots => "Role-runner agent slots idle under the ceiling.",
            Self::ForgeStageDwell => "Seconds items spent in a forge label stage, by state.",
            Self::ForgeStageDwellSamples => "Stage transitions counted in loom.forge.stage_dwell.",
            Self::ForgeStageItems => {
                "Open items per repo by state: stage labels and queue states (overlapping)."
            }
            Self::ForgeStageOldestAgeSeconds => {
                "Age of the oldest item in a stage_items state, by reason (pr_refused)."
            }
            Self::StepActionable => "Items the step tick could draw this tick, per repo.",
            Self::QueueDispositionRowsDropped => {
                "Ready-queue rows dropped from a disposition export pass, by reason."
            }
            Self::GithubRateLimitRemaining => "GitHub API requests left, per bucket.",
            Self::GithubRateLimitUsed => "GitHub API requests spent this window, per bucket.",
            Self::GithubRateLimitReset => "GitHub rate-limit window reset, Unix epoch seconds.",
            Self::GithubRateLimitBreakerSkips => {
                "Job passes skipped by the rate-limit breaker, by job."
            }
            Self::ForgeCalls => {
                "GitHub requests sent by the gh facade, by caller, bucket and outcome."
            }
            Self::ForgeFacadeEvents => {
                "Named gh-facade events that are not forge calls, by reason (counter name)."
            }
            Self::MergeRedatePrs => "PRs re-dated in the trailing window, by landing state.",
            Self::MergeRedatesMax => "Most re-dates on one PR in the trailing window, by state.",
            Self::MergeTimeToLandMax => {
                "Longest first-re-date-to-landing time of a PR landed in the window."
            }
            Self::DaemonTaskAlive => "1 while a long-running daemon loop is beating, by task.",
            Self::EgressRequests => "Provider requests observed by the egress proxy, by seat.",
            Self::EgressTokens => "Tokens read off provider responses by the egress proxy.",
            Self::EgressLatency => "Summed egress-proxy request latency, by seat.",
            Self::EgressTtft => "Summed egress-proxy time to first response byte, by seat.",
            Self::DaemonTaskFaults => "Faults of a long-running daemon loop, by task and reason.",
            Self::DaemonIpcLatencyMax => "Slowest IPC request answered in the interval, by kind.",
            Self::DaemonIpcLatency => "Summed IPC request latency, by request kind.",
            Self::DaemonIpcRequests => "IPC requests answered, by request kind.",
            Self::DaemonIpcStatusBuilds => "DaemonStatus builds finished, by outcome.",
            Self::CodexSessionState => {
                "Codex session container state per account: 1 for the current state \
                 (running, stopped, restarting, missing, stale_mounts), 0 for the others."
            }
            Self::CodexSessionRecord => {
                "1 while an operator hold or a drift-removal record stands for a Codex \
                 session account, by kind."
            }
            Self::CodexSessionMountDrift => {
                "Workspace paths a Codex session container is missing, mounts extra, or \
                 mounts though denied, by kind."
            }
            Self::CaptainGaugeAgeSeconds => {
                "Age of the fleet captain's last run of a fleet gauge job, by task."
            }
            Self::CaptainGaugeFallback => {
                "1 while a dispatcher produces a fleet gauge job locally (captain stale)."
            }
            Self::AgentScopePeakMemoryBytes => {
                "Peak memory (cgroup memory.peak) of one finished agent scope, by repo."
            }
            Self::AgentScopeOomKills => {
                "Processes OOM-killed inside finished agent scopes, by repo and role."
            }
            Self::ForgeMirrorReads => {
                "Reads that asked the GitHub state mirror, by caller, repo and outcome."
            }
            Self::ForgeMirrorChanges => "Changes the state mirror's change stream delivered.",
            Self::ForgeMirrorWakeLatency => {
                "Summed seconds from a change at the state mirror to the pull that saw it."
            }
            Self::StageEntryLookups => {
                "Stage-entry times the step engine looked up, by repo and answering source (kind)."
            }
            Self::FeedCompletions => {
                "Public-feed completions the narration sink built, by trigger (kind) and outcome."
            }
            Self::FeedReconcilePasses => {
                "Merge-reconciliation passes of the narration sink, by outcome."
            }
        }
    }
}

/// One data point's value.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum MetricValue {
    Int(i64),
    Double(f64),
}

/// One data point.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MetricPoint {
    pub name: MetricName,
    pub value: MetricValue,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub labels: BTreeMap<String, String>,
}

impl MetricPoint {
    /// An unlabelled integer point.
    #[must_use]
    pub fn int(name: MetricName, value: i64) -> Self {
        MetricPoint {
            name,
            value: MetricValue::Int(value),
            labels: BTreeMap::new(),
        }
    }

    /// An unlabelled floating-point point.
    #[must_use]
    pub fn double(name: MetricName, value: f64) -> Self {
        MetricPoint {
            name,
            value: MetricValue::Double(value),
            labels: BTreeMap::new(),
        }
    }

    /// Add one label (builder style). Value policy is applied at emit and
    /// again at export; an unallowlisted key is a programming error, caught in
    /// debug builds here and dropped by [`bounded_labels`] in release.
    #[must_use]
    pub fn label(mut self, key: &str, value: impl Into<String>) -> Self {
        debug_assert!(
            OPS_METRIC_LABEL_KEYS.contains(&key),
            "metric label key {key:?} is not in OPS_METRIC_LABEL_KEYS"
        );
        self.labels.insert(key.to_string(), value.into());
        self
    }
}

/// `metric.points` — a batch of points sampled at one instant. Host-level: it
/// references no repository, so it carries no visibility tag.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MetricPointsRecord {
    /// When the points were sampled.
    pub captured_at: DateTime<Utc>,
    /// Start of the interval the delta counters in this batch cover (the OTLP
    /// `start_time_unix_nano`). `None` falls back to `captured_at`; gauges
    /// ignore it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interval_start: Option<DateTime<Utc>>,
    pub points: Vec<MetricPoint>,
}

/// Keep only allowlisted, short, control-free labels, at most
/// [`MAX_LABELS_PER_POINT`] of them.
#[must_use]
pub fn bounded_labels(labels: &BTreeMap<String, String>) -> BTreeMap<String, String> {
    labels
        .iter()
        .filter(|(key, value)| {
            OPS_METRIC_LABEL_KEYS.contains(&key.as_str())
                && value.len() <= MAX_LABEL_VALUE_BYTES
                && !value.chars().any(char::is_control)
        })
        .take(MAX_LABELS_PER_POINT)
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect()
}

impl MetricPointsRecord {
    /// Points with policy applied: at most [`MAX_POINTS_PER_RECORD`], labels
    /// bounded, non-finite doubles dropped.
    #[must_use]
    pub fn bounded_points(&self) -> Vec<MetricPoint> {
        self.points
            .iter()
            .filter(|point| match point.value {
                MetricValue::Int(_) => true,
                MetricValue::Double(value) => value.is_finite(),
            })
            .take(MAX_POINTS_PER_RECORD)
            .map(|point| MetricPoint {
                name: point.name,
                value: point.value,
                labels: bounded_labels(&point.labels),
            })
            .collect()
    }
}
