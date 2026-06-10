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
    DispatchSweep {
        kind: SweepKind,
        idempotency_key: Option<String>,
        #[serde(default)]
        model: Option<String>,
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
            Self::TopicLag { .. } => "sweep.system.topic_lag".to_string(),
            Self::Generic { topic, .. } => topic.clone(),
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
