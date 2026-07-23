use crate::activity::{ActivityEntry, ClaimResult, ClaimType, ClaimsSummary, IssueClaim};
use crate::errors::DaemonError;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

pub type TerminalId = String;

/// Unique identifier for a sweep dispatched via the daemon (Issue #3452).
///
/// Format mirrors the spawn-loop convention:
/// `sweep-issue-<N>-<unix-secs>` or `sweep-prs-<n1>-<n2>-..-<unix-secs>`.
pub type SweepId = String;

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", content = "payload")]
pub enum Request {
    Ping,
    CreateTerminal {
        config_id: String,
        name: String,
        working_dir: Option<String>,
        role: Option<String>,
        instance_number: Option<u32>,
    },
    ListTerminals,
    DestroyTerminal {
        id: TerminalId,
    },
    SendInput {
        id: TerminalId,
        data: String,
    },
    GetTerminalOutput {
        id: TerminalId,
        start_byte: Option<usize>,
    },
    ResizeTerminal {
        id: TerminalId,
        cols: u16,
        rows: u16,
    },
    CheckSessionHealth {
        id: TerminalId,
    },
    ListAvailableSessions,
    AttachToSession {
        id: TerminalId,
        session_name: String,
    },
    KillSession {
        session_name: String,
    },
    SetWorktreePath {
        id: TerminalId,
        worktree_path: String,
    },
    GetTerminalActivity {
        id: TerminalId,
        limit: usize,
    },
    /// Capture git changes for a specific input
    /// Called after a prompt completes to record code changes
    CaptureGitChanges {
        input_id: i64,
        working_dir: String,
        before_commit: Option<String>,
    },
    /// Get the current git commit hash for a directory
    GetCurrentCommit {
        working_dir: String,
    },
    // ========================================================================
    // Issue Claim Registry Requests (Issue #1159)
    // ========================================================================
    /// Claim an issue or PR for a terminal
    ClaimIssue {
        number: i32,
        claim_type: ClaimType,
        terminal_id: TerminalId,
        label: Option<String>,
        agent_role: Option<String>,
        /// Stale threshold in seconds (default: 3600 = 1 hour)
        stale_threshold_secs: Option<i64>,
    },
    /// Release a claim on an issue or PR
    ReleaseClaim {
        number: i32,
        claim_type: ClaimType,
        /// Only release if owned by this terminal
        terminal_id: Option<TerminalId>,
    },
    /// Update heartbeat for an active claim
    HeartbeatClaim {
        number: i32,
        claim_type: ClaimType,
        terminal_id: TerminalId,
    },
    /// Get a specific claim
    GetClaim {
        number: i32,
        claim_type: ClaimType,
    },
    /// Get all claims for a terminal
    GetTerminalClaims {
        terminal_id: TerminalId,
    },
    /// Get all active claims
    GetAllClaims,
    /// Get claims summary
    GetClaimsSummary {
        /// Stale threshold in seconds (default: 3600 = 1 hour)
        stale_threshold_secs: Option<i64>,
    },
    /// Release all stale claims (crash recovery)
    ReleaseStaleCliams {
        /// Stale threshold in seconds (default: 3600 = 1 hour)
        stale_threshold_secs: Option<i64>,
    },
    /// Release all claims for a terminal
    ReleaseTerminalClaims {
        terminal_id: TerminalId,
    },
    // ========================================================================
    // Sweep Registry Requests (Issue #3452 — Phase A of #3449)
    // ========================================================================
    /// Dispatch a `/loom:sweep` child for the given kind.
    ///
    /// Shells out to `defaults/scripts/spawn-claude.sh` for token rotation and
    /// detaches a `claude -p "/loom:sweep <args>"` child. Tracking is in-memory
    /// only — no daemon state file is written.
    ///
    /// `idempotency_key` allows the caller to deduplicate concurrent dispatches.
    /// If a `Running` sweep with the same key exists, the existing `sweep_id`
    /// is returned with no new spawn. If the matching sweep has `Exited` or
    /// `Crashed`, a new sweep is spawned.
    ///
    /// `model` (issue #3477, Phase 1) optionally selects the Claude model for
    /// the spawned child. When `Some`, the daemon appends `--model <value>`
    /// to the `spawn-claude.sh` invocation — the highest-precedence tier of
    /// the model chain (explicit dispatch param, then workspace
    /// `roleConfig.model`, then role `suggestedModel`, then session default).
    /// When `None` (or absent on the wire — `#[serde(default)]` keeps
    /// existing clients compatible), NO `--model` flag is emitted and the
    /// session/CLI default is preserved.
    ///
    /// `effort` (issue #3716) mirrors `model` exactly: it optionally selects
    /// the reasoning-effort level (`low|medium|high|xhigh|max`) for the
    /// spawned child. When `Some` and non-empty, the daemon appends
    /// `--effort <level>` to the `spawn-claude.sh` invocation (the
    /// highest-precedence tier, beating any ambient `LOOM_EFFORT`). When
    /// `None` (or absent on the wire — `#[serde(default)]` keeps existing
    /// clients compatible) or empty, NO `--effort` flag is emitted and the
    /// session-default effort is preserved.
    ///
    /// `depends_on` (issue #3729, stacked-PR v1) optionally names the single
    /// parent issue this sweep is stacked on. When `Some(N)`, the daemon
    /// appends `--depends-on <N>` to the `/loom:sweep` argv (mirroring the
    /// `--model` / `--effort` append-only, empty-means-unset contract), and
    /// the spawned child branches its worktree/PR off `feature/issue-<N>`
    /// instead of the default branch. When `None` (or absent on the wire —
    /// `#[serde(default)]` keeps existing clients compatible), NO
    /// `--depends-on` flag is emitted and behavior is byte-for-byte
    /// unchanged. A single optional parent (not a `Vec`) makes diamonds /
    /// multi-parent stacks structurally unrepresentable — see #3729 v1 scope.
    DispatchSweep {
        kind: SweepKind,
        idempotency_key: Option<String>,
        #[serde(default)]
        model: Option<String>,
        #[serde(default)]
        effort: Option<String>,
        #[serde(default)]
        depends_on: Option<u32>,
    },
    /// List tracked sweeps, optionally filtered by state.
    ListSweeps {
        state_filter: Option<SweepState>,
    },
    // ========================================================================
    // Event Bus Requests (Issue #3453 — Phase B of #3449)
    // ========================================================================
    /// Publish a sweep-lifecycle event onto the in-memory bus.
    ///
    /// `topic` must follow the frozen taxonomy (`sweep.issue.{N}.phase`,
    /// `sweep.issue.{N}.blocker`, `sweep.issue.{N}.exited`, etc.). The bus
    /// itself accepts arbitrary topic strings, but downstream consumers
    /// only subscribe to the documented topics.
    ///
    /// `payload` is opaque JSON — the schema is per-topic and documented
    /// in `defaults/.claude/commands/loom/sweep.md`.
    PublishEvent {
        topic: String,
        payload: serde_json::Value,
    },
    /// Subscribe to one or more topic prefixes on the event bus.
    ///
    /// This is a long-lived request: instead of returning a single
    /// `Response`, the daemon streams `Response::EventStream { events }`
    /// frames over the open connection as events arrive on the bus.
    /// An empty `topics` vec subscribes to all events on the bus (useful
    /// for the `tail_event_bus` debug tool slated for Phase C).
    ///
    /// Topic matching is **prefix match**, segment-aligned —
    /// `sweep.issue` matches `sweep.issue.123.phase` but not
    /// `sweep.issuetype.foo`. See `event_bus::topic_matches` for the
    /// authoritative routing rule.
    SubscribeEvents {
        topics: Vec<String>,
    },
    // ========================================================================
    // Sweep Monitoring Requests (Issue #3455 — Phase C of #3449)
    // ========================================================================
    /// Return the `SweepInfo` for a given sweep ID. The daemon does NOT
    /// include the recent event log here — recent events are filtered
    /// client-side via a separate `SubscribeEvents` call. Phase C exposes
    /// this as `get_sweep_status` in the MCP layer.
    GetSweepStatus {
        sweep_id: SweepId,
    },
    /// Read the last `lines` lines from a sweep's per-sweep log file.
    /// Resolved relative to the registry's workspace root. Used by the
    /// `tail_sweep_log` MCP tool.
    TailSweepLog {
        sweep_id: SweepId,
        lines: usize,
    },
    /// Cancel a running sweep: send SIGTERM, wait the grace window, then
    /// SIGKILL if still alive. Transitions the registry entry from
    /// `Running` -> `Exited{code: None, at: now}` and releases the lock.
    CancelSweep {
        sweep_id: SweepId,
        /// Seconds to wait between SIGTERM and SIGKILL. Defaults are
        /// chosen by the MCP layer; the daemon honours whatever value
        /// arrives in the request.
        grace_secs: u64,
    },
    // ========================================================================
    // Autonomous Daemon Status (Issue #3891 — follow-up to #3813 Phase D)
    // ========================================================================
    /// Request the daemon's autonomous-mode operability snapshot: the live
    /// in-flight sweeps, the three dynamic-cap inputs (token-pool size, disk
    /// headroom, configured ceiling) plus their `min` cap, and the reactive
    /// main-health-gate halt state.
    ///
    /// Per-token usage is deliberately NOT part of this response — probing each
    /// account for rate-limit headers is a slow network call that would block
    /// the IPC handler, so the `loom-daemon status` CLI shells out to
    /// `loom-tokens check --json` client-side (mirroring `probe-tokens.sh`).
    DaemonStatus,
    Shutdown,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", content = "payload")]
pub enum Response {
    Pong,
    TerminalCreated {
        id: TerminalId,
    },
    TerminalList {
        terminals: Vec<TerminalInfo>,
    },
    TerminalOutput {
        output: String,
        byte_count: usize,
    },
    /// Response from `SendInput` with tracking info for git changes
    InputSent {
        input_id: i64,
        before_commit: Option<String>,
    },
    SessionHealth {
        has_session: bool,
    },
    AvailableSessions {
        sessions: Vec<String>,
    },
    TerminalActivity {
        entries: Vec<ActivityEntry>,
    },
    /// Response with current git commit hash
    CurrentCommit {
        commit: Option<String>,
    },
    /// Response with git changes captured
    GitChangesCaptured {
        files_changed: i32,
        lines_added: i32,
        lines_removed: i32,
    },
    // ========================================================================
    // Issue Claim Registry Responses (Issue #1159)
    // ========================================================================
    /// Result of claiming an issue
    ClaimResult(ClaimResult),
    /// A specific claim
    Claim(Option<IssueClaim>),
    /// List of claims
    Claims(Vec<IssueClaim>),
    /// Claims summary
    ClaimsSummary(ClaimsSummary),
    /// Count of claims released
    ClaimsReleased {
        count: usize,
    },
    Success,
    // ========================================================================
    // Sweep Registry Responses (Issue #3452 — Phase A of #3449)
    // ========================================================================
    /// Result of a successful `DispatchSweep` request.
    SweepDispatched {
        sweep_id: SweepId,
        pid: u32,
        token_name: String,
        log_path: PathBuf,
    },
    /// Result of a `ListSweeps` request.
    SweepList {
        sweeps: Vec<SweepInfo>,
    },
    // ========================================================================
    // Event Bus Responses (Issue #3453 — Phase B of #3449)
    // ========================================================================
    /// Acknowledgement frame returned by a successful `PublishEvent`.
    /// Includes the receiver count so debug tooling can verify routing.
    EventPublished {
        topic: String,
        receivers: usize,
    },
    /// A frame in the long-lived event stream returned by `SubscribeEvents`.
    ///
    /// Each frame may carry one or more events. The daemon sends one
    /// frame per event in practice; the `events` vec is a structural
    /// allowance for future batching without a wire-protocol change.
    EventStream {
        events: Vec<Event>,
    },
    // ========================================================================
    // Sweep Monitoring Responses (Issue #3455 — Phase C of #3449)
    // ========================================================================
    /// Result of a `GetSweepStatus` request.
    ///
    /// `info` is `None` when no sweep with the requested ID is tracked.
    SweepStatus {
        info: Option<SweepInfo>,
    },
    /// Result of a `TailSweepLog` request.
    ///
    /// `lines` carries the requested tail (most-recent last). Missing
    /// log files are reported via `Response::Error` instead of an empty
    /// vec, so callers can distinguish "no entries yet" from "log gone".
    SweepLogTail {
        sweep_id: SweepId,
        lines: Vec<String>,
        /// Path actually read; useful for surfacing in operator output.
        log_path: PathBuf,
    },
    /// Result of a `CancelSweep` request.
    ///
    /// `was_running` is `false` when the sweep was already terminal at
    /// the moment of the cancel call (no-op success — the registry
    /// state is unchanged).
    SweepCancelled {
        sweep_id: SweepId,
        pid: u32,
        sigkill_sent: bool,
        was_running: bool,
    },
    // ========================================================================
    // Autonomous Daemon Status (Issue #3891 — follow-up to #3813 Phase D)
    // ========================================================================
    /// Result of a `DaemonStatus` request — the autonomous-mode operability
    /// snapshot rendered by `loom-daemon status`.
    DaemonStatus(DaemonStatusReport),
    /// Legacy error response (deprecated, use `StructuredError` for new code)
    /// Kept for backwards compatibility with existing frontends
    Error {
        message: String,
    },
    /// Structured error response with typed domains (Issue #1171)
    /// Provides rich error information for smart error handling
    StructuredError(DaemonError),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TerminalInfo {
    pub id: TerminalId,
    pub name: String,
    pub tmux_session: String,
    pub working_dir: Option<String>,
    pub created_at: i64,
    // Agent-specific fields
    pub role: Option<String>,
    pub worktree_path: Option<String>,
    pub agent_pid: Option<u32>,
    #[serde(default)]
    pub agent_status: AgentStatus,
    pub last_interval_run: Option<i64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AgentStatus {
    #[default]
    NotStarted,
    Initializing,
    Ready,
    Busy,
    WaitingForInput,
    Error,
    Stopped,
}

// ========================================================================
// Sweep Registry Types (Issue #3452 — Phase A of #3449)
// ========================================================================

/// The kind of sweep to dispatch.
///
/// This phase delivers issue-keyed dispatch. PR-set dispatch (Mode C) is
/// reserved here for future phases — the daemon's API surface accepts it,
/// but Phase A only fully implements `Issue`. `PrSet` is an explicit
/// non-goal for Phase A per epic #3449.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", content = "value")]
pub enum SweepKind {
    /// Issue-keyed sweep: `claude -p "/loom:sweep <N>"`.
    Issue(u32),
    /// PR-set sweep: `claude -p "/loom:sweep --prs <n1> <n2> ..."`.
    /// Reserved for future phases; current code returns an error.
    PrSet(Vec<u32>),
}

/// Lifecycle state of a tracked sweep.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "state", content = "details")]
pub enum SweepState {
    /// Spawn requested but the child PID has not yet been confirmed alive.
    /// In Phase A this transient state collapses immediately into `Running`,
    /// but the variant is reserved for future async-spawn paths.
    Pending,
    /// Child PID is alive (verified by the most recent reaper tick).
    Running,
    /// Child exited; recorded by the reaper task on a `kill(pid, 0)` failure.
    Exited {
        /// Exit code if available (`waitpid` is not used post-detach;
        /// in practice this is always `None` for detached children).
        code: Option<i32>,
        at: DateTime<Utc>,
    },
    /// Child died with a checkpoint present on disk; the reaper has
    /// flipped the issue label back to `loom:issue` so the next dispatch
    /// can resume from the checkpointed phase (sweep skill #3373).
    Crashed { at: DateTime<Utc> },
}

impl SweepState {
    /// Returns true when the sweep is no longer live.
    #[must_use]
    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Exited { .. } | Self::Crashed { .. })
    }
}

/// In-memory record of a dispatched sweep.
///
/// This is the schema returned by `ListSweeps`; downstream consumers
/// (mcp-loom, UI) should treat this as the canonical sweep shape.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SweepInfo {
    /// Stable opaque ID assigned at dispatch time.
    pub sweep_id: SweepId,
    /// The dispatched kind (used to render the prompt).
    pub kind: SweepKind,
    /// PID of the detached child process.
    pub pid: u32,
    /// Token account name selected by `spawn-claude.sh` (e.g. `agent-2.token`).
    /// "unknown" when not surfaced by the wrapper (Phase A logs this in
    /// the per-sweep log rather than recording it on the entry).
    pub token_name: String,
    /// Path to the per-sweep log file (relative to the workspace).
    pub log_path: PathBuf,
    /// Optional idempotency key supplied at dispatch.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub idempotency_key: Option<String>,
    /// Timestamp of the original spawn.
    pub started_at: DateTime<Utc>,
    /// Current lifecycle state.
    pub state: SweepState,
    /// Most-recent phase the sweep advertised via its checkpoint, if any.
    /// Populated by `sweep_registry::reconstruct_from_checkpoints`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub latest_phase: Option<String>,
    /// PR number the sweep eventually opened, if known. Reserved for
    /// future phases (Phase A always sets this to `None`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pr_number: Option<i32>,
    /// Model requested at dispatch time (issue #3482, Phase 3a
    /// observability). Mirrors the `model` param of `DispatchSweep`:
    /// `Some(value)` when an explicit non-empty model was supplied,
    /// `None` otherwise — consumers should render `None` as "default"
    /// (the child inherited the session/CLI default; no `--model` flag
    /// was emitted). `#[serde(default)]` keeps pre-#3482 wire data and
    /// clients compatible.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Reasoning-effort level requested at dispatch time (issue #3716).
    /// Mirrors the `effort` param of `DispatchSweep`: `Some(level)` when an
    /// explicit non-empty effort was supplied, `None` otherwise — consumers
    /// should render `None` as "default" (the child inherited the
    /// session-default effort; no `--effort` flag was emitted).
    /// `#[serde(default)]` keeps pre-#3716 wire data and clients compatible.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    /// Single parent issue this sweep is stacked on (issue #3729, stacked-PR
    /// v1). Mirrors the `depends_on` param of `DispatchSweep`: `Some(N)` when
    /// the sweep was dispatched with `--depends-on <N>` (so its worktree/PR
    /// branches off `feature/issue-<N>`), `None` for an independent sweep.
    /// The reaper uses this to block a stacked child's subtree when its
    /// parent ends in `loom:blocked` (block-the-subtree, #3729 item 4).
    /// A single optional parent makes diamonds structurally unrepresentable.
    /// `#[serde(default)]` keeps pre-#3729 wire data and clients compatible.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub depends_on: Option<u32>,
}

