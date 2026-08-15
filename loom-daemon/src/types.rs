use crate::activity::{ActivityEntry, ClaimResult, ClaimType, ClaimsSummary, IssueClaim};
use crate::errors::DaemonError;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

pub use crate::runtime_admission::{RuntimeRejection, RuntimeSource};

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
        /// Operator override for the host-distress circuit breaker (Issue
        /// #4235). A *tripped* breaker represents **sustained**, already-observed
        /// host distress — a stronger signal than the point-in-time headroom
        /// advisory — so it **hard-blocks** an explicit `dispatch_sweep` by
        /// default (distinct from the deliberately-advisory headroom check, which
        /// never blocks). `force: true` is the operator saying "I know the host
        /// is distressed, dispatch anyway" and bypasses that block. When `false`
        /// (or absent on the wire — `#[serde(default)]` keeps existing clients
        /// byte-for-byte compatible) a tripped breaker refuses the dispatch.
        #[serde(default)]
        force: bool,
    },
    /// List tracked sweeps, optionally filtered by state.
    ListSweeps {
        state_filter: Option<SweepState>,
        /// Target managed-workspace root (Issue #3929). `Some(root)` lists the
        /// sweeps tracked by that repo's registry; `None` (or absent) preserves
        /// the default-workspace-only behavior.
        #[serde(default)]
        workspace_root: Option<String>,
        /// Fan out across every registered managed workspace (Issue #6006, the
        /// deferred follow-up to #3930). When `true`, `workspace_root` is
        /// ignored and the response aggregates sweeps from every root in
        /// [`crate::workspace_registry::WorkspaceRegistry::effective_roots`]
        /// (mirroring the fan-out `Request::ListQuarantines` already performs
        /// for its own `None` case) — an empty registry still yields exactly
        /// the default workspace, so a single-workspace daemon's fleet-wide
        /// query returns the same set as `workspace_root: None`. Each returned
        /// `SweepInfo` already carries a `repo` field naming its owner, so
        /// callers do not need to know the individual repo roots in advance.
        /// `false` (or absent on the wire — `#[serde(default)]` keeps existing
        /// clients byte-for-byte compatible) preserves the existing
        /// `workspace_root`-scoped (or default-workspace-only) behavior
        /// unchanged — this field is purely additive, never a breaking
        /// reinterpretation of `None`/absent `workspace_root` the way
        /// `ListQuarantines` uses it.
        #[serde(default)]
        all_workspaces: bool,
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
    /// List active insta-crash quarantines (Issue #4215), the read-side
    /// counterpart to [`Request::ClearQuarantine`] — the operator-reachable
    /// authority for "which issues are quarantined right now", distinct from a
    /// forge `loom:blocked` query (which conflates quarantines with genuine
    /// dependency blocks, since `apply_quarantine_label` reuses that label).
    ///
    /// **`workspace_root` semantics deliberately differ from
    /// [`Request::ClearQuarantine`]**: there, `None` means "the daemon's
    /// default workspace" (you're targeting one specific repo you already
    /// know). Here, `None` means "every registered workspace" — mirroring how
    /// `DaemonStatus` enumerates `WorkspaceRegistry::load_default().
    /// effective_roots(..)` — because "which issues are quarantined anywhere?"
    /// is the operator's actual question during a quarantine wave.
    /// `Some(root)` still scopes to that one repo's registry.
    ListQuarantines {
        #[serde(default)]
        workspace_root: Option<String>,
    },
    /// Manually record a **failed dispatch attempt** for `issue` against the
    /// per-issue dispatch-backoff machinery (Issue #4485), arming (or
    /// extending) its backoff window so the next automatic re-dispatch is
    /// deferred instead of immediate — the operator/script-reachable
    /// counterpart to the reaper's own automatic `record_dispatch_failure`
    /// calls (Issue #6192).
    ///
    /// Added so a sweep-side toolchain-invocation timeout with no access to
    /// the daemon's in-memory `SweepRegistry` (e.g. `build-gate.sh`'s bounded
    /// per-step timeout, running inside a builder's worktree) has a surface
    /// to arm the SAME backoff the daemon's own crash/no-progress detection
    /// uses — `loom-daemon dispatch-backoff record --issue <N>` — instead of
    /// a hung-then-killed build letting the next dispatch retry immediately
    /// against a still-wedged host. A no-op (idempotent success) when the
    /// backoff mechanism itself is disabled (`autonomous.workFinder.
    /// dispatchBackoff.enabled: false` / `LOOM_DISPATCH_BACKOFF`), mirroring
    /// [`Request::ClearQuarantine`]'s "state is in-memory, connect to the
    /// running daemon" contract.
    RecordDispatchFailure {
        issue: u32,
        /// Free-form context for the daemon log line (e.g. "build-gate
        /// timeout: cargo test --workspace --lib --bins (1800s elapsed)").
        /// Logged verbatim, never parsed.
        #[serde(default)]
        reason: Option<String>,
        /// Target managed-workspace root (Issue #3929). `Some(root)` records
        /// against that repo's registry; `None` uses the daemon's default
        /// workspace, matching [`Request::ClearQuarantine`].
        #[serde(default)]
        workspace_root: Option<String>,
    },
    // ========================================================================
    // Autonomous Daemon Status (Issue #3891 — follow-up to #3813 Phase D)
    // ========================================================================
    /// Request the daemon's autonomous-mode operability snapshot: the live
    /// in-flight sweeps, the three dynamic-cap inputs (disk headroom, ram
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
    /// [`Request::RestartDaemon`]'s refusal contract — **unless** `then_exit`
    /// is set (see below), in which case the supervisor requirement does not
    /// apply at all.
    ///
    /// - `then_exit` (Issue #4343, `fleet drain`'s teardown use case):
    ///   `false` (the #4090 default) preserves the original restart-when-drained
    ///   behavior byte-for-byte. `true` changes the terminal action from
    ///   "exit [`crate::ipc::EXIT_RESTART`] for a supervised relaunch" to "exit
    ///   [`crate::ipc::EXIT_SHUTDOWN`] and **stay down**" — the daemon must not
    ///   pick up new dispatch on a host about to be powered off, so a relaunch
    ///   would defeat the whole point of draining before teardown. Because a
    ///   `then_exit` drain deliberately does **not** want a relaunch, the
    ///   supervisor-detection refusal gate is skipped entirely for it (there is
    ///   nothing to prove supervision *for*). `#[serde(default)]` keeps
    ///   pre-#4343 wire data (`{"type":"DrainAndRestartDaemon","payload":{...}}`
    ///   with no `then_exit` key) parsing as `false` — the original behavior.
    ///
    ///   When a drain is **already in progress**, `timeout_secs` and
    ///   `force_after_timeout` are ignored (the active deadline is pinned) but
    ///   `then_exit: true` **escalates** the active drain one-way from relaunch
    ///   to stay-down (Issue #4521); `then_exit: false` never downgrades an
    ///   active teardown drain. The reply's `then_exit` reports what the active
    ///   drain will actually do — see [`Response::DaemonDrain`].
    DrainAndRestartDaemon {
        timeout_secs: Option<u64>,
        force_after_timeout: bool,
        #[serde(default)]
        then_exit: bool,
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
    /// Typed, secret-free fail-closed runtime admission rejection.
    RuntimeRejected(RuntimeRejection),
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
    /// Result of a `ListQuarantines` request (Issue #4215): the active
    /// insta-crash quarantines, across one or every registered workspace
    /// depending on the request's `workspace_root`. Empty when nothing is
    /// currently quarantined.
    QuarantineList {
        entries: Vec<QuarantineEntry>,
    },
    /// Result of a `RecordDispatchFailure` request (Issue #6192). Mirrors the
    /// daemon's own bookkeeping so the caller can log/report the resulting
    /// window without a second round trip. `backoff_secs` is `None` when the
    /// backoff mechanism is disabled (the record call is then a no-op —
    /// `consecutive` is also `0` in that case).
    DispatchFailureRecorded {
        issue: u32,
        /// Consecutive failed-dispatch count now on record for `issue`.
        consecutive: u32,
        /// Seconds until the next dispatch is allowed, or `None` when the
        /// backoff mechanism is disabled.
        backoff_secs: Option<u64>,
    },
    // ========================================================================
    // Autonomous Daemon Status (Issue #3891 — follow-up to #3813 Phase D)
    // ========================================================================
    /// Result of a `DaemonStatus` request — the autonomous-mode operability
    /// snapshot rendered by `loom-daemon status`. Boxed (issue #4292, when
    /// `token_pool_dir` pushed `DaemonStatusReport` far enough past the next-
    /// largest `Response` variant to trip `clippy::large_enum_variant`) so this
    /// one large, infrequent (once per status poll) payload does not force
    /// every other `Response` variant to reserve its stack space.
    DaemonStatus(Box<DaemonStatusReport>),
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
    /// `then_exit` (Issue #4343) reports the **active** drain's terminal action:
    /// `true` means the daemon will exit and stay down once drained (never
    /// relaunch), `false` means it will exit for a supervised relaunch.
    ///
    /// It is **not** an echo of the request (Issue #4521). On the
    /// already-draining path the requested value may differ from the active
    /// drain's: a `then_exit: true` request escalates an in-progress
    /// relaunch-drain one-way to stay-down (so the reply is `true`), while a
    /// `then_exit: false` request against an active teardown drain is *not*
    /// honored (the reply stays `true`). Clients MUST render "will stop" vs
    /// "will restart" from this field, never from their own request — a client
    /// that renders from the request promises a teardown the daemon never
    /// performs. `#[serde(default)]` keeps pre-#4343 wire data parsing (as
    /// `false`), which is also how a new client detects version skew against an
    /// old daemon that silently dropped the request's `then_exit`.
    DaemonDrain {
        accepted: bool,
        supervisor: Option<String>,
        in_flight: usize,
        message: String,
        #[serde(default)]
        then_exit: bool,
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
/// `Issue` drives the full Curator → Builder → Judge → Doctor → Merge
/// lifecycle for one issue. `PrSet` (Mode C, issue #5342) drives
/// Judge/Doctor → Judge/Merge for an existing set of open PRs without
/// re-running Curator/Builder — see `/loom:sweep --prs <n1> <n2> ...`.
/// `PrSet` has no single issue number, so it claims no `loom:building`
/// label and is tracked via a per-PR claim lock (`.loom/locks/pr-<N>/`,
/// distinct from `Issue`'s `.loom/locks/issue-<N>/`) rather than the
/// issue-keyed guard chain `Issue` dispatch uses.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", content = "value")]
pub enum SweepKind {
    /// Issue-keyed sweep: `claude -p "/loom:sweep <N>"`.
    Issue(u32),
    /// PR-set sweep: `claude -p "/loom:sweep --prs <n1> <n2> ..."`.
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
    /// Process-group ID of the detached child (Issue #4980). Sweeps are spawned
    /// as their own group leader (`process_group(0)`, #3800), so for a live
    /// dispatch this equals [`pid`](Self::pid) — but recording it explicitly is
    /// what makes group termination survive the loss of the spawning process:
    ///
    /// - a `reconstruct()`-ed entry (daemon restart) has no retained `Child`
    ///   handle, and used to silently degrade cancellation to a single-PID kill
    ///   that orphaned the whole subtree;
    /// - the crash-path reaper needs a group handle to reap survivors of a
    ///   *dead* leader, whose pgid can no longer be queried from the OS.
    ///
    /// `None` for entries whose group is unknown (a pre-#4980 `owner.json`, a
    /// checkpoint-only recovery entry, a non-Unix host, or a test-injected
    /// entry) — every consumer degrades to single-PID signalling and logs,
    /// never assumes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pgid: Option<u32>,
    /// Token account name selected by `spawn-claude.sh` (e.g. `agent-2.token`).
    /// "unknown" when not surfaced by the wrapper (Phase A logs this in
    /// the per-sweep log rather than recording it on the entry).
    pub token_name: String,
    /// Runtime adapter selected for this dispatch (`claude`, `codex`, etc.).
    /// Legacy entries and fixture adapters that do not emit the neutral
    /// observability contract safely degrade to `unknown`.
    #[serde(default = "default_sweep_runtime")]
    pub runtime: String,
    /// Precedence tier that selected `runtime`; absent for legacy entries.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime_source: Option<RuntimeSource>,
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
    /// Set directly on `Crashed` entries (the reaper's crash path and
    /// `reconstruct_from_checkpoints`). For `Running`/`Pending` entries this
    /// field is `None` in the stored registry entry — `SweepRegistry::list`
    /// overlays a live read of the on-disk checkpoint at query time (#4328)
    /// so `ListSweeps`/`loom-daemon status` show the sweep's current phase
    /// without the registry needing to poll the filesystem on every tick.
    /// The checkpoint's `phase` field is a completion marker
    /// (`curator-done`/`builder-done`/`judge-rejected`/`judge-done`/
    /// `doctor-done`/`merge-done`), rendered verbatim rather than mapped to
    /// an inferred "current phase" — a mapping risks misleading whoever is
    /// reading it when a sweep dies between phase boundaries.
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

fn default_sweep_runtime() -> String {
    "unknown".to_string()
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
/// `Default` (added with #4761) is the zero-workspaces / zero-in-flight shape a
/// freshly-started daemon with no registered repos reports — it exists so test
/// fixtures can spread `..Default::default()` instead of restating ~40 fields
/// (and needing an edit every time one is added).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DaemonStatusReport {
    /// Sweeps in a non-terminal state (`Pending` / `Running`) at snapshot time.
    /// The full `SweepInfo` is carried so the CLI can render issue numbers,
    /// PIDs, token account, and latest phase without a second round-trip.
    pub in_flight: Vec<SweepInfo>,
    /// Issues whose per-issue lock (`.loom/locks/issue-<N>/owner.json`) has a
    /// **live** `owner_pid` but no matching entry in [`Self::in_flight`]
    /// (Issue #4214) — a live, locked sweep that the in-memory registry union
    /// has (transiently or otherwise) lost track of. `build_daemon_status`
    /// cross-checks every root's lock directory against its own registry so
    /// this class can never be silently empty when the underlying condition is
    /// real: a liveness monitor should read entries here as **alive** (a
    /// reconciliation gap, not a dead sweep), never as absence. A stale lock
    /// (dead `owner_pid`) is deliberately excluded — that remains
    /// `reconstruct()`'s cleanup remit. Empty in the overwhelmingly common
    /// case. `#[serde(default)]` keeps pre-#4214 wire data / older clients
    /// compatible (an absent field parses as an empty vec).
    #[serde(default)]
    pub unregistered_locked: Vec<UnregisteredLockedSweep>,
    /// Dynamic-cap input 1: size of the multi-account token pool
    /// (`.loom/tokens/*.token`), the hard ceiling on concurrent sweeps
    /// (never over-subscribe an OAuth account). Via [`crate::tokens::token_pool_size`].
    pub token_pool_size: usize,
    /// The tokens-pool directory the daemon actually resolved for
    /// [`Self::token_pool_size`] / [`Self::capacity`] (issue #4292): the
    /// primary workspace's per-repo pool (`<workspace>/.loom/tokens`) when it
    /// holds `*.token` files, else the shared machine-level pool
    /// (`~/.loom/tokens`, override `LOOM_SHARED_TOKENS_DIR`) — the same
    /// precedence [`crate::tokens_pool::paths::resolve_tokens_dir`] applies
    /// everywhere else. Surfaced so `loom-daemon status` can print *which*
    /// pool it used no matter which directory it was invoked from, and so the
    /// CLI's client-side per-token usage probe (`collect_token_usage`) can
    /// target this exact directory instead of independently re-deriving one
    /// from its own cwd — the mismatch that let `status` report a false
    /// token picture when run from a directory other than the daemon's own
    /// workspace. `#[serde(default)]` keeps pre-#4292 wire data / older
    /// daemon binaries compatible (an absent field parses as `None`).
    #[serde(default)]
    pub token_pool_dir: Option<PathBuf>,
    /// Dynamic-cap input 2: how many worktrees the scratch volume can hold at
    /// `LOOM_PER_WORKTREE_GB` each. Via [`crate::disk_headroom::disk_headroom_limit`].
    pub disk_headroom: usize,
    /// Dynamic-cap input (#5270): how many worktrees the host's
    /// currently-available RAM can hold at `LOOM_PER_WORKTREE_RAM_GB` each.
    /// Via [`crate::ram_headroom::ram_headroom_limit`] — the second machine-
    /// headroom axis alongside [`Self::disk_headroom`], added when the token
    /// axis was removed from admission entirely (operator direction: "we
    /// should only ever limit parallelism based on the machine
    /// disk/RAM/CPU"). `#[serde(default)]` keeps pre-#5270 wire data / older
    /// clients compatible (an absent field parses as `0`, i.e. "unknown" —
    /// callers should treat a `0` from a stale client cautiously, the same
    /// caution any dynamic-cap input warrants).
    #[serde(default)]
    pub ram_headroom: usize,
    /// The host's logical CPU count (#3978), via
    /// [`crate::cpu_headroom::logical_cpu_count`]. **Observational only since
    /// #4512** — the CPU headroom *term* it used to feed was removed from the
    /// admission formula; the status view still renders it (with
    /// [`Self::cpu_idle_fraction`]) because observed CPU usage is exactly the
    /// evidence an operator tunes `maxConcurrent` from. `#[serde(default)]`
    /// keeps pre-#3978 wire data compatible (`0` = "not reported").
    #[serde(default)]
    pub logical_cpus: usize,
    /// The current 1-minute load average (#3978), via
    /// [`crate::cpu_headroom::read_loadavg_1m`]. `None` on a platform/host where
    /// no load-average source is available. Observational (see
    /// [`Self::logical_cpus`]); the load-per-core ratio derived from it is what
    /// the host-distress circuit breaker (#4235) trips on. `#[serde(default)]`
    /// keeps pre-#3978 wire data compatible.
    #[serde(default)]
    pub loadavg_1m: Option<f64>,
    /// The measured CPU idle fraction (`0.0..=1.0`) (#4031), via
    /// [`crate::cpu_headroom::cached_cpu_idle_fraction`] — the signal that
    /// replaced the 1-minute load average as the source of "consumed cores"
    /// (load average overstated consumption by ~1.5× on macOS because it counts
    /// network-I/O-blocked `claude` sessions that consume no core). `None` when
    /// no idle sample has been taken yet. Observational since #4512 (see
    /// [`Self::logical_cpus`]). `#[serde(default)]` keeps pre-#4031 wire data /
    /// older clients compatible.
    #[serde(default)]
    pub cpu_idle_fraction: Option<f64>,
    /// Whether in-flight occupancy has actually reached the dynamic cap
    /// (`in_flight.len() >= dynamic_cap`) — i.e. the cap is *currently binding*,
    /// not merely the smallest ceiling (#4031). When `false`, no resource term
    /// (tokens/disk/ceiling) is the limiter — the limiter is **work availability**,
    /// and the status renderer suppresses the "token-bound" diagnosis rather than
    /// misreporting a bottleneck at, say, 1 in-flight against a cap of 7.
    /// `#[serde(default)]` keeps pre-#4031 wire data / older clients compatible
    /// (an absent field parses as `false` — "not capacity-bound").
    #[serde(default)]
    pub capacity_bound: bool,
    /// Whether the default/primary workspace's claude-wrapper pre-flight-death
    /// tripwire is currently tripped (Issue #4386): N consecutive dispatches,
    /// across *different* issues, died at the wrapper's MCP-init pre-flight
    /// check before ever reaching `# CLAUDE_CLI_START` — the classic
    /// stale-`.mcp.json` fleet-wide silent-failure signature. See
    /// `SweepRegistry::preflight_advisory`. While `true`, the status renderer
    /// must surface [`Self::preflight_advisory_message`] instead of — or
    /// immediately alongside — the bare "not capacity-bound … the limiter is
    /// work availability" line, so a fleet-wide spawn failure never reads as
    /// an idle-healthy daemon. `#[serde(default)]` keeps pre-#4386 wire data /
    /// older clients compatible (an absent field parses as `false`).
    #[serde(default)]
    pub preflight_advisory_active: bool,
    /// The operator-facing advisory message when
    /// [`Self::preflight_advisory_active`] is `true` (e.g. `"WARNING: last 3
    /// dispatches died at claude-wrapper pre-flight (preflight-mcp-failed) —
    /// check .mcp.json"`), else `None`. `#[serde(default)]` keeps pre-#4386
    /// wire data compatible.
    #[serde(default)]
    pub preflight_advisory_message: Option<String>,
    /// Wall-clock time of the most recent trip/clear transition backing
    /// [`Self::preflight_advisory_active`] (Issue #5029) — `None` before the
    /// first transition this daemon process has observed. Lets the status
    /// renderer show an "as of" freshness indicator alongside the warning, so
    /// a historical (already-cleared) tripped count is never mistaken for a
    /// live one. Purely a display addition — carries no decision logic of its
    /// own. `#[serde(default)]` keeps pre-#5029 wire data / older clients
    /// compatible (an absent field parses as `None`).
    #[serde(default)]
    pub preflight_advisory_changed_at: Option<DateTime<Utc>>,
    /// Dynamic-cap input 3: **the** per-machine admission knob
    /// (`autonomous.workFinder.maxConcurrent` / `LOOM_WORK_FINDER_MAX_CONCURRENT`).
    /// Since #4512 this is the only *policy* term in the cap — the other two
    /// meter exhaustible resources (accounts, bytes) — so it is what an operator
    /// tunes for a host, empirically, from the observed idle fraction above.
    pub configured_max: usize,
    /// The effective dynamic concurrency cap —
    /// `min(disk_headroom, ram_headroom, configured_max)`
    /// (`resolve_dynamic_max_concurrent`; the CPU term that sat in this `min`
    /// from #3978 was removed in #4512, and the token axis was removed
    /// entirely in #5270 — see [`Self::ram_headroom`]). This is the
    /// total-occupancy ceiling the work finder recomputes every tick.
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
    /// Whether the gate's most recent tick for the daemon's primary workspace
    /// DEFERRED for host load (#4259) — a bounded, load-aware scheduling
    /// decision distinct from both `main_health_gate_halted` (verified-red) and
    /// `main_health_gate_not_evaluated` (the gate could not run this tick). A
    /// deferred tick leaves any prior halt flag untouched and is NOT evidence
    /// about `main`; it exists so a host that is permanently at the dispatch cap
    /// reports `deferred (load …)` instead of burning the full timeout to report
    /// a `not evaluated (timeout …)`. `#[serde(default)]` keeps pre-#4259 wire
    /// data compatible.
    #[serde(default)]
    pub main_health_gate_deferred: bool,
    /// A short `load …` summary of *why* the gate is deferring (#4259) — e.g.
    /// `"load 1.05/core for 14m — fast tier runs at the 30m bound"` — or `None`
    /// when the most recent tick was not a deferral. Rendered distinctly from
    /// the UNEVALUATED `not_evaluated_reason` so an operator can tell "the host
    /// is too busy to run the gate right now" from "the gate ran and could not
    /// produce a verdict". `#[serde(default)]` keeps pre-#4259 wire data
    /// compatible.
    #[serde(default)]
    pub main_health_gate_deferred_reason: Option<String>,
    /// The tier (`"full"` / `"fast"`) of the most recent completed verdict for
    /// the daemon's primary workspace (#4259), or `None` before the first. The
    /// fast tier runs only a compile+smoke subset, so a fast-tier Green is NOT
    /// equivalent to a full-suite Green — this label keeps the two
    /// distinguishable on the status surface. `#[serde(default)]` keeps
    /// pre-#4259 wire data compatible.
    #[serde(default)]
    pub main_health_gate_verdict_tier: Option<String>,
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
    /// Whether the autonomous self-update loop (Issue #4055) is enabled for this
    /// daemon process. `false` in the common opt-out case (the loop is
    /// default-OFF). `#[serde(default)]` keeps pre-#4055 wire data / older
    /// clients compatible (an absent field parses as `false` — "loop off"),
    /// mirroring the `draining` forward-compat convention.
    #[serde(default)]
    pub auto_update_enabled: bool,
    /// Wall-clock time of the auto-update loop's most recent staleness check
    /// (Issue #4055), or `None` when the loop has not ticked yet (or is
    /// disabled). `#[serde(default)]` keeps pre-#4055 wire data compatible.
    #[serde(default)]
    pub auto_update_last_check: Option<DateTime<Utc>>,
    /// Wall-clock time of the auto-update loop's most recent successful roll
    /// (rebuild + provision + drain-triggered restart, Issue #4055), or `None`
    /// when no roll has happened this process. `#[serde(default)]` keeps
    /// pre-#4055 wire data compatible.
    #[serde(default)]
    pub auto_update_last_roll: Option<DateTime<Utc>>,
    /// Consecutive retryable build failures the auto-update loop has seen
    /// (Issue #4055); resets to `0` on a successful roll or when the source
    /// commit advances. Feeds the exponential backoff. `#[serde(default)]`
    /// keeps pre-#4055 wire data compatible (an absent field parses as `0`).
    #[serde(default)]
    pub auto_update_consecutive_failures: u32,
    /// The current backoff delay in seconds the auto-update loop is waiting out
    /// after a retryable build failure (Issue #4055), or `None` when it is not
    /// backing off. `#[serde(default)]` keeps pre-#4055 wire data compatible.
    #[serde(default)]
    pub auto_update_backoff_secs: Option<u64>,
    /// A terminal give-up reason (Issue #4055): a non-retryable failure such as
    /// the #4053 build-verification mismatch (exit 4/5). The loop stops
    /// attempting until the source commit advances; `None` when not in a
    /// terminal state. `#[serde(default)]` keeps pre-#4055 wire data compatible.
    #[serde(default)]
    pub auto_update_terminal_reason: Option<String>,
    /// A short human-readable note about the auto-update loop's most recent tick
    /// (Issue #4055) — e.g. "up to date", "within settle window", "source tree
    /// dirty", or the last roll/failure detail — so `loom-daemon status` never
    /// leaves the loop's behavior unexplained. `None` before the first tick.
    /// `#[serde(default)]` keeps pre-#4055 wire data compatible.
    #[serde(default)]
    pub auto_update_note: Option<String>,
    /// Host-distress circuit-breaker state (Issue #4235). `Some` when a breaker
    /// has been registered this process (the work-finder loop is running and the
    /// breaker is enabled); `None` when no breaker is active — which the status
    /// renderer treats as "breaker inactive", the zero-behavior-change baseline.
    /// `#[serde(default)]` keeps pre-#4235 wire data / older clients compatible
    /// (an absent field parses as `None`). Boxed (`clippy::large_enum_variant`):
    /// `HostBreakerStatus` is the field that tips `Response::DaemonStatus` past
    /// the second-largest variant, and the indirection is the cheapest fix (one
    /// heap alloc on an already-rare, human-latency status round-trip).
    #[serde(default)]
    pub host_breaker: Option<Box<HostBreakerStatus>>,
    /// Saturation admission-brake state (Issue #4903). `Some` once a brake has
    /// been registered this process (the daemon registers one at startup
    /// alongside the host breaker); `None` when none is active — which the
    /// status renderer treats as "brake inactive", the zero-behavior-change
    /// baseline. Answers the question `capacity_bound: false` could not: a host
    /// that is refusing new work because it is already saturated now says so
    /// instead of reading as idle. `#[serde(default)]` keeps pre-#4903 wire data
    /// / older clients compatible (an absent field parses as `None`).
    #[serde(default)]
    pub admission_brake: Option<AdmissionBrakeStatus>,
    /// GitHub rate-limit circuit-breaker state (Issue #4429). `Some` once the
    /// breaker is registered at startup; `None` on older daemons.
    /// `#[serde(default)]` keeps pre-#4429 wire data / older clients compatible.
    /// Boxed for the same `clippy::large_enum_variant` reason as
    /// [`Self::host_breaker`].
    #[serde(default)]
    pub rate_limit_breaker: Option<Box<RateLimitBreakerStatus>>,
    /// Live safehouse fleet-comms connection state (Issue #4345): distinguishes
    /// `not_configured` (no `safehouse` block / disabled) from `unreachable`
    /// (enabled, socket resolved, but the daemon's own connection attempt
    /// failed/dropped) from `connected` (room joined) — before this the three
    /// looked identical to an operator (silence). Rendered from
    /// [`crate::safehouse::SafehouseState`] via
    /// [`crate::workspace_pool::WorkspacePool::safehouse_status`]. `None` only
    /// for a pre-#4345 wire payload from an older daemon binary that never
    /// computed one. `#[serde(default)]` keeps that wire data compatible.
    #[serde(default)]
    pub safehouse: Option<SafehouseStatus>,
    /// Whether the autonomous work-finder loop is enabled for THIS running
    /// daemon process (Issue #4693), via
    /// [`crate::work_finder::resolve_enabled`] read from the daemon's own
    /// environment/config at query time — the exact same precedence (env >
    /// config > default) `loom-daemon-start.sh` used to bake `LOOM_WORK_FINDER`
    /// into the plist/unit this process was launched from, so this field always
    /// reflects the truth of the process actually answering, never a
    /// re-derivation from the CLI client's own cwd. Mirrors
    /// [`Self::main_health_gate_enabled`]'s shape and forward-compat contract:
    /// `Some(true)` / `Some(false)` are resolved daemon-side; `None` only for a
    /// pre-#4693 wire payload from an older daemon binary that never computed
    /// one (never misread as `false`). Consumed by the `status` CLI to flag the
    /// marker-vs-non-autonomous-daemon mismatch (AC3 of #4693): the
    /// autonomy-desired marker (#4011) present while this reads `Some(false)`
    /// means dispatch is silently not happening on an otherwise-healthy,
    /// reachable daemon. `#[serde(default)]` keeps pre-#4693 wire data
    /// compatible.
    #[serde(default)]
    pub work_finder_enabled: Option<bool>,
    /// The most recent work-finder tick's dispatch/skip summary (Issue #4761),
    /// published by the loop itself via
    /// [`crate::work_finder::publish_tick_summary`] and read back here so a
    /// cross-process consumer (`loom-daemon health`, the dashboard) can see
    /// *why* a tick dispatched nothing without grepping the daemon log. `None`
    /// when the loop has not completed a tick this process (or is disabled),
    /// and for a pre-#4761 wire payload. `#[serde(default)]` keeps older wire
    /// data / older clients compatible.
    #[serde(default)]
    pub last_work_finder_tick: Option<WorkFinderTickSummary>,
    /// A bounded, newest-last window of per-(root, role) role-runner tick
    /// outcomes (Issue #4761), published by the role-runner loop via
    /// [`crate::role_runner::record_role_tick`]. Carried as raw records rather
    /// than a pre-computed verdict so the *client* chooses the window
    /// (`loom-daemon health --since 30m`) and applies the
    /// transient-vs-persistent classifier
    /// ([`crate::health::summarize_role_ticks`]) — the daemon has no opinion
    /// about which window an operator cares about. Bounded to
    /// [`crate::role_runner::ROLE_TICK_RING_CAPACITY`] entries so the payload
    /// stays small enough for a 5s-interval dashboard poll. Empty when the
    /// role runner is disabled or has not ticked. `#[serde(default)]` keeps
    /// pre-#4761 wire data compatible.
    #[serde(default)]
    pub role_tick_records: Vec<RoleTickRecord>,
    /// The last-observed-tick timestamp for every `(root, role)` pair this
    /// daemon process has ever ticked (Issue #6201), published by
    /// [`crate::role_runner::record_role_tick_at`] and read back here via
    /// [`crate::role_runner::last_role_tick_snapshot`].
    ///
    /// Deliberately **independent** of [`Self::role_tick_records`]'s bounded
    /// ring: a role that stops ticking entirely while several other roles on
    /// the same workspace keep ticking normally has its ring entries evicted
    /// within hours (see [`crate::role_runner::ROLE_TICK_RING_CAPACITY`]'s
    /// doc comment), at which point the windowed `roles` health section sees
    /// zero records for it and reports a clean bill of health instead of a
    /// silent, indefinite gap — the exact incident #6201 was filed for. This
    /// field is bounded by `(root, role)` cardinality, not by tick volume, so
    /// it survives that eviction and lets
    /// [`crate::health::assess_role_liveness`] answer "when did this role
    /// last tick AT ALL", independent of how busy the rest of the fleet is.
    /// `#[serde(default)]` keeps pre-#6201 wire data compatible.
    #[serde(default)]
    pub role_last_tick: Vec<RoleLastTick>,
    /// Role-runner agents in flight **right now** across every managed
    /// workspace (#6102), sampled from
    /// [`crate::role_runner::global_active_run_count`].
    ///
    /// Reported alongside [`Self::in_flight`] because the two together — not
    /// sweeps alone — are this daemon's agent load on the host. Before #6102
    /// the only way to see this number was `pgrep -f claude-wrapper.sh`, which
    /// is how an operator came to read "1 sweep in flight, cap 8" on a box
    /// running 11 agents at a 1m load average of 32.
    ///
    /// `0` when the role runner is disabled or has never been spawned in this
    /// process. `#[serde(default)]` keeps pre-#6102 wire data / older clients
    /// compatible (an absent field parses as `0`).
    #[serde(default)]
    pub active_role_agents: usize,
    /// The concurrent role-agent ceiling this daemon resolved for its primary
    /// workspace (#6102) — env > `autonomous.roleRunner.maxConcurrent` >
    /// [`crate::role_runner::default_max_concurrent`], via
    /// [`crate::role_runner::resolve_max_concurrent`].
    ///
    /// This is a **separate** ceiling from the sweep dynamic cap
    /// ([`Self::dynamic_cap`]): `autonomous.workFinder.maxConcurrent` bounds
    /// sweep dispatch only, and role agents are never routed through
    /// work-finder admission. Both numbers are reported so `status` can state
    /// the total agent budget instead of implying one knob covers it.
    ///
    /// `None` for a pre-#6102 wire payload / an older daemon that does not
    /// report one — never conflate that with "unbounded".
    #[serde(default)]
    pub role_agent_max_concurrent: Option<usize>,
    /// The **real OS pid of the process that answered this request** (Issue
    /// #4774) — i.e. the daemon that actually owns the IPC socket, established
    /// by `std::process::id()` inside the handler rather than read from any
    /// file.
    ///
    /// This is the ground truth every pid-file consumer was missing. Before
    /// #4774 the only "daemon pid" available to `status` / `health` came from
    /// `<state home>/.daemon.pid`, which `loom-daemon-start.sh` wrote once at
    /// provisioning time and no supervisor relaunch ever refreshed — so a
    /// stale file was indistinguishable from a correct one. With this field a
    /// client can cross-check the two ([`crate::daemon_pidfile::classify`])
    /// and report a mismatch instead of silently trusting the file.
    /// `#[serde(default)]` keeps pre-#4774 wire data / older daemons
    /// compatible (an absent field parses as `None` ⇒ "cannot cross-check",
    /// never a false mismatch).
    #[serde(default)]
    pub daemon_pid: Option<u32>,
    /// The pid file path this daemon resolved and claimed at startup (Issue
    /// #4774), via [`crate::daemon_pidfile::resolve_pid_file_path`]. `None`
    /// when no path could be resolved, and for a pre-#4774 wire payload.
    ///
    /// Reported so a client cross-checks the file the **daemon** actually
    /// writes rather than one re-derived from the CLI process's own
    /// environment — the same rule [`Self::token_pool_dir`] (#4292) follows,
    /// and for the same reason: `status` / `health` are routinely run from a
    /// different cwd (and a different `LOOM_*` env) than the daemon.
    #[serde(default)]
    pub pid_file: Option<PathBuf>,
    /// The git commit the **running daemon binary** was built from (Issue
    /// #4824) — [`crate::self_update::BUILT_COMMIT`], baked in at compile time
    /// by `build.rs` and taken daemon-side inside the status handler.
    ///
    /// This is the field that lets a client tell *CLI/daemon build skew* apart
    /// from a genuine fault. `loom-daemon` is one binary serving two roles, so
    /// a client (`health`, the dashboard) knows its own `BUILT_COMMIT` but had
    /// no way to learn the answering process's — and after every
    /// `git pull` + rebuild the two disagree until the daemon is rolled. A
    /// newer client then reads an *older* daemon's honest "I have no such
    /// telemetry" (`None`) as "the subsystem is dead" and pages on a healthy
    /// fleet (the 2026-07-31 false `DEGRADED`: a `health` built from a commit
    /// with #4771's tick telemetry querying a daemon built one commit before
    /// it). With this field the client reports the skew as its own condition.
    ///
    /// Deliberately distinct from [`crate::self_update::SelfUpdateStatus`]'s
    /// `built_commit`, which compares *this* process's binary against its
    /// source checkout; this compares the **client** process's build against
    /// the **daemon** process's.
    ///
    /// `None` for a pre-#4824 wire payload from an older daemon binary that
    /// never reported one (never misread as "matches"), and `Some("unknown")`
    /// for a tarball build with no git commit available — both mean "cannot
    /// compare". `#[serde(default)]` keeps older wire data compatible.
    #[serde(default)]
    pub daemon_build_commit: Option<String>,
    /// The raw build-time stamp the **running daemon process** was built at
    /// (Issue #5341) — [`crate::self_update::BUILT_AT_RAW`], taken daemon-side
    /// inside the status handler, the same way [`Self::daemon_build_commit`]
    /// is. ISO-8601 UTC (e.g. `2026-08-02T03:09:51Z`), or `"unknown"` for a
    /// build host that lacked a usable `date`.
    ///
    /// Exists for the same reason `daemon_build_commit` does: a `loom-daemon
    /// status`/`--version` invocation execs the ON-DISK binary fresh, so it
    /// can only ever speak for the disk build — never for a long-running
    /// daemon PROCESS that predates a since-updated disk binary. Comparing
    /// this against this CLI process's own `self_update::BUILT_AT_RAW` (which,
    /// because it is a fresh exec, IS the disk build) is what lets `status`
    /// tell a stale-but-still-answering daemon apart from a current one,
    /// instead of a rebuilt disk binary silently masking a stale running
    /// process (the `loom-worker-1` incident this issue exists for).
    ///
    /// `None` for a pre-#5341 wire payload from an older daemon binary that
    /// never reported one. `#[serde(default)]` keeps older wire data
    /// compatible.
    #[serde(default)]
    pub daemon_built_at_raw: Option<String>,
    /// The work-finder tick interval, in seconds, that THIS running daemon
    /// process resolved (Issue #4824) via
    /// [`crate::work_finder::resolve_interval_with_config`] — env > config >
    /// default, exactly as the loop itself resolved it.
    ///
    /// Reported for the same reason [`Self::work_finder_enabled`] is: a client
    /// must not re-derive the daemon's cadence from its own cwd/env. It is the
    /// unit a client scales the post-restart grace window by ("no tick yet" is
    /// only a fault once the daemon has been up for more than a couple of tick
    /// intervals); without it a `health` run against a daemon on a longer
    /// interval false-alarms for the whole first interval after every roll.
    ///
    /// `None` for a pre-#4824 wire payload ⇒ clients fall back to
    /// [`crate::work_finder::DEFAULT_WORK_FINDER_INTERVAL_SECS`].
    /// `#[serde(default)]` keeps older wire data compatible.
    #[serde(default)]
    pub work_finder_interval_secs: Option<u64>,
    /// An observability-exporter host-identity mismatch detected this process
    /// (Issue #4830): the ingest key installed on this host is bound to a
    /// *different* `host_id` than the one this daemon reports for itself, so
    /// every record it pushes is filed under the wrong host.
    ///
    /// `None` in the overwhelmingly common case — the ids agree, the exporter
    /// is disabled/keyless, or no batch has been acked yet this process — and
    /// for a pre-#4830 wire payload. Published by
    /// [`crate::observability::HostIdStatus`] and read here via
    /// [`crate::observability::global_host_id_mismatch`]. `#[serde(default)]`
    /// keeps older wire data / older clients compatible.
    #[serde(default)]
    pub observability_host_id_mismatch: Option<ObservabilityHostIdMismatch>,
    /// **Positive** confirmation of whether telemetry is actually reaching the
    /// backend, and under which `host_id` (Issue #5083).
    ///
    /// [`Self::observability_host_id_mismatch`] above is anomaly-only by
    /// design (#4830), which left "exporting fine", "observability disabled",
    /// and "configured but silently never exported" all rendering as the same
    /// thing: nothing at all. This field is the counterpart that always has an
    /// answer — see [`ObservabilityExportStatus`].
    ///
    /// `None` **only** from a pre-#5083 daemon binary that never computed one
    /// (never misread as "disabled" — a running daemon that has observability
    /// off reports `Some(ObservabilityExportStatus::disabled())`).
    /// `#[serde(default)]` keeps older wire data / older clients compatible.
    #[serde(default)]
    pub observability_export: Option<ObservabilityExportStatus>,
    /// The daemon-wide peer-claim view + transport counters (Issue #5921):
    /// which issues THIS host currently sees claimed, by which host and with
    /// what remaining TTL, plus how many claims have been advertised /
    /// received / expired / caused a dispatch skip since process start.
    /// Before this field, [`crate::peer_claims::PeerClaimView`] had no
    /// external observability surface at all — a duplicate cross-host
    /// dispatch (e.g. #5789) could not be distinguished from a broken
    /// re-advertisement path without attaching a debugger.
    ///
    /// `None` when no safehouse peer-claim coordination has been established
    /// for this daemon (`safehouse.enabled` false, or enabled with no socket
    /// ever resolving) — the same condition
    /// [`SweepRegistry::peer_claimed_issues`](crate::sweep_registry::SweepRegistry::peer_claimed_issues)
    /// already treats as "no view attached", rendered honestly here rather
    /// than as an empty-but-present view. `#[serde(default)]` keeps pre-#5921
    /// wire data / older clients compatible.
    #[serde(default)]
    pub peer_claims: Option<PeerClaimStatus>,
    /// Per-repo state of the pressure-triggered deep-clean pass (#5919), one
    /// entry per registered workspace the reaper has evaluated this process.
    ///
    /// Answers "is this host reclaiming its own disk?" without reading logs —
    /// including the negative case, which is the common one: an entry whose
    /// `last_fired_at` is `None` but whose `last_reason` reads
    /// `"118G free >= 20G floor — no disk pressure"` is a *healthy* host, and
    /// is meaningfully different from a host where the pass is disabled or has
    /// never run at all (no entry). Published by
    /// [`crate::deep_clean::publish`], read here via
    /// [`crate::deep_clean::snapshot`].
    ///
    /// Empty until the reaper's first post-startup tick (it deliberately skips
    /// the immediate first one), and for a pre-#5919 wire payload.
    /// `#[serde(default)]` keeps older wire data / older clients compatible.
    #[serde(default)]
    pub deep_clean: Vec<DeepCleanRepoStatus>,
    /// Live idle-exit eligibility (Issue #5565) — process-global snapshot
    /// published by the `autonomous.idleExit` tracker task on every tick, via
    /// [`crate::idle_exit::global_status_snapshot`]. Always present (never
    /// `None`) from a daemon of this vintage: a daemon with the task never
    /// spawned (feature disabled) reports `enabled: false, eligible: false`
    /// — the fail-safe "cannot determine" baseline — rather than omitting
    /// the field, so a consumer distinguishes "disabled" from "a pre-#5565
    /// daemon that never computed this at all" only via the *absence* of the
    /// field on the wire (`#[serde(default)]` deserializes an older payload
    /// as `None`).
    #[serde(default)]
    pub idle_exit: Option<IdleExitStatus>,
}

/// One registered repo's deep-clean state on the status wire (#5919) — the
/// serializable projection of [`crate::deep_clean::DeepCleanState`].
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct DeepCleanRepoStatus {
    /// The repo root whose **primary checkout** this describes.
    pub root: PathBuf,
    /// When the pass most recently evaluated this repo (fired or not).
    pub last_evaluated_at: Option<DateTime<Utc>>,
    /// That evaluation's verdict in words, e.g. `"118G free >= 20G floor — no
    /// disk pressure"` or `"DISK PRESSURE — 2G free < 20G floor"`.
    pub last_reason: Option<String>,
    /// Free GB measured on the repo's own volume at that evaluation. `None`
    /// when unmeasurable (unknown != zero, #4164).
    pub last_free_gb: Option<u64>,
    /// When a pass most recently **fired** for this repo in this daemon
    /// process. `None` means "not since this daemon started" — not "never":
    /// the cooldown is process state by design (see
    /// [`crate::deep_clean::DeepCleanState`]).
    pub last_fired_at: Option<DateTime<Utc>>,
    /// What that firing pass reclaimed, e.g. `"target/ (34.1G)"` or
    /// `"nothing"`.
    pub last_reclaimed: Option<String>,
}

/// A confirmed disagreement between the host identity this daemon resolves for
/// itself and the `host_id` the ingest backend echoes back for the key it
/// authenticated (Issue #4830).
///
/// Filed as a *data* type on the status wire rather than a log-only condition
/// because the 2026-07-31 incident it exists for was invisible for hours: a Mac
/// Studio pushed its whole first night of telemetry under another host's id
/// because the wrong key file had been installed on it, and neither side had any
/// way to notice. The backend cannot notice (a key-bound id is authoritative by
/// design), so the *daemon* is the only party that holds both halves.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ObservabilityHostIdMismatch {
    /// What this daemon calls itself —
    /// [`crate::sweep_registry::host_identity`], resolved with the precedence
    /// `$LOOM_HOST_ID`, then `$HOSTNAME`, then the `hostname` binary, then
    /// `"unknown-host"`. The same value it stamps on every outgoing envelope.
    pub daemon_host_id: String,
    /// The `host_id` the `/ingest` response echoed — the identity the
    /// authenticated key is bound to, i.e. the host every pushed record is
    /// actually being filed under.
    pub ingest_host_id: String,
    /// When the mismatch was first observed this daemon process. Never
    /// re-stamped on subsequent flushes: the WARN and this record are both
    /// once-per-lifetime, so this is the age of the condition, not of the last
    /// flush.
    pub first_seen_at: DateTime<Utc>,
}

/// The one-word answer to "is this host's telemetry landing?" (Issue #5083),
/// derived from [`ObservabilityExportStatus`] by
/// [`ObservabilityExportStatus::classify`].
///
/// Serialized in `snake_case` so a watch loop can assert on it directly, e.g.
/// `loom-daemon status --json | jq -e '.observability_export.state == "healthy"'`.
/// An unknown variant from a *newer* daemon deserializes as
/// [`Self::Unrecognized`] rather than failing the whole status parse.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ObservabilityExportState {
    /// `observability.enabled` is `false` (or the block is absent). Nothing is
    /// being collected and nothing is being sent — a legitimate, deliberate
    /// steady state, not a fault. **Never** reported when `enabled: true`; a
    /// misconfiguration under an explicit opt-in reports [`Self::Misconfigured`]
    /// instead (#5337) — before that fix the two were byte-identical on the
    /// wire, making a bad `ingestKeyFile` path indistinguishable from telemetry
    /// being off by choice.
    #[default]
    Disabled,
    /// `observability.enabled` is `true` but a required piece of config could
    /// not be resolved: no `endpoint`, no `ingestKeyFile`, or the configured
    /// `ingestKeyFile` could not be read (missing, unreadable, or empty after
    /// trimming) — see #5337. Also covers `exporter = "otlp"` requested on a
    /// build without the `otlp` Cargo feature. The exporter never started, so
    /// there is no `started_at`, but this is a **config error an operator
    /// should fix**, not the same benign absence as [`Self::Disabled`].
    /// [`ObservabilityExportStatus::endpoint`] carries whatever *did* resolve
    /// and [`ObservabilityExportStatus::last_failure_detail`] names the
    /// offending path and the underlying error.
    Misconfigured,
    /// The exporter is running but has not had a fair chance to flush yet —
    /// it has been up for less than
    /// [`ObservabilityExportStatus::never_exported_grace_secs`]. Distinguished
    /// from [`Self::NeverExported`] precisely so a freshly-restarted daemon
    /// does not trip a watch loop for the first flush interval.
    Starting,
    /// **The silent failure mode this issue exists for.** The exporter has
    /// been running well past its grace window and has still never had a batch
    /// acked. Before #5083 this was indistinguishable from healthy: no health
    /// section, no status line, and a 0-byte queue file that reads the same
    /// whether it drained or was never written.
    NeverExported,
    /// At least one batch has been acked and the most recent attempt did not
    /// fail. Telemetry is flowing, filed under
    /// [`ObservabilityExportStatus::host_id`].
    Healthy,
    /// Batches are being acked, but the backend echoes a *different* `host_id`
    /// than this daemon reports for itself (#4830) — the records are landing,
    /// under the wrong host. Takes precedence over [`Self::Failing`]: it is a
    /// config-shaped fault that cannot self-recover, whereas a failing flush
    /// usually can.
    HostIdMismatch,
    /// The most recent flush attempt failed (the queue is retrying with
    /// backoff). `last_failure_detail` carries the exporter's own error text;
    /// `last_success_at` says whether this is a regression or has never worked.
    Failing,
    /// A state name this build does not know — a newer daemon reporting to an
    /// older client. Never produced by [`ObservabilityExportStatus::classify`].
    #[serde(other)]
    Unrecognized,
}

impl ObservabilityExportState {
    /// The short, upper-case token the human-readable renderers lead with.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            ObservabilityExportState::Disabled => "disabled",
            ObservabilityExportState::Misconfigured => "MISCONFIGURED",
            ObservabilityExportState::Starting => "starting",
            ObservabilityExportState::NeverExported => "NEVER EXPORTED",
            ObservabilityExportState::Healthy => "OK",
            ObservabilityExportState::HostIdMismatch => "HOST-ID MISMATCH",
            ObservabilityExportState::Failing => "FAILING",
            ObservabilityExportState::Unrecognized => "unrecognized",
        }
    }

    /// Whether this state is a *problem* an operator should act on.
    /// `disabled`, `starting`, and `healthy` are not; the rest are.
    #[must_use]
    pub fn is_problem(self) -> bool {
        matches!(
            self,
            ObservabilityExportState::Misconfigured
                | ObservabilityExportState::NeverExported
                | ObservabilityExportState::HostIdMismatch
                | ObservabilityExportState::Failing
        )
    }
}

