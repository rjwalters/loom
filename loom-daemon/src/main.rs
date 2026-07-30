use loom_daemon::activity::{self, ActivityDb, StatsQueries};
use loom_daemon::auto_update;
use loom_daemon::autonomy_marker;
use loom_daemon::claim_reconciliation;
use loom_daemon::credential_preflight;
use loom_daemon::daemon_heartbeat;
use loom_daemon::daemon_install_state::{self, InstallStateReport};
use loom_daemon::epic_supervisor;
use loom_daemon::event_bus::EventBus;
use loom_daemon::health_monitor;
use loom_daemon::host_breaker;
use loom_daemon::idle_exit;
use loom_daemon::ipc::IpcServer;
use loom_daemon::main_health_gate;
use loom_daemon::metrics_collector;
use loom_daemon::quarantine_reconciliation;
use loom_daemon::rate_limit_breaker;
use loom_daemon::role_runner;
use loom_daemon::role_validation;
use loom_daemon::script_helpers;
use loom_daemon::self_update;
use loom_daemon::serve;
use loom_daemon::sweep_registry::{self, SweepRegistry, SweepRegistryConfig};
use loom_daemon::terminal::TerminalManager;
use loom_daemon::token_ranking_refresh;
use loom_daemon::types::{DaemonStatusReport, QuarantineEntry, Request, Response, SweepKind};
use loom_daemon::watch_registry;
use loom_daemon::work_finder;
use loom_daemon::workspace_pool::WorkspacePool;
use loom_daemon::worktree_ops::{aggressive, clean};
use loom_daemon::{extract_configured_terminal_ids, rotate_log_file};

use anyhow::{anyhow, bail, Context, Result};
use chrono::{DateTime, Utc};
use clap::{Parser, Subcommand};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

/// Loom daemon - terminal multiplexing and workspace orchestration
#[derive(Parser)]
#[command(name = "loom-daemon")]
#[command(about = "Loom daemon for AI-powered development orchestration", long_about = None)]
// Embed git commit + build timestamp alongside the crate version so
// `--version` distinguishes rebuilds of the same release. Motivated by
// issue #3470: stale daemon binaries are otherwise indistinguishable from
// fresh ones and cause hard-to-diagnose install regressions (#3287 class).
// `LOOM_DAEMON_GIT_COMMIT` and `LOOM_DAEMON_BUILD_TIME` are populated by
// `build.rs`; both fall back to "unknown" when the build host lacks the
// tooling, which is loud but harmless.
#[command(version = concat!(
    env!("CARGO_PKG_VERSION"),
    " (commit ",
    env!("LOOM_DAEMON_GIT_COMMIT"),
    ", built ",
    env!("LOOM_DAEMON_BUILD_TIME"),
    ")"
))]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    /// Initialize a Loom workspace in a target repository
    Init {
        /// Target workspace directory (must be a git repository)
        #[arg(value_name = "PATH", default_value = ".")]
        workspace: String,

        /// Path to defaults directory
        #[arg(long, default_value = "defaults")]
        defaults: String,

        /// Overwrite existing .loom directory if it exists
        #[arg(long)]
        force: bool,

        /// Print what would be done without making changes
        #[arg(long)]
        dry_run: bool,
    },

    /// Rewrite only the marker-delimited Loom-managed `.gitignore` block in a
    /// workspace, converging it on the current `EPHEMERAL_PATTERNS` set without
    /// running a full `init` (Issue #4280). This is the standalone entry point
    /// `defaults/scripts/resync-installed.sh` invokes so existing consumer
    /// installs pick up newly-ignored runtime paths (e.g. `.loom/sweep-checkpoint/`,
    /// `.loom/worktrees-local/`) at resync time — the pattern list stays
    /// single-sourced in the daemon, never copied into shell. Idempotent: a
    /// workspace already carrying the current block is a byte-for-byte no-op.
    UpdateGitignore {
        /// Target workspace directory (the repo root whose `.gitignore` to
        /// refresh). Defaults to the current directory.
        #[arg(value_name = "PATH", default_value = ".")]
        workspace: String,
    },

    /// Display agent effectiveness and activity metrics.
    ///
    /// With no positional `command`, prints the original interactive
    /// dashboard (unchanged, backward compatible). With an explicit
    /// `command` (`summary`/`effectiveness`/`costs`/`velocity`) this is the
    /// native port of `loom_tools.agent_metrics` (epic #4081 Phase 3 family
    /// 4, issue #4274) — the CLI contract `agent-metrics.sh` and
    /// `mcp__loom__get_agent_metrics` invoke, including `--period` and
    /// `--by-model` (#3482).
    Stats {
        /// Metrics command: summary, effectiveness, costs, velocity. Omit for
        /// the original interactive dashboard.
        #[arg(value_name = "COMMAND")]
        command: Option<String>,

        /// Filter by agent role (builder, judge, curator, etc.)
        #[arg(long)]
        role: Option<String>,

        /// Filter by GitHub issue number
        #[arg(long)]
        issue: Option<i32>,

        /// Show weekly trends instead of daily (dashboard mode only; use the
        /// `velocity` command for the agent-metrics-parity table).
        #[arg(long)]
        weekly: bool,

        /// Time period for `command` mode: today, week, month, all (default: week).
        #[arg(long)]
        period: Option<String>,

        /// Add a per-model dimension to `effectiveness`/`costs` output
        /// (`command` mode only); NULL/absent model values render as
        /// `default` (#3482).
        #[arg(long)]
        by_model: bool,

        /// Output format: table (default), json
        #[arg(long, default_value = "table")]
        format: String,
    },

    /// Show the running daemon's autonomous-mode status: in-flight sweeps, the
    /// three dynamic-cap inputs (token-pool size, disk headroom, configured
    /// ceiling) plus their `min` cap, the main-health-gate halt state, and
    /// per-token usage. Connects to the running daemon over its Unix socket
    /// (Issue #3891 — follow-up to #3813 Phase D).
    Status {
        /// Emit machine-readable JSON instead of the human-readable table.
        #[arg(long)]
        json: bool,

        /// Also show the forge-side pipeline snapshot per managed repo (Issue
        /// #3977): open `loom:issue` (queued), open `loom:building`
        /// (claimed), open PRs by `loom:review-requested` /
        /// `loom:changes-requested` / `loom:pr`, and PRs merged in the last
        /// 24h. Opt-in because it makes several `gh` calls per managed repo
        /// (client-side, after the fast IPC round-trip) rather than being
        /// bundled into the default view.
        #[arg(long)]
        pipeline: bool,
    },

    /// Measure the host and recommend — or with `--write`, apply — the
    /// autonomous concurrency knobs (`autonomous.workFinder.maxConcurrent` /
    /// `autonomous.perTokenConcurrency`, issue #4390). Prints the same
    /// `min(...)` cap breakdown `status` uses, plus which term would bind
    /// after applying the recommendation. Purely file/host-based; does not
    /// require a running daemon (unlike `status`, which reports the running
    /// daemon's own in-memory dispatch state).
    Calibrate {
        /// Repo root to measure/write (plain path, default `.` — no upward
        /// `.git` walk).
        #[arg(long, value_name = "PATH", default_value = ".")]
        workspace: String,

        /// Merge the recommendation into `<workspace>/.loom/config.json`
        /// (only `autonomous.workFinder.maxConcurrent` /
        /// `autonomous.perTokenConcurrency` — every other key is preserved).
        /// Idempotent: a repeat `--write` with the same recommendation is
        /// byte-identical. Without this flag, calibrate is strictly
        /// read-only.
        #[arg(long)]
        write: bool,

        /// Emit machine-readable JSON instead of the human-readable report.
        #[arg(long)]
        json: bool,
    },

    /// Manage the machine-level workspace registry (`~/.loom/workspaces.json`):
    /// the set of repos the one-per-machine daemon manages (Issue #3926 — phase
    /// 1 of #3835). Operates directly on the registry file, so it works whether
    /// or not the daemon is running; a running daemon re-reads the file on its
    /// next tick (hot-apply).
    Workspace {
        #[command(subcommand)]
        action: WorkspaceAction,
    },

    /// Operator-triggered multi-host worker fanout (epic #4340). `fleet
    /// add-worker <ssh-host> --repo <repo>` takes a reachable, already-provisioned
    /// Ubuntu host to "daemon running, workspace registered, tokens ranked,
    /// dispatch verified" in one idempotent command, over ssh. Generic VM
    /// provisioning stays in `repo:remote` — this consumes a reachable box + an
    /// SSH alias, it never wrangles a cloud CLI. The bootstrap is an ordered,
    /// idempotent plan; `--dry-run` prints it without touching the host.
    Fleet {
        #[command(subcommand)]
        action: FleetAction,
    },

    /// Manage insta-crash quarantines (Issue #3939): the in-memory pauses the
    /// daemon applies to issues whose sweeps insta-crash repeatedly. Connects to
    /// the running daemon over its Unix socket, since the quarantine state lives
    /// in the daemon's memory (not on disk).
    Quarantine {
        #[command(subcommand)]
        action: QuarantineAction,
    },

    /// Dispatch a `/loom:sweep <issue>` via the running daemon (Issue #3952): a
    /// first-class, non-MCP operator entry point over the same IPC `DispatchSweep`
    /// request the `mcp__loom__dispatch_sweep` tool uses. Registry tracking, the
    /// `loom:issue → loom:building` claim flip, and event publishing all come for
    /// free — this is the resilient replacement for the hand-rolled
    /// `LOOM_SWEEP_CLAIM_OWNED=<N>` + `spawn-claude.sh -p "/loom:sweep N"` pattern.
    ///
    /// Applies a bounded client-side ack timeout: if the daemon does not respond
    /// within a few seconds the command exits nonzero with a clear error instead
    /// of hanging (motivated by the 1800s MCP wedge in #3945).
    Dispatch {
        /// The issue number to dispatch a sweep for.
        #[arg(value_name = "ISSUE")]
        issue: u32,

        /// Target managed-workspace root (Issue #3929 plumbing). Omit to dispatch
        /// into the daemon's default workspace.
        #[arg(long, value_name = "PATH")]
        workspace: Option<String>,

        /// Pin the spawned child to an explicit model (e.g. `sonnet`, `opus`).
        /// Omit to let the daemon resolve `autonomous.model` / the shipped default.
        #[arg(long, value_name = "M")]
        model: Option<String>,

        /// Reasoning-effort override forwarded to the spawned child.
        #[arg(long, value_name = "E")]
        effort: Option<String>,

        /// Single parent issue for a stacked sweep (Issue #3729): the child
        /// branches its worktree/PR off `feature/issue-<P>` instead of the default
        /// branch.
        #[arg(long, value_name = "P")]
        depends_on: Option<u32>,

        /// Override the host-distress circuit breaker (Issue #4235). By default a
        /// *tripped* breaker (sustained host distress) refuses an explicit
        /// dispatch; pass `--force` to dispatch anyway.
        #[arg(long)]
        force: bool,
    },

    /// Start a minimal read-only HTTP status-snapshot listener + embedded
    /// dashboard (Issue #4391 phase 1, #4392 phase 2, #4393 phase 3 of
    /// #4329). `GET /api/status` (#4391) serializes the same
    /// `DaemonStatusReport` `loom-daemon status --json` already aggregates —
    /// fetched live over the *existing* Unix socket (the same `DaemonStatus`
    /// IPC request `status` sends), so the aggregation logic in
    /// `ipc::build_daemon_status` runs exactly once and is never duplicated
    /// here. `GET /api/events` (#4392) tails the daemon's event bus as
    /// `text/event-stream`, bridged from the same socket's existing
    /// `SubscribeEvents` request over the frozen `sweep.*` topics. `GET /`
    /// (#4393) serves an embedded single-page dashboard (vanilla JS, no
    /// build toolchain) that consumes both of the above plus two new
    /// read-only endpoints: `GET /api/pipeline` (forge queue counts per
    /// managed repo, reusing `pipeline_snapshot::GhPipelineSource`) and
    /// `GET /api/tokens` (per-account `.ranking` rows). Every endpoint is
    /// read-only: no new persistent store, no mutation, no publish.
    ///
    /// Off by default: nothing listens until this subcommand is explicitly
    /// run — a running daemon started without `serve` never opens this port.
    /// Binds `127.0.0.1` by default; a non-loopback `--bind` (e.g. a tailnet
    /// interface address, for the multi-host fleet's cross-host visibility)
    /// additionally requires `--allow-non-loopback` — the address alone is
    /// never enough, and a wildcard bind (`0.0.0.0`/`::`) is refused even
    /// with both flags, so this can never become publicly reachable. Every
    /// response carries this host's identity (`hostname`) so the dashboard's
    /// client-side multihost aggregator (#4393) can label sources without
    /// any server-side fan-out.
    Serve {
        /// TCP port for the HTTP listener.
        #[arg(long, default_value_t = serve::DEFAULT_PORT)]
        port: u16,

        /// Interface address to bind. Defaults to loopback-only (127.0.0.1).
        /// A non-loopback address additionally requires
        /// `--allow-non-loopback`; a wildcard address (0.0.0.0 / ::) is
        /// refused unconditionally.
        #[arg(long, default_value = "127.0.0.1")]
        bind: String,

        /// Explicit opt-in required to bind any non-loopback address (e.g. a
        /// tailnet interface). Never permits a wildcard bind even when set.
        #[arg(long)]
        allow_non_loopback: bool,

        /// Comma-separated list of peer daemon `serve` base URLs (e.g.
        /// `http://host2:7420,http://host3:7420`), Issue #4393's multihost
        /// fleet requirement. Served verbatim at `GET /api/peers`; the
        /// dashboard fetches each one **from the browser** — this daemon
        /// never queries a peer itself, so there is no server-side
        /// aggregation and no new central store. Empty by default (the
        /// single-host view).
        #[arg(long, default_value = "")]
        peers: String,
    },

    /// Manage durable operator watches on issue/PR terminal state (Issue #3971).
    /// A watch registered here is persisted to `~/.loom/watches.json` and polled
    /// by the running daemon, so it survives this shell — and even a daemon
    /// restart. Terminal resolutions land in `~/.loom/logs/watch-results.log`.
    /// Connects to the running daemon over its Unix socket.
    Watch {
        #[command(subcommand)]
        action: WatchAction,
    },

    /// Deliberately restart the running daemon (Issue #4054 — the supervised
    /// restart primitive). Sends a `RestartDaemon` request over the Unix socket;
    /// when the daemon is supervised by launchd it exits 0 for a clean
    /// `KeepAlive:SuccessfulExit` relaunch (a new pid, exactly its start flags,
    /// in-flight sweeps preserved). On an unsupervised host (nohup / Linux /
    /// `--foreground`) the daemon refuses and stays running, and this command
    /// prints the refusal and exits non-zero. This is the primitive #4017 Phase
    /// 3 will call after a rebuild — it does nothing on its own.
    ///
    /// With `--drain` (Issue #4090) the daemon instead stops admitting new work
    /// immediately and waits for every in-flight sweep to finish before
    /// restarting — no sweep killed, no orphan left behind. `--abort-drain`
    /// cancels an in-progress drain and resumes normal dispatch.
    ///
    /// With `--drain --then-exit` (Issue #4343 — `fleet drain`'s teardown use
    /// case) the daemon stops (and stays stopped — exits without a supervised
    /// relaunch) instead of restarting once drained, so it cannot pick up new
    /// dispatch on a host that is about to be powered off.
    Restart {
        /// Finish all in-flight sweeps before restarting, instead of restarting
        /// immediately (#4090). New dispatch is paused for the duration.
        #[arg(long)]
        drain: bool,
        /// Max seconds to wait for in-flight sweeps to drain (with `--drain`).
        /// Defaults to the daemon's built-in timeout (tens of minutes).
        #[arg(long)]
        timeout: Option<u64>,
        /// On drain timeout, cancel the remaining sweeps and restart anyway
        /// (with `--drain`). Without this, a timeout refuses the restart and
        /// keeps the daemon running (fail-safe).
        #[arg(long)]
        force_after_timeout: bool,
        /// Abort an in-progress drain and resume normal dispatch (no restart).
        #[arg(long)]
        abort_drain: bool,
        /// With `--drain`, stop (and stay down) instead of restarting once
        /// drained (Issue #4343). Requires `--drain`; the daemon does not
        /// require a recognized supervisor for this variant (there is
        /// nothing to prove supervision for — a `then-exit` drain never
        /// wants a relaunch).
        #[arg(long)]
        then_exit: bool,
    },

    /// Manage the multi-account OAuth token pool at `.loom/tokens/` (Issue
    /// #4082/#4108, epic #4081 "eliminate Python from Loom"). Native Rust
    /// port of the token pool: the 3-tier selection algorithm, the HTTP
    /// rate-limit probe (`check`), and the operator-facing pin/unpin/unblock
    /// bookkeeping CLI. As of #4080 (Phase 2) `check` is also the
    /// implementation `probe-tokens.sh` and the daemon's own ranking
    /// self-refresh invoke natively. As of #4105 `bootstrap` is native too
    /// (multi-source `.env` merge + pool provisioning), and as of #4106
    /// `import-from-monitor` is native too (claude-monitor live SQLite
    /// import). As of #4228 `select` and the new `mark-bad` are also what
    /// `spawn-claude.sh` / `claude-wrapper.sh` invoke — zero Python left on
    /// the token hot path (see `loom-daemon/src/tokens_pool/mod.rs`). Purely
    /// file-based; does not require a running daemon.
    Tokens {
        #[command(subcommand)]
        action: TokensAction,
    },

    /// Standalone CLI surface over `terminal.rs`'s per-agent
    /// `CLAUDE_CONFIG_DIR` isolation (issue #4415, epic #4081 Phase 3 family
    /// 4): create/remove/validate an agent's isolated config directory, and
    /// pre-seed the folder-trust modal for a spawn target. Exists so the
    /// manual-mode spawn path reuses the same logic
    /// `TerminalManager::create_terminal`/`destroy_terminal` already use
    /// internally, rather than a second (Python) reimplementation. Purely
    /// file-based; does not require a running daemon.
    ClaudeConfig {
        #[command(subcommand)]
        action: ClaudeConfigAction,
    },

    /// Native port of `loom-agent-spawn` (issue #4415, epic #4081 Phase 3
    /// family 4): spawn a Claude Code agent in a tmux session on the shared
    /// `loom` socket. Backs `defaults/scripts/agent-spawn.sh`; flags, exit
    /// codes (0 success / 1 error), and the `--json` payload mirror the
    /// retired Python CLI. Does not require a running daemon.
    AgentSpawn {
        /// Role name (builder, judge, curator, ...).
        #[arg(long)]
        role: Option<String>,

        /// Session identifier (tmux session becomes `loom-<name>`).
        #[arg(long)]
        name: Option<String>,

        /// Arguments appended to the role slash command.
        #[arg(long, default_value = "")]
        args: String,

        /// Path to a git worktree the agent should run in.
        #[arg(long)]
        worktree: Option<String>,

        /// Mark the session ephemeral (for `agent-destroy.sh` cleanup).
        #[arg(long = "on-demand")]
        on_demand: bool,

        /// Force a new session even if one exists (kills stuck sessions).
        #[arg(long)]
        fresh: bool,

        /// Block until the agent completes.
        #[arg(long)]
        wait: bool,

        /// Timeout in seconds for `--wait`.
        #[arg(long, default_value_t = 3600)]
        timeout: u64,

        /// Emit the spawn result as JSON on stdout.
        #[arg(long)]
        json: bool,

        /// Check whether a session exists (exit 0 if yes, 1 if no).
        #[arg(long, value_name = "NAME")]
        check: Option<String>,

        /// List all active loom-agent tmux sessions.
        #[arg(long)]
        list: bool,
    },

    /// Native port of `loom-agent-wait` (issue #4415, epic #4081 Phase 3
    /// family 4): block until a tmux Claude agent finishes. Backs
    /// `defaults/scripts/agent-wait.sh`; exit codes are unchanged
    /// (0 completed / 1 timeout / 2 not found or error). Does not require a
    /// running daemon.
    AgentWait {
        /// Agent session name (without the `loom-` prefix).
        #[arg(value_name = "NAME")]
        name: String,

        /// Maximum time to wait, in seconds. `0` performs a single
        /// non-blocking check.
        #[arg(long, default_value_t = 3600)]
        timeout: u64,

        /// Seconds between polls.
        #[arg(long = "poll-interval", default_value_t = 5)]
        poll_interval: u64,

        /// Minimum session age before idle-prompt detection activates.
        /// `--min-idle-elapsed` is a deprecated alias.
        #[arg(
            long = "min-session-age",
            alias = "min-idle-elapsed",
            default_value_t = 10
        )]
        min_session_age: u64,

        /// Emit the wait result as JSON on stdout.
        #[arg(long)]
        json: bool,
    },

    /// Native port of `loom-clean` (Issue #4272, epic #4081 Phase 3 family 2):
    /// worktree/branch/tmux/agent-config cleanup, `--deep` build-artifact
    /// removal, `--safe` merged-PR-only mode, and `--daemon` crash recovery.
    /// `--aggressive` additionally enumerates every `git worktree` entry
    /// (not just `.loom/worktrees/issue-*`) under the vestigial-worktree
    /// decision tree from issue #3332. Purely file/git/gh-based; does not
    /// require a running daemon. Flags mirror the retired `loom-clean`
    /// console script byte-for-byte.
    Clean {
        /// Workspace directory (repo root, or any path under it).
        #[arg(long, value_name = "PATH", default_value = ".")]
        workspace: String,

        #[arg(long)]
        dry_run: bool,

        #[arg(long)]
        deep: bool,

        #[arg(short = 'f', long, visible_alias = "yes", visible_short_alias = 'y')]
        force: bool,

        #[arg(long)]
        safe: bool,

        #[arg(long, default_value_t = clean::DEFAULT_GRACE_PERIOD_SECS)]
        grace_period: i64,

        #[arg(long, visible_alias = "worktrees")]
        worktrees_only: bool,

        #[arg(long, visible_alias = "branches")]
        branches_only: bool,

        #[arg(long, visible_alias = "tmux")]
        tmux_only: bool,

        /// Crash recovery: kill tmux sessions, revert stale `loom:building`
        /// labels for issues with no live spawn-loop task, clear stale
        /// claim-lock dirs, reset issue-failures.json.
        #[arg(long)]
        daemon: bool,

        /// Enumerate ALL worktrees and remove vestigial ones reachable from
        /// origin/main (see issue #3332). Respects open PRs, active
        /// spawn-loop tasks, the `.loom-managed` sentinel, and uncommitted
        /// changes.
        #[arg(long)]
        aggressive: bool,

        #[arg(long, default_value_t = aggressive::DEFAULT_AGGRESSIVE_MIN_AGE)]
        aggressive_min_age: u64,
    },

    /// Native port of `loom-cleanup` (Issue #4272): log archival, the only
    /// cleanup.py functionality that survived the daemon-brain retirement
    /// (#3396). Purely file-based; does not require a running daemon.
    Cleanup {
        #[command(subcommand)]
        action: CleanupAction,
    },

    /// Native port of `loom-recover-orphans` (Issue #4272): detects `loom:building`
    /// issues with no live sweep tracking them and spawn-loop tasks with a
    /// stale heartbeat + dead PID, and (with `--recover`) resets them.
    /// Fail-safe (#3651): absent liveness evidence means every claim is
    /// treated as ALIVE, never as orphaned. Purely file/git/gh-based; does
    /// not require a running daemon.
    RecoverOrphans {
        #[arg(long, value_name = "PATH", default_value = ".")]
        workspace: String,

        /// Actually perform recovery (default is dry-run detection only).
        #[arg(long)]
        recover: bool,

        /// Emit JSON instead of the human-readable report.
        #[arg(long)]
        json: bool,

        #[arg(long, short = 'v')]
        verbose: bool,
    },

    /// Forge-agnostic issue/PR/auto-merge operations — the native Rust port of
    /// the `loom-forge` (`loom_tools.forge_cli`) and `loom-auto-merge`
    /// (`loom_tools.auto_merge`) Python CLIs (epic #4081 Phase 3, family 3).
    ///
    /// GitHub is native: `issue`/`pr`/`auth` are a byte-identical passthrough
    /// to `gh`, and `auto-merge` enables auto-merge via the
    /// `enablePullRequestAutoMerge` GraphQL mutation (no working-tree
    /// checkout). Gitea declines with exit code 3 so the caller's shell path
    /// (`merge-pr.sh`'s `forge_auto_merge`, or the `gh` read fallback) carries
    /// it. Forge config resolves from the canonical repo root (never a
    /// worktree CWD); see `forge_cmd.rs` for the full #4061 semantics.
    Forge {
        #[command(subcommand)]
        action: ForgeAction,
    },

    // ---------------------------------------------------------------------
    // Script helpers (epic #4081 Phase 3 family 5, issue #4275) — native
    // replacements for the `loom_tools` modules that existed only to back a
    // thin `defaults/scripts/*.sh` entry point. Flags, stdout shapes and exit
    // codes are unchanged from the Python CLIs they replace, so a zero-pip
    // consumer workspace behaves identically.
    // ---------------------------------------------------------------------
    /// Strip ANSI escapes and Claude Code TUI noise from terminal output
    /// (native port of `loom_tools.log_filter`, #4275). Backs
    /// `defaults/scripts/strip-ansi.sh`.
    ///
    /// With no arguments this is the real-time stdin→stdout `tmux pipe-pane`
    /// filter (dedup + noise suppression); `--file` deep-cleans a captured
    /// agent log to stdout.
    StripAnsi {
        /// Post-process a captured log file instead of filtering stdin.
        #[arg(long, value_name = "PATH")]
        file: Option<String>,
    },

    /// Resolve a logical model tier/alias to the concrete model ID to dispatch
    /// on the wire (issue #3982; native port of `loom_tools.model_tiers`,
    /// #4275). Backs `resolve-model.sh` and `resolve-tier-model.sh`.
    ///
    /// Unknown aliases and pinned IDs pass through unchanged and the
    /// `model@effort` grammar is preserved. `--config` is an **explicit
    /// bypass**: it reads exactly that file and skips all config tiering
    /// (#4060); only the default path routes through the config resolver.
    /// Resolution never fails — outside a Loom repo the config is simply empty
    /// and the shipped default map applies.
    ///
    /// Exit codes: `0` resolved, `2` usage error, `3` no mapping (both `--tier`
    /// and `--task-alias`) so the caller falls through to its own precedence
    /// chain.
    ResolveModel {
        /// A logical tier/alias (`opus`, `sonnet`, `sonnet@xhigh`) or a pinned
        /// model ID.
        #[arg(value_name = "MODEL")]
        model: Option<String>,

        /// Path to a `.loom/config.json` to read verbatim (default: resolve
        /// the `./.loom` config tier chain).
        #[arg(long, value_name = "PATH")]
        config: Option<String>,

        /// Print the resolved generation number instead of the model ID.
        #[arg(long)]
        generation: bool,

        /// Map the model back to the nearest value the in-session Task/Agent
        /// tool's `model` enum accepts (`haiku|sonnet|opus|fable`) — issue
        /// #4282. Exits 3 with no output when there is no Task-passable alias,
        /// so the caller omits `model` entirely.
        #[arg(long = "task-alias")]
        task_alias: bool,

        /// Complexity-tier mode (issue #4238): resolve
        /// `sweep.tierModels[<runtime>][<tier>]`, falling back to the
        /// `sweep.optimization` preset, instead of a bare alias.
        #[arg(long, value_name = "TIER")]
        tier: Option<String>,

        /// Worker runtime for `--tier` resolution.
        #[arg(long, default_value = "claude")]
        runtime: String,
    },

    /// Query Claude API usage via the Anthropic OAuth API (native port of
    /// `loom_tools.common.usage`, #4275). Backs `check-usage.sh`.
    ///
    /// Exits 1 when the payload carries an `error` key (no Keychain token, API
    /// failure, or not inside a Loom repo) — the historical contract.
    Usage {
        /// Print a human-readable status block instead of JSON.
        #[arg(long)]
        status: bool,
    },

    /// Manage builder checkpoints for progress tracking (native port of
    /// `loom_tools.checkpoints`, #4275). Backs `checkpoint.sh`.
    Checkpoint {
        #[command(subcommand)]
        action: CheckpointAction,
    },

    /// Atomic file-based issue claiming for parallel agent orchestration
    /// (native port of `loom_tools.claim`, #4275). Backs `claim.sh`.
    ///
    /// Exit codes: `0` success, `1` already claimed / general error, `2`
    /// invalid arguments, `3` claim not found, `4` agent-ID mismatch.
    Claim {
        /// One of: `claim`, `extend`, `release`, `check`, `list`, `cleanup`.
        #[arg(value_name = "COMMAND")]
        command: Option<String>,

        /// Positional command arguments (issue number, agent id, ttl).
        #[arg(value_name = "ARGS", trailing_var_arg = true)]
        args: Vec<String>,
    },

    /// Model-cost experiment instrumentation for `/loom:sweep` (issue #3725;
    /// native port of `loom_tools.sweep_experiment`, #4275). Backs
    /// `sweep-experiment.sh`.
    SweepExperiment {
        #[command(subcommand)]
        action: SweepExperimentAction,
    },

    /// Validate a sweep phase contract and attempt mechanical recovery (native
    /// port of `loom_tools.validate_phase`, #4275). Backs `validate-phase.sh`.
    ///
    /// Exit codes: `0` contract satisfied (initially or after recovery), `1`
    /// contract failed, `2` invalid arguments.
    ValidatePhase {
        /// `curator` | `builder` | `judge` | `doctor`.
        #[arg(value_name = "PHASE")]
        phase: String,

        #[arg(value_name = "ISSUE")]
        issue: i64,

        /// Worktree path (required for builder recovery).
        #[arg(long, value_name = "PATH")]
        worktree: Option<String>,

        /// PR number (for judge/doctor, and a cached hint for builder).
        #[arg(long = "pr", value_name = "N")]
        pr_number: Option<i64>,

        /// Sweep task ID for milestone reporting.
        #[arg(long = "task-id", value_name = "ID")]
        task_id: Option<String>,

        /// Emit the result as JSON.
        #[arg(long)]
        json: bool,

        /// Only check contract status; skip all side effects.
        #[arg(long = "check-only")]
        check_only: bool,

        /// Attempt recovery but suppress diagnostic comments and label changes
        /// on failure (issue #2609 — used by retry loops).
        #[arg(long)]
        quiet: bool,
    },

    /// Validate role configuration completeness
    Validate {
        /// Workspace directory containing .loom/config.json
        #[arg(value_name = "WORKSPACE", default_value = ".")]
        workspace: String,

        /// Output format: text (default), json
        #[arg(long, default_value = "text")]
        format: String,

        /// Fail with exit code 2 if warnings found (for CI)
        #[arg(long)]
        strict: bool,

        /// Show verbose output including configured roles
        #[arg(long, short)]
        verbose: bool,
    },
}

/// Sub-actions for `loom-daemon checkpoint` (issue #4275).
#[derive(Subcommand)]
enum CheckpointAction {
    /// Write a checkpoint to a worktree.
    Write {
        /// Worktree directory (default: the current directory).
        #[arg(long, short = 'w', value_name = "PATH")]
        worktree: Option<String>,

        /// One of: planning, implementing, tested, committed, pushed, pr_created.
        #[arg(long, short = 's', value_name = "STAGE")]
        stage: String,

        #[arg(long, short = 'i', value_name = "N")]
        issue: Option<i64>,

        #[arg(long = "files-changed", value_name = "N")]
        files_changed: Option<i64>,

        #[arg(long = "test-command", value_name = "CMD")]
        test_command: Option<String>,

        /// `pass` or `fail`.
        #[arg(long = "test-result", value_name = "RESULT")]
        test_result: Option<String>,

        #[arg(long = "test-output-summary", value_name = "TEXT")]
        test_output_summary: Option<String>,

        #[arg(long = "commit-sha", value_name = "SHA")]
        commit_sha: Option<String>,

        #[arg(long = "pr-number", value_name = "N")]
        pr_number: Option<i64>,

        #[arg(long, short = 'q')]
        quiet: bool,
    },

    /// Read the checkpoint from a worktree.
    Read {
        #[arg(long, short = 'w', value_name = "PATH")]
        worktree: Option<String>,

        #[arg(long, short = 'j')]
        json: bool,
    },

    /// Clear the checkpoint in a worktree.
    Clear {
        #[arg(long, short = 'w', value_name = "PATH")]
        worktree: Option<String>,

        #[arg(long, short = 'q')]
        quiet: bool,
    },

    /// List the valid checkpoint stages and their recovery paths.
    Stages {
        #[arg(long, short = 'j')]
        json: bool,
    },
}

/// Sub-actions for `loom-daemon sweep-experiment` (issue #4275).
#[derive(Subcommand)]
enum SweepExperimentAction {
    /// Print the effective tri-state mode (after the canary guardrail).
    ResolveMode {
        #[arg(long, value_name = "PATH")]
        config: Option<String>,
    },

    /// Print the deterministic per-issue arm + forced model.
    AssignArm {
        #[arg(long, value_name = "N")]
        issue: i64,

        #[arg(long, value_name = "TIER")]
        complexity: Option<String>,

        #[arg(long, default_value = "text", value_parser = ["text", "json"])]
        format: String,

        /// Print the concrete model ID the arm's alias resolves to (#3982)
        /// instead of the bare alias.
        #[arg(long)]
        resolve: bool,

        #[arg(long, value_name = "PATH")]
        config: Option<String>,
    },

    /// Print the loud startup banner naming mode + arm.
    Banner {
        #[arg(long, value_name = "N")]
        issue: i64,

        #[arg(long, value_name = "TIER")]
        complexity: Option<String>,

        #[arg(long, value_name = "PATH")]
        config: Option<String>,
    },

    /// Append one JSONL outcome-chain record.
    Record {
        #[arg(long, value_name = "N")]
        issue: i64,

        #[arg(long)]
        phase: String,

        #[arg(long)]
        role: String,

        #[arg(long)]
        model: Option<String>,

        #[arg(long, default_value = "observe")]
        mode: String,

        #[arg(long)]
        arm: Option<String>,

        #[arg(long, default_value_t = 1)]
        attempt: i64,

        #[arg(long)]
        complexity: Option<String>,

        #[arg(long)]
        verdict: Option<String>,

        #[arg(long = "cycle-count", default_value_t = 0)]
        cycle_count: i64,

        #[arg(long)]
        pr: Option<i64>,

        #[arg(long)]
        effort: Option<String>,

        #[arg(long = "agent-id")]
        agent_id: Option<String>,

        #[arg(long)]
        transcript: Option<String>,

        #[arg(long = "in-tok")]
        in_tok: Option<i64>,

        #[arg(long = "out-tok")]
        out_tok: Option<i64>,

        #[arg(long = "token-fidelity", default_value = "none")]
        token_fidelity: String,

        #[arg(long = "stats-file")]
        stats_file: Option<String>,

        #[arg(long)]
        quiet: bool,
    },

    /// Aggregate the stats store into the per-arm #3718 inequality inputs.
    Harvest {
        #[arg(long = "stats-file")]
        stats_file: Option<String>,

        #[arg(long = "archive-dir")]
        archive_dir: Option<String>,

        #[arg(long, default_value = "text", value_parser = ["text", "json"])]
        format: String,
    },
}

/// Sub-actions for `loom-daemon workspace`.
#[derive(Subcommand)]
enum WorkspaceAction {
    /// Register a repo as a managed workspace.
    Add {
        /// Path to the repo root (relative or absolute; normalized on store).
        #[arg(value_name = "PATH")]
        path: String,

        /// Cross-repo dispatch priority tier (#3946): lower = higher priority.
        /// The autonomous work-finder and epic supervisor drain higher-priority
        /// repos first. Defaults to 100 when omitted.
        #[arg(long, value_name = "N", default_value_t = loom_daemon::workspace_registry::DEFAULT_WORKSPACE_PRIORITY)]
        priority: u32,

        /// Optional per-repo config overrides as a JSON object string.
        #[arg(long, value_name = "JSON")]
        config_overrides: Option<String>,
    },
    /// Set the dispatch priority tier of an already-registered workspace (#3946).
    SetPriority {
        /// Path to the repo root (normalized the same way as `add`).
        #[arg(value_name = "PATH")]
        path: String,

        /// New priority tier: lower = higher priority.
        #[arg(value_name = "N")]
        priority: u32,
    },
    /// Deregister a managed workspace by root.
    Remove {
        /// Path to the repo root (normalized the same way as `add`).
        #[arg(value_name = "PATH")]
        path: String,
    },
    /// List the managed workspaces.
    List {
        /// Emit machine-readable JSON instead of the human-readable table.
        #[arg(long)]
        json: bool,
    },
}

/// Sub-actions for `loom-daemon fleet` (epic #4340).
// `AddWorker` legitimately carries many operator-supplied `--safehouse-*`
// flags (#3998) next to `Drain`/`Status`'s few fields — boxing individual
// clap-derive `Option`/`Vec` fields would break the derive macro's
// Option-arity detection for negligible benefit on a parsed-once-at-startup
// CLI enum, so this size skew is accepted rather than worked around.
#[derive(Subcommand)]
#[allow(clippy::large_enum_variant)]
enum FleetAction {
    /// Bootstrap a provisioned host into a working loom worker (issue #4341).
    /// Runs an ordered, idempotent plan over `ssh <ssh-host>`: base deps, the
    /// machine-level loom-daemon build, Claude Code, forge auth, the full token
    /// pool, workspace clone + registration, a systemd --user daemon unit, an
    /// optional idle-shutdown guard, and a verify step. Secrets (PAT,
    /// accounts.env) travel only over ssh stdin, never a command line.
    AddWorker {
        /// SSH alias/host to reach the worker (from `repo:remote` or operator
        /// supplied).
        #[arg(value_name = "SSH_HOST")]
        ssh_host: String,

        /// Workspace repo(s) to clone + register on the worker (`owner/name`).
        /// Repeat for several repos. At least one is required.
        #[arg(long, value_name = "OWNER/NAME", required = true)]
        repo: Vec<String>,

        /// Cross-repo dispatch priority the workspaces are registered at (#3946;
        /// lower = higher priority). Defaults to 100.
        #[arg(long, value_name = "N", default_value_t = loom_daemon::workspace_registry::DEFAULT_WORKSPACE_PRIORITY)]
        priority: u32,

        /// Local path to the operator's fine-grained forge PAT (Contents+Issues+PRs
        /// on the target repos). Read locally at preflight; transferred to the
        /// worker only via ssh stdin. Omit to skip forge auth (skip-with-notice).
        #[arg(long, value_name = "PATH")]
        pat_file: Option<String>,

        /// Local path to the operator's `accounts.env` (the full token pool).
        /// Read locally at preflight; transferred to the worker only via ssh
        /// stdin. Omit to skip token-pool provisioning (skip-with-notice).
        #[arg(long, value_name = "PATH")]
        accounts_env: Option<String>,

        /// Upstream Loom repo URL cloned to the worker's machine-level layout.
        #[arg(long, value_name = "URL", default_value = loom_daemon::fleet::add_worker::DEFAULT_LOOM_REPO_URL)]
        loom_repo: String,

        /// Wire safehouse fleet-comms on the worker (issue #3998): tailnet
        /// join, `safehoused` build/config/room-invite/supervision, then a
        /// restart of the worker's own loom-daemon with `LOOM_SAFEHOUSE_*`
        /// env. Requires every `--safehouse-*` input below; preflight fails
        /// fast (before touching the host) if any is missing.
        #[arg(long)]
        safehouse: bool,

        /// Local path to the operator-minted, ephemeral + `tag:loom-worker`
        /// Tailscale auth key. Read locally at preflight; transferred to the
        /// worker only via ssh stdin. Required with `--safehouse`.
        #[arg(long, value_name = "PATH")]
        safehouse_tailnet_auth_key_file: Option<String>,

        /// Local path to a `KEY=VALUE` env-style file carrying the per-host
        /// Matrix account credentials and store/recovery passphrases
        /// (`SAFEHOUSE_MATRIX_USER_ID`, `SAFEHOUSE_MATRIX_PASSWORD`,
        /// `SAFEHOUSE_STORE_PASSPHRASE`, `SAFEHOUSE_RECOVERY_PASSPHRASE`).
        /// Read locally at preflight; transferred to the worker only via ssh
        /// stdin. Required with `--safehouse`.
        #[arg(long, value_name = "PATH")]
        safehouse_secrets_file: Option<String>,

        /// The external `rjwalters/safehouse` checkout `safehoused` is built
        /// from on the worker.
        #[arg(long, value_name = "URL", default_value = loom_daemon::fleet::add_worker::DEFAULT_SAFEHOUSE_REPO_URL)]
        safehouse_repo_url: String,

        /// The homeserver URL (resolves inside the tailnet) written into
        /// safehoused's config. Not secret. Required with `--safehouse`.
        #[arg(long, value_name = "URL")]
        safehouse_homeserver_url: Option<String>,

        /// The fleet room safehoused joins. Not secret. Required with
        /// `--safehouse`.
        #[arg(long, value_name = "ROOM")]
        safehouse_room: Option<String>,

        /// A persona this host's safehoused boots with (repeat for several).
        /// Mirrors the studio host's allowlist (#3999). Not secret. At least
        /// one is required with `--safehouse` — the allowlist is written
        /// into the boot-time TOML before safehoused's first start
        /// (boot-time-only, no reload).
        #[arg(long = "safehouse-persona", value_name = "NAME")]
        safehouse_personas: Vec<String>,

        /// Override the safehouse#39 room-`invite` op invocation (loom does
        /// not vendor safehoused's argv — owned by the external
        /// `rjwalters/safehouse` repo). Default: `safehoused invite --config
        /// <path>`.
        #[arg(long, value_name = "ARGV")]
        safehouse_invite_exec: Option<String>,

        /// Install an idle-shutdown cron guard that powers the host off after
        /// this many idle minutes (skipping while claude / loom-daemon work).
        #[arg(long, value_name = "MINUTES")]
        idle_shutdown_minutes: Option<u32>,

        /// Print the ordered plan without contacting the host.
        #[arg(long)]
        dry_run: bool,
    },

    /// Aggregate sweep/token/health state across every fleet host, side by
    /// side, including the local host (issue #4342). Reads the fleet registry
    /// #4341 writes, collects the local host's own status in-process (over
    /// the daemon's Unix socket — never `ssh localhost`), and fans out to
    /// every remote worker's `loom-daemon status --json` concurrently, each
    /// bounded by a per-host timeout so one hung host cannot stall the report.
    /// Distinct, loud per-host states (`UP` / `DAEMON DOWN` / `UNREACHABLE` /
    /// `PARSE ERROR` / `DRAINING`) — silence never reads as idle. Exits
    /// non-zero unless every roster host is `UP`.
    Status {
        /// Emit machine-readable JSON instead of the human-readable table.
        #[arg(long)]
        json: bool,
    },

    /// Retire a worker without losing in-flight work, forge claims, or (when
    /// wired) E2E room keys (issue #4343). SSH orchestration over the existing
    /// `restart --drain` primitive (#4090), plus the teardown-specific deltas:
    /// a drain-then-*exit* remote invocation (never restart into new dispatch
    /// on a box about to be powered off), an immediate targeted `loom:building`
    /// claim reset via `gh` (not SSH — the forge is global), a safehoused
    /// key-backup flush check (a supervised `systemctl --user stop
    /// safehoused` IS the flush, #3998 — see `loom_daemon::fleet::drain`'s
    /// module doc), workspace deregistration, and
    /// finally removing the worker from the fleet registry. Idempotent +
    /// resumable: an interrupted drain re-runs from its last completed phase.
    /// Never calls a cloud CLI itself — prints the exact `repo:remote --down`
    /// teardown command instead (epic #4340's boundary).
    Drain {
        /// SSH alias/host to drain (must already be in the fleet registry;
        /// draining a host not in the registry is a clean no-op).
        #[arg(value_name = "SSH_HOST")]
        ssh_host: String,

        /// Max seconds the remote daemon waits for in-flight sweeps to drain.
        #[arg(long, value_name = "SECS", default_value_t = loom_daemon::fleet::drain::DEFAULT_DRAIN_TIMEOUT_SECS)]
        timeout: u64,

        /// On remote drain timeout, force-cancel stragglers
        /// (SIGTERM→grace→SIGKILL) and proceed anyway. Without this, a
        /// timeout refuses and the remote daemon stays running (fail-safe) —
        /// this command then also refuses to proceed past waiting for it to
        /// exit.
        #[arg(long)]
        force_after_timeout: bool,

        /// Emit machine-readable JSON instead of the human-readable report.
        #[arg(long)]
        json: bool,
    },
}

/// Sub-actions for `loom-daemon watch` (Issue #3971).
#[derive(Subcommand)]
enum WatchAction {
    /// Register a durable watch on an issue's or PR's terminal state.
    Add {
        /// The issue or PR number to watch.
        #[arg(value_name = "NUMBER")]
        number: u32,

        /// Watch a pull request instead of an issue.
        #[arg(long)]
        pr: bool,

        /// Forge slug `owner/name` (preferred — works cross-repo for a repo this
        /// machine may not manage). Omit to resolve from `--workspace-root` or the
        /// daemon's own cwd.
        #[arg(long, value_name = "OWNER/NAME")]
        repo: Option<String>,

        /// Workspace root the `gh` query runs in when `--repo` is absent.
        #[arg(long, value_name = "PATH")]
        workspace_root: Option<String>,

        /// Optional note surfaced in the recorded result line.
        #[arg(long, value_name = "TEXT")]
        note: Option<String>,
    },
    /// List the currently-registered durable watches.
    List {
        /// Emit machine-readable JSON instead of the human-readable table.
        #[arg(long)]
        json: bool,
    },
    /// Remove a registered watch by its id.
    Remove {
        /// The watch id (as printed by `watch add` / `watch list`).
        #[arg(value_name = "ID")]
        id: String,
    },
}

/// Sub-actions for `loom-daemon quarantine`.
#[derive(Subcommand)]
enum QuarantineAction {
    /// Clear an issue's insta-crash quarantine (Issue #3939): release the
    /// daemon's in-memory pause + insta-crash tally so the work finder
    /// re-qualifies it immediately (instead of waiting for the TTL) and restore
    /// `loom:issue` on the forge. Idempotent — clearing a non-quarantined issue
    /// is a no-op success.
    Clear {
        /// The issue number whose quarantine to clear.
        #[arg(value_name = "ISSUE")]
        issue: u32,

        /// Target managed-workspace root (Issue #3929). Omit to use the daemon's
        /// default workspace.
        #[arg(long, value_name = "PATH")]
        workspace_root: Option<String>,
    },

    /// List active insta-crash quarantines (Issue #4215): issue, workspace,
    /// insta-crash tally vs threshold, applied-at, and TTL remaining.
    ///
    /// This is the authority for "which issues are quarantined right now" — a
    /// forge `loom:blocked` query is NOT equivalent, because
    /// `apply_quarantine_label` reuses that same label for genuine
    /// dependency-blocked issues, and (since #4206) a TTL-expired quarantine
    /// can leave `loom:blocked` in place after a manual re-park. Prefer this
    /// command over grepping `loom:blocked` when triaging a quarantine wave.
    List {
        /// Target managed-workspace root (Issue #3929). Omit to list
        /// quarantines across EVERY registered workspace — unlike `clear`,
        /// whose omitted `--workspace-root` targets only the daemon's default
        /// workspace (see the `Request::ListQuarantines` doc comment for why
        /// the default differs).
        #[arg(long, value_name = "PATH")]
        workspace_root: Option<String>,
    },
}

/// Sub-actions for `loom-daemon forge`.
///
/// The `issue`/`pr`/`auth` variants capture their trailing args verbatim and
/// (on GitHub) exec `gh <entity> <args…>`, so the surface stays byte-identical
/// to the `FORGE=gh` shell fallback the four scripts already understand.
#[derive(Subcommand)]
enum ForgeAction {
    /// `forge issue <args…>` — e.g. `issue view 42 --json labels --jq
    /// '.labels[].name'`. GitHub: exec `gh issue <args…>`.
    Issue {
        #[arg(
            trailing_var_arg = true,
            allow_hyphen_values = true,
            value_name = "ARGS"
        )]
        args: Vec<String>,
    },
    /// `forge pr <args…>` — e.g. `pr list --state=merged --limit 20 --json
    /// number,title,body`. GitHub: exec `gh pr <args…>`.
    Pr {
        #[arg(
            trailing_var_arg = true,
            allow_hyphen_values = true,
            value_name = "ARGS"
        )]
        args: Vec<String>,
    },
    /// `forge auth <args…>` — e.g. `auth status`. GitHub: exec `gh auth
    /// <args…>`.
    Auth {
        #[arg(
            trailing_var_arg = true,
            allow_hyphen_values = true,
            value_name = "ARGS"
        )]
        args: Vec<String>,
    },
    /// `forge auto-merge <pr> [--method M]` — enable auto-merge for a PR
    /// (formerly `loom-auto-merge`). GitHub: `enablePullRequestAutoMerge`
    /// GraphQL mutation. Gitea: declines (exit 3) → shell `forge_auto_merge`.
    /// `--poll-interval` / `--timeout` are accepted for CLI compatibility and
    /// ignored on GitHub (the server queues the merge).
    #[command(name = "auto-merge")]
    AutoMerge {
        /// Pull request number.
        #[arg(value_name = "PR")]
        pr_number: u32,

        /// Merge method (squash | merge | rebase). Default squash.
        #[arg(long, default_value = "squash")]
        method: String,

        /// Seconds between CI polls (Gitea shell path only). Accepted for
        /// compatibility; unused on the GitHub native path.
        #[arg(long, value_name = "SECONDS")]
        poll_interval: Option<u64>,

        /// Max seconds to wait for CI (Gitea shell path only). Accepted for
        /// compatibility; unused on the GitHub native path.
        #[arg(long, value_name = "SECONDS")]
        timeout: Option<u64>,
    },
}

/// Sub-actions for `loom-daemon tokens`.
#[derive(Subcommand)]
enum TokensAction {
    /// Select an OAuth token from the pool using the 3-tier algorithm
    /// (ranking -> allowlist -> random), skipping bad-marked tokens at every
    /// tier. Native Rust port of `python3 -m loom_tools.tokens.select`,
    /// invoked directly by `spawn-claude.sh` / `claude-wrapper.sh` as of
    /// issue #4228 (epic #4081 Phase 2) — the Python selector is no longer on
    /// the token hot path.
    Select {
        /// Repo root containing `.loom/tokens/` (the canonical main-checkout
        /// root when called from a worktree — no upward `.git` walk).
        #[arg(long, value_name = "PATH", default_value = ".")]
        workspace: String,

        /// Account provider. The default preserves the legacy Claude token
        /// selector and every one of its state formats.
        #[arg(long, value_name = "PROVIDER", default_value = "claude")]
        provider: String,

        /// Emit shell-evalable `export CLAUDE_CODE_OAUTH_TOKEN=...` /
        /// `export LOOM_TOKEN_NAME=...` lines (plus a non-exported
        /// `LOOM_TOKEN_MODE=...` assignment) instead of JSON — designed to be
        /// consumed via `eval "$(loom-daemon tokens select --export ...)"`.
        #[arg(long)]
        export: bool,

        /// Omit the secret key from output (safe inspection).
        #[arg(long)]
        no_key: bool,

        /// Pre-flight (issue #4228): if every allowlisted (pinned) account
        /// has hit the consecutive-failure threshold, clear `.allowlist` and
        /// reset `.failure_counts` before selecting, rather than trap the
        /// spawner on exhausted pinned accounts. Mirrors the inline Python
        /// heredoc `spawn-claude.sh` historically ran ahead of selection; a
        /// firing auto-unpin logs an `[auto-unpin] ...` advisory to stderr.
        #[arg(long)]
        auto_unpin: bool,
    },

    /// Materialize `.loom/tokens/` from `ACCOUNT_*_N` triples, merging by email
    /// with precedence claude-monitor (`~/.claude-monitor/accounts.env`,
    /// primary) > repo-local. The home master (`~/.loom/accounts.env`) is
    /// opt-in only: read solely when `$LOOM_ACCOUNTS_ENV` (or `--home-env`)
    /// points at it. `ACCOUNT_TOKEN_FILE_N` is optional — auto-derived from
    /// `ACCOUNT_EMAIL_N` when omitted. Native Rust port of the historical Python
    /// `loom-tokens bootstrap` CLI (issue #4105, epic #4081).
    Bootstrap {
        /// Repo root (plain path, default `.` — no upward `.git` walk). The
        /// pool is written to `<workspace>/.loom/tokens` unless `--shared`.
        #[arg(long, value_name = "PATH", default_value = ".")]
        workspace: String,

        /// Path to the repo-local account source (default:
        /// `<repo>/.loom/accounts.env` if present, else `<repo>/.env`).
        #[arg(long, value_name = "PATH")]
        env: Option<String>,

        /// Path to the home-dir master account source. Opt-in only; with no
        /// flag the home master is read only when `$LOOM_ACCOUNTS_ENV` points
        /// at a file.
        #[arg(long, value_name = "PATH")]
        home_env: Option<String>,

        /// Ignore the home master; bootstrap from the repo-local source only.
        #[arg(long)]
        no_home: bool,

        /// Materialize the SHARED machine-level pool at `~/.loom/tokens`
        /// (override with `$LOOM_SHARED_TOKENS_DIR`) instead of the repo-local
        /// `<repo>/.loom/tokens`.
        #[arg(long)]
        shared: bool,

        /// Overwrite existing token files even if their fingerprint matches.
        #[arg(long)]
        force: bool,

        /// Report what would change without writing any files.
        #[arg(long)]
        dry_run: bool,

        /// Emit a JSON summary on stdout.
        #[arg(long)]
        json: bool,
    },

    /// Materialize `.loom/tokens/` from claude-monitor's LIVE credential store
    /// (`~/.claude-monitor/usage.db`) instead of the `accounts.env` snapshot.
    /// Use this after rolling accounts: the snapshot keeps the old (now
    /// revoked) tokens, so `bootstrap --force` would rewrite them unchanged.
    /// Native Rust port of the historical Python `loom-tokens
    /// import-from-monitor` CLI (issue #4106, epic #4081).
    ImportFromMonitor {
        /// Repo root (plain path, default `.` — no upward `.git` walk). The
        /// pool is written to `<workspace>/.loom/tokens` unless `--shared`.
        #[arg(long, value_name = "PATH", default_value = ".")]
        workspace: String,

        /// Import into the SHARED machine-level pool at `~/.loom/tokens`
        /// (override with `$LOOM_SHARED_TOKENS_DIR`) instead of the
        /// repo-local `<repo>/.loom/tokens`.
        #[arg(long)]
        shared: bool,

        /// Path to claude-monitor's `usage.db` (default: `<claude-monitor
        /// dir>/usage.db`, honoring `$LOOM_CLAUDE_MONITOR_DIR`).
        #[arg(long, value_name = "PATH")]
        db: Option<String>,

        /// Overwrite on-disk tokens that differ from the store. Required to
        /// apply rolled tokens, since every rolled token differs by design.
        #[arg(long)]
        force: bool,

        /// Delete `*.token` files for accounts claude-monitor no longer
        /// reports active (pool state files are never touched).
        #[arg(long)]
        prune: bool,

        /// Report what would change without writing any files.
        #[arg(long)]
        dry_run: bool,

        /// Emit a JSON summary on stdout.
        #[arg(long)]
        json: bool,
    },

    /// Probe each bootstrapped account for rate-limit headers and rank by
    /// available quota, optionally writing `.loom/tokens/.ranking`. A
    /// byte-compatible port of the historical Python `loom-tokens check` CLI
    /// (issue #4108) — as of #4080 this is also what `probe-tokens.sh` and
    /// the daemon's own ranking self-refresh invoke natively. The HTTP probe
    /// shells to `curl` (no HTTP-client crate — see `tokens_pool::check`).
    Check {
        /// Repo root containing `.loom/tokens/` (plain path, default `.` — no
        /// upward `.git` walk).
        #[arg(long, value_name = "PATH", default_value = ".")]
        workspace: String,

        /// Write `.loom/tokens/.ranking` atomically (consumed by the spawn
        /// wrapper, #3235).
        #[arg(long)]
        ranking: bool,

        /// Where to source the ranking (#3697): `auto` (default) uses
        /// claude-monitor's `ranking.json` when fresh, else probes; `monitor`
        /// uses claude-monitor only (no probe); `probe` always live-probes.
        /// Overrides `$LOOM_RANKING_SOURCE`.
        #[arg(long, value_name = "SOURCE")]
        source: Option<String>,

        /// Override the probe prompt (default `"hi"`). The probe always uses
        /// `max_tokens=1` regardless of prompt.
        #[arg(long, value_name = "TEXT")]
        probe_prompt: Option<String>,

        /// Emit the full report as JSON to stdout (instead of a human table).
        #[arg(long)]
        json: bool,

        /// Skip the 0.5-1.5s jitter between probes (mostly for tests).
        #[arg(long)]
        no_stagger: bool,
    },

    /// Manage the `.allowlist` file constraining which accounts `select` may
    /// pick.
    Pin {
        #[command(subcommand)]
        action: PinAction,

        /// Repo root containing `.loom/tokens/`.
        #[arg(long, value_name = "PATH", default_value = ".", global = true)]
        workspace: String,
    },

    /// Clear the allowlist (all accounts become eligible).
    Unpin {
        /// Repo root containing `.loom/tokens/`.
        #[arg(long, value_name = "PATH", default_value = ".")]
        workspace: String,

        /// Emit a JSON status instead of a human message.
        #[arg(long)]
        json: bool,
    },

    /// Remove auth-reason entries for the given accounts from `.bad_tokens`
    /// (e.g. after re-authenticating).
    Unblock {
        /// Account names to unblock (exact match).
        #[arg(required = true, value_name = "NAME")]
        names: Vec<String>,

        /// Repo root containing `.loom/tokens/`.
        #[arg(long, value_name = "PATH", default_value = ".")]
        workspace: String,

        /// Also drop non-auth entries (TTL-style, exhausted/expired).
        /// Default is auth-reason only.
        #[arg(long)]
        all_reasons: bool,

        /// Emit JSON status.
        #[arg(long)]
        json: bool,
    },

    /// Append a bad-token entry to `.bad_tokens` for `name`. Native Rust CLI
    /// exposure of the existing `tokens_pool::bad_tokens::mark_bad` library
    /// function (issue #4228, epic #4081 Phase 2) — closes the last gap that
    /// kept `claude-wrapper.sh`'s account-rotation path on an inline Python
    /// heredoc. Byte-compatible with the historical Python `mark_bad`: the
    /// reason is newline-sanitized (embedded `\n`/`\r` collapsed to spaces)
    /// so every `.bad_tokens` entry is exactly one line.
    MarkBad {
        /// Account name (token file stem, no extension) to mark bad.
        #[arg(value_name = "NAME")]
        name: String,

        /// Free-form reason recorded alongside the timestamp + name.
        #[arg(long, value_name = "TEXT", default_value = "")]
        reason: String,

        /// Repo root containing `.loom/tokens/`.
        #[arg(long, value_name = "PATH", default_value = ".")]
        workspace: String,

        /// Emit a JSON status instead of a human message.
        #[arg(long)]
        json: bool,
    },
}

/// Sub-actions for `loom-daemon claude-config` (issue #4415).
#[derive(Subcommand)]
enum ClaudeConfigAction {
    /// Create (or refresh) an isolated `CLAUDE_CONFIG_DIR` for `name`.
    /// Prints the resulting config dir path on success.
    Setup {
        /// Agent/session name (e.g. "builder-1").
        #[arg(value_name = "NAME")]
        name: String,

        /// Repo root the config dir is created under
        /// (`<workspace>/.loom/claude-config/<name>`).
        #[arg(long, value_name = "PATH", default_value = ".")]
        workspace: String,

        /// Emit `{"config_dir": "..."}` instead of the bare path.
        #[arg(long)]
        json: bool,
    },

    /// Remove one agent's config directory. Exits 0 whether or not it
    /// existed (non-fatal-if-missing).
    Cleanup {
        #[arg(value_name = "NAME")]
        name: String,

        #[arg(long, value_name = "PATH", default_value = ".")]
        workspace: String,

        /// Emit `{"removed": bool}` instead of a human message.
        #[arg(long)]
        json: bool,
    },

    /// Validate that an agent's config directory is present and healthy.
    /// Exits 0 when healthy, 1 when missing/corrupted (never spawns a
    /// process to check — see issue #2909).
    Validate {
        #[arg(value_name = "NAME")]
        name: String,

        #[arg(long, value_name = "PATH", default_value = ".")]
        workspace: String,

        /// Emit `{"healthy": bool}` instead of a human message.
        #[arg(long)]
        json: bool,
    },

    /// Pre-seed the folder-trust modal for a non-interactive spawn target
    /// (issue #4334) so the role command isn't delivered as keystrokes into
    /// the "Is this a project you trust?" dialog. Idempotent no-op if
    /// already trusted. Gated by `LOOM_AUTO_TRUST` (default enabled).
    Trust {
        /// Exact spawn target directory to mark trusted (the worktree path
        /// when spawning into a worktree, not just the repo root).
        #[arg(long, value_name = "PATH")]
        project_dir: String,
    },
}

/// Sub-actions for `loom-daemon tokens pin`.
#[derive(Subcommand)]
enum PinAction {
    /// Replace the allowlist with exactly the given accounts.
    Set {
        #[arg(required = true, value_name = "NAME")]
        names: Vec<String>,
    },
    /// Add account(s) to the existing allowlist.
    Add {
        #[arg(required = true, value_name = "NAME")]
        names: Vec<String>,
    },
    /// Remove account(s) from the allowlist.
    Remove {
        #[arg(required = true, value_name = "NAME")]
        names: Vec<String>,
    },
    /// Show the current allowlist and all available accounts.
    Status {
        /// Emit JSON instead of a human table.
        #[arg(long)]
        json: bool,
    },
}

/// Sub-actions for `loom-daemon cleanup`.
#[derive(Subcommand)]
enum CleanupAction {
    /// Archive task outputs and prune old archives (delegates to
    /// `archive-logs.sh`; the only surviving cleanup.py functionality).
    Logs {
        #[arg(long, value_name = "PATH", default_value = ".")]
        workspace: String,

        #[arg(long)]
        dry_run: bool,

        /// Skip new archival; only prune archives older than retention.
        #[arg(long)]
        prune_only: bool,

        /// Override `LOOM_RETENTION_DAYS` (default: 7).
        #[arg(long, value_name = "N")]
        retention_days: Option<i64>,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    // Handle CLI commands (init mode)
    if let Some(command) = cli.command {
        return match command {
            // `status` connects to the running daemon over its Unix socket, so
            // it needs the async runtime (unlike the other sync subcommands).
            Commands::Status { json, pipeline } => handle_status_command(json, pipeline).await,
            // `quarantine` connects to the running daemon over its Unix socket
            // (the quarantine state is in-memory), so it needs the async runtime.
            Commands::Quarantine { action } => handle_quarantine_command(action).await,
            // `dispatch` connects to the running daemon over its Unix socket to
            // enqueue a sweep (Issue #3952), so it needs the async runtime.
            Commands::Dispatch {
                issue,
                workspace,
                model,
                effort,
                depends_on,
                force,
            } => handle_dispatch_command(issue, workspace, model, effort, depends_on, force).await,
            // `watch` connects to the running daemon over its Unix socket to
            // register/list/remove durable watches (Issue #3971).
            Commands::Watch { action } => handle_watch_command(action).await,
            // `serve` binds a local HTTP listener and, per request, connects to
            // the running daemon over its Unix socket for a fresh `DaemonStatus`
            // snapshot (Issue #4391), so it needs the async runtime.
            Commands::Serve {
                port,
                bind,
                allow_non_loopback,
                peers,
            } => handle_serve_command(port, &bind, allow_non_loopback, &peers).await,
            // `restart` connects to the running daemon over its Unix socket to
            // trigger the supervised restart primitive (Issue #4054), or a
            // scheduled drain-and-restart (Issue #4090).
            Commands::Restart {
                drain,
                timeout,
                force_after_timeout,
                abort_drain,
                then_exit,
            } => {
                handle_restart_command(drain, timeout, force_after_timeout, abort_drain, then_exit)
                    .await
            }
            // `fleet status` collects the local host's own status over the
            // daemon's Unix socket (issue #4342), so — unlike `fleet
            // add-worker`, which is pure ssh/filesystem and stays on the sync
            // `handle_cli_command` path — it needs the async runtime too.
            Commands::Fleet {
                action: FleetAction::Status { json },
            } => handle_fleet_status_command(json).await,
            other => handle_cli_command(other),
        };
    }

    // Setup logging to ~/.loom/daemon.log
    setup_logging()?;

    // Check tmux
    check_tmux_installed()?;

    // Setup loom directory and socket path
    // For testing, allow override via LOOM_SOCKET_PATH env var
    let (loom_dir, socket_path) = if let Ok(path) = std::env::var("LOOM_SOCKET_PATH") {
        // For testing, use the parent directory of the provided socket path
        let socket_path = std::path::PathBuf::from(path);
        let loom_dir = resolve_loom_dir()?;
        (loom_dir, socket_path)
    } else {
        let loom_dir = resolve_loom_dir()?;
        fs::create_dir_all(&loom_dir)?;
        let socket_path = loom_dir.join("loom-daemon.sock");
        (loom_dir, socket_path)
    };

    // Initialize activity database
    let db_path = loom_dir.join("activity.db");
    let activity_db = ActivityDb::new(db_path.clone())?;
    log::info!("Activity database initialized");

    // Crash recovery: Release stale claims on startup (Issue #1159)
    // Claims older than 1 hour without heartbeat are considered stale
    let stale_threshold_secs = std::env::var("LOOM_CLAIM_TTL_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(3600); // Default: 1 hour

    match activity_db.release_stale_claims(stale_threshold_secs) {
        Ok(count) if count > 0 => {
            log::warn!(
                "Crash recovery: Released {count} stale claims (older than {stale_threshold_secs}s)"
            );
        }
        Ok(_) => {
            log::debug!("No stale claims to release on startup");
        }
        Err(e) => {
            log::warn!("Failed to release stale claims on startup: {e}");
        }
    }

    let activity_db = Arc::new(Mutex::new(activity_db));

    // Load configured terminal IDs for config-based session filtering (Issue #1952)
    // This prevents importing stale sessions from crashed daemons or other instances
    let workspace_from_env = std::env::var("LOOM_WORKSPACE").ok();
    let configured_ids = workspace_from_env
        .as_ref()
        .and_then(|workspace| extract_configured_terminal_ids(Path::new(workspace)));

    // Initialize terminal manager and clean up stale sessions
    let mut tm = TerminalManager::new();

    // Use config-based filtering if workspace config is available
    if let Some(ref ids) = configured_ids {
        tm.restore_from_tmux_with_filter(Some(ids))?;
    } else {
        // Fall back to legacy behavior (import all) when no config available
        log::warn!("No workspace config found - using legacy restore (all sessions)");
        tm.restore_from_tmux()?;
    }
    log::info!("Restored {} terminals", tm.list_terminals().len());

    match tm.clean_stale_sessions() {
        Ok(0) => log::debug!("No stale tmux sessions to clean"),
        Ok(count) => log::info!("Cleaned {count} stale tmux session(s) from previous run"),
        Err(e) => log::warn!("Failed to clean stale tmux sessions: {e}"),
    }

    let tm = Arc::new(Mutex::new(tm));

    // Start health monitoring (enabled by default)
    if let Some(interval) = health_monitor::check_env_enabled() {
        let (_health_handle, _health_state) = health_monitor::start_tmux_health_monitor(interval);
        log::info!("tmux health monitoring enabled (interval: {interval}s)");
        // Note: health_handle is dropped here, but the thread keeps running
        // health_state could be stored for querying crash status if needed
    }

    // Start GitHub metrics collection (if workspace is set)
    // Workspace can be set via LOOM_WORKSPACE environment variable (reuse variable from above)
    let db_path_str = db_path.to_str().map(std::string::ToString::to_string);

    if let (Some(workspace), Some(db_path_string)) = (workspace_from_env.as_deref(), db_path_str) {
        let _metrics_handle =
            metrics_collector::try_init_metrics_collector(Some(workspace), &db_path_string);
        // Note: metrics_handle is dropped here, but the thread keeps running if enabled
    }

    // Initialize the sweep registry (Issue #3452 — Phase A of #3449).
    // The registry tracks `/loom:sweep` children dispatched via the
    // `DispatchSweep` IPC request. It writes no daemon-side state file;
    // recovery on restart relies on lock dirs + sweep checkpoints + the
    // forge (labels). The reaper task polls live PIDs on a configurable
    // interval (`LOOM_SWEEP_REAPER_INTERVAL_SECS`, default 30s).
    //
    // `sweep_workspace` seeds the *default* registry only. As of #4299, an
    // explicit-`workspace_root`-absent `DispatchSweep`/`dispatch_sweep`
    // request no longer blindly trusts this cwd-derived value as its target:
    // `ipc::resolve_dispatch_registry` consults the on-disk workspace
    // registry first and only falls back to this default when the registry
    // is empty or this root is itself registered. See that function's doc
    // comment for the full precedence.
    let sweep_workspace = workspace_from_env
        .as_ref()
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    let sweep_config = SweepRegistryConfig::new(sweep_workspace.clone());

    // Phase B (#3453): construct the in-memory pub/sub event bus *before*
    // the sweep registry so we can wire it in at construction time. The
    // bus is shared between the registry (publisher for reaper + dispatch
    // events) and the IPC server (publisher for `PublishEvent` requests
    // from sweep children, plus consumer for `SubscribeEvents` streams).
    let event_bus = Arc::new(EventBus::new());
    log::info!("event_bus: started in-memory pub/sub (capacity={})", event_bus.capacity());

    let mut sweep = SweepRegistry::with_event_bus(sweep_config, event_bus.clone());
    match sweep.reconstruct() {
        Ok(0) => log::debug!("sweep_registry: no sweeps to reconstruct"),
        Ok(n) => log::info!(
            "sweep_registry: reconstructed {n} sweep entr{}",
            if n == 1 { "y" } else { "ies" }
        ),
        Err(e) => log::warn!("sweep_registry: reconstruction failed: {e}"),
    }

    // Startup forge-credential preflight (Issue #4005; GitHub App identity
    // mechanism added by #4430). Resolved once, here — immediately before the
    // claim-reconciliation pass below, the daemon's first `gh` consumer — so
    // a headless/SSH-only start with neither an exported GH_TOKEN/GITHUB_TOKEN
    // nor an unlockable GUI login keychain is diagnosed loudly at boot
    // instead of surfacing as silent per-tick 401s for the life of the
    // process. Non-fatal and bounded (same posture as the reconciliation pass
    // itself): a `gh` hiccup here never blocks startup.
    //
    // #4430: when a GitHub App id + private key are configured (env or
    // `forge.githubApp.*` config — see `defaults/scripts/lib/github-app-token.sh`),
    // this mints a short-lived installation token and exports it as this
    // process's own `GH_TOKEN`, so every `gh`/`git` child the daemon spawns
    // from this point on inherits it. Absent that config, `owner_repo`
    // resolves fine but the shell helper reports "not_configured" and
    // `run_with_github_app` falls through to the byte-identical pre-#4430
    // `run(...)` path below — no behavior change for any host that hasn't
    // opted in.
    let credential_preflight_probe = credential_preflight::RealGhAuthProbe {
        gh_bin: "gh".to_string(),
        cwd: sweep_workspace.clone(),
    };
    let github_app_script = credential_preflight::resolve_github_app_script(&sweep_workspace);
    let github_app_owner_repo = credential_preflight::nwo_from_git_remote(&sweep_workspace);
    let github_app_preflight = match &github_app_script {
        Some(script_path) => {
            let minter = credential_preflight::RealGithubAppMinter {
                script_path: script_path.clone(),
                cwd: sweep_workspace.clone(),
            };
            credential_preflight::run_with_github_app(
                &credential_preflight_probe,
                &minter,
                github_app_owner_repo.as_deref(),
            )
        }
        // No `github-app-token.sh` on disk at all (stale install predating
        // #4430, or a workspace root with no `.loom`/`defaults` tree) —
        // exactly the pre-#4430 path, with zero extra subprocess overhead.
        None => credential_preflight::GithubAppPreflight {
            report: credential_preflight::run(&credential_preflight_probe),
            minted_gh_token: None,
        },
    };
    if let Some(token) = &github_app_preflight.minted_gh_token {
        // NEVER logged: only the fingerprint in `github_app_preflight.report`
        // is. Exported into this process's own env so every `gh`/`git`
        // child spawned from here on (Command::new without env_clear)
        // inherits it.
        std::env::set_var("GH_TOKEN", token);
    }
    let credential_preflight = github_app_preflight.report;

    // #4430: keep the minted installation token fresh across its ~1h
    // lifetime for a long-running daemon. A no-op tick (unconfigured, or the
    // shell helper's own cache still has >10min left) costs a handful of
    // cheap subprocess execs and never touches `GH_TOKEN`; a genuine mint
    // failure is logged and the loop keeps whatever `GH_TOKEN` the process
    // already has (fail-open, matching the startup preflight's posture).
    if let (Some(script_path), Some(owner_repo)) = (&github_app_script, &github_app_owner_repo) {
        let script_path = script_path.clone();
        let cwd = sweep_workspace.clone();
        let owner_repo = owner_repo.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(credential_preflight::GITHUB_APP_REFRESH_INTERVAL).await;
                let script_path = script_path.clone();
                let cwd = cwd.clone();
                let owner_repo = owner_repo.clone();
                let outcome = tokio::task::spawn_blocking(move || {
                    let minter = credential_preflight::RealGithubAppMinter { script_path, cwd };
                    credential_preflight::GithubAppMinter::mint(&minter, &owner_repo)
                })
                .await;
                match outcome {
                    Ok(credential_preflight::GithubAppOutcome::Minted {
                        token,
                        installation_id,
                        app_id,
                        ..
                    }) => {
                        std::env::set_var("GH_TOKEN", token);
                        log::debug!(
                            "credential_preflight: github-app refresh tick minted a token \
                             (app {app_id} installation {installation_id}) — #4430"
                        );
                    }
                    Ok(credential_preflight::GithubAppOutcome::NotConfigured) => {
                        // Cheap no-op tick -- nothing to refresh.
                    }
                    Ok(credential_preflight::GithubAppOutcome::Error(reason)) => {
                        log::warn!(
                            "credential_preflight: github-app refresh tick failed ({reason}); \
                             GH_TOKEN left unchanged — #4430"
                        );
                    }
                    Err(e) => {
                        log::warn!(
                            "credential_preflight: github-app refresh task join error: {e} — #4430"
                        );
                    }
                }
            }
        });
    }

    // GitHub rate-limit circuit breaker (#4429). Registered here — before the
    // startup reconciliation passes just below — because *every* gh-polling
    // consumer (reconciliation, work-finder, epic supervisor, role runner)
    // starts after this point and consults the global handle. Unlike the host
    // breaker (registered inside the work-finder branch, which is its sole
    // sampler), rate-limit failures can be observed by any of those loops, so
    // the breaker exists whether or not the work-finder is enabled.
    let rate_limit_config = rate_limit_breaker::resolve_config_for(&sweep_workspace);
    rate_limit_breaker::register_global(std::sync::Arc::new(
        rate_limit_breaker::SharedRateLimitBreaker::new(rate_limit_config),
    ));
    log::info!(
        "rate_limit_breaker: enabled={} (fallback_cooldown_secs={}) — forge polling pauses \
         until the API window resets when the shared rate limit is exhausted (#4429)",
        rate_limit_config.enabled,
        rate_limit_config.fallback_cooldown_secs,
    );

    // Stale-`loom:building`-claim reconciliation across every managed
    // workspace (Issue #3953). `sweep.reconstruct()` above only recovers
    // entries this registry itself owns evidence for (locks/checkpoints); it
    // has nothing to say about a claim left behind by a sweep that died
    // between the previous daemon's exit and this one's start (rate-limit
    // kill, print-mode ceiling, an operator upgrade — the daemon-restart
    // gap where `loom-recover-orphans` used to find *no* authoritative
    // liveness source at all and refuse to reclaim anything, #3651).
    //
    // The machine-level sweep journal (`~/.loom/sweeps.json`, written by
    // every `dispatch()` — see `sweep_journal`) is that liveness source, and
    // it survives exactly the restart that wipes this in-memory registry.
    // This pass is a bounded, logged, best-effort sweep over every
    // `effective_roots()` workspace (empty registry ⇒ just this one). It
    // never blocks daemon startup — a `gh` hiccup in one repo is logged and
    // skipped, and the remaining repos are still reconciled.
    //
    // Promoted from a startup-only pass to ALSO run on an interval (Issue
    // #4348): a daemon restart is not the only way a `loom:building` claim's
    // sweep can die. A manually/externally spawned detached sweep killed by
    // an external `SIGKILL` (the incident that motivated #4348) never writes
    // the journal entry above, but its checkpoint's `task_id` joined against
    // `.loom/sweep-run/<task_id>.json` gives `claim_reconciliation` the same
    // provable-death answer — see that module's doc comment for the full
    // evidence-source precedence. `run_reconciliation_pass` (this call) and
    // the periodic task spawned just below share one implementation, so the
    // startup and periodic behavior are identical by construction.
    claim_reconciliation::run_reconciliation_pass(&sweep_workspace);
    let _claim_reconciliation_handle =
        claim_reconciliation::spawn_periodic_reconciliation_task(sweep_workspace.clone());

    // Stranded-quarantine reconciliation across every managed workspace
    // (Issue #4110). The insta-crash quarantine (#3939) is memory-only, so a
    // restart drops the in-memory pause while the `loom:blocked` label it
    // applied survives on the forge — with nothing left to release it, the
    // issue is permanently invisible to the work finder. This pass scans
    // every registered workspace's open `loom:blocked` issues and releases
    // the ones carrying a daemon-authored quarantine comment back to
    // `loom:issue`; a human's manual `loom:blocked` (no such comment) is
    // never touched. Reuses the same `workspace_registry` roots resolved
    // above for claim reconciliation.
    if quarantine_reconciliation::reconciliation_enabled() {
        let workspace_registry =
            loom_daemon::workspace_registry::WorkspaceRegistry::load_default().unwrap_or_default();
        let roots = workspace_registry.effective_roots(&sweep_workspace);
        let gh_bin = std::path::PathBuf::from("gh");
        let mut total_checked = 0usize;
        let mut total_released = 0usize;
        for root in &roots {
            let (checked, released) =
                quarantine_reconciliation::forge::reconcile_workspace(&gh_bin, root);
            total_checked += checked;
            total_released += released;
        }
        if total_released > 0 {
            log::info!(
                "quarantine_reconciliation: startup pass checked {total_checked} loom:blocked \
                 issue(s) across {} workspace(s), released {total_released} stranded \
                 quarantine(s) (#4110)",
                roots.len()
            );
        } else {
            log::debug!(
                "quarantine_reconciliation: startup pass checked {total_checked} loom:blocked \
                 issue(s) across {} workspace(s), nothing to release",
                roots.len()
            );
        }
    } else {
        log::info!(
            "quarantine_reconciliation: startup pass disabled ({}=0)",
            quarantine_reconciliation::RECONCILE_ENABLED_ENV
        );
    }

    // Startup-race mitigation (Issue #3887): resolve the dispatch stagger + the
    // watchdog knobs from `.loom/config.json → autonomous` with env override
    // (precedence env > config > default). The stagger serializes back-to-back
    // child startups so a burst dispatch does not trip the 0-HTTPS MCP-init
    // race; the watchdog is the self-healing backstop for any hang that slips
    // past it.
    let startup_race_config = sweep_registry::read_startup_race_config(&sweep_workspace);
    let dispatch_stagger = sweep_registry::resolve_dispatch_stagger(&startup_race_config);
    sweep.set_dispatch_stagger(dispatch_stagger);
    log::info!("sweep_registry: dispatch stagger = {}ms (#3887)", dispatch_stagger.as_millis());

    // Startup-proof occupancy grace (Issue #4003): a freshly-dispatched sweep
    // counts toward the work-finder's admission budget regardless of progress
    // for this long; past it, a sweep with zero observed startup-proof signal
    // (no worktree/checkpoint/log-past-header) stops consuming a slot, well
    // before the (unchanged) 300s startup watchdog would cancel/re-dispatch it.
    let startup_proof_grace = sweep_registry::resolve_startup_proof_grace(&startup_race_config);
    sweep.set_startup_proof_grace(startup_proof_grace);
    log::info!(
        "sweep_registry: startup-proof occupancy grace = {}s (#4003)",
        startup_proof_grace.as_secs()
    );

    // Insta-crash quarantine (#3939): resolve env > config > default for the
    // default workspace so the reaper quarantines a repeatedly-insta-crashing
    // issue instead of letting the work finder re-dispatch it every tick.
    let quarantine_config = sweep_registry::resolve_quarantine_config(&sweep_workspace);
    sweep.set_quarantine_config(quarantine_config);
    log::info!(
        "sweep_registry: insta-crash quarantine {} (threshold={}, ttl={}s, insta-crash<{}s) (#3939)",
        if quarantine_config.enabled { "enabled" } else { "disabled" },
        quarantine_config.threshold,
        quarantine_config.ttl.as_secs(),
        quarantine_config.insta_crash_secs
    );

    // Claude-wrapper pre-flight-death workspace tripwire (#4386): resolve
    // env > config > default for the default workspace so a fleet-wide,
    // cross-issue spawn failure (e.g. a stale `.mcp.json`) trips a visible
    // advisory instead of reading as an idle-healthy daemon.
    let preflight_tripwire_config =
        sweep_registry::resolve_preflight_tripwire_config(&sweep_workspace);
    sweep.set_preflight_tripwire_config(preflight_tripwire_config);
    log::info!(
        "sweep_registry: pre-flight-death tripwire threshold={} (#4386)",
        preflight_tripwire_config.threshold
    );

    // Cross-host collision detection (#4085, Phase 0 of #4028): resolve env >
    // config > default(off) for the default workspace so a shared-backlog
    // deployment can measure the baseline duplicate-dispatch rate. Detection
    // only — a detected collision is logged/counted, never acted on.
    let detect_collisions = sweep_registry::resolve_collision_detection(&sweep_workspace);
    sweep.set_collision_detection(detect_collisions);
    log::info!(
        "sweep_registry: cross-host collision detection {} (#4085)",
        if detect_collisions {
            "enabled"
        } else {
            "disabled"
        }
    );

    let sweep_registry = Arc::new(Mutex::new(sweep));
    let _reaper_handle = sweep_registry::spawn_reaper_task(sweep_registry.clone());

    // Multi-workspace sweep-registry pool (Issue #3928 — phase b of #3835/#3926).
    // Both autonomous loops (work-finder + epic supervisor) share ONE pool so a
    // given repo has exactly one `SweepRegistry` instance — unifying in-flight
    // dedup and the reaper across both loops. The pool captures a handle to this
    // shared daemon runtime so every provisioned registry's reaper runs here even
    // when a workspace is first touched from the epic supervisor's OS thread. The
    // default workspace is *seeded* with the registry constructed above (also used
    // by the IPC `DispatchSweep` path), so the empty-registry case reuses it
    // byte-for-byte rather than building a second instance.
    let workspace_pool =
        Arc::new(WorkspacePool::new(event_bus.clone(), tokio::runtime::Handle::current()));
    workspace_pool.seed(sweep_workspace.clone(), sweep_registry.clone());

    // Optional safehouse fleet-comms narration (#3997): subscribe the shared
    // event bus and narrate sweep-lifecycle transitions into an E2E Matrix room.
    // Byte-for-byte no-op when `safehouse.enabled` is false/absent.
    workspace_pool.start_safehouse_narration(&sweep_workspace);

    // Optional cross-host soft-claim coordination (#4028): a dedicated safehouse
    // connection that advertises this daemon's dispatch claims and consumes peer
    // advertisements into a shared TTL-bounded view, so peer daemons back off
    // before the non-atomic `loom:building` label flip would let them race.
    // Byte-for-byte no-op when `safehouse.enabled` is false/absent. Injected into
    // the seeded default registry here (the IPC `DispatchSweep` path); every other
    // provisioned registry is injected in `get_or_provision`.
    workspace_pool.start_peer_coordination(&sweep_workspace);
    {
        let mut reg = sweep_registry
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        workspace_pool.inject_peer_coordination(&mut reg);
    }

    // Startup watchdog (Issue #3887): auto-cancel + re-dispatch (once, bounded)
    // any daemon-dispatched sweep that hangs at startup with no progress. On by
    // default; disable with LOOM_SWEEP_WATCHDOG=0 or
    // `autonomous.watchdog.enabled = false`.
    let _watchdog_handle = if sweep_registry::resolve_watchdog_enabled(&startup_race_config) {
        let timeout = sweep_registry::resolve_watchdog_timeout(&startup_race_config);
        let interval = sweep_registry::resolve_watchdog_interval(&startup_race_config);
        // Review-phase stall watchdog (Issue #3910): a third backstop, resolved
        // independently (env > config > default, defaults on) and threaded into
        // the same tick. `None` disables it without touching the startup
        // watchdog. It catches a still-running sweep wedged in a hung
        // Judge/Doctor subagent (log silent past the stall timeout).
        let review_stall_timeout =
            if sweep_registry::resolve_review_stall_enabled(&startup_race_config) {
                Some(sweep_registry::resolve_review_stall_timeout(&startup_race_config))
            } else {
                log::info!("sweep_registry: review-phase stall watchdog disabled (#3910)");
                None
            };
        Some(sweep_registry::spawn_watchdog_task(
            sweep_registry.clone(),
            timeout,
            interval,
            review_stall_timeout,
        ))
    } else {
        log::info!("sweep_registry: startup watchdog disabled (#3887)");
        None
    };

    // Shared reactive main-health halt flag (Issue #3812 — Phase C of epic
    // #3809). Constructed here — before both the epic supervisor and the
    // work-finder — so it can be threaded into both dispatch paths. When the gate
    // loop (below) is disabled nothing ever flips it, so neither the supervisor
    // nor the work-finder is ever halted (zero behavior change with the gate off).
    // Per-repo halt state (#3930): one `MainHealthState` per registered repo,
    // keyed by normalized root, so a red `main` in one managed repo halts only
    // that repo's dispatch — never the siblings'. With an empty registry (the
    // common single-workspace case) exactly one root is keyed, reducing to the
    // pre-#3930 single-flag behavior byte-for-byte.
    let workspace_health_states = Arc::new(main_health_gate::WorkspaceHealthStates::new());

    // Shared drain-and-restart state (Issue #4090). Constructed here — before the
    // epic supervisor, work-finder, and role runner — so its flag can be threaded
    // into all three dispatch producers AND into the IPC server (which sets/aborts
    // it and renders it in `loom-daemon status`). With no drain requested the flag
    // stays `false`, so every producer's halt check is byte-for-byte unchanged.
    let drain_state = Arc::new(loom_daemon::ipc::DrainState::new());
    let drain_flag = drain_state.flag();

    // Shared role-runner in-progress guard (#4364): one set, cloned into both
    // the work-finder's idle-edge path and every interval role loop, so an
    // interval run and an idle-triggered run never overlap for the same
    // (root, role). In-process shared state only — no event-bus topic.
    let role_in_progress = role_runner::new_in_progress_guard();

    // Epic supervisor loop (Issue #3872 — Phase 4 of epic #3842). Opt-in via
    // `LOOM_EPIC_SUPERVISOR`. The loop drives every open `loom:epic` issue
    // through its fork-join lifecycle by dispatching the enabled role each tick.
    //
    // It runs on a DEDICATED OS THREAD with its own current-thread runtime —
    // NOT `tokio::spawn` on this shared daemon runtime — because the concrete
    // `SpawnDispatcher::dispatch_role` is spawn-and-wait (`Command::status()`
    // blocks for the full lifetime of each Architect/Champion process, holding
    // the #3707 issue-creation mutex across the burst). Keeping that blocking
    // call off the shared runtime preserves the responsiveness of the event
    // bus, reaper, sweep registry, and IPC listener while a role process runs.
    // Multi-workspace fan-out (#3928): the supervisor loop resolves
    // `effective_roots()` each tick and drives one `EpicSupervisor` per registered
    // workspace (empty registry ⇒ the single `sweep_workspace`, byte-for-byte).
    // Per-root spawn-binary resolution now happens inside the loop thread, so no
    // single up-front `resolve_spawn_bin()` gate is needed here.
    let supervisor_handle = if epic_supervisor::supervisor_enabled() {
        let interval = epic_supervisor::resolve_supervisor_interval();
        match epic_supervisor::spawn_multi_supervisor_thread(
            workspace_pool.clone(),
            sweep_workspace.clone(),
            event_bus.clone(),
            workspace_health_states.clone(),
            interval,
            drain_flag.clone(),
        ) {
            Ok(handle) => {
                log::info!(
                    "epic_supervisor: enabled (multi-workspace, interval={}s)",
                    interval.as_secs()
                );
                Some(handle)
            }
            Err(e) => {
                log::error!("epic_supervisor: failed to start loop thread: {e}");
                None
            }
        }
    } else {
        log::debug!("epic_supervisor: disabled (set LOOM_EPIC_SUPERVISOR=1 to enable)");
        None
    };
    // Shared shutdown flag so the signal handler can stop the loop cleanly.
    let supervisor_shutdown = supervisor_handle
        .as_ref()
        .map(epic_supervisor::SupervisorHandle::shutdown_token);
    // Keep the handle alive for the daemon's lifetime; its Drop signals stop.
    let _supervisor_handle = supervisor_handle;

    // Autonomous work-finder loop (Issue #3810 — Phase A of epic #3809; dynamic
    // concurrency scaling added in #3811 — Phase B). Opt-in via
    // `LOOM_WORK_FINDER`. Each tick queries the forge for open `loom:issue`
    // items and dispatches up to a **work-driven** cap — recomputed every tick as
    // `min(token-pool size, disk headroom, cpu/load headroom, configured_max)`
    // (CPU/load term added in #3978) — through the same `SweepRegistry::dispatch()`
    // path the IPC `DispatchSweep` request uses. `LOOM_WORK_FINDER_MAX_CONCURRENT`
    // is repurposed (Phase A → B) from a fixed target into the operator ceiling;
    // the cap also never exceeds the token-pool size (no account
    // over-subscription), the scratch-volume disk headroom, nor the host's CPU
    // headroom (never starve concurrent sweep builds into starving the
    // main-health gate's own build, #3978).
    //
    // Unlike the epic supervisor above, this runs as a plain `tokio::spawn`
    // interval task on the shared daemon runtime (like the reaper): every call
    // into `dispatch()` returns promptly (fire-and-forget child spawn), so the
    // finder never parks a runtime worker in a long blocking call.
    // The shared reactive main-health halt flag (`main_health_state`) is
    // constructed above (before the epic supervisor) so both dispatch paths share
    // it. Config surface (#3813): `.loom/config.json → autonomous.workFinder` lets a
    // repo enable/tune the loop from committed config with zero env vars, while
    // an operator env var still overrides for a single run (precedence env >
    // config > default). An absent `autonomous` block is byte-for-byte the
    // env-only behavior shipped in Phases A/B.
    let work_finder_config = work_finder::read_work_finder_config(&sweep_workspace);

    let _work_finder_handle = if work_finder::resolve_enabled(&work_finder_config) {
        let interval = work_finder::resolve_interval_with_config(&work_finder_config);
        let configured_max = work_finder::resolve_max_concurrent_with_config(&work_finder_config);
        let per_token_concurrency = work_finder::resolve_per_token_concurrency(&work_finder_config);
        // CPU headroom knobs (#4032): resolved once at startup from the same
        // `work_finder_config`, precedence env > config > default — matching
        // `per_token_concurrency` exactly (single-root, startup-time; the
        // dynamic cap is one global number per tick, computed before the
        // workspace registry is even loaded, so there is no per-workspace
        // variant of these knobs).
        let cpu_utilization_target =
            work_finder::resolve_cpu_utilization_target(&work_finder_config);
        let cpu_est_cores_per_sweep =
            work_finder::resolve_cpu_est_cores_per_sweep(&work_finder_config);
        // Per-tick admission (ramp) cap (#4234): resolved once at startup from
        // the same `work_finder_config`, precedence env > config > default —
        // the same startup-capture pattern as the CPU knobs above. Bounds how
        // many *new* sweeps a single tick may admit, independent of how large
        // the dynamic cap computes to that tick — see
        // `work_finder::WORK_FINDER_MAX_ADMISSIONS_PER_TICK_ENV` for the full
        // ramp-lag rationale (#4231's second wave).
        let max_admissions_per_tick =
            work_finder::resolve_max_admissions_per_tick_with_config(&work_finder_config);
        // #4084: hold new dispatch off a root while its build-gate run is in
        // flight, so a fresh sweep build does not race the gate's own build for
        // cores (the contention that timed the gate out under #4073's mild
        // niceness). Resolved once at startup from the same primary workspace
        // config as the gate's master switch (env > config > default(on)).
        let suppress_dispatch_during_gate = main_health_gate::resolve_suppress_dispatch_during_gate(
            &main_health_gate::read_autonomous_gate_config(&sweep_workspace),
        );
        // Host-distress circuit breaker (#4235): resolve its config once at
        // startup (env > config > default, default-ON) from the daemon's primary
        // workspace and register the process-global handle. The work-finder loop
        // (spawned just below) samples load-per-core into it each tick and
        // consults it as a second dispatch suppressor; `loom-daemon status` and
        // the `dispatch_sweep` IPC handler read it via the same global. Registered
        // only when the work-finder is enabled, since the loop is the breaker's
        // sole sampler — a daemon with no work-finder never trips it (and its
        // dispatch_sweep sees a Closed/absent breaker: zero behavior change).
        let host_breaker_config = host_breaker::resolve_config_for(&sweep_workspace);
        host_breaker::register_global(std::sync::Arc::new(host_breaker::SharedHostBreaker::new(
            host_breaker_config,
        )));
        log::info!(
            "host_breaker: enabled={} (load_per_core_trip={:.2}, sustain_ticks={}, cooldown_secs={})",
            host_breaker_config.enabled,
            host_breaker_config.load_per_core_threshold,
            host_breaker_config.sustain_ticks,
            host_breaker_config.cooldown_secs,
        );
        log::info!(
            "work_finder: enabled (multi-workspace, interval={}s, configured_max={configured_max}, \
             per_token_concurrency={per_token_concurrency}, cpu_utilization_target={cpu_utilization_target}, \
             cpu_est_cores_per_sweep={cpu_est_cores_per_sweep}, \
             max_admissions_per_tick={max_admissions_per_tick}, \
             dynamic cap = min(healthy tokens × per-token, disk, cpu, configured_max), \
             global across workspaces)",
            interval.as_secs()
        );
        // Multi-workspace fan-out (#3928): re-reads `effective_roots()` each tick
        // and dispatches into each registered repo's own working tree via the
        // shared `workspace_pool`. Empty registry ⇒ the single `sweep_workspace`.
        Some(work_finder::spawn_multi_work_finder_task(
            workspace_pool.clone(),
            sweep_workspace.clone(),
            interval,
            configured_max,
            per_token_concurrency,
            cpu_utilization_target,
            cpu_est_cores_per_sweep,
            max_admissions_per_tick,
            workspace_health_states.clone(),
            suppress_dispatch_during_gate,
            event_bus.clone(),
            drain_flag.clone(),
            role_in_progress.clone(),
        ))
    } else {
        log::debug!("work_finder: disabled (set LOOM_WORK_FINDER=1 to enable)");
        None
    };

    // Idle-edge role triggering (#4364) is inert without the work-finder loop:
    // the work finder is the sole source of the per-root idle signal, so an
    // `autonomous.roleRunner.onIdle` set with no work finder enabled can never
    // fire. Warn once at startup so a misconfiguration surfaces in the log
    // rather than silently doing nothing. (The interval cadence still runs.)
    if !work_finder::resolve_enabled(&work_finder_config)
        && !role_runner::resolve_on_idle_roles(&role_runner::read_role_runner_config(
            &sweep_workspace,
        ))
        .is_empty()
    {
        log::warn!(
            "role_runner: autonomous.roleRunner.onIdle is set but the work finder is disabled — \
             idle-edge triggering has no idle signal to observe and will never fire (enable the \
             work finder with LOOM_WORK_FINDER=1 or autonomous.workFinder.enabled=true). The \
             interval cadence is unaffected."
        );
    }

    // Reactive main-health backstop loop (Issue #3812 — Phase C of epic #3809).
    // Opt-in via `LOOM_MAIN_HEALTH_GATE` AND a `buildGate` block in
    // `.loom/config.json`. On a red `main` (a non-zero `buildGate.command`) it
    // sets `main_health_state` halted, which stops the work-finder above from
    // dispatching new sweeps until a green run clears it. The gate command runs
    // on a blocking thread (it may take minutes), so a plain `tokio::spawn`
    // interval task on the shared runtime is correct (like the reaper).
    // Config surface (#3813): `autonomous.mainHealthGate.enabled` can enable the
    // gate from committed config; `LOOM_MAIN_HEALTH_GATE` remains the master
    // on/off override (precedence env > config > default). The gate's *behavior*
    // (command, timeout) still comes from the separate `buildGate` block, so
    // Phase C's tested semantics are unchanged.
    // Multi-workspace fan-out (#3930): the gate loop re-reads `effective_roots()`
    // each cycle and runs one gate check per registered root, writing into that
    // root's own `MainHealthState` in `workspace_health_states`. A red repo halts
    // only its own dispatch. Per-root enablement + `buildGate` config are read
    // from each repo's own `.loom/config.json` inside the loop, so no single
    // up-front buildGate resolution is needed. The startup master switch is still
    // the daemon's own workspace config (env > config), mirroring how the
    // work-finder / epic-supervisor loops are gated at startup by `sweep_workspace`.
    let autonomous_gate_config = main_health_gate::read_autonomous_gate_config(&sweep_workspace);
    let _main_health_gate_handle = if main_health_gate::resolve_enabled(&autonomous_gate_config) {
        let interval = main_health_gate::resolve_interval();
        log::info!("main_health_gate: enabled (multi-workspace, interval={}s)", interval.as_secs());
        Some(main_health_gate::spawn_multi_main_health_gate_task(
            workspace_health_states.clone(),
            sweep_workspace.clone(),
            interval,
        ))
    } else {
        log::debug!("main_health_gate: disabled (set LOOM_MAIN_HEALTH_GATE=1 or autonomous.mainHealthGate.enabled=true + a buildGate config to enable)");
        None
    };

    // Autonomous token-ranking refresh loop (Issue #3969): the daemon itself
    // periodically re-probes each registered repo's token pool and rewrites
    // `.loom/tokens/.ranking` (atomically — same script an operator's cron used
    // to run by hand: `probe-tokens.sh --ranking` / `loom-tokens check
    // --ranking`), so token selection's ranking tier stays fresh without a
    // standing manual/cron step. Unlike the work-finder and main-health-gate
    // loops above, this is **default-ON** — it only ever reads rate-limit
    // headers and rewrites a bookkeeping file with no dispatch side effect, so
    // an absent daemon-side refresher would silently regress every install back
    // to the stale-ranking failure mode this issue exists to fix. Config
    // surface: `autonomous.tokenRankingRefresh.{enabled,intervalSecs}`,
    // precedence env > config > default (env:
    // `LOOM_TOKEN_RANKING_REFRESH` / `LOOM_TOKEN_RANKING_REFRESH_INTERVAL_SECS`).
    // An operator cron running the identical script concurrently is harmless —
    // the underlying `loom-tokens check --ranking` write is atomic, so the two
    // refreshers can race to schedule a write but never to a torn file.
    let token_ranking_refresh_config =
        token_ranking_refresh::read_token_ranking_refresh_config(&sweep_workspace);
    let _token_ranking_refresh_handle =
        if token_ranking_refresh::resolve_enabled(&token_ranking_refresh_config) {
            let interval = token_ranking_refresh::resolve_interval(&token_ranking_refresh_config);
            log::info!(
                "token_ranking_refresh: enabled (multi-workspace, interval={}s)",
                interval.as_secs()
            );
            Some(token_ranking_refresh::spawn_multi_token_ranking_refresh_task(
                sweep_workspace.clone(),
                interval,
            ))
        } else {
            log::debug!(
                "token_ranking_refresh: disabled (set LOOM_TOKEN_RANKING_REFRESH=0 or \
             autonomous.tokenRankingRefresh.enabled=false to opt out)"
            );
            None
        };

    // Declared-cadence liveness heartbeat (Issue #4011): the daemon touches
    // `<loom_dir>/daemon.heartbeat` on a fixed cadence so a host-side watchdog
    // (`loom-daemon-watchdog.sh`, a second StartInterval launchd job) can detect
    // "a daemon should be running but isn't" WITHOUT talking to the daemon — a
    // dead daemon cannot report its own death, so the reporter must live outside
    // this process. Default-ON like the token-ranking refresh / watch-monitor
    // loops (it only writes a small bookkeeping file with no dispatch side
    // effect); opt out with `LOOM_DAEMON_HEARTBEAT=0` /
    // `autonomous.heartbeat.enabled=false`. We deliberately do NOT reuse the
    // token-ranking `.ranking` mtime as an accidental heartbeat: that is a
    // config-disableable side effect, so a detector keyed to it would silently
    // stop working when that loop is turned off.
    let heartbeat_config = daemon_heartbeat::read_heartbeat_config(&sweep_workspace);
    // Resolved once here so the healing marker below (#4331) and the running
    // heartbeat loop agree on the cadence the watchdog derives its staleness
    // threshold from — even if the loop itself is disabled.
    let heartbeat_interval = daemon_heartbeat::resolve_interval(&heartbeat_config);
    let _heartbeat_handle = if daemon_heartbeat::resolve_enabled(&heartbeat_config) {
        log::info!("daemon_heartbeat: enabled (interval={}s)", heartbeat_interval.as_secs());
        daemon_heartbeat::spawn_heartbeat_task(heartbeat_interval)
    } else {
        log::debug!(
            "daemon_heartbeat: disabled (set LOOM_DAEMON_HEARTBEAT=1 or \
             autonomous.heartbeat.enabled=true to opt in)"
        );
        None
    };

    // Startup autonomy-desired marker healing (Issue #4331). The marker is the
    // durable "a daemon is EXPECTED on this host" signal the watchdog + status
    // key off (#4011). `loom-daemon restart` (#4054), the in-daemon self-update
    // loop, and a bare launchd/systemd relaunch all bring up a fresh daemon
    // WITHOUT re-running the start script's `write_intent_marker` — so an ABSENT
    // marker was never healed, leaving a supervised daemon running with crash
    // protection silently disarmed. This single startup choke point covers all
    // three paths: if the daemon is supervised and the marker is absent, re-write
    // it. An unsupervised (`--foreground`/nohup/debug) run never writes one — it
    // must not arm the host-side pager for a process nothing will relaunch.
    match autonomy_marker::heal_on_startup(heartbeat_interval.as_secs()) {
        Some(autonomy_marker::HealOutcome::Healed(path)) => log::warn!(
            "autonomy_marker: HEALED an absent autonomy-desired marker at {} — a supervised \
             daemon was running with crash protection disarmed (restart-primitive / self-update / \
             bare relaunch never re-writes it). The watchdog and `loom-daemon status` now see this \
             daemon as EXPECTED again (#4331).",
            path.display()
        ),
        Some(autonomy_marker::HealOutcome::AlreadyPresent) => {
            log::debug!("autonomy_marker: marker already present — no healing needed (#4331)")
        }
        Some(autonomy_marker::HealOutcome::UnsupervisedSkip) => log::debug!(
            "autonomy_marker: unsupervised run (no LOOM_DAEMON_SUPERVISOR) — deliberately NOT \
             writing an autonomy-desired marker (#4331)"
        ),
        Some(autonomy_marker::HealOutcome::WriteFailed { path, error }) => log::warn!(
            "autonomy_marker: failed to heal the autonomy-desired marker at {} (logged, never \
             fatal; the daemon keeps running): {error} (#4331)",
            path.display()
        ),
        None => log::warn!(
            "autonomy_marker: could not resolve a loom dir (no LOOM_SOCKET_PATH / home) — \
             skipping marker healing for this run (#4331)"
        ),
    }

    // Autonomous periodic support-role runner (Issue #4015): dispatches the
    // standalone support roles (Champion, Curator, Judge, Auditor, Guide)
    // host-side through `spawn-claude.sh` on their own per-role cadence,
    // drawing from the SAME rotated, health-ranked token pool sweeps already
    // use via `sweep_registry` — instead of relying solely on the GitHub
    // Actions cron workflows (`.github/workflows/loom-*.yml`), which
    // authenticate with a single static `CLAUDE_API_KEY` secret with no
    // rotation and no health-awareness. Opt-in via `LOOM_ROLE_RUNNER` (or
    // `autonomous.roleRunner.enabled=true`) — like the work-finder and
    // main-health-gate loops above, this has dispatch-affecting side effects
    // (each tick is a full `claude -p "/<role>"` session that can mutate
    // issues/PRs), so an absent config leaves the daemon's behavior
    // byte-for-byte unchanged. The Actions workflows remain a supported
    // fallback for deployments with no always-on daemon. One task per
    // enabled role is spawned, each on its own multi-workspace loop
    // (`role_runner::spawn_multi_role_task`) — mirrors the token-ranking
    // refresh loop's re-fan-out-every-tick shape.
    let role_runner_config = role_runner::read_role_runner_config(&sweep_workspace);
    let _role_runner_handles = if role_runner::resolve_enabled(&role_runner_config) {
        let roles = role_runner::resolve_roles(&role_runner_config);
        log::info!(
            "role_runner: enabled (multi-workspace, {} role(s): {})",
            roles.len(),
            roles.iter().map(|r| r.name).collect::<Vec<_>>().join(", ")
        );
        let handles: Vec<_> = roles
            .iter()
            .map(|spec| {
                let interval = role_runner::resolve_interval_for_role(spec, &role_runner_config);
                log::info!("role_runner: {} interval={}s", spec.name, interval.as_secs());
                role_runner::spawn_multi_role_task(
                    *spec,
                    sweep_workspace.clone(),
                    interval,
                    drain_flag.clone(),
                    role_in_progress.clone(),
                )
            })
            .collect();
        Some(handles)
    } else {
        log::debug!(
            "role_runner: disabled (set LOOM_ROLE_RUNNER=1 or autonomous.roleRunner.enabled=true to enable)"
        );
        None
    };

    // Durable watch-monitor loop (Issue #3971): the daemon polls the forge for
    // the terminal state of operator-registered issue/PR watches
    // (`~/.loom/watches.json`) and appends resolutions to
    // `~/.loom/logs/watch-results.log`. Because the watch lives in a file the
    // long-lived daemon owns, an operator's watch survives their Claude Code
    // session dying — the failure this issue exists to fix. Like the
    // token-ranking refresh above (and unlike the dispatch-affecting work-finder
    // / main-health-gate loops) it is **default-ON**: it has no dispatch side
    // effect and makes ZERO forge calls until an operator registers a watch.
    // Config surface: `autonomous.watchMonitor.{enabled,intervalSecs,expirySecs}`,
    // precedence env > config > default (env: `LOOM_WATCH_MONITOR` /
    // `LOOM_WATCH_MONITOR_INTERVAL_SECS` / `LOOM_WATCH_MONITOR_EXPIRY_SECS`).
    let watch_monitor_config = watch_registry::read_watch_monitor_config(&sweep_workspace);
    let _watch_monitor_handle = if watch_registry::resolve_enabled(&watch_monitor_config) {
        let interval = watch_registry::resolve_interval(&watch_monitor_config);
        let expiry = watch_registry::resolve_expiry(&watch_monitor_config);
        log::info!(
            "watch_monitor: enabled (interval={}s, expiry={}s)",
            interval.as_secs(),
            expiry.as_secs()
        );
        Some(watch_registry::spawn_watch_monitor_task(
            watch_registry::GhWatchProbe::new(),
            interval,
            expiry,
        ))
    } else {
        log::debug!(
            "watch_monitor: disabled (set LOOM_WATCH_MONITOR=1 or \
             autonomous.watchMonitor.enabled=true to opt in)"
        );
        None
    };

    // Autonomous self-update loop (Issue #4055 — Phase 3 of #4017). Opt-in via
    // `LOOM_AUTO_UPDATE` / `autonomous.autoUpdate.enabled`. When the daemon's own
    // source checkout advances past the commit this binary was built from, the
    // loop rebuilds + provisions (reusing `loom-daemon-update.sh --no-restart`)
    // and rolls onto the fresh binary via #4090's drain path — in-flight sweeps
    // finish first and survive in the registry. Gated on a clean tree, a settle
    // window, zero in-flight sweeps (`ipc::count_in_flight_sweeps`), and exponential
    // backoff with a terminal give-up state, all surfaced in `loom-daemon status`.
    //
    // Unlike the per-workspace loops above this is spawned exactly **once** for the
    // whole daemon (its subject is the daemon process itself — one binary, one
    // source checkout, one restart), NOT a `spawn_multi_*` per-workspace fan-out.
    // Config is read from the daemon's default workspace, like the sibling readers.
    // Default OFF (side effects on the running process). Cloned handles here because
    // `event_bus` is moved into `IpcServer::new` below.
    let auto_update_config = auto_update::read_auto_update_config(&sweep_workspace);
    let _auto_update_handle = if auto_update::resolve_enabled(&auto_update_config) {
        let interval = auto_update::resolve_interval(&auto_update_config);
        let settle = auto_update::resolve_settle(&auto_update_config);
        log::info!(
            "auto_update: enabled (interval={}s, settle={}s)",
            interval.as_secs(),
            settle.as_secs()
        );
        let probe = auto_update::ScriptAutoUpdateProbe::new(
            workspace_pool.clone(),
            sweep_workspace.clone(),
        );
        let trigger = auto_update::IpcDrainTrigger::new(
            drain_state.clone(),
            workspace_pool.clone(),
            sweep_workspace.clone(),
            event_bus.clone(),
            tokio::runtime::Handle::current(),
        );
        let status = std::sync::Arc::new(auto_update::AutoUpdateStatus::new(true));
        Some(auto_update::spawn_auto_update_task(probe, trigger, status, interval, settle))
    } else {
        log::debug!(
            "auto_update: disabled (set LOOM_AUTO_UPDATE=1 or autonomous.autoUpdate.enabled=true to opt in)"
        );
        None
    };

    // Independent, opt-in idle exit (#4467). The daemon only exits; the host
    // guard retains sole authority to power off.
    let idle_exit_config = idle_exit::read_config(&sweep_workspace);
    let _idle_exit_handle = if idle_exit::resolve_enabled(&idle_exit_config) {
        let minutes = idle_exit::resolve_minutes(&idle_exit_config);
        let starvation = idle_exit::resolve_starvation(&idle_exit_config);
        if loom_daemon::ipc::detect_supervisor().as_deref() == Some("launchd") {
            log::warn!(
                "idle_exit: ENABLED under launchd; KeepAlive:SuccessfulExit relaunches exit(0), \
                 so idle exit is meaningful only under on-failure-style supervision"
            );
        }
        log::info!("idle_exit: enabled (idle_minutes={minutes}, on_token_starvation={starvation})");
        Some(idle_exit::spawn_task(
            idle_exit_config,
            sweep_workspace.clone(),
            workspace_pool.clone(),
            role_in_progress.clone(),
            event_bus.clone(),
            socket_path.clone(),
        ))
    } else {
        log::debug!("idle_exit: disabled (opt in with autonomous.idleExit.enabled=true)");
        None
    };

    // Start IPC server. `workspace_health_states` is threaded in so the
    // `DaemonStatus` request can report each registered repo's own halt state
    // (#3930), and `sweep_workspace` is the `effective_roots` fallback for the
    // per-repo status breakdown — the same values the work-finder and gate loop
    // share above. `credential_preflight` (#4005) is threaded in so
    // `DaemonStatus` can report the startup credential resolution computed
    // once above, without reading logs.
    let server = IpcServer::new(
        socket_path.clone(),
        tm,
        activity_db,
        sweep_registry,
        event_bus,
        workspace_health_states.clone(),
        workspace_pool.clone(),
        sweep_workspace.clone(),
        credential_preflight,
        drain_state.clone(),
    );

    // Setup signal handler for graceful shutdown. We listen for BOTH SIGINT
    // (Ctrl-C, interactive) and SIGTERM (`kill <pid>`, the default signal a
    // backgrounded daemon receives from `loom-daemon-stop.sh` — #3813). Either
    // one removes the socket and exits cleanly so a subsequent start does not
    // trip the singleton guard on a stale socket.
    //
    // In-flight `/loom:sweep` children are NOT cancelled on shutdown — they are
    // independent detached processes and survive a daemon restart by design
    // (killing the dispatcher must not kill dispatched work). This is the
    // documented "survive, don't drain" decision (see daemon-reference.md).
    let socket_path_clone = socket_path.clone();
    tokio::spawn(async move {
        let signal_name = wait_for_shutdown_signal().await;
        log::info!("Received {signal_name}, cleaning up...");
        // Signal the off-runtime epic supervisor loop to stop.
        if let Some(flag) = &supervisor_shutdown {
            flag.store(true, std::sync::atomic::Ordering::Relaxed);
        }
        let _ = tokio::fs::remove_file(&socket_path_clone).await;
        // Exit code carries shutdown intent (Issue #4054, Curator Finding 1):
        // under launchd `KeepAlive:SuccessfulExit` a clean exit(0) triggers a
        // relaunch, so a signal-driven stop MUST exit non-zero — otherwise an
        // operator stop would race launchd into relaunching the daemon. Only the
        // `RestartDaemon` primitive exits 0. SIGTERM => 143, SIGINT => 130.
        let code = if signal_name == "SIGTERM" {
            loom_daemon::ipc::EXIT_SIGTERM
        } else {
            loom_daemon::ipc::EXIT_SIGINT
        };
        log::info!("Socket cleaned up, exiting {code} (operator stop — no supervised relaunch)");
        std::process::exit(code);
    });

    log::info!("Loom daemon starting...");
    server.run().await?;

    Ok(())
}

/// Await either SIGINT (Ctrl-C) or, on Unix, SIGTERM (`kill <pid>`), returning a
/// short human-readable name for whichever fired first. On non-Unix platforms
/// only Ctrl-C is available. Introduced in #3813 so a backgrounded daemon shut
/// down via `kill` (SIGTERM) cleans up its socket exactly like an interactive
/// Ctrl-C, rather than being torn down by the default SIGTERM disposition with
/// the socket left behind.
async fn wait_for_shutdown_signal() -> &'static str {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        // If a signal stream cannot be installed, fall back to Ctrl-C only
        // rather than aborting shutdown handling entirely.
        match signal(SignalKind::terminate()) {
            Ok(mut sigterm) => {
                tokio::select! {
                    r = tokio::signal::ctrl_c() => {
                        if let Err(err) = r {
                            log::error!("Unable to listen for Ctrl-C: {err}");
                        }
                        "SIGINT (Ctrl-C)"
                    }
                    _ = sigterm.recv() => "SIGTERM",
                }
            }
            Err(err) => {
                log::error!("Unable to install SIGTERM handler ({err}); listening for Ctrl-C only");
                if let Err(err) = tokio::signal::ctrl_c().await {
                    log::error!("Unable to listen for Ctrl-C: {err}");
                }
                "SIGINT (Ctrl-C)"
            }
        }
    }
    #[cfg(not(unix))]
    {
        if let Err(err) = tokio::signal::ctrl_c().await {
            log::error!("Unable to listen for Ctrl-C: {err}");
        }
        "SIGINT (Ctrl-C)"
    }
}

fn check_tmux_installed() -> Result<()> {
    Command::new("which")
        .arg("tmux")
        .output()?
        .status
        .success()
        .then_some(())
        .ok_or_else(|| anyhow!("tmux not installed. Install with: brew install tmux"))
}

/// Resolve the daemon's loom directory: the parent of `LOOM_SOCKET_PATH` when
/// that env var is set (test isolation), else `$HOME/.loom`. Pure — no side
/// effects (no directory creation), so it's safe to call from `setup_logging()`
/// (which runs before `main()`'s own `loom_dir`/`socket_path` resolution block)
/// as well as from `main()` and `resolve_socket_path()` without duplicating the
/// branching logic three times over.
fn resolve_loom_dir() -> Result<PathBuf> {
    if let Ok(path) = std::env::var("LOOM_SOCKET_PATH") {
        let socket_path = PathBuf::from(path);
        let loom_dir = socket_path
            .parent()
            .ok_or_else(|| anyhow!("Socket path has no parent directory"))?
            .to_path_buf();
        return Ok(loom_dir);
    }
    let loom_dir = dirs::home_dir()
        .ok_or_else(|| anyhow!("No home directory"))?
        .join(".loom");
    Ok(loom_dir)
}

/// Resolve the daemon log file path: `LOOM_DAEMON_LOG` (full override) when
/// set, else `<loom dir>/daemon.log` derived from [`resolve_loom_dir`]. This
/// means `LOOM_SOCKET_PATH`-style test isolation covers the log file for free
/// — a test daemon pointed at a tempdir socket also logs into that tempdir,
/// never into the operator's `~/.loom/daemon.log` (Issue #4010). Precedence is
/// env > default only; see #4010 for why a config tier is out of scope here
/// (`setup_logging()` runs before workspace/config resolution).
fn resolve_log_path() -> Result<PathBuf> {
    if let Ok(path) = std::env::var("LOOM_DAEMON_LOG") {
        return Ok(PathBuf::from(path));
    }
    Ok(resolve_loom_dir()?.join("daemon.log"))
}

fn setup_logging() -> Result<()> {
    let log_path = resolve_log_path()?;

    if let Some(parent) = log_path.parent() {
        fs::create_dir_all(parent)?;
    }

    // Rotate log file if it exceeds 10MB (keeps last 10 files)
    rotate_log_file(&log_path, 10 * 1024 * 1024, 10)?;

    let log_file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)?;

    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .target(env_logger::Target::Pipe(Box::new(log_file)))
        .format(|buf, record| {
            writeln!(
                buf,
                "[{}] [{}] {}",
                chrono::Local::now().format("%Y-%m-%dT%H:%M:%S%.3f"),
                record.level(),
                record.args()
            )
        })
        .init();

    log::info!("Daemon logging initialized to {}", log_path.display());

    Ok(())
}

#[cfg(test)]
mod resolve_paths_tests {
    //! Tests for [`resolve_loom_dir`] and [`resolve_log_path`] (Issue #4010).
    //!
    //! `setup_logging()` itself is deliberately NOT unit-tested here: it calls
    //! `env_logger::Builder::...init()`, which panics if called a second time
    //! in the same process. Splitting the pure path-resolution logic out into
    //! these two functions is exactly what makes it testable at all.
    //!
    //! Both tests mutate the process-global `LOOM_SOCKET_PATH` / `LOOM_DAEMON_LOG`
    //! env vars, so they're `#[serial]` (the crate already depends on
    //! `serial_test` for this exact purpose — see `dispatch_tests` below) to
    //! avoid racing other env-mutating tests in the same binary.
    use super::{resolve_log_path, resolve_loom_dir};
    use serial_test::serial;

    #[test]
    #[serial]
    fn resolve_loom_dir_defaults_to_home_loom() {
        std::env::remove_var("LOOM_SOCKET_PATH");
        let expected = dirs::home_dir().expect("home dir").join(".loom");
        assert_eq!(resolve_loom_dir().expect("resolve"), expected);
    }

    #[test]
    #[serial]
    fn resolve_loom_dir_honors_socket_path_override() {
        let dir = tempfile::tempdir().expect("tempdir");
        let socket_path = dir.path().join("loom-daemon.sock");
        std::env::set_var("LOOM_SOCKET_PATH", &socket_path);

        let resolved = resolve_loom_dir().expect("resolve");

        std::env::remove_var("LOOM_SOCKET_PATH");
        assert_eq!(resolved, dir.path());
    }

    #[test]
    #[serial]
    fn resolve_log_path_defaults_to_home_loom_daemon_log() {
        std::env::remove_var("LOOM_SOCKET_PATH");
        std::env::remove_var("LOOM_DAEMON_LOG");
        let expected = dirs::home_dir()
            .expect("home dir")
            .join(".loom")
            .join("daemon.log");
        assert_eq!(resolve_log_path().expect("resolve"), expected);
    }

    #[test]
    #[serial]
    fn resolve_log_path_honors_loom_daemon_log_override() {
        std::env::remove_var("LOOM_SOCKET_PATH");
        std::env::set_var("LOOM_DAEMON_LOG", "/tmp/some/d/daemon.log");

        let resolved = resolve_log_path().expect("resolve");

        std::env::remove_var("LOOM_DAEMON_LOG");
        assert_eq!(resolved, std::path::PathBuf::from("/tmp/some/d/daemon.log"));
    }

    /// `LOOM_DAEMON_LOG` must win even when `LOOM_SOCKET_PATH` is also set —
    /// the explicit log override always takes precedence over the derived path.
    #[test]
    #[serial]
    fn resolve_log_path_daemon_log_wins_over_socket_path_derivation() {
        let dir = tempfile::tempdir().expect("tempdir");
        let socket_path = dir.path().join("loom-daemon.sock");
        std::env::set_var("LOOM_SOCKET_PATH", &socket_path);
        std::env::set_var("LOOM_DAEMON_LOG", "/tmp/explicit/daemon.log");

        let resolved = resolve_log_path().expect("resolve");

        std::env::remove_var("LOOM_SOCKET_PATH");
        std::env::remove_var("LOOM_DAEMON_LOG");
        assert_eq!(resolved, std::path::PathBuf::from("/tmp/explicit/daemon.log"));
    }
}

/// Handle CLI commands (init, stats, validate modes)
/// Handle `loom-daemon update-gitignore [PATH]` (Issue #4280).
///
/// Rewrites only the marker-delimited Loom-managed `.gitignore` block, converging
/// it on the current `EPHEMERAL_PATTERNS` set. This is the standalone refresh
/// entry point `resync-installed.sh` calls so existing consumer installs pick up
/// newly-ignored runtime paths without a full `init`. Idempotent: a workspace
/// already carrying the current block is a byte-for-byte no-op.
fn handle_update_gitignore_command(workspace: &str) -> Result<()> {
    let workspace_path = std::path::Path::new(workspace);
    let absolute_workspace = if workspace_path.is_absolute() {
        workspace_path.to_path_buf()
    } else {
        std::env::current_dir()?.join(workspace_path)
    };

    loom_daemon::init::update_gitignore(&absolute_workspace)
        .map_err(|e| anyhow!("Failed to update .gitignore: {e}"))?;

    println!(
        "Refreshed the Loom-managed .gitignore block in {}",
        absolute_workspace.display()
    );
    Ok(())
}

#[allow(clippy::too_many_lines)]
fn handle_cli_command(command: Commands) -> Result<()> {
    match command {
        // Script helpers (epic #4081 Phase 3 family 5, issue #4275).
        Commands::StripAnsi { file } => handle_strip_ansi_command(file.as_deref()),
        Commands::ResolveModel {
            model,
            config,
            generation,
            task_alias,
            tier,
            runtime,
        } => handle_resolve_model_command(
            model.as_deref(),
            config.as_deref(),
            generation,
            task_alias,
            tier.as_deref(),
            &runtime,
        ),
        Commands::Usage { status } => {
            let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
            std::process::exit(script_helpers::usage::run(status, &cwd));
        }
        Commands::Checkpoint { action } => handle_checkpoint_command(action),
        Commands::Claim { command, args } => {
            let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
            std::process::exit(script_helpers::claim::run(&cwd, command.as_deref(), &args));
        }
        Commands::SweepExperiment { action } => handle_sweep_experiment_command(action),
        Commands::ValidatePhase {
            phase,
            issue,
            worktree,
            pr_number,
            task_id,
            json,
            check_only,
            quiet,
        } => {
            let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
            let opts = script_helpers::validate_phase::ValidateOpts {
                phase,
                issue,
                worktree,
                pr_number,
                task_id,
                json_output: json,
                check_only,
                quiet,
            };
            std::process::exit(script_helpers::validate_phase::run(&cwd, &opts));
        }
        Commands::Validate {
            workspace,
            format,
            strict,
            verbose,
        } => handle_validate_command(&workspace, &format, strict, verbose),
        Commands::Stats {
            command,
            role,
            issue,
            weekly,
            period,
            by_model,
            format,
        } => {
            if let Some(cmd) = command {
                handle_agent_metrics_command(
                    &cmd,
                    role.as_deref(),
                    issue,
                    period.as_deref().unwrap_or("week"),
                    by_model,
                    &format,
                )
            } else {
                handle_stats_command(role.as_deref(), issue, weekly, &format)
            }
        }
        Commands::Calibrate {
            workspace,
            write,
            json,
        } => handle_calibrate_command(&workspace, write, json),
        Commands::Workspace { action } => handle_workspace_command(action),
        Commands::Fleet { action } => handle_fleet_command(action),
        Commands::Tokens { action } => handle_tokens_command(action),
        Commands::ClaudeConfig { action } => handle_claude_config_command(action),
        Commands::AgentSpawn {
            role,
            name,
            args,
            worktree,
            on_demand,
            fresh,
            wait,
            timeout,
            json,
            check,
            list,
        } => {
            use loom_daemon::agent_session::spawn::{self, SpawnOptions};
            let opts = SpawnOptions {
                role: role.unwrap_or_default(),
                name: name.unwrap_or_default(),
                args,
                worktree: worktree.unwrap_or_default(),
                on_demand,
                fresh,
                do_wait: wait,
                wait_timeout: timeout,
                json_output: json,
                check_name: check.unwrap_or_default(),
                do_list: list,
                ..Default::default()
            }
            .with_process_env();
            let cwd = std::env::current_dir()?;
            std::process::exit(spawn::run(&loom_daemon::agent_session::SystemEnv, &opts, &cwd));
        }
        Commands::AgentWait {
            name,
            timeout,
            poll_interval,
            min_session_age,
            json,
        } => {
            use loom_daemon::agent_session::wait::{self, WaitOptions};
            let opts = WaitOptions {
                name,
                timeout,
                poll_interval,
                min_session_age,
                json_output: json,
            };
            let cwd = std::env::current_dir()?;
            std::process::exit(wait::run(&loom_daemon::agent_session::SystemEnv, &opts, &cwd));
        }
        Commands::UpdateGitignore { workspace } => handle_update_gitignore_command(&workspace),
        Commands::Clean {
            workspace,
            dry_run,
            deep,
            force,
            safe,
            grace_period,
            worktrees_only,
            branches_only,
            tmux_only,
            daemon,
            aggressive,
            aggressive_min_age,
        } => handle_clean_command(
            &workspace,
            dry_run,
            deep,
            force,
            safe,
            grace_period,
            worktrees_only,
            branches_only,
            tmux_only,
            daemon,
            aggressive,
            aggressive_min_age,
        ),
        Commands::Cleanup { action } => handle_cleanup_command(action),
        Commands::RecoverOrphans {
            workspace,
            recover,
            json,
            verbose,
        } => handle_recover_orphans_command(&workspace, recover, json, verbose),
        Commands::Forge { action } => handle_forge_command(action),
        Commands::Status { .. } => {
            // Routed directly in `main()` (it needs the async runtime for the
            // socket round-trip), never dispatched through this sync handler.
            unreachable!("Status is handled in main() before handle_cli_command")
        }
        Commands::Quarantine { .. } => {
            // Routed directly in `main()` (it needs the async runtime for the
            // socket round-trip), never dispatched through this sync handler.
            unreachable!("Quarantine is handled in main() before handle_cli_command")
        }
        Commands::Dispatch { .. } => {
            // Routed directly in `main()` (it needs the async runtime for the
            // socket round-trip), never dispatched through this sync handler.
            unreachable!("Dispatch is handled in main() before handle_cli_command")
        }
        Commands::Watch { .. } => {
            // Routed directly in `main()` (it needs the async runtime for the
            // socket round-trip), never dispatched through this sync handler.
            unreachable!("Watch is handled in main() before handle_cli_command")
        }
        Commands::Restart { .. } => {
            // Routed directly in `main()` (it needs the async runtime for the
            // socket round-trip), never dispatched through this sync handler.
            unreachable!("Restart is handled in main() before handle_cli_command")
        }
        Commands::Serve { .. } => {
            // Routed directly in `main()` (it needs the async runtime for the
            // HTTP listener + socket round-trips), never dispatched through
            // this sync handler.
            unreachable!("Serve is handled in main() before handle_cli_command")
        }
        Commands::Init {
            workspace,
            defaults,
            force,
            dry_run,
        } => {
            let workspace_path = std::path::Path::new(&workspace);
            let absolute_workspace = if workspace_path.is_absolute() {
                workspace_path.to_path_buf()
            } else {
                std::env::current_dir()?.join(workspace_path)
            };

            let workspace_str = absolute_workspace
                .to_str()
                .ok_or_else(|| anyhow!("Invalid workspace path"))?;

            if dry_run {
                println!("Dry run mode - no changes will be made\n");
                println!("Would initialize Loom workspace:");
                println!("  Workspace: {workspace_str}");
                println!("  Defaults:  {defaults}");
                println!("  Force:     {force}");
                println!("\nActions that would be performed:");
                println!("  1. Validate {workspace_str} is a git repository");
                println!("  2. Copy .loom/ configuration from {defaults}");
                println!(
                    "  3. Setup repository scaffolding (CLAUDE.md, AGENTS.md, .claude/, .github/)"
                );
                println!("  4. Update .gitignore with Loom ephemeral patterns");
                return Ok(());
            }

            println!("Initializing Loom workspace...");
            println!("  Workspace: {workspace_str}");
            println!("  Defaults:  {defaults}");

            match loom_daemon::init::initialize_workspace(workspace_str, &defaults, force) {
                Ok(report) => {
                    if report.is_self_install {
                        println!("\nLoom source repository detected!");
                        println!("\nMode: Validation only (self-installation)");
                        println!("\nValidating configuration...");

                        if let Some(ref validation) = report.validation {
                            println!(
                                "  .loom/roles/    - {} role definitions found",
                                validation.roles_found.len()
                            );
                            println!(
                                "  .loom/scripts/  - {} scripts found",
                                validation.scripts_found.len()
                            );
                            println!(
                                "  .claude/commands/loom/ - {} slash commands found",
                                validation.commands_found.len()
                            );

                            if validation.has_claude_md {
                                println!("  CLAUDE.md       - Present");
                            } else {
                                println!("  CLAUDE.md       - Missing");
                            }

                            // AGENTS.md is not mandatory (see ValidationReport::has_agents_md),
                            // so its absence is informational only, never an issue.
                            if validation.has_agents_md {
                                println!("  AGENTS.md       - Present");
                            } else {
                                println!("  AGENTS.md       - Not present (optional)");
                            }

                            if validation.has_labels_yml {
                                println!("  .github/labels.yml - Present");
                            } else {
                                println!("  .github/labels.yml - Missing");
                            }

                            if validation.issues.is_empty() {
                                println!("\nLoom source repository is properly configured");
                            } else {
                                println!("\nIssues found:");
                                for issue in &validation.issues {
                                    println!("  - {issue}");
                                }
                            }

                            println!("\nRoles found: {}", validation.roles_found.join(", "));
                        }

                        println!("\nSelf-installation skips file copying to prevent data loss.");
                        println!("   The Loom repo's .loom/ directory IS the source of truth.");
                        println!("\nTo use Loom orchestration:");
                        println!(
                            "  - Open Claude Code terminals with /loom:builder, /loom:judge, etc."
                        );
                        println!(
                            "  - Or start the daemon: ./.loom/scripts/cli/loom-daemon-start.sh"
                        );

                        return Ok(());
                    }

                    println!("\nLoom workspace initialized successfully!");
                    println!("\nFiles installed:");
                    println!("  .loom/          - Configuration directory");
                    println!("  .loom/config.json - Terminal configuration");
                    println!("  .loom/roles/    - Agent role definitions");
                    println!("  CLAUDE.md       - AI context documentation (Claude Code)");
                    println!("  AGENTS.md       - AI context documentation (OpenAI Codex)");
                    println!("  .claude/        - Claude Code configuration");
                    println!("  .github/        - GitHub labels and issue templates");
                    println!("  .gitignore      - Updated with Loom patterns");

                    if !report.added.is_empty()
                        || !report.preserved.is_empty()
                        || !report.removed.is_empty()
                    {
                        println!();
                        if !report.added.is_empty() {
                            println!("Files added ({}):", report.added.len());
                            for file in &report.added {
                                println!("  + {file}");
                            }
                        }
                        if !report.preserved.is_empty() {
                            println!("\nFiles preserved ({}):", report.preserved.len());
                            for file in &report.preserved {
                                println!("  = {file}");
                            }
                            println!("\n  Preserved files were not overwritten. To update them,");
                            println!("     delete them and run install again, or use --force.");
                        }
                        if !report.updated.is_empty() {
                            println!("\nFiles updated ({}):", report.updated.len());
                            for file in &report.updated {
                                println!("  ~ {file}");
                            }
                        }
                        if !report.removed.is_empty() {
                            println!("\nFiles removed ({}):", report.removed.len());
                            for file in &report.removed {
                                println!("  - {file}");
                            }
                        }
                        if !report.verification_failures.is_empty() {
                            eprintln!(
                                "\nUnexpected file divergence ({}):",
                                report.verification_failures.len()
                            );
                            for failure in &report.verification_failures {
                                eprintln!("  {failure}");
                            }
                            eprintln!(
                                "\n  These files were copied from defaults but their installed"
                            );
                            eprintln!(
                                "  contents differ from the source. This is informational only —"
                            );
                            eprintln!(
                                "  installation completed. Inspect the listed files to confirm"
                            );
                            eprintln!("  they look correct.");
                        }
                    }

                    println!("\nNext steps:");
                    println!(
                        "  1. Commit the changes: git add -A && git commit -m 'Add Loom configuration'"
                    );
                    println!("  2. Choose your workflow:");
                    println!("     Manual Mode (recommended to start):");
                    println!("       cd {workspace_str} && claude");
                    println!("       Then use /loom:builder, /loom:judge, or other role commands");
                    println!("     Daemon Mode (autonomous orchestration):");
                    println!(
                        "       cd {workspace_str} && ./.loom/scripts/cli/loom-daemon-start.sh"
                    );
                    println!("       Then in Claude Code: /loom:loom");
                    Ok(())
                }
                Err(e) => {
                    eprintln!("\nFailed to initialize workspace: {e}");
                    std::process::exit(1);
                }
            }
        }
    }
}

/// Resolve the daemon's IPC socket path exactly as the running daemon does in
/// `main()`: honour `LOOM_SOCKET_PATH` (test override) first, else
/// `~/.loom/loom-daemon.sock`.
fn resolve_socket_path() -> Result<PathBuf> {
    if let Ok(path) = std::env::var("LOOM_SOCKET_PATH") {
        return Ok(PathBuf::from(path));
    }
    Ok(resolve_loom_dir()?.join("loom-daemon.sock"))
}

/// How a single [`query_daemon_status_once`] attempt failed (#4279), so the
/// caller retries ONLY the transient "daemon dropped the connection before
/// replying" case — never a clean "socket absent" or a slow-daemon timeout.
enum StatusAttemptError {
    /// Connect phase failed — socket absent, connection refused, or the connect
    /// itself timed out. A reconnect cannot help (the daemon is simply not
    /// listening), so this is never retried: fast-fail preserves the operator's
    /// "is the daemon running?" latency.
    Connect(anyhow::Error),
    /// The daemon accepted the connection but dropped it before writing a full
    /// response line — either a clean pre-response EOF or, on Linux, a RST that
    /// surfaces as a `ConnectionReset`/`BrokenPipe`/`UnexpectedEof` read/write
    /// error (see [`classify_roundtrip_error`]). This is the transient
    /// contention failure #4279 retries exactly once — under concurrent-sweep
    /// load a per-connection task can briefly drop a `status` connection that
    /// the very next one answers.
    DroppedBeforeReply(anyhow::Error),
    /// The round-trip failed for a non-transient reason: it timed out against a
    /// slow-but-live daemon (honor the single 5s budget rather than doubling it)
    /// or the response frame was malformed / an explicit daemon error. Retrying
    /// would not change the outcome, so it is not retried.
    Roundtrip(anyhow::Error),
}

impl StatusAttemptError {
    /// Unwrap to the underlying diagnostic surfaced to the operator.
    fn into_inner(self) -> anyhow::Error {
        match self {
            Self::Connect(e) | Self::DroppedBeforeReply(e) | Self::Roundtrip(e) => e,
        }
    }
}

/// Connect to the running daemon over its Unix socket, send a single
/// `DaemonStatus` request, and return the parsed report (Issue #3891).
///
/// Both the connect and the round-trip are individually bounded so an
/// unresponsive/wedged daemon cannot hang the CLI. A single bounded reconnect
/// retry (#4279) absorbs a transient dropped connection — a daemon under
/// concurrent-sweep load can accept then close a `status` connection with zero
/// bytes written, which the client would otherwise surface as a bare EOF that a
/// stdout-capturing monitor misreads as an empty status. A clean "socket absent"
/// or a slow-daemon timeout is deliberately NOT retried. Errors (after the one
/// retry, where applicable) when the daemon is unreachable or the response is
/// malformed.
async fn query_daemon_status(socket_path: &Path) -> Result<DaemonStatusReport> {
    const TIMEOUT: Duration = Duration::from_secs(5);

    match query_daemon_status_once(socket_path, TIMEOUT).await {
        Ok(report) => Ok(report),
        Err(StatusAttemptError::DroppedBeforeReply(_first)) => {
            // One bounded reconnect retry — the transient case only.
            query_daemon_status_once(socket_path, TIMEOUT)
                .await
                .map_err(StatusAttemptError::into_inner)
        }
        Err(other) => Err(other.into_inner()),
    }
}

/// A single connect + `DaemonStatus` round-trip attempt, classifying any failure
/// so [`query_daemon_status`] can decide whether to retry (#4279).
async fn query_daemon_status_once(
    socket_path: &Path,
    timeout: Duration,
) -> std::result::Result<DaemonStatusReport, StatusAttemptError> {
    let stream = tokio::time::timeout(timeout, UnixStream::connect(socket_path))
        .await
        .map_err(|_| {
            StatusAttemptError::Connect(anyhow!("connect timed out after {}s", timeout.as_secs()))
        })?
        .map_err(|e| StatusAttemptError::Connect(anyhow!("connect failed: {e}")))?;
    let (reader, mut writer) = stream.into_split();

    // The round-trip yields `Ok(None)` on a clean pre-response EOF (the retryable
    // drop), `Ok(Some(report))` on success, and `Err(_)` for either a
    // malformed/error frame OR a pre-response read/write I/O error (e.g. the
    // Linux RST drop) — the timeout wrapper below routes each `Err(_)` through
    // `classify_roundtrip_error` to decide whether it is the retryable drop.
    let roundtrip = async move {
        let request_json = serde_json::to_string(&Request::DaemonStatus)?;
        writer.write_all(request_json.as_bytes()).await?;
        writer.write_all(b"\n").await?;
        writer.flush().await?;

        let mut lines = BufReader::new(reader).lines();
        match lines.next_line().await? {
            None => Ok(None),
            Some(line) => {
                let response: Response = serde_json::from_str(&line)?;
                match response {
                    // `Response::DaemonStatus` is boxed (issue #4292); unbox
                    // here so the retry-aware `Result<Option<DaemonStatusReport>>`
                    // signature (and its callers' field accesses) stays unchanged.
                    Response::DaemonStatus(report) => Ok(Some(*report)),
                    Response::Error { message } => Err(anyhow!("daemon error: {message}")),
                    other => Err(anyhow!("unexpected response: {other:?}")),
                }
            }
        }
    };

    match tokio::time::timeout(timeout, roundtrip).await {
        Err(_elapsed) => Err(StatusAttemptError::Roundtrip(anyhow!(
            "status round-trip timed out after {}s",
            timeout.as_secs()
        ))),
        Ok(Err(e)) => Err(classify_roundtrip_error(e)),
        Ok(Ok(None)) => Err(StatusAttemptError::DroppedBeforeReply(anyhow!(
            "daemon closed the connection without responding"
        ))),
        Ok(Ok(Some(report))) => Ok(report),
    }
}

/// Classify a round-trip `Err` from [`query_daemon_status_once`]'s I/O closure as
/// retryable or not (#4279). A read/write I/O error that fires before a full
/// response line arrived is the SAME transient drop as a clean pre-response EOF:
/// on Linux a peer that closes the socket with unread request bytes still queued
/// in its kernel receive buffer replies with RST, so the client's read surfaces
/// `ConnectionReset` (os error 104) instead of the clean EOF macOS reports — both
/// mean "the daemon dropped us before replying". `ConnectionReset`, `BrokenPipe`,
/// and `UnexpectedEof` are therefore reclassified as the retryable
/// [`StatusAttemptError::DroppedBeforeReply`] (reusing the same friendly
/// diagnostic as the EOF path so the operator message is platform-independent).
/// Malformed-JSON responses and explicit `Response::Error` replies are NOT
/// `io::Error`s, so they stay non-retryable [`StatusAttemptError::Roundtrip`].
fn classify_roundtrip_error(e: anyhow::Error) -> StatusAttemptError {
    let dropped_before_reply = e.downcast_ref::<std::io::Error>().is_some_and(|io_err| {
        matches!(
            io_err.kind(),
            std::io::ErrorKind::ConnectionReset
                | std::io::ErrorKind::BrokenPipe
                | std::io::ErrorKind::UnexpectedEof
        )
    });
    if dropped_before_reply {
        StatusAttemptError::DroppedBeforeReply(anyhow!(
            "daemon closed the connection without responding"
        ))
    } else {
        StatusAttemptError::Roundtrip(e)
    }
}

/// Handle the `quarantine` subcommand (Issue #3939). Connects to the running
/// daemon over its Unix socket and dispatches the requested action. The
/// quarantine state is in the daemon's memory, so — unlike `workspace` — this
/// cannot operate on a file when the daemon is down.
async fn handle_quarantine_command(action: QuarantineAction) -> Result<()> {
    let socket_path = resolve_socket_path()?;
    match action {
        QuarantineAction::Clear {
            issue,
            workspace_root,
        } => {
            let request = Request::ClearQuarantine {
                issue,
                workspace_root,
            };
            match query_daemon(&socket_path, &request).await {
                Ok(Response::QuarantineCleared {
                    issue,
                    was_quarantined,
                }) => {
                    if was_quarantined {
                        println!(
                            "Cleared quarantine for issue #{issue} — it will re-qualify for \
                             dispatch and `loom:issue` has been restored on the forge."
                        );
                    } else {
                        println!("Issue #{issue} was not quarantined — nothing to clear (no-op).");
                    }
                    Ok(())
                }
                Ok(Response::Error { message }) => {
                    eprintln!("Daemon error: {message}");
                    std::process::exit(1);
                }
                Ok(other) => {
                    eprintln!("Unexpected response from daemon: {other:?}");
                    std::process::exit(1);
                }
                Err(e) => {
                    eprintln!("Could not reach loom-daemon at {}: {e}", socket_path.display());
                    eprintln!();
                    eprintln!("Is the daemon running? Start it with:");
                    eprintln!("  ./.loom/scripts/cli/loom-daemon-start.sh");
                    std::process::exit(1);
                }
            }
        }

        QuarantineAction::List { workspace_root } => {
            let request = Request::ListQuarantines { workspace_root };
            match query_daemon(&socket_path, &request).await {
                Ok(Response::QuarantineList { entries }) => {
                    render_quarantine_list(&entries);
                    Ok(())
                }
                Ok(Response::Error { message }) => {
                    eprintln!("Daemon error: {message}");
                    std::process::exit(1);
                }
                Ok(other) => {
                    eprintln!("Unexpected response from daemon: {other:?}");
                    std::process::exit(1);
                }
                Err(e) => {
                    eprintln!("Could not reach loom-daemon at {}: {e}", socket_path.display());
                    eprintln!();
                    eprintln!("Is the daemon running? Start it with:");
                    eprintln!("  ./.loom/scripts/cli/loom-daemon-start.sh");
                    std::process::exit(1);
                }
            }
        }
    }
}

/// Render `loom-daemon quarantine list` output (Issue #4215): one line per
/// entry, grouped by workspace root (in the all-workspaces case there may be
/// several), with a disambiguation footer whenever there is at least one
/// entry to show. Entries within a group are already issue-sorted by
/// [`SweepRegistry::quarantine_entries`]; `BTreeMap` keeps groups themselves
/// ordered (by path) for deterministic output across runs.
fn render_quarantine_list(entries: &[QuarantineEntry]) {
    if entries.is_empty() {
        println!("no active quarantines");
        return;
    }

    let mut by_root: std::collections::BTreeMap<&Path, Vec<&QuarantineEntry>> =
        std::collections::BTreeMap::new();
    for entry in entries {
        by_root
            .entry(entry.workspace_root.as_path())
            .or_default()
            .push(entry);
    }

    for (root, group) in &by_root {
        println!("{}:", root.display());
        for e in group {
            println!(
                "  #{}  insta-crash {}/{}  applied {}  ttl remaining {}s",
                e.issue,
                e.insta_crash_count,
                e.insta_crash_threshold,
                e.quarantined_at.to_rfc3339(),
                e.ttl_remaining_secs
            );
        }
    }

    println!();
    println!(
        "Note: `loom:blocked` on the forge may be a quarantine or a real dependency — this \
         command is the authority for quarantines."
    );
}

/// Handle the `restart` subcommand (Issue #4054 — the supervised restart
/// primitive). Connects to the running daemon over its Unix socket and sends a
/// single `RestartDaemon` request.
///
/// When the daemon is supervised (launchd) it replies `DaemonRestart {
/// scheduled: true }` and then exits 0 for a `KeepAlive:SuccessfulExit`
/// relaunch — we print the ack and exit 0. When it is unsupervised (nohup /
/// Linux / `--foreground`) it replies `DaemonRestart { scheduled: false }` and
/// keeps running — we print the refusal and exit non-zero, so an operator or
/// Phase 3 can detect that no restart happened rather than assuming it did.
async fn handle_restart_command(
    drain: bool,
    timeout: Option<u64>,
    force_after_timeout: bool,
    abort_drain: bool,
    then_exit: bool,
) -> Result<()> {
    let socket_path = resolve_socket_path()?;

    if then_exit && !drain {
        eprintln!("--then-exit requires --drain (there is nothing to drain-then-exit without it)");
        std::process::exit(1);
    }

    // Drain-mode variants (Issue #4090) speak `DrainAndRestartDaemon` /
    // `AbortDrain` and expect a `DaemonDrain` reply; the plain restart keeps its
    // #4054 `RestartDaemon` / `DaemonRestart` contract byte-for-byte.
    if abort_drain {
        return handle_drain_reply(
            &socket_path,
            &Request::AbortDrain,
            "loom-daemon drain aborted",
            "no drain was in progress",
        )
        .await;
    }
    if drain {
        let (accepted_prefix, refused_prefix) = if then_exit {
            (
                "loom-daemon drain-then-exit scheduled (will stop, not restart, once drained)",
                "loom-daemon did NOT drain",
            )
        } else {
            ("loom-daemon drain scheduled", "loom-daemon did NOT drain")
        };
        return handle_drain_reply(
            &socket_path,
            &Request::DrainAndRestartDaemon {
                timeout_secs: timeout,
                force_after_timeout,
                then_exit,
            },
            accepted_prefix,
            refused_prefix,
        )
        .await;
    }

    match query_daemon(&socket_path, &Request::RestartDaemon).await {
        Ok(Response::DaemonRestart {
            scheduled,
            supervisor,
            message,
        }) => {
            if scheduled {
                println!(
                    "loom-daemon restart scheduled (supervisor: {}).",
                    supervisor.as_deref().unwrap_or("unknown")
                );
                println!("{message}");
                Ok(())
            } else {
                eprintln!("loom-daemon did NOT restart: {message}");
                std::process::exit(1);
            }
        }
        Ok(Response::Error { message }) => {
            eprintln!("Daemon error: {message}");
            std::process::exit(1);
        }
        Ok(other) => {
            eprintln!("Unexpected response from daemon: {other:?}");
            std::process::exit(1);
        }
        Err(e) => {
            eprintln!("Could not reach loom-daemon at {}: {e}", socket_path.display());
            eprintln!();
            eprintln!("Is the daemon running? Start it with:");
            eprintln!("  ./.loom/scripts/cli/loom-daemon-start.sh");
            std::process::exit(1);
        }
    }
}

/// Shared reply handler for the drain-mode restart variants (Issue #4090): both
/// `DrainAndRestartDaemon` and `AbortDrain` answer with a `DaemonDrain` frame.
/// A refused request (unsupervised host, or abort with no drain in progress)
/// exits non-zero so a script can detect that nothing happened.
async fn handle_drain_reply(
    socket_path: &Path,
    request: &Request,
    accepted_prefix: &str,
    refused_prefix: &str,
) -> Result<()> {
    match query_daemon(socket_path, request).await {
        Ok(Response::DaemonDrain {
            accepted,
            supervisor,
            in_flight,
            message,
            then_exit,
        }) => {
            if accepted {
                let sup_note = if then_exit {
                    "then-exit — no relaunch".to_string()
                } else {
                    supervisor.as_deref().unwrap_or("unknown").to_string()
                };
                println!("{accepted_prefix} (supervisor: {sup_note}, {in_flight} in-flight).");
                println!("{message}");
                Ok(())
            } else {
                eprintln!("{refused_prefix}: {message}");
                std::process::exit(1);
            }
        }
        Ok(Response::Error { message }) => {
            eprintln!("Daemon error: {message}");
            std::process::exit(1);
        }
        Ok(other) => {
            eprintln!("Unexpected response from daemon: {other:?}");
            std::process::exit(1);
        }
        Err(e) => {
            eprintln!("Could not reach loom-daemon at {}: {e}", socket_path.display());
            eprintln!();
            eprintln!("Is the daemon running? Start it with:");
            eprintln!("  ./.loom/scripts/cli/loom-daemon-start.sh");
            std::process::exit(1);
        }
    }
}

/// Handle the `watch` subcommand (Issue #3971). Connects to the running daemon
/// over its Unix socket and registers/lists/removes durable watches. The watches
/// are persisted machine-level (`~/.loom/watches.json`), so — like the daemon's
/// other file-backed state — they survive both this shell and a daemon restart;
/// the resolution report lands in `~/.loom/logs/watch-results.log`.
async fn handle_watch_command(action: WatchAction) -> Result<()> {
    use loom_daemon::watch_registry::WatchKind;

    let socket_path = resolve_socket_path()?;
    // `json_out` only applies to `list`; captured here so the request build below
    // does not have to smuggle it back out.
    let mut json_out = false;
    let request = match action {
        WatchAction::Add {
            number,
            pr,
            repo,
            workspace_root,
            note,
        } => Request::RegisterWatch {
            kind: if pr { WatchKind::Pr } else { WatchKind::Issue },
            number,
            repo,
            workspace_root,
            note,
        },
        WatchAction::List { json } => {
            json_out = json;
            Request::ListWatches
        }
        WatchAction::Remove { id } => Request::RemoveWatch { id },
    };

    match query_daemon(&socket_path, &request).await {
        Ok(Response::WatchRegistered {
            watch,
            already_present,
        }) => {
            if already_present {
                println!("Already watching {} (id {}) — no-op.", watch.target_label(), watch.id);
            } else {
                println!("Registered watch on {}", watch.target_label());
                println!("  id:   {}", watch.id);
                println!("Terminal state will be recorded to {}", watch_results_log_hint());
            }
            Ok(())
        }
        Ok(Response::WatchList { watches }) => {
            if json_out {
                println!("{}", serde_json::to_string_pretty(&watches)?);
            } else if watches.is_empty() {
                println!("No durable watches registered.");
            } else {
                println!("{} durable watch(es):", watches.len());
                for w in &watches {
                    let note = w
                        .note
                        .as_deref()
                        .map(|n| format!(" — {n}"))
                        .unwrap_or_default();
                    println!("  {}  {}{}", w.id, w.target_label(), note);
                }
                println!();
                println!("Resolutions are recorded to {}", watch_results_log_hint());
            }
            Ok(())
        }
        Ok(Response::WatchRemoved { id, was_present }) => {
            if was_present {
                println!("Removed watch {id}.");
            } else {
                println!("No watch with id {id} — nothing to remove (no-op).");
            }
            Ok(())
        }
        Ok(Response::Error { message }) => {
            eprintln!("Daemon error: {message}");
            std::process::exit(1);
        }
        Ok(other) => {
            eprintln!("Unexpected response from daemon: {other:?}");
            std::process::exit(1);
        }
        Err(e) => {
            eprintln!("Could not reach loom-daemon at {}: {e}", socket_path.display());
            eprintln!();
            eprintln!("Is the daemon running? Start it with:");
            eprintln!("  ./.loom/scripts/cli/loom-daemon-start.sh");
            std::process::exit(1);
        }
    }
}

/// Best-effort human hint for where watch results are recorded (honors the env
/// override). Purely cosmetic — never fails.
fn watch_results_log_hint() -> String {
    watch_registry::default_results_log_path()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "~/.loom/logs/watch-results.log".to_string())
}

/// Connect to the running daemon over its Unix socket, send a single `request`,
/// and return the parsed `Response`. Both the connect and the round-trip are
/// individually bounded so an unresponsive/wedged daemon cannot hang the CLI.
/// Mirrors `query_daemon_status` but for arbitrary single-frame requests.
async fn query_daemon(socket_path: &Path, request: &Request) -> Result<Response> {
    query_daemon_bounded(socket_path, request, Duration::from_secs(5)).await
}

/// Like [`query_daemon`] but with a caller-supplied bound on both the connect
/// and the round-trip (Issue #3952). Extracted so the `dispatch` subcommand can
/// name its own ack budget and so the timeout path is unit-testable against a
/// deliberately-unresponsive fake socket without a multi-second wait.
async fn query_daemon_bounded(
    socket_path: &Path,
    request: &Request,
    timeout: Duration,
) -> Result<Response> {
    let stream = tokio::time::timeout(timeout, UnixStream::connect(socket_path))
        .await
        .map_err(|_| anyhow!("connect timed out after {}s", timeout.as_secs()))?
        .map_err(|e| anyhow!("connect failed: {e}"))?;
    let (reader, mut writer) = stream.into_split();

    let request_json = serde_json::to_string(request)?;
    let roundtrip = async move {
        writer.write_all(request_json.as_bytes()).await?;
        writer.write_all(b"\n").await?;
        writer.flush().await?;

        let mut lines = BufReader::new(reader).lines();
        let line = lines
            .next_line()
            .await?
            .ok_or_else(|| anyhow!("daemon closed the connection without responding"))?;
        let response: Response = serde_json::from_str(&line)?;
        Ok::<Response, anyhow::Error>(response)
    };

    tokio::time::timeout(timeout, roundtrip)
        .await
        .map_err(|_| anyhow!("round-trip timed out after {}s", timeout.as_secs()))?
}

/// Default bounded ack budget for the `dispatch` subcommand (Issue #3952).
///
/// Dispatch is emphatically **not** an immediate ack: `SweepRegistry::dispatch()`
/// runs synchronously before replying, and its own documented internal budget for
/// a legitimate, successful dispatch is comfortably multi-second. It flips the
/// label via a blocking `gh issue edit` network round-trip, applies up to a 2s
/// dispatch stagger (`DEFAULT_DISPATCH_STAGGER_MS`) under concurrent dispatch,
/// and polls up to 5s (`TOKEN_NAME_CAPTURE_TIMEOUT`) for the child's account
/// name (an explicitly anticipated graceful-degradation window) before falling
/// back to `UNKNOWN_TOKEN_NAME`. A 5s client bound had essentially zero headroom
/// over that and would false-fail on a real success. We therefore mirror
/// `mcp-loom`'s `DISPATCH_TIMEOUT_MS` (`mcp-loom/src/tools/sweeps.ts`) of 30s for
/// the identical underlying IPC call: real margin over the worst case, while
/// still a bounded, finite value that never reproduces the ~1800s wedge of
/// #3945. On expiry the CLI exits nonzero with a clear "is loom-daemon running?"
/// message.
const DISPATCH_ACK_TIMEOUT: Duration = Duration::from_secs(30);

/// Env override for the dispatch ack budget, sharing the exact name
/// `mcp-loom` uses (`LOOM_DAEMON_IPC_TIMEOUT_MS`) so a single operator-facing
/// convention tunes the client-side IPC timeout across both surfaces.
const DAEMON_IPC_TIMEOUT_ENV: &str = "LOOM_DAEMON_IPC_TIMEOUT_MS";

/// Resolve the effective dispatch ack timeout.
///
/// Mirrors `mcp-loom`'s `Math.max(DISPATCH_TIMEOUT_MS, resolveDaemonIpcTimeoutMs())`
/// semantics for `dispatch_sweep`: a positive-integer-millisecond
/// `LOOM_DAEMON_IPC_TIMEOUT_MS` can only ever *raise* the bound above the 30s
/// floor (for a slow forge / heavily-loaded daemon), never lower it — lowering
/// it would reintroduce exactly the false-"did not ack" negative this widening
/// fixes. An absent, empty, non-numeric, zero, or negative value falls back to
/// the {@link DISPATCH_ACK_TIMEOUT} floor.
fn resolve_dispatch_ack_timeout() -> Duration {
    if let Ok(raw) = std::env::var(DAEMON_IPC_TIMEOUT_ENV) {
        if let Ok(ms) = raw.trim().parse::<u64>() {
            if ms > 0 {
                return Duration::from_millis(ms).max(DISPATCH_ACK_TIMEOUT);
            }
        }
    }
    DISPATCH_ACK_TIMEOUT
}

/// Build the `DispatchSweep` IPC request from the `dispatch` subcommand's args
/// (Issue #3952). Pure and side-effect-free so flag plumbing
/// (`--workspace`/`--model`/`--effort`/`--depends-on`) is unit-testable without
/// a socket. Mirrors the field mapping the `mcp__loom__dispatch_sweep` tool uses
/// so both operator surfaces enqueue byte-for-byte-equivalent requests.
fn build_dispatch_request(
    issue: u32,
    workspace: Option<String>,
    model: Option<String>,
    effort: Option<String>,
    depends_on: Option<u32>,
    force: bool,
) -> Request {
    Request::DispatchSweep {
        kind: SweepKind::Issue(issue),
        // The daemon derives its own idempotency key from the issue when this is
        // absent; an operator dispatching by hand has no key to supply.
        idempotency_key: None,
        model,
        effort,
        depends_on,
        workspace_root: workspace,
        // Host-distress circuit-breaker override (#4235): default false, so a
        // tripped breaker refuses the dispatch unless the operator passes --force.
        force,
    }
}

/// Resolve the `dispatch` subcommand's effective `--workspace` (Issue #4299):
/// an explicit `--workspace` always wins; when absent, defaults it from the
/// CLI process's own `cwd` if `cwd` falls under a registered workspace root —
/// the daemon cannot see the client's cwd, so if that resolution is going to
/// happen at all it must happen client-side, before the `DispatchSweep`
/// request is built. This is what fixes `loom-daemon dispatch <N>` run from
/// inside a registered repo previously making no difference. A `cwd` outside
/// every registered root (or an empty registry) leaves the result `None`, and
/// the daemon's own registry-based resolution (`ipc::resolve_dispatch_registry`)
/// then applies.
///
/// Pure and side-effect-free — takes the already-resolved `cwd` and
/// already-loaded `registry` rather than performing I/O itself, so it is
/// unit-testable without touching the real filesystem/env; `cwd`/`registry`
/// are resolved once by [`handle_dispatch_command`] via `std::env::current_dir()`
/// / `WorkspaceRegistry::load_default()`.
fn resolve_cli_dispatch_workspace(
    explicit: Option<String>,
    cwd: &Path,
    registry: &loom_daemon::workspace_registry::WorkspaceRegistry,
) -> Option<String> {
    explicit.or_else(|| {
        loom_daemon::workspace_registry::resolve_client_workspace_default(cwd, registry)
            .map(|root| root.to_string_lossy().into_owned())
    })
}

/// Handle the `dispatch` subcommand (Issue #3952). Connects to the running
/// daemon over its Unix socket and enqueues a sweep via the same `DispatchSweep`
/// request the MCP `dispatch_sweep` tool uses — but with a bounded client-side
/// ack timeout so a wedged daemon can never hang the CLI (the #3945 failure
/// mode). On success prints the sweep id + per-sweep log path and exits 0.
///
/// See [`resolve_cli_dispatch_workspace`] for the `--workspace` default logic
/// applied here (Issue #4299).
async fn handle_dispatch_command(
    issue: u32,
    workspace: Option<String>,
    model: Option<String>,
    effort: Option<String>,
    depends_on: Option<u32>,
    force: bool,
) -> Result<()> {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let registry =
        loom_daemon::workspace_registry::WorkspaceRegistry::load_default().unwrap_or_default();
    let workspace = resolve_cli_dispatch_workspace(workspace, &cwd, &registry);

    let socket_path = resolve_socket_path()?;
    let request = build_dispatch_request(issue, workspace, model, effort, depends_on, force);
    let ack_timeout = resolve_dispatch_ack_timeout();

    match query_daemon_bounded(&socket_path, &request, ack_timeout).await {
        Ok(Response::SweepDispatched {
            sweep_id,
            pid,
            token_name,
            log_path,
        }) => {
            println!("Dispatched sweep for issue #{issue}");
            println!("  sweep id:  {sweep_id}");
            println!("  pid:       {pid}");
            // Surface a degraded token-name capture distinctly: the daemon fell
            // back to `UNKNOWN_TOKEN_NAME` because the child hadn't logged its
            // account-selection line within the capture window — a hint the
            // dispatch was slow on the daemon side, not that it failed.
            if token_name == sweep_registry::UNKNOWN_TOKEN_NAME {
                println!("  token:     {token_name} (account not captured before ack — slow child startup)");
            } else {
                println!("  token:     {token_name}");
            }
            println!("  log path:  {}", log_path.display());
            println!();
            println!("Tail the sweep log with:");
            println!("  tail -f {}", log_path.display());
            Ok(())
        }
        Ok(Response::Error { message }) => {
            eprintln!("Daemon rejected the dispatch: {message}");
            std::process::exit(1);
        }
        Ok(Response::StructuredError(err)) => {
            eprintln!("Daemon rejected the dispatch: {}", err.message);
            std::process::exit(1);
        }
        Ok(other) => {
            eprintln!("Unexpected response from daemon: {other:?}");
            std::process::exit(1);
        }
        Err(e) => {
            eprintln!(
                "Daemon did not ack the dispatch within {}s ({e}) — is loom-daemon running?",
                ack_timeout.as_secs()
            );
            eprintln!();
            eprintln!("Start it with:");
            eprintln!("  ./.loom/scripts/cli/loom-daemon-start.sh");
            std::process::exit(1);
        }
    }
}

/// Collect per-token usage via an in-process call to
/// [`loom_daemon::tokens_pool::check::run_check`] — the same native probe
/// `loom-daemon tokens check --json` (`TokensAction::Check`) runs, called
/// directly rather than shelled out to (issue #4080, epic #4081 Phase 2).
/// `loom-daemon status` runs client-side with no supervision requirement, and
/// the probe code is already linked into this binary, so there is no reason
/// to pay a subprocess round-trip the way the historical `loom-tokens` /
/// `python3 -m` two-tier shell-out did. Best-effort — never panics, never
/// propagates an error.
///
/// `tokens_dir` is the pool directory to probe — pass the daemon's own
/// [`DaemonStatusReport::token_pool_dir`] (issue #4292), not a directory
/// re-resolved from this *client* process's own cwd. Before #4292 this probed
/// `resolve_tokens_workspace(".")` independently, so `loom-daemon status` run
/// from a directory other than the daemon's own workspace could report a
/// stale/false (e.g. 0/0 healthy) token picture even though the *daemon*
/// itself had a perfectly healthy pool — the CLI and the daemon disagreed on
/// which pool "the" pool was. Falling back to `None` only when the daemon
/// report predates #4292 keeps the pre-existing cwd-based behavior for that
/// one legacy case (an old daemon binary talking to a newer CLI).
fn collect_token_usage(tokens_dir: Option<&Path>) -> Option<serde_json::Value> {
    use loom_daemon::tokens_pool::check::{
        self, CheckOptions, CurlTransport, DEFAULT_PROBE_MODEL, DEFAULT_PROBE_PROMPT,
    };

    let tokens_dir = match tokens_dir {
        Some(dir) => dir.to_path_buf(),
        None => {
            let ws = resolve_tokens_workspace(".").ok()?;
            loom_daemon::tokens_pool::paths::resolve_tokens_dir(&ws)
        }
    };

    let opts = CheckOptions {
        source: check::resolve_source(None),
        write_ranking: false,
        probe_prompt: DEFAULT_PROBE_PROMPT,
        model: DEFAULT_PROBE_MODEL,
        stagger: true,
    };
    let report = check::run_check(&tokens_dir, &opts, &CurlTransport);
    Some(report.to_json())
}

/// Handle the `serve` subcommand (Issue #4391, dashboard phase 1 of #4329):
/// validate the requested bind address against the non-negotiable security
/// posture (loopback by default; non-loopback requires the explicit
/// `--allow-non-loopback` opt-in; a wildcard bind is refused unconditionally
/// — see [`serve::validate_bind`]), bind the TCP listener, and hand off to
/// [`serve::run`] for the accept loop. Each request re-fetches a fresh
/// snapshot over the daemon's existing Unix socket — this function never
/// touches daemon state directly.
async fn handle_serve_command(
    port: u16,
    bind: &str,
    allow_non_loopback: bool,
    peers: &str,
) -> Result<()> {
    let addr: std::net::IpAddr = bind
        .parse()
        .map_err(|e| anyhow!("invalid --bind address {bind:?}: {e}"))?;
    serve::validate_bind(addr, allow_non_loopback).map_err(|e| anyhow!(e))?;

    let socket_path = resolve_socket_path()?;
    let listener = tokio::net::TcpListener::bind((addr, port))
        .await
        .map_err(|e| anyhow!("failed to bind {addr}:{port}: {e}"))?;
    let local_addr = listener.local_addr()?;
    let peer_list = serve::parse_peer_list(peers);
    println!(
        "loom-daemon serve: listening on http://{local_addr}/ (dashboard), \
         http://{local_addr}/api/status, http://{local_addr}/api/events, \
         http://{local_addr}/api/pipeline, http://{local_addr}/api/tokens, \
         http://{local_addr}/api/peers (proxying {}){}",
        socket_path.display(),
        if peer_list.is_empty() {
            String::new()
        } else {
            format!(" — {} configured peer(s)", peer_list.len())
        }
    );

    let state = serve::ServeState::new(socket_path).with_peers(peer_list);
    serve::run_with_state(listener, state).await
}

/// Handle the `status` subcommand — render the running daemon's autonomous-mode
/// operability snapshot (Issue #3891). Fetches the daemon-native part over IPC
/// and layers on the client-side per-token usage probe.
///
/// `pipeline` opts into the forge-side pipeline snapshot (Issue #3977): per
/// managed repo, open `loom:issue`/`loom:building` counts, open PR counts by
/// review-state label, and PRs merged in the last 24h. Like the per-token
/// usage table, this is collected client-side (several `gh` calls per repo)
/// rather than inside the IPC handler, and is opt-in specifically because
/// those extra forge calls are too slow to bundle into the default view.
async fn handle_status_command(json: bool, pipeline: bool) -> Result<()> {
    let socket_path = resolve_socket_path()?;

    let report = match query_daemon_status(&socket_path).await {
        Ok(report) => report,
        Err(e) => {
            // Issue #4069 (AC3 of #4011): classify WHY the daemon is
            // unreachable using the same autonomy-desired marker + heartbeat
            // `loom-daemon-watchdog.sh` reads, so `status` and the watchdog
            // log can never disagree. Purely local, read-only, never fails
            // the command — `install_state` is `None` only when no loom dir
            // can be resolved at all, in which case we fall back to the
            // pre-#4069 generic message.
            let install_state = daemon_install_state::probe();
            let exit_code = install_state
                .as_ref()
                .map_or(daemon_install_state::EXIT_NOT_EXPECTED, |r| r.state.exit_code());
            if json {
                print_status_unreachable_json(&socket_path, &e, install_state.as_ref())?;
            } else {
                print_status_unreachable_human(&socket_path, &e, install_state.as_ref());
            }
            std::process::exit(exit_code);
        }
    };

    // Per-token usage is a slow per-account network probe the daemon deliberately
    // does NOT perform inside the IPC handler; collect it client-side here —
    // but against the SAME pool directory the daemon itself resolved (#4292),
    // not one independently re-derived from this CLI invocation's own cwd.
    let token_usage = collect_token_usage(report.token_pool_dir.as_deref());

    // Self-update staleness (#3968): purely local, read-only — compares the
    // commit baked into THIS `loom-daemon status` binary against the source
    // checkout's current HEAD, when that checkout is still on this machine.
    // Advisory only; never triggers a rebuild or restart (see
    // `.loom/scripts/cli/loom-daemon-update.sh` for the opt-in update flow).
    let update = self_update::check();

    // Forge-side pipeline snapshot (#3977) — opt-in, fetched in the same
    // priority order as `report.per_repo` so the rendered table lines up with
    // the "Managed repos" dispatch table above it.
    let pipeline_snapshots = if pipeline {
        let roots: Vec<PathBuf> = report.per_repo.iter().map(|r| r.root.clone()).collect();
        let source = Arc::new(loom_daemon::pipeline_snapshot::GhPipelineSource::new());
        Some(loom_daemon::pipeline_snapshot::collect_pipeline_snapshots(source, roots).await)
    } else {
        None
    };

    if json {
        print_status_json(&report, token_usage.as_ref(), &update, pipeline_snapshots.as_deref())?;
    } else {
        print_status_human(&report, token_usage.as_ref(), &update, pipeline_snapshots.as_deref());
    }
    Ok(())
}

/// Emit the unreachable-daemon `--json` error, state-aware (Issue #4069). The
/// existing `error` prose key is retained for compatibility; `install_state`
/// (when the probe could classify at all) adds a machine-readable enum plus
/// the diagnostic fields a script or human can act on.
fn print_status_unreachable_json(
    socket_path: &Path,
    err: &anyhow::Error,
    install_state: Option<&InstallStateReport>,
) -> Result<()> {
    let mut payload = serde_json::json!({
        "error": format!("could not reach loom-daemon at {}: {err}", socket_path.display()),
    });
    if let Some(r) = install_state {
        payload["install_state"] = serde_json::json!({
            "state": r.state.as_str(),
            "started_at": r.started_at,
            "pid": r.pid,
            "liveness_detail": r.liveness_detail,
            "heartbeat": {
                "freshness": r.heartbeat_freshness.map(daemon_install_state::HeartbeatFreshness::as_str),
                "age_secs": r.heartbeat_age_secs,
                "stale_threshold_secs": r.heartbeat_stale_threshold_secs,
            },
            "process_age_secs": r.process_age_secs,
            "startup_grace_threshold_secs": r.startup_grace_threshold_secs,
            "watchdog_log": r.watchdog_log_path.display().to_string(),
        });
    }
    println!("{}", serde_json::to_string_pretty(&payload)?);
    Ok(())
}

/// Handle `loom-daemon fleet status` (#4342, epic #4340): collect the local
/// host's own status in-process, fan out to every registered fleet worker
/// concurrently, merge, render, and exit non-zero unless every roster host is
/// `UP`. Thin clap→module wiring: the merge/render/exit-code logic lives in
/// [`loom_daemon::fleet::status`]; only the local-host collection (which needs
/// this binary's own socket/install-state machinery) lives here.
async fn handle_fleet_status_command(json: bool) -> Result<()> {
    use loom_daemon::fleet::status::{collect_fleet_report, SshStatusSource};
    use loom_daemon::fleet::FleetRegistry;

    let registry = FleetRegistry::load_default()?;
    let local = collect_local_fleet_report().await;
    let source: Arc<dyn loom_daemon::fleet::status::HostStatusSource> =
        Arc::new(SshStatusSource::new());
    let timeout = Duration::from_secs(loom_daemon::fleet::status::DEFAULT_TIMEOUT_SECS);
    let report = collect_fleet_report(source, registry, local, timeout).await;

    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print!("{}", report.render_human());
    }
    std::process::exit(report.exit_code());
}

/// Collect the local host's own [`loom_daemon::fleet::status::HostReport`] —
/// in-process, over the daemon's Unix socket (never `ssh localhost`, per
/// #4342's implementation guidance). Reuses the exact same
/// [`build_status_json_value`] payload shape `loom-daemon status --json`
/// emits, so the local row's fields line up with every remote host's
/// self-reported `status --json` (#4069's unreachable-daemon classification is
/// reused for the down case too).
async fn collect_local_fleet_report() -> loom_daemon::fleet::status::HostReport {
    use loom_daemon::fleet::status::HostReport;

    let socket_path = match resolve_socket_path() {
        Ok(path) => path,
        Err(e) => {
            return HostReport::local_down(format!("could not resolve daemon socket path: {e}"))
        }
    };

    match query_daemon_status(&socket_path).await {
        Ok(daemon_report) => {
            let token_usage = collect_token_usage(daemon_report.token_pool_dir.as_deref());
            let update = self_update::check();
            let value =
                build_status_json_value(&daemon_report, token_usage.as_ref(), &update, None);
            HostReport::local_up(value)
        }
        Err(e) => {
            // Reuse the #4069 install-state classification so the local row's
            // "why is it down" detail matches what `loom-daemon status --json`
            // itself would report for this same daemon.
            let install_state = daemon_install_state::probe();
            let detail = match install_state {
                Some(r) => format!("{} ({e})", r.state.as_str()),
                None => format!("daemon unreachable: {e}"),
            };
            HostReport::local_down(detail)
        }
    }
}

/// Emit the unreachable-daemon human-readable error, state-aware (Issue
/// #4069). Remediation advice differs per state: `NotExpected` /
/// `ExpectedButDead` suggest a start; `AliveStarting` (#4213) reports a normal
/// in-progress startup and prints NO remediation; `AliveButUnresponsive` does
/// NOT suggest a start either (the singleton guard would refuse it) and instead
/// points at the live pid.
fn print_status_unreachable_human(
    socket_path: &Path,
    err: &anyhow::Error,
    install_state: Option<&InstallStateReport>,
) {
    eprintln!("Could not reach loom-daemon at {}: {err}", socket_path.display());
    eprintln!();

    match install_state {
        None => {
            // Undiagnosable (no loom dir could be resolved) — the pre-#4069
            // generic fallback.
            eprintln!("Is the daemon running? Start it with:");
            eprintln!("  ./.loom/scripts/cli/loom-daemon-start.sh");
        }
        Some(r) => match r.state {
            daemon_install_state::InstallState::NotExpected => {
                eprintln!(
                    "No autonomy-desired marker found — a daemon is not currently expected \
                     to be running on this host."
                );
                eprintln!();
                eprintln!("Start it with:");
                eprintln!("  ./.loom/scripts/cli/loom-daemon-start.sh");
            }
            daemon_install_state::InstallState::ExpectedButDead => {
                let started = r.started_at.as_deref().unwrap_or("unknown");
                eprintln!(
                    "A daemon is EXPECTED (autonomy-desired marker present, started {started}) \
                     but is NOT running: {}.",
                    r.liveness_detail.as_deref().unwrap_or("no liveness detail")
                );
                eprintln!(
                    "Autonomous dispatch has stopped — this is the silent-autonomy-loss \
                     scenario (#4011)."
                );
                eprintln!();
                eprintln!("Recover with:");
                eprintln!("  ./.loom/scripts/cli/loom-daemon-start.sh");
                eprintln!("See {} for prior divergence reports.", r.watchdog_log_path.display());
            }
            daemon_install_state::InstallState::AliveStarting => {
                let detail = r.liveness_detail.as_deref().unwrap_or("process alive");
                let grace = r.startup_grace_threshold_secs.unwrap_or_default();
                eprintln!("The daemon process IS alive ({detail}) but is not responding over IPC.");
                eprintln!(
                    "It is still STARTING (process age {}s ≤ {grace}s grace) — its IPC socket has \
                     not bound yet (normal for up to ~{grace}s after a bootout/bootstrap restart).",
                    r.process_age_secs.unwrap_or_default()
                );
                eprintln!();
                eprintln!(
                    "This is NOT a fault — no action needed. Re-run `loom-daemon status` in a few \
                     seconds; the socket should bind and status will succeed."
                );
                // Deliberately NOT printing the stop/start remediation: doing so
                // during every normal restart is exactly the ghost-chase #4213
                // set out to prevent.
            }
            daemon_install_state::InstallState::AliveButUnresponsive => {
                let detail = r.liveness_detail.as_deref().unwrap_or("process alive");
                eprintln!("The daemon process IS alive ({detail}) but is not responding over IPC.");
                match r.heartbeat_freshness {
                    Some(daemon_install_state::HeartbeatFreshness::Fresh) => {
                        eprintln!(
                            "Heartbeat is fresh ({}s ago) — likely an IPC/socket-layer fault, \
                             not a wedged daemon.",
                            r.heartbeat_age_secs.unwrap_or_default()
                        );
                    }
                    Some(daemon_install_state::HeartbeatFreshness::Stale) => {
                        eprintln!(
                            "Heartbeat is STALE ({}s ago, > {}s threshold) — the daemon is likely \
                             wedged.",
                            r.heartbeat_age_secs.unwrap_or_default(),
                            r.heartbeat_stale_threshold_secs.unwrap_or_default()
                        );
                    }
                    Some(daemon_install_state::HeartbeatFreshness::PriorBoot) => {
                        eprintln!(
                            "Heartbeat file is from a PREVIOUS boot ({}s old; this process is \
                             only {}s old) — it is not evidence about the current process. (A \
                             daemon that wedged before writing its first heartbeat this boot \
                             would look identical — re-check after the process is well past \
                             startup if you still suspect a wedge.)",
                            r.heartbeat_age_secs.unwrap_or_default(),
                            r.process_age_secs.unwrap_or_default()
                        );
                    }
                    _ => {
                        eprintln!(
                            "Heartbeat status unknown (no heartbeat file, or disabled) — \
                             liveness-only signal."
                        );
                    }
                }
                eprintln!();
                // Advice gating (#4368): the imperative stop/start remediation
                // is only warranted for a *current-boot* Stale verdict — the
                // one case where the evidence actually points at a wedge.
                // Fresh/Unknown/PriorBoot get inspect-first guidance instead,
                // so an operator is never steered into restarting a daemon
                // that is merely mid-fault-diagnosis or missing heartbeat
                // evidence, not actually wedged.
                if r.heartbeat_freshness == Some(daemon_install_state::HeartbeatFreshness::Stale) {
                    if let Some(pid) = r.pid {
                        eprintln!(
                            "Do NOT run loom-daemon-start.sh — the singleton guard will refuse"
                        );
                        eprintln!(
                            "while pid {pid} is alive. Inspect it directly, or restart explicitly:"
                        );
                    } else {
                        eprintln!(
                            "Do NOT run loom-daemon-start.sh — the singleton guard will refuse"
                        );
                        eprintln!(
                            "while the daemon is alive. Inspect it directly, or restart explicitly:"
                        );
                    }
                    eprintln!("  ./.loom/scripts/cli/loom-daemon-stop.sh && ./.loom/scripts/cli/loom-daemon-start.sh");
                } else {
                    eprintln!("Inspect before acting — this evidence does not indicate a wedge:");
                    if let Some(pid) = r.pid {
                        eprintln!(
                            "  ps -p {pid} -o pid,etime,command   # confirm what it is actually doing"
                        );
                    }
                    eprintln!("  loom-daemon status --json           # machine-readable detail");
                    eprintln!("If it is still unresponsive after inspecting, restart explicitly:");
                    eprintln!("  ./.loom/scripts/cli/loom-daemon-stop.sh && ./.loom/scripts/cli/loom-daemon-start.sh");
                }
            }
        },
    }
}

/// The capacity figures the whole status view shares, resolved from a single
/// source so the summary, the healthy-tokens cap input, and the per-token table
/// never contradict each other (issue #3936).
///
/// Preference order:
/// 1. **fresh probe** — when a client-side `loom-tokens check --json` succeeded
///    (the *same* data that renders the per-token table), the health counts are
///    derived from it via [`loom_daemon::capacity::summarize_probe`], applying
///    the near-ceiling threshold uniformly. This is the accurate *current*
///    capacity and matches the table row-for-row.
/// 2. **daemon ranking** — when no fresh probe is available but the daemon
///    reported a parsed `.loom/tokens/.ranking`, fall back to its snapshot.
/// 3. **raw pool** — no probe and no ranking: the pre-#3902 flat pool basis.
struct ResolvedCapacity {
    /// Where the figures came from — one of `"probe"`, `"ranking"`, `"pool"`.
    source: &'static str,
    /// Whether any account-health data (probe or ranking) was available.
    ranking_present: bool,
    total: usize,
    healthy: usize,
    exhausted: usize,
    /// Health-adjusted token axis (healthy accounts, or the raw pool as a
    /// fallback) — the "healthy tokens" input to the dynamic concurrency cap.
    token_axis_limit: usize,
    /// The effective dynamic cap consistent with `token_axis_limit`:
    /// `min(token_axis_limit, disk_headroom, cpu_headroom, configured_max)`
    /// (CPU term added in #3978).
    effective_cap: usize,
    /// Whether the token axis is the binding (minimum) constraint.
    token_bound: bool,
}

/// Resolve the shared capacity figures for a status render (#3936). Prefers the
/// fresh client-side probe over the daemon's possibly-stale ranking snapshot so
/// the summary count, the cap's healthy-tokens input, and the per-token table
/// all agree.
fn resolve_capacity(
    report: &DaemonStatusReport,
    token_usage: Option<&serde_json::Value>,
) -> ResolvedCapacity {
    // Tier 1: fresh probe — the same source as the per-token table.
    if let Some(usage) = token_usage {
        if let Some(accounts) = usage.get("accounts").and_then(serde_json::Value::as_array) {
            let pairs: Vec<(&str, Option<f64>)> = accounts
                .iter()
                .map(|a| {
                    let status = a
                        .get("status")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("?");
                    let util_7d = a.get("7d_utilization").and_then(serde_json::Value::as_f64);
                    (status, util_7d)
                })
                .collect();
            let cap = loom_daemon::capacity::summarize_probe(pairs.iter().copied());
            let token_axis_limit = cap.healthy;
            // The token axis of the cap is `healthy × per-token` (#3947); treat a
            // pre-#3947 wire `0` as the effective floor of 1.
            let factor = report.per_token_concurrency.max(1);
            let token_axis_effective = token_axis_limit.saturating_mul(factor);
            // The CPU term (#3978) is policy-floored at 1 by
            // `cpu_headroom::cpu_headroom` on every real computation, so a wire
            // value of exactly `0` unambiguously means "an older daemon that
            // predates #3978 didn't send this field" (`#[serde(default)]`) —
            // not a genuine zero-headroom reading. Treat it as unconstrained
            // rather than collapsing the cap to 0 against a daemon that never
            // computed a CPU term at all.
            let cpu_headroom = if report.cpu_headroom == 0 {
                usize::MAX
            } else {
                report.cpu_headroom
            };
            let effective_cap = token_axis_effective
                .min(report.disk_headroom)
                .min(cpu_headroom)
                .min(report.configured_max);
            let token_bound = token_axis_effective <= report.disk_headroom
                && token_axis_effective <= cpu_headroom
                && token_axis_effective <= report.configured_max;
            return ResolvedCapacity {
                source: "probe",
                ranking_present: true,
                total: cap.total,
                healthy: cap.healthy,
                exhausted: cap.exhausted,
                token_axis_limit,
                effective_cap,
                token_bound,
            };
        }
    }

    // Tier 2: daemon-reported ranking snapshot.
    if report.capacity.ranking_present {
        return ResolvedCapacity {
            source: "ranking",
            ranking_present: true,
            total: report.capacity.total_accounts,
            healthy: report.capacity.healthy_accounts,
            exhausted: report.capacity.exhausted_accounts,
            token_axis_limit: report.capacity.token_axis_limit,
            effective_cap: report.dynamic_cap,
            token_bound: report.capacity.token_bound,
        };
    }

    // Tier 3: no probe, no ranking — raw pool basis (pre-#3902 behavior).
    ResolvedCapacity {
        source: "pool",
        ranking_present: false,
        total: report.token_pool_size,
        healthy: 0,
        exhausted: 0,
        token_axis_limit: report.token_pool_size,
        effective_cap: report.dynamic_cap,
        token_bound: report.capacity.token_bound,
    }
}

/// Build the combined status payload (daemon report + per-token usage) as a
/// [`serde_json::Value`] — the shared value builder behind both `loom-daemon
/// status --json` ([`print_status_json`]) and each fleet host's own
/// self-reported status, including the local host's row collected in-process
/// by `fleet status` (#4342, [`collect_local_fleet_report`]) — keeping the two
/// call sites' JSON shape identical by construction rather than by
/// convention.
fn build_status_json_value(
    report: &DaemonStatusReport,
    token_usage: Option<&serde_json::Value>,
    update: &self_update::SelfUpdateStatus,
    pipeline: Option<&[loom_daemon::pipeline_snapshot::RepoPipelineSnapshot]>,
) -> serde_json::Value {
    let rc = resolve_capacity(report, token_usage);
    serde_json::json!({
        "in_flight_count": report.in_flight.len(),
        "in_flight": report.in_flight,
        // Live-locked-but-unregistered sweeps (#4214): a live `owner_pid` lock
        // with no matching `in_flight` entry. Non-empty here means a sweep is
        // demonstrably alive (the lock proves it) but the in-memory registry
        // union above has lost track of it — read these as **alive**, not
        // dead, and reconcile rather than re-dispatching. Empty in the
        // overwhelmingly common case.
        "unregistered_locked_count": report.unregistered_locked.len(),
        "unregistered_locked": report.unregistered_locked.iter().map(|u| serde_json::json!({
            "root": u.root,
            "issue": u.issue,
            "owner_pid": u.owner_pid,
        })).collect::<Vec<_>>(),
        // "Currently binding" vs "smallest ceiling" (#4031): the cap only binds
        // once in-flight reaches it. `false` ⇒ the limiter is work availability,
        // not any resource term, so scripted consumers don't misread the
        // token/CPU ceiling as a bottleneck at low occupancy.
        "capacity_bound": report.capacity_bound,
        // Claude-wrapper pre-flight-death workspace tripwire (#4386): `true`
        // means N consecutive dispatches, across different issues, died at
        // the wrapper's MCP-init pre-flight check before ever reaching
        // `# CLAUDE_CLI_START` — the classic stale-`.mcp.json` fleet-wide
        // silent-failure signature. `message` is `null` when not tripped.
        "preflight_advisory_active": report.preflight_advisory_active,
        "preflight_advisory_message": report.preflight_advisory_message,
        "dynamic_cap": {
            "token_pool_size": report.token_pool_size,
            // The directory the daemon resolved for the pool above (#4292) —
            // `null` only from a pre-#4292 daemon binary that never computed
            // one. Lets an operator confirm at a glance which of the
            // per-repo/shared pools is actually in effect, independent of
            // whatever cwd `loom-daemon status` itself was run from.
            "token_pool_dir": report.token_pool_dir,
            "disk_headroom": report.disk_headroom,
            // CPU headroom term (#3978; measured-idle signal #4031) — see the
            // field docs on `DaemonStatusReport::cpu_headroom` for the pre-#3978
            // `0` ⇒ "field absent" wire-compat convention.
            "cpu_headroom": report.cpu_headroom,
            "logical_cpus": report.logical_cpus,
            "loadavg_1m": report.loadavg_1m,
            // Measured CPU idle fraction (#4031), the signal that replaced
            // loadavg as the source of consumed cores. `null` until sampled.
            "cpu_idle_fraction": report.cpu_idle_fraction,
            "configured_max": report.configured_max,
            "per_token_concurrency": report.per_token_concurrency.max(1),
            "token_axis_effective": rc.token_axis_limit.saturating_mul(report.per_token_concurrency.max(1)),
            "effective": rc.effective_cap,
        },
        "capacity": {
            "source": rc.source,
            "ranking_present": rc.ranking_present,
            "total_accounts": rc.total,
            "healthy_accounts": rc.healthy,
            "exhausted_accounts": rc.exhausted,
            "token_axis_limit": rc.token_axis_limit,
            "token_bound": rc.token_bound,
        },
        "main_health_gate": {
            "halted": report.main_health_gate_halted,
            // "Not evaluated" is distinct from "halted" (verified-red main) —
            // #3950 AC3. Both can be true at once: a prior halt from a
            // genuinely red run persists untouched while a later tick can't
            // even evaluate (dirty tree, timeout, missing tool, broken `git`).
            "not_evaluated": report.main_health_gate_not_evaluated,
            // Which failure class actually blocked evaluation (#3974 AC2).
            "not_evaluated_reason": report.main_health_gate_not_evaluated_reason,
            // Whether the gate is actually enabled for this root, and when its
            // last completed verdict landed (#4012) — the disambiguators
            // between "disabled", "pending" (enabled, no verdict yet), and
            // "clear" (verified green), all three of which pre-#4012 rendered
            // identically as `halted: false, not_evaluated: false`.
            "enabled": report.main_health_gate_enabled,
            "verdict_at": report.main_health_gate_verdict_at,
            // Load-aware deferral + tier label (#4259). `deferred` is a bounded
            // scheduling decision distinct from both `halted` and
            // `not_evaluated`; `verdict_tier` ("full"/"fast") keeps a fast-tier
            // green distinguishable from a full-suite green.
            "deferred": report.main_health_gate_deferred,
            "deferred_reason": report.main_health_gate_deferred_reason,
            "verdict_tier": report.main_health_gate_verdict_tier,
        },
        // Startup forge-credential preflight (#4005) — resolved once at
        // daemon boot, before the daemon's first `gh` consumer. Never
        // contains a token value; `null` only from a pre-#4005 daemon binary
        // that never computed one.
        "credential_preflight": report.credential_preflight.as_ref().map(|c| serde_json::json!({
            "ok": c.ok,
            "mechanism": c.mechanism,
            "fingerprint": c.fingerprint,
            "message": c.message,
            "checked_at": c.checked_at,
        })),
        // Scheduled drain-and-restart state (#4090). `draining: false` in the
        // common case; `note` carries the last transition (timeout refusal /
        // abort) so a scripted consumer sees why a drain ended without a restart.
        "drain": {
            "draining": report.draining,
            "deadline": report.drain_deadline,
            "note": report.drain_note,
        },
        // Per-repo breakdown across every registered managed workspace (#3930).
        "per_repo": report.per_repo.iter().map(|r| serde_json::json!({
            "root": r.root,
            "priority": r.priority,
            "in_flight_count": r.in_flight_count,
            "health_gate_halted": r.health_gate_halted,
            "health_gate_not_evaluated": r.health_gate_not_evaluated,
            "health_gate_not_evaluated_reason": r.health_gate_not_evaluated_reason,
            "health_gate_enabled": r.health_gate_enabled,
            "health_gate_verdict_at": r.health_gate_verdict_at,
            "health_gate_deferred": r.health_gate_deferred,
            "health_gate_deferred_reason": r.health_gate_deferred_reason,
            "health_gate_verdict_tier": r.health_gate_verdict_tier,
            // Per-root role-runner enablement (#4377) — resolved from THIS
            // root's own `.loom/config.json`, independent of the daemon
            // workspace's own master switch. `on_idle_roles` non-empty while
            // `enabled` is `false` is the exact silent-no-op this issue fixes.
            "role_runner_enabled": r.role_runner_enabled,
            "role_runner_roles": r.role_runner_roles,
            "role_runner_on_idle_roles": r.role_runner_on_idle_roles,
        })).collect::<Vec<_>>(),
        // Forge-side pipeline snapshot (#3977) — present only when `--pipeline`
        // was passed; `null` otherwise so a consumer can tell "not requested"
        // apart from "requested but empty".
        "pipeline": pipeline.map(|snapshots| snapshots.iter().map(|s| serde_json::json!({
            "root": s.root,
            "queued": s.queued,
            "building": s.building,
            "review_requested": s.review_requested,
            "changes_requested": s.changes_requested,
            "approved": s.approved,
            "merged_24h": s.merged_24h,
            "error": s.error,
        })).collect::<Vec<_>>()),
        "token_usage": token_usage,
        // Self-update staleness (#3968) — read-only, local-only comparison of
        // this binary's baked-in commit vs. the source checkout's HEAD.
        "self_update": {
            "built_commit": update.built_commit,
            "source_commit": update.source_commit,
            "update_available": update.update_available,
        },
        // Autonomous self-update loop state (#4055) — daemon-side loop status
        // (distinct from the client-side `self_update` staleness read above).
        "auto_update": {
            "enabled": report.auto_update_enabled,
            "last_check": report.auto_update_last_check,
            "last_roll": report.auto_update_last_roll,
            "consecutive_failures": report.auto_update_consecutive_failures,
            "backoff_secs": report.auto_update_backoff_secs,
            "terminal_reason": report.auto_update_terminal_reason,
            "note": report.auto_update_note,
        },
        // Host-distress circuit breaker (#4235) — `null` when no breaker is
        // registered (work-finder off / breaker disabled). Otherwise the current
        // phase (closed/open/cooldown), why it tripped, and the cool-down
        // release time so a scripted consumer sees a paused-dispatch host.
        "host_breaker": report.host_breaker.as_ref().map(|h| serde_json::json!({
            "enabled": h.enabled,
            "phase": h.phase,
            "suppressed": h.suppressed,
            "reason": h.reason,
            "tripped_at": h.tripped_at,
            "releases_at": h.releases_at,
            "last_load_per_core": h.last_load_per_core,
            "load_per_core_threshold": h.load_per_core_threshold,
            "sustain_ticks": h.sustain_ticks,
            "cooldown_secs": h.cooldown_secs,
        })),
        "rate_limit_breaker": report.rate_limit_breaker.as_ref().map(|r| serde_json::json!({
            "enabled": r.enabled,
            "phase": r.phase,
            "suppressed": r.suppressed,
            "source": r.source,
            "tripped_at": r.tripped_at,
            "cooldown_until": r.cooldown_until,
            "trips_total": r.trips_total,
            "core_remaining": r.core_remaining,
            "graphql_remaining": r.graphql_remaining,
            "budget_probed_at": r.budget_probed_at,
        })),
        // Live safehouse fleet-comms connection state (#4345) — `null` only
        // from a pre-#4345 daemon binary that never computed one. `state` is
        // one of "not_configured" / "unreachable" / "connected".
        "safehouse": report.safehouse.as_ref().map(|s| serde_json::json!({
            "state": s.state,
            "socket": s.socket,
            "room": s.room,
        })),
    })
}

/// Emit the combined status (daemon report + per-token usage) as JSON.
fn print_status_json(
    report: &DaemonStatusReport,
    token_usage: Option<&serde_json::Value>,
    update: &self_update::SelfUpdateStatus,
    pipeline: Option<&[loom_daemon::pipeline_snapshot::RepoPipelineSnapshot]>,
) -> Result<()> {
    let combined = build_status_json_value(report, token_usage, update, pipeline);
    println!("{}", serde_json::to_string_pretty(&combined)?);
    Ok(())
}

/// The reportable main-health-gate condition for one workspace root (#4012).
///
/// Pre-#4012, `loom-daemon status` derived its summary from just the
/// `halted`/`not_evaluated` boolean pair — and `(false, false)` meant any of
/// three genuinely different things: the gate is disabled, the gate is
/// enabled but has not completed its first evaluation this process
/// ("pending"), or the gate's last completed run verified `main` green
/// ("clear"). Two booleans cannot encode three states, so this enum widens
/// the reporting boundary rather than reusing the same pair for a fourth
/// meaning. [`classify_gate_verdict`] builds one from the raw wire-report
/// ingredients; [`format_gate_status`] (long form) and
/// [`gate_status_short_label`] (13-char table column) both render it, so the
/// top-level summary and the per-repo table can never disagree.
#[derive(Debug, Clone, PartialEq, Eq)]
enum GateVerdict {
    /// The gate is not enabled for this root — or is enabled but has no
    /// usable `buildGate` block, which the gate loop treats identically
    /// (always-green, never runs). Dispatch is allowed; nothing will ever
    /// evaluate this root until it is turned on (and configured).
    Disabled,
    /// The gate is enabled but has not completed a first evaluation yet this
    /// daemon process. Dispatch is allowed — this is NOT evidence that `main`
    /// is healthy, only that nothing has said otherwise yet.
    Pending,
    /// The gate's most recent completed run verified `main` green. `since`
    /// is the wall-clock time of that verdict, when known (#4012 AC4) — a
    /// `clear` reading with no `since` predates the daemon populating it.
    /// `tier` (#4259) labels which stage set produced it (`"fast"` ⇒ a
    /// compile+smoke subset, NOT a full-suite green); `None` for a full-tier
    /// verdict or a pre-#4259 payload.
    Clear {
        since: Option<DateTime<Utc>>,
        tier: Option<String>,
    },
    /// The most recent tick DEFERRED for host load (#4259): the host was
    /// saturated and the bounded max-defer window had not yet elapsed, so no
    /// command ran. NOT evidence about `main` either way; dispatch is NOT
    /// halted by this. Distinct from `NotEvaluated` — the gate chose not to run,
    /// it did not run and fail to conclude.
    Deferred { reason: Option<String> },
    /// The most recent tick could not produce a verdict at all (dirty tree,
    /// timeout, missing tool, broken `git`, …) — NOT evidence about `main`
    /// either way; dispatch is NOT halted by this.
    NotEvaluated { reason: Option<String> },
    /// A completed run verified `main` red — dispatch is paused (in-flight
    /// sweeps keep running). `not_evaluated` records whether a *later* tick
    /// also failed to produce a verdict (#3950 AC3): the two can co-occur,
    /// since an unevaluated tick leaves the prior halt untouched.
    Halted {
        not_evaluated: bool,
        reason: Option<String>,
    },
}

/// Classify the reportable gate condition from a [`DaemonStatusReport`] /
/// [`crate::types::RepoStatus`]'s raw fields (#4012).
///
/// `enabled` is `Some(false)` only when the daemon positively resolved this
/// root's gate as off (or effectively off — enabled but no usable
/// `buildGate` block, via [`main_health_gate::effective_enabled`]); `None`
/// means an older daemon that never reported the flag at all, which must NOT
/// be misread as "disabled" (a bare `bool`'s wire default would do exactly
/// that — see the `Option<bool>` rationale on
/// [`DaemonStatusReport::main_health_gate_enabled`]). `halted` and
/// `not_evaluated` take priority over disabled/pending so a genuinely active
/// halt is never hidden behind either newer state — a case that in practice
/// only arises from a test poking the raw state directly, since the gate
/// loop's own disabled path always clears `halted` first.
// A pure classifier that maps the raw, independent gate status fields (each
// carried separately on `DaemonStatusReport` / `RepoStatus` with its own
// `#[serde(default)]`) onto one verdict. The argument count tracks the field
// count 1:1 by design; grouping them into a struct here would just move the
// same primitives around without adding meaning.
#[allow(clippy::too_many_arguments)]
fn classify_gate_verdict(
    enabled: Option<bool>,
    halted: bool,
    not_evaluated: bool,
    deferred: bool,
    reason: Option<&str>,
    deferred_reason: Option<&str>,
    verdict_tier: Option<&str>,
    verdict_at: Option<DateTime<Utc>>,
) -> GateVerdict {
    if halted {
        return GateVerdict::Halted {
            not_evaluated,
            reason: reason.map(str::to_string),
        };
    }
    // A load-deferral (#4259) is a current-tick scheduling decision; surface it
    // ahead of `not_evaluated` (a deferred tick clears the unevaluated flag, so
    // in practice they do not co-occur) and ahead of the disabled/pending/clear
    // readings, so the operator sees "the host is too busy to run the gate right
    // now" rather than a stale green.
    if deferred {
        return GateVerdict::Deferred {
            reason: deferred_reason.map(str::to_string),
        };
    }
    if not_evaluated {
        return GateVerdict::NotEvaluated {
            reason: reason.map(str::to_string),
        };
    }
    if enabled == Some(false) {
        return GateVerdict::Disabled;
    }
    if verdict_at.is_none() {
        return GateVerdict::Pending;
    }
    GateVerdict::Clear {
        since: verdict_at,
        tier: verdict_tier.map(str::to_string),
    }
}

/// Render the main-health gate summary line for `loom-daemon status`.
///
/// Before #3974 this line asserted "workspace tree is dirty" for every skip,
/// which reported a clean tree as dirty whenever the real cause was a
/// timeout, a missing build tool, or a broken `git`; before #4012 `clear` and
/// "the gate has never run" were the same string.
fn format_gate_status(verdict: &GateVerdict) -> String {
    match verdict {
        GateVerdict::Disabled => "disabled (gate not enabled; dispatch allowed)".to_string(),
        GateVerdict::Pending => {
            "pending (no verdict yet this process — dispatch allowed)".to_string()
        }
        GateVerdict::Clear { since, tier } => {
            // #4259: a fast-tier green covers only the compile+smoke subset, so
            // it must never read as an unqualified "clear".
            let tier_suffix = match tier.as_deref() {
                Some("fast") => " [fast tier — compile+smoke only, NOT a full-suite green]",
                _ => "",
            };
            match since {
                Some(t) => format!(
                    "clear (dispatch allowed; last verified green at {}){tier_suffix}",
                    t.to_rfc3339()
                ),
                None => format!("clear (dispatch allowed){tier_suffix}"),
            }
        }
        GateVerdict::Deferred { reason } => {
            let detail = reason
                .clone()
                .unwrap_or_else(|| "host saturated".to_string());
            format!(
                "deferred ({detail}) — the host is too busy to run the gate right now, which is \
                 NOT evidence about main; dispatch is NOT halted by this. The fast tier runs at \
                 the max-defer bound so a permanently-loaded host still reaches a verdict"
            )
        }
        GateVerdict::NotEvaluated { reason } => {
            let cause = reason
                .clone()
                .unwrap_or_else(|| "cause unrecorded".to_string());
            format!(
                "not evaluated ({cause}) — the gate could not run, which is NOT evidence about \
                 main; dispatch is NOT halted by this"
            )
        }
        GateVerdict::Halted {
            not_evaluated,
            reason,
        } => {
            if *not_evaluated {
                let cause = reason
                    .clone()
                    .unwrap_or_else(|| "cause unrecorded".to_string());
                format!(
                    "HALTED (main verified red — new dispatch paused) + NOT EVALUATED ({cause}) — \
                     the gate cannot currently confirm main is still red, or check for recovery"
                )
            } else {
                "HALTED (main verified red — new dispatch paused; in-flight sweeps keep running)"
                    .to_string()
            }
        }
    }
}

/// Render `verdict` as a short label for the per-repo table's 13-char `GATE`
/// column (#4012) — the short-form counterpart to [`format_gate_status`].
fn gate_status_short_label(verdict: &GateVerdict) -> &'static str {
    match verdict {
        GateVerdict::Disabled => "disabled",
        GateVerdict::Pending => "pending",
        GateVerdict::Clear { tier: Some(t), .. } if t == "fast" => "clear(fast)",
        GateVerdict::Clear { .. } => "clear",
        GateVerdict::Deferred { .. } => "deferred",
        GateVerdict::NotEvaluated { .. } => "not-evaluated",
        GateVerdict::Halted {
            not_evaluated: true,
            ..
        } => "HALTED+UNEVAL",
        GateVerdict::Halted {
            not_evaluated: false,
            ..
        } => "HALTED",
    }
}

/// Emit the combined status as a human-readable table.
fn print_status_human(
    report: &DaemonStatusReport,
    token_usage: Option<&serde_json::Value>,
    update: &self_update::SelfUpdateStatus,
    pipeline: Option<&[loom_daemon::pipeline_snapshot::RepoPipelineSnapshot]>,
) {
    println!("\n=== Loom Autonomous Daemon Status ===\n");

    println!("In-flight sweeps: {}", report.in_flight.len());
    if report.in_flight.is_empty() {
        println!("  (none)");
    } else {
        println!("  {:<30} {:>7} {:>8}  {:<20} PHASE", "SWEEP", "ISSUE", "PID", "TOKEN");
        println!("  {:-<75}", "");
        for s in &report.in_flight {
            let issue = match &s.kind {
                SweepKind::Issue(n) => format!("#{n}"),
                SweepKind::PrSet(_) => "prs".to_string(),
            };
            let phase = s.latest_phase.as_deref().unwrap_or("-");
            println!(
                "  {:<30} {:>7} {:>8}  {:<20} {}",
                s.sweep_id, issue, s.pid, s.token_name, phase
            );
        }
    }

    // Live-locked-but-unregistered sweeps (#4214): a sweep whose per-issue lock
    // has a live `owner_pid` but no matching in-flight entry above. Non-empty
    // means the daemon's in-memory registry union has lost track of a sweep
    // that is demonstrably still alive (the lock proves it) — a monitor should
    // read this as ALIVE, not dead, and an operator should reconcile rather
    // than re-dispatch (re-dispatch is blocked by the lock anyway, #4146).
    if !report.unregistered_locked.is_empty() {
        println!(
            "\nWARNING: {} live-locked sweep(s) missing from the in-flight registry \
             (alive, not dead — reconcile, don't re-dispatch):",
            report.unregistered_locked.len()
        );
        for u in &report.unregistered_locked {
            println!("  issue #{} (pid {}) in {}", u.issue, u.owner_pid, u.root.display());
        }
    }

    // Claude-wrapper pre-flight-death tripwire (#4386): printed prominently,
    // ahead of the capacity section, so a fleet-wide spawn failure is visible
    // even to an operator skimming just the top of `status`. Printed FIRST so
    // the "not capacity-bound … the limiter is work availability" line further
    // below is never the only diagnosis shown while this is tripped — see the
    // guard on that line.
    if report.preflight_advisory_active {
        if let Some(msg) = &report.preflight_advisory_message {
            println!("\n{msg}");
        }
    }

    // Capacity figures resolved from a single source (fresh probe when
    // available, else the daemon's ranking snapshot) so the cap's healthy-tokens
    // input, the Token-capacity summary, and the Per-token table all agree (#3936).
    let rc = resolve_capacity(report, token_usage);

    let factor = report.per_token_concurrency.max(1);
    // #4344: `rc` prefers a fresh client-side probe when one succeeded, which
    // can legitimately show a *different* (usually fresher) number than what
    // the running daemon actually used for its own dispatch decision this
    // tick. `report.dynamic_cap` / `report.capacity.token_axis_limit` are that
    // daemon-side truth — the number dispatch decisions are actually gated
    // on — so the headline always names the daemon's own cap; the probe's
    // number is shown as a labeled secondary line only when it disagrees.
    let dispatch_cap = report.dynamic_cap;
    let dispatch_token_axis = report.capacity.token_axis_limit;
    println!("\nDynamic concurrency cap: {dispatch_cap}  (the number dispatch uses)");
    println!(
        "  = min(healthy {} × per-token {} = {}, disk headroom {}, cpu headroom {}, \
         configured max {})",
        dispatch_token_axis,
        factor,
        dispatch_token_axis.saturating_mul(factor),
        report.disk_headroom,
        report.cpu_headroom,
        report.configured_max
    );
    if rc.source == "probe" && rc.effective_cap != dispatch_cap {
        println!(
            "  fresh probe suggests: {} (healthy {} × per-token {} = {}) — not yet reflected in \
             dispatch; if this persists, refresh with `loom-tokens check --ranking`.",
            rc.effective_cap,
            rc.token_axis_limit,
            factor,
            rc.token_axis_limit.saturating_mul(factor)
        );
    }
    // CPU headroom detail (#3978 AC4; measured-idle signal #4031). The signal
    // chain is measured idle → loadavg → static capacity, so the line names
    // which signal actually fed the term. `logical_cpus == 0` means an older
    // daemon (pre-#3978) never sent these fields — nothing to show.
    if report.logical_cpus > 0 {
        let basis = match (report.cpu_idle_fraction, report.loadavg_1m) {
            // Preferred: measured CPU consumption (#4031). Show consumed cores so
            // the operator can see the term is tracking real usage, not loadavg.
            (Some(idle), _) => {
                let consumed = report.logical_cpus as f64 * (1.0 - idle.clamp(0.0, 1.0));
                format!(
                    "{} logical cores, {:.0}% idle measured (≈{:.1} cores consumed)",
                    report.logical_cpus,
                    idle * 100.0,
                    consumed
                )
            }
            // Fallback: 1-minute load average (#3978 behavior) until an idle
            // sample exists (e.g. the first Linux cross-tick delta not yet taken).
            (None, Some(load)) => format!(
                "{} logical cores, 1m loadavg {load:.2} (no idle sample yet — loadavg fallback)",
                report.logical_cpus
            ),
            // Static capacity only: no CPU signal at all on this platform.
            (None, None) => format!(
                "{} logical cores, no CPU signal on this platform — static capacity only",
                report.logical_cpus
            ),
        };
        println!("  cpu headroom: {} concurrent-sweep slot(s) ({basis})", report.cpu_headroom);
    }

    // Token-capacity backpressure section (#3902, source-unified in #3936).
    println!("\nToken capacity:");
    // Name the resolved pool directory (#4292) — the same one the per-token
    // usage table below was probed against — so a mismatch between "where I
    // ran this command from" and "where the daemon's pool actually lives" is
    // visible instead of silent. `None` only from a pre-#4292 daemon binary.
    match &report.token_pool_dir {
        Some(dir) => println!("  pool: {}", dir.display()),
        None => println!("  pool: (unknown — daemon predates #4292)"),
    }
    // "Currently binding" vs "smallest ceiling" (#4031): the dynamic cap is the
    // minimum of several ceilings, but a ceiling only *binds* once in-flight
    // occupancy reaches it. Below the cap the limiter is work availability, not
    // any resource term — so the token-bound diagnosis below is gated on this.
    // #4344: this must be checked against the daemon's *actual* dispatch cap
    // (`dispatch_cap`), not `rc.effective_cap` — the latter is recomputed from
    // a fresh client-side probe when one succeeds and can disagree with what
    // the daemon itself used, which previously let "not capacity-bound" print
    // even while the daemon's real (lower) cap was already saturated.
    let capacity_bound = report.in_flight.len() >= dispatch_cap;
    // #4344: the daemon's own dispatch decision reads 0 healthy accounts while
    // a fresher probe (or the raw pool) shows real capacity — the exact
    // wedge this issue exists for. When this holds, promote the diagnosis to
    // the headline and suppress the misleading "limiter is work availability"
    // line below (the limiter is unmistakably the token term: `0 × per-token
    // = 0`).
    let dispatch_starved_but_disagrees = report.capacity.ranking_present
        && report.capacity.healthy_accounts == 0
        && rc.ranking_present
        && rc.healthy > 0;
    if rc.ranking_present {
        let src = if rc.source == "probe" {
            "live probe: loom-tokens check --json"
        } else {
            "from .loom/tokens/.ranking"
        };
        println!(
            "  {}/{} accounts healthy, {} exhausted/near-ceiling ({src})",
            rc.healthy, rc.total, rc.exhausted
        );
        if dispatch_starved_but_disagrees {
            // Headline promotion (#4344, was a small-print "note" pre-fix):
            // the daemon's own dispatch decision is starved at 0 healthy
            // accounts while the number above disagrees — dispatch will not
            // resume until the ranking the daemon actually reads is fresh.
            let pool_display = report
                .token_pool_dir
                .as_ref()
                .map_or_else(|| "(unknown pool dir)".to_string(), |d| d.display().to_string());
            println!(
                "  \u{26a0} DISPATCH IS TOKEN-STARVED: the daemon's own ranking read shows \
                 0/{} healthy (dispatch cap {dispatch_cap}), disagreeing with the {} healthy \
                 shown above from {pool_display}. The token term is the limiter — refresh the \
                 ranking with `loom-tokens check --ranking` (or wait for the next self-refresh).",
                report.capacity.total_accounts, rc.healthy,
            );
        } else if rc.source == "probe"
            && report.capacity.ranking_present
            && report.capacity.healthy_accounts != rc.healthy
        {
            // Non-zero disagreement (the daemon still has *some* healthy
            // accounts, just a different count than the fresh probe) stays a
            // small-print note — dispatch is not silently starved here, just
            // running on slightly stale data.
            println!(
                "  note: daemon dispatch cap still uses a stale .ranking ({} healthy); \
                 refresh it with `loom-tokens check --ranking`.",
                report.capacity.healthy_accounts
            );
        }
        // #4344: when the daemon's own dispatch decision is unambiguously
        // token-starved (see above), never print "the limiter is work
        // availability" — the headline diagnosis already named the real
        // limiter, and running the generic capacity_bound/token_bound chain
        // underneath it would contradict it (e.g. `capacity_bound` is
        // trivially true against a dispatch cap of 0).
        if !dispatch_starved_but_disagrees {
            if !capacity_bound {
                // In-flight is below the cap: nothing is binding. Naming tokens
                // (or any resource) as "the bottleneck" here is the #4031
                // defect — at, say, 1 in-flight against a cap of 7 the limiter
                // is simply how much ready work exists. Suppress the
                // token-bound diagnosis.
                //
                // #4386: while the pre-flight tripwire is active, this bare
                // "work availability" line is actively misleading — every
                // dispatch IS starting, it just dies within ~1s at
                // claude-wrapper pre-flight, which reads as "no work" rather
                // than "everything is crashing." The warning printed above
                // already names the real cause, so suppress this line rather
                // than let it stand uncontested.
                if !report.preflight_advisory_active {
                    println!(
                        "  not capacity-bound ({} in flight, cap {dispatch_cap} — the limiter is \
                         work availability, not tokens/disk/CPU)",
                        report.in_flight.len(),
                    );
                }
            } else if rc.token_bound {
                if rc.healthy == 0 {
                    println!(
                        "  token-bound: NO healthy accounts — new dispatch deferred until \
                         capacity returns. Add accounts (~/.claude-monitor/accounts.env + \
                         `loom-tokens bootstrap`) or buy API credits, then `loom-tokens check \
                         --ranking`."
                    );
                } else {
                    println!(
                        "  token-bound: tokens are the binding constraint on throughput. Add \
                         accounts or API credits to dispatch more concurrently."
                    );
                }
            } else {
                println!("  not token-bound (tokens are not the current bottleneck)");
            }
        }
    } else {
        println!(
            "  (no ranking — run `loom-tokens check --ranking`; token pool size {} used as the \
             health basis)",
            report.token_pool_size
        );
        if !capacity_bound && !report.preflight_advisory_active {
            // #4386: same suppression as the ranking-present branch above —
            // the warning printed at the top of `status` already names the
            // real cause while the tripwire is active.
            println!(
                "  not capacity-bound ({} in flight, cap {dispatch_cap} — the limiter is work \
                 availability, not tokens/disk/CPU)",
                report.in_flight.len(),
            );
        }
    }

    // "Halted" (a completed gate run found main verified-red) and "not
    // evaluated" (the gate could not run this tick) are distinct states that can
    // co-occur (#3950 AC3): a prior halt persists untouched while an
    // environmental failure blocks the *next* evaluation. The not-evaluated
    // cause is reported verbatim from the gate (#3974 AC2) — pre-#3974 this
    // line hard-coded "workspace tree is dirty" for every skip, which
    // misreported timeouts / missing tools / broken `git` as a dirty tree.
    let verdict = classify_gate_verdict(
        report.main_health_gate_enabled,
        report.main_health_gate_halted,
        report.main_health_gate_not_evaluated,
        report.main_health_gate_deferred,
        report.main_health_gate_not_evaluated_reason.as_deref(),
        report.main_health_gate_deferred_reason.as_deref(),
        report.main_health_gate_verdict_tier.as_deref(),
        report.main_health_gate_verdict_at,
    );
    let gate = format_gate_status(&verdict);
    println!("\nMain-health gate: {gate}");

    // Startup forge-credential preflight (#4005) — resolved once at daemon
    // boot, before the daemon's first `gh` consumer, so a headless/SSH-only
    // start with no usable credential is visible here rather than only as
    // silent per-tick 401s in the logs. `None` only from a pre-#4005 daemon
    // binary that never computed one.
    match &report.credential_preflight {
        Some(c) if c.ok => {
            println!(
                "Forge credential: OK — {} ({})",
                c.mechanism,
                c.fingerprint.as_deref().unwrap_or("no fingerprint")
            );
        }
        Some(c) => println!("Forge credential: DEGRADED — {}", c.message),
        None => {
            println!("Forge credential: unknown (older daemon binary — restart to pick up #4005)")
        }
    }

    // Live safehouse fleet-comms connection state (#4345): before this,
    // "not configured", "configured but unreachable", and "connected" all
    // looked identical — silence. See `.loom/docs/safehouse.md`.
    match &report.safehouse {
        Some(s) if s.state == "connected" => {
            println!(
                "Safehouse:     connected (room: {}, socket: {})",
                s.room.as_deref().unwrap_or("(default — sole joined room)"),
                s.socket
                    .as_ref()
                    .map_or_else(|| "?".to_string(), |p| p.display().to_string())
            );
        }
        Some(s) if s.state == "unreachable" => {
            println!(
                "Safehouse:     configured, unreachable (socket: {})",
                s.socket
                    .as_ref()
                    .map_or_else(|| "unresolved".to_string(), |p| p.display().to_string())
            );
        }
        Some(_) => println!("Safehouse:     not configured"),
        None => {
            println!("Safehouse:     unknown (older daemon binary — restart to pick up #4345)")
        }
    }

    // Scheduled drain-and-restart (#4090): a drain that quietly hangs is worse
    // than no drain, so surface DRAINING with the remaining count + deadline.
    // A `drain_note` (timeout refusal / abort) persists after a drain ends so
    // the operator sees WHY the daemon is still up rather than restarted.
    if report.draining {
        let deadline = report.drain_deadline.map_or_else(
            || "no deadline".to_string(),
            |d| {
                let secs = (d - Utc::now()).num_seconds();
                if secs >= 0 {
                    format!("deadline in {secs}s ({d})")
                } else {
                    format!("deadline passed {}s ago ({d})", -secs)
                }
            },
        );
        println!("Drain: DRAINING ({} sweep(s) remaining, {deadline})", report.in_flight.len());
    } else if let Some(note) = &report.drain_note {
        println!("Drain: not draining (last: {note})");
    }

    // Host-distress circuit breaker (#4235): surface the phase, why it tripped,
    // and when the cool-down releases so an operator sees a paused-dispatch host
    // and can tell it apart from a main-health halt or a drain. A Closed breaker
    // prints a one-line "OK" with its configured thresholds; an absent breaker
    // (work-finder off / disabled) prints nothing.
    if let Some(hb) = &report.host_breaker {
        let load = hb
            .last_load_per_core
            .map_or_else(|| "n/a".to_string(), |l| format!("{l:.2}"));
        match hb.phase.as_str() {
            "closed" => {
                if hb.enabled {
                    println!(
                        "Host breaker: OK (closed; load/core {load}, trip ≥ {:.2} for {} tick(s), cooldown {}s)",
                        hb.load_per_core_threshold, hb.sustain_ticks, hb.cooldown_secs
                    );
                } else {
                    println!("Host breaker: disabled");
                }
            }
            "open" => {
                println!(
                    "Host breaker: OPEN — new dispatch paused, running work draining ({})",
                    hb.reason.as_deref().unwrap_or("sustained host distress")
                );
                if let Some(t) = hb.tripped_at {
                    println!("  tripped at: {t}");
                }
            }
            "cooldown" => {
                let releases = hb.releases_at.map_or_else(
                    || "unknown".to_string(),
                    |r| {
                        let secs = (r - Utc::now()).num_seconds();
                        if secs >= 0 {
                            format!("in {secs}s ({r})")
                        } else {
                            format!("overdue by {}s ({r})", -secs)
                        }
                    },
                );
                println!(
                    "Host breaker: COOLING DOWN — dispatch paused, releases {releases} (load/core {load})"
                );
            }
            other => println!("Host breaker: {other}"),
        }
    }

    // GitHub rate-limit circuit breaker (#4429): one line while Closed, a
    // fuller block while cooling (the operator's first question is "when does
    // polling resume").
    if let Some(rl) = &report.rate_limit_breaker {
        if !rl.enabled {
            println!("GitHub rate limit: breaker disabled");
        } else if rl.suppressed {
            let releases = rl.cooldown_until.map_or_else(
                || "unknown".to_string(),
                |r| {
                    let secs = (r - Utc::now()).num_seconds();
                    if secs >= 0 {
                        format!("in {secs}s ({r})")
                    } else {
                        format!("overdue by {}s ({r})", -secs)
                    }
                },
            );
            let source = rl.source.as_deref().unwrap_or("unknown");
            println!(
                "GitHub rate limit: COOLDOWN — forge polling paused (tripped by {source}), \
                 resumes {releases}"
            );
            if let (Some(core), Some(gql)) = (rl.core_remaining, rl.graphql_remaining) {
                println!("  last probed budget: core {core} remaining, graphql {gql} remaining");
            }
        } else if rl.trips_total > 0 {
            println!(
                "GitHub rate limit: OK (breaker closed; {} trip(s) this daemon lifetime)",
                rl.trips_total
            );
        } else {
            println!("GitHub rate limit: OK (breaker closed)");
        }
    }

    // Per-repo breakdown across every registered managed workspace (#3930). In
    // the common single-workspace case this is one line for the daemon's own
    // workspace; with `loom-daemon workspace add <path>` it lists every managed
    // repo, its in-flight count, and its own gate state.
    println!(
        "\nManaged repos: {} (priority: lower = higher dispatch priority)",
        report.per_repo.len()
    );
    if report.per_repo.is_empty() {
        println!("  (none)");
    } else {
        println!("  {:>4}  {:>9}  {:<13}  {:<5}  REPO", "PRIO", "IN-FLIGHT", "GATE", "ROLES");
        println!("  {:-<68}", "");
        for r in &report.per_repo {
            // Same classification as the top-level summary above, condensed
            // for the table column (#3950 AC3, widened #4012).
            let verdict = classify_gate_verdict(
                r.health_gate_enabled,
                r.health_gate_halted,
                r.health_gate_not_evaluated,
                r.health_gate_deferred,
                r.health_gate_not_evaluated_reason.as_deref(),
                r.health_gate_deferred_reason.as_deref(),
                r.health_gate_verdict_tier.as_deref(),
                r.health_gate_verdict_at,
            );
            let gate = gate_status_short_label(&verdict);
            // Per-root role-runner enablement (#4377) — resolved from this
            // root's OWN config, so it can legitimately read "off" even while
            // the daemon's own workspace has the loops running.
            let roles = if r.role_runner_enabled { "on" } else { "off" };
            println!(
                "  {:>4}  {:>9}  {:<13}  {:<5}  {}{}",
                r.priority,
                r.in_flight_count,
                gate,
                roles,
                r.root.display(),
                if r.root_missing {
                    "  [MISSING ROOT]"
                } else {
                    ""
                }
            );
            // Issue #4326: a dangling registry entry (root deleted without
            // `workspace remove`) — the work-finder already warns-and-skips
            // it on dispatch; this is the operator-facing pointer to clean it
            // up (or, if the root is only transiently unavailable, e.g. an
            // unmounted volume, to leave it registered).
            if r.root_missing {
                println!(
                    "        root does not exist on disk — dispatch is skipped; \
                     run `loom-daemon workspace remove {}` if this is permanent",
                    r.root.display()
                );
            }
            // Name the failure class behind a not-evaluated repo (#3974 AC2) so
            // the operator can tell "dirty tree" from "cargo not on PATH".
            if let Some(reason) = &r.health_gate_not_evaluated_reason {
                println!("        gate not evaluated — {reason}");
            }
            // Load-aware deferral (#4259): name why the gate is deferring so a
            // repo whose gate is not producing verdicts under host load is
            // explained (distinct from the not-evaluated line above).
            if let Some(reason) = &r.health_gate_deferred_reason {
                println!("        gate deferred — {reason}");
            }
            // Insta-crash quarantine (#3939): list the issues this repo is
            // currently refusing to re-dispatch so a stalled-but-nonempty backlog
            // is explained. Auto-releases on a TTL (or `loom:blocked` removal).
            if !r.quarantined_issues.is_empty() {
                let list = r
                    .quarantined_issues
                    .iter()
                    .map(|n| format!("#{n}"))
                    .collect::<Vec<_>>()
                    .join(", ");
                println!("        quarantined (insta-crash, #3939): {list}");
            }
            // #4377: onIdle configured but the per-root gate is off is
            // exactly the silent no-op this issue fixes — call it out
            // explicitly rather than requiring the operator to cross-check
            // the ROLES column against a separate onIdle listing.
            if !r.role_runner_enabled && !r.role_runner_on_idle_roles.is_empty() {
                let list = r.role_runner_on_idle_roles.join(", ");
                println!(
                    "        role runner disabled for this root but onIdle=[{list}] is \
                     configured — these roles will never fire until \
                     autonomous.roleRunner.enabled=true is set in this root's own \
                     .loom/config.json (#4377)"
                );
            }
        }
    }

    // Forge-side pipeline snapshot (#3977) — opt-in via `--pipeline`. Rendered
    // in the same order as the "Managed repos" table above (both iterate
    // `report.per_repo`), so the two tables line up row-for-row.
    if let Some(snapshots) = pipeline {
        println!("\nForge pipeline (per repo, --pipeline):");
        if snapshots.is_empty() {
            println!("  (none)");
        } else {
            println!(
                "  {:>6}  {:>8}  {:>6}  {:>7}  {:>5}  {:>9}  REPO",
                "QUEUED", "BUILDING", "REVIEW", "CHNG-RQ", "PR", "MERGED24H"
            );
            println!("  {:-<75}", "");
            for s in snapshots {
                use loom_daemon::pipeline_snapshot::format_count;
                println!(
                    "  {:>6}  {:>8}  {:>6}  {:>7}  {:>5}  {:>9}  {}",
                    format_count(s.queued),
                    format_count(s.building),
                    format_count(s.review_requested),
                    format_count(s.changes_requested),
                    format_count(s.approved),
                    format_count(s.merged_24h),
                    s.root.display()
                );
                if let Some(err) = &s.error {
                    println!("        forge query failed for one or more metrics ({err}) — unreachable fields shown as ?");
                }
            }
        }
    }

    println!("\nPer-token usage:");
    match token_usage {
        Some(value) => print_token_usage_table(value),
        None => println!(
            "  (unavailable — `loom-tokens check --json` failed or the token pool is not bootstrapped)"
        ),
    }

    // Self-update staleness (#3968) — read-only, local-only. Never implies an
    // auto-restart; run `.loom/scripts/cli/loom-daemon-update.sh` to act on it.
    print!("\nSelf-update: built from {}", update.built_commit);
    match (update.source_commit.as_deref(), update.update_available) {
        (Some(source), Some(true)) => println!(
            " — UPDATE AVAILABLE (source checkout HEAD is {source}); run \
             `./.loom/scripts/cli/loom-daemon-update.sh` to rebuild + provision + restart"
        ),
        (Some(source), Some(false)) => println!(" — up to date with source HEAD ({source})"),
        _ => println!(" (source checkout not found on this machine; staleness unknown)"),
    }

    // Autonomous self-update loop (#4055) — the daemon-side loop that acts on the
    // staleness above. Only rendered when enabled (opt-in); otherwise silent.
    if report.auto_update_enabled {
        print!("Auto-update loop: enabled");
        match &report.auto_update_last_check {
            Some(ts) => print!(" (last check {})", ts.format("%Y-%m-%dT%H:%M:%SZ")),
            None => print!(" (no check yet)"),
        }
        if let Some(ts) = &report.auto_update_last_roll {
            print!(", last roll {}", ts.format("%Y-%m-%dT%H:%M:%SZ"));
        }
        if let Some(reason) = &report.auto_update_terminal_reason {
            print!(" — TERMINAL: {reason}");
        } else if let Some(secs) = report.auto_update_backoff_secs {
            print!(
                " — backing off {secs}s after {} consecutive failure(s)",
                report.auto_update_consecutive_failures
            );
        }
        println!();
        if let Some(note) = &report.auto_update_note {
            println!("  last tick: {note}");
        }
    }

    println!();
}

/// Render the `loom-tokens check --json` report (`{ "accounts": [ { name,
/// status, 5h_utilization, 7d_utilization, 7d_reset } ] }`) as a small table.
/// Falls back to pretty-printed JSON if the shape is unexpected.
fn print_token_usage_table(value: &serde_json::Value) {
    let Some(accounts) = value.get("accounts").and_then(serde_json::Value::as_array) else {
        // Unexpected shape — surface the raw JSON rather than dropping it.
        if let Ok(pretty) = serde_json::to_string_pretty(value) {
            for line in pretty.lines() {
                println!("  {line}");
            }
        }
        return;
    };

    if accounts.is_empty() {
        println!("  (no accounts probed)");
        return;
    }

    println!("  {:<22} {:<14} {:>8} {:>8}", "ACCOUNT", "STATUS", "5h", "7d");
    println!("  {:-<54}", "");
    for acct in accounts {
        let name = acct
            .get("name")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("?");
        let raw_status = acct
            .get("status")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("?");
        let util_7d = acct
            .get("7d_utilization")
            .and_then(serde_json::Value::as_f64);
        // Apply the same near-ceiling override the summary uses so a 99%-7d
        // `available` row never renders `available` here (#3936).
        let status = loom_daemon::capacity::effective_probe_status(raw_status, util_7d);
        let fmt_pct = |key: &str| {
            acct.get(key)
                .and_then(serde_json::Value::as_f64)
                .map_or_else(|| "-".to_string(), |u| format!("{:.0}%", u * 100.0))
        };
        println!(
            "  {:<22} {:<14} {:>8} {:>8}",
            name,
            status,
            fmt_pct("5h_utilization"),
            fmt_pct("7d_utilization")
        );
    }
}

/// Handle the stats subcommand - display agent effectiveness and activity metrics.
#[allow(clippy::too_many_lines)]
fn handle_stats_command(
    role: Option<&str>,
    issue: Option<i32>,
    weekly: bool,
    format: &str,
) -> Result<()> {
    let loom_dir = dirs::home_dir()
        .ok_or_else(|| anyhow!("No home directory"))?
        .join(".loom");

    let db_path = loom_dir.join("activity.db");

    if !db_path.exists() {
        eprintln!("No activity database found at {}", db_path.display());
        eprintln!("Run the Loom daemon first to start collecting metrics.");
        return Ok(());
    }

    let db = ActivityDb::new(db_path)?;

    let is_json = format == "json";

    if let Some(issue_num) = issue {
        let costs = db.get_cost_per_issue(Some(issue_num))?;

        if is_json {
            println!("{}", serde_json::to_string_pretty(&costs)?);
        } else {
            println!("\n=== Cost Breakdown for Issue #{issue_num} ===\n");
            if costs.is_empty() {
                println!("No data found for issue #{issue_num}");
            } else {
                for cost in &costs {
                    println!("Issue #{}:", cost.issue_number);
                    println!("  Prompts:      {}", cost.prompt_count);
                    println!("  Total Cost:   ${:.4}", cost.total_cost);
                    println!("  Total Tokens: {}", cost.total_tokens);
                    if let Some(started) = &cost.started {
                        println!("  Started:      {}", started.format("%Y-%m-%d %H:%M"));
                    }
                    if let Some(completed) = &cost.completed {
                        println!("  Completed:    {}", completed.format("%Y-%m-%d %H:%M"));
                    }
                    println!();
                }
            }
        }
        return Ok(());
    }

    if let Some(role_filter) = role {
        let effectiveness = db.get_agent_effectiveness(Some(role_filter))?;

        if is_json {
            println!("{}", serde_json::to_string_pretty(&effectiveness)?);
        } else {
            println!("\n=== Agent Effectiveness: {role_filter} ===\n");
            if effectiveness.is_empty() {
                println!("No data found for role '{role_filter}'");
            } else {
                for agent in &effectiveness {
                    print_agent_effectiveness(agent);
                }
            }
        }
        return Ok(());
    }

    if weekly {
        let velocity = db.get_weekly_velocity()?;

        if is_json {
            println!("{}", serde_json::to_string_pretty(&velocity)?);
        } else {
            println!("\n=== Weekly Velocity ===\n");
            if velocity.is_empty() {
                println!("No weekly data available.");
            } else {
                println!("{:<12} {:>10} {:>12}", "Week", "Prompts", "Cost (USD)");
                println!("{:-<36}", "");
                for week in &velocity {
                    println!("{:<12} {:>10} {:>12.4}", week.week, week.prompts, week.cost);
                }
            }
        }
        return Ok(());
    }

    let summary = db.get_stats_summary()?;
    let effectiveness = db.get_agent_effectiveness(None)?;

    if is_json {
        #[derive(serde::Serialize)]
        struct FullStats {
            summary: activity::StatsSummary,
            effectiveness: Vec<activity::AgentEffectiveness>,
        }
        let full = FullStats {
            summary,
            effectiveness,
        };
        println!("{}", serde_json::to_string_pretty(&full)?);
    } else {
        println!("\n=== Loom Activity Summary ===\n");
        println!("Total Prompts:   {}", summary.total_prompts);
        println!("Total Cost:      ${:.4}", summary.total_cost);
        println!("Total Tokens:    {}", summary.total_tokens);
        println!("Issues Worked:   {}", summary.issues_count);
        println!("PRs Created:     {}", summary.prs_count);
        println!("Avg Success:     {:.1}%", summary.avg_success_rate);

        if !effectiveness.is_empty() {
            println!("\n=== Agent Effectiveness by Role ===\n");
            println!(
                "{:<12} {:>10} {:>10} {:>12} {:>12} {:>12}",
                "Role", "Prompts", "Success", "Rate", "Avg Cost", "Avg Time"
            );
            println!("{:-<70}", "");
            for agent in &effectiveness {
                println!(
                    "{:<12} {:>10} {:>10} {:>11.1}% {:>11.4} {:>10.1}s",
                    agent.agent_role,
                    agent.total_prompts,
                    agent.successful_prompts,
                    agent.success_rate,
                    agent.avg_cost,
                    agent.avg_duration_sec
                );
            }
        }

        let top_issues = db.get_cost_per_issue(None)?;
        if !top_issues.is_empty() {
            println!("\n=== Top 5 Most Expensive Issues ===\n");
            println!("{:<8} {:>10} {:>12} {:>12}", "Issue", "Prompts", "Cost (USD)", "Tokens");
            println!("{:-<44}", "");
            for cost in top_issues.iter().take(5) {
                println!(
                    "#{:<7} {:>10} {:>12.4} {:>12}",
                    cost.issue_number, cost.prompt_count, cost.total_cost, cost.total_tokens
                );
            }
        }

        println!();
    }

    Ok(())
}

fn print_agent_effectiveness(agent: &activity::AgentEffectiveness) {
    println!("Role: {}", agent.agent_role);
    println!("  Total Prompts:      {}", agent.total_prompts);
    println!("  Successful Prompts: {}", agent.successful_prompts);
    println!("  Success Rate:       {:.1}%", agent.success_rate);
    println!("  Average Cost:       ${:.4}", agent.avg_cost);
    println!("  Average Duration:   {:.1}s", agent.avg_duration_sec);
    println!();
}

/// Valid `agent-metrics` subcommands (mirrors `loom_tools.agent_metrics`'s
/// argparse `choices`).
const AGENT_METRICS_COMMANDS: &[&str] = &["summary", "effectiveness", "costs", "velocity"];
/// Valid `--period` values.
const AGENT_METRICS_PERIODS: &[&str] = &["today", "week", "month", "all"];
/// Valid `--role` values (mirrors `loom_tools.agent_metrics._VALID_ROLES`).
const AGENT_METRICS_ROLES: &[&str] = &[
    "builder",
    "judge",
    "curator",
    "architect",
    "hermit",
    "doctor",
    "guide",
    "champion",
    "shepherd",
];

/// Handle `loom-daemon stats <command>` — the native port of
/// `loom_tools.agent_metrics` (epic #4081 Phase 3 family 4, issue #4274).
/// `command` is one of `summary`/`effectiveness`/`costs`/`velocity`, the CLI
/// contract `agent-metrics.sh` forwards to (and, transitively,
/// `mcp__loom__get_agent_metrics`). Exits nonzero on validation failure or a
/// missing activity database, matching the retired Python CLI's exit codes.
#[allow(clippy::too_many_lines)]
fn handle_agent_metrics_command(
    command: &str,
    role: Option<&str>,
    issue: Option<i32>,
    period: &str,
    by_model: bool,
    format: &str,
) -> Result<()> {
    if !AGENT_METRICS_COMMANDS.contains(&command) {
        eprintln!(
            "Invalid command: {command} (expected one of: {})",
            AGENT_METRICS_COMMANDS.join(", ")
        );
        std::process::exit(1);
    }

    if let Some(r) = role {
        if !AGENT_METRICS_ROLES.contains(&r) {
            eprintln!("Invalid role: {r}");
            std::process::exit(1);
        }
    }

    if !AGENT_METRICS_PERIODS.contains(&period) {
        eprintln!(
            "Invalid period: {period} (expected one of: {})",
            AGENT_METRICS_PERIODS.join(", ")
        );
        std::process::exit(1);
    }

    let is_json = format == "json";

    let db_path = std::env::var_os("LOOM_ACTIVITY_DB")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            dirs::home_dir()
                .unwrap_or_else(|| std::path::PathBuf::from("."))
                .join(".loom")
                .join("activity.db")
        });

    if !db_path.is_file() {
        if is_json {
            println!(
                "{}",
                serde_json::json!({
                    "error": "Activity database not available",
                    "db_path": db_path.display().to_string(),
                })
            );
        } else {
            println!(
                "Error: activity database not found at {}. Set LOOM_ACTIVITY_DB or enable agent activity tracking.",
                db_path.display()
            );
        }
        std::process::exit(1);
    }

    let db = ActivityDb::new(db_path)?;

    match command {
        "summary" => {
            let metrics = db.get_summary_metrics(role, period)?;
            if is_json {
                println!("{}", serde_json::to_string_pretty(&metrics)?);
            } else {
                let tokens_k = metrics.total_tokens / 1000;
                println!("\nAgent Performance Summary ({period})");
                println!("{:-<40}", "");
                println!("  Total Prompts:   {}", metrics.total_prompts);
                println!("  Total Tokens:    {tokens_k}K");
                println!("  Total Cost:      ${:.4}", metrics.total_cost);
                println!("  Issues Worked:   {}", metrics.issues_count);
                println!("  PRs Created:     {}", metrics.prs_count);
                println!("  Success Rate:    {:.1}%", metrics.success_rate);
                println!();
            }
        }
        "effectiveness" => {
            let rows = db.get_effectiveness_rows(role, period, by_model)?;
            if is_json {
                println!("{}", serde_json::to_string_pretty(&rows)?);
            } else {
                println!("\nAgent Effectiveness by Role ({period})");
                if by_model {
                    println!(
                        "{:<12} {:<22} {:>10} {:>10} {:>10} {:>10} {:>10}",
                        "Role", "Model", "Prompts", "Success", "Rate", "Avg Cost", "Avg Time"
                    );
                } else {
                    println!(
                        "{:<12} {:>10} {:>10} {:>10} {:>10} {:>10}",
                        "Role", "Prompts", "Success", "Rate", "Avg Cost", "Avg Time"
                    );
                }
                for r in &rows {
                    let rate_str = format!("{:.1}%", r.success_rate);
                    let cost_str = format!("${:.4}", r.avg_cost);
                    let time_str = format!("{:.1}s", r.avg_duration_sec);
                    if by_model {
                        println!(
                            "{:<12} {:<22} {:>10} {:>10} {:>10} {:>10} {:>10}",
                            r.role,
                            r.model.as_deref().unwrap_or("default"),
                            r.total_prompts,
                            r.successful_prompts,
                            rate_str,
                            cost_str,
                            time_str
                        );
                    } else {
                        println!(
                            "{:<12} {:>10} {:>10} {:>10} {:>10} {:>10}",
                            r.role,
                            r.total_prompts,
                            r.successful_prompts,
                            rate_str,
                            cost_str,
                            time_str
                        );
                    }
                }
                if rows.is_empty() {
                    println!("No data found");
                }
                println!();
            }
        }
        "costs" => {
            let rows = db.get_cost_rows(issue, by_model)?;
            if is_json {
                println!("{}", serde_json::to_string_pretty(&rows)?);
            } else {
                println!("\nCost Breakdown by Issue");
                if by_model {
                    println!(
                        "{:<8} {:<22} {:>10} {:>12} {:>12}",
                        "Issue", "Model", "Prompts", "Cost", "Tokens"
                    );
                } else {
                    println!("{:<8} {:>10} {:>12} {:>12}", "Issue", "Prompts", "Cost", "Tokens");
                }
                for r in &rows {
                    let cost_str = format!("${:.4}", r.total_cost);
                    if by_model {
                        println!(
                            "#{:<7} {:<22} {:>10} {:>12} {:>12}",
                            r.issue_number,
                            r.model.as_deref().unwrap_or("default"),
                            r.prompt_count,
                            cost_str,
                            r.total_tokens
                        );
                    } else {
                        println!(
                            "#{:<7} {:>10} {:>12} {:>12}",
                            r.issue_number, r.prompt_count, cost_str, r.total_tokens
                        );
                    }
                }
                if rows.is_empty() {
                    println!("No data found");
                }
                println!();
            }
        }
        "velocity" => {
            let rows = db.get_velocity_rows()?;
            if is_json {
                println!("{}", serde_json::to_string_pretty(&rows)?);
            } else {
                println!("\nDevelopment Velocity (Last 8 Weeks)");
                println!(
                    "{:<10} {:>10} {:>10} {:>10} {:>10}",
                    "Week", "Prompts", "Issues", "PRs", "Cost"
                );
                for r in &rows {
                    println!(
                        "{:<10} {:>10} {:>10} {:>10} {:>10}",
                        r.week,
                        r.prompts,
                        r.issues,
                        r.prs_merged,
                        format!("${:.2}", r.cost)
                    );
                }
                println!();
            }
        }
        _ => unreachable!("validated above"),
    }

    Ok(())
}

/// Handle the `workspace` subcommand — mutate/inspect the machine-level
/// workspace registry (`~/.loom/workspaces.json`) directly on the filesystem.
/// This runs whether or not the daemon is up; a running daemon re-reads the
/// same file on its next tick (hot-apply), and its `RegisterWorkspace` /
/// `DeregisterWorkspace` / `ListWorkspaces` IPC handlers touch the same file.
/// Handle `loom-daemon fleet …` (epic #4340). Thin clap→module wiring: all
/// bootstrap logic (the step planner/executor, shell templates, fleet registry)
/// lives in [`loom_daemon::fleet`].
fn handle_fleet_command(action: FleetAction) -> Result<()> {
    use loom_daemon::fleet::add_worker::{self, AddWorkerConfig};
    use loom_daemon::fleet::drain::{self, DrainConfig};

    match action {
        FleetAction::AddWorker {
            ssh_host,
            repo,
            priority,
            pat_file,
            accounts_env,
            loom_repo,
            safehouse,
            safehouse_tailnet_auth_key_file,
            safehouse_secrets_file,
            safehouse_repo_url,
            safehouse_homeserver_url,
            safehouse_room,
            safehouse_personas,
            safehouse_invite_exec,
            idle_shutdown_minutes,
            dry_run,
        } => {
            let config = AddWorkerConfig {
                ssh_host,
                repos: repo,
                priority,
                dry_run,
                loom_repo_url: loom_repo,
                pat_file: pat_file.map(PathBuf::from),
                accounts_env_file: accounts_env.map(PathBuf::from),
                safehouse_enabled: safehouse,
                idle_shutdown_minutes,
                safehouse_tailnet_auth_key_file: safehouse_tailnet_auth_key_file.map(PathBuf::from),
                safehouse_secrets_file: safehouse_secrets_file.map(PathBuf::from),
                safehouse_repo_url,
                safehouse_homeserver_url,
                safehouse_room,
                safehouse_personas,
                safehouse_invite_exec,
            };
            add_worker::run(&config)
        }
        FleetAction::Status { .. } => {
            // Routed directly in `main()` (it needs the async runtime for the
            // local host's in-process socket round-trip), never dispatched
            // through this sync handler.
            unreachable!("Fleet Status is handled in main() before handle_cli_command")
        }
        FleetAction::Drain {
            ssh_host,
            timeout,
            force_after_timeout,
            json,
        } => {
            // Resolved once, from the operator's own cwd (mirrors
            // `WorkspacePool::start_safehouse_narration`'s
            // `safehouse::resolve_config(repo_root)` call shape) — see
            // `fleet::drain::flush_safehouse`'s doc comment for the real
            // supervised-stop flush check this now gates on (#3998).
            let cwd = std::env::current_dir().context("resolving cwd for safehouse config")?;
            let safehouse_enabled = loom_daemon::safehouse::resolve_config(&cwd).enabled;

            let poll_interval = Duration::from_secs(drain::DEFAULT_POLL_INTERVAL_SECS);
            let max_polls = ((timeout / drain::DEFAULT_POLL_INTERVAL_SECS.max(1)) + 12) as u32;
            let config = DrainConfig {
                ssh_host,
                timeout_secs: timeout,
                force_after_timeout,
                poll_interval,
                max_polls,
                safehouse_enabled,
                json,
            };
            let report = drain::run(&config)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else {
                print!("{}", report.render_human());
            }
            let code = report.exit_code();
            if code != 0 {
                std::process::exit(code);
            }
            Ok(())
        }
    }
}

fn handle_workspace_command(action: WorkspaceAction) -> Result<()> {
    use loom_daemon::workspace_registry::{AddOutcome, WorkspaceRegistry};

    let path = loom_daemon::workspace_registry::default_registry_path()?;

    match action {
        WorkspaceAction::Add {
            path: repo_path,
            priority,
            config_overrides,
        } => {
            let overrides = match config_overrides {
                Some(raw) => Some(
                    serde_json::from_str::<serde_json::Value>(&raw)
                        .map_err(|e| anyhow!("--config-overrides is not valid JSON: {e}"))?,
                ),
                None => None,
            };
            let mut registry = WorkspaceRegistry::load(&path)?;
            match registry.add_with_priority(
                std::path::Path::new(&repo_path),
                overrides,
                priority,
            )? {
                AddOutcome::AlreadyPresent { canonical } => {
                    println!("Already registered: {}", canonical.display());
                    println!(
                        "  (priority unchanged — use `loom-daemon workspace set-priority {} <N>` \
                         to retier it)",
                        canonical.display()
                    );
                }
                AddOutcome::Added {
                    canonical,
                    looks_like_workspace,
                } => {
                    registry.save(&path)?;
                    println!("Registered workspace: {} (priority {priority})", canonical.display());
                    if !looks_like_workspace {
                        eprintln!(
                            "  warning: {} has no .git or .loom — register it anyway, but confirm \
                             it is a Loom-managed repo (run `loom-daemon init` there if not).",
                            canonical.display()
                        );
                    }
                    // Issue #4027: `.git`/`.loom` alone does not mean the
                    // `/loom:sweep` slash command is installed — those files
                    // are install-not-committed (gitignored), so a bare
                    // `git clone` passes the `looks_like_workspace` check
                    // above while still being undispatchable. Dispatching
                    // into it insta-crashes on `Unknown command:
                    // /loom:sweep`, and because the reaper reverts
                    // `loom:building` -> `loom:issue` on that insta-crash,
                    // the work-finder re-dispatches every tick — an infinite
                    // token-burning loop. `dispatch()` itself now refuses
                    // this case (the load-bearing fix), but warn here too so
                    // the operator sees it at registration time, before any
                    // dispatch is attempted.
                    if !SweepRegistryConfig::new(canonical.clone()).has_sweep_command() {
                        eprintln!(
                            "  warning: {} has no .claude/commands/loom/sweep.md — the \
                             /loom:sweep command is not installed there. Dispatch into this \
                             workspace will be refused until you run `loom-daemon init {}`.",
                            canonical.display(),
                            canonical.display()
                        );
                    }
                }
            }
            Ok(())
        }
        WorkspaceAction::SetPriority {
            path: repo_path,
            priority,
        } => {
            let mut registry = WorkspaceRegistry::load(&path)?;
            if registry.set_priority(std::path::Path::new(&repo_path), priority) {
                registry.save(&path)?;
                println!("Set priority of {repo_path} to {priority}");
            } else {
                eprintln!(
                    "Not registered (no-op): {repo_path}\n  Register it first with \
                     `loom-daemon workspace add {repo_path} --priority {priority}`."
                );
            }
            Ok(())
        }
        WorkspaceAction::Remove { path: repo_path } => {
            let mut registry = WorkspaceRegistry::load(&path)?;
            if registry.remove(std::path::Path::new(&repo_path)) {
                registry.save(&path)?;
                println!("Deregistered workspace: {repo_path}");
            } else {
                println!("Not registered (no-op): {repo_path}");
            }
            Ok(())
        }
        WorkspaceAction::List { json } => {
            let registry = WorkspaceRegistry::load(&path)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&registry)?);
            } else if registry.workspaces.is_empty() {
                println!("No managed workspaces registered.");
                println!("Registry file: {}", path.display());
                println!("\nAdd one with:  loom-daemon workspace add <repo-path>");
            } else {
                println!("Managed workspaces ({}):", registry.workspaces.len());
                println!("Registry file: {}", path.display());
                println!("(priority: lower = higher dispatch priority; default 100)\n");
                println!("  {:>4}  WORKSPACE", "PRIO");
                println!("  {:-<60}", "");
                // Display in dispatch-priority order (#3946) — the same order the
                // autonomous loops drain — without mutating stored insertion order.
                let mut ordered: Vec<&_> = registry.workspaces.iter().collect();
                ordered.sort_by(|a, b| {
                    a.priority
                        .cmp(&b.priority)
                        .then_with(|| a.root.cmp(&b.root))
                });
                for ws in ordered {
                    let overrides = if ws.config_overrides.is_some() {
                        " (has config overrides)"
                    } else {
                        ""
                    };
                    println!("  {:>4}  {}{overrides}", ws.priority, ws.root.display());
                }
            }
            Ok(())
        }
    }
}

/// Resolve the `--workspace` flag to an absolute path. No upward `.git`
/// walk (unlike Python's `find_repo_root()`) — a deliberate Phase-1
/// simplification since this CLI has no existing callers yet; pass the
/// canonical repo root explicitly (as `spawn-claude.sh` already resolves via
/// `git rev-parse --git-common-dir` for the Python selector).
fn resolve_tokens_workspace(workspace: &str) -> Result<PathBuf> {
    let p = Path::new(workspace);
    if p.is_absolute() {
        Ok(p.to_path_buf())
    } else if p == Path::new(".") {
        // The common case (every `--workspace` flag defaults to `"."`): return
        // the cwd itself rather than `cwd.join(".")`, which `PathBuf::join`
        // does not normalize away — it appends a literal trailing `Component::
        // CurDir`, producing paths like `/home/ubuntu/./.loom/tokens` in
        // "no tokens found" warnings (issue #4292). Functionally identical
        // either way; this just keeps resolved/printed paths readable.
        std::env::current_dir().map_err(Into::into)
    } else {
        Ok(std::env::current_dir()?.join(p))
    }
}

/// `loom-daemon claude-config` handler (issue #4415).
fn handle_claude_config_command(action: ClaudeConfigAction) -> Result<()> {
    use loom_daemon::terminal::{
        claude_config_cleanup, claude_config_setup, claude_config_trust, claude_config_validate,
    };

    match action {
        ClaudeConfigAction::Setup {
            name,
            workspace,
            json,
        } => {
            let repo_root = resolve_tokens_workspace(&workspace)?;
            match claude_config_setup(&name, &repo_root) {
                Some(config_dir) => {
                    if json {
                        let mut obj = serde_json::Map::new();
                        obj.insert(
                            "config_dir".to_string(),
                            serde_json::Value::String(config_dir.display().to_string()),
                        );
                        println!("{}", serde_json::Value::Object(obj));
                    } else {
                        println!("{}", config_dir.display());
                    }
                    Ok(())
                }
                None => {
                    eprintln!("error: failed to set up config dir for '{name}'");
                    std::process::exit(1);
                }
            }
        }

        ClaudeConfigAction::Cleanup {
            name,
            workspace,
            json,
        } => {
            let repo_root = resolve_tokens_workspace(&workspace)?;
            let removed = claude_config_cleanup(&name, &repo_root);
            if json {
                let mut obj = serde_json::Map::new();
                obj.insert("removed".to_string(), serde_json::Value::Bool(removed));
                println!("{}", serde_json::Value::Object(obj));
            } else if removed {
                println!("Removed config dir for '{name}'");
            } else {
                println!("No config dir found for '{name}' (nothing to remove)");
            }
            Ok(())
        }

        ClaudeConfigAction::Validate {
            name,
            workspace,
            json,
        } => {
            let repo_root = resolve_tokens_workspace(&workspace)?;
            let healthy = claude_config_validate(&name, &repo_root);
            if json {
                let mut obj = serde_json::Map::new();
                obj.insert("healthy".to_string(), serde_json::Value::Bool(healthy));
                println!("{}", serde_json::Value::Object(obj));
            }
            if healthy {
                Ok(())
            } else {
                if !json {
                    eprintln!("config dir for '{name}' is missing or unhealthy");
                }
                std::process::exit(1);
            }
        }

        ClaudeConfigAction::Trust { project_dir } => {
            let p = Path::new(&project_dir);
            let project_path = if p.is_absolute() {
                p.to_path_buf()
            } else {
                std::env::current_dir()?.join(p)
            };
            claude_config_trust(&project_path);
            Ok(())
        }
    }
}

/// Resolve the effective tokens-pool directory for a `tokens` CLI
/// subcommand's `--workspace` flag (issue #4292, trip-wire 3).
///
/// An **explicit** `--workspace` (anything other than the clap default `"."`)
/// always resolves via today's per-repo/shared precedence
/// ([`resolve_tokens_workspace`] +
/// [`tokens_pool::paths::resolve_tokens_dir`], #3938) — unchanged, so an
/// operator pointing at a specific (possibly unregistered) repo is never
/// silently redirected.
///
/// Only the **default** `"."` case additionally asks whether the resolved cwd
/// is itself a recognized Loom workspace, reusing the exact registry check
/// #4299 established for CLI `--workspace` defaulting
/// ([`workspace_registry::resolve_client_workspace_default`], via
/// [`tokens_pool::paths::resolve_tokens_dir_anchored`]) rather than a second,
/// parallel detection path. When it is not — the machine-level daemon's own
/// `tokens check --ranking` invoked from a bare cwd, or an operator running
/// `loom-daemon tokens check` from `$HOME` — this anchors straight to the
/// shared machine-level pool instead of a per-repo(cwd) path that can
/// coincidentally collide with the shared default (both resolve to
/// `~/.loom/tokens` when cwd is `$HOME`).
///
/// Returns `(tokens_dir, anchored_to_shared)` — the second element is `true`
/// only when the "not a recognized workspace" branch fired, so callers can
/// surface *how* the directory was chosen (not just *which* directory).
fn resolve_tokens_pool_dir_for_cli(workspace: &str) -> Result<(PathBuf, bool)> {
    use loom_daemon::tokens_pool::paths;
    use loom_daemon::workspace_registry::{resolve_client_workspace_default, WorkspaceRegistry};

    let ws = resolve_tokens_workspace(workspace)?;
    if workspace != "." {
        return Ok((paths::resolve_tokens_dir(&ws), false));
    }

    let registry = WorkspaceRegistry::load_default().unwrap_or_default();
    if registry.workspaces.is_empty() {
        return Ok((paths::resolve_tokens_dir(&ws), false));
    }
    // Only actually "anchored to shared" when the shared pool is enabled and
    // would be used — if `LOOM_SHARED_TOKENS_DIR=""` opts out,
    // `resolve_tokens_dir_anchored` falls back to the per-repo(cwd) path
    // instead, and the caller's messaging should say so.
    let anchored_to_shared = resolve_client_workspace_default(&ws, &registry).is_none()
        && paths::shared_tokens_dir().is_some();
    Ok((paths::resolve_tokens_dir_anchored(&ws, &registry), anchored_to_shared))
}

#[cfg(test)]
mod resolve_tokens_workspace_tests {
    //! Tests for [`resolve_tokens_workspace`] (issue #4292). No test here
    //! `chdir`s the process (unsafe to do in a parallel test binary) — the
    //! `"."` case is instead verified against a live `current_dir()` read,
    //! and the absolute-path case needs no cwd at all.
    use super::resolve_tokens_workspace;
    use std::path::Path;

    #[test]
    fn absolute_workspace_is_returned_unchanged() {
        let resolved = resolve_tokens_workspace("/some/repo").expect("resolve");
        assert_eq!(resolved, Path::new("/some/repo"));
    }

    /// The clap default value for every `--workspace` flag is exactly `"."`.
    /// Resolving it must equal the cwd itself — no literal trailing `.`
    /// component (the #4292 cosmetic bug: `cwd.join(".")` produces
    /// `<cwd>/.`, which then reads as `<cwd>/./.loom/tokens` in "no tokens
    /// found" warnings).
    #[test]
    fn dot_workspace_resolves_to_bare_cwd() {
        let resolved = resolve_tokens_workspace(".").expect("resolve");
        let cwd = std::env::current_dir().expect("current_dir");
        assert_eq!(resolved, cwd);
        assert!(
            !resolved.to_string_lossy().contains("/./"),
            "resolved path must not carry a literal './' component: {}",
            resolved.display()
        );
    }

    #[test]
    fn other_relative_workspace_is_joined_to_cwd() {
        let resolved = resolve_tokens_workspace("some/relative/repo").expect("resolve");
        let expected = std::env::current_dir()
            .expect("current_dir")
            .join("some/relative/repo");
        assert_eq!(resolved, expected);
    }
}

#[cfg(test)]
mod resolve_tokens_pool_dir_for_cli_tests {
    //! Tests for [`resolve_tokens_pool_dir_for_cli`] (issue #4292, trip-wire
    //! 3). Like `resolve_tokens_workspace_tests`, no test here `chdir`s the
    //! process — cases that need to distinguish "cwd is/isn't a registered
    //! workspace" register the test's *actual* `current_dir()` (or a sibling
    //! of it) in a scratch `LOOM_WORKSPACES_PATH` registry instead.
    use super::resolve_tokens_pool_dir_for_cli;
    use loom_daemon::tokens_pool::paths::{
        per_repo_tokens_dir, shared_tokens_dir, SHARED_TOKENS_DIR_ENV,
    };
    use loom_daemon::workspace_registry::{
        normalize_path, Workspace, WorkspaceRegistry, REGISTRY_PATH_ENV,
    };
    use serial_test::serial;
    use std::fs;

    /// Mirrors how `WorkspaceRegistry::add` actually stores roots — normalized
    /// / canonicalized — so a raw `tempdir().path()` (which can differ from
    /// its canonical form, e.g. macOS `/var/folders` -> `/private/var/folders`)
    /// compares correctly against the canonicalized query path inside
    /// `resolve_client_workspace_default`.
    fn write_registry(path: &std::path::Path, roots: &[&std::path::Path]) {
        let registry = WorkspaceRegistry {
            version: 1,
            workspaces: roots
                .iter()
                .map(|r| Workspace {
                    root: normalize_path(r),
                    priority: 100,
                    config_overrides: None,
                })
                .collect(),
        };
        fs::write(path, serde_json::to_string_pretty(&registry).unwrap()).unwrap();
    }

    /// Explicit (non-`"."`) `--workspace` never consults the registry, even
    /// when one is present and would otherwise redirect to shared.
    #[test]
    #[serial]
    fn explicit_workspace_bypasses_registry_anchoring() {
        let registry_dir = tempfile::tempdir().unwrap();
        let unrelated = tempfile::tempdir().unwrap();
        write_registry(&registry_dir.path().join("workspaces.json"), &[unrelated.path()]);
        std::env::set_var(REGISTRY_PATH_ENV, registry_dir.path().join("workspaces.json"));
        std::env::set_var(SHARED_TOKENS_DIR_ENV, "");

        let explicit = tempfile::tempdir().unwrap();
        let (dir, anchored) =
            resolve_tokens_pool_dir_for_cli(explicit.path().to_str().unwrap()).unwrap();
        assert_eq!(dir, per_repo_tokens_dir(explicit.path()));
        assert!(!anchored, "an explicit --workspace must never anchor to shared");

        std::env::remove_var(REGISTRY_PATH_ENV);
        std::env::remove_var(SHARED_TOKENS_DIR_ENV);
    }

    /// An empty registry (no `loom-daemon workspace add` ever run) preserves
    /// today's byte-for-byte per-repo/shared behavior for the default `"."`
    /// case too — never anchors to shared.
    #[test]
    #[serial]
    fn empty_registry_default_workspace_is_unaffected() {
        let registry_dir = tempfile::tempdir().unwrap();
        // A registry file that parses to an empty registry.
        write_registry(&registry_dir.path().join("workspaces.json"), &[]);
        std::env::set_var(REGISTRY_PATH_ENV, registry_dir.path().join("workspaces.json"));

        let (dir, anchored) = resolve_tokens_pool_dir_for_cli(".").unwrap();
        let cwd = std::env::current_dir().unwrap();
        assert_eq!(dir, loom_daemon::tokens_pool::paths::resolve_tokens_dir(&cwd));
        assert!(!anchored);

        std::env::remove_var(REGISTRY_PATH_ENV);
    }

    /// A non-empty registry that happens to register the test's own cwd
    /// resolves against that (registered) root — unchanged per-repo/shared
    /// precedence, no shared-anchoring.
    #[test]
    #[serial]
    fn non_empty_registry_with_cwd_registered_is_unaffected() {
        let cwd = std::env::current_dir().unwrap();
        let registry_dir = tempfile::tempdir().unwrap();
        write_registry(&registry_dir.path().join("workspaces.json"), &[&cwd]);
        std::env::set_var(REGISTRY_PATH_ENV, registry_dir.path().join("workspaces.json"));

        let (dir, anchored) = resolve_tokens_pool_dir_for_cli(".").unwrap();
        assert_eq!(dir, loom_daemon::tokens_pool::paths::resolve_tokens_dir(&cwd));
        assert!(!anchored);

        std::env::remove_var(REGISTRY_PATH_ENV);
    }

    /// A non-empty registry that does NOT include the test's cwd anchors the
    /// default `"."` case straight to the shared pool (the machine-level
    /// daemon / bare-cwd CLI-invocation case this trip-wire fixes).
    #[test]
    #[serial]
    fn non_empty_registry_with_cwd_unregistered_anchors_to_shared() {
        let cwd = std::env::current_dir().unwrap();
        let unrelated = tempfile::tempdir().unwrap();
        assert_ne!(unrelated.path(), cwd, "sanity: must not coincide with cwd");
        let registry_dir = tempfile::tempdir().unwrap();
        write_registry(&registry_dir.path().join("workspaces.json"), &[unrelated.path()]);
        std::env::set_var(REGISTRY_PATH_ENV, registry_dir.path().join("workspaces.json"));
        std::env::remove_var(SHARED_TOKENS_DIR_ENV); // default shared dir enabled

        let (dir, anchored) = resolve_tokens_pool_dir_for_cli(".").unwrap();
        assert_eq!(dir, shared_tokens_dir().unwrap());
        assert!(anchored);

        std::env::remove_var(REGISTRY_PATH_ENV);
    }

    /// The `LOOM_SHARED_TOKENS_DIR=""` opt-out disables shared-anchoring too,
    /// not just the original per-repo/shared fallback — falls back to the
    /// per-repo(cwd) path and reports `anchored = false`.
    #[test]
    #[serial]
    fn shared_pool_disabled_falls_back_to_per_repo_cwd() {
        let cwd = std::env::current_dir().unwrap();
        let unrelated = tempfile::tempdir().unwrap();
        let registry_dir = tempfile::tempdir().unwrap();
        write_registry(&registry_dir.path().join("workspaces.json"), &[unrelated.path()]);
        std::env::set_var(REGISTRY_PATH_ENV, registry_dir.path().join("workspaces.json"));
        std::env::set_var(SHARED_TOKENS_DIR_ENV, "");

        let (dir, anchored) = resolve_tokens_pool_dir_for_cli(".").unwrap();
        assert_eq!(dir, loom_daemon::tokens_pool::paths::resolve_tokens_dir(&cwd));
        assert!(!anchored);

        std::env::remove_var(REGISTRY_PATH_ENV);
        std::env::remove_var(SHARED_TOKENS_DIR_ENV);
    }
}

/// Human-readable label for an account's merge provenance. Mirrors
/// `cli._SOURCE_LABEL`.
fn source_label(source: &str) -> &str {
    match source {
        "home" => "home",
        "repo" => "repo",
        "repo-override" => "repo (overrides home)",
        "monitor" => "claude-monitor",
        "monitor-override" => "claude-monitor (overrides repo/home)",
        other => other,
    }
}

/// Print the effective merged account set and where each came from. Mirrors
/// `cli._print_effective_accounts` — secrets are never shown, only email, token
/// filename, and source.
fn print_effective_accounts(result: &loom_daemon::tokens_pool::bootstrap::BootstrapResult) {
    let disp = |p: &Option<std::path::PathBuf>| -> String {
        p.as_ref()
            .map_or_else(|| "(none)".to_string(), |p| p.display().to_string())
    };
    println!("Account sources:");
    println!("  claude-monitor: {}", disp(&result.monitor_env));
    println!("  home: {}", disp(&result.home_env));
    println!("  repo: {}", disp(&result.repo_env));

    if result.effective.is_empty() {
        println!("Effective accounts: (none)");
        return;
    }

    println!("Effective accounts ({}):", result.effective.len());
    let width = result
        .effective
        .iter()
        .map(|a| a.name.chars().count())
        .max()
        .unwrap_or(0);
    for a in &result.effective {
        let label = source_label(&a.source);
        println!("  {:<width$}  {}  [{label}]", a.name, a.email, width = width);
    }
}

/// Print the imported account set from `import-from-monitor`. Secrets are
/// never shown. Mirrors `cli._print_monitor_import`.
fn print_monitor_import(result: &loom_daemon::tokens_pool::monitor_db::MonitorImportResult) {
    let disp = |p: &Option<std::path::PathBuf>| -> String {
        p.as_ref()
            .map_or_else(|| "(none)".to_string(), |p| p.display().to_string())
    };
    println!("claude-monitor store: {}", disp(&result.db_path));
    println!("Destination pool: {}", disp(&result.tokens_dir));

    if result.effective.is_empty() {
        println!("Active accounts: (none)");
        return;
    }

    let written: std::collections::HashSet<&str> =
        result.written.iter().map(String::as_str).collect();
    let unchanged: std::collections::HashSet<&str> =
        result.unchanged.iter().map(String::as_str).collect();
    let drifted: std::collections::HashSet<&str> =
        result.drifted.iter().map(String::as_str).collect();

    println!("Active accounts ({}):", result.effective.len());
    for a in &result.effective {
        let disposition = if written.contains(a.file.as_str()) {
            "written"
        } else if unchanged.contains(a.file.as_str()) {
            "unchanged"
        } else if drifted.contains(a.file.as_str()) {
            "DRIFT (use --force)"
        } else {
            "-"
        };
        println!("  {}  {}  [{disposition}]", a.name, a.email);
    }

    if !result.pruned.is_empty() {
        println!("Pruned ({}): {}", result.pruned.len(), result.pruned.join(", "));
    }
}

/// Handle `loom-daemon forge <issue|pr|auth|auto-merge>` (epic #4081 Phase 3,
/// family 3 — the native port of `loom-forge` / `loom-auto-merge`). Handlers
/// exec `gh` / exit the process directly, so this only returns `Err` when a
/// child process cannot be spawned. See `loom-daemon/src/forge_cmd.rs`.
fn handle_forge_command(action: ForgeAction) -> Result<()> {
    use loom_daemon::forge_cmd::{dispatch, ForgeCmd};
    let cmd = match action {
        ForgeAction::Issue { args } => ForgeCmd::Issue(args),
        ForgeAction::Pr { args } => ForgeCmd::Pr(args),
        ForgeAction::Auth { args } => ForgeCmd::Auth(args),
        ForgeAction::AutoMerge {
            pr_number, method, ..
        } => ForgeCmd::AutoMerge {
            pr: pr_number,
            method,
        },
    };
    dispatch(cmd)
}

/// Handle `loom-daemon tokens <select|pin|unpin|unblock|mark-bad>` (Issue
/// #4082, Phase 1 of epic #4081; `mark-bad` added in #4228, Phase 2). Purely
/// file-based — does not require a running daemon. See
/// `loom-daemon/src/tokens_pool/mod.rs` for the ported subset and what is
/// deliberately deferred.
fn shell_single_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn handle_tokens_command(action: TokensAction) -> Result<()> {
    use loom_daemon::tokens_pool::{allowlist, bad_tokens, failure_counts, select};

    match action {
        TokensAction::Select {
            workspace,
            provider,
            export,
            no_key,
            auto_unpin,
        } => {
            let ws = resolve_tokens_workspace(&workspace)?;
            if provider == "codex" {
                let selected = loom_daemon::tokens_pool::select_account(
                    &ws,
                    loom_daemon::tokens_pool::AccountProvider::Codex,
                )
                .map_err(|error| anyhow!(error))?;
                let directory = match &selected.binding {
                    loom_daemon::tokens_pool::AccountBinding::CodexHome { directory } => directory,
                    _ => unreachable!("Codex selection returned a non-Codex binding"),
                };
                if export {
                    println!(
                        "export CODEX_HOME={}",
                        shell_single_quote(&directory.display().to_string())
                    );
                    println!(
                        "export LOOM_ACCOUNT_PROVIDER='codex'\nexport LOOM_ACCOUNT_NAME={}",
                        shell_single_quote(&selected.id.name)
                    );
                    println!("LOOM_TOKEN_MODE='{}'", selected.mode);
                } else {
                    println!(
                        "{}",
                        serde_json::json!({
                            "provider": "codex",
                            "name": selected.id.name,
                            "credential_kind": "codex_home",
                            "credential_reference": directory,
                            "mode": selected.mode,
                        })
                    );
                }
                return Ok(());
            }
            if provider != "claude" {
                bail!("invalid provider {provider:?}; expected claude or codex");
            }
            if auto_unpin {
                if let Some(msg) = loom_daemon::tokens_pool::maybe_auto_unpin(&ws) {
                    eprintln!("{msg}");
                }
            }
            match select::select_token(&ws, None) {
                Ok(sel) => {
                    if export {
                        if no_key {
                            println!(
                                "# selected={} mode={} file={}",
                                sel.name,
                                sel.mode,
                                sel.file.display()
                            );
                        } else {
                            // Tokens are base64/hex-like and never contain a
                            // single quote in practice; this is a simple
                            // wrap, not a full Python repr() escape.
                            println!("export CLAUDE_CODE_OAUTH_TOKEN='{}'", sel.key);
                            // Shell-evalable (issue #4228): lets
                            // spawn-claude.sh / claude-wrapper.sh `eval` this
                            // output directly instead of round-tripping
                            // through `python3 -c 'import json...'`.
                            println!("export LOOM_TOKEN_NAME='{}'", sel.name);
                            println!("LOOM_TOKEN_MODE='{}'", sel.mode);
                            println!(
                                "# selected={} mode={} file={}",
                                sel.name,
                                sel.mode,
                                sel.file.display()
                            );
                        }
                    } else {
                        let mut obj = serde_json::Map::new();
                        obj.insert("name".to_string(), serde_json::Value::String(sel.name));
                        obj.insert(
                            "file".to_string(),
                            serde_json::Value::String(sel.file.display().to_string()),
                        );
                        obj.insert(
                            "mode".to_string(),
                            serde_json::Value::String(sel.mode.to_string()),
                        );
                        if !no_key {
                            obj.insert("key".to_string(), serde_json::Value::String(sel.key));
                        }
                        println!("{}", serde_json::Value::Object(obj));
                    }
                    Ok(())
                }
                Err(e) => {
                    eprintln!("error: {e}");
                    std::process::exit(select::EX_CONFIG);
                }
            }
        }

        TokensAction::Bootstrap {
            workspace,
            env,
            home_env,
            no_home,
            shared,
            force,
            dry_run,
            json,
        } => {
            use loom_daemon::tokens_pool::bootstrap::{self, BootstrapError, BootstrapOptions};
            use loom_daemon::tokens_pool::paths::shared_tokens_dir;

            let repo_root = resolve_tokens_workspace(&workspace)?;

            // `--shared` redirects the destination pool to the machine-level
            // location (issue #3938). Only the write target changes; account
            // sources are unchanged. Refuse when the shared pool is disabled.
            let tokens_dir = if shared {
                match shared_tokens_dir() {
                    Some(dir) => {
                        eprintln!(
                            "Bootstrapping the shared machine-level pool at {}",
                            dir.display()
                        );
                        Some(dir)
                    }
                    None => {
                        eprintln!(
                            "error: --shared requested but the shared pool is disabled \
                             (LOOM_SHARED_TOKENS_DIR is empty). Unset it or point it at a directory."
                        );
                        std::process::exit(1);
                    }
                }
            } else {
                None
            };

            // `--no-home` disables the master; `--home-env` points elsewhere;
            // neither falls through to default resolution ($LOOM_ACCOUNTS_ENV).
            let home_env_path = if no_home {
                Some(None)
            } else {
                home_env.map(|p| Some(std::path::PathBuf::from(p)))
            };

            let opts = BootstrapOptions {
                repo_root,
                env_path: env.map(std::path::PathBuf::from),
                home_env_path,
                force,
                dry_run,
                tokens_dir,
            };

            let result = match bootstrap::bootstrap_tokens(&opts) {
                Ok(r) => r,
                Err(e @ (BootstrapError::NoSource(_) | BootstrapError::DuplicateFile(_))) => {
                    eprintln!("error: {e}");
                    std::process::exit(1);
                }
                Err(BootstrapError::Io(e)) => return Err(anyhow!("bootstrap failed: {e}")),
            };

            // Warnings (partial triples, drift, unreadable tokens) go to stderr
            // so `--json` stdout stays clean.
            for w in &result.warnings {
                eprintln!("warning: {w}");
            }

            if json {
                println!("{}", serde_json::to_string_pretty(&result.to_json())?);
            } else {
                print_effective_accounts(&result);
            }

            // Unresolved drift without --force is a non-zero exit so CI can
            // detect divergence (mirrors cli._cmd_bootstrap).
            if !result.drifted.is_empty() && !force {
                std::process::exit(2);
            }
            Ok(())
        }

        TokensAction::ImportFromMonitor {
            workspace,
            shared,
            db,
            force,
            prune,
            dry_run,
            json,
        } => {
            use loom_daemon::tokens_pool::monitor_db::{
                import_from_monitor, ImportOptions, MonitorImportError,
            };
            use loom_daemon::tokens_pool::paths::shared_tokens_dir;

            // Destination: the shared machine-level pool, or this repo's pool
            // (mirrors cli._cmd_import_from_monitor). `--workspace` is a
            // plain path here (no upward `.git` walk), matching the sibling
            // `bootstrap` / `check` / `select` CLI arms.
            let tokens_dir = if shared {
                match shared_tokens_dir() {
                    Some(dir) => {
                        eprintln!(
                            "Importing into the shared machine-level pool at {}",
                            dir.display()
                        );
                        dir
                    }
                    None => {
                        eprintln!(
                            "error: --shared requested but the shared pool is disabled \
                             (LOOM_SHARED_TOKENS_DIR is empty). Unset it or point it at a directory."
                        );
                        std::process::exit(1);
                    }
                }
            } else {
                let ws = resolve_tokens_workspace(&workspace)?;
                ws.join(".loom").join("tokens")
            };

            let db_path = db.map(std::path::PathBuf::from);
            let opts = ImportOptions {
                tokens_dir: &tokens_dir,
                db_path: db_path.as_deref(),
                monitor_dir: None,
                force,
                dry_run,
                prune,
            };

            let result = match import_from_monitor(&opts) {
                Ok(r) => r,
                Err(
                    e @ (MonitorImportError::DbUnavailable(_)
                    | MonitorImportError::DuplicateFile(_)),
                ) => {
                    eprintln!("error: {e}");
                    std::process::exit(1);
                }
                Err(MonitorImportError::Io(e)) => {
                    return Err(anyhow!("import-from-monitor failed: {e}"))
                }
            };

            // Warnings (no active credentials, unreadable tokens, prune
            // failures, drift) go to stderr so `--json` stdout stays clean.
            for w in &result.warnings {
                eprintln!("warning: {w}");
            }

            if json {
                println!("{}", serde_json::to_string_pretty(&result.to_json())?);
            } else {
                print_monitor_import(&result);
            }

            // Unresolved drift without --force is the "rolled tokens not
            // applied" case; exit non-zero so a script notices the pool is
            // still stale (mirrors cli._cmd_import_from_monitor).
            if !result.drifted.is_empty() && !force {
                std::process::exit(2);
            }
            Ok(())
        }

        TokensAction::Check {
            workspace,
            ranking,
            source,
            probe_prompt,
            json,
            no_stagger,
        } => {
            use loom_daemon::tokens_pool::check::{
                self, CheckOptions, CurlTransport, DEFAULT_PROBE_MODEL, DEFAULT_PROBE_PROMPT,
            };

            let (tokens_dir, anchored_to_shared) = resolve_tokens_pool_dir_for_cli(&workspace)?;
            if anchored_to_shared {
                eprintln!(
                    "note: --workspace defaulted to a directory that is not a registered Loom \
                     workspace; anchoring to the shared machine-level pool at {} (issue #4292)",
                    tokens_dir.display()
                );
            }

            let source_flag = match source {
                Some(raw) => match check::Source::parse(&raw) {
                    Some(s) => Some(s),
                    None => {
                        eprintln!(
                            "error: invalid --source {raw:?}; expected one of auto, monitor, probe"
                        );
                        std::process::exit(2);
                    }
                },
                None => None,
            };
            let resolved_source = check::resolve_source(source_flag);
            let prompt = probe_prompt.unwrap_or_else(|| DEFAULT_PROBE_PROMPT.to_string());

            let opts = CheckOptions {
                source: resolved_source,
                write_ranking: ranking,
                probe_prompt: &prompt,
                model: DEFAULT_PROBE_MODEL,
                stagger: !no_stagger,
            };
            let transport = CurlTransport;
            let report = check::run_check(&tokens_dir, &opts, &transport);

            if json {
                println!("{}", serde_json::to_string_pretty(&report.to_json())?);
            } else {
                println!("{}", check::format_table(&report));
            }

            // Exit 1 only when every probe failed (selector has nothing usable).
            if !report.accounts.is_empty()
                && report
                    .accounts
                    .iter()
                    .all(|a| a.status == "error" || a.status == "skipped")
            {
                std::process::exit(1);
            }
            Ok(())
        }

        TokensAction::Pin { action, workspace } => {
            let ws = resolve_tokens_workspace(&workspace)?;
            handle_pin_action(action, &ws)
        }

        TokensAction::Unpin { workspace, json } => {
            let ws = resolve_tokens_workspace(&workspace)?;
            let had_file = allowlist::clear_allowlist(&ws).map_err(|e| anyhow!(e))?;
            let _ = failure_counts::reset_all(&ws);
            if json {
                println!("{}", serde_json::json!({ "cleared": had_file }));
            } else if had_file {
                println!("Allowlist cleared. All accounts are eligible.");
            } else {
                println!("No allowlist was active.");
            }
            Ok(())
        }

        TokensAction::Unblock {
            names,
            workspace,
            all_reasons,
            json,
        } => {
            let ws = resolve_tokens_workspace(&workspace)?;

            let available = allowlist::list_accounts(&ws);
            let available_set: std::collections::HashSet<&str> =
                available.iter().map(String::as_str).collect();
            let mut validated: Vec<String> = Vec::new();
            for raw in &names {
                let name = raw.trim();
                if name.is_empty() {
                    continue;
                }
                if !available_set.contains(name) {
                    let avail = if available.is_empty() {
                        "(none)".to_string()
                    } else {
                        available.join(", ")
                    };
                    eprintln!("Unknown account '{name}'. Available: {avail}");
                    std::process::exit(2);
                }
                validated.push(name.to_string());
            }
            if validated.is_empty() {
                eprintln!("`unblock` requires at least one account name.");
                std::process::exit(1);
            }

            let outcome =
                bad_tokens::unblock(&ws, &validated, all_reasons).map_err(|e| anyhow!(e))?;
            let removed = outcome.removed;
            let kept = outcome.kept;
            let excluded = outcome.excluded;
            for name in &validated {
                let _ = failure_counts::record_success(&ws, name);
            }

            if json {
                println!(
                    "{}",
                    serde_json::json!({
                        "removed": removed,
                        "kept": kept,
                        "excluded": excluded,
                    })
                );
            } else {
                if removed > 0 {
                    let plural = if removed == 1 { "y" } else { "ies" };
                    println!(
                        "Removed {removed} bad-token entr{plural} for: {}",
                        validated.join(", ")
                    );
                }
                if !excluded.is_empty() {
                    // #4212: a no-op that looks like success is the failure
                    // mode. Name the still-blocked accounts and fail below.
                    let plural = if excluded.len() == 1 { "y" } else { "ies" };
                    eprintln!(
                        "Left {} non-auth (exhausted/rate-limited) entr{plural} in place for: \
                         {}. These are still blocking selection — re-run with --all-reasons to \
                         drop them (or wait for the exhaustion cooldown to expire them \
                         automatically).",
                        excluded.len(),
                        excluded.join(", ")
                    );
                } else if removed == 0 {
                    println!(
                        "No matching entries removed (use --all-reasons to drop non-auth entries \
                         too)."
                    );
                }
            }

            // Non-zero when the default scope left the named accounts still
            // blocked — the operator's intent ("unblock X") was not achieved.
            if !excluded.is_empty() {
                std::process::exit(3);
            }
            Ok(())
        }

        TokensAction::MarkBad {
            name,
            reason,
            workspace,
            json,
        } => {
            let ws = resolve_tokens_workspace(&workspace)?;
            match bad_tokens::mark_bad(&ws, &name, &reason) {
                Ok(()) => {
                    if json {
                        println!("{}", serde_json::json!({ "marked": true, "name": name }));
                    } else {
                        println!("Marked '{name}' bad ({reason}).");
                    }
                    Ok(())
                }
                Err(e) => {
                    eprintln!("error: {e}");
                    std::process::exit(1);
                }
            }
        }
    }
}

/// Handle `loom-daemon tokens pin <set|add|remove|status>`.
fn handle_pin_action(action: PinAction, ws: &Path) -> Result<()> {
    use loom_daemon::tokens_pool::{allowlist, failure_counts};

    match action {
        PinAction::Set { names } => match allowlist::write_allowlist(ws, &names) {
            Ok(written) => {
                let _ = failure_counts::reset_all(ws);
                if written.is_empty() {
                    println!("Allowlist cleared (no names resolved). All accounts are eligible.");
                } else {
                    println!(
                        "Allowlist set to {} account(s): {}",
                        written.len(),
                        written.join(", ")
                    );
                }
                Ok(())
            }
            Err(allowlist::AllowlistError::Unknown(e)) => {
                eprintln!("{e}");
                std::process::exit(2);
            }
            Err(allowlist::AllowlistError::Io(e)) => {
                eprintln!("{e}");
                std::process::exit(1);
            }
        },

        PinAction::Add { names } => match allowlist::add_to_allowlist(ws, &names) {
            Ok((added, skipped)) => {
                let _ = failure_counts::reset_all(ws);
                if !added.is_empty() {
                    println!("Added {} account(s): {}", added.len(), added.join(", "));
                }
                if !skipped.is_empty() {
                    println!("Already present: {}", skipped.join(", "));
                }
                Ok(())
            }
            Err(allowlist::AllowlistError::Unknown(e)) => {
                eprintln!("{e}");
                std::process::exit(2);
            }
            Err(allowlist::AllowlistError::Io(e)) => {
                eprintln!("{e}");
                std::process::exit(1);
            }
        },

        PinAction::Remove { names } => match allowlist::remove_from_allowlist(ws, &names) {
            Ok((removed, skipped)) => {
                let _ = failure_counts::reset_all(ws);
                if !removed.is_empty() {
                    println!("Removed {} account(s): {}", removed.len(), removed.join(", "));
                }
                if !skipped.is_empty() {
                    println!("Not in allowlist: {}", skipped.join(", "));
                }
                Ok(())
            }
            Err(allowlist::AllowlistError::Unknown(e)) => {
                eprintln!("{e}");
                std::process::exit(2);
            }
            Err(allowlist::AllowlistError::Io(e)) => {
                eprintln!("{e}");
                std::process::exit(1);
            }
        },

        PinAction::Status { json } => {
            let all_accounts = allowlist::list_accounts(ws);
            let active = allowlist::read_allowlist(ws);
            if json {
                let payload = serde_json::json!({
                    "allowlist_active": !active.is_empty(),
                    "allowlist": active,
                    "accounts": all_accounts,
                });
                println!("{payload}");
                return Ok(());
            }
            if active.is_empty() {
                println!("No allowlist active — all accounts are eligible.");
            } else {
                println!("Allowlist active ({} account(s)):", active.len());
                for name in &active {
                    println!("  * {name}");
                }
            }
            if all_accounts.is_empty() {
                println!();
                eprintln!("No .token files found. Run `loom-tokens bootstrap` first.");
            } else {
                println!();
                println!("Available accounts ({}):", all_accounts.len());
                let active_set: std::collections::HashSet<&str> =
                    active.iter().map(String::as_str).collect();
                for name in &all_accounts {
                    let mark = if active_set.contains(name.as_str()) {
                        "*"
                    } else {
                        " "
                    };
                    println!("  {mark} {name}");
                }
            }
            Ok(())
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn handle_clean_command(
    workspace: &str,
    dry_run: bool,
    deep: bool,
    force: bool,
    safe: bool,
    grace_period: i64,
    worktrees_only: bool,
    branches_only: bool,
    tmux_only: bool,
    daemon: bool,
    aggressive: bool,
    aggressive_min_age: u64,
) -> Result<()> {
    use loom_daemon::worktree_ops::aggressive as agg;
    use loom_daemon::worktree_ops::repo;

    let repo_root = repo::resolve_repo_root(workspace)?;

    if daemon {
        println!();
        println!("========================================");
        println!("  Loom Crash Recovery");
        if dry_run {
            println!("  (DRY RUN MODE)");
        }
        println!("========================================");
        println!();
        clean::clean_daemon_crash_state(&repo_root, dry_run);
        if dry_run {
            println!("Dry run complete - no changes made");
        } else {
            println!("Crash recovery complete!");
        }
        println!();
        return Ok(());
    }

    if aggressive {
        println!();
        println!("========================================");
        println!("  Loom Aggressive Worktree Cleanup");
        if dry_run {
            println!("  (DRY RUN MODE)");
        }
        println!("========================================");
        println!();
        eprintln!(
            "Aggressive mode overrides .loom-in-use markers and process-table guards. Respects \
             open PRs, active shepherds, the .loom-managed sentinel, uncommitted changes, and \
             reachability from origin/main."
        );
        println!();

        let stats = agg::clean_aggressive(&repo_root, dry_run, force, aggressive_min_age);
        agg::print_aggressive_summary(&stats, dry_run);
        if dry_run {
            println!("Dry run complete - no changes made");
            println!("Run without --dry-run to perform cleanup");
        } else {
            println!("Aggressive cleanup complete!");
        }
        println!();
        std::process::exit(i32::from(stats.errors > 0));
    }

    let opts = clean::CleanOptions {
        dry_run,
        deep,
        force,
        safe,
        grace_period_secs: grace_period,
        worktrees_only,
        branches_only,
        tmux_only,
    };
    let exit_code = clean::run_clean(&repo_root, &opts);
    if exit_code != 0 {
        std::process::exit(exit_code);
    }
    Ok(())
}

fn handle_cleanup_command(action: CleanupAction) -> Result<()> {
    use loom_daemon::worktree_ops::{logs, repo};
    match action {
        CleanupAction::Logs {
            workspace,
            dry_run,
            prune_only,
            retention_days,
        } => {
            let repo_root = repo::resolve_repo_root(&workspace)?;
            let rc = logs::handle_logs(&repo_root, dry_run, prune_only, retention_days);
            if rc != 0 {
                std::process::exit(rc);
            }
            Ok(())
        }
    }
}

fn handle_recover_orphans_command(
    workspace: &str,
    recover: bool,
    json: bool,
    verbose: bool,
) -> Result<()> {
    use loom_daemon::worktree_ops::{orphan_recovery as orphans, repo};

    let repo_root = repo::resolve_repo_root(workspace)?;

    if !json {
        println!("Orphaned Spawn-Loop Task Detection & Recovery");
        if !recover {
            println!("DRY RUN - No changes will be made");
            println!("Use --recover to actually perform recovery");
        }
    }

    let result = orphans::run_orphan_recovery(&repo_root, recover, verbose);

    if json {
        println!("{}", orphans::format_result_json(&result));
    } else {
        println!("{}", orphans::format_result_human(&result));
    }

    if !result.orphaned.is_empty() && !recover {
        std::process::exit(2);
    }
    Ok(())
}

/// Handle `loom-daemon calibrate` (issue #4390): measure the host, print (or
/// `--write` apply) the recommended `autonomous.workFinder.maxConcurrent` /
/// `autonomous.perTokenConcurrency` values. See `loom_daemon::calibrate` for
/// the measurement + recommendation policy this thinly wires up.
fn handle_calibrate_command(workspace: &str, write: bool, json: bool) -> Result<()> {
    use loom_daemon::calibrate;
    use loom_daemon::worktree_ops::repo;

    let repo_root = repo::resolve_repo_root(workspace)?;

    let measurements = calibrate::measure(&repo_root);
    let recommendation = calibrate::recommend(&measurements);

    let written = if write {
        Some(
            calibrate::write_workfinder_config(
                &repo_root,
                recommendation.recommended_max_concurrent,
                recommendation.recommended_per_token_concurrency,
            )
            .map_err(|e| anyhow!("{e}"))?,
        )
    } else {
        None
    };

    if json {
        let report = calibrate::report_json(&measurements, &recommendation, written.as_deref());
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print!(
            "{}",
            calibrate::report_human(&measurements, &recommendation, written.as_deref())
        );
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Script-helper CLIs (epic #4081 Phase 3 family 5, issue #4275)
//
// Each of these backs one `defaults/scripts/*.sh` entry point whose Python
// implementation was deleted. The shell stubs exec the daemon binary with the
// same flags they always accepted, so a zero-pip consumer workspace sees no
// behavior change.
// ---------------------------------------------------------------------------

/// `loom-daemon strip-ansi [--file PATH]` — backs `strip-ansi.sh`.
fn handle_strip_ansi_command(file: Option<&str>) -> Result<()> {
    use script_helpers::log_filter;

    if let Some(path) = file {
        print!("{}", log_filter::clean_file(Path::new(path)));
        return Ok(());
    }
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    // A broken downstream pipe is the normal way a pipe-pane filter ends; the
    // Python swallowed BrokenPipeError, so do the same rather than surfacing a
    // crash in every agent log.
    match log_filter::filter_stream(stdin.lock(), &mut stdout) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => Ok(()),
        Err(e) => Err(e.into()),
    }
}

/// `loom-daemon resolve-model` — backs `resolve-model.sh` / `resolve-tier-model.sh`.
///
/// Exit codes mirror the retired Python CLI exactly: `0` on success, `2` when
/// neither a model nor `--tier` was supplied, and `3` with NO output when
/// `--tier` / `--task-alias` has no mapping so the caller keeps its own
/// precedence chain.
fn handle_resolve_model_command(
    model: Option<&str>,
    config: Option<&str>,
    generation: bool,
    task_alias: bool,
    tier: Option<&str>,
    runtime: &str,
) -> Result<()> {
    use script_helpers::model_tiers;

    let cfg = model_tiers::load_config(config.map(Path::new));

    // Complexity-tier mode (#4238). Checked before the positional model, which
    // the Python parser also treated as optional in this mode.
    if let Some(tier) = tier {
        let env_override = std::env::var("LOOM_SWEEP_OPTIMIZATION").ok();
        let resolved =
            model_tiers::resolve_tier_model(Some(tier), runtime, &cfg, env_override.as_deref());
        if resolved.is_empty() {
            std::process::exit(3);
        }
        println!("{resolved}");
        return Ok(());
    }

    let Some(model) = model else {
        eprintln!("loom-daemon resolve-model: error: a model argument or --tier is required");
        std::process::exit(2);
    };

    // Task-tool degradation mode (#4282).
    if task_alias {
        let alias = model_tiers::task_alias_of(model);
        if alias.is_empty() {
            std::process::exit(3);
        }
        println!("{alias}");
        return Ok(());
    }

    if generation {
        match model_tiers::generation_of(model, &cfg) {
            Some(g) => println!("{g}"),
            // The Python printed an empty line for an unrecognized model.
            None => println!(),
        }
    } else {
        println!("{}", model_tiers::resolve_model(model, &cfg));
    }
    Ok(())
}

/// The worktree a checkpoint command targets: `--worktree` when given, else the
/// current directory (matching the Python default).
fn checkpoint_worktree(worktree: Option<&str>) -> PathBuf {
    worktree.map_or_else(
        || std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
        |w| std::fs::canonicalize(w).unwrap_or_else(|_| PathBuf::from(w)),
    )
}

/// `loom-daemon checkpoint <write|read|clear|stages>` — backs `checkpoint.sh`.
fn handle_checkpoint_command(action: CheckpointAction) -> Result<()> {
    use script_helpers::checkpoints;

    match action {
        CheckpointAction::Stages { json } => {
            if json {
                println!("{}", checkpoints::stages_value());
            } else {
                println!("{}", checkpoints::stages_text());
            }
            Ok(())
        }
        CheckpointAction::Write {
            worktree,
            stage,
            issue,
            files_changed,
            test_command,
            test_result,
            test_output_summary,
            commit_sha,
            pr_number,
            quiet,
        } => {
            let details = checkpoints::CheckpointDetails {
                files_changed: files_changed.unwrap_or(0),
                test_command: test_command.unwrap_or_default(),
                test_result: test_result.unwrap_or_default(),
                test_output_summary: test_output_summary.unwrap_or_default(),
                commit_sha: commit_sha.unwrap_or_default(),
                pr_number,
            };
            let ok = checkpoints::write_checkpoint(
                &checkpoint_worktree(worktree.as_deref()),
                &stage,
                issue.unwrap_or(0),
                details,
                quiet,
            );
            std::process::exit(i32::from(!ok));
        }
        CheckpointAction::Read { worktree, json } => {
            let path = checkpoint_worktree(worktree.as_deref());
            match checkpoints::read_checkpoint(&path) {
                None => {
                    if json {
                        println!("{}", serde_json::json!({"checkpoint": null, "exists": false}));
                    } else {
                        script_helpers::log_warning(&format!(
                            "No checkpoint found in {}",
                            path.display()
                        ));
                    }
                }
                Some(cp) => {
                    if json {
                        println!(
                            "{}",
                            serde_json::json!({
                                "checkpoint": cp.to_value(),
                                "exists": true,
                                "recommendation": checkpoints::recovery_recommendation(Some(&cp)),
                            })
                        );
                    } else {
                        println!("{}", checkpoints::read_text(&cp));
                    }
                }
            }
            Ok(())
        }
        CheckpointAction::Clear { worktree, quiet } => {
            let ok =
                checkpoints::clear_checkpoint(&checkpoint_worktree(worktree.as_deref()), quiet);
            std::process::exit(i32::from(!ok));
        }
    }
}

/// `loom-daemon sweep-experiment <subcommand>` — backs `sweep-experiment.sh`.
#[allow(clippy::too_many_lines)]
fn handle_sweep_experiment_command(action: SweepExperimentAction) -> Result<()> {
    use script_helpers::{model_tiers, sweep_experiment as se};

    let env_mode = std::env::var("LOOM_MODEL_EXPERIMENT").ok();
    let env_canary = std::env::var("LOOM_MODEL_EXPERIMENT_CANARY").ok();

    match action {
        SweepExperimentAction::ResolveMode { config } => {
            let cfg = model_tiers::load_config(config.as_deref().map(Path::new));
            let (mode, warnings) = se::resolve_effective_mode_default(
                env_mode.as_deref(),
                env_canary.as_deref(),
                &cfg,
                None,
            );
            for w in warnings {
                eprintln!("[sweep-experiment] WARNING: {w}");
            }
            println!("{mode}");
            Ok(())
        }
        SweepExperimentAction::AssignArm {
            issue,
            complexity,
            format,
            resolve,
            config,
        } => {
            let arm = se::assign_arm(issue, complexity.as_deref());
            // The default prints the logical alias (Arm A -> `opus`), which the
            // arm identity and the shell test key off. `--resolve` prints the
            // concrete ID the #3982 tier map resolves that alias to.
            let model = if resolve {
                let cfg = model_tiers::load_config(config.as_deref().map(Path::new));
                se::resolved_arm_model(arm, &cfg)
            } else {
                se::arm_model(arm)
            };
            if format == "json" {
                println!(
                    "{}",
                    serde_json::json!({
                        "issue": issue,
                        "complexity": se::normalize_complexity(complexity.as_deref()),
                        "arm": arm,
                        "model": model,
                    })
                );
            } else {
                println!("{arm} {model}");
            }
            Ok(())
        }
        SweepExperimentAction::Banner {
            issue,
            complexity,
            config,
        } => {
            let cfg = model_tiers::load_config(config.as_deref().map(Path::new));
            let (raw_mode, _) = se::resolve_raw_mode(env_mode.as_deref(), &cfg);
            let (mode, warnings) = se::resolve_effective_mode_default(
                env_mode.as_deref(),
                env_canary.as_deref(),
                &cfg,
                None,
            );
            for w in warnings {
                eprintln!("[sweep-experiment] WARNING: {w}");
            }
            let mut arm: Option<&str> = None;
            let mut model = String::new();
            let mut canary_source: Option<String> = None;
            if mode == "experiment" {
                let assigned = se::assign_arm(issue, complexity.as_deref());
                arm = Some(assigned);
                model = se::arm_model(assigned);
                let (_ok, source, _w) = se::evaluate_canary_default(env_canary.as_deref(), None);
                canary_source = source.map(|s| s.label().to_string());
            } else if raw_mode == "experiment" {
                // Requested experiment but downgraded to observe — canary
                // unconfirmed.
                canary_source = Some("unconfirmed".to_string());
            }
            println!(
                "{}",
                se::format_banner(
                    &mode,
                    issue,
                    arm,
                    if model.is_empty() {
                        None
                    } else {
                        Some(model.as_str())
                    },
                    canary_source.as_deref(),
                )
            );
            Ok(())
        }
        SweepExperimentAction::Record {
            issue,
            phase,
            role,
            model,
            mode,
            arm,
            attempt,
            complexity,
            verdict,
            cycle_count,
            pr,
            effort,
            agent_id,
            transcript,
            in_tok,
            out_tok,
            token_fidelity,
            stats_file,
            quiet,
        } => {
            let record = se::build_record(
                &se::RecordFields {
                    issue,
                    phase: &phase,
                    role: &role,
                    model: model.as_deref(),
                    mode: &mode,
                    arm: arm.as_deref(),
                    attempt,
                    complexity: complexity.as_deref(),
                    judge_verdict: verdict.as_deref(),
                    cycle_count,
                    pr,
                    effort: effort.as_deref(),
                    agent_id: agent_id.as_deref(),
                    transcript: transcript.as_deref(),
                    in_tok,
                    out_tok,
                    token_fidelity: &token_fidelity,
                },
                &script_helpers::now_iso(),
            );
            se::append_record(&record, stats_file.as_deref())?;
            if !quiet {
                println!("{record}");
            }
            Ok(())
        }
        SweepExperimentAction::Harvest {
            stats_file,
            archive_dir,
            format,
        } => {
            let raw = archive_dir.or_else(|| std::env::var("LOOM_TRANSCRIPT_ARCHIVE").ok());
            let archive = se::normalize_archive_dir(raw.as_deref());
            let report = se::harvest(stats_file.as_deref(), archive.as_deref());
            if format == "json" {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else {
                println!("{}", se::format_harvest_text(&report));
            }
            Ok(())
        }
    }
}

fn handle_validate_command(
    workspace: &str,
    format: &str,
    strict: bool,
    verbose: bool,
) -> Result<()> {
    use loom_daemon::config_resolver;
    use role_validation::{format_validation_result, validate_from_config, ValidationMode};

    let workspace_path = std::path::Path::new(workspace);
    let absolute_workspace = if workspace_path.is_absolute() {
        workspace_path.to_path_buf()
    } else {
        std::env::current_dir()?.join(workspace_path)
    };

    // #4059: resolve the effective config through the full tier chain rather
    // than reading the legacy `.loom/config.json` directly. Under tiering,
    // "the legacy file is absent" no longer implies "there is no config" — a
    // repo may configure `terminals` entirely from `.loom-project/project.json`
    // or `.loom-local/local.json`.
    let config = config_resolver::resolve_effective_config(&absolute_workspace);

    // The tiers searched, in precedence order — named in every "not found" error
    // (Finding 5: both text and json branches).
    let mut searched_tiers: Vec<String> = Vec::new();
    if let Some(defaults) = config_resolver::private_defaults_path() {
        searched_tiers.push(defaults.display().to_string());
    }
    searched_tiers.push(
        absolute_workspace
            .join(config_resolver::LEGACY_CONFIG_REL)
            .display()
            .to_string(),
    );
    searched_tiers.push(
        absolute_workspace
            .join(config_resolver::PROJECT_CONFIG_REL)
            .display()
            .to_string(),
    );
    searched_tiers.push(
        absolute_workspace
            .join(config_resolver::LOCAL_CONFIG_REL)
            .display()
            .to_string(),
    );

    // Retargeted precondition (#4059) — stated verbatim for #4062 to mirror:
    //   "A workspace is validatable iff resolve_effective_config(workspace)
    //    yields a `terminals` array with at least one element; otherwise the
    //    command exits 1, naming every tier it searched."
    let has_terminals = config
        .get("terminals")
        .and_then(serde_json::Value::as_array)
        .is_some_and(|arr| !arr.is_empty());

    if !has_terminals {
        if format == "json" {
            let err = serde_json::json!({
                "error": "No Loom config with a non-empty terminals array found in any tier",
                "searched": searched_tiers,
            });
            println!("{}", serde_json::to_string(&err)?);
        } else {
            eprintln!(
                "Error: No Loom config with a non-empty `terminals` array found in any tier."
            );
            eprintln!("\nSearched (lowest to highest precedence):");
            for tier in &searched_tiers {
                eprintln!("  - {tier}");
            }
            eprintln!("\nMake sure you're in a Loom workspace or specify the path:");
            eprintln!("  loom-daemon validate /path/to/workspace");
        }
        std::process::exit(1);
    }

    let mode = if strict {
        ValidationMode::Strict
    } else {
        ValidationMode::Warn
    };

    let result = validate_from_config(&config, mode);

    if format == "json" {
        println!("{}", serde_json::to_string_pretty(&result)?);
    } else {
        if verbose {
            println!("\nValidating role configuration...");
            println!("  Workspace: {}", absolute_workspace.display());
            println!();
        }

        let output = format_validation_result(&result, verbose);
        if !output.is_empty() {
            print!("{output}");
        }

        if result.warnings.is_empty() && result.errors.is_empty() {
            println!("All role dependencies are satisfied.");
        }
    }

    if !result.errors.is_empty() {
        std::process::exit(1);
    } else if !result.warnings.is_empty() && strict {
        std::process::exit(2);
    }

    Ok(())
}

#[cfg(test)]
mod dispatch_tests {
    //! Tests for the `loom-daemon dispatch` subcommand (Issue #3952): flag
    //! plumbing into the `DispatchSweep` IPC request, a successful round-trip
    //! against a fake daemon, and the bounded-timeout path against a
    //! deliberately-unresponsive socket (the #3945 wedge must never hang).
    use super::{
        build_dispatch_request, classify_gate_verdict, format_gate_status, gate_status_short_label,
        query_daemon_bounded, resolve_cli_dispatch_workspace, resolve_dispatch_ack_timeout,
        GateVerdict, DAEMON_IPC_TIMEOUT_ENV, DISPATCH_ACK_TIMEOUT,
    };
    use chrono::{DateTime, Utc};
    use loom_daemon::types::{Request, Response, SweepKind};
    use serial_test::serial;
    use std::time::Duration;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::UnixListener;

    /// `build_dispatch_request` maps every flag into the right `DispatchSweep`
    /// field — the core plumbing acceptance criterion.
    #[test]
    fn build_dispatch_request_plumbs_all_flags() {
        let request = build_dispatch_request(
            3952,
            Some("/some/repo".to_string()),
            Some("sonnet".to_string()),
            Some("high".to_string()),
            Some(3945),
            true,
        );
        match request {
            Request::DispatchSweep {
                kind,
                idempotency_key,
                model,
                effort,
                depends_on,
                workspace_root,
                force,
            } => {
                assert_eq!(kind, SweepKind::Issue(3952));
                assert_eq!(idempotency_key, None);
                assert_eq!(model.as_deref(), Some("sonnet"));
                assert_eq!(effort.as_deref(), Some("high"));
                assert_eq!(depends_on, Some(3945));
                assert_eq!(workspace_root.as_deref(), Some("/some/repo"));
                assert!(force, "--force must plumb through to the request");
            }
            other => panic!("expected DispatchSweep, got {other:?}"),
        }
    }

    /// With no optional flags the request carries only the issue kind and leaves
    /// every override `None`, so the daemon applies its own defaults.
    #[test]
    fn build_dispatch_request_defaults_are_none() {
        let request = build_dispatch_request(42, None, None, None, None, false);
        match request {
            Request::DispatchSweep {
                kind,
                model,
                effort,
                depends_on,
                workspace_root,
                force,
                ..
            } => {
                assert_eq!(kind, SweepKind::Issue(42));
                assert!(model.is_none());
                assert!(effort.is_none());
                assert!(depends_on.is_none());
                assert!(workspace_root.is_none());
                assert!(!force, "force defaults to false");
            }
            other => panic!("expected DispatchSweep, got {other:?}"),
        }
    }

    // ===== `resolve_cli_dispatch_workspace` (Issue #4299) =====

    /// An explicit `--workspace` always wins, regardless of cwd/registry state.
    #[test]
    fn resolve_cli_dispatch_workspace_explicit_always_wins() {
        let registry = loom_daemon::workspace_registry::WorkspaceRegistry::default();
        let resolved = resolve_cli_dispatch_workspace(
            Some("/explicit/repo".to_string()),
            std::path::Path::new("/somewhere/else"),
            &registry,
        );
        assert_eq!(resolved.as_deref(), Some("/explicit/repo"));
    }

    /// Issue #4299's core CLI fix: running `loom-daemon dispatch <N>` from
    /// inside a registered repo (no `--workspace` flag) must default
    /// `workspace_root` to that repo's root.
    #[test]
    fn resolve_cli_dispatch_workspace_defaults_from_cwd_inside_registered_root() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();

        let mut registry = loom_daemon::workspace_registry::WorkspaceRegistry::default();
        registry.add(&repo, None).unwrap();

        let canonical_repo = std::fs::canonicalize(&repo).unwrap();
        let resolved = resolve_cli_dispatch_workspace(None, &repo, &registry);
        assert_eq!(resolved, Some(canonical_repo.to_string_lossy().into_owned()));
    }

    /// A cwd outside every registered root (or an empty registry) leaves the
    /// result `None` — the daemon's own registry-based resolution applies.
    #[test]
    fn resolve_cli_dispatch_workspace_none_when_cwd_unregistered() {
        let dir = tempfile::tempdir().unwrap();
        let registry = loom_daemon::workspace_registry::WorkspaceRegistry::default();
        let resolved = resolve_cli_dispatch_workspace(None, dir.path(), &registry);
        assert_eq!(resolved, None);
    }

    /// A fake daemon that accepts one connection, verifies the received request
    /// is the expected `DispatchSweep`, and replies with `SweepDispatched`. This
    /// exercises the full client round-trip: request serialization + flag
    /// plumbing on the wire + response parsing.
    #[tokio::test]
    async fn round_trip_parses_dispatched_response_and_forwards_flags() {
        let dir = tempfile::tempdir().expect("tempdir");
        let socket_path = dir.path().join("daemon.sock");
        let listener = UnixListener::bind(&socket_path).expect("bind");

        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accept");
            let (reader, mut writer) = stream.into_split();
            let mut lines = BufReader::new(reader).lines();
            let line = lines.next_line().await.expect("read").expect("line");
            let request: Request = serde_json::from_str(&line).expect("parse request");
            // Assert the flags arrived intact on the wire.
            match request {
                Request::DispatchSweep {
                    kind,
                    model,
                    effort,
                    depends_on,
                    workspace_root,
                    ..
                } => {
                    assert_eq!(kind, SweepKind::Issue(3952));
                    assert_eq!(model.as_deref(), Some("sonnet"));
                    assert_eq!(effort.as_deref(), Some("high"));
                    assert_eq!(depends_on, Some(3945));
                    assert_eq!(workspace_root.as_deref(), Some("/repo"));
                }
                other => panic!("expected DispatchSweep, got {other:?}"),
            }
            let response = Response::SweepDispatched {
                sweep_id: "sweep-abc".to_string(),
                pid: 12345,
                token_name: "agent-2.token".to_string(),
                log_path: std::path::PathBuf::from(".loom/logs/sweep-issue-3952.log"),
            };
            let json = serde_json::to_string(&response).expect("serialize");
            writer.write_all(json.as_bytes()).await.expect("write");
            writer.write_all(b"\n").await.expect("newline");
            writer.flush().await.expect("flush");
        });

        let request = build_dispatch_request(
            3952,
            Some("/repo".to_string()),
            Some("sonnet".to_string()),
            Some("high".to_string()),
            Some(3945),
            false,
        );
        let response = query_daemon_bounded(&socket_path, &request, Duration::from_secs(5))
            .await
            .expect("round-trip ok");

        match response {
            Response::SweepDispatched {
                sweep_id,
                pid,
                token_name,
                log_path,
            } => {
                assert_eq!(sweep_id, "sweep-abc");
                assert_eq!(pid, 12345);
                assert_eq!(token_name, "agent-2.token");
                assert_eq!(log_path, std::path::PathBuf::from(".loom/logs/sweep-issue-3952.log"));
            }
            other => panic!("expected SweepDispatched, got {other:?}"),
        }

        server.await.expect("server task");
    }

    /// An unresponsive daemon — accepts the connection but never replies —
    /// must trip the bounded round-trip timeout rather than hang (the #3945
    /// failure mode). The client returns an `Err` well before any 1800s wedge.
    #[tokio::test]
    async fn unresponsive_socket_times_out() {
        let dir = tempfile::tempdir().expect("tempdir");
        let socket_path = dir.path().join("daemon.sock");
        let listener = UnixListener::bind(&socket_path).expect("bind");

        // Accept the connection but deliberately never write a response, holding
        // the stream open so the client's read blocks until its own timeout.
        let server = tokio::spawn(async move {
            let (_stream, _) = listener.accept().await.expect("accept");
            tokio::time::sleep(Duration::from_secs(30)).await;
        });

        let request = build_dispatch_request(3952, None, None, None, None, false);
        let started = std::time::Instant::now();
        let result = query_daemon_bounded(&socket_path, &request, Duration::from_millis(200)).await;
        let elapsed = started.elapsed();

        assert!(result.is_err(), "expected a timeout error, got {result:?}");
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("timed out"), "error should mention the timeout, got: {msg}");
        // It must return promptly (well under a second), never hang.
        assert!(elapsed < Duration::from_secs(2), "timeout took too long: {elapsed:?}");

        server.abort();
    }

    /// A missing socket (no daemon listening at all) surfaces a connect error
    /// promptly — the operator's "is the daemon running?" case.
    #[tokio::test]
    async fn absent_socket_errors_fast() {
        let dir = tempfile::tempdir().expect("tempdir");
        let socket_path = dir.path().join("nonexistent.sock");

        let request = build_dispatch_request(3952, None, None, None, None, false);
        let result = query_daemon_bounded(&socket_path, &request, Duration::from_millis(500)).await;

        assert!(result.is_err(), "expected a connect error, got {result:?}");
    }

    /// The documented middle case (issue #3952 review): a **legitimate, slow**
    /// dispatch. The daemon does real synchronous work before acking — a
    /// `gh issue edit` label flip, up to a 2s dispatch stagger, and up to a 5s
    /// token-name capture window — so a successful ack can land well past the
    /// old hardcoded 5s client bound. This fake daemon sleeps *past* that old
    /// 5s bound before replying `SweepDispatched`, and the CLI's real default
    /// budget ([`DISPATCH_ACK_TIMEOUT`], 30s) must still parse it as the success
    /// it is — never misreport a real dispatch as "did not ack". Neither the
    /// instant-ack round-trip nor the permanently-unresponsive stub exercises
    /// this path.
    #[tokio::test]
    async fn slow_but_legitimate_ack_succeeds() {
        // Sleep beyond the *old* 5s hardcoded bound so this test would have
        // false-failed before the widening, proving the regression is fixed.
        const SLOW_ACK: Duration = Duration::from_millis(5_500);

        let dir = tempfile::tempdir().expect("tempdir");
        let socket_path = dir.path().join("daemon.sock");
        let listener = UnixListener::bind(&socket_path).expect("bind");

        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accept");
            let (reader, mut writer) = stream.into_split();
            let mut lines = BufReader::new(reader).lines();
            let _ = lines.next_line().await.expect("read").expect("line");
            // Model the daemon's synchronous pre-ack work (label flip + stagger +
            // token-name poll) taking longer than the retired 5s client bound.
            tokio::time::sleep(SLOW_ACK).await;
            let response = Response::SweepDispatched {
                sweep_id: "sweep-slow".to_string(),
                pid: 4242,
                token_name: "agent-1.token".to_string(),
                log_path: std::path::PathBuf::from(".loom/logs/sweep-issue-3952.log"),
            };
            let json = serde_json::to_string(&response).expect("serialize");
            writer.write_all(json.as_bytes()).await.expect("write");
            writer.write_all(b"\n").await.expect("newline");
            writer.flush().await.expect("flush");
        });

        let request = build_dispatch_request(3952, None, None, None, None, false);
        let started = std::time::Instant::now();
        // Use the CLI's real default budget — the exact value a plain
        // `loom-daemon dispatch` resolves to with no env override.
        let result = query_daemon_bounded(&socket_path, &request, DISPATCH_ACK_TIMEOUT).await;
        let elapsed = started.elapsed();

        match result {
            Ok(Response::SweepDispatched { sweep_id, .. }) => {
                assert_eq!(sweep_id, "sweep-slow");
            }
            other => panic!("expected a slow-but-successful SweepDispatched, got {other:?}"),
        }
        // The daemon genuinely took longer than the retired 5s bound — this
        // asserts the test exercised the slow path rather than acking instantly.
        assert!(
            elapsed >= Duration::from_secs(5),
            "slow-ack test should have waited past the old 5s bound, took: {elapsed:?}"
        );

        server.await.expect("server task");
    }

    /// The 30s floor is the default when the override env var is absent — real
    /// headroom over the daemon's documented worst-case internal dispatch budget.
    #[test]
    #[serial]
    fn resolve_dispatch_ack_timeout_defaults_to_floor() {
        std::env::remove_var(DAEMON_IPC_TIMEOUT_ENV);
        assert_eq!(resolve_dispatch_ack_timeout(), DISPATCH_ACK_TIMEOUT);
        assert_eq!(DISPATCH_ACK_TIMEOUT, Duration::from_secs(30));
    }

    /// `LOOM_DAEMON_IPC_TIMEOUT_MS` mirrors `mcp-loom`'s `Math.max` semantics:
    /// it can only *raise* the bound above the 30s floor (never lower it, which
    /// would reintroduce the false-negative), and any invalid / non-positive
    /// value falls back to the floor. A single test owns this env var so its
    /// mutation never races a parallel reader.
    #[test]
    #[serial]
    fn resolve_dispatch_ack_timeout_env_raises_only() {
        // Above the floor → raised.
        std::env::set_var(DAEMON_IPC_TIMEOUT_ENV, "60000");
        assert_eq!(resolve_dispatch_ack_timeout(), Duration::from_secs(60));

        // Below the floor → clamped up to the 30s floor (never lowered).
        std::env::set_var(DAEMON_IPC_TIMEOUT_ENV, "1000");
        assert_eq!(resolve_dispatch_ack_timeout(), DISPATCH_ACK_TIMEOUT);

        // Zero / negative / non-numeric / empty → floor.
        for bad in ["0", "-5", "garbage", "", "  "] {
            std::env::set_var(DAEMON_IPC_TIMEOUT_ENV, bad);
            assert_eq!(
                resolve_dispatch_ack_timeout(),
                DISPATCH_ACK_TIMEOUT,
                "value {bad:?} should fall back to the floor"
            );
        }

        std::env::remove_var(DAEMON_IPC_TIMEOUT_ENV);
    }

    // ===================================================================
    // Main-health gate status line (#3950 AC3 shape, #3974 AC2 cause)
    // ===================================================================

    /// Build a [`GateVerdict`] the same way `print_status_human` /
    /// `print_status_json` do, so these tests exercise the real classification
    /// path rather than constructing variants by hand.
    fn classify(
        enabled: Option<bool>,
        halted: bool,
        not_evaluated: bool,
        reason: Option<&str>,
        verdict_at: Option<DateTime<Utc>>,
    ) -> GateVerdict {
        // The 5-arg helper keeps the pre-#4259 test call sites unchanged: no
        // deferral, no tier label. Dedicated tests below exercise those paths
        // by calling `classify_gate_verdict` directly.
        classify_gate_verdict(enabled, halted, not_evaluated, false, reason, None, None, verdict_at)
    }

    #[test]
    fn format_gate_status_names_the_actual_not_evaluated_cause() {
        // Pre-#3974 this line asserted "workspace tree is dirty" for EVERY
        // skip, so a `git fetch` failure on a completely clean tree was
        // reported as a dirty tree. The cause is now passed through verbatim.
        let v = classify(
            Some(true),
            false,
            true,
            Some("git-failure: `git -C /repo fetch origin main` failed (exit 128)"),
            None,
        );
        let s = format_gate_status(&v);
        assert!(s.contains("git-failure"), "got: {s}");
        assert!(s.contains("exit 128"), "got: {s}");
        assert!(!s.contains("dirty"), "must not assume a dirty tree: {s}");
        assert!(s.contains("NOT evidence about"), "got: {s}");
        assert!(s.contains("NOT halted"), "an unevaluated gate does not halt: {s}");

        // A dirty tree still reads as a dirty tree — because the gate said so.
        let v = classify(Some(true), false, true, Some("dirty-tree: [ M src/main.rs]"), None);
        let s = format_gate_status(&v);
        assert!(s.contains("dirty-tree"), "got: {s}");
        assert!(s.contains("src/main.rs"), "got: {s}");
    }

    #[test]
    fn format_gate_status_covers_all_halted_and_not_evaluated_states() {
        let clear = classify(Some(true), false, false, None, Some(Utc::now()));
        assert!(format_gate_status(&clear).starts_with("clear (dispatch allowed"));

        let halted = classify(Some(true), true, false, None, None);
        let s = format_gate_status(&halted);
        assert!(s.starts_with("HALTED"), "got: {s}");
        assert!(s.contains("verified red"), "got: {s}");

        // Both at once: a prior verified-red halt persists while the next tick
        // cannot evaluate.
        let both = classify(Some(true), true, true, Some("timeout: gate command timed out"), None);
        let s = format_gate_status(&both);
        assert!(s.contains("HALTED"), "got: {s}");
        assert!(s.contains("NOT EVALUATED"), "got: {s}");
        assert!(s.contains("timeout"), "got: {s}");

        // A missing cause degrades gracefully rather than inventing one.
        let no_cause = classify(Some(true), false, true, None, None);
        let s = format_gate_status(&no_cause);
        assert!(s.contains("cause unrecorded"), "got: {s}");
    }

    /// #4012: the core regression this issue fixes — a fresh, enabled gate
    /// that has never completed an evaluation must render distinctly from a
    /// verified-green gate, even though both allow dispatch (`halted: false`,
    /// `not_evaluated: false` in both cases pre-#4012).
    #[test]
    fn format_gate_status_distinguishes_pending_from_clear() {
        let pending = classify(Some(true), false, false, None, None);
        assert_eq!(pending, GateVerdict::Pending);
        let s = format_gate_status(&pending);
        assert!(s.starts_with("pending"), "got: {s}");
        assert!(s.contains("dispatch allowed"), "got: {s}");
        assert!(!s.contains("clear"), "must not read as verified-green: {s}");

        let now = Utc::now();
        let clear = classify(Some(true), false, false, None, Some(now));
        assert_eq!(
            clear,
            GateVerdict::Clear {
                since: Some(now),
                tier: None
            }
        );
        let s = format_gate_status(&clear);
        assert!(s.starts_with("clear"), "got: {s}");
        assert!(s.contains(&now.to_rfc3339()), "clear must carry its own recency evidence: {s}");
        assert_ne!(
            format_gate_status(&pending),
            format_gate_status(&clear),
            "pending and clear must never render identically"
        );
    }

    /// #4259: a load-deferral is a distinct verdict — it must render as
    /// `deferred (…)`, never as `not evaluated (timeout …)` nor as a stale
    /// `clear`, and it must never halt dispatch.
    #[test]
    fn format_gate_status_deferred_is_distinct_and_never_halts() {
        let deferred = classify_gate_verdict(
            Some(true),
            false, // not halted
            false, // not unevaluated
            true,  // deferred
            None,
            Some("load 1.05/core for 14m — fast tier runs at the 30m bound"),
            None,
            None,
        );
        assert!(matches!(deferred, GateVerdict::Deferred { .. }));
        let s = format_gate_status(&deferred);
        assert!(s.starts_with("deferred"), "got: {s}");
        assert!(s.contains("load 1.05/core"), "carries the load reason: {s}");
        assert!(s.contains("NOT evidence about main"), "got: {s}");
        assert!(s.contains("NOT halted"), "a deferred gate does not halt: {s}");
        // Distinct from a timeout not-evaluated line for the same host stress.
        let timeout = classify(
            Some(true),
            false,
            true,
            Some("timeout: gate command timed out after 1200s"),
            None,
        );
        assert_ne!(
            format_gate_status(&deferred),
            format_gate_status(&timeout),
            "deferred (load) must never render identically to not-evaluated (timeout)"
        );
        assert_eq!(gate_status_short_label(&deferred), "deferred");
    }

    /// #4259: a fast-tier green must be labeled so it is never mistaken for a
    /// full-suite green.
    #[test]
    fn format_gate_status_fast_tier_clear_is_labeled() {
        let now = Utc::now();
        let full = classify_gate_verdict(
            Some(true),
            false,
            false,
            false,
            None,
            None,
            Some("full"),
            Some(now),
        );
        let fast = classify_gate_verdict(
            Some(true),
            false,
            false,
            false,
            None,
            None,
            Some("fast"),
            Some(now),
        );
        let full_s = format_gate_status(&full);
        let fast_s = format_gate_status(&fast);
        assert!(full_s.starts_with("clear"), "got: {full_s}");
        assert!(!full_s.contains("fast tier"), "full tier is unlabeled: {full_s}");
        assert!(fast_s.contains("fast tier"), "fast tier is labeled: {fast_s}");
        assert!(
            fast_s.contains("NOT a full-suite green"),
            "the fast-tier caveat is explicit: {fast_s}"
        );
        assert_eq!(gate_status_short_label(&full), "clear");
        assert_eq!(gate_status_short_label(&fast), "clear(fast)");
        // The short label still fits the 13-char table column.
        assert!(gate_status_short_label(&fast).len() <= 13);
    }

    /// #4012 AC2: the gate-disabled case must be distinguishable from both
    /// `pending` and `clear`.
    #[test]
    fn format_gate_status_distinguishes_disabled() {
        let disabled = classify(Some(false), false, false, None, None);
        assert_eq!(disabled, GateVerdict::Disabled);
        let s = format_gate_status(&disabled);
        assert!(s.starts_with("disabled"), "got: {s}");
        assert!(s.contains("dispatch allowed"), "got: {s}");

        let pending = classify(Some(true), false, false, None, None);
        assert_ne!(
            format_gate_status(&disabled),
            format_gate_status(&pending),
            "disabled and pending must never render identically"
        );

        // A disabled root that (implausibly) still carries a stale verdict
        // timestamp from before it was turned off still reports `Disabled` —
        // the enabled flag takes priority over verdict presence.
        let disabled_with_stale_verdict =
            classify(Some(false), false, false, None, Some(Utc::now()));
        assert_eq!(disabled_with_stale_verdict, GateVerdict::Disabled);
    }

    /// #4012 AC3: `pending` and `disabled` both still allow dispatch —
    /// observability-only, no new halt path.
    #[test]
    fn pending_and_disabled_never_halt() {
        for verdict in [
            classify(Some(true), false, false, None, None),
            classify(Some(false), false, false, None, None),
        ] {
            assert!(
                !matches!(verdict, GateVerdict::Halted { .. }),
                "{verdict:?} must never be classified as halted"
            );
            let s = format_gate_status(&verdict);
            assert!(s.contains("dispatch allowed"), "got: {s}");
        }
    }

    /// An older daemon that never populated `main_health_gate_enabled` (wire
    /// field absent ⇒ `None`, #4012) must not be misread as "disabled" — that
    /// is exactly the `bool::default() == false` trap the `Option<bool>` wire
    /// type exists to avoid.
    #[test]
    fn format_gate_status_legacy_none_enabled_is_not_disabled() {
        let v = classify(None, false, false, None, None);
        assert_ne!(v, GateVerdict::Disabled);
        // With no verdict either, it reads as pending (dispatch allowed) —
        // the conservative reading, never a fabricated "clear".
        assert_eq!(v, GateVerdict::Pending);
    }

    /// `halted`/`not_evaluated` always win over disabled/pending, matching the
    /// gate loop's own soft-fail contract (its disabled path always clears
    /// `halted` first) — this combination should only ever arise from a test
    /// poking the raw state directly, and the renderer must still surface it
    /// as halted rather than silently downgrading to "disabled".
    #[test]
    fn format_gate_status_halted_beats_disabled_and_pending() {
        let v = classify(Some(false), true, false, None, None);
        assert!(matches!(
            v,
            GateVerdict::Halted {
                not_evaluated: false,
                ..
            }
        ));
    }

    #[test]
    fn gate_status_short_label_fits_table_width_and_matches_long_form() {
        let cases = [
            classify(Some(false), false, false, None, None),
            classify(Some(true), false, false, None, None),
            classify(Some(true), false, false, None, Some(Utc::now())),
            classify(Some(true), false, true, Some("timeout"), None),
            classify(Some(true), true, false, None, None),
            classify(Some(true), true, true, Some("timeout"), None),
        ];
        for v in cases {
            let short = gate_status_short_label(&v);
            assert!(short.len() <= 13, "{short:?} exceeds the 13-char GATE column");
        }
        // The short label and long form must agree on the halted/not distinction.
        let halted = classify(Some(true), true, false, None, None);
        assert_eq!(gate_status_short_label(&halted), "HALTED");
        assert!(format_gate_status(&halted).starts_with("HALTED"));
    }
}

#[cfg(test)]
mod status_client_tests {
    //! Tests for the `loom-daemon status` IPC client (Issue #4279): the silent
    //! empty-output failure mode. A daemon under concurrent-sweep load could
    //! accept a `status` connection and drop it with zero bytes written; the
    //! client surfaced that as a bare EOF. These tests lock in the two
    //! invariants: (1) an EOF (accept-then-close) yields a non-zero-worthy
    //! `Err` with a diagnostic — never an empty success — and (2) a single
    //! reconnect retry absorbs a transient first-connection drop.
    use super::query_daemon_status;
    use chrono::Utc;
    use loom_daemon::types::{
        CapacityReport, CredentialPreflightReport, DaemonStatusReport, Response,
    };
    use std::time::Duration;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::UnixListener;

    /// A fully-populated report the fake daemon can serialize back to the client
    /// on a successful round-trip. Every field is compiler-checked, so a schema
    /// change surfaces here rather than as a silently-skewed wire payload.
    fn sample_report() -> DaemonStatusReport {
        DaemonStatusReport {
            in_flight: vec![],
            unregistered_locked: vec![],
            token_pool_size: 4,
            token_pool_dir: Some(std::path::PathBuf::from("/repo/a/.loom/tokens")),
            disk_headroom: 10,
            cpu_headroom: 6,
            logical_cpus: 8,
            loadavg_1m: Some(1.25),
            cpu_idle_fraction: Some(0.90),
            capacity_bound: false,
            preflight_advisory_active: false,
            preflight_advisory_message: None,
            configured_max: 5,
            per_token_concurrency: 2,
            dynamic_cap: 3,
            main_health_gate_halted: false,
            main_health_gate_not_evaluated: false,
            main_health_gate_not_evaluated_reason: None,
            main_health_gate_enabled: Some(true),
            main_health_gate_verdict_at: Some(Utc::now()),
            main_health_gate_deferred: false,
            main_health_gate_deferred_reason: None,
            main_health_gate_verdict_tier: None,
            capacity: CapacityReport {
                ranking_present: true,
                total_accounts: 4,
                healthy_accounts: 3,
                exhausted_accounts: 1,
                token_axis_limit: 3,
                token_bound: true,
            },
            per_repo: vec![],
            credential_preflight: Some(CredentialPreflightReport {
                ok: true,
                mechanism: "test-fixture".to_string(),
                fingerprint: None,
                message: "test fixture — not a real preflight".to_string(),
                checked_at: Utc::now(),
            }),
            draining: false,
            drain_deadline: None,
            drain_note: None,
            auto_update_enabled: false,
            auto_update_last_check: None,
            auto_update_last_roll: None,
            auto_update_consecutive_failures: 0,
            auto_update_backoff_secs: None,
            auto_update_terminal_reason: None,
            auto_update_note: None,
            host_breaker: None,
            rate_limit_breaker: None,
            safehouse: None,
        }
    }

    /// The core #4279 invariant: a daemon that accepts the connection and closes
    /// it before writing any response byte (the silent-EOF failure mode) must
    /// surface an `Err` with a diagnostic — never an empty "successful" report.
    /// The client retries once, so the fake server drops BOTH connections.
    #[tokio::test]
    async fn status_eof_yields_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let socket_path = dir.path().join("daemon.sock");
        let listener = UnixListener::bind(&socket_path).expect("bind");

        // Accept and immediately drop every connection (the initial attempt plus
        // the one bounded reconnect retry).
        let server = tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                drop(stream);
            }
        });

        let started = std::time::Instant::now();
        let result = query_daemon_status(&socket_path).await;
        let elapsed = started.elapsed();

        assert!(result.is_err(), "accept-then-close must be an error, not an empty success");
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("closed the connection without responding"),
            "error should name the dropped connection, got: {msg}"
        );
        // Both attempts hit an immediate EOF, so this returns promptly — the
        // reconnect retry never stretches into the 5s round-trip budget.
        assert!(elapsed < Duration::from_secs(2), "EOF path took too long: {elapsed:?}");

        server.abort();
    }

    /// The single-reconnect acceptance criterion: the daemon drops the first
    /// `status` connection (transient contention) but answers the second. The
    /// client's one bounded retry must absorb the drop and return the report.
    #[tokio::test]
    async fn status_retry_succeeds_after_transient_drop() {
        let dir = tempfile::tempdir().expect("tempdir");
        let socket_path = dir.path().join("daemon.sock");
        let listener = UnixListener::bind(&socket_path).expect("bind");

        let server = tokio::spawn(async move {
            // First connection: accept and drop without replying.
            let (first, _) = listener.accept().await.expect("accept #1");
            drop(first);

            // Second connection: read the request line, then reply with a valid
            // DaemonStatus frame.
            let (stream, _) = listener.accept().await.expect("accept #2");
            let (reader, mut writer) = stream.into_split();
            let mut lines = BufReader::new(reader).lines();
            let _ = lines
                .next_line()
                .await
                .expect("read")
                .expect("request line");
            let json = serde_json::to_string(&Response::DaemonStatus(Box::new(sample_report())))
                .expect("serialize response");
            writer.write_all(json.as_bytes()).await.expect("write");
            writer.write_all(b"\n").await.expect("newline");
            writer.flush().await.expect("flush");
        });

        let result = query_daemon_status(&socket_path).await;
        match result {
            Ok(report) => assert_eq!(report.token_pool_size, 4),
            Err(e) => panic!("retry should have absorbed the first-connection drop, got: {e}"),
        }

        server.await.expect("server task");
    }

    /// A missing socket (no daemon listening at all) must fail fast WITHOUT a
    /// reconnect retry — a clean "socket absent" is not the transient case, so
    /// retrying would only fail twice for no benefit.
    #[tokio::test]
    async fn status_absent_socket_errors_fast() {
        let dir = tempfile::tempdir().expect("tempdir");
        let socket_path = dir.path().join("nonexistent.sock");

        let started = std::time::Instant::now();
        let result = query_daemon_status(&socket_path).await;
        let elapsed = started.elapsed();

        assert!(result.is_err(), "expected a connect error for an absent socket");
        assert!(
            elapsed < Duration::from_secs(2),
            "absent-socket path took too long: {elapsed:?}"
        );
    }
}