// ========================================================================
// Autonomous Daemon Status Types (Issue #3891 — follow-up to #3813 Phase D)
// ========================================================================

/// The autonomous-mode operability snapshot returned by `Request::DaemonStatus`
/// and rendered by the `loom-daemon status` CLI subcommand.
///
/// This mirrors, at the daemon-native level, what the tmux-pool `loom status`
/// shows for the terminal pool (#3735 precedent): what work is live and what
/// the concurrency ceiling currently is. The per-token usage table the CLI also
/// prints is NOT included here — it is a slow per-account network probe the CLI
/// collects client-side via `loom-tokens check --json` (mirroring
/// `probe-tokens.sh`), so the IPC handler stays fast.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DaemonStatusReport {
    /// Sweeps in a non-terminal state (`Pending` / `Running`) at snapshot time.
    /// The full `SweepInfo` is carried so the CLI can render issue numbers,
    /// PIDs, token account, and latest phase without a second round-trip.
    pub in_flight: Vec<SweepInfo>,
    /// Dynamic-cap input 1: size of the multi-account token pool
    /// (`.loom/tokens/*.token`), the hard ceiling on concurrent sweeps
    /// (never over-subscribe an OAuth account). Via [`crate::tokens::token_pool_size`].
    pub token_pool_size: usize,
    /// Dynamic-cap input 2: how many worktrees the scratch volume can hold at
    /// `LOOM_PER_WORKTREE_GB` each. Via [`crate::disk_headroom::disk_headroom_limit`].
    pub disk_headroom: usize,
    /// Dynamic-cap input 3: the configured operator ceiling
    /// (`autonomous.workFinder.maxConcurrent` / `LOOM_WORK_FINDER_MAX_CONCURRENT`).
    pub configured_max: usize,
    /// The effective dynamic concurrency cap — `min` of the three inputs above
    /// (`resolve_dynamic_max_concurrent`). This is the total-occupancy ceiling
    /// the work finder recomputes every tick.
    pub dynamic_cap: usize,
    /// Whether autonomous dispatch is currently halted by the reactive
    /// main-health gate (#3812). `true` means a red `main` has paused new
    /// dispatch (in-flight sweeps keep running); `false` means dispatch is
    /// allowed. Always `false` when the gate loop is not enabled.
    pub main_health_gate_halted: bool,
    /// Token-capacity backpressure snapshot (#3902): account health derived from
    /// the rotation ranking (`.loom/tokens/.ranking`) and whether the token axis
    /// is the binding constraint on the dynamic cap. `#[serde(default)]` keeps
    /// pre-#3902 wire data / older clients compatible.
    #[serde(default)]
    pub capacity: CapacityReport,
}