/// Positive, always-present state of this daemon's telemetry export (Issue
/// #5083) — the counterpart to [`ObservabilityHostIdMismatch`]'s anomaly-only
/// signal.
///
/// The 2026-08-03 incident this exists for: two hosts with byte-identical
/// observability config, one showing an `observability` health section and one
/// not. The absence was the *only* evidence the second host was fine, which is
/// inference from absence — and the exact same absence would have been shown
/// for a host whose exporter had never successfully sent a single batch.
/// Confirming it took a `daemon.log` grep for the *lack* of a warning.
///
/// Published by [`crate::observability::ExportStatus`] (updated by
/// [`crate::observability::sender::try_flush`] on every attempt) and read back
/// via [`crate::observability::global_export_status`], mirroring the
/// process-global pattern [`ObservabilityHostIdMismatch`] already uses.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ObservabilityExportStatus {
    /// The state as classified by the *daemon* at status-build time. Consumers
    /// that hold their own `now` (e.g. `loom-daemon health`, which stamps one
    /// `at` for the whole report) may re-derive it with [`Self::classify`];
    /// both agree except across the grace boundary.
    ///
    /// `#[serde(default)]` (⇒ `disabled`) so a partial payload from any other
    /// producer still parses rather than failing the whole status read — every
    /// consumer that cares re-derives with [`Self::classify`] anyway.
    #[serde(default)]
    pub state: ObservabilityExportState,
    /// The host identity this daemon stamps on every outgoing envelope —
    /// [`crate::sweep_registry::host_identity`]. `None` when the exporter is
    /// not running. This is the "under which `host_id`" half of the AC.
    #[serde(default)]
    pub host_id: Option<String>,
    /// The `host_id` the ingest backend echoed back, when it *disagrees* with
    /// [`Self::host_id`] — i.e. `Some` exactly when a #4830 mismatch has been
    /// confirmed, so [`Self::classify`] needs no second input. `None`
    /// otherwise, including when the ids agree.
    #[serde(default)]
    pub ingest_host_id: Option<String>,
    /// The configured export endpoint, so an operator can confirm *where* the
    /// data is going without opening the config. `None` when not running.
    #[serde(default)]
    pub endpoint: Option<String>,
    /// Which exporter implementation is running: `"https"` (Loom's native
    /// ingest) or `"otlp"`. `None` when not running.
    #[serde(default)]
    pub exporter: Option<String>,
    /// When the exporter task started this daemon process. The denominator for
    /// "has it had a fair chance to flush yet" — see [`Self::classify`].
    #[serde(default)]
    pub started_at: Option<DateTime<Utc>>,
    /// When a batch was most recently acked by the backend. `None` means *no
    /// batch has ever been acked this process* — the never-exported signal.
    #[serde(default)]
    pub last_success_at: Option<DateTime<Utc>>,
    /// When a flush attempt most recently failed, if ever.
    #[serde(default)]
    pub last_failure_at: Option<DateTime<Utc>>,
    /// The error text of that most recent failure (the exporter's own
    /// `Display`, e.g. `sink rejected batch: HTTP 401 — …`). Never contains
    /// the ingest key: the exporter's errors are built from status codes and
    /// truncated body snippets only.
    ///
    /// Also doubles as the [`ObservabilityExportState::Misconfigured`] detail
    /// (#5337) — the offending config path plus the underlying error (e.g. a
    /// missing-file `io::Error`'s `Display`, which includes the OS errno on
    /// platforms that report one). Same "never the key itself" discipline.
    #[serde(default)]
    pub last_failure_detail: Option<String>,
    /// Total envelopes acked this daemon process. `0` alongside a `Some`
    /// `started_at` is the never-exported signature.
    #[serde(default)]
    pub records_exported: u64,
    /// Consecutive failed flush attempts since the last success (reset to 0 on
    /// any ack). Non-zero ⇒ [`ObservabilityExportState::Failing`].
    #[serde(default)]
    pub consecutive_failures: u32,
    /// The flush cadence this exporter resolved, in seconds — the unit the
    /// grace window is scaled by, so a host configured with a long interval
    /// does not false-alarm as never-exported. `None` when not running.
    #[serde(default)]
    pub flush_interval_secs: Option<u64>,
}

