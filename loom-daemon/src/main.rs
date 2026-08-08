use loom_daemon::script_helpers;
use loom_daemon::serve;
use loom_daemon::worktree_ops::{aggressive, clean};

use anyhow::Result;
use clap::{Parser, Subcommand};
use std::io::Write;
use std::path::PathBuf;

mod cli;
mod daemon_service;

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
// tooling, which is loud but harmless. The composed string lives in the lib
// crate (`self_update::BUILD_IDENTITY`) so library code that must name the
// deciding binary — e.g. the empty-token-pool error (#4643) — reports exactly
// what `--version` reports.
//
// IMPORTANT (Issue #5341): `--version` reports the ON-DISK binary ONLY, never
// the running daemon PROCESS's build. Clap intercepts `--version` before any
// subcommand logic runs — no IPC round-trip happens, and none is added here
// on purpose: `--version` is also the only way to inspect a binary that has
// no daemon running yet (e.g. before the first `loom-daemon-start.sh`), so it
// cannot assume a live process to query. A long-running daemon PROCESS that
// predates a since-rebuilt disk binary will therefore answer `--version` with
// the NEWER disk build, not its own older one — the exact stale-daemon
// blind spot this issue exists for. Use `loom-daemon status` instead when you
// need the RUNNING process's own build: its `Build: …` line (and the
// `daemon_build` block under `--json`) compares the two explicitly and warns
// when they differ, sourcing the running build over IPC from the process
// itself rather than re-reading the file on disk.
#[command(version = loom_daemon::self_update::BUILD_IDENTITY)]
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
        /// #3977): open, dispatchable `loom:issue` (queued — park-labeled
        /// rows excluded, #4825), open `loom:building`
        /// (claimed), open PRs by `loom:review-requested` /
        /// `loom:changes-requested` / `loom:pr`, and PRs merged in the last
        /// 24h. Opt-in because it makes several `gh` calls per managed repo
        /// (client-side, after the fast IPC round-trip) rather than being
        /// bundled into the default view.
        #[arg(long)]
        pipeline: bool,
    },

    /// One-shot consolidated fleet vitals with an exit-code contract for watch
    /// loops (Issue #4761): trusted liveness, dispatch state, token pool,
    /// role-tick health, queue depth, and merge throughput — one structured
    /// line per section, or `--json` for machine consumers.
    ///
    /// Exit codes: `0` healthy, `1` degraded (any section non-green, including
    /// "could not determine"), `2` the daemon is genuinely dead. A watch loop
    /// can therefore branch without parsing anything.
    ///
    /// The liveness verdict is **pgrep + pid-file first** — the launchd domain
    /// probe is never trusted alone (#4694: it twice declared a live,
    /// dispatching daemon dead). A DEAD verdict requires all three independent
    /// signals (IPC, launchd/pid-file classification, `pgrep`) to agree.
    ///
    /// Not every section is trustworthy from the same vantage point (#5061):
    /// `liveness`/`dispatch`/`tokens`/`roles`/`observability` are
    /// daemon-authoritative (sourced from the daemon's own IPC round-trip or
    /// a local probe of this host), so they read the same over SSH as
    /// locally. `queues`/`throughput` instead run `gh` calls in THIS
    /// process — a `gh` missing from a non-login shell's `PATH` (this
    /// process's `PATH`, not the daemon's) reports as one distinct fact
    /// rather than a per-repo forge-query failure, and cross-references the
    /// daemon's own `credential_preflight` verdict when available.
    Health {
        /// Window for the role-tick and throughput sections: `30m` (default),
        /// `2h`, `90s`, `1d`, or a bare number of seconds.
        #[arg(long, value_name = "WINDOW")]
        since: Option<String>,

        /// Emit the machine-readable JSON report instead of the section lines.
        #[arg(long)]
        json: bool,
    },

    /// Measure the host and the currently-resolved concurrency knobs (issue
    /// #4390; measurement-only since #4512). Prints the same `min(token axis,
    /// disk, maxConcurrent)` cap breakdown `status` uses, which term binds, and
    /// a one-line reading of whether this machine's `maxConcurrent` should go
    /// up. Purely file/host-based; does not require a running daemon (unlike
    /// `status`, which reports the running daemon's own in-memory dispatch
    /// state). Always read-only.
    Calibrate {
        /// Repo root to measure (plain path, default `.` — no upward `.git` walk).
        #[arg(long, value_name = "PATH", default_value = ".")]
        workspace: String,

        /// DEPRECATED and ignored (#4512). calibrate no longer derives a
        /// recommendation to write: the CPU-headroom term it was based on is
        /// gone, and `autonomous.workFinder.maxConcurrent` is a per-machine knob
        /// you tune by hand from the measurements. Retained so existing scripts
        /// passing `--write` keep working; prints a deprecation notice.
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

    /// Classify and (opt-in) retire `loom-quarantine:` **git stashes** —
    /// unrelated to `Quarantine` above, which manages the daemon's in-memory
    /// insta-crash pauses; this operates on `check-main-clean.sh
    /// --quarantine`'s rescue stashes on `refs/stash` (Issue #5693, sub-issue
    /// of #5690). Two independent conditions are both required before a
    /// stash is ever eligible for retirement: its content must be either
    /// byte-identical to `HEAD` or provably installer/build-artifact-only,
    /// AND its `loom-quarantine:` label's referenced issue must be CLOSED.
    /// Pure local git/`gh` operation — no running daemon required, unlike
    /// `Quarantine`.
    Stashes {
        #[command(subcommand)]
        action: StashesAction,
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

    /// Cancel a running sweep via the running daemon (Issue #4980): the `dispatch`
    /// sibling, over the same `CancelSweep` IPC request the
    /// `mcp__loom__cancel_sweep` tool uses.
    ///
    /// This is the sanctioned way to stop a wedged sweep over ssh, where no MCP
    /// server is attached. Do NOT hand-`kill` a sweep's pids instead: the daemon
    /// tracks the wrapper, and killing it leaves the underlying `claude` agent
    /// alive — in the 2026-08-03 incident that surviving agent noticed its
    /// subprocesses had died and relaunched them, against an issue whose claim
    /// had already been returned to the queue. The daemon signals the whole
    /// process GROUP (SIGTERM, then SIGKILL after the grace window), releases the
    /// claim lock, restores the label, and emits the lifecycle events.
    Cancel {
        /// The sweep id to cancel (as shown by `loom-daemon status`). Mutually
        /// exclusive with `--issue`.
        #[arg(value_name = "SWEEP_ID", required_unless_present = "issue")]
        sweep_id: Option<String>,

        /// Cancel the live sweep for this issue instead of naming a sweep id.
        /// Resolved client-side against the daemon's registry; refuses rather
        /// than guesses if the issue somehow has more than one live sweep.
        #[arg(long, value_name = "N", conflicts_with = "sweep_id")]
        issue: Option<u32>,

        /// Seconds to wait between SIGTERM and SIGKILL. Defaults to the same
        /// value the `cancel_sweep` MCP tool sends.
        #[arg(long, value_name = "SECS", default_value_t = cli::cancel::DEFAULT_CANCEL_GRACE_SECS)]
        grace: u64,

        /// Target managed-workspace root (Issue #3929). Omit to use the daemon's
        /// default workspace, or the registered repo the CLI's own cwd falls
        /// under (#4299) — the same resolution `dispatch` applies.
        #[arg(long, value_name = "PATH")]
        workspace: Option<String>,
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
    /// a supervised daemon exits 0 for a clean relaunch (a new pid, exactly its
    /// start flags) via launchd `KeepAlive:SuccessfulExit` / systemd
    /// `Restart=on-success`. **In-flight sweeps survive on launchd only** — on
    /// systemd they live in the unit's cgroup and the stop job reaps them, so
    /// use `restart --drain` there to finish in-flight work first (#5119). On an
    /// unsupervised host (nohup / Linux without a unit / `--foreground`) the
    /// daemon refuses and stays running, and this command prints the refusal and
    /// exits non-zero. This is the primitive #4017 Phase 3 will call after a
    /// rebuild — it does nothing on its own.
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

    /// Manage secret-safe machine-level AI account profiles.
    Accounts {
        #[command(subcommand)]
        action: AccountsAction,

        /// Loom workspace whose provider-aware account registry is updated.
        #[arg(long, value_name = "PATH", default_value = ".", global = true)]
        workspace: String,
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
    ///
    /// **What `--safe` actually narrows** (issue #4890): only the artifact
    /// classes that have a merged-PR concept to check — worktrees (gated on
    /// issue-closed + PR-merged + grace period) and branches. Tmux sessions
    /// are not an artifact of a merged PR, so `--safe` skips tmux cleanup
    /// entirely rather than pretending to gate it; pair with `--tmux-only`
    /// (optionally `--force`) to clean tmux sessions explicitly. Outside
    /// `--safe`, a tmux session with an attached client (a live operator
    /// terminal) is likewise preserved unless `--force` is passed.
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

        /// Merged-PR-only mode for worktrees/branches. Tmux sessions have no
        /// PR association, so `--safe` skips tmux cleanup entirely (see the
        /// `Clean` doc comment) rather than silently killing a live session.
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
    ///
    /// NOTE (#5047): the byte-identical passthrough means `forge issue
    /// create` inherits `gh issue create`'s GraphQL cost with no REST
    /// fallback of its own on exhaustion — see
    /// `.loom/docs/gh-issue-create-rest-fallback.md` /
    /// `forge_gh_create_issue_rl_safe` for the fallback path instead.
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

        /// Map the model to the next CHEAPER Task-tool alias (issue #5687) —
        /// `fable → opus → sonnet → haiku`. This is the "one rung down" step
        /// `/loom:sweep` applies after a `MODEL_CREDITS_EXHAUSTED` kill.
        /// Exits 3 with no output when the model is already at the cheapest
        /// rung or is unrecognized, so the caller falls through to its normal
        /// mid-phase-death handling instead of guessing a model.
        #[arg(long)]
        downgrade: bool,

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

    /// Inspect the durable `sweep.outcome` telemetry journal (Issue #4704,
    /// absorbs #4137): every completed sweep's model, config, result, and
    /// duration, persisted locally regardless of whether any exporter is
    /// configured. Purely file-based — does not require a running daemon
    /// (unlike `status`). See `.loom/docs/telemetry-schema.md` for the wire
    /// format this journal's lines follow.
    ///
    /// With no filters, prints the "success rate and median duration by
    /// model" summary #4137 asked for. `--records` instead lists individual
    /// outcome records (still respecting `--model`/`--result`/`--limit`).
    SweepOutcomes {
        /// Repo root whose `.loom/logs/sweep-outcome-telemetry.jsonl` to
        /// read (plain path, default `.` — no upward `.git` walk).
        #[arg(long, value_name = "PATH", default_value = ".")]
        workspace: String,

        /// Only include records for this dispatched model (matches the
        /// `"default"` group for records with no explicit model).
        #[arg(long)]
        model: Option<String>,

        /// Only include records with this terminal result: success, failure,
        /// cancelled, blocked.
        #[arg(long)]
        result: Option<String>,

        /// List individual records (newest first) instead of the
        /// summary-by-model table.
        #[arg(long)]
        records: bool,

        /// Cap the number of records considered (after filtering), newest
        /// first. Applies to both the summary and `--records` listing.
        #[arg(long, value_name = "N")]
        limit: Option<usize>,

        /// Print how many locally-journaled records the observability
        /// exporter has not yet attempted to send to the backend (Issue
        /// #5084) instead of the summary/records output — every other flag
        /// except `--json`/`--workspace` is ignored in this mode. This is the
        /// AC's "N local outcomes not yet in the backend" measurability
        /// gauge: `0` means the backfill drain (see
        /// `.loom/docs/observability.md`) is caught up.
        #[arg(long = "pending-export")]
        pending_export: bool,

        /// Emit machine-readable JSON instead of the human-readable table.
        #[arg(long)]
        json: bool,
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
// `Record` carries the full outcome-chain field set, so it is much larger
// than the other variants; boxing it would only add an allocation to a
// once-per-invocation CLI parse.
#[allow(clippy::large_enum_variant)]
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
    /// LINUX TARGETS ONLY (#5395): the plan is apt-get + systemd --user, so a
    /// non-Linux host is refused up front (after a `uname -s` probe, before
    /// anything is touched) and must be onboarded by hand — see the
    /// `fleet add-worker` section of the daemon reference.
    AddWorker {
        /// SSH alias/host to reach the worker (from `repo:remote` or operator
        /// supplied). Must be a Linux (Debian/Ubuntu) host.
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

    /// Provision a pinned SPICE simulation toolchain (ngspice + Xyce, built
    /// from source) plus the gf180mcu and sky130 open PDKs onto an
    /// already-reachable SSH host (issue #4931, Phase 1a of "elastic cloud
    /// compute for SPICE simulations"). A **sim runner is not a loom
    /// worker**: unlike `add-worker`, this touches no cloud CLI, no
    /// Tailscale API, no forge/token credentials, and does not write the
    /// fleet registry. Idempotent: a re-run against an already-bootstrapped
    /// host (at the same pins) reports every step already-satisfied.
    BootstrapSpice {
        /// SSH alias/host to reach the runner (from `repo:remote` or
        /// operator supplied).
        #[arg(value_name = "SSH_HOST")]
        ssh_host: String,

        /// ngspice source repository URL.
        #[arg(long, value_name = "URL", default_value = loom_daemon::fleet::spice_runner::DEFAULT_NGSPICE_REPO_URL)]
        ngspice_repo_url: String,

        /// Pinned ngspice ref (tag/branch/commit) to build.
        #[arg(long, value_name = "REF", default_value = loom_daemon::fleet::spice_runner::DEFAULT_NGSPICE_REF)]
        ngspice_ref: String,

        /// Skip the Xyce (+ Trilinos) source build — an ngspice-only runner.
        /// Xyce's from-source build takes hours; analog repos that only
        /// simulate with ngspice should not pay for it.
        #[arg(long)]
        skip_xyce: bool,

        /// Xyce source repository URL.
        #[arg(long, value_name = "URL", default_value = loom_daemon::fleet::spice_runner::DEFAULT_XYCE_REPO_URL)]
        xyce_repo_url: String,

        /// Pinned Xyce ref to build.
        #[arg(long, value_name = "REF", default_value = loom_daemon::fleet::spice_runner::DEFAULT_XYCE_REF)]
        xyce_ref: String,

        /// Trilinos source repository URL (Xyce's solver dependency).
        #[arg(long, value_name = "URL", default_value = loom_daemon::fleet::spice_runner::DEFAULT_TRILINOS_REPO_URL)]
        trilinos_repo_url: String,

        /// Pinned Trilinos ref to build.
        #[arg(long, value_name = "REF", default_value = loom_daemon::fleet::spice_runner::DEFAULT_TRILINOS_REF)]
        trilinos_ref: String,

        /// gf180mcu-pdk repository URL.
        #[arg(long, value_name = "URL", default_value = loom_daemon::fleet::spice_runner::DEFAULT_GF180MCU_REPO_URL)]
        gf180mcu_repo_url: String,

        /// Pinned gf180mcu-pdk ref to check out.
        #[arg(long, value_name = "REF", default_value = loom_daemon::fleet::spice_runner::DEFAULT_GF180MCU_REF)]
        gf180mcu_ref: String,

        /// Submodule path inside the gf180mcu checkout holding the SPICE
        /// device models. Pass an empty string to clone the top-level repo
        /// only (when a pin's layout differs from the default).
        #[arg(long, value_name = "PATH", default_value = loom_daemon::fleet::spice_runner::DEFAULT_GF180MCU_MODELS_PATH)]
        gf180mcu_models_path: String,

        /// skywater-pdk (sky130) repository URL.
        #[arg(long, value_name = "URL", default_value = loom_daemon::fleet::spice_runner::DEFAULT_SKY130_REPO_URL)]
        sky130_repo_url: String,

        /// Pinned skywater-pdk ref to check out.
        #[arg(long, value_name = "REF", default_value = loom_daemon::fleet::spice_runner::DEFAULT_SKY130_REF)]
        sky130_ref: String,

        /// Submodule path inside the sky130 checkout holding the SPICE
        /// device models (see `--gf180mcu-models-path`).
        #[arg(long, value_name = "PATH", default_value = loom_daemon::fleet::spice_runner::DEFAULT_SKY130_MODELS_PATH)]
        sky130_models_path: String,

        /// Print the ordered plan without contacting the host.
        #[arg(long)]
        dry_run: bool,
    },

    /// Aggregate sweep/token/health state across every fleet host, side by
    /// side, including the local host (issue #4342). Reads the fleet registry
    /// (`~/.loom/fleet.json`, or the `LOOM_FLEET_PATH`-pointed file — a
    /// first-class supported way to hand this an operator-maintained roster
    /// covering hosts `fleet add-worker` never touched, e.g. macOS targets
    /// per #5395; see the daemon reference's "The fleet registry &
    /// LOOM_FLEET_PATH" section for the joint-ownership/local-host caveats,
    /// #5576), collects the local host's own status in-process (over
    /// the daemon's Unix socket — never `ssh localhost`), and fans out to
    /// every remote worker's `loom-daemon status --json` concurrently, each
    /// bounded by a per-host timeout so one hung host cannot stall the report.
    /// A roster entry that resolves to the local host is automatically
    /// excluded from that fanout (it is already the `local` row) rather than
    /// producing a spurious `ssh <self>`/"Permission denied" row. Distinct,
    /// loud per-host states (`UP` / `DAEMON DOWN` / `UNREACHABLE` /
    /// `PARSE ERROR` / `DRAINING`) — silence never reads as idle. Exits
    /// non-zero unless every roster host is `UP`.
    Status {
        /// Emit machine-readable JSON instead of the human-readable table.
        #[arg(long)]
        json: bool,

        /// Per-host SSH connect + collection timeout, in seconds (issue
        /// #5575). Bounds BOTH the `ssh -o ConnectTimeout` used to reach a
        /// host and the outer per-host `tokio::time::timeout` wrapping the
        /// whole collection — a worker running several in-flight sweeps can
        /// legitimately take longer than the 8s default to answer `status
        /// --json` over ssh; raise this rather than misreading a merely-busy
        /// host as UNREACHABLE.
        #[arg(
            long,
            value_name = "SECS",
            default_value_t = loom_daemon::fleet::status::DEFAULT_TIMEOUT_SECS
        )]
        timeout_secs: u64,
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

    /// Roll the `loom-daemon` binary across fleet hosts, with a **measured**
    /// success verdict (issue #5504): SSH-invokes the existing
    /// `loom-daemon-update.sh` per host and compares the running process's
    /// start time against the installed binary's build time — never
    /// `loom-daemon --version` (which execs the binary and reports whatever
    /// was last *built*, not what is *running*, #5467) and never the update
    /// script's own transcript/exit code alone (#5390). Exactly one of
    /// `<SSH_HOST>` or `--all` must be given. Distinct, loud per-host
    /// outcomes (`ROLLED` / `ALREADY CURRENT` / `FAILED` / `UNREACHABLE`),
    /// matching `fleet status`'s "silence never reads as idle" discipline —
    /// see `loom_daemon::fleet::roll`'s module doc for the full design,
    /// including why an in-flight update is never signaled/interrupted.
    Roll {
        /// SSH alias/host to roll. Omit and pass `--all` instead to roll
        /// every host in the fleet registry.
        #[arg(value_name = "SSH_HOST")]
        host: Option<String>,

        /// Roll every host in the fleet registry (mutually exclusive with
        /// `<SSH_HOST>`).
        #[arg(long, conflicts_with = "host")]
        all: bool,

        /// Max seconds the orchestrator waits for one host's roll sequence to
        /// finish locally. Generous by design — a `cargo build --release`
        /// fallback can take several minutes — and never signals the remote
        /// update on expiry; see the module doc's "Interrupt safety" section.
        #[arg(long, value_name = "SECS", default_value_t = loom_daemon::fleet::roll::DEFAULT_ROLL_TIMEOUT_SECS)]
        timeout: u64,

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

/// Sub-actions for `loom-daemon stashes` (Issue #5693). Both classify every
/// `loom-quarantine:` stash it finds; only `retire --execute` ever drops one.
#[derive(Subcommand)]
enum StashesAction {
    /// Classify every `loom-quarantine:` stash and print a report — always
    /// read-only, never drops anything. Equivalent to `retire` without
    /// `--execute`.
    List {
        /// Repo root to scan (default: current directory).
        #[arg(long, value_name = "PATH", default_value = ".")]
        workspace: String,

        /// Restrict to stashes whose `loom-quarantine:` label references this
        /// issue number. Omit to consider every quarantine stash in the repo.
        #[arg(long, value_name = "ISSUE")]
        issue: Option<u64>,

        /// Print the per-path recoverability proof for EVERY path in every
        /// stash. Without it the report previews the first few (blocking
        /// paths first) and elides the rest — #5690's worst case was a single
        /// stash of 1,749 files.
        #[arg(long)]
        paths: bool,

        /// Emit machine-readable JSON instead of the human-readable report.
        /// Always includes every per-path verdict, regardless of `--paths`.
        #[arg(long)]
        json: bool,
    },

    /// Classify every `loom-quarantine:` stash and, ONLY with `--execute`,
    /// drop the ones classified `Retire` — that is, the issue named by the
    /// label is CLOSED *and* every path in the stash is provably recoverable
    /// without it (identical to `HEAD`, identical to a commit reachable from
    /// `HEAD`, installer-managed/regenerable, or a machine-generated
    /// artifact). Both conditions are required; neither alone retires.
    /// Without `--execute` this is a dry run — identical output to `list`, no
    /// drops. Safely re-runnable: retiring an already-retired/gone stash is a
    /// no-op, not an error. Every drop is journaled to
    /// `.loom/logs/stash-retirement.log` with the stash's commit sha before
    /// the drop, so it stays recoverable with `git stash apply <sha>` until
    /// the object is gc'd.
    Retire {
        /// Repo root to scan (default: current directory).
        #[arg(long, value_name = "PATH", default_value = ".")]
        workspace: String,

        /// Actually drop the stashes classified `Retire`. Omit for a dry run
        /// (the default — this command never drops anything without this
        /// flag).
        #[arg(long)]
        execute: bool,

        /// Restrict to stashes whose `loom-quarantine:` label references this
        /// issue number. Omit to consider every quarantine stash in the repo.
        #[arg(long, value_name = "ISSUE")]
        issue: Option<u64>,

        /// Print the per-path recoverability proof for EVERY path in every
        /// stash instead of a previewed subset.
        #[arg(long)]
        paths: bool,

        /// Emit machine-readable JSON instead of the human-readable report.
        /// Always includes every per-path verdict, regardless of `--paths`.
        #[arg(long)]
        json: bool,
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
    ///
    /// This is a byte-identical passthrough, so `forge issue create` is
    /// GraphQL-backed and has **no REST fallback** — it dies on GraphQL-quota
    /// exhaustion exactly like `gh issue create`, and is not an escape hatch
    /// from it (#5047). To file an issue that survives an exhausted GraphQL
    /// pool, use `.loom/scripts/create-issue.sh`.
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
    /// `forge auto-merge <pr> [--method M] [--expected-head-sha SHA]` —
    /// enable auto-merge for a PR (formerly `loom-auto-merge`). GitHub:
    /// `enablePullRequestAutoMerge` GraphQL mutation. Gitea: declines (exit
    /// 3) → shell `forge_auto_merge`. `--poll-interval` / `--timeout` are
    /// accepted for CLI compatibility and ignored on GitHub (the server
    /// queues the merge).
    #[command(name = "auto-merge")]
    AutoMerge {
        /// Pull request number.
        #[arg(value_name = "PR")]
        pr_number: u32,

        /// Merge method (squash | merge | rebase). Default squash.
        #[arg(long, default_value = "squash")]
        method: String,

        /// Optimistic-concurrency precondition (#5589, mirrors #5579's shell
        /// `EXPECTED_HEAD_SHA`): the SHA the PR's head branch must currently
        /// match. Threaded into the GitHub `expectedHeadOid` GraphQL mutation
        /// input; a mismatch exits `EX_FORGE_HEAD_MISMATCH` (4) instead of
        /// the generic failure exit `1`, distinguishable from a Gitea decline
        /// (exit 3). Omit to preserve prior (unguarded) behavior.
        #[arg(long, value_name = "SHA")]
        expected_head_sha: Option<String>,

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
    /// `loom-tokens bootstrap` CLI (issue #4105, epic #4081). That Python CLI no
    /// longer exists — the package was deleted in Phase 4 (#4557) — so this name
    /// is a historical reference, not runnable advice.
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

/// Sub-actions for `loom-daemon accounts`.
#[derive(Subcommand)]
enum AccountsAction {
    /// Create a named profile and run the provider's interactive login.
    Add {
        #[arg(value_name = "PROVIDER")]
        provider: String,
        #[arg(value_name = "NAME")]
        name: String,
        #[arg(long)]
        device_auth: bool,
        #[arg(long)]
        json: bool,
    },
    /// Import an explicit opaque Codex auth file into a new named profile.
    Import {
        #[arg(value_name = "PROVIDER")]
        provider: String,
        #[arg(value_name = "NAME")]
        name: String,
        #[arg(long, value_name = "PATH")]
        auth_file: PathBuf,
        #[arg(long)]
        json: bool,
    },
    /// List registered accounts and secret-free structural diagnostics.
    List {
        #[arg(long, value_name = "PROVIDER", default_value = "codex")]
        provider: String,
        #[arg(long)]
        json: bool,
    },
    /// Probe one account's structural and login status.
    Status {
        #[arg(value_name = "PROVIDER")]
        provider: String,
        #[arg(value_name = "NAME")]
        name: String,
        #[arg(long)]
        json: bool,
    },
    /// Make an account ineligible without changing its credential state.
    Disable {
        #[arg(value_name = "PROVIDER")]
        provider: String,
        #[arg(value_name = "NAME")]
        name: String,
        #[arg(long)]
        json: bool,
    },
    /// Restore eligibility after structural and permission validation.
    Enable {
        #[arg(value_name = "PROVIDER")]
        provider: String,
        #[arg(value_name = "NAME")]
        name: String,
        #[arg(long)]
        json: bool,
    },
    /// Reauthenticate the existing canonical profile in place.
    Reauth {
        #[arg(value_name = "PROVIDER")]
        provider: String,
        #[arg(value_name = "NAME")]
        name: String,
        #[arg(long)]
        device_auth: bool,
        #[arg(long)]
        json: bool,
    },
    /// Retire to private quarantine, or irreversibly delete with `--purge`.
    Remove {
        #[arg(value_name = "PROVIDER")]
        provider: String,
        #[arg(value_name = "NAME")]
        name: String,
        /// Irreversibly delete credential state instead of quarantining it.
        #[arg(long)]
        purge: bool,
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

/// Process entry point.
///
/// The real body lives in [`run_daemon`]; this wrapper exists so a startup
/// failure **terminates the process itself** instead of returning `Err` out of
/// `#[tokio::main]` (#4531).
///
/// Returning `Err` is correct but not prompt: the `#[tokio::main]` wrapper drops
/// its `Runtime` when `block_on` returns, and `Runtime::drop` blocks the calling
/// thread until every in-flight `spawn_blocking` task finishes. By the time the
/// singleton guard refuses to start (`IpcServer::run`, #3806), the daemon has
/// already spawned its periodic loops, several of which do their work on the
/// blocking pool — so the refusal message printed, then the process sat in
/// `Runtime::drop` for ~10s before `Termination` ever ran. Under a shorter
/// timeout that is indistinguishable from a hang, and it left a doomed second
/// daemon alive (running role-runner/work-finder ticks) long after it had
/// decided not to start.
///
/// The observable contract is deliberately unchanged: the message is the same
/// `Error: {err:?}` line `Termination for Result` would have written to stderr,
/// and [`loom_daemon::ipc::EXIT_STARTUP_FAILURE`] is `1`, the value
/// `ExitCode::FAILURE` carries.
/// Only the latency changes. stderr is explicitly flushed before
/// `std::process::exit` so the refusal is never truncated (`exit` runs no
/// destructors); the log file needs no such care because `env_logger` flushes
/// each record as it writes it — the same assumption every other
/// `std::process::exit` path in this daemon already makes.
#[tokio::main]
async fn main() {
    if let Err(err) = daemon_service::run_daemon().await {
        eprintln!("Error: {err:?}");
        let _ = std::io::stderr().flush();
        std::process::exit(loom_daemon::ipc::EXIT_STARTUP_FAILURE);
    }
}

use cli::accounts::handle_accounts_command;
use cli::cleanup_ops::{
    handle_clean_command, handle_cleanup_command, handle_recover_orphans_command,
};
use cli::legacy_script_cmds::{
    handle_checkpoint_command, handle_resolve_model_command, handle_strip_ansi_command,
    handle_sweep_experiment_command,
};
use cli::misc_cmds::{
    handle_sweep_outcomes_command, handle_update_gitignore_command, handle_validate_command,
};
use cli::stats::{handle_agent_metrics_command, handle_stats_command};
use cli::tokens::{handle_claude_config_command, handle_forge_command, handle_tokens_command};
use cli::workspace_fleet::{
    handle_calibrate_command, handle_fleet_command, handle_workspace_command,
};

fn handle_cli_command(command: Commands) -> Result<()> {
    match command {
        // Script helpers (epic #4081 Phase 3 family 5, issue #4275).
        Commands::StripAnsi { file } => handle_strip_ansi_command(file.as_deref()),
        Commands::ResolveModel {
            model,
            config,
            generation,
            task_alias,
            downgrade,
            tier,
            runtime,
        } => handle_resolve_model_command(
            model.as_deref(),
            config.as_deref(),
            generation,
            task_alias,
            downgrade,
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
        Commands::SweepOutcomes {
            workspace,
            model,
            result,
            records,
            limit,
            pending_export,
            json,
        } => handle_sweep_outcomes_command(
            &workspace,
            model.as_deref(),
            result.as_deref(),
            records,
            limit,
            pending_export,
            json,
        ),
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
        Commands::Stashes { action } => cli::stashes::handle_stashes_command(action),
        Commands::Workspace { action } => handle_workspace_command(action),
        Commands::Fleet { action } => handle_fleet_command(action),
        Commands::Tokens { action } => handle_tokens_command(action),
        Commands::Accounts { action, workspace } => handle_accounts_command(action, &workspace),
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
        Commands::Health { .. } => {
            // Routed directly in `main()` (it needs the async runtime for the
            // socket round-trip + forge fan-out), never dispatched through this
            // sync handler.
            unreachable!("Health is handled in main() before handle_cli_command")
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
        Commands::Cancel { .. } => {
            // Routed directly in `main()` (it needs the async runtime for the
            // socket round-trip), never dispatched through this sync handler.
            unreachable!("Cancel is handled in main() before handle_cli_command")
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
        } => cli::misc_cmds::run_init(workspace, defaults, force, dry_run),
    }
}