/// The token-capacity section of [`DaemonStatusReport`] (#3902).
///
/// Derived from the rotation ranking file (`.loom/tokens/.ranking`) — a fast
/// filesystem read, no network probe. When no ranking exists (`ranking_present`
/// is `false`) the health counts are zero and `token_axis_limit` equals the raw
/// token-pool size (byte-for-byte the pre-#3902 dynamic-cap basis).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct CapacityReport {
    /// Whether a ranking file was found and parsed. `false` ⇒ the other fields
    /// fall back to the raw pool (no probe data).
    pub ranking_present: bool,
    /// Total accounts listed in the ranking (or the raw pool size when absent).
    pub total_accounts: usize,
    /// Healthy (`available`) accounts — the dispatchable set.
    pub healthy_accounts: usize,
    /// Unhealthy (exhausted / rate-limited / blocked) accounts.
    pub exhausted_accounts: usize,
    /// The health-adjusted token-axis concurrency limit: `healthy_accounts` when
    /// a ranking exists, else the raw token-pool size. This is what the work
    /// finder now feeds into the dynamic cap in place of the flat pool count.
    pub token_axis_limit: usize,
    /// Whether the token axis is the binding (minimum) constraint on the dynamic
    /// cap — i.e. tokens, not disk or the operator ceiling, are the bottleneck.
    pub token_bound: bool,
}