/// Floor on the never-exported grace window — a fresh exporter is never called
/// out before this much wall-clock has passed, regardless of flush cadence.
/// Sized above the collector's own 5-minute host-snapshot interval
/// ([`crate::observability::SNAPSHOT_INTERVAL`]) so a host with no sweep
/// activity at all still has had at least one record enqueued and one flush
/// attempted before the window closes.
pub const NEVER_EXPORTED_GRACE_FLOOR_SECS: u64 = 10 * 60;

impl ObservabilityExportStatus {
    /// The "deliberately off" reading — `observability.enabled` is `false` (or
    /// the block is absent). Distinct from a `None` field, which means "this
    /// daemon binary predates #5083 and cannot tell you", **and** distinct
    /// from [`Self::misconfigured`] (#5337) — `enabled: true` with a config
    /// problem is never reported this way.
    #[must_use]
    pub fn disabled() -> Self {
        Self {
            state: ObservabilityExportState::Disabled,
            ..Self::default()
        }
    }

    /// The "enabled but a required piece of config is missing or unusable"
    /// reading (#5337) — `observability.enabled` is `true` but the exporter
    /// never started because `endpoint`/`ingestKeyFile` did not resolve, or
    /// the ingest key file could not be read. `endpoint` carries whatever
    /// *did* resolve (`None` only when the endpoint itself is what's missing)
    /// so an operator can see where telemetry would have gone; `detail` names
    /// the offending path and the underlying error (never the key itself).
    #[must_use]
    pub fn misconfigured(endpoint: Option<String>, detail: String) -> Self {
        Self {
            state: ObservabilityExportState::Misconfigured,
            endpoint,
            last_failure_detail: Some(detail),
            ..Self::default()
        }
    }

