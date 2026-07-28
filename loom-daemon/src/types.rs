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
        /// Target managed-workspace root (Issue #3929). When `Some(root)`, the
        /// daemon resolves the per-repo sweep registry for that root via the
        /// [`WorkspacePool`](crate::workspace_pool::WorkspacePool) — so a sweep
        /// can be dispatched into a managed repo other than the daemon's default
        /// workspace. When `None` (or absent on the wire — `#[serde(default)]`
        /// keeps existing clients byte-for-byte compatible), the default
        /// workspace registry is used, exactly as before.
        #[serde(default)]
        workspace_root: Option<String>,
    },
    /// List tracked sweeps, optionally filtered by state.
    ListSweeps {
        state_filter: Option<SweepState>,
        /// Target managed-workspace root (Issue #3929). `Some(root)` lists the
        /// sweeps tracked by that repo's registry; `None` (or absent) preserves
        /// the default-workspace-only behavior. Cross-repo aggregation in a
        /// single call is deferred to phase d (#3930).
        #[serde(default)]
        workspace_root: Option<String>,
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
        /// Target managed-workspace root (Issue #3929). `Some(root)` looks the
        /// sweep up in that repo's registry; `None` (or absent) uses the default
        /// workspace, exactly as before.
        #[serde(default)]
        workspace_root: Option<String>,
    },
    /// Read the last `lines` lines from a sweep's per-sweep log file.
    /// Resolved relative to the registry's workspace root. Used by the
    /// `tail_sweep_log` MCP tool.
    TailSweepLog {
        sweep_id: SweepId,
        lines: usize,
        /// Target managed-workspace root (Issue #3929). `Some(root)` resolves the
        /// log against that repo's registry; `None` (or absent) uses the default
        /// workspace, exactly as before.
        #[serde(default)]
        workspace_root: Option<String>,
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
        /// Target managed-workspace root (Issue #3929). `Some(root)` cancels a
        /// sweep tracked by that repo's registry; `None` (or absent) uses the
        /// default workspace, exactly as before.
        #[serde(default)]
        workspace_root: Option<String>,
    },
    /// Manually clear an insta-crash quarantine (Issue #3939) for `issue`,
    /// the operator-reachable release path (`loom-daemon quarantine clear
    /// <issue>`). Clears the daemon's in-memory quarantine + insta-crash tally
    /// so the work finder re-qualifies the issue immediately instead of waiting
    /// for the TTL, and restores `loom:issue` on the forge. Idempotent — clearing
    /// an issue that is not quarantined is a no-op success (`was_quarantined:
    /// false`).
    ClearQuarantine {
        issue: u32,
        /// Target managed-workspace root (Issue #3929). `Some(root)` clears a
        /// quarantine tracked by that repo's registry; `None` (or absent) uses
        /// the default workspace, exactly like `CancelSweep`.
        #[serde(default)]
        workspace_root: Option<String>,
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
    // ========================================================================
    // Workspace Registry Requests (Issue #3926 — phase 1 of #3835)
    // ========================================================================
    /// Register a repo as a managed workspace in the machine-level registry
    /// (`~/.loom/workspaces.json`). `root` is any path to the repo root; the
    /// daemon normalizes it (canonicalize/absolutize) before storing, so
    /// relative or symlinked paths dedup correctly. Idempotent — re-registering
    /// an already-present workspace is a no-op success.
    ///
    /// `config_overrides` is stored verbatim as opaque JSON (per-repo config
    /// overrides). It is only applied on a genuine insert; a re-register does
    /// not overwrite existing overrides.
    RegisterWorkspace {
        root: String,
        #[serde(default)]
        config_overrides: Option<serde_json::Value>,
    },
    /// Deregister a managed workspace by root. Normalized the same way as
    /// [`Request::RegisterWorkspace`]. Removing an absent workspace is a no-op
    /// success (`was_present: false`).
    DeregisterWorkspace {
        root: String,
    },
    /// List the managed workspaces in the machine-level registry.
    ListWorkspaces,
    // ========================================================================
    // Durable Watch Registry Requests (Issue #3971)
    // ========================================================================
    /// Register a durable watch on an issue's or PR's terminal state. The watch
    /// is persisted to `~/.loom/watches.json` (machine-level) so it survives the
    /// registering operator session's death AND a daemon restart; the daemon's
    /// watch-monitor loop polls the forge and, on a terminal state (closed /
    /// merged / blocked) or expiry, appends a result line to
    /// `~/.loom/logs/watch-results.log`.
    ///
    /// Address the target cross-repo with either `repo` (a forge slug
    /// `owner/name`, preferred — works for a repo this machine may not manage) or
    /// `workspace_root` (the `gh` query runs in that repo's working dir, mirroring
    /// the `workspace_root` param on `DispatchSweep` / `ListSweeps`). Both absent
    /// ⇒ the daemon's own cwd. Idempotent — re-registering the same
    /// `(target, kind, number)` returns the existing watch (`already_present:
    /// true`).
    RegisterWatch {
        kind: crate::watch_registry::WatchKind,
        number: u32,
        #[serde(default)]
        repo: Option<String>,
        #[serde(default)]
        workspace_root: Option<String>,
        #[serde(default)]
        note: Option<String>,
    },
    /// List the currently-registered durable watches.
    ListWatches,
    /// Remove a registered watch by its id (as returned by `RegisterWatch` /
    /// `ListWatches`). Removing an unknown id is a no-op success
    /// (`was_present: false`).
    RemoveWatch {
        id: String,
    },
    // ========================================================================
    // Supervised restart primitive (Issue #4054 — Phase 2 of #4017)
    // ========================================================================
    /// Deliberately restart the daemon by ending the current process so the
    /// supervisor (macOS launchd) relaunches it — the manually-triggerable
    /// restart primitive #4017 Phase 3 will call to complete an auto-roll.
    ///
    /// This is the ONLY path that exits the process with status `0`. Under the
    /// launchd `KeepAlive: { SuccessfulExit: true }` contract (see
    /// `render_launchd_plist` in `loom-daemon-start.sh`), a clean exit-0
    /// triggers a relaunch, while every operator/signal/crash exit is non-zero
    /// and does NOT relaunch. The relaunched process re-reads the same plist, so
    /// it comes back with EXACTLY the flags/env it was started with — never
    /// wider.
    ///
    /// The daemon only ends the process when it can prove it is supervised
    /// (`LOOM_DAEMON_SUPERVISOR=launchd`, baked into the plist by the start
    /// script). On an unsupervised host (nohup / Linux / `--foreground`) it
    /// refuses: it logs loudly, leaves itself running, and returns a
    /// `DaemonRestart { scheduled: false, .. }` — there is no supervisor that
    /// would bring it back, and exiting would be strictly worse than the status
    /// quo (#4017).
    ///
    /// It never fires on its own — nothing in the daemon issues this request.
    RestartDaemon,
    /// Scheduled drain-and-restart (Issue #4090): stop admitting *new* work
    /// immediately, wait for every in-flight sweep to finish, then exit
    /// [`EXIT_RESTART`](crate::ipc::EXIT_RESTART) for a supervised relaunch —
    /// no sweep killed, no orphan left behind.
    ///
    /// A deliberately **separate** variant from [`Request::RestartDaemon`] (a
    /// unit variant) rather than fields on it: the wire window where an older
    /// `loom-daemon restart` client and a newer daemon disagree is *precisely a
    /// roll*, so `{"type":"RestartDaemon"}` must keep deserializing unchanged.
    ///
    /// - `timeout_secs`: bound on how long to wait for the registry to empty.
    ///   `None` ⇒ the daemon's default (tens of minutes; a sweep is ~10–20 min).
    /// - `force_after_timeout`: when the deadline passes with sweeps still in
    ///   flight, `true` cancels the stragglers via the existing `cancel_sweep`
    ///   path and restarts anyway; `false` (fail-safe) refuses the restart,
    ///   resumes dispatch, and stays up.
    ///
    /// On an unsupervised host the daemon refuses immediately (before pausing
    /// dispatch) with `DaemonDrain { accepted: false, .. }`, mirroring
    /// [`Request::RestartDaemon`]'s refusal contract.
    DrainAndRestartDaemon {
        timeout_secs: Option<u64>,
        force_after_timeout: bool,
    },
    /// Abort an in-progress drain (Issue #4090): clear the drain flag so new
    /// dispatch resumes, and stop the drain-supervisor task so no later restart
    /// fires — even if the in-flight count subsequently reaches zero on its own.
    /// A no-op (idempotent) when no drain is in progress.
    AbortDrain,
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
    /// Result of a `ClearQuarantine` request (Issue #3939). `was_quarantined`
    /// is `false` when the issue was not quarantined at the moment of the call
    /// (idempotent no-op success — the in-memory state was already clear).
    QuarantineCleared {
        issue: u32,
        was_quarantined: bool,
    },
    // ========================================================================
    // Autonomous Daemon Status (Issue #3891 — follow-up to #3813 Phase D)
    // ========================================================================
    /// Result of a `DaemonStatus` request — the autonomous-mode operability
    /// snapshot rendered by `loom-daemon status`.
    DaemonStatus(DaemonStatusReport),
    /// Result of a `RestartDaemon` request (Issue #4054).
    ///
    /// `scheduled` is `true` when the daemon is supervised and is about to exit
    /// `0` for a supervised relaunch (the process ends immediately after this
    /// frame is flushed). It is `false` when the daemon is NOT supervised and
    /// therefore refused to exit — the daemon stays running. `supervisor` names
    /// the detected supervisor (`Some("launchd")`) or `None` when unsupervised,
    /// and `message` is a human-readable explanation for operator output.
    DaemonRestart {
        scheduled: bool,
        supervisor: Option<String>,
        message: String,
    },
    /// Result of a `DrainAndRestartDaemon` / `AbortDrain` request (Issue #4090).
    ///
    /// `accepted` is `true` when the drain was scheduled (or the abort took
    /// effect); `false` on an unsupervised host (a drain would have nowhere to
    /// relaunch into) or an abort with no drain in progress. `supervisor` names
    /// the detected supervisor (`Some("launchd")`) or `None` when unsupervised.
    /// `in_flight` is the cross-root non-terminal sweep count at request time,
    /// so the operator immediately sees how much work the drain must wait for.
    /// `message` is a human-readable explanation for operator output.
    DaemonDrain {
        accepted: bool,
        supervisor: Option<String>,
        in_flight: usize,
        message: String,
    },
    // ========================================================================
    // Workspace Registry Responses (Issue #3926 — phase 1 of #3835)
    // ========================================================================
    /// Result of a `RegisterWorkspace` request. `root` is the normalized root
    /// actually stored; `already_present` is `true` when the workspace was
    /// already registered (no-op).
    WorkspaceRegistered {
        root: PathBuf,
        already_present: bool,
        /// Whether the directory looks like a Loom-managed repo (has `.git`
        /// and/or `.loom`) — a soft advisory, not a rejection.
        looks_like_workspace: bool,
    },
    /// Result of a `DeregisterWorkspace` request. `was_present` is `false` when
    /// no matching workspace was registered (no-op success).
    WorkspaceDeregistered {
        root: PathBuf,
        was_present: bool,
    },
    /// Result of a `ListWorkspaces` request.
    WorkspaceList {
        workspaces: Vec<crate::workspace_registry::Workspace>,
    },
    // ========================================================================
    // Durable Watch Registry Responses (Issue #3971)
    // ========================================================================
    /// Result of a `RegisterWatch` request. `watch` is the stored spec (the
    /// pre-existing one on a dedup hit); `already_present` is `true` when a watch
    /// for the same `(target, kind, number)` was already registered.
    WatchRegistered {
        watch: crate::watch_registry::WatchSpec,
        already_present: bool,
    },
    /// Result of a `ListWatches` request.
    WatchList {
        watches: Vec<crate::watch_registry::WatchSpec>,
    },
    /// Result of a `RemoveWatch` request. `was_present` is `false` when no watch
    /// with the given id existed (no-op success).
    WatchRemoved {
        id: String,
        was_present: bool,
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
    /// The managed-workspace root that owns this sweep (Issue #3929). Populated
    /// from the owning registry's `config.workspace_root` so a `list_sweeps` /
    /// `get_sweep_status` response is self-describing: two managed repos can each
    /// have an issue #42, and this field disambiguates repo A's sweep from repo
    /// B's. `None` on internally-constructed entries that have not yet been
    /// stamped; `#[serde(default)]` keeps pre-#3929 wire data and clients
    /// compatible.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
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
    /// Dynamic-cap input 3 (#3978): how many additional concurrent sweeps the
    /// host's CPU/load headroom can currently absorb. Via
    /// [`crate::cpu_headroom::cpu_headroom`]. Added because the token/disk axes
    /// alone let a batch of resetting token accounts raise the cap regardless
    /// of how many concurrent `cargo build`s were already saturating the host
    /// — which starved the main-health gate's own build of CPU badly enough to
    /// false-time-out (the #3978 incident). `#[serde(default)]` keeps pre-#3978
    /// wire data / older clients compatible (an absent field parses as `0`,
    /// which the status renderer never treats as a real reading — see
    /// [`Self::logical_cpus`]).
    #[serde(default)]
    pub cpu_headroom: usize,
    /// The host's logical CPU count feeding [`Self::cpu_headroom`] (#3978), via
    /// [`crate::cpu_headroom::logical_cpu_count`]. Surfaced so the status view
    /// can render "X healthy cores" context next to the headroom number.
    /// `#[serde(default)]` keeps pre-#3978 wire data compatible.
    #[serde(default)]
    pub logical_cpus: usize,
    /// The current 1-minute load average feeding [`Self::cpu_headroom`]
    /// (#3978), via [`crate::cpu_headroom::read_loadavg_1m`]. `None` on a
    /// platform/host where no load-average source is available (the CPU term
    /// then falls back to its static, load-agnostic capacity — see the
    /// [`crate::cpu_headroom`] module docs). `#[serde(default)]` keeps
    /// pre-#3978 wire data compatible.
    #[serde(default)]
    pub loadavg_1m: Option<f64>,
    /// The measured CPU idle fraction (`0.0..=1.0`) feeding [`Self::cpu_headroom`]
    /// (#4031), via [`crate::cpu_headroom::cached_cpu_idle_fraction`]. This is the
    /// signal that replaced the 1-minute load average as the source of "consumed
    /// cores" — load average overstated consumption by ~1.5× on macOS because it
    /// counts network-I/O-blocked `claude` sessions that consume no core. `None`
    /// when no idle sample has been taken yet (the term then falls back to
    /// [`Self::loadavg_1m`], then to static capacity — see the
    /// [`crate::cpu_headroom`] module docs). `#[serde(default)]` keeps pre-#4031
    /// wire data / older clients compatible.
    #[serde(default)]
    pub cpu_idle_fraction: Option<f64>,
    /// Whether in-flight occupancy has actually reached the dynamic cap
    /// (`in_flight.len() >= dynamic_cap`) — i.e. the cap is *currently binding*,
    /// not merely the smallest ceiling (#4031). When `false`, no resource term
    /// (tokens/disk/CPU) is the limiter — the limiter is **work availability**,
    /// and the status renderer suppresses the "token-bound" diagnosis rather than
    /// misreporting a bottleneck at, say, 1 in-flight against a cap of 7.
    /// `#[serde(default)]` keeps pre-#4031 wire data / older clients compatible
    /// (an absent field parses as `false` — "not capacity-bound").
    #[serde(default)]
    pub capacity_bound: bool,
    /// Dynamic-cap input 4: the configured operator ceiling
    /// (`autonomous.workFinder.maxConcurrent` / `LOOM_WORK_FINDER_MAX_CONCURRENT`).
    pub configured_max: usize,
    /// The per-token concurrency factor (#3947): how many concurrent sweeps the
    /// dynamic cap allows per *healthy* token. The token axis of the cap is
    /// `healthy_tokens × per_token_concurrency`, not the old implicit
    /// `× 1`. Resolved with precedence env (`LOOM_PER_TOKEN_CONCURRENCY`) >
    /// config (`autonomous.perTokenConcurrency`) > default (2).
    /// `#[serde(default)]` keeps pre-#3947 wire data / older clients compatible
    /// (an absent field parses as `0`; the status renderer treats `< 1` as `1`).
    #[serde(default)]
    pub per_token_concurrency: usize,
    /// The effective dynamic concurrency cap —
    /// `min(token_axis × per_token_concurrency, disk_headroom, cpu_headroom,
    /// configured_max)` (`resolve_dynamic_max_concurrent`, CPU term #3978).
    /// This is the total-occupancy ceiling the work finder recomputes every
    /// tick.
    pub dynamic_cap: usize,
    /// Whether autonomous dispatch is currently halted by the reactive
    /// main-health gate (#3812). `true` means a red `main` has paused new
    /// dispatch (in-flight sweeps keep running); `false` means dispatch is
    /// allowed — which covers three distinct conditions the gate loop cannot
    /// tell apart from this flag alone: the gate is disabled, the gate is
    /// enabled but has not completed a first evaluation yet ("pending"), or
    /// the gate's last completed run verified `main` green ("clear"). See
    /// [`Self::main_health_gate_enabled`] and [`Self::main_health_gate_verdict_at`]
    /// (#4012) for the fields that disambiguate those three.
    pub main_health_gate_halted: bool,
    /// Whether the gate's most recent tick for this workspace was
    /// `Unevaluated` rather than a completed Green/Red run — "not evaluated",
    /// distinguished from `main_health_gate_halted`'s "halted (verified-red
    /// main)" (#3950 AC3). The two are independent: an unevaluated tick leaves
    /// any prior halt flag exactly as it was, so both can be `true` at once
    /// (main was verified red before the environment broke). Always `false`
    /// when the gate loop is not enabled or has never run. `#[serde(default)]`
    /// keeps pre-#3950 wire data / older clients compatible.
    #[serde(default)]
    pub main_health_gate_not_evaluated: bool,
    /// A short `"<class>: <reason>"` summary of *why* the most recent tick was
    /// unevaluated (#3974 AC2) — e.g. `"timeout: gate command … timed out after
    /// 600s"` or `"git-failure: `git fetch origin main` failed …"`. `None` when
    /// the last tick completed. Pre-#3974 the status line hard-coded
    /// "workspace tree is dirty" for *every* skip, which misreported timeouts,
    /// missing tools, and broken-process-tree `git` failures as a dirty tree.
    /// `#[serde(default)]` keeps pre-#3974 wire data compatible.
    #[serde(default)]
    pub main_health_gate_not_evaluated_reason: Option<String>,
    /// Whether the reactive main-health gate is actually enabled for this
    /// workspace root (#4012) — `resolve_enabled(..)` **and** a usable
    /// `buildGate` block, so a root that is nominally `enabled: true` but has
    /// no command configured (the gate loop treats that as always-green,
    /// `main_health_gate.rs`) also reports `Some(false)` here. `Some(true)` /
    /// `Some(false)` are resolved daemon-side (reading the daemon's own
    /// environment and `.loom/config.json`, never the CLI client's); `None`
    /// only for a pre-#4012 wire payload that never reported this field.
    /// Deliberately `Option<bool>` rather than `bool`: a legacy payload
    /// deserializing a missing `bool` field defaults to `false`, which would
    /// misreport an older, perfectly healthy daemon as "gate disabled" —
    /// `None` honestly means "unknown, older daemon" instead. `#[serde(default)]`
    /// keeps pre-#4012 wire data compatible.
    #[serde(default)]
    pub main_health_gate_enabled: Option<bool>,
    /// Wall-clock time of the most recent **completed** (Green/Red) gate
    /// verdict for this workspace root (#4012), or `None` when no verdict has
    /// landed yet this daemon process — the disambiguator between "pending"
    /// (enabled, no verdict yet) and "clear" (verified green), and the
    /// recency evidence a `clear` reading otherwise lacks. Stamped only on a
    /// completed run, never on the #3984 SHA-memo skip path (a skip proves
    /// nothing new). `#[serde(default)]` keeps pre-#4012 wire data compatible
    /// (an absent field parses as `None`, which reads as "pending" — the
    /// conservative choice for data an older daemon never populated).
    #[serde(default)]
    pub main_health_gate_verdict_at: Option<DateTime<Utc>>,
    /// Token-capacity backpressure snapshot (#3902): account health derived from
    /// the rotation ranking (`.loom/tokens/.ranking`) and whether the token axis
    /// is the binding constraint on the dynamic cap. `#[serde(default)]` keeps
    /// pre-#3902 wire data / older clients compatible.
    #[serde(default)]
    pub capacity: CapacityReport,
    /// Per-repo breakdown across every registered managed workspace (Issue #3930
    /// — phase d of #3835/#3926). One entry per [`crate::workspace_registry::WorkspaceRegistry::effective_roots`]
    /// root: its in-flight sweep count and per-repo main-health-gate halt state.
    /// The top-level [`Self::in_flight`] is the **union** across these repos, so a
    /// sweep the autonomous loops dispatched into a non-default repo is now
    /// visible in `loom-daemon status`. In the common single-workspace case this
    /// is a single entry for the daemon's own workspace. `#[serde(default)]` keeps
    /// pre-#3930 wire data / older clients compatible (an empty vec).
    #[serde(default)]
    pub per_repo: Vec<RepoStatus>,
    /// Startup forge-credential preflight snapshot (#4005): resolved once at
    /// daemon boot, before the claim-reconciliation startup pass (the
    /// daemon's first `gh` consumer) — see
    /// [`crate::credential_preflight::run`]. `None` only for a pre-#4005 wire
    /// payload from an older daemon binary that never computed one.
    /// `#[serde(default)]` keeps that wire data compatible.
    #[serde(default)]
    pub credential_preflight: Option<CredentialPreflightReport>,
    /// Whether a scheduled drain-and-restart (Issue #4090) is currently in
    /// progress: new dispatch is paused and the daemon is waiting for the
    /// in-flight sweep count ([`Self::in_flight`]) to reach zero before exiting
    /// for a supervised relaunch. `false` in the common no-drain case.
    /// `#[serde(default)]` keeps pre-#4090 wire data / older clients compatible
    /// (an absent field parses as `false` — "not draining"), mirroring the
    /// `capacity_bound` forward-compat convention.
    #[serde(default)]
    pub draining: bool,
    /// Wall-clock deadline at which an in-progress drain (Issue #4090) gives up
    /// waiting: without `--force-after-timeout` it refuses the restart and
    /// resumes dispatch; with it, the stragglers are cancelled and the daemon
    /// restarts anyway. `None` when no drain is active. `#[serde(default)]`
    /// keeps pre-#4090 wire data compatible.
    #[serde(default)]
    pub drain_deadline: Option<DateTime<Utc>>,
    /// A short human-readable note about the most recent drain transition
    /// (Issue #4090) — e.g. why a drain timed out and was refused, or that a
    /// drain was aborted by the operator. Surfaced so `loom-daemon status`
    /// never leaves a drain that quietly ended unexplained. `None` when no
    /// drain has run this process. `#[serde(default)]` keeps pre-#4090 wire
    /// data compatible.
    #[serde(default)]
    pub drain_note: Option<String>,
}

/// One registered managed-workspace's status line in [`DaemonStatusReport`]
/// (Issue #3930). The daemon enumerates every
/// [`crate::workspace_registry::WorkspaceRegistry::effective_roots`] root for the
/// per-repo breakdown so sweeps dispatched into any managed repo — not just the
/// daemon's own default workspace — are observable in `loom-daemon status`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RepoStatus {
    /// The normalized workspace root this line describes.
    pub root: PathBuf,
    /// This repo's cross-repo dispatch priority tier (Issue #3946): lower = higher
    /// priority, default [`crate::workspace_registry::DEFAULT_WORKSPACE_PRIORITY`].
    /// Surfaced so `loom-daemon status` shows which repos the autonomous loops
    /// drain first. `#[serde(default)]` keeps pre-#3946 wire data compatible (an
    /// absent field parses as the default tier).
    #[serde(default = "crate::workspace_registry::default_priority")]
    pub priority: u32,
    /// Count of non-terminal (`Pending` / `Running`) sweeps live in this repo's
    /// own [`crate::sweep_registry::SweepRegistry`] at snapshot time.
    pub in_flight_count: usize,
    /// Whether this repo's dispatch is currently halted by the per-repo reactive
    /// main-health gate (#3930). A red `main` in this repo halts only this repo's
    /// dispatch, never the siblings'. Always `false` for a repo whose gate is
    /// disabled / has no `buildGate` block, or when the gate loop is off.
    pub health_gate_halted: bool,
    /// Issue numbers currently quarantined for repeated insta-crashing in this
    /// repo (Issue #3939), sorted ascending. The work finder skips these until
    /// their TTL elapses (or an operator clears them), so this surfaces *why* a
    /// repo with a visible backlog is dispatching nothing. Empty in the common
    /// case. `#[serde(default)]` keeps pre-#3939 wire data / older clients
    /// compatible (an absent field parses as an empty vec).
    #[serde(default)]
    pub quarantined_issues: Vec<u32>,
    /// Whether this repo's most recent gate tick was `Unevaluated` — "not
    /// evaluated", distinguished from `health_gate_halted`'s "halted
    /// (verified-red main)" (#3950 AC3). See the field doc on
    /// [`DaemonStatusReport::main_health_gate_not_evaluated`] for the
    /// independence of the two flags. `#[serde(default)]` keeps pre-#3950
    /// wire data compatible.
    #[serde(default)]
    pub health_gate_not_evaluated: bool,
    /// A short `"<class>: <reason>"` summary of *why* this repo's most recent
    /// tick was unevaluated (#3974 AC2), or `None` when it completed. See
    /// [`DaemonStatusReport::main_health_gate_not_evaluated_reason`].
    #[serde(default)]
    pub health_gate_not_evaluated_reason: Option<String>,
    /// Whether this repo's gate is actually enabled (#4012). See
    /// [`DaemonStatusReport::main_health_gate_enabled`] for the `Option<bool>`
    /// rationale and the "enabled but no usable `buildGate` block ⇒ `false`"
    /// rule. `#[serde(default)]` keeps pre-#4012 wire data compatible.
    #[serde(default)]
    pub health_gate_enabled: Option<bool>,
    /// Wall-clock time of this repo's most recent completed gate verdict
    /// (#4012), or `None` before the first one this process. See
    /// [`DaemonStatusReport::main_health_gate_verdict_at`]. `#[serde(default)]`
    /// keeps pre-#4012 wire data compatible.
    #[serde(default)]
    pub health_gate_verdict_at: Option<DateTime<Utc>>,
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

/// Startup forge-credential preflight snapshot (#4005) — see
/// [`crate::credential_preflight`] for the resolution logic. Resolved exactly
/// once, before the daemon's first `gh` consumer, so headless/SSH-only
/// operation (no unlockable GUI login keychain) is diagnosed loudly at boot
/// instead of surfacing as an unexplained per-tick `401` on every forge call.
///
/// GitHub-only: the daemon's own forge calls all shell out to `gh`, which
/// resolves GitHub credentials exclusively. `GITEA_TOKEN`/`FORGE_TOKEN`
/// forwarding (`loom-daemon-start.sh`) exists only for dispatched sweep
/// children targeting a Gitea-backed repo — the daemon process itself never
/// calls a Gitea API, so there is nothing here to preflight for it.
///
/// **Never carries a token value** — only a resolution-mechanism label and a
/// non-secret fingerprint (last 4 chars of an env-sourced token, or the
/// authenticated `gh` login for a credential-store resolution).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CredentialPreflightReport {
    /// `true` when a usable GitHub credential was resolved; `false` when
    /// none was found, or the probe itself could not complete within its
    /// bound (`gh` missing from `PATH`, timed out, spawn failure, …) — see
    /// [`Self::mechanism`] `"unknown"` for that latter case.
    pub ok: bool,
    /// Which mechanism resolved the credential: `"GH_TOKEN"` / `"GITHUB_TOKEN"`
    /// (the daemon's own process environment) or `gh`'s own `tokenSource`
    /// (e.g. `"keyring"`, `"oauth_token"`) reported by `gh auth status`.
    /// `"none"` when nothing resolved; `"unknown"` when the probe itself
    /// failed to run. NEVER a token value.
    pub mechanism: String,
    /// A non-secret fingerprint: the last 4 characters of an env-sourced
    /// token, or the authenticated `gh` login for a credential-store
    /// resolution. `None` when no credential resolved.
    pub fingerprint: Option<String>,
    /// Human-readable, log/print-safe summary — never contains a token.
    /// Names both remediations (export `GH_TOKEN` before starting the
    /// daemon, or unlock the login keychain from a GUI session) when `ok` is
    /// `false`.
    pub message: String,
    /// Wall-clock time this snapshot was taken (daemon startup).
    pub checked_at: DateTime<Utc>,
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
        /// Owning managed-workspace root (Issue #3929). Stamped by the emitting
        /// registry from `config.workspace_root` so a subscriber that matches
        /// the shared `sweep.issue.{N}.phase` topic can disambiguate two managed
        /// repos' issue #N. Additive/backward-compatible — the topic string is
        /// unchanged; the field lives in the payload only. `#[serde(default)]`
        /// keeps pre-#3929 subscribers/wire data compatible.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        repo: Option<String>,
    },
    /// `sweep.issue.{N}.blocker` — sweep child encountered a blocker.
    SweepBlocker {
        issue: u32,
        reason: String,
        label_added: String,
        /// Owning managed-workspace root (Issue #3929). See [`Self::SweepPhase`].
        #[serde(default, skip_serializing_if = "Option::is_none")]
        repo: Option<String>,
    },
    /// `sweep.issue.{N}.exited` — reaper detected clean exit.
    SweepExited {
        issue: u32,
        #[serde(skip_serializing_if = "Option::is_none")]
        exit_code: Option<i32>,
        duration_sec: i64,
        /// Owning managed-workspace root (Issue #3929). See [`Self::SweepPhase`].
        #[serde(default, skip_serializing_if = "Option::is_none")]
        repo: Option<String>,
    },
    /// `sweep.issue.{N}.crashed` — reaper detected dead pid + checkpoint.
    SweepCrashed {
        issue: u32,
        #[serde(skip_serializing_if = "Option::is_none")]
        checkpoint_phase: Option<String>,
        /// Owning managed-workspace root (Issue #3929). See [`Self::SweepPhase`].
        #[serde(default, skip_serializing_if = "Option::is_none")]
        repo: Option<String>,
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
    ///
    /// The four `SweepPhase` / `SweepBlocker` / `SweepExited` / `SweepCrashed`
    /// variants also carry a `repo` payload field (Issue #3929) so a subscriber
    /// on the shared `sweep.issue.{N}.*` bus can disambiguate two managed repos'
    /// issue #N. The topic **strings are unchanged** — `repo` lives in the
    /// payload only, so existing single-repo subscribers route identically.
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

    /// Stamp the owning managed-workspace `repo` into the sweep-scoped event
    /// variants (Issue #3929), but only when the field is still absent — a
    /// caller that already knows the repo (e.g. a `PublishEvent` payload from a
    /// sweep child running in a specific workspace) is never overwritten.
    ///
    /// Non-sweep-scoped variants (`SweepGlobalDispatch` / `SweepGlobalCompleted`
    /// already carry a unique `sweep_id`; `EpicAction`; `CapacityAdvisory`;
    /// `TopicLag`; `Generic`) are left untouched. Called centrally from
    /// `SweepRegistry::emit_event` so every emitted sweep event is stamped with
    /// its registry's `workspace_root` without touching each construction site.
    pub fn set_repo_if_absent(&mut self, repo: &str) {
        let slot = match self {
            Self::SweepPhase { repo, .. }
            | Self::SweepBlocker { repo, .. }
            | Self::SweepExited { repo, .. }
            | Self::SweepCrashed { repo, .. } => repo,
            _ => return,
        };
        if slot.is_none() {
            *slot = Some(repo.to_string());
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

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    // ---- Issue #3929: event `repo` payload field is additive; topic unchanged ----

    #[test]
    fn sweep_scoped_event_topics_are_unchanged_by_repo_field() {
        // The topic string format is frozen; adding `repo` to the payload must
        // not shift any topic segment.
        let phase = Event::SweepPhase {
            issue: 42,
            phase: "builder".to_string(),
            pr_number: None,
            repo: Some("/repos/a".to_string()),
        };
        assert_eq!(phase.topic(), "sweep.issue.42.phase");

        let blocker = Event::SweepBlocker {
            issue: 42,
            reason: "x".to_string(),
            label_added: "loom:blocked".to_string(),
            repo: Some("/repos/b".to_string()),
        };
        assert_eq!(blocker.topic(), "sweep.issue.42.blocker");

        let exited = Event::SweepExited {
            issue: 7,
            exit_code: Some(0),
            duration_sec: 5,
            repo: None,
        };
        assert_eq!(exited.topic(), "sweep.issue.7.exited");

        let crashed = Event::SweepCrashed {
            issue: 7,
            checkpoint_phase: Some("doctor".to_string()),
            repo: Some("/repos/c".to_string()),
        };
        assert_eq!(crashed.topic(), "sweep.issue.7.crashed");
    }

    #[test]
    fn event_repo_round_trips_through_serde() {
        let ev = Event::SweepPhase {
            issue: 99,
            phase: "judge".to_string(),
            pr_number: Some(1234),
            repo: Some("/repos/alpha".to_string()),
        };
        let json = serde_json::to_string(&ev).unwrap();
        assert!(json.contains("\"repo\":\"/repos/alpha\""));
        let back: Event = serde_json::from_str(&json).unwrap();
        match back {
            Event::SweepPhase { issue, repo, .. } => {
                assert_eq!(issue, 99);
                assert_eq!(repo.as_deref(), Some("/repos/alpha"));
            }
            other => panic!("expected SweepPhase, got {other:?}"),
        }
    }

    #[test]
    fn event_repo_defaults_to_none_for_pre_3929_wire_data() {
        // A payload emitted before #3929 has no `repo` key; it must still parse,
        // with `repo` defaulting to None (backward-compatible subscribers).
        let json = r#"{"type":"SweepPhase","issue":5,"phase":"builder"}"#;
        let ev: Event = serde_json::from_str(json).unwrap();
        match ev {
            Event::SweepPhase {
                issue,
                repo,
                pr_number,
                ..
            } => {
                assert_eq!(issue, 5);
                assert!(repo.is_none());
                assert!(pr_number.is_none());
            }
            other => panic!("expected SweepPhase, got {other:?}"),
        }
    }

    #[test]
    fn set_repo_if_absent_stamps_only_when_empty_and_only_sweep_scoped() {
        // Stamps when absent.
        let mut ev = Event::SweepExited {
            issue: 1,
            exit_code: None,
            duration_sec: 0,
            repo: None,
        };
        ev.set_repo_if_absent("/repos/x");
        match &ev {
            Event::SweepExited { repo, .. } => assert_eq!(repo.as_deref(), Some("/repos/x")),
            other => panic!("unexpected {other:?}"),
        }
        // Does not overwrite an already-known repo.
        ev.set_repo_if_absent("/repos/y");
        match &ev {
            Event::SweepExited { repo, .. } => assert_eq!(repo.as_deref(), Some("/repos/x")),
            other => panic!("unexpected {other:?}"),
        }
        // Leaves non-sweep-scoped variants untouched (no panic, no field).
        let mut global = Event::SweepGlobalCompleted {
            sweep_id: "s1".to_string(),
            outcome: SweepOutcome::Exited,
        };
        global.set_repo_if_absent("/repos/z"); // no-op, must not panic
        assert_eq!(global.topic(), "sweep.global.completed");
    }

    // ---- Issue #3929: SweepInfo `repo` field is additive/backward-compatible ----

    #[test]
    fn sweep_info_repo_round_trips_and_defaults_to_none() {
        let json = r#"{
            "sweep_id":"s1",
            "kind":{"type":"Issue","value":42},
            "pid":1234,
            "token_name":"agent-1.token",
            "log_path":".loom/logs/sweep-issue-42.log",
            "started_at":"2026-07-24T00:00:00Z",
            "state":{"state":"Running"}
        }"#;
        // Pre-#3929 wire data (no `repo`) parses with repo == None.
        let info: SweepInfo = serde_json::from_str(json).unwrap();
        assert!(info.repo.is_none());

        // Round-trip with repo populated.
        let mut info = info;
        info.repo = Some("/repos/beta".to_string());
        let round = serde_json::to_string(&info).unwrap();
        assert!(round.contains("\"repo\":\"/repos/beta\""));
        let back: SweepInfo = serde_json::from_str(&round).unwrap();
        assert_eq!(back.repo.as_deref(), Some("/repos/beta"));
    }
}