// ========================================================================
// Event Bus Types (Issue #3453 — Phase B of #3449)
// ========================================================================

/// A sweep-lifecycle event published on the in-memory bus.
///
/// The enum is tagged on the `type` field; the topic each variant maps
/// to is determined by [`Event::topic`]. Subscribers route by topic
/// prefix (see `event_bus::topic_matches`).
///
/// The taxonomy below is **frozen for v0.10.0** — new topics require a
/// follow-up issue per epic #3449.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum Event {
    /// `sweep.issue.{N}.phase` — sweep child advanced a phase.
    /// Payload published by the sweep skill via `PublishEvent`.
    SweepPhase {
        issue: u32,
        phase: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        pr_number: Option<i32>,
    },
    /// `sweep.issue.{N}.blocker` — sweep child encountered a blocker.
    SweepBlocker {
        issue: u32,
        reason: String,
        label_added: String,
    },
    /// `sweep.issue.{N}.exited` — reaper detected clean exit.
    SweepExited {
        issue: u32,
        #[serde(skip_serializing_if = "Option::is_none")]
        exit_code: Option<i32>,
        duration_sec: i64,
    },
    /// `sweep.issue.{N}.crashed` — reaper detected dead pid + checkpoint.
    SweepCrashed {
        issue: u32,
        #[serde(skip_serializing_if = "Option::is_none")]
        checkpoint_phase: Option<String>,
    },
    /// `sweep.global.dispatch` — daemon dispatched a new sweep.
    SweepGlobalDispatch { sweep_id: SweepId, kind: SweepKind },
    /// `sweep.global.completed` — daemon reaper recorded sweep completion.
    SweepGlobalCompleted {
        sweep_id: SweepId,
        outcome: SweepOutcome,
    },
    /// `epic.issue.{N}.{action}` — the epic supervisor (#3842) fired one of its
    /// four action-class transitions for epic `{N}`. Published by the epic
    /// supervisor loop; authorized by #3873 (epic #3842 Phase 4).
    EpicAction {
        epic: u32,
        action: EpicActionClass,
        /// The derived epic state the action fired from (e.g.
        /// `"epic:needs_decomp"`). Redundant with `action` but carried for
        /// observability so subscribers see the source state directly.
        state: String,
    },
    /// `daemon.capacity.advisory` — the autonomous work finder crossed (or
    /// cleared) a token-capacity pressure threshold. Published by the work-finder
    /// loop on **state change only** (entered/left the token-bound state), never
    /// every tick. Authorized by #3902 (epic #3809). Advisory only — it never
    /// blocks dispatch; it tells the operator when to add accounts / API credits.
    CapacityAdvisory {
        /// True when entering the pressured state; false on recovery.
        pressured: bool,
        /// Issues queued (deferred) behind the token-bound cap at the transition.
        queued: usize,
        /// Healthy (`available`) accounts at the transition.
        healthy_accounts: usize,
        /// Unhealthy (exhausted / rate-limited / blocked) accounts.
        exhausted_accounts: usize,
        /// Total accounts in the rotation ranking at the transition.
        total_accounts: usize,
        /// Estimated minutes to drain the backlog at current healthy capacity;
        /// omitted when no healthy account exists (cannot drain yet).
        #[serde(skip_serializing_if = "Option::is_none")]
        estimated_drain_minutes: Option<u64>,
        /// Operator-facing advisory message naming the concrete levers.
        message: String,
    },
    /// Synthetic event signalling that the subscription fell behind the
    /// publisher. The number of events dropped is reported in `skipped`.
    /// Matches `tokio::sync::broadcast::Receiver::Lagged` semantics.
    TopicLag { skipped: u64 },
    /// Generic event for forward compatibility — a topic + opaque payload.
    /// Used by the `PublishEvent` IPC variant when the publisher does not
    /// supply a strongly-typed event.
    Generic {
        topic: String,
        payload: serde_json::Value,
    },
}