    /// How long a freshly-started exporter is given before a still-empty
    /// success record is called out: three flush intervals, floored at
    /// [`NEVER_EXPORTED_GRACE_FLOOR_SECS`].
    #[must_use]
    pub fn never_exported_grace_secs(&self) -> u64 {
        self.flush_interval_secs
            .unwrap_or(0)
            .saturating_mul(3)
            .max(NEVER_EXPORTED_GRACE_FLOOR_SECS)
    }

    /// Seconds since the last acked batch, as of `now`. `None` when nothing has
    /// ever been acked. Clamped at zero so a small clock skew never renders a
    /// negative age.
    #[must_use]
    pub fn last_success_age_secs(&self, now: DateTime<Utc>) -> Option<u64> {
        self.last_success_at.map(|at| {
            u64::try_from(now.signed_duration_since(at).num_seconds().max(0)).unwrap_or(0)
        })
    }

    /// Seconds the exporter has been running, as of `now`. `None` when it is
    /// not running.
    #[must_use]
    pub fn uptime_secs(&self, now: DateTime<Utc>) -> Option<u64> {
        self.started_at.map(|at| {
            u64::try_from(now.signed_duration_since(at).num_seconds().max(0)).unwrap_or(0)
        })
    }

    /// Derive the state from the recorded facts, as of `now`. Pure, so every
    /// surface (`status`, `health`, the dashboard) reaches the same verdict
    /// from the same wire payload rather than each re-inventing the rules.
    ///
    /// Precedence, most-specific first:
    /// 0. explicitly recorded as misconfigured ⇒ `Misconfigured` (#5337) — the
    ///    exporter never started (no `started_at`), so this has to be checked
    ///    *before* the not-running fallback below or it would silently
    ///    collapse into `Disabled`, which is exactly the bug this precedence
    ///    branch exists to prevent. `self.state` is otherwise never an input
    ///    to this function (every other branch re-derives from the other
    ///    fields) — `Misconfigured` is the one sticky, explicitly-set
    ///    terminal state, since nothing else ever transitions out of it.
    /// 1. not running ⇒ `Disabled`
    /// 2. confirmed id disagreement ⇒ `HostIdMismatch` (config-shaped, cannot
    ///    self-recover — outranks a transient flush failure, whose facts stay
    ///    readable in `last_failure_*` either way)
    /// 3. the last attempt failed ⇒ `Failing`
    /// 4. something has been acked ⇒ `Healthy`
    /// 5. nothing acked, still inside the grace window ⇒ `Starting`
    /// 6. nothing acked, past the grace window ⇒ `NeverExported`
    #[must_use]
    pub fn classify(&self, now: DateTime<Utc>) -> ObservabilityExportState {
        if self.state == ObservabilityExportState::Misconfigured {
            return ObservabilityExportState::Misconfigured;
        }
        let Some(uptime) = self.uptime_secs(now) else {
            return ObservabilityExportState::Disabled;
        };
        if self
            .ingest_host_id
            .as_ref()
            .is_some_and(|ingest| Some(ingest) != self.host_id.as_ref())
        {
            return ObservabilityExportState::HostIdMismatch;
        }
        if self.consecutive_failures > 0 {
            return ObservabilityExportState::Failing;
        }
        if self.last_success_at.is_some() {
            return ObservabilityExportState::Healthy;
        }
        if uptime < self.never_exported_grace_secs() {
            ObservabilityExportState::Starting
        } else {
            ObservabilityExportState::NeverExported
        }
    }
}

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
    /// Issues skipped because they already have an open linked PR.
    pub skipped_pr_open: usize,
    /// Issues skipped because a peer host advertised a live soft claim.
    pub skipped_peer_claim: usize,
    /// Issues skipped inside a per-issue dispatch-backoff window.
    pub skipped_backoff: usize,
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
            (self.skipped_pr_open, "pr-open-skip"),
            (self.skipped_peer_claim, "peer-claim-skip"),
            (self.skipped_backoff, "backoff-skip"),
            (self.deferred_capacity, "deferred-capacity"),
            (self.deferred_ramp_cap, "deferred-ramp"),
            (self.deferred_saturation, "deferred-saturation"),
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
        parts.join(", ")
    }
}