impl Event {
    /// Resolve the topic string for this event.
    ///
    /// Per-variant rules:
    ///
    /// | Variant | Topic |
    /// |---------|-------|
    /// | `SweepPhase {issue, ..}` | `sweep.issue.{issue}.phase` |
    /// | `SweepBlocker {issue, ..}` | `sweep.issue.{issue}.blocker` |
    /// | `SweepExited {issue, ..}` | `sweep.issue.{issue}.exited` |
    /// | `SweepCrashed {issue, ..}` | `sweep.issue.{issue}.crashed` |
    /// | `SweepGlobalDispatch {..}` | `sweep.global.dispatch` |
    /// | `SweepGlobalCompleted {..}` | `sweep.global.completed` |
    /// | `EpicAction {epic, action, ..}` | `epic.issue.{epic}.{action}` |
    /// | `CapacityAdvisory {..}` | `daemon.capacity.advisory` |
    /// | `TopicLag {..}` | `sweep.system.topic_lag` |
    /// | `Generic {topic, ..}` | the explicit topic string |
    #[must_use]
    pub fn topic(&self) -> String {
        match self {
            Self::SweepPhase { issue, .. } => format!("sweep.issue.{issue}.phase"),
            Self::SweepBlocker { issue, .. } => format!("sweep.issue.{issue}.blocker"),
            Self::SweepExited { issue, .. } => format!("sweep.issue.{issue}.exited"),
            Self::SweepCrashed { issue, .. } => format!("sweep.issue.{issue}.crashed"),
            Self::SweepGlobalDispatch { .. } => "sweep.global.dispatch".to_string(),
            Self::SweepGlobalCompleted { .. } => "sweep.global.completed".to_string(),
            Self::EpicAction { epic, action, .. } => {
                format!("epic.issue.{epic}.{}", action.as_str())
            }
            Self::CapacityAdvisory { .. } => "daemon.capacity.advisory".to_string(),
            Self::TopicLag { .. } => "sweep.system.topic_lag".to_string(),
            Self::Generic { topic, .. } => topic.clone(),
        }
    }
}