/// One role-runner tick outcome for one `(root, role)` pair (Issue #4761).
///
/// The raw evidence the transient-vs-persistent classifier
/// ([`crate::health::summarize_role_ticks`]) consumes: a failure whose
/// `(root, role)` later ticked successfully **within the same window** was
/// self-recovered (transient) and is deliberately not surfaced; only a
/// `(root, role)` whose *latest* record in the window is a failure is
/// persistent.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RoleTickRecord {
    /// The workspace root this tick ran for.
    pub root: PathBuf,
    /// The role name (`champion`, `curator`, …).
    pub role: String,
    /// When the tick completed.
    pub at: DateTime<Utc>,
    /// Whether the invocation succeeded.
    pub ok: bool,
    /// Short failure detail when `ok` is `false` (the failure reason / runtime
    /// rejection / `no-token-pool` sentinel), else `None`.
    pub detail: Option<String>,
}

/// One `(root, role)` pair's last-observed-tick timestamp (Issue #6201) — see
/// [`DaemonStatusReport::role_last_tick`]'s doc comment for why this is
/// tracked independently of [`RoleTickRecord`]'s bounded ring.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RoleLastTick {
    /// The workspace root this role ticks for.
    pub root: PathBuf,
    /// The role name (`champion`, `curator`, …).
    pub role: String,
    /// When this `(root, role)` pair last completed a tick, of any outcome
    /// (success or failure) — this is a liveness signal, not a health verdict.
    pub at: DateTime<Utc>,
}

/// Live safehouse connection status for `loom-daemon status` (Issue #4345).
/// See [`DaemonStatusReport::safehouse`] and `.loom/docs/safehouse.md`
/// "New-host onboarding" for the operator story.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SafehouseStatus {
    /// One of `"not_configured"`, `"unreachable"`, `"connected"`,
    /// `"send_rejected"` (#4464: handshake succeeds but every `send` is
    /// rejected at the protocol layer, e.g. `'room' required` on a multi-room
    /// host with `safehouse.room` unset).
    pub state: String,
    /// The resolved socket path the daemon last tried/uses, when known.
    /// `None` for `"not_configured"` (including the "enabled but no socket
    /// resolves at all" sub-case, which also reports as `"not_configured"` —
    /// there is nothing to name a path against).
    #[serde(default)]
    pub socket: Option<PathBuf>,
    /// The configured room name, present only when `state == "connected"`.
    /// `None` even when connected if no `safehouse.room` was configured
    /// (valid only when safehoused joined exactly one room, resolved
    /// server-side — this client is never told the resolved name in that
    /// case).
    #[serde(default)]
    pub room: Option<String>,
    /// The rejection reason, present only when `state == "send_rejected"`
    /// (#4464) — the raw `error` string safehoused returned for the rejected
    /// `send` (e.g. `'room' required: 3 rooms joined`). `#[serde(default)]`
    /// keeps pre-#4464 wire payloads (which never carried it) compatible.
    #[serde(default)]
    pub reason: Option<String>,
}

/// Live peer-claim view + transport counters for `loom-daemon status` /
/// `loom-daemon peer-claims` (Issue #5921). Rendered from
/// [`crate::peer_claims::PeerClaimView::to_status`]. See
/// [`DaemonStatusReport::peer_claims`] for the `None`-vs-empty-view contract.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PeerClaimStatus {
    /// This daemon's own host identity — the self-claim-recognition key
    /// (`crate::sweep_registry::host_identity`).
    pub self_host: String,
    /// The configured peer-claim TTL, in seconds
    /// ([`crate::peer_claims::resolve_peer_claim_ttl`]).
    pub ttl_secs: u64,
    /// Every tracked claim (live or not-yet-pruned-expired), sorted by
    /// `(repo, issue)` for a deterministic render.
    pub entries: Vec<PeerClaimEntryStatus>,
    /// How many `Advertise` ads THIS host has published (dispatch-time plus
    /// every reaper re-advertisement heartbeat, #4431).
    pub advertised: u64,
    /// How many peer (non-self) ads this daemon has accepted.
    pub received: u64,
    /// How many entries have aged out past the TTL with no re-advertisement.
    pub expired: u64,
    /// How many dispatches were backed off because this view showed a live
    /// peer claim (the #5789 enforcement path) — the proof the mechanism
    /// actually prevented a duplicate.
    pub dispatch_skipped: u64,
    /// Peer-coordination degradation state (Issue #6157) — see
    /// [`PeerCoordinationHealth`]. `#[serde(default)]` keeps pre-#6157 wire
    /// data / older clients compatible (deserializes as the all-healthy
    /// default).
    #[serde(default)]
    pub coordination: PeerCoordinationHealth,
}

/// Peer-coordination degradation state for `loom-daemon health`'s
/// `peer_coordination` section (Issue #6157): whether this host's
/// peer-claim RECEIVE path currently looks broken — advertising its own
/// claims but not hearing from any peer, the 2026-08-13 incident's exact
/// signature — and, while degraded, how close recovery is. Rendered from
/// [`crate::peer_claims::PeerClaimView`]'s internal tracking; see
/// [`crate::peer_claims::PeerClaimView::evaluate_coordination`] for the
/// decision rule.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct PeerCoordinationHealth {
    /// Whether coordination is currently judged degraded.
    pub degraded: bool,
    /// Seconds since coordination was judged degraded; `None` while healthy.
    pub degraded_for_secs: Option<u64>,
    /// How many consecutive genuine peer receives have landed since going
    /// degraded, toward `recovery_threshold`. Always `0` while healthy.
    pub consecutive_receives_toward_recovery: u64,
    /// How many consecutive receives recovery requires
    /// ([`crate::peer_claims::resolve_coordination_recovery_threshold`]).
    pub recovery_threshold: u64,
}

/// One live peer claim entry within [`PeerClaimStatus::entries`] (Issue
/// #5921).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PeerClaimEntryStatus {
    /// The cross-host-stable repo identity (`crate::peer_claims::repo_slug`)
    /// this claim was advertised under.
    pub repo: String,
    /// The claimed issue number.
    pub issue: u32,
    /// The host holding the claim.
    pub host: String,
    /// How long this entry has left before it expires from THIS daemon's
    /// view. `0` reads as "expired, not yet pruned" rather than absent.
    pub remaining_ttl_secs: u64,
}

/// Host-distress circuit-breaker snapshot for `loom-daemon status` (Issue
/// #4235). Rendered from [`crate::host_breaker::BreakerSnapshot`]. The
/// `daemon.host_breaker.state` event carries the same transition data on every
/// phase change; this is the point-in-time status view.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct HostBreakerStatus {
    /// Whether the breaker is enabled (default ON — a safety backstop).
    pub enabled: bool,
    /// The current phase: `"closed"`, `"open"`, or `"cooldown"`.
    pub phase: String,
    /// Whether the breaker is currently suppressing new dispatch (Open or
    /// CoolDown).
    pub suppressed: bool,
    /// Human-readable reason for the current non-Closed state; `None` while
    /// Closed.
    #[serde(default)]
    pub reason: Option<String>,
    /// When the breaker last tripped to Open; `None` while Closed.
    #[serde(default)]
    pub tripped_at: Option<DateTime<Utc>>,
    /// When the current cool-down completes and normal dispatch resumes; `None`
    /// outside CoolDown.
    #[serde(default)]
    pub releases_at: Option<DateTime<Utc>>,
    /// The most recent load-per-core sample observed; `None` when no
    /// load-average source is available.
    #[serde(default)]
    pub last_load_per_core: Option<f64>,
    /// The configured load-per-core trip threshold.
    pub load_per_core_threshold: f64,
    /// The configured number of consecutive over-threshold ticks needed to trip.
    pub sustain_ticks: u32,
    /// The configured cool-down window, in seconds.
    pub cooldown_secs: u64,
}

/// Saturation admission-brake snapshot for `loom-daemon status` (Issue #4903).
/// Rendered from [`crate::admission_brake::BrakeSnapshot`].
///
/// Distinct from [`HostBreakerStatus`] on purpose: the brake is the
/// **point-in-time** guard that holds new admissions while the host is already
/// saturated and releases the moment it recovers, whereas the breaker is the
/// stateful trip that remembers sustained distress across a cool-down. A host
/// holding admissions must *say so* — before this field, a worker at 12×
/// overcommit reported `capacity_bound: false` and read as idle.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
pub struct AdmissionBrakeStatus {
    /// Whether the brake is enabled (default ON — a safety backstop).
    pub enabled: bool,
    /// Whether **new** sweep admissions are currently held. In-flight sweeps are
    /// never affected by this flag: the brake has no path to running work.
    pub held: bool,
    /// The most recent load-per-core sample observed; `None` when no
    /// load-average source is available (which fails safe to *not* holding).
    #[serde(default)]
    pub load_per_core: Option<f64>,
    /// The configured load-per-core hold threshold.
    pub load_per_core_threshold: f64,
    /// When the current hold streak began; `None` while not holding.
    #[serde(default)]
    pub held_since: Option<DateTime<Utc>>,
    /// How many consecutive ticks the current streak has held; `0` when not
    /// holding.
    #[serde(default)]
    pub held_ticks: u32,
    /// When the current **starvation** streak began — held with **zero**
    /// sweeps in flight, continuously, since this timestamp (Issue #5715).
    /// `None` when not currently starving, including whenever sweeps are
    /// genuinely in flight (healthy backpressure never starves, however long
    /// it holds). `#[serde(default)]` keeps a pre-#5715 wire payload
    /// compatible.
    #[serde(default)]
    pub starving_since: Option<DateTime<Utc>>,
    /// How many consecutive ticks the current starvation streak has held;
    /// `0` when not starving. `#[serde(default)]` keeps a pre-#5715 wire
    /// payload compatible.
    #[serde(default)]
    pub starving_ticks: u32,
    /// Cumulative starvation-escape-hatch grants this daemon process's
    /// lifetime (Issue #5715) — `0` on a healthy host forever; any nonzero
    /// count means the brake has had to force at least one admission through
    /// despite the host still reading saturated. `#[serde(default)]` keeps a
    /// pre-#5715 wire payload compatible.
    #[serde(default)]
    pub escape_hatch_grants: u32,
}

/// GitHub rate-limit circuit-breaker snapshot for `loom-daemon status` (Issue
/// #4429). Rendered from [`crate::rate_limit_breaker::RateLimitSnapshot`]. The
/// `daemon.rate_limit_breaker.state` event carries the transition data on
/// every phase change; this is the point-in-time status view.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RateLimitBreakerStatus {
    /// Whether the breaker is enabled (default ON — a safety backstop).
    pub enabled: bool,
    /// The current phase: `"closed"` or `"cooldown"`.
    pub phase: String,
    /// Whether forge polling is currently suppressed.
    pub suppressed: bool,
    /// Which loop's failure tripped the active cooldown; `None` while Closed.
    #[serde(default)]
    pub source: Option<String>,
    /// When the active cooldown was tripped; `None` while Closed.
    #[serde(default)]
    pub tripped_at: Option<DateTime<Utc>>,
    /// When the active cooldown releases; `None` while Closed.
    #[serde(default)]
    pub cooldown_until: Option<DateTime<Utc>>,
    /// Lifetime trip count for this daemon process.
    pub trips_total: u64,
    /// Last-probed REST core budget remaining (`None` before the first trip —
    /// the budget is probed on trip, never on a status call).
    #[serde(default)]
    pub core_remaining: Option<u64>,
    /// Last-probed GraphQL budget remaining.
    #[serde(default)]
    pub graphql_remaining: Option<u64>,
    /// When the cached budget snapshot was probed.
    #[serde(default)]
    pub budget_probed_at: Option<DateTime<Utc>>,
}

/// Live idle-exit eligibility for `loom-daemon status` (Issue #5565).
/// Rendered from [`crate::idle_exit::IdleExitStatusSnapshot`] — the SAME
/// 0-in-flight / no-active-role / no-lifecycle-activity-within-the-window
/// (or token-starvation) determination `autonomous.idleExit`'s tracker uses,
/// exposed so the fleet cron idle-shutdown guard
/// (`fleet::add_worker::render_idle_shutdown`) can ask the running daemon
/// "are you eligible right now" instead of vetoing on bare `loom-daemon`
/// process presence, which under `Restart=on-success` fleet supervision is
/// essentially always true.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct IdleExitStatus {
    /// Whether `autonomous.idleExit` is enabled (the tracker task is
    /// running) for THIS daemon process. `false` means "cannot determine
    /// eligibility here" — a consumer MUST NOT treat that as "eligible".
    pub enabled: bool,
    /// Whether the tracker would fire an idle-exit right now. Always `false`
    /// while `enabled` is `false`.
    pub eligible: bool,
    /// Which trigger would fire: `"idle"` or `"token_starvation"`; `None`
    /// while not eligible.
    #[serde(default)]
    pub trigger: Option<String>,
    /// The configured idle window, in minutes.
    pub idle_minutes: u64,
    /// The most recently observed in-flight sweep count.
    pub in_flight_sweeps: usize,
    /// The most recently observed active role-run count.
    pub active_role_runs: usize,
    /// The most recently observed healthy-account count.
    pub healthy_tokens: usize,
    /// The most recently observed total-account count.
    pub total_tokens: usize,
    /// Seconds the ordinary-idle clock has been running uninterrupted.
    pub idle_elapsed_secs: u64,
    /// Seconds the starvation clock has been running uninterrupted.
    pub starved_elapsed_secs: u64,
    /// Whether the token-starvation trigger is enabled for this tracker.
    pub starvation_enabled: bool,
    /// Wall-clock time of the tick that produced this snapshot; `None`
    /// before the tracker's first tick.
    #[serde(default)]
    pub observed_at: Option<DateTime<Utc>>,
}

/// A live-locked sweep with no matching [`DaemonStatusReport::in_flight`] entry
/// (Issue #4214). See the field doc on
/// [`DaemonStatusReport::unregistered_locked`] for the full rationale; this is
/// the per-entry shape, carrying enough to locate and reconcile it manually
/// (which root, which issue, which PID is holding the lock).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct UnregisteredLockedSweep {
    /// The workspace root whose `.loom/locks/issue-<N>/` directory this lock
    /// lives under.
    pub root: PathBuf,
    /// The issue number the lock is for (parsed from the `issue-<N>` dir name).
    pub issue: u32,
    /// The lock's `owner_pid`, confirmed alive (`kill(pid, 0)` succeeds) at
    /// snapshot time — a dead-owner lock is stale-lock cleanup territory
    /// (`reconstruct()`), not this diagnostic.
    pub owner_pid: u32,
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
    /// Whether `root` no longer exists on disk (Issue #4326) — e.g. a leaked
    /// or stale registry entry (a scratch dir that was deleted without
    /// `loom-daemon workspace remove`). The work-finder warns-and-skips a
    /// missing root rather than dispatching into it, but never auto-removes
    /// the registration (a root can be transiently absent, e.g. an unmounted
    /// volume), so this flag is `status`'s visible backstop: an operator
    /// seeing `true` here should run `workspace remove <root>` once confirmed
    /// permanent. `#[serde(default)]` keeps pre-#4326 wire data compatible (an
    /// absent field parses as `false`, i.e. "not known to be missing").
    #[serde(default)]
    pub root_missing: bool,
    /// Whether this repo's most recent gate tick DEFERRED for host load (#4259)
    /// — a bounded, load-aware scheduling decision distinct from both
    /// `health_gate_halted` (verified-red) and `health_gate_not_evaluated`
    /// (the gate could not run). A deferred tick leaves the halt flag untouched
    /// and is NOT evidence about `main`. `#[serde(default)]` keeps pre-#4259
    /// wire data compatible.
    #[serde(default)]
    pub health_gate_deferred: bool,
    /// A short `load …` summary of this repo's current load-deferral (#4259),
    /// or `None` when not deferring. See
    /// [`DaemonStatusReport::main_health_gate_deferred_reason`].
    #[serde(default)]
    pub health_gate_deferred_reason: Option<String>,
    /// The tier (`"full"` / `"fast"`) of this repo's most recent completed
    /// verdict (#4259), or `None` before the first — so a fast-tier Green is
    /// never mistaken for a full-suite Green. See
    /// [`DaemonStatusReport::main_health_gate_verdict_tier`].
    #[serde(default)]
    pub health_gate_verdict_tier: Option<String>,
    /// Whether the periodic support-role runner (Issue #4015) is enabled for
    /// **this** root's own `.loom/config.json` (Issue #4377) —
    /// `role_runner::resolve_enabled(role_runner::read_role_runner_config(root))`,
    /// resolved daemon-side, never the CLI client's environment. This is a
    /// **per-root** gate, independent of whether the daemon's own workspace
    /// happens to have `autonomous.roleRunner.enabled: true` — a registered
    /// workspace can be `false` here even while the daemon's own workspace is
    /// `true`, which was previously undiagnosable without reading that root's
    /// config file directly (the motivating incident: 9 of 10 registered
    /// workspaces silently receiving zero role ticks). `#[serde(default)]`
    /// keeps pre-#4377 wire data / older daemon binaries compatible (an
    /// absent field parses as `false`).
    #[serde(default)]
    pub role_runner_enabled: bool,
    /// The interval-loop roles this root would dispatch if
    /// [`Self::role_runner_enabled`] were `true` (Issue #4377) —
    /// `role_runner::resolve_roles(..)` names, in [`crate::role_runner::DEFAULT_ROLES`]
    /// order. Populated even when disabled, so `loom-daemon status` can show
    /// *what* is being suppressed, not just *that* it is. `#[serde(default)]`
    /// keeps pre-#4377 wire data compatible (an absent field parses as an
    /// empty vec).
    #[serde(default)]
    pub role_runner_roles: Vec<String>,
    /// This root's `autonomous.roleRunner.onIdle` roles (Issue #4377) —
    /// `role_runner::resolve_on_idle_roles(..)` names. A non-empty value here
    /// combined with [`Self::role_runner_enabled`] being `false` is exactly
    /// the silent-no-op this issue fixes: `onIdle` configured, but the
    /// per-root gate off, so it never fires. `#[serde(default)]` keeps
    /// pre-#4377 wire data compatible (an absent field parses as an empty
    /// vec).
    #[serde(default)]
    pub role_runner_on_idle_roles: Vec<String>,
    /// This root's OWN resolved token-pool directory (Issue #5269) —
    /// `tokens_pool::paths::resolve_tokens_dir(&root)`, the exact resolution
    /// [`crate::token_ranking_refresh`]'s self-refresh loop already uses to
    /// decide which pool to refresh for this repo. Deliberately **not** the
    /// anchored/CWD-based resolution the top-level
    /// [`DaemonStatusReport::token_pool_dir`] uses — that field answers "which
    /// pool is the daemon's own primary workspace's", which for a multi-repo
    /// daemon can be a different repo's pool entirely. `#[serde(default)]`
    /// keeps pre-#5269 wire data compatible (an absent field parses as
    /// `None`).
    #[serde(default)]
    pub token_pool_dir: Option<PathBuf>,
    /// Whether this root's own resolved pool (`Self::token_pool_dir`) has a
    /// `.ranking` file (Issue #5269) — same semantics as the top-level
    /// [`DaemonStatusReport`]'s `health`/`capacity` staleness inputs, just
    /// scoped to this repo's own pool instead of the daemon's anchored
    /// primary workspace. `#[serde(default)]` keeps pre-#5269 wire data
    /// compatible (an absent field parses as `false`).
    #[serde(default)]
    pub ranking_present: bool,
    /// Age in seconds of this root's own `.ranking`, when readable (Issue
    /// #5269). `#[serde(default)]` keeps pre-#5269 wire data compatible (an
    /// absent field parses as `None`).
    #[serde(default)]
    pub ranking_age_secs: Option<u64>,
    /// Total `refs/stash` entries in this repo (Issue #5692) — the
    /// fleet-wide status counterpart of `check-quarantine-stashes.sh`'s
    /// (#5185) single-repo, single-host `git stash list` enumeration.
    /// `#[serde(default)]` keeps pre-#5692 wire data compatible (an absent
    /// field parses as `0`).
    #[serde(default)]
    pub stash_total_count: usize,
    /// Of `Self::stash_total_count`, how many carry the
    /// [`crate::quarantine_stash_status::QUARANTINE_STASH_LABEL`] — the
    /// subset `check-main-clean.sh --quarantine` created to rescue
    /// contaminated main-worktree changes, as opposed to an ad-hoc `git
    /// stash` (a Judge park, an Auditor drift-stash, etc.). `#[serde(default)]`
    /// keeps pre-#5692 wire data compatible.
    #[serde(default)]
    pub stash_quarantine_count: usize,
    /// Age, in seconds, of the OLDEST entry in this repo's `refs/stash` (any
    /// label) at snapshot time (Issue #5692) — `None` when there are no
    /// stashes at all. `#[serde(default)]` keeps pre-#5692 wire data
    /// compatible.
    #[serde(default)]
    pub stash_oldest_age_secs: Option<u64>,
    /// Whether `root` is missing `.claude/commands/loom/sweep.md` (Issue
    /// #5682) — i.e. `!SweepRegistryConfig::new(root).has_sweep_command()`,
    /// recomputed live at every status snapshot (not just once at
    /// `workspace add` time), so a root that loses `sweep.md` after
    /// registration (deleted, or never installed post-clone) is also caught.
    /// `dispatch()` already refuses this root unconditionally
    /// (`sweep_registry/dispatch.rs`); this field is what lets
    /// `loom-daemon status` render that undispatchable-forever state as
    /// visibly distinct from a healthy idle repo instead of `GATE disabled` /
    /// `ROLES on` — indistinguishable from "no work" — forever. `#[serde(default)]`
    /// keeps pre-#5682 wire data compatible (an absent field parses as
    /// `false`, i.e. "not known to be missing").
    #[serde(default)]
    pub sweep_command_missing: bool,
}

/// One active insta-crash quarantine (Issue #4215), as surfaced by
/// `loom-daemon quarantine list` / [`Request::ListQuarantines`]. Joins the
/// three pieces of quarantine state [`crate::sweep_registry::SweepRegistry`]
/// already tracks in-memory (`quarantined`, `insta_crash_counts`,
/// `quarantine_config.ttl`) into one read-only row.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct QuarantineEntry {
    /// The quarantined issue number.
    pub issue: u32,
    /// The workspace whose registry this quarantine lives in — meaningful once
    /// `ListQuarantines` enumerates across every registered workspace.
    pub workspace_root: PathBuf,
    /// When the quarantine was applied.
    pub quarantined_at: DateTime<Utc>,
    /// The consecutive-insta-crash tally that triggered (or is at) quarantine.
    pub insta_crash_count: u32,
    /// The consecutive-insta-crash threshold configured for this issue's
    /// workspace, so the CLI can render "tally / threshold" instead of a bare
    /// count. Read from the same per-workspace [`crate::sweep_registry::
    /// QuarantineConfig`] the reaper enforces against — different managed
    /// workspaces may configure different thresholds.
    pub insta_crash_threshold: u32,
    /// Seconds remaining before TTL auto-release, clamped to `0` — the TTL is
    /// enforced only by [`crate::sweep_registry::SweepRegistry::reap_once`], so
    /// an entry can be momentarily past-TTL between reaper ticks; a negative
    /// remainder would be a confusing thing to render.
    pub ttl_remaining_secs: u64,
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
    /// The health-adjusted token-axis limit: `healthy_accounts` when a ranking
    /// exists, else the raw token-pool size. The token axis no longer feeds the
    /// dynamic concurrency cap (`min(disk headroom, ram headroom,
    /// configured_max)`, removed in #5270) — this field is informational only,
    /// reported here because it still drives spawn-time account *selection*
    /// (prefer healthier accounts) elsewhere in the daemon.
    pub token_axis_limit: usize,
    /// Whether the account pool is genuinely starved — zero healthy accounts to
    /// select from at spawn time. Since #5270/#5305 this does **not** mean
    /// tokens are the binding constraint on the dynamic cap (they no longer
    /// participate in it at all); it only surfaces the add-accounts advisory
    /// when the pool itself has run dry.
    pub token_bound: bool,
}