/// The four action classes the epic supervisor emits on the event bus, one per
/// singleton lifecycle transition (epic #3842 Phase 4, #3873).
///
/// | Variant | Fires from | Supervisor transition |
/// |---------|-----------|-----------------------|
/// | [`Decompose`](Self::Decompose) | `epic:needs_decomp` | Architect enriches the epic body with `### Phase` structure |
/// | [`Expand`](Self::Expand)       | `epic:designed`     | Champion materializes the first phase's children |
/// | [`Join`](Self::Join)           | `epic:phase_join`   | Champion advances: materializes phase N+1's children (barrier-gated) |
/// | [`Close`](Self::Close)         | `epic:done`         | Champion closes the completed epic |
///
/// The `BuildChildren` transition (per-child `/loom:sweep` dispatch) is **not**
/// an action class here — those dispatches already surface on the frozen
/// `sweep.global.dispatch` topic.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum EpicActionClass {
    /// Decompose an undecomposed epic (`epic:needs_decomp` → `epic:designed`).
    Decompose,
    /// Expand the first phase's children (`epic:designed` → `epic:active`).
    Expand,
    /// Fork-join advance to the next phase (`epic:phase_join` → `epic:active`).
    Join,
    /// Close a completed epic (`epic:done`).
    Close,
}

impl EpicActionClass {
    /// The lower-case topic segment for this action, e.g. `"decompose"`. Used to
    /// build the `epic.issue.{N}.{action}` topic string.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Decompose => "decompose",
            Self::Expand => "expand",
            Self::Join => "join",
            Self::Close => "close",
        }
    }

    /// The derived epic state id this action fires from (e.g.
    /// `"epic:needs_decomp"` for [`Decompose`](Self::Decompose)).
    #[must_use]
    pub fn source_state_id(self) -> &'static str {
        match self {
            Self::Decompose => "epic:needs_decomp",
            Self::Expand => "epic:designed",
            Self::Join => "epic:phase_join",
            Self::Close => "epic:done",
        }
    }
}

/// Outcome of a completed sweep, used by `Event::SweepGlobalCompleted`.
///
/// `Exited` is the clean-exit path; `Crashed` is the dead-pid +
/// checkpoint-present path that triggers a `loom:building` →
/// `loom:issue` label re-arm on the reaper side.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SweepOutcome {
    Exited,
    Crashed,
}