/// Startup forge-credential preflight snapshot (#4005; GitHub App identity
/// mechanism added by #4430) — see [`crate::credential_preflight`] for the
/// resolution logic. Resolved exactly once, before the daemon's first `gh`
/// consumer, so headless/SSH-only operation (no unlockable GUI login
/// keychain) is diagnosed loudly at boot instead of surfacing as an
/// unexplained per-tick `401` on every forge call.
///
/// GitHub-only: the daemon's own forge calls all shell out to `gh`, which
/// resolves GitHub credentials exclusively (whether that's an ambient
/// `GH_TOKEN`/keyring credential, or a `GH_TOKEN` this process minted itself
/// via the `"github-app"` mechanism below and exported into its own
/// environment). `GITEA_TOKEN`/`FORGE_TOKEN` forwarding (`loom-daemon-start.sh`)
/// exists only for dispatched sweep children targeting a Gitea-backed repo —
/// the daemon process itself never calls a Gitea API, so there is nothing
/// here to preflight for it.
///
/// **Never carries a token value** — only a resolution-mechanism label and a
/// non-secret fingerprint (last 4 chars of an env-sourced token, the
/// authenticated `gh` login for a credential-store resolution, or `app
/// <id> installation <id>` for the GitHub App mechanism — never the minted
/// token itself).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CredentialPreflightReport {
    /// `true` when a usable GitHub credential was resolved; `false` when
    /// none was found, or the probe itself could not complete within its
    /// bound (`gh` missing from `PATH`, timed out, spawn failure, …) — see
    /// [`Self::mechanism`] `"unknown"` for that latter case.
    pub ok: bool,
    /// Which mechanism resolved the credential: `"GH_TOKEN"` / `"GITHUB_TOKEN"`
    /// (the daemon's own process environment) or `gh`'s own `tokenSource`
    /// (e.g. `"keyring"`, `"oauth_token"`) reported by `gh auth status`;
    /// `"github-app"` when a GitHub App installation token was minted and
    /// exported as `GH_TOKEN` (#4430 — see
    /// [`crate::credential_preflight::run_with_github_app`]).
    /// `"none"` when nothing resolved; `"unknown"` when the probe itself
    /// failed to run. NEVER a token value.
    pub mechanism: String,
    /// A non-secret fingerprint: the last 4 characters of an env-sourced
    /// token, the authenticated `gh` login for a credential-store
    /// resolution, or `app <id> installation <id>` for the `"github-app"`
    /// mechanism. `None` when no credential resolved.
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
    /// `sweep.issue.{N}.exited` — reaper detected clean exit (no checkpoint).
    SweepExited {
        issue: u32,
        #[serde(skip_serializing_if = "Option::is_none")]
        exit_code: Option<i32>,
        duration_sec: i64,
        /// Issue #4366: classifies this checkpoint-less exit as a genuine
        /// no-lifecycle-progress death (`true`) vs an ordinary/benign exit
        /// (`false`) — e.g. a legitimate self-skip / already-done no-work
        /// exit, or a clean exit that produced an open linked PR or a closed
        /// issue. `true` requires ALL of: `exit_code == Some(0)`, no open
        /// linked PR, and the issue *verifiably* open (a positive "open"
        /// verdict — a failed/timed-out forge probe fails open and yields
        /// `false`, per PR #4408's review) — the reaper counts a `true`
        /// verdict toward the insta-crash quarantine tally instead of
        /// resetting it, so a headless child that repeatedly parks on a
        /// monitored background task and exits 0 (the observed "cache
        /// download is running in the background... I'll pick this back up"
        /// signature) can no longer churn the dispatch queue forever without
        /// ever being counted as a failure. `#[serde(default)]` keeps
        /// pre-#4366 wire data compatible (defaults to `false`, the prior
        /// behavior).
        #[serde(default)]
        no_progress: bool,
        /// Issue #4386: set to a claude-wrapper pre-flight-death label (e.g.
        /// `"preflight-mcp-failed"`, `"preflight-no-cli-start"`) when the
        /// reaper's [`crate::sweep_registry::classify_preflight_death`]-derived
        /// classification matched this dead sweep's log tail. `None` for every
        /// other exit (including the common clean self-skip/no-work case).
        /// This is an additive payload extension of the existing frozen
        /// `sweep.issue.{N}.exited` topic — the topic string is unchanged.
        /// `#[serde(default)]` keeps pre-#4386 wire data / older clients
        /// compatible.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        death_class: Option<String>,
        /// Owning managed-workspace root (Issue #3929). See [`Self::SweepPhase`].
        #[serde(default, skip_serializing_if = "Option::is_none")]
        repo: Option<String>,
    },
    /// `sweep.issue.{N}.crashed` — reaper detected dead pid + checkpoint.
    SweepCrashed {
        issue: u32,
        #[serde(skip_serializing_if = "Option::is_none")]
        checkpoint_phase: Option<String>,
        /// Issue #4255: the reaper's best-effort error classification derived
        /// from the dead sweep's log tail + exit code (e.g. `execution-error`,
        /// `account-exhausted:rate-limited`, `exit-<code>`). Carried alongside
        /// `checkpoint_phase` so a subscriber (#4137 durable telemetry) can
        /// attribute WHY a sweep died, not just which phase it reached. `None`
        /// when the log yields no recognizable signature (or is unreadable).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        classification: Option<String>,
        /// Issue #4386: set to a claude-wrapper pre-flight-death label when
        /// this dead sweep's log tail matched the pre-flight classifier —
        /// same additive-payload-extension rationale as `SweepExited`'s
        /// `death_class` field, applied here too because a stale checkpoint
        /// from an earlier dispatch can route a pre-flight death through this
        /// Crashed branch instead of `SweepExited`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        death_class: Option<String>,
        /// Owning managed-workspace root (Issue #3929). See [`Self::SweepPhase`].
        #[serde(default, skip_serializing_if = "Option::is_none")]
        repo: Option<String>,
    },
    /// `sweep.issue.{N}.resume_dispatched` — reaper-driven resume (Issue
    /// #4256). A crashed sweep's issue had an open linked PR plus a
    /// Builder-or-later checkpoint, so the reaper re-dispatched the SAME
    /// issue with the #4123 open-PR guard bypassed for that one resume — the
    /// checkpoint-resume machinery (#3373) then picks back up at the correct
    /// phase (typically Judge) instead of redoing the Builder. This is the
    /// one dispatch path that proceeds despite an open PR; every other
    /// dispatch caller still refuses via `OpenPrDispatchError`, so #4123's
    /// anti-duplicate property is unchanged.
    SweepResumeDispatched {
        issue: u32,
        /// The open linked PR the resume targets.
        pr: u32,
        /// The checkpoint phase the crashed sweep last recorded.
        #[serde(skip_serializing_if = "Option::is_none")]
        checkpoint_phase: Option<String>,
        /// Whether the resume dispatch itself succeeded (spawned a child).
        /// `false` means the reaper attempted recovery but the dispatch call
        /// failed (e.g. a spawn error) — still emitted so the attempt is
        /// visible on the event bus even when the retry itself fails.
        dispatched: bool,
        /// Owning managed-workspace root (Issue #3929). See [`Self::SweepPhase`].
        #[serde(default, skip_serializing_if = "Option::is_none")]
        repo: Option<String>,
    },
    /// `sweep.global.dispatch` — daemon dispatched a new sweep.
    SweepGlobalDispatch {
        sweep_id: SweepId,
        kind: SweepKind,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        runtime: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        runtime_source: Option<RuntimeSource>,
        /// Owning managed-workspace root (Issue #3929's pattern, extended here
        /// by #4201). This was the one sweep-scoped variant that did **not**
        /// carry `repo` — the safehouse narration sink needs it to
        /// repo-qualify this event's `task_id` (a bare issue number collides
        /// across managed repos, e.g. loom #N vs vibesql #N narrating into the
        /// same Matrix thread). `#[serde(default)]` keeps pre-#4201 wire data
        /// compatible.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        repo: Option<String>,
    },
    /// `sweep.global.runtime_rejected` — a dispatch was **refused** by
    /// fail-closed runtime admission (issue #4494, epic #4489 Phase 5), before
    /// any claim lock, forge mutation, account selection, log header, or child
    /// spawn. `SweepGlobalDispatch` only exists for *admitted* work, so without
    /// this variant refused work had no event representation at all.
    ///
    /// The payload is deliberately the same structured, secret-free shape the
    /// typed [`RuntimeRejection`] carries over IPC (role / runtime / source /
    /// unmet capability names / reason) — no token name, account, credential,
    /// or log path is included, and emitting it introduces **no** claim,
    /// account, or log side effect (it is a pure event publish).
    SweepGlobalRuntimeRejected {
        /// The refused work item (issue or PR set) — there is no `sweep_id`,
        /// because no sweep was ever created.
        kind: SweepKind,
        /// Canonical role/lifecycle the admission decision was made for
        /// (`sweep-lifecycle` for a full sweep).
        role: String,
        /// Runtime that was resolved and then refused.
        runtime: String,
        /// Precedence tier that selected `runtime`.
        runtime_source: RuntimeSource,
        /// Named capabilities the runtime failed to declare as exactly `"yes"`.
        /// Empty for config/adapter/manifest-shaped refusals.
        #[serde(default)]
        unmet_capabilities: Vec<String>,
        /// Operator-facing rejection reason (the [`RuntimeRejection`] reason).
        reason: String,
        /// Owning managed-workspace root (Issue #3929's pattern). Stamped by
        /// `SweepRegistry::emit_event` -> [`Self::set_repo_if_absent`].
        #[serde(default, skip_serializing_if = "Option::is_none")]
        repo: Option<String>,
    },
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
    /// `daemon.preflight.advisory` — a workspace's reaper crossed (or cleared)
    /// the consecutive claude-wrapper pre-flight-death threshold (Issue
    /// #4386): N dispatches in a row, across *different* issues, died at the
    /// wrapper's MCP-init pre-flight check before ever reaching `# CLAUDE_CLI_
    /// START` (the classic stale-`.mcp.json` fleet-wide silent failure).
    /// Published by [`crate::sweep_registry::SweepRegistry`]'s reaper on
    /// **state change only** (entered/left the tripped state), mirroring
    /// `daemon.capacity.advisory`'s dedup discipline — never every tick. This
    /// issue (#4386) is the authorizing issue for this topic per the frozen-
    /// taxonomy "new topics require a follow-up issue" rule (see
    /// `event_bus.rs`'s module doc). Advisory only — it never blocks
    /// dispatch; it tells the operator to check `.mcp.json`.
    PreflightAdvisory {
        /// The workspace root whose reaper tripped (or cleared) the advisory.
        workspace_root: String,
        /// Consecutive pre-flight deaths observed at the transition.
        consecutive_deaths: u32,
        /// The most recent matched pre-flight death-class marker (e.g.
        /// `"preflight-mcp-failed"`), or empty on the clearing transition.
        marker: String,
        /// Operator-facing advisory message naming the concrete cause.
        message: String,
    },
    /// `daemon.idle_exit` — the daemon is cleanly yielding to a host
    /// idle-shutdown guard (Issue #4467).
    DaemonIdleExit {
        trigger: String,
        idle_minutes: u64,
        in_flight_sweeps: usize,
        active_role_runs: usize,
        healthy_tokens: usize,
        total_tokens: usize,
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
    /// | `SweepResumeDispatched {issue, ..}` | `sweep.issue.{issue}.resume_dispatched` |
    /// | `SweepGlobalDispatch {..}` | `sweep.global.dispatch` |
    /// | `SweepGlobalRuntimeRejected {..}` | `sweep.global.runtime_rejected` |
    /// | `SweepGlobalCompleted {..}` | `sweep.global.completed` |
    /// | `EpicAction {epic, action, ..}` | `epic.issue.{epic}.{action}` |
    /// | `CapacityAdvisory {..}` | `daemon.capacity.advisory` |
    /// | `PreflightAdvisory {..}` | `daemon.preflight.advisory` |
    /// | `TopicLag {..}` | `sweep.system.topic_lag` |
    /// | `Generic {topic, ..}` | the explicit topic string |
    ///
    /// The `SweepPhase` / `SweepBlocker` / `SweepExited` / `SweepCrashed`
    /// variants (Issue #3929), plus `SweepGlobalDispatch` (#4201), also carry a
    /// `repo` payload field so a subscriber on the shared `sweep.issue.{N}.*`
    /// bus (or the global dispatch topic) can disambiguate two managed repos'
    /// issue #N. The topic **strings are unchanged** — `repo` lives in the
    /// payload only, so existing single-repo subscribers route identically.
    #[must_use]
    pub fn topic(&self) -> String {
        match self {
            Self::SweepPhase { issue, .. } => format!("sweep.issue.{issue}.phase"),
            Self::SweepBlocker { issue, .. } => format!("sweep.issue.{issue}.blocker"),
            Self::SweepExited { issue, .. } => format!("sweep.issue.{issue}.exited"),
            Self::SweepCrashed { issue, .. } => format!("sweep.issue.{issue}.crashed"),
            Self::SweepResumeDispatched { issue, .. } => {
                format!("sweep.issue.{issue}.resume_dispatched")
            }
            Self::SweepGlobalDispatch { .. } => "sweep.global.dispatch".to_string(),
            Self::SweepGlobalRuntimeRejected { .. } => "sweep.global.runtime_rejected".to_string(),
            Self::SweepGlobalCompleted { .. } => "sweep.global.completed".to_string(),
            Self::EpicAction { epic, action, .. } => {
                format!("epic.issue.{epic}.{}", action.as_str())
            }
            Self::CapacityAdvisory { .. } => "daemon.capacity.advisory".to_string(),
            Self::PreflightAdvisory { .. } => "daemon.preflight.advisory".to_string(),
            Self::DaemonIdleExit { .. } => "daemon.idle_exit".to_string(),
            Self::TopicLag { .. } => "sweep.system.topic_lag".to_string(),
            Self::Generic { topic, .. } => topic.clone(),
        }
    }

    /// Stamp the owning managed-workspace `repo` into the sweep-scoped event
    /// variants (Issue #3929, extended to `SweepGlobalDispatch` by #4201), but
    /// only when the field is still absent — a caller that already knows the
    /// repo (e.g. a `PublishEvent` payload from a sweep child running in a
    /// specific workspace) is never overwritten.
    ///
    /// `SweepGlobalCompleted` (already carries a unique `sweep_id`), `EpicAction`,
    /// `CapacityAdvisory`, `PreflightAdvisory` (already carries its own
    /// `workspace_root` field), `TopicLag`, and `Generic` are left untouched. Called
    /// centrally from `SweepRegistry::emit_event` so every emitted sweep event
    /// is stamped with its registry's `workspace_root` without touching each
    /// construction site.
    pub fn set_repo_if_absent(&mut self, repo: &str) {
        let slot = match self {
            Self::SweepPhase { repo, .. }
            | Self::SweepBlocker { repo, .. }
            | Self::SweepExited { repo, .. }
            | Self::SweepCrashed { repo, .. }
            | Self::SweepResumeDispatched { repo, .. }
            | Self::SweepGlobalDispatch { repo, .. }
            | Self::SweepGlobalRuntimeRejected { repo, .. } => repo,
            _ => return,
        };
        if slot.is_none() {
            *slot = Some(repo.to_string());
        }
    }

    /// Build the bus event for a child-published `PublishEvent { topic, payload }`
    /// request (Issue #4466).
    ///
    /// The two **child-published** sweep topics — `sweep.issue.{N}.phase` and
    /// `sweep.issue.{N}.blocker` — are upgraded to their typed [`Event`]
    /// variants ([`Event::SweepPhase`] / [`Event::SweepBlocker`]) when the topic
    /// parses and the payload matches the documented schema
    /// (`defaults/.claude/commands/loom/sweep.md`). This is what lets the
    /// narration sink ([`crate::safehouse::event_to_envelope`]) emit the
    /// documented `task` / `handoff` room lines — a raw [`Event::Generic`] is
    /// never narrated, so before this upgrade the child's phase/blocker lines
    /// silently never reached the room.
    ///
    /// Publish is **fire-and-forget advisory**: an unknown topic, a
    /// non-integer issue segment, or a payload that does not match the
    /// documented schema is never rejected — it falls through to
    /// [`Event::Generic`] with the topic + payload passed through **unchanged**
    /// (byte-for-byte the pre-#4466 behavior for everything except the two
    /// documented topics). Unknown fields in an otherwise-valid payload are
    /// ignored for forward-compatibility.
    #[must_use]
    pub fn from_published(topic: String, payload: serde_json::Value) -> Self {
        match Self::try_typed_child_event(&topic, &payload) {
            Some(event) => event,
            None => Self::Generic { topic, payload },
        }
    }

    /// Attempt to parse a child-published `topic` + `payload` into a typed
    /// sweep event. Returns `None` (→ caller keeps it [`Event::Generic`]) for
    /// any topic outside the two documented child-published topics, or for a
    /// payload that does not match the documented schema. See
    /// [`Self::from_published`].
    fn try_typed_child_event(topic: &str, payload: &serde_json::Value) -> Option<Self> {
        // Segment-aligned parse of `sweep.issue.{N}.{kind}` — exactly four
        // segments, matching the bus's segment-aligned prefix routing (so
        // `sweep.issuetype.foo` is NOT mistaken for a sweep-issue topic).
        let segments: Vec<&str> = topic.split('.').collect();
        if segments.len() != 4 || segments[0] != "sweep" || segments[1] != "issue" {
            return None;
        }
        let issue: u32 = segments[2].parse().ok()?;

        match segments[3] {
            "phase" => {
                #[derive(Deserialize)]
                struct PhasePayload {
                    phase: String,
                    #[serde(default)]
                    pr_number: Option<i32>,
                    #[serde(default)]
                    repo: Option<String>,
                }
                let p: PhasePayload = serde_json::from_value(payload.clone()).ok()?;
                Some(Self::SweepPhase {
                    issue,
                    phase: p.phase,
                    pr_number: p.pr_number,
                    repo: p.repo,
                })
            }
            "blocker" => {
                #[derive(Deserialize)]
                struct BlockerPayload {
                    reason: String,
                    label_added: String,
                    #[serde(default)]
                    repo: Option<String>,
                }
                let p: BlockerPayload = serde_json::from_value(payload.clone()).ok()?;
                Some(Self::SweepBlocker {
                    issue,
                    reason: p.reason,
                    label_added: p.label_added,
                    repo: p.repo,
                })
            }
            _ => None,
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
            no_progress: false,
            death_class: None,
            repo: None,
        };
        assert_eq!(exited.topic(), "sweep.issue.7.exited");

        let crashed = Event::SweepCrashed {
            issue: 7,
            checkpoint_phase: Some("doctor".to_string()),
            classification: None,
            death_class: None,
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

    // ---- Issue #4466: child-published topics upgrade to typed variants ----

    #[test]
    fn from_published_upgrades_phase_topic() {
        let ev = Event::from_published(
            "sweep.issue.123.phase".to_string(),
            serde_json::json!({"phase": "builder", "pr_number": 42, "repo": "/work/loom"}),
        );
        match ev {
            Event::SweepPhase {
                issue,
                phase,
                pr_number,
                repo,
            } => {
                assert_eq!(issue, 123);
                assert_eq!(phase, "builder");
                assert_eq!(pr_number, Some(42));
                assert_eq!(repo.as_deref(), Some("/work/loom"));
            }
            other => panic!("expected SweepPhase, got {other:?}"),
        }
    }

    #[test]
    fn from_published_upgrades_phase_topic_minimal_payload() {
        // Only the required `phase` field — optionals default to None.
        let ev = Event::from_published(
            "sweep.issue.7.phase".to_string(),
            serde_json::json!({"phase": "curator"}),
        );
        match ev {
            Event::SweepPhase {
                issue,
                phase,
                pr_number,
                repo,
            } => {
                assert_eq!(issue, 7);
                assert_eq!(phase, "curator");
                assert!(pr_number.is_none());
                assert!(repo.is_none());
            }
            other => panic!("expected SweepPhase, got {other:?}"),
        }
    }

    #[test]
    fn from_published_upgrades_blocker_topic() {
        let ev = Event::from_published(
            "sweep.issue.456.blocker".to_string(),
            serde_json::json!({"reason": "needs human", "label_added": "loom:blocked"}),
        );
        match ev {
            Event::SweepBlocker {
                issue,
                reason,
                label_added,
                repo,
            } => {
                assert_eq!(issue, 456);
                assert_eq!(reason, "needs human");
                assert_eq!(label_added, "loom:blocked");
                assert!(repo.is_none());
            }
            other => panic!("expected SweepBlocker, got {other:?}"),
        }
    }

    #[test]
    fn from_published_keeps_generic_for_unknown_and_malformed() {
        // Malformed payload (missing required field), unknown sub-topic,
        // non-integer issue, and unrelated topics all stay Generic with the
        // payload passed through unchanged.
        let cases: &[(&str, serde_json::Value)] = &[
            ("sweep.issue.1.phase", serde_json::json!({"pr_number": 5})),
            ("sweep.issue.1.blocker", serde_json::json!({"reason": "x"})),
            ("sweep.issue.1.other", serde_json::json!({"phase": "builder"})),
            ("sweep.issue.abc.phase", serde_json::json!({"phase": "builder"})),
            ("sweep.issuetype.foo", serde_json::json!({"phase": "builder"})),
            ("custom.topic", serde_json::json!({"k": "v"})),
        ];
        for (topic, payload) in cases {
            let ev = Event::from_published((*topic).to_string(), payload.clone());
            match ev {
                Event::Generic {
                    topic: got_topic,
                    payload: got_payload,
                } => {
                    assert_eq!(&got_topic, topic);
                    assert_eq!(&got_payload, payload, "payload unchanged for {topic}");
                }
                other => panic!("expected Generic for {topic}, got {other:?}"),
            }
        }
    }

    #[test]
    fn from_published_phase_rejects_wrong_typed_pr_number() {
        // A non-integer `pr_number` is a malformed payload → stays Generic.
        let ev = Event::from_published(
            "sweep.issue.1.phase".to_string(),
            serde_json::json!({"phase": "builder", "pr_number": "not-an-int"}),
        );
        assert!(matches!(ev, Event::Generic { .. }));
    }

    #[test]
    fn set_repo_if_absent_stamps_upgraded_child_events() {
        // Issue #4466: an upgraded child-published event with no `repo` in its
        // payload is stampable via the same `set_repo_if_absent` path the
        // daemon uses for its own sweep events.
        let mut phase = Event::from_published(
            "sweep.issue.9.phase".to_string(),
            serde_json::json!({"phase": "judge"}),
        );
        phase.set_repo_if_absent("/repos/stamped");
        match &phase {
            Event::SweepPhase { repo, .. } => assert_eq!(repo.as_deref(), Some("/repos/stamped")),
            other => panic!("expected SweepPhase, got {other:?}"),
        }

        let mut blocker = Event::from_published(
            "sweep.issue.9.blocker".to_string(),
            serde_json::json!({"reason": "x", "label_added": "loom:blocked"}),
        );
        blocker.set_repo_if_absent("/repos/stamped");
        match &blocker {
            Event::SweepBlocker { repo, .. } => assert_eq!(repo.as_deref(), Some("/repos/stamped")),
            other => panic!("expected SweepBlocker, got {other:?}"),
        }
    }

    #[test]
    fn set_repo_if_absent_stamps_only_when_empty_and_only_sweep_scoped() {
        // Stamps when absent.
        let mut ev = Event::SweepExited {
            issue: 1,
            exit_code: None,
            duration_sec: 0,
            no_progress: false,
            death_class: None,
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

    // ---- Issue #4326: RepoStatus `root_missing` is additive/backward-compatible ----

    fn sample_repo_status(root_missing: bool) -> RepoStatus {
        RepoStatus {
            root: PathBuf::from("/repos/gamma"),
            priority: 100,
            in_flight_count: 0,
            health_gate_halted: false,
            quarantined_issues: vec![],
            health_gate_not_evaluated: false,
            health_gate_not_evaluated_reason: None,
            health_gate_enabled: Some(true),
            health_gate_verdict_at: None,
            root_missing,
            health_gate_deferred: false,
            health_gate_deferred_reason: None,
            health_gate_verdict_tier: None,
            role_runner_enabled: false,
            role_runner_roles: vec![],
            role_runner_on_idle_roles: vec![],
            token_pool_dir: None,
            ranking_present: false,
            ranking_age_secs: None,
            stash_total_count: 0,
            stash_quarantine_count: 0,
            stash_oldest_age_secs: None,
            sweep_command_missing: false,
        }
    }

    #[test]
    fn repo_status_root_missing_round_trips_through_serde() {
        let status = sample_repo_status(true);
        let json = serde_json::to_string(&status).unwrap();
        assert!(json.contains("\"root_missing\":true"));
        let back: RepoStatus = serde_json::from_str(&json).unwrap();
        assert!(back.root_missing);
    }

    #[test]
    fn repo_status_root_missing_defaults_to_false_for_pre_4326_wire_data() {
        // A payload emitted before #4326 has no `root_missing` key; it must
        // still parse — old daemons stay wire-compatible with a newer CLI,
        // and the field defaults to "not known to be missing" rather than
        // failing to deserialize.
        let json = r#"{
            "root":"/repos/delta",
            "priority":100,
            "in_flight_count":0,
            "health_gate_halted":false,
            "quarantined_issues":[],
            "health_gate_not_evaluated":false,
            "health_gate_enabled":true
        }"#;
        let status: RepoStatus = serde_json::from_str(json).unwrap();
        assert!(!status.root_missing);
    }

    // ==================================================================
    // Observability export status (Issue #5083)
    // ==================================================================

    fn export_now() -> DateTime<Utc> {
        "2026-08-03T12:00:00Z".parse().unwrap()
    }

    /// A running exporter that started `uptime_secs` ago on the default
    /// 30s cadence and has never been touched by a flush attempt.
    fn running_exporter(uptime_secs: i64) -> ObservabilityExportStatus {
        ObservabilityExportStatus {
            state: ObservabilityExportState::Starting,
            host_id: Some("robb-studio".to_string()),
            ingest_host_id: None,
            endpoint: Some("https://dashboard.example/ingest".to_string()),
            exporter: Some("https".to_string()),
            started_at: Some(export_now() - chrono::Duration::seconds(uptime_secs)),
            last_success_at: None,
            last_failure_at: None,
            last_failure_detail: None,
            records_exported: 0,
            consecutive_failures: 0,
            flush_interval_secs: Some(30),
        }
    }

    #[test]
    fn an_unstarted_exporter_classifies_as_disabled() {
        // The "observability off / keyless / under-configured" reading — a real
        // answer, materially different from a `None` field on the wire.
        let status = ObservabilityExportStatus::disabled();
        assert_eq!(status.classify(export_now()), ObservabilityExportState::Disabled);
        assert!(!ObservabilityExportState::Disabled.is_problem());
        assert!(status.uptime_secs(export_now()).is_none());
    }

    #[test]
    fn a_fresh_exporter_with_no_export_yet_is_starting_not_never_exported() {
        // The false-alarm guard: a daemon restarted 12 seconds ago has not yet
        // had a fair chance to flush, so it must not read as broken.
        let status = running_exporter(12);
        assert_eq!(status.classify(export_now()), ObservabilityExportState::Starting);
        assert!(!ObservabilityExportState::Starting.is_problem());
    }

    #[test]
    fn past_the_grace_window_with_no_export_is_never_exported() {
        // THE state this issue exists for: configured, running for hours, and
        // silently never landed a single batch. Pre-#5083 this was
        // indistinguishable from healthy on every surface.
        let status = running_exporter(4 * 3600);
        assert_eq!(status.classify(export_now()), ObservabilityExportState::NeverExported);
        assert!(ObservabilityExportState::NeverExported.is_problem());
    }

    #[test]
    fn the_grace_window_scales_with_the_flush_interval_but_never_below_the_floor() {
        // A host configured with a one-hour flush cadence must not be called
        // out as never-exported before it has had three chances to flush.
        let mut slow = running_exporter(30 * 60);
        slow.flush_interval_secs = Some(3600);
        assert_eq!(slow.never_exported_grace_secs(), 3 * 3600);
        assert_eq!(slow.classify(export_now()), ObservabilityExportState::Starting);

        // ...and a very fast cadence still gets the floor, so a quiet host with
        // nothing to enqueue yet is not misreported either.
        let mut fast = running_exporter(60);
        fast.flush_interval_secs = Some(1);
        assert_eq!(fast.never_exported_grace_secs(), NEVER_EXPORTED_GRACE_FLOOR_SECS);
        assert_eq!(fast.classify(export_now()), ObservabilityExportState::Starting);
    }

    #[test]
    fn an_acked_batch_with_agreeing_ids_is_healthy() {
        let mut status = running_exporter(4 * 3600);
        status.last_success_at = Some(export_now() - chrono::Duration::seconds(12));
        status.records_exported = 3481;
        assert_eq!(status.classify(export_now()), ObservabilityExportState::Healthy);
        assert_eq!(status.last_success_age_secs(export_now()), Some(12));
        assert!(!ObservabilityExportState::Healthy.is_problem());
    }

    #[test]
    fn a_disagreeing_ingest_id_is_a_mismatch_even_while_exporting() {
        // #4830's condition, expressed on the positive surface: data IS
        // landing, under the wrong identity.
        let mut status = running_exporter(4 * 3600);
        status.last_success_at = Some(export_now() - chrono::Duration::seconds(12));
        status.ingest_host_id = Some("robb-pro".to_string());
        assert_eq!(status.classify(export_now()), ObservabilityExportState::HostIdMismatch);
        assert!(ObservabilityExportState::HostIdMismatch.is_problem());
    }

    #[test]
    fn an_echoed_id_that_agrees_is_not_a_mismatch() {
        // Defensive: only a *disagreement* is a mismatch. An echo equal to the
        // daemon's own id must stay healthy.
        let mut status = running_exporter(4 * 3600);
        status.last_success_at = Some(export_now());
        status.ingest_host_id = Some("robb-studio".to_string());
        assert_eq!(status.classify(export_now()), ObservabilityExportState::Healthy);
    }

    #[test]
    fn consecutive_failures_classify_as_failing() {
        let mut status = running_exporter(4 * 3600);
        status.last_success_at = Some(export_now() - chrono::Duration::seconds(7200));
        status.consecutive_failures = 3;
        status.last_failure_detail = Some("sink rejected batch: HTTP 401 — denied".to_string());
        assert_eq!(status.classify(export_now()), ObservabilityExportState::Failing);
        assert!(ObservabilityExportState::Failing.is_problem());
        // "Never worked" vs "worked, then broke" stays legible.
        assert_eq!(status.last_success_age_secs(export_now()), Some(7200));
    }

    #[test]
    fn a_mismatch_outranks_a_transient_flush_failure() {
        // Precedence documented on `classify`: the config-shaped fault that
        // cannot self-recover wins; the failure facts remain readable in the
        // `last_failure_*` fields either way.
        let mut status = running_exporter(4 * 3600);
        status.last_success_at = Some(export_now() - chrono::Duration::seconds(60));
        status.ingest_host_id = Some("robb-pro".to_string());
        status.consecutive_failures = 2;
        assert_eq!(status.classify(export_now()), ObservabilityExportState::HostIdMismatch);
        assert_eq!(status.consecutive_failures, 2);
    }

    #[test]
    fn never_exported_beats_a_stale_failure_only_after_grace() {
        // A brand-new exporter whose very first flush errored is `Failing`
        // (there is a real, current error) — not `Starting`: the error is
        // evidence, not absence of it.
        let mut status = running_exporter(12);
        status.consecutive_failures = 1;
        assert_eq!(status.classify(export_now()), ObservabilityExportState::Failing);
    }

    #[test]
    fn export_state_serializes_snake_case_for_watch_loops() {
        // The AC's machine-readability requirement: a watch loop asserts on
        // these exact strings via `status --json | jq`.
        for (state, wire) in [
            (ObservabilityExportState::Disabled, "\"disabled\""),
            (ObservabilityExportState::Misconfigured, "\"misconfigured\""),
            (ObservabilityExportState::Starting, "\"starting\""),
            (ObservabilityExportState::NeverExported, "\"never_exported\""),
            (ObservabilityExportState::Healthy, "\"healthy\""),
            (ObservabilityExportState::HostIdMismatch, "\"host_id_mismatch\""),
            (ObservabilityExportState::Failing, "\"failing\""),
        ] {
            assert_eq!(serde_json::to_string(&state).unwrap(), wire);
            let back: ObservabilityExportState = serde_json::from_str(wire).unwrap();
            assert_eq!(back, state);
        }
    }

    #[test]
    fn an_unknown_state_from_a_newer_daemon_does_not_break_the_parse() {
        let back: ObservabilityExportState = serde_json::from_str("\"quantum_entangled\"").unwrap();
        assert_eq!(back, ObservabilityExportState::Unrecognized);
    }

    #[test]
    fn export_status_round_trips_and_tolerates_pre_5083_wire_data() {
        let mut status = running_exporter(600);
        status.last_success_at = Some(export_now());
        status.records_exported = 12;
        status.state = status.classify(export_now());
        let json = serde_json::to_string(&status).unwrap();
        let back: ObservabilityExportStatus = serde_json::from_str(&json).unwrap();
        assert_eq!(back, status);
        assert_eq!(back.state, ObservabilityExportState::Healthy);

        // A pre-#5083 daemon omits the whole field; the report must still parse
        // and the absence must never be misread as "disabled".
        let minimal: ObservabilityExportStatus = serde_json::from_str("{}").unwrap();
        assert_eq!(minimal.state, ObservabilityExportState::Disabled);
        assert!(minimal.host_id.is_none());
    }

    // ==================================================================
    // Misconfigured export status (Issue #5337)
    // ==================================================================

    #[test]
    fn misconfigured_is_distinct_from_disabled() {
        // AC #1 / #4: `enabled: true` with a bad `ingestKeyFile` must report a
        // state distinct from — and never collapsing into — `disabled`, which
        // stays reserved for `enabled: false` / no observability block.
        let disabled = ObservabilityExportStatus::disabled();
        let misconfigured = ObservabilityExportStatus::misconfigured(
            Some("https://ingest.example.com/v1/telemetry".to_string()),
            "could not read ingest key file /etc/loom/ingest.key: No such file or directory (os error 2)"
                .to_string(),
        );
        assert_eq!(disabled.classify(export_now()), ObservabilityExportState::Disabled);
        assert_eq!(misconfigured.classify(export_now()), ObservabilityExportState::Misconfigured);
        assert_ne!(disabled.classify(export_now()), misconfigured.classify(export_now()));
        assert!(ObservabilityExportState::Misconfigured.is_problem());
        assert!(!ObservabilityExportState::Disabled.is_problem());
    }

    #[test]
    fn misconfigured_state_is_sticky_across_classify_despite_no_started_at() {
        // The precedence-chain regression this issue's fix guards against:
        // `classify`'s "not running ⇒ Disabled" fallback (branch 1) triggers
        // on ANY status with no `started_at` — which is true of a
        // `misconfigured()` status too, since the exporter never started.
        // Without the new branch-0 check, this would silently read back as
        // `Disabled`, reproducing the exact bug #5337 reports.
        let misconfigured =
            ObservabilityExportStatus::misconfigured(None, "no endpoint configured".to_string());
        assert!(misconfigured.uptime_secs(export_now()).is_none(), "never started ⇒ no uptime");
        assert_eq!(misconfigured.classify(export_now()), ObservabilityExportState::Misconfigured);
    }

    #[test]
    fn misconfigured_detail_names_the_path_and_endpoint_reflects_what_resolved() {
        // AC #2 (detail names the offending path and errno) and AC #3
        // (`endpoint` reflects what IS configured rather than `null`, when a
        // config block exists but a later field — the ingest key file — is
        // what's broken).
        let status = ObservabilityExportStatus::misconfigured(
            Some("https://ingest.example.com/v1/telemetry".to_string()),
            "could not read ingest key file /etc/loom/ingest.key: No such file or directory (os error 2)"
                .to_string(),
        );
        assert_eq!(status.endpoint.as_deref(), Some("https://ingest.example.com/v1/telemetry"));
        let detail = status.last_failure_detail.as_deref().unwrap();
        assert!(
            detail.contains("/etc/loom/ingest.key"),
            "detail must name the offending path: {detail}"
        );
        assert!(
            detail.contains("os error 2"),
            "detail must carry the underlying errno: {detail}"
        );
    }

    #[test]
    fn misconfigured_endpoint_is_none_when_the_endpoint_itself_is_what_is_missing() {
        // When `observability.endpoint` is the missing piece, there is nothing
        // to report — `None`, not an invented value.
        let status = ObservabilityExportStatus::misconfigured(
            None,
            "observability.endpoint not configured".to_string(),
        );
        assert!(status.endpoint.is_none());
        assert_eq!(status.classify(export_now()), ObservabilityExportState::Misconfigured);
    }
}
