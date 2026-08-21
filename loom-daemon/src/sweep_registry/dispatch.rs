//! The dispatch call path: `SweepRegistry::dispatch`, dispatch-backoff
//! bookkeeping, and peer-claim publishing.

use super::*;

/// Issue #3943: print-mode background-task wait ceiling (milliseconds). A
/// daemon-spawned sweep child is a headless `claude -p` session; in print mode
/// the harness reaps still-running background tasks (the sweep's Builder/Judge
/// subagents) after a 600s ceiling. `spawn_child` pins this to `0` (no cap) on
/// the child env so a long role phase runs to completion instead of being
/// killed mid-build.
pub const BG_WAIT_CEILING_ENV: &str = "CLAUDE_CODE_PRINT_BG_WAIT_CEILING_MS";

/// Resolve the process group of a just-spawned sweep leader (Issue #4980).
///
/// `spawn_child` sets `process_group(0)` on every Unix spawn (#3800), so the
/// child is its own group leader and `getpgid(child) == child`. We *verify* that
/// rather than assume it, because the recorded value later authorizes a
/// `kill(-pgid, …)`: recording a group the child does not actually lead would
/// aim a SIGKILL at unrelated processes (in the worst case the daemon's own
/// group).
///
/// The three outcomes:
///
/// - **Confirmed leader** (`getpgid == pid`) → `Some(pid)`.
/// - **Contradiction** (`getpgid` names some other group) → `None` + a warning.
///   `process_group(0)` did not take; degrade to single-PID signalling rather
///   than signal a group we do not own.
/// - **Unanswerable** (the child already exited, `ESRCH`) → `Some(pid)`. The
///   spawn unconditionally requested its own group, so `pgid == pid` is the only
///   shape this spawn can have, and this is precisely the crash case where the
///   persisted group is the only handle on any surviving descendants. Every
///   consumer re-checks `group_has_members` before signalling, so a fully-dead
///   group is a no-op.
fn spawned_leader_pgid(pid: u32) -> Option<u32> {
    if !cfg!(unix) {
        return None;
    }
    match process_group_of(pid) {
        Some(pgid) if pgid == pid => Some(pid),
        Some(other) => {
            log::warn!(
                "sweep_registry: spawned child pid {pid} reports process group {other} rather \
                 than leading its own — `process_group(0)` did not take. Recording NO group; \
                 cancellation will degrade to single-PID signalling (#4980)."
            );
            None
        }
        None => Some(pid),
    }
}

/// Typed, matchable error returned by [`SweepRegistry::dispatch`] when the
/// open-PR guard (Issue #4123, step 2.6) refuses a dispatch because the target
/// issue already has an **open** linked pull request.
///
/// Every in-memory dedup signal (idempotency key, in-flight set, the
/// `loom:building` label) clears when the parent sweep exits, so an issue whose
/// approved PR is still open looks identical to fresh work the moment its sweep
/// dies — and the work-finder re-dispatches it, redoing finished work against a
/// scarce token pool. The forge's closes-graph is the one durable signal that
/// survives process death and daemon restarts, so this guard consults it.
///
/// This is a **distinct, downcast-matchable** type (not a string-matched
/// `anyhow` message) so the work-finder can attribute the refusal to its own
/// `pr-open-skip` counter rather than a generic dispatch failure. It is created
/// via `.into()` so `anyhow::Error` preserves the concrete type for
/// `downcast_ref::<OpenPrDispatchError>()`.
///
/// Issue #6593: the refusal names the **PR-set alternative** explicitly. Refusing
/// the issue-keyed dispatch is correct — but the candidate class it refuses is
/// exactly the class `sweep.md`'s aggressive taxonomy says to *drive to merge*
/// ("Has an open linked PR … do not build a duplicate"), and that routing normally
/// happens in the spawned child's per-issue pre-flight — which never runs here,
/// because the guard fires before any child exists. `SweepKind::PrSet` (#5342) is
/// the reachable route for that class, so the `Display` text hands the caller the
/// exact dispatch it should issue instead of leaving it to independently know the
/// variant exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OpenPrDispatchError {
    /// The issue whose dispatch was refused.
    pub issue: u32,
    /// The open linked PR that triggered the refusal.
    pub pr: u32,
}

impl std::fmt::Display for OpenPrDispatchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "refusing to dispatch issue #{issue}: it already has an open linked PR #{pr} \
             (#4123 open-PR guard). A fresh issue sweep would duplicate work already \
             in review. To drive the existing PR forward instead of rebuilding, dispatch \
             kind={{\"PrSet\":[{pr}]}} (Mode C, #5342) — that runs Judge/Doctor -> Merge on \
             PR #{pr} without re-running Curator/Builder, and claims no issue.",
            issue = self.issue,
            pr = self.pr
        )
    }
}

impl std::error::Error for OpenPrDispatchError {}

/// Typed, matchable error returned by [`SweepRegistry::dispatch`] when the
/// park-label guard (Issue #4444, step 2.7) refuses a dispatch because the
/// target issue currently carries a [`PARK_LABELS`] entry (`loom:blocked` /
/// `loom:operator-only`).
///
/// The work-finder's [`SKIP_LABELS`] filter only covers *its own* candidate
/// query. Every other dispatch route — all three watchdogs (#3887 / #3895 /
/// #3910), the reaper's checkpoint-resume (#4256), the epic supervisor, and the
/// IPC/CLI `dispatch_sweep` — funnels through `dispatch_inner` without ever
/// re-reading the forge labels, so a park applied *after* the original dispatch
/// was invisible to them and the daemon overrode a deliberate human park
/// (observed on #4366). This guard closes that hole for every route at once.
///
/// Like [`OpenPrDispatchError`] this is a **distinct, downcast-matchable** type
/// (not a string-matched `anyhow` message) so the work-finder can attribute the
/// refusal to its labeled-skip counter rather than to a generic dispatch
/// failure.
///
/// [`PARK_LABELS`]: crate::work_finder::PARK_LABELS
/// [`SKIP_LABELS`]: crate::work_finder::SKIP_LABELS
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParkedIssueDispatchError {
    /// The issue whose dispatch was refused.
    pub issue: u32,
    /// The park label that triggered the refusal (`loom:blocked` or
    /// `loom:operator-only`).
    pub label: String,
}

impl std::fmt::Display for ParkedIssueDispatchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "refusing to dispatch issue #{}: it currently carries `{}` (#4444 park-label \
             guard). A deliberate park must survive every re-dispatch route — watchdog, \
             checkpoint-resume, epic supervisor, IPC/CLI — until the label is cleared.",
            self.issue, self.label
        )
    }
}

impl std::error::Error for ParkedIssueDispatchError {}

/// Typed, matchable error returned by [`SweepRegistry::dispatch`] when the
/// per-issue dispatch backoff (Issue #4485, step 2.8) refuses a dispatch
/// because this issue's previous dispatch failed and its backoff window has
/// not elapsed yet.
///
/// Distinct, downcast-matchable type — same rationale as
/// [`OpenPrDispatchError`]: a backoff refusal is a *deliberate skip*, not a
/// dispatch failure, so the work-finder attributes it to its own
/// `backoff-skip` counter instead of the generic error tally.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DispatchBackoffError {
    /// The issue whose dispatch was refused.
    pub issue: u32,
    /// Consecutive failed dispatch attempts recorded for this issue.
    pub consecutive: u32,
    /// Whole seconds remaining before the next attempt is allowed.
    pub retry_after_secs: u64,
}

impl std::fmt::Display for DispatchBackoffError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "refusing to dispatch issue #{}: its last {} dispatch attempt(s) failed fast; \
             backing off for another {}s (#4485 dispatch backoff)",
            self.issue, self.consecutive, self.retry_after_secs
        )
    }
}

impl std::error::Error for DispatchBackoffError {}

/// Typed, matchable error returned by [`SweepRegistry::dispatch`] when the
/// workspace-commands guard (Issue #4027, step 2.4) refuses a dispatch
/// because this workspace is missing `.claude/commands/loom/sweep.md` — a
/// **structural, workspace-level** refusal (every candidate issue in this
/// workspace would be refused identically), unlike the other typed dispatch
/// errors in this module, which are all issue-scoped.
///
/// Previously a plain `anyhow!` string, so the work-finder's generic
/// `else` fallback counted and logged it once **per candidate issue, every
/// tick** — the #6440 incident's literal 865-refusals-in-an-hour signature.
/// Making this downcast-matchable lets [`crate::work_finder`] attribute it to
/// its own `workspace-commands-missing` counter and, more importantly, check
/// [`WorkDispatcher::workspace_commands_missing`](crate::work_finder::WorkDispatcher::workspace_commands_missing)
/// **before** the per-candidate loop so the whole workspace's candidate batch
/// is skipped in one step per tick rather than re-discovering the same
/// refusal once per ready issue.
///
/// `loom-daemon status` already surfaces this condition per repo
/// independently (`RepoStatus.sweep_command_missing`, Issue #5682, rendered
/// as the `no-sweep` GATE-column override) — this type is the dispatch-path
/// half of the same signal, not a duplicate of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceCommandsMissingDispatchError {
    /// The workspace root refusing every dispatch.
    pub workspace: std::path::PathBuf,
}

impl std::fmt::Display for WorkspaceCommandsMissingDispatchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "refusing to dispatch: workspace {} is missing .claude/commands/loom/sweep.md — \
             the /loom:sweep slash command is not installed there (#4027 wedge-loop guard). Run \
             `loom-daemon init {}` in that workspace first.",
            self.workspace.display(),
            self.workspace.display()
        )
    }
}

impl std::error::Error for WorkspaceCommandsMissingDispatchError {}

/// Which signal caught the cross-host dispatch collision enforced by
/// [`CollisionDispatchError`] (Issue #5789, upgrading #4028/#4085 from
/// detection-only to enforcement).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CollisionSource {
    /// Caught by the fast, in-memory soft-claim broadcast (Issue #4028's
    /// [`crate::peer_claims::PeerClaimView`]) — a peer host's advertisement for
    /// this same issue is still live. Cheapest and earliest signal: no `gh`
    /// round trip, checked before the claim lock is even acquired.
    PeerClaim,
    /// Caught by the opt-in forge-label pre-flip read
    /// ([`crate::sweep_registry::guards::classify_preflip_labels`]) — the
    /// peer's `loom:building` flip (or `loom:issue` removal) already landed on
    /// the forge before this host's own flip. Carries the observed pre-flip
    /// label set for diagnostics.
    ForgeLabel { labels: Vec<String> },
}

impl std::fmt::Display for CollisionSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CollisionSource::PeerClaim => {
                write!(f, "a peer host's live soft-claim advertisement")
            }
            CollisionSource::ForgeLabel { labels } => {
                write!(
                    f,
                    "a peer host's forge label flip (observed labels=[{}])",
                    labels.join(", ")
                )
            }
        }
    }
}

/// Typed, matchable error returned by [`SweepRegistry::dispatch`] when a
/// cross-host claim collision is detected and this host backs off rather than
/// duplicating the sweep (Issue #5789, upgrading #4028's soft claim /
/// #4085's collision detection from detection-only into real enforcement).
///
/// Every side effect this host had already applied before the collision was
/// caught — the claim lock ([`SweepRegistry::acquire_lock`]) and, for the
/// [`CollisionSource::ForgeLabel`] case, the peer-claim advertisement
/// ([`SweepRegistry::publish_peer_claim`]) — is unwound before this error is
/// returned, so a losing host leaves no trace of its aborted attempt: the
/// issue is exactly as claimable as it was before this host ever looked at
/// it (modulo the winning peer's own claim, which this host must not touch).
///
/// Distinct, downcast-matchable type — same rationale as
/// [`OpenPrDispatchError`]: a collision back-off is a *deliberate skip*, not a
/// dispatch failure, so a caller (the work-finder, a CLI/IPC dispatch, a
/// watchdog) can attribute it to its own collision-skip counter instead of
/// the generic error tally.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CollisionDispatchError {
    /// The issue whose dispatch was refused.
    pub issue: u32,
    /// Which signal caught the collision.
    pub source: CollisionSource,
}

impl std::fmt::Display for CollisionDispatchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "refusing to dispatch issue #{}: a cross-host claim collision was detected via {} \
             (#5789 collision enforcement, built on #4028/#4085). Backing off rather than \
             duplicating a sweep another host already claimed.",
            self.issue, self.source
        )
    }
}

impl std::error::Error for CollisionDispatchError {}

/// Typed, matchable error returned by [`SweepRegistry::dispatch`] when this
/// dispatcher loses the claim-then-verify-order tie-break (Issue #6287, Epic
/// #6165 Phase 2) — its own lease comment, read back immediately after the
/// label flip and lease write, is not the earliest live one on the issue.
///
/// Distinct from [`CollisionDispatchError`]: a collision back-off (4a) fires
/// BEFORE this host ever flips the label, on evidence a peer already holds
/// the claim; this error fires AFTER this host's own (successful, and
/// unconditionally idempotent) flip, when a peer's flip is confirmed to have
/// happened first by the forge's own comment-creation order — the residual
/// race #4a's pre-flip read cannot fully close since two flips can both
/// commit in the same window with neither pre-flip read observing the
/// other. Every side effect this host exclusively controls (the peer-claim
/// advertisement, the claim lock) is unwound before this error is returned;
/// the shared `loom:building` label is deliberately left alone — see
/// `dispatch_inner`'s 4d comment for why reverting it would be unsafe.
///
/// Same rationale as [`CollisionDispatchError`]/[`OpenPrDispatchError`]: a
/// distinct, downcast-matchable type so a caller can attribute this to its
/// own tie-break-lost counter rather than the generic error tally.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeaseOrderDispatchError {
    /// The issue whose dispatch was refused.
    pub issue: u32,
    /// This (losing) dispatcher's own sweep id.
    pub sweep_id: String,
    /// The host that holds the earliest live lease comment.
    pub earliest_host: String,
    /// The sweep id that holds the earliest live lease comment.
    pub earliest_sweep_id: String,
}

impl std::fmt::Display for LeaseOrderDispatchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "refusing to dispatch issue #{}: sweep {} lost the claim-then-verify-order \
             tie-break (#6287) to sweep {} on host {} — its lease comment has an earlier \
             forge-assigned order. Standing down rather than duplicating a sweep another host \
             already won.",
            self.issue, self.sweep_id, self.earliest_sweep_id, self.earliest_host
        )
    }
}

impl std::error::Error for LeaseOrderDispatchError {}

/// Typed, matchable error returned by [`SweepRegistry::dispatch`] when the
/// spawned child died immediately in `spawn-claude.sh`'s token-selection
/// pre-flight step (exit 78 / [`crate::tokens_pool::select::EX_CONFIG`]) —
/// the "no usable OAuth token in the pool" shape (#4689, typed by #6614).
///
/// Before #6614 this was a bare `anyhow!` with the same text. The text is
/// preserved byte-for-byte (both the CLI's `Daemon rejected the dispatch: …`
/// and `mcp__loom__dispatch_sweep`'s `Failed` render it verbatim); the type
/// exists so the work-finder can recognize *this specific* failure by
/// downcast — never by string match — and feed the cross-issue empty-pool
/// brake ([`SweepRegistry::record_token_selection_failure`]) instead of
/// letting it disappear into the generic error tally and be re-dispatched on
/// the next tick, forever.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TokenSelectionDispatchError {
    /// The issue whose dispatch died at token selection.
    pub issue: u32,
    /// The per-sweep log holding the exact selection failure.
    pub log_path: std::path::PathBuf,
}

impl std::fmt::Display for TokenSelectionDispatchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "issue #{}: spawned child exited immediately — token selection failed (no usable \
             OAuth token in the pool). Add accounts to ~/.claude-monitor/accounts.env then \
             `loom-daemon tokens bootstrap`, or re-probe an existing pool with `loom-daemon \
             tokens check --ranking`. See the sweep log for the exact failure: {}",
            self.issue,
            self.log_path.display()
        )
    }
}

impl std::error::Error for TokenSelectionDispatchError {}

/// Issue #3730: experiment-related env vars forwarded to the detached sweep
/// child via an EXPLICIT ALLOWLIST (never a blanket env_clear/copy). Byte-exact
/// names verified against `loom_tools/sweep_experiment.py` (`LOOM_MODEL_EXPERIMENT`,
/// `LOOM_MODEL_EXPERIMENT_CANARY`) and `.loom/scripts/archive-transcripts.sh`
/// (`LOOM_TRANSCRIPT_ARCHIVE`). Forwarding these makes env-based experiment
/// enablement reliable regardless of how the daemon itself was launched — an
/// operator can export them right before dispatching and have them reach the
/// child. Each is forwarded only when set to a non-empty value (see
/// `spawn_child`), so the spawn is a no-op when none are set.
pub const EXPERIMENT_ENV_ALLOWLIST: &[&str] = &[
    "LOOM_MODEL_EXPERIMENT",
    "LOOM_MODEL_EXPERIMENT_CANARY",
    "LOOM_TRANSCRIPT_ARCHIVE",
];

// ============================================================================
// Per-issue dispatch backoff / flap circuit breaker (Issue #4485)
// ============================================================================
//
// The insta-crash quarantine above (#3939) is the only brake on re-dispatching
// a failing issue, and it is a *three-strikes* brake with two deliberate
// carve-outs: an account-exhaustion death (#4122) and a claude-wrapper
// pre-flight death (#4386) both leave the per-issue tally **untouched** on
// purpose (the issue is not at fault). Nothing else limits how *often* one
// issue may be re-dispatched: `reap_once` restores `loom:building` ->
// `loom:issue` the moment the child dies and the issue "re-qualifies on the
// next work-finder poll" (see the module comment at the quarantine section) —
// a documented no-backoff loop.
//
// The observed consequence (#4485) was ~90 `loom:issue`/`loom:building` label
// events on one issue in ~7 minutes: every dispatch's child died ~4s in, the
// claim was restored ~1s later, and the next tick re-dispatched it. Because
// every strike fell into a carve-out (or landed on a *different* daemon
// process's in-memory tally — quarantine state is per-process and never
// shared), the 3-strike quarantine did not engage for over 20 cycles.
//
// This backoff closes that gap from the other direction: instead of asking
// *why* a dispatch failed, it caps *how often* a failing issue may be
// re-attempted at all. A fast (sub-`insta_crash_secs`) or zero-progress
// terminal outcome — including the two quarantine carve-outs — records a
// failure and pushes the issue's next-allowed dispatch instant out
// exponentially (base, 2x, 4x, …, capped). Any outcome that made real progress
// clears the entry immediately.
//
// Deliberately **narrow and fail-open**:
//
// - In-memory only, per registry: a daemon restart clears it, so it can never
//   permanently strand an issue.
// - Never touches a forge label (so the breaker itself cannot flap anything)
//   and costs zero API calls.
// - Bounded by `max`, and the consecutive tally restarts from scratch when the
//   previous failure is older than `max` (an issue that fails once a day never
//   accretes toward a long backoff).
// - Exempts the bounded one-shot recovery paths — the reaper-driven resume
//   (#4256, capped by `MAX_RESUME_ATTEMPTS`) and the three watchdogs (#3887 /
//   #3895 / #3910, each latched to a single retry per issue) — so a refusal can
//   never burn a recovery attempt that is already rate-limited by its own latch.

/// Env var toggling the per-issue dispatch backoff (Issue #4485).
/// `0`/`false`/`no`/`off` disables; `1`/`true`/`yes`/`on` forces on. Overrides
/// config. Defaults ON — like quarantine it is a safety backstop, and unlike
/// quarantine it never blocks an issue for longer than
/// [`DispatchBackoffConfig::max`].
pub const DISPATCH_BACKOFF_ENABLE_ENV: &str = "LOOM_DISPATCH_BACKOFF";

/// Env var overriding the first-failure backoff delay, in seconds (Issue
/// #4485). A zero/invalid value falls through to config/default.
pub const DISPATCH_BACKOFF_BASE_ENV: &str = "LOOM_DISPATCH_BACKOFF_BASE_SECS";

/// Env var overriding the maximum backoff delay, in seconds (Issue #4485). A
/// zero/invalid value falls through to config/default.
pub const DISPATCH_BACKOFF_MAX_ENV: &str = "LOOM_DISPATCH_BACKOFF_MAX_SECS";

/// Default first-failure backoff delay (#4485): one work-finder tick
/// ([`crate::work_finder::DEFAULT_WORK_FINDER_INTERVAL_SECS`]). A single
/// failed dispatch therefore costs at most one extra tick of latency, while a
/// repeatedly-failing issue doubles away from the tick cadence instead of
/// flapping its label on every poll.
pub const DEFAULT_DISPATCH_BACKOFF_BASE_SECS: u64 = 60;

/// Default maximum backoff delay (#4485). Reached after 5 consecutive failures
/// (60s → 120 → 240 → 480 → 900). Well under the quarantine TTL
/// ([`DEFAULT_QUARANTINE_TTL_SECS`]), so on an issue that IS quarantine-eligible
/// the quarantine remains the longer, louder, operator-visible brake and this
/// only smooths the ramp toward it.
pub const DEFAULT_DISPATCH_BACKOFF_MAX_SECS: u64 = 900;

// ============================================================================
// Cross-issue empty-pool dispatch brake (Issue #6614)
// ============================================================================
//
// The per-issue backoff above bounds how often ONE issue is re-attempted; it
// plateaus at `max` (900s) and then repeats at that cadence forever. That is
// the right shape for an issue that is itself broken, and the wrong shape for
// a machine-level fault: when the token pool is empty, EVERY candidate issue
// dies identically in `spawn-claude.sh`'s token-selection step, so a
// per-issue brake just spreads the same doomed dispatch across the backlog —
// the "~15-minute crash-loop with no operator-facing signal" reported in
// #6614 (a `/login` on the host revoked the pooled OAuth credential the fleet
// was riding on mid-run).
//
// The fleet-scoped brake this needs already exists: #4386's cross-issue
// pre-flight-death streak, its #5030 half-open dispatch gate (hold every new
// dispatch to the workspace except one probe per cooldown), and its loud
// `PreflightAdvisory` event. The gap is purely that nothing FEEDS it in this
// case — the streak is only ever incremented by `reap_once`, and a
// token-selection death is caught SYNCHRONOUSLY by `finish_issue_dispatch`
// (#4689), which returns before any entry the reaper could reap is recorded.
//
// So this is a counter, not a second breaker: distinct issues that died at
// token selection inside a trailing window. At/above the threshold it becomes
// one more disjunct in `update_preflight_advisory`'s existing trip decision,
// reusing that mechanism's hold, half-open probe, log line, and event
// wholesale.
//
// Why DISTINCT issues, not raw failures: a single unlucky issue cycling
// through its own #4485 backoff must never trip a fleet-wide hold (the
// over-trigger failure mode #6614 explicitly warns against). N *different*
// issues all dying at the same step cannot be explained by any one issue —
// it can only be the pool. And why a WINDOW: a slow trickle of unrelated
// one-off failures spread over hours is not a systemic fault, so entries
// older than the window stop counting.

/// Env var overriding how many DISTINCT issues must die at token selection
/// inside [`EMPTY_POOL_BREAKER_WINDOW_ENV`] before the workspace's pre-flight
/// advisory trips (Issue #6614). Zero/invalid falls through to the default.
pub const EMPTY_POOL_BREAKER_THRESHOLD_ENV: &str = "LOOM_EMPTY_POOL_BREAKER_THRESHOLD";

/// Env var overriding the trailing window, in seconds, over which distinct
/// token-selection deaths are counted (Issue #6614). Zero/invalid falls
/// through to the default.
pub const EMPTY_POOL_BREAKER_WINDOW_ENV: &str = "LOOM_EMPTY_POOL_BREAKER_WINDOW_SECS";

/// Default distinct-issue threshold (#6614). Three different issues dying at
/// the same pre-flight step is already unambiguous — one is an unlucky issue,
/// two is a coincidence, three different issues cannot all be individually
/// broken in the identical way — while still being small enough that the
/// fleet stops within a single work-finder tick's worth of candidates rather
/// than after a whole backlog has been burned. Matches
/// [`DEFAULT_PREFLIGHT_TRIPWIRE_THRESHOLD`](crate::sweep_registry::DEFAULT_PREFLIGHT_TRIPWIRE_THRESHOLD)'s
/// spirit deliberately: this is the same tripwire, fed from a path the reaper
/// cannot see.
pub const DEFAULT_EMPTY_POOL_BREAKER_THRESHOLD: usize = 3;

/// Default trailing window (#6614): 30 minutes, twice the per-issue backoff
/// plateau ([`DEFAULT_DISPATCH_BACKOFF_MAX_SECS`]) so a genuinely systemic
/// fault — whose failures arrive far faster than that plateau — always
/// accumulates, while isolated failures spaced further apart than any
/// plausible common cause never do.
pub const DEFAULT_EMPTY_POOL_BREAKER_WINDOW_SECS: i64 = 1800;

/// Resolve the distinct-issue threshold (env > default, #6614).
#[must_use]
pub fn resolve_empty_pool_breaker_threshold() -> usize {
    std::env::var(EMPTY_POOL_BREAKER_THRESHOLD_ENV)
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
        .filter(|&n| n > 0)
        .unwrap_or(DEFAULT_EMPTY_POOL_BREAKER_THRESHOLD)
}

/// Resolve the trailing window in seconds (env > default, #6614).
#[must_use]
pub fn resolve_empty_pool_breaker_window_secs() -> i64 {
    std::env::var(EMPTY_POOL_BREAKER_WINDOW_ENV)
        .ok()
        .and_then(|v| v.trim().parse::<i64>().ok())
        .filter(|&n| n > 0)
        .unwrap_or(DEFAULT_EMPTY_POOL_BREAKER_WINDOW_SECS)
}

/// Default label-flip flap window (#4485): the trailing window over which
/// [`SweepRegistry`] counts its own `loom:issue` <-> `loom:building` writes for
/// one issue.
pub const DEFAULT_FLAP_WINDOW_SECS: i64 = 300;

/// Default label-flip flap threshold (#4485): this many of *this registry's*
/// own label writes for one issue inside [`DEFAULT_FLAP_WINDOW_SECS`] logs a
/// loud warning. A healthy sweep writes exactly 2 (claim + release) per
/// dispatch, so 6 means "three full dispatch/revert cycles in five minutes" —
/// unambiguously a flap, never normal traffic.
pub const DEFAULT_FLAP_THRESHOLD: usize = 6;

/// Resolved per-issue dispatch-backoff parameters (Issue #4485), set on the
/// registry at construction so [`SweepRegistry::dispatch`] can enforce them
/// without a per-dispatch config read. Defaults mirror the shipped constants
/// (enabled — it is a safety backstop).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DispatchBackoffConfig {
    /// Whether the backoff is active. When `false`, dispatch neither records
    /// failures nor refuses on backoff (byte-for-byte the pre-#4485 path).
    pub enabled: bool,
    /// Delay applied after the first failed dispatch; doubled per consecutive
    /// failure.
    pub base: Duration,
    /// Ceiling on the doubling — also the idle window after which an issue's
    /// consecutive-failure tally restarts from zero.
    pub max: Duration,
}

impl Default for DispatchBackoffConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            base: Duration::from_secs(DEFAULT_DISPATCH_BACKOFF_BASE_SECS),
            max: Duration::from_secs(DEFAULT_DISPATCH_BACKOFF_MAX_SECS),
        }
    }
}

/// Per-issue dispatch-backoff bookkeeping (Issue #4485).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DispatchBackoffState {
    /// Consecutive failed dispatch outcomes for this issue.
    consecutive: u32,
    /// When the most recent failure was recorded — used to decide whether the
    /// streak is still "consecutive" (see [`DispatchBackoffConfig::max`]).
    last_failure_at: DateTime<Utc>,
    /// The instant at which the next dispatch attempt becomes allowed.
    until: DateTime<Utc>,
}

/// Compute the backoff delay for the `consecutive`-th consecutive failure
/// (Issue #4485): `base * 2^(consecutive - 1)`, clamped to `max`. Pure function
/// so the growth curve is unit-testable without a registry.
///
/// `consecutive == 0` (no recorded failure) yields [`Duration::ZERO`], and the
/// doubling saturates rather than overflowing for large streaks.
#[must_use]
pub fn backoff_delay(consecutive: u32, base: Duration, max: Duration) -> Duration {
    if consecutive == 0 || base.is_zero() {
        return Duration::ZERO;
    }
    // Saturating shift: anything past 32 doublings is max regardless.
    let factor = 2_u64.saturating_pow((consecutive - 1).min(32));
    let secs = base.as_secs().saturating_mul(factor);
    Duration::from_secs(secs).min(max)
}

/// The subset of `.loom/config.json → autonomous.workFinder.dispatchBackoff`
/// this module consumes (Issue #4485). Mirrors [`QuarantineFileConfig`]'s shape:
/// every field is `Option` so an absent key falls through to the env-var /
/// built-in-default resolution — precedence **env > config > default**.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DispatchBackoffFileConfig {
    /// `autonomous.workFinder.dispatchBackoff.enabled`.
    pub enabled: Option<bool>,
    /// `autonomous.workFinder.dispatchBackoff.baseSecs` (zero/invalid dropped).
    pub base_secs: Option<u64>,
    /// `autonomous.workFinder.dispatchBackoff.maxSecs` (zero/invalid dropped).
    pub max_secs: Option<u64>,
}

/// Read `.loom/config.json → autonomous.workFinder.dispatchBackoff` (Issue
/// #4485), soft-failing every field to `None` on a missing file, malformed
/// JSON, or an absent block — mirrors [`read_quarantine_file_config`].
#[must_use]
pub fn read_dispatch_backoff_file_config(repo_root: &Path) -> DispatchBackoffFileConfig {
    let effective = crate::config_resolver::resolve_effective_config(repo_root);
    let Some(b) =
        crate::config_resolver::get_path(&effective, "autonomous.workFinder.dispatchBackoff")
    else {
        return DispatchBackoffFileConfig::default();
    };
    DispatchBackoffFileConfig {
        enabled: b.get("enabled").and_then(serde_json::Value::as_bool),
        base_secs: b
            .get("baseSecs")
            .and_then(serde_json::Value::as_u64)
            .filter(|&s| s > 0),
        max_secs: b
            .get("maxSecs")
            .and_then(serde_json::Value::as_u64)
            .filter(|&s| s > 0),
    }
}

/// Resolve the full [`DispatchBackoffConfig`] for `repo_root` with precedence
/// **env > config > default** for every knob (Issue #4485), mirroring
/// [`resolve_quarantine_config`]. `max` is clamped up to `base` so a
/// misconfigured pair can never produce a ceiling below the first delay.
#[must_use]
pub fn resolve_dispatch_backoff_config(repo_root: &Path) -> DispatchBackoffConfig {
    let file = read_dispatch_backoff_file_config(repo_root);

    let enabled = if let Ok(v) = std::env::var(DISPATCH_BACKOFF_ENABLE_ENV) {
        matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on")
    } else {
        file.enabled.unwrap_or(true)
    };

    let base_secs = std::env::var(DISPATCH_BACKOFF_BASE_ENV)
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|&s| s > 0)
        .or(file.base_secs)
        .unwrap_or(DEFAULT_DISPATCH_BACKOFF_BASE_SECS);

    let max_secs = std::env::var(DISPATCH_BACKOFF_MAX_ENV)
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|&s| s > 0)
        .or(file.max_secs)
        .unwrap_or(DEFAULT_DISPATCH_BACKOFF_MAX_SECS)
        .max(base_secs);

    DispatchBackoffConfig {
        enabled,
        base: Duration::from_secs(base_secs),
        max: Duration::from_secs(max_secs),
    }
}

/// Compute how long a spawn must wait so that consecutive spawns are separated
/// by at least `stagger` (Issue #3887). Pure function of the last spawn instant,
/// the configured gap, and the current instant — unit-tested in isolation.
///
/// Returns `Duration::ZERO` when the stagger is disabled (zero), when no prior
/// spawn has happened, or when at least `stagger` has already elapsed.
#[must_use]
pub fn stagger_wait(last_spawn_at: Option<Instant>, stagger: Duration, now: Instant) -> Duration {
    if stagger.is_zero() {
        return Duration::ZERO;
    }
    match last_spawn_at {
        None => Duration::ZERO,
        Some(last) => {
            let elapsed = now.saturating_duration_since(last);
            stagger.checked_sub(elapsed).unwrap_or(Duration::ZERO)
        }
    }
}

impl SweepRegistry {
    /// Attach the outbound peer-claim advertiser (Issue #4028). The workspace
    /// pool / `main.rs` call this once at provision time **only when
    /// `safehouse.enabled`** — an unset publisher keeps dispatch byte-for-byte
    /// unchanged.
    pub fn set_peer_claim_publisher(&mut self, tx: tokio::sync::mpsc::Sender<ClaimAd>) {
        self.peer_claim_publisher = Some(tx);
    }

    /// Attach the shared inbound peer-claim view (Issue #4028), fed by the
    /// safehouse coordination task. Only set when `safehouse.enabled`.
    pub fn set_peer_claims(&mut self, view: Arc<Mutex<PeerClaimView>>) {
        self.peer_claims = Some(view);
    }

    /// The set of issues a **peer** host has advertised as in-flight and not yet
    /// expired (Issue #4028) — the work-finder's peer-claim skip set. Empty when
    /// no view is attached (`safehouse.enabled` false) or the mutex is poisoned
    /// (fail-open: an unavailable view never blocks dispatch). Scoped to this
    /// registry's repo via [`peer_claims::repo_slug`], so two managed repos'
    /// issue #N never cross-suppress.
    #[must_use]
    pub fn peer_claimed_issues(&self) -> HashSet<u32> {
        let Some(view) = &self.peer_claims else {
            return HashSet::new();
        };
        let repo = peer_claims::repo_slug(&self.config.workspace_root);
        match view.lock() {
            Ok(v) => v.claimed_issues_at(&repo, Instant::now()),
            Err(poisoned) => {
                log::error!("sweep_registry: peer-claim view mutex poisoned ({poisoned:?})");
                HashSet::new()
            }
        }
    }

    /// Publish a peer-claim advertisement/retraction over the safehouse room
    /// (Issue #4028). Best-effort and **non-blocking** — a bounded `try_send` so
    /// the dispatch path never waits on the coordination task, and a `Full`
    /// (safehoused outage backlog) or `Closed` (task gone) channel is a
    /// **fail-open** drop: logged once, dispatch proceeds. A no-op when no
    /// publisher is attached (`safehouse.enabled` false).
    ///
    /// Dispatch-only: `kind` is always [`peer_claims::ClaimKind::Advertise`]
    /// or [`peer_claims::ClaimKind::Retract`] from every caller in this
    /// module. [`peer_claims::ClaimKind::Completed`] (Issue #6352) is a
    /// narration-layer concern published from
    /// [`crate::safehouse::build_and_narrate_completion`] instead, over the
    /// same channel/room but never through this method — reached here only
    /// if a future caller mis-threads it, in which case this degrades to a
    /// logged no-op rather than mis-publishing a claim-shaped envelope for a
    /// kind this method was never designed to carry.
    pub(crate) fn publish_peer_claim(&self, kind: peer_claims::ClaimKind, issue: u32) {
        if kind == peer_claims::ClaimKind::Completed {
            log::warn!(
                "sweep_registry: publish_peer_claim called with ClaimKind::Completed for \
                 issue #{issue} — this is a dispatch-only path (#6352); dropping"
            );
            return;
        }
        let Some(tx) = &self.peer_claim_publisher else {
            return;
        };
        // #5921: count every outbound `Advertise` attempt (dispatch-time plus
        // each reaper re-advertisement heartbeat, #4431) BEFORE the `try_send`
        // below, so a saturated/closed channel is still visible as "we tried"
        // — the counter answers "did this host attempt to advertise", not
        // "did the room confirm delivery" (the transport is fire-and-forget
        // by design, same fail-open contract as the rest of this method).
        if kind == peer_claims::ClaimKind::Advertise {
            if let Some(view) = &self.peer_claims {
                match view.lock() {
                    Ok(mut v) => v.record_advertised(),
                    Err(poisoned) => poisoned.into_inner().record_advertised(),
                }
            }
        }
        let repo = peer_claims::repo_slug(&self.config.workspace_root);
        let host = host_identity();
        let pid = std::process::id();
        let ts = Utc::now().to_rfc3339();
        let ad = match kind {
            peer_claims::ClaimKind::Advertise => ClaimAd::advertise(issue, repo, host, pid, ts),
            peer_claims::ClaimKind::Retract => ClaimAd::retract(issue, repo, host, pid, ts),
            // Unreachable: the early return above already handles `Completed`.
            peer_claims::ClaimKind::Completed => return,
        };
        if let Err(e) = tx.try_send(ad) {
            // Fail-open: the soft claim is an optimization, never a liveness
            // dependency. Debug (not warn) so a persistent safehoused outage
            // does not spam the log once per dispatch.
            log::debug!(
                "sweep_registry: peer-claim advertisement for issue #{issue} dropped \
                 ({e}); dispatch unaffected (#4028)"
            );
        }
    }

    /// Re-advertise the peer claim of every live (`Running`/`Pending`) Issue
    /// sweep over the safehouse room (Issue #4431).
    ///
    /// The dispatch-time advertisement is a one-shot publish, and peer claims
    /// expire after [`crate::peer_claims::DEFAULT_PEER_CLAIM_TTL`] (120s) —
    /// tuned for the *soft-backoff* era when the forge label was the durable
    /// signal behind it. With claim reconciliation slowed to a healing cadence
    /// on safehouse-enabled hosts (#4431), a live sweep's claim must not
    /// silently fall out of peers' [`crate::peer_claims::PeerClaimView`]s
    /// mid-run. The reaper calls this every tick (default 30s, well under the
    /// TTL), so a live claim is refreshed ~4× per TTL window while a crashed
    /// host's claims still expire within one TTL of its last heartbeat — the
    /// crash-release property the short TTL exists for is preserved exactly.
    ///
    /// Same fail-open contract as [`Self::publish_peer_claim`]: a no-op
    /// without a publisher (`safehouse.enabled` false), and a full/closed
    /// channel drops the ad without blocking the reaper. Returns how many
    /// claims were re-advertised (for the reaper's debug line).
    pub fn readvertise_peer_claims(&self) -> usize {
        if self.peer_claim_publisher.is_none() {
            return 0;
        }
        let live: Vec<u32> = self
            .entries
            .values()
            .filter(|info| matches!(info.state, SweepState::Running | SweepState::Pending))
            .filter_map(|info| match info.kind {
                SweepKind::Issue(issue) => Some(issue),
                _ => None,
            })
            .collect();
        for issue in &live {
            self.publish_peer_claim(peer_claims::ClaimKind::Advertise, *issue);
        }
        live.len()
    }

    /// Evaluate this host's peer-coordination health at the current instant
    /// (Issue #6157) — called by the reaper on its own cadence, right after
    /// [`Self::readvertise_peer_claims`]. `None` when no view is attached
    /// (`safehouse.enabled` false), a byte-for-byte no-op matching every
    /// other peer-claim method's disabled-state contract.
    ///
    /// Multiple registries (one per managed repo, #3928) can share the SAME
    /// injected view when they are all in one safehouse-enabled fleet
    /// ([`crate::workspace_pool::WorkspacePool::inject_peer_coordination`]
    /// clones one `Arc` into every provisioned registry), so more than one
    /// reaper may call this on the same tick cadence — harmless, since
    /// [`peer_claims::PeerClaimView::evaluate_coordination`] only changes
    /// state when a grace/threshold boundary is actually crossed.
    pub fn evaluate_peer_coordination(&self) -> Option<peer_claims::CoordinationEvaluation> {
        let view = self.peer_claims.as_ref()?;
        let grace = peer_claims::resolve_coordination_degrade_grace();
        let threshold = peer_claims::resolve_coordination_recovery_threshold();
        let now = Instant::now();
        match view.lock() {
            Ok(mut v) => Some(v.evaluate_coordination(now, grace, threshold)),
            Err(poisoned) => {
                log::error!("sweep_registry: peer-claim view mutex poisoned ({poisoned:?})");
                Some(
                    poisoned
                        .into_inner()
                        .evaluate_coordination(now, grace, threshold),
                )
            }
        }
    }

    // ------------------------------------------------------------------------
    // Per-issue dispatch backoff (Issue #4485)
    // ------------------------------------------------------------------------

    /// Record a **failed** dispatch outcome for `issue` (Issue #4485) and push
    /// its next-allowed dispatch instant out by
    /// [`backoff_delay`]`(consecutive, base, max)`.
    ///
    /// Called by [`reap_once`](Self::reap_once) for a terminal outcome that made
    /// no progress **and** died fast (inside the insta-crash window) or exited
    /// cleanly with zero lifecycle progress (#4366) — including the shapes the
    /// quarantine tally deliberately does NOT charge to the issue (account
    /// exhaustion #4122, claude-wrapper pre-flight death #4386), which is
    /// precisely how a failing issue could otherwise be re-dispatched every tick
    /// forever. A *slow* checkpoint-less death is deliberately excluded: that is
    /// the mid-build (#3895) / review-stall (#3910) watchdogs' remit, each
    /// already bounded to one retry per issue.
    ///
    /// The streak restarts at `1` when the previous failure is older than
    /// [`DispatchBackoffConfig::max`], so an issue that fails rarely never
    /// accretes toward a long backoff. A no-op when the backoff is disabled.
    pub(crate) fn record_dispatch_failure(&mut self, issue: u32) {
        if !self.dispatch_backoff_config.enabled {
            return;
        }
        let now = Utc::now();
        let max_secs =
            i64::try_from(self.dispatch_backoff_config.max.as_secs()).unwrap_or(i64::MAX);
        let consecutive = match self.dispatch_backoff.get(&issue) {
            Some(prev) if (now - prev.last_failure_at).num_seconds() <= max_secs => {
                prev.consecutive.saturating_add(1)
            }
            // No prior record, or the streak went cold — start a fresh streak.
            _ => 1,
        };
        let delay = backoff_delay(
            consecutive,
            self.dispatch_backoff_config.base,
            self.dispatch_backoff_config.max,
        );
        let until =
            now + chrono::Duration::from_std(delay).unwrap_or_else(|_| chrono::Duration::zero());
        self.dispatch_backoff.insert(
            issue,
            DispatchBackoffState {
                consecutive,
                last_failure_at: now,
                until,
            },
        );
        log::info!(
            "sweep_registry: issue #{issue} dispatch backoff armed — {consecutive} consecutive \
             failed dispatch(es), next attempt allowed in {}s (#4485)",
            delay.as_secs()
        );
    }

    /// Clear `issue`'s dispatch-backoff record (Issue #4485) — called on any
    /// terminal outcome that made real progress, so a recovered issue is
    /// immediately eligible again. Returns `true` when a record existed.
    pub(crate) fn clear_dispatch_backoff(&mut self, issue: u32) -> bool {
        self.dispatch_backoff.remove(&issue).is_some()
    }

    /// Remaining dispatch backoff for `issue` at `now` (Issue #4485), or `None`
    /// when it may be dispatched immediately. `Some(Duration::ZERO)` is never
    /// returned — an elapsed window reads as `None`.
    #[must_use]
    pub fn dispatch_backoff_remaining(&self, issue: u32, now: DateTime<Utc>) -> Option<Duration> {
        if !self.dispatch_backoff_config.enabled {
            return None;
        }
        let state = self.dispatch_backoff.get(&issue)?;
        let remaining = state.until - now;
        if remaining <= chrono::Duration::zero() {
            return None;
        }
        remaining.to_std().ok().filter(|d| !d.is_zero())
    }

    /// Consecutive failed dispatch attempts recorded for `issue` (Issue #4485).
    /// `0` when no failure is on record. Test/inspection helper, mirroring
    /// [`insta_crash_count`](Self::insta_crash_count).
    #[must_use]
    pub fn dispatch_failure_count(&self, issue: u32) -> u32 {
        self.dispatch_backoff
            .get(&issue)
            .map_or(0, |s| s.consecutive)
    }

    /// Every issue whose dispatch backoff is still in effect at `now` (Issue
    /// #4485) — the set the work finder skips *before* the capacity gate, so a
    /// backed-off candidate never reserves a shared dispatch slot (mirroring
    /// [`quarantined_issues`](Self::quarantined_issues)).
    #[must_use]
    pub fn dispatch_backoff_issues(&self, now: DateTime<Utc>) -> HashSet<u32> {
        if !self.dispatch_backoff_config.enabled {
            return HashSet::new();
        }
        self.dispatch_backoff
            .iter()
            .filter(|(_, s)| s.until > now)
            .map(|(issue, _)| *issue)
            .collect()
    }

    // ------------------------------------------------------------------------
    // Cross-issue empty-pool dispatch brake (Issue #6614)
    // ------------------------------------------------------------------------

    /// Record that `issue`'s dispatch died in `spawn-claude.sh`'s
    /// token-selection pre-flight step (exit 78) — the **cross-issue** half of
    /// the empty-pool brake (Issue #6614).
    ///
    /// Called from [`finish_issue_dispatch`](Self::finish_issue_dispatch)'s
    /// synchronous #4689 branch, the one path the reaper (and therefore
    /// #4386's streak) structurally cannot observe. Prunes entries older than
    /// the window, records/refreshes this issue's timestamp, and re-evaluates
    /// the workspace pre-flight advisory — so crossing the distinct-issue
    /// threshold trips the existing #4386/#5030 hold + `PreflightAdvisory`
    /// event rather than a second, parallel breaker.
    ///
    /// Recording the same issue twice refreshes its timestamp but does not
    /// advance the count: the trip condition is N *different* issues, so one
    /// issue cycling through its own #4485 backoff can never trip it.
    pub(crate) fn record_token_selection_failure(&mut self, issue: u32) {
        let now = Utc::now();
        let window = chrono::Duration::seconds(resolve_empty_pool_breaker_window_secs());
        self.token_selection_failures
            .retain(|_, at| now - *at <= window);
        self.token_selection_failures.insert(issue, now);
        let distinct = self.token_selection_failures.len();
        let threshold = resolve_empty_pool_breaker_threshold();
        log::warn!(
            "sweep_registry: issue #{issue} died at token selection (empty/unusable token pool) \
             — {distinct} distinct issue(s) in the last {}s have now died the same way \
             (threshold {threshold}) (#6614)",
            window.num_seconds()
        );
        // Re-evaluate the workspace advisory: at/above threshold this trips the
        // #4386/#5030 dispatch hold, which logs once and emits the advisory
        // event. Below threshold it is a no-op (no state change ⇒ no event).
        self.update_preflight_advisory();
    }

    /// Clear every recorded token-selection failure (Issue #6614) — called on
    /// any dispatch that got PAST token selection, which is direct proof the
    /// pool can still hand out a credential. Re-evaluates the advisory so a
    /// tripped hold clears on the first successful probe dispatch, with no
    /// operator action. Returns `true` when something was cleared.
    ///
    /// A no-op (and, deliberately, no advisory re-evaluation) when the map is
    /// already empty, so the healthy steady state costs nothing.
    pub(crate) fn clear_token_selection_failures(&mut self) -> bool {
        if self.token_selection_failures.is_empty() {
            return false;
        }
        let cleared = self.token_selection_failures.len();
        self.token_selection_failures.clear();
        log::info!(
            "sweep_registry: a dispatch got past token selection — clearing {cleared} recorded \
             token-selection failure(s) (#6614)"
        );
        self.update_preflight_advisory();
        true
    }

    /// How many DISTINCT issues died at token selection inside the trailing
    /// window at `now` (Issue #6614). Side-effect-free: stale entries are
    /// filtered out of the count here and physically pruned on the next
    /// [`record_token_selection_failure`](Self::record_token_selection_failure).
    #[must_use]
    pub fn token_selection_failure_count(&self, now: DateTime<Utc>) -> usize {
        let window = chrono::Duration::seconds(resolve_empty_pool_breaker_window_secs());
        self.token_selection_failures
            .values()
            .filter(|at| now - **at <= window)
            .count()
    }

    /// Whether the cross-issue empty-pool brake is tripped at `now` (Issue
    /// #6614) — one of the disjuncts in
    /// [`update_preflight_advisory`](Self::update_preflight_advisory)'s trip
    /// decision.
    #[must_use]
    pub fn empty_pool_breaker_tripped(&self, now: DateTime<Utc>) -> bool {
        self.token_selection_failure_count(now) >= resolve_empty_pool_breaker_threshold()
    }

    /// Note one `loom:issue` <-> `loom:building` label write this registry
    /// performed for `issue` (Issue #4485) and warn loudly when the trailing
    /// [`DEFAULT_FLAP_WINDOW_SECS`] window holds at least
    /// [`DEFAULT_FLAP_THRESHOLD`] of them — the detection half of #4485.
    ///
    /// A healthy dispatch writes exactly two labels (claim + release), so the
    /// threshold is only reachable by repeated dispatch/revert cycling. Warns at
    /// most once per window per issue.
    pub(crate) fn note_label_flip(&mut self, issue: u32) {
        let now = Utc::now();
        let window = chrono::Duration::seconds(DEFAULT_FLAP_WINDOW_SECS);
        let flips = self.label_flip_log.entry(issue).or_default();
        flips.push_back(now);
        while flips.front().is_some_and(|t| now - *t > window) {
            flips.pop_front();
        }
        let count = flips.len();
        if count < DEFAULT_FLAP_THRESHOLD {
            return;
        }
        let recently_warned = self
            .flap_warned_at
            .get(&issue)
            .is_some_and(|t| now - *t <= window);
        if recently_warned {
            return;
        }
        self.flap_warned_at.insert(issue, now);
        log::warn!(
            "sweep_registry: issue #{issue} LABEL FLAPPING — this daemon wrote \
             loom:issue/loom:building {count} time(s) in the last {}s (threshold \
             {DEFAULT_FLAP_THRESHOLD}). A dispatch is dying immediately and being retried; check \
             the sweep log tail, `loom-daemon quarantine list`, and whether a second daemon \
             instance is dispatching the same workspace (#4485).",
            DEFAULT_FLAP_WINDOW_SECS
        );
    }

    // ------------------------------------------------------------------------
    // Dispatch
    // ------------------------------------------------------------------------

    /// Dispatch a sweep. See module docs.
    ///
    /// On idempotency hit returns the existing entry with `was_new = false`.
    ///
    /// `model` (issue #3477): when `Some` and non-empty, the spawned child
    /// receives `--model <value>` appended to the `spawn-claude.sh` argv.
    /// When `None`, no `--model` flag is emitted at all — the session/CLI
    /// default is preserved end-to-end.
    ///
    /// `effort` (issue #3716): mirrors `model` exactly. When `Some` and
    /// non-empty, the spawned child receives `--effort <level>` appended to
    /// the argv (immediately after any `--model`). When `None` or empty, no
    /// `--effort` flag is emitted at all — the session default reasoning
    /// effort is preserved end-to-end.
    ///
    /// `depends_on` (issue #3729, stacked-PR v1): when `Some(N)`, the spawned
    /// child receives `--depends-on <N>` embedded in the `-p` prompt string
    /// (immediately after `--claim-owned`; issue #4121 — NOT a sibling argv
    /// token, since `--depends-on` is not a real `claude` CLI flag),
    /// instructing `/loom:sweep` to branch its worktree/PR off
    /// `feature/issue-<N>`. When `None`, no `--depends-on` text is emitted —
    /// byte-for-byte unchanged behavior. A single optional parent (not a
    /// list) makes diamonds unrepresentable.
    pub fn dispatch(
        &mut self,
        kind: &SweepKind,
        idempotency_key: Option<String>,
        model: Option<&str>,
        effort: Option<&str>,
        depends_on: Option<u32>,
    ) -> Result<DispatchOutcome> {
        self.dispatch_inner(kind, idempotency_key, model, effort, depends_on, None)
    }

    /// Issue #4256: reaper-driven resume. When [`Self::reap_once`] observes a
    /// crashed sweep whose checkpoint shows real Builder-or-later progress
    /// (`RESUMABLE_CHECKPOINT_PHASES`) AND whose issue still has an open
    /// linked PR, it is not fresh work — it is the exact scenario the #4123
    /// open-PR guard exists to protect (an ordinary re-dispatch would
    /// double-build), but here the open PR *is* this crashed sweep's own PR
    /// and the checkpoint-resume machinery (#3373) exists precisely to pick
    /// back up at the correct phase (typically Judge) instead of redoing the
    /// Builder.
    ///
    /// This bypasses guard step 2.6 for exactly this one issue/PR pair —
    /// `resume_pr` must equal the PR the guard would itself find, so a stale
    /// or mismatched caller can never silently disable the guard. It is
    /// **only** reachable from [`Self::reap_once`]; no other call site
    /// (work-finder, IPC/CLI dispatch, epic supervisor, watchdogs) can pass a
    /// bypass, so the anti-duplicate property of #4123 is unchanged for
    /// every other dispatch path.
    pub(crate) fn dispatch_resume_after_crash(
        &mut self,
        issue: u32,
        resume_pr: u32,
    ) -> Result<DispatchOutcome> {
        self.dispatch_inner(&SweepKind::Issue(issue), None, None, None, None, Some(resume_pr))
    }

    /// The **synchronous, self-contained** composition of the
    /// [`begin_issue_dispatch`](Self::begin_issue_dispatch) →
    /// [`poll_and_classify_spawned_child`] →
    /// [`finish_issue_dispatch`](Self::finish_issue_dispatch) split (Issue
    /// #6592, mirroring the `cancel` / `begin_cancel` / `poll_cancel` /
    /// `finish_cancel` split, #3807). It holds `&mut self` (and therefore,
    /// when the registry lives behind a `Mutex`, the lock) across the whole
    /// account-selection poll, so callers that must not freeze other
    /// registry access during that poll should orchestrate the three steps
    /// themselves and release the lock across the poll (see the non-blocking
    /// IPC handler for `DispatchSweep`, `ipc.rs::dispatch_sweep_nonblocking`).
    /// Kept for `dispatch()` / `dispatch_resume_after_crash()` and unit
    /// tests, where lock contention is irrelevant.
    pub(crate) fn dispatch_inner(
        &mut self,
        kind: &SweepKind,
        idempotency_key: Option<String>,
        model: Option<&str>,
        effort: Option<&str>,
        depends_on: Option<u32>,
        resume_bypass_pr: Option<u32>,
    ) -> Result<DispatchOutcome> {
        match self.begin_issue_dispatch(
            kind,
            idempotency_key,
            model,
            effort,
            depends_on,
            resume_bypass_pr,
        )? {
            BeginIssueDispatch::Done(result) => result,
            BeginIssueDispatch::Spawned(mut prepared) => {
                let (token_name, runtime, immediate_preflight_death) =
                    poll_and_classify_spawned_child(
                        &mut prepared.child,
                        &prepared.log_path,
                        &prepared.header_anchor,
                    );
                self.finish_issue_dispatch(
                    *prepared,
                    token_name,
                    runtime,
                    immediate_preflight_death,
                )
            }
        }
    }

    /// First, lock-scoped step of a split `Issue`/`PrSet` dispatch (Issue
    /// #6592): idempotency dedup, the FULL `Issue`-kind guard chain
    /// (workspace-commands / closed-issue / open-PR / park-label / backoff /
    /// live-claim / peer-claim), claim-lock acquisition, the forge label
    /// flip, the #3887 dispatch stagger, and finally `Command::spawn()` for
    /// the child — everything through obtaining a live [`Child`] handle.
    /// Deliberately does **not** poll the child for its account-selection log
    /// line: that is the one genuinely multi-second wait in the whole
    /// dispatch path (bounded by `TOKEN_NAME_CAPTURE_TIMEOUT`, up to 5s), and
    /// it does not need the registry mutex — only the child handle and its
    /// log file. Splitting it out here is what lets the IPC layer release
    /// the registry mutex for that wait (Issue #6592's "double-blocking
    /// hazard": before this split, `handle_request`'s `DispatchSweep` arm
    /// held the registry mutex through the FULL guard chain AND the poll,
    /// serializing concurrent dispatches behind each other's poll wait and
    /// starving unrelated requests like `ListSweeps` on the same mutex).
    ///
    /// `PrSet` dispatch has no single-issue guard chain (see
    /// [`Self::dispatch_prset_inner`]'s doc comment) and no long poll to
    /// split around, so it is dispatched here fully synchronously and its
    /// result returned as [`BeginIssueDispatch::Done`] — unchanged behavior,
    /// still holding the mutex for its whole (comparatively short) duration.
    ///
    /// Returns [`BeginIssueDispatch::Done`] when the final [`DispatchOutcome`]
    /// is already known — an idempotency hit, a guard refusal, a `PrSet`
    /// result, or a spawn failure — and [`BeginIssueDispatch::Spawned`] when
    /// the child has been spawned and the caller must now poll it (via
    /// [`poll_and_classify_spawned_child`], OUTSIDE any lock it wants to
    /// release) and then call [`Self::finish_issue_dispatch`].
    ///
    /// # Idempotency-key dedup inside the unlocked-poll window
    ///
    /// Step 1's dedup consults
    /// [`find_running_by_key`](Self::find_running_by_key), which matches only
    /// entries already in `self.entries` — and this dispatch's entry is not
    /// inserted until [`finish_issue_dispatch`](Self::finish_issue_dispatch),
    /// after the poll, under a re-taken lock. So a same-idempotency-key retry
    /// that lands **between** a `Spawned` return here and the matching
    /// `finish_issue_dispatch` (i.e. inside the window the mutex is released
    /// for, bounded by `TOKEN_NAME_CAPTURE_TIMEOUT`, up to ~5s) MISSES the
    /// short-circuit above and falls through to the guard chain.
    ///
    /// **No double-spawn results** — the guard chain refuses such a retry
    /// before any second `Command::spawn()`, via one of two independent
    /// mechanisms (which one fires is platform-dependent; both are treated as
    /// correct):
    ///
    /// - The #4556 live-claim guard's **argv process-scan** leg
    ///   (`live_claim::live_sweep_process_in`) matches the just-spawned child
    ///   directly, needing neither a tracked entry nor lock ownership. It
    ///   fires wherever the platform exposes another process's argv (Linux
    ///   `/proc`). Note the guard's *bookkeeping* legs genuinely are blind
    ///   here — `has_tracked_sweep_for` is still false, and the claim lock's
    ///   `owner.json` still holds this daemon's own pid because
    ///   `record_child_pid_in_lock` runs in `finish_issue_dispatch` — so do
    ///   not reason about this window from those legs alone.
    /// - The atomic `acquire_lock` mkdir at step 3 is the unconditional,
    ///   platform-independent backstop: `lock collision`.
    ///
    /// The behavior that *does* differ inside this window is the retry's
    /// caller-visible outcome: a hard `Err` (of either shape above) instead of
    /// the graceful `was_new: false` hand-back it receives before or after.
    /// This is accepted rather than papered over — the realistic client retry
    /// the split targets follows a 30s ack timeout, well past a ~5s window —
    /// and is pinned by
    /// `same_key_retry_during_the_unlocked_poll_window_is_refused_not_double_spawned`
    /// in this module's tests.
    pub(crate) fn begin_issue_dispatch(
        &mut self,
        kind: &SweepKind,
        idempotency_key: Option<String>,
        model: Option<&str>,
        effort: Option<&str>,
        depends_on: Option<u32>,
        resume_bypass_pr: Option<u32>,
    ) -> Result<BeginIssueDispatch> {
        // Runtime admission is deliberately the first dispatch decision:
        // before idempotency/account selection, claim lock, forge mutation,
        // log header, or child spawn. A full sweep remains one runtime and is
        // checked against Builder's (strongest lifecycle) requirements.
        let runtime_admission = if self.config.skip_label_flip {
            None // hermetic unit fixtures do not install runtime manifests
        } else {
            match crate::runtime_admission::resolve_and_admit(
                &self.config.workspace_root,
                "sweep-lifecycle",
                None,
            ) {
                Ok(admitted) => Some(admitted),
                Err(rejection) => {
                    // Refused work still gets an event representation (#4494):
                    // `sweep.global.dispatch` describes admitted work only, so
                    // without this the refusal was invisible on the bus. This
                    // is a PURE publish — no claim lock, no account selection,
                    // no log header, no forge call — so the pre-claim
                    // side-effect contract is preserved.
                    self.emit_event(Event::SweepGlobalRuntimeRejected {
                        kind: kind.clone(),
                        role: rejection.role.clone(),
                        runtime: rejection.runtime.clone(),
                        runtime_source: rejection.source.clone(),
                        unmet_capabilities: rejection.unmet_capabilities.clone(),
                        reason: rejection.reason.clone(),
                        // Stamped by `emit_event` -> `set_repo_if_absent`.
                        repo: None,
                    });
                    return Err(anyhow::Error::new(rejection));
                }
            }
        };

        // 1. Idempotency dedup against Running entries.
        if let Some(ref key) = idempotency_key {
            if let Some(existing) = self.find_running_by_key(key) {
                return Ok(BeginIssueDispatch::Done(Ok(DispatchOutcome {
                    sweep_id: existing.sweep_id.clone(),
                    pid: existing.pid,
                    token_name: existing.token_name.clone(),
                    log_path: existing.log_path.clone(),
                    was_new: false,
                })));
            }
        }

        // 2. `Issue` and `PrSet` dispatch diverge here (Issue #5342): `PrSet`
        //    has no single issue number to run the issue-keyed guard chain
        //    (2.4-2.9 below) against — it claims no `loom:building` label and
        //    is tracked via a per-PR claim lock instead (see
        //    `dispatch_prset_inner` / `SweepRegistry::pr_lock_dir`).
        let issue_number = match kind {
            SweepKind::Issue(n) => *n,
            SweepKind::PrSet(prs) => {
                return Ok(BeginIssueDispatch::Done(self.dispatch_prset_inner(
                    prs,
                    kind,
                    idempotency_key,
                    model,
                    effort,
                    runtime_admission,
                )));
            }
        };

        // 2.4 Workspace-commands guard (Issue #4027). A workspace registered
        //     (or hot-added, #3926) without ever running `loom-daemon init` —
        //     e.g. a bare `git clone` on a second daemon host — has `.git`/
        //     `.loom` so it "looks like" a workspace, but lacks the
        //     install-not-committed `.claude/commands/loom/` slash commands.
        //     Dispatching `/loom:sweep <N>` into it insta-crashes the child on
        //     `Unknown command: /loom:sweep` within seconds, and because it
        //     exits before any checkpoint/worktree exists, the reaper reverts
        //     `loom:building` -> `loom:issue` and the work-finder re-dispatches
        //     on the next tick: an infinite fast-fail loop burning a rotated
        //     token roughly every tick, forever. Checked FIRST — before even
        //     the closed-issue guard's `gh` probe below — because it is a
        //     single local `stat` versus a subprocess spawn: a misconfigured
        //     workspace should cost as little as possible per tick, and zero
        //     tokens either way. Skipped when label flips are disabled (test
        //     fixtures exercising pure in-memory dispatch mechanics without a
        //     fully Loom-managed workspace on disk), mirroring the #4088
        //     closed-issue guard's skip condition below.
        if !self.config.skip_label_flip && !self.config.has_sweep_command() {
            return Err(WorkspaceCommandsMissingDispatchError {
                workspace: self.config.workspace_root.clone(),
            }
            .into());
        }

        // 2.5 Closed-issue guard (Issue #4088, widened in #4504). All three
        //     watchdogs (startup #3887, mid-build-death #3895, review-stall
        //     #3910) re-dispatch through this method, and `gh issue edit`
        //     succeeds on a closed issue, so nothing else stops a watchdog
        //     false-positive from re-claiming an issue whose PR already merged.
        //     Placing the guard here — before the lock/label flip — covers all
        //     three call sites with one check. #4504 widened the probe from a
        //     `state == "CLOSED"` string match to a REST payload that also
        //     reports PR-ness, so a dispatch number that resolves to a pull
        //     request (open, closed, or merged — issues and PRs share one number
        //     namespace) is refused too instead of slipping through the fail-open
        //     arm. Best-effort and fail-open: a forge lookup error returns `None`
        //     and dispatch proceeds, so a `gh` outage can never wedge the daemon.
        //     Skipped when label flips are disabled (test fixtures without `gh`
        //     credentials).
        if !self.config.skip_label_flip && self.issue_is_closed_or_pr(issue_number) == Some(true) {
            return Err(anyhow!(
                "refusing to dispatch issue #{issue_number}: it is closed on the forge, or the \
                 number resolves to a pull request rather than an open issue (#4088/#4504 \
                 closed-issue guard). A watchdog re-dispatch must not re-claim a closed/merged \
                 issue or a PR number."
            ));
        }

        // 2.6 Open-PR guard (Issue #4123). Every in-memory dedup signal — the
        //     idempotency key, the in-flight set, the `loom:building` label —
        //     is scoped to the running sweep's lifetime and clears when the
        //     parent exits (`reconstruct()` even drops the idempotency key on a
        //     daemon restart). So an issue whose approved PR is still open looks
        //     identical to fresh work the moment its sweep dies, and every
        //     caller that routes through `dispatch()` — the work-finder, the
        //     epic supervisor, the IPC/CLI dispatch, and all three watchdogs
        //     (startup #3887, mid-build-death #3895, review-stall #3910) — would
        //     re-dispatch it, redoing finished work against a scarce token pool.
        //     The forge's closes-graph is the one durable signal that survives
        //     process death and restarts, so this guard consults it, right after
        //     the closed-issue guard and before the lock/label flip so a single
        //     check covers all six call sites. Keys on PR *openness* only, never
        //     on review labels — driving an open PR forward is the
        //     Judge/Champion/Doctor path's job, not the issue work-finder's.
        //     Best-effort and fail-open: any forge error/timeout/unparseable
        //     output returns `None` and dispatch proceeds, so a `gh` outage (or
        //     a Gitea workspace — this is GitHub-only, like `issue_is_closed_or_pr`)
        //     can never wedge the daemon. Skipped when label flips are disabled
        //     (test fixtures without `gh` credentials), mirroring 2.5.
        //
        //     Issue #6593: the refusal is not a dead end — `OpenPrDispatchError`'s
        //     `Display` names the reachable alternative for this candidate class,
        //     a `SweepKind::PrSet` dispatch (#5342) for the PR the probe found,
        //     which drives Judge/Doctor -> Merge on it without rebuilding. This
        //     guard deliberately does NOT auto-convert: an issue-keyed dispatch
        //     claims an issue and flips `loom:building`, a PrSet dispatch claims
        //     PR locks instead, so silently substituting one for the other would
        //     hand the caller a sweep it did not ask for (and a different
        //     idempotency/dedup domain). Naming it is the caller's cue to re-issue.
        //
        //     Issue #4256: `resume_bypass_pr` — set only by
        //     `dispatch_resume_after_crash`, itself only reachable from
        //     `reap_once` — exempts a resume of THIS issue's own crashed sweep
        //     from the guard, but only when it names the exact PR the guard
        //     would find; any other PR (or none) still refuses normally, so a
        //     stale/mismatched resume can never widen into a blanket bypass.
        if !self.config.skip_label_flip {
            // Fail-open (#4452): only a VERIFIED `Open(pr)` blocks; both
            // `NoneOpen` and `ProbeFailed` fall through and proceed, so a forge
            // outage can never wedge dispatch (unchanged pre-#4452 behavior).
            if let OpenPrProbe::Open(pr) = self.probe_open_linked_pr(issue_number) {
                if resume_bypass_pr != Some(pr) {
                    return Err(OpenPrDispatchError {
                        issue: issue_number,
                        pr,
                    }
                    .into());
                }
                log::info!(
                    "issue #{issue_number}: reaper-driven resume dispatch bypassing the #4123 \
                     open-PR guard for its own PR #{pr} (#4256)"
                );
            }
        }

        // 2.7 Park-label guard (Issue #4444). The work-finder's `SKIP_LABELS`
        //     hard-skip is enforced only in *its own* candidate query, so it
        //     covers exactly one of the six dispatch routes. Every other route
        //     — all three watchdogs (#3887 / #3895 / #3910), the reaper's
        //     checkpoint-resume (#4256), the epic supervisor, the IPC/CLI
        //     `dispatch_sweep` — funnels through here, and until this guard
        //     existed none of them ever re-read the forge labels. A
        //     `loom:blocked` / `loom:operator-only` park applied *after* the
        //     original dispatch was therefore invisible to every re-dispatch
        //     path, and the daemon overrode a deliberate human/agent park
        //     (observed on #4366). Placing the check here — before the
        //     lock/label flip — covers all routes with one probe.
        //
        //     Three properties are load-bearing:
        //
        //     - It guards on `PARK_LABELS` only, NOT the full `SKIP_LABELS`
        //       set. `loom:building` is legitimately present on a watchdog
        //       re-dispatch or a checkpoint-resume of the daemon's OWN claim,
        //       so refusing it would break the review-stall watchdog's
        //       cancel-and-re-dispatch and the reaper's resume.
        //     - It is NOT exempted by `resume_bypass_pr`. The #4256 bypass
        //       covers step 2.6 (its own open PR) and nothing else: a park
        //       applied after the crash must still stop the resume, which is
        //       the exact defect this guard fixes.
        //     - It probes over **REST** (`gh api repos/{owner}/{repo}/issues/N`),
        //       a separate rate-limit bucket from the GraphQL calls 2.5/2.6
        //       ride, so the park still holds while the GraphQL quota is
        //       exhausted — the condition under which the #4123 guard failed
        //       open during the 2026-07-29 incident.
        //
        //     Best-effort and fail-open, mirroring 2.5/2.6: any forge
        //     error/timeout/unresolvable repo returns `None` and dispatch
        //     proceeds, so a `gh` outage can never wedge the daemon. Skipped
        //     entirely when label flips are disabled (test fixtures without
        //     `gh` credentials).
        if !self.config.skip_label_flip {
            if let Some(label) = self.first_park_label(issue_number) {
                log::info!(
                    "issue #{issue_number}: refusing dispatch — the issue carries `{label}`, a \
                     deliberate park that every dispatch route must respect (#4444 park-label \
                     guard); clear the label to re-enable automation"
                );
                return Err(ParkedIssueDispatchError {
                    issue: issue_number,
                    label,
                }
                .into());
            }
        }

        // 2.8 Per-issue dispatch backoff (Issue #4485). The quarantine backstop
        //     (#3939) only engages after three *tally-eligible* insta-crashes,
        //     and both the account-exhaustion (#4122) and claude-wrapper
        //     pre-flight (#4386) carve-outs deliberately leave that tally
        //     untouched — so an issue whose every dispatch dies in that shape
        //     was re-dispatched on every tick forever, flapping
        //     `loom:issue`/`loom:building` at the reap→restore→re-poll cadence
        //     (~90 label events in 7 minutes on #4398). This guard caps the
        //     *rate* rather than the *cause*: any no-progress terminal outcome
        //     arms an exponential per-issue window (see
        //     `record_dispatch_failure`) and dispatch refuses until it elapses.
        //
        //     Placed with the other pre-flip guards (2.4-2.7) so one check
        //     covers every dispatch call site — work-finder, epic supervisor,
        //     IPC/CLI, and all three watchdogs — and, critically, so a refusal
        //     costs **no lock, no label write, and no forge round trip of its
        //     own**: the refusal itself can never contribute to a flap. Unlike
        //     2.4-2.7 it is NOT gated on `skip_label_flip`: it is pure
        //     in-memory bookkeeping with no `gh` dependency, and the flap it
        //     prevents is driven by dispatch cadence, not by credentials.
        //
        //     Runs *after* the 2.7 park-label guard, deliberately. When an issue
        //     is both parked and inside a backoff window, the park is the
        //     durable operator decision and the more actionable refusal, so it
        //     wins and the skip is attributed to `labeled-skip` rather than to
        //     `backoff-skip` (which advertises an imminent auto-retry that a
        //     park forbids). The two guards share no state: a refusal here never
        //     spawns a sweep, so no refusal — park or backoff — can ever call
        //     `record_dispatch_failure` and arm/extend a window, and a backoff
        //     window keeps decaying on wall-clock time while an issue sits
        //     parked. Clearing the park therefore re-exposes any *still-live*
        //     window, which is correct: the park did not prove the failing
        //     dispatch loop fixed.
        //
        //     Exempt: the reaper-driven resume (#4256, `resume_bypass_pr`),
        //     which re-dispatches an issue whose own PR is open and is already
        //     bounded by `MAX_RESUME_ATTEMPTS`. Never a work-finder loop.
        if resume_bypass_pr.is_none() {
            if let Some(remaining) = self.dispatch_backoff_remaining(issue_number, Utc::now()) {
                let consecutive = self.dispatch_failure_count(issue_number);
                log::info!(
                    "sweep_registry: refusing to dispatch issue #{issue_number} — \
                     {consecutive} consecutive failed dispatch(es), {}s of backoff remaining \
                     (#4485)",
                    remaining.as_secs()
                );
                return Err(DispatchBackoffError {
                    issue: issue_number,
                    consecutive,
                    retry_after_secs: remaining.as_secs(),
                }
                .into());
            }
        }

        // 2.9 Live-claim guard (Issue #4556). The single hard, dispatch-time
        //     refusal for an issue whose sweep is *confirmed still running*.
        //
        //     Every guard before this one, and `acquire_lock` below, keys on
        //     state that a re-dispatch path has already invalidated by the time
        //     it dispatches:
        //
        //     - `acquire_lock` refuses only on the lock **existing**, and every
        //       reaper / cancel / watchdog path releases that lock *first*, on
        //       the strength of its own dead-sweep verdict.
        //     - The `loom:building` label is reverted by
        //       `claim_reconciliation` the moment a recorded PID looks dead
        //       (confirmed on #4275 at 03:08:15Z), re-exposing the issue to the
        //       work-finder.
        //     - The in-memory entry set (`issue_has_active_sweep`,
        //       `in_flight()`) is scoped to ONE daemon process — invisible to a
        //       second `loom-daemon` instance on the same host, which is where
        //       3 of the 7 #4275 dispatches came from.
        //
        //     `live_claim::probe` asks the strictly stronger question instead:
        //     is a sweep process for this issue *alive right now*? Its three
        //     evidence legs (live lock owner / machine-level `~/.loom/sweeps.json`
        //     journal / `/proc` scan for a `/loom:sweep <N>` process rooted in
        //     this workspace) each survive a lock release, a label revert, a
        //     daemon restart, AND a second daemon instance.
        //
        //     Placed with the other pre-flip guards so ONE check covers all six
        //     dispatch routes — work-finder, epic supervisor, IPC/CLI, and all
        //     three watchdogs — and so a refusal costs no lock, no label write,
        //     and no forge round trip. Deliberately NOT exempted by
        //     `resume_bypass_pr`: a checkpoint-resume of a crashed sweep is
        //     still a duplicate if the "crashed" sweep turns out to be alive.
        //     Not gated on `skip_label_flip` either — it is pure local
        //     filesystem bookkeeping with no `gh` dependency.
        //
        //     Fail-open: `probe` returns `None` on every ambiguity (missing or
        //     corrupt `owner.json`, unreadable journal, no `/proc`), and treats
        //     a zombie PID as dead, so a garbage file can never wedge an issue.
        if let Some(evidence) = self.live_claim_evidence(issue_number) {
            log::warn!(
                "sweep_registry: refusing to dispatch issue #{issue_number} — {evidence} \
                 (#4556 live-claim guard). This is the duplicate-dispatch storm guard; the \
                 live sweep keeps its claim and runs its own lifecycle."
            );
            return Err(LiveClaimDispatchError {
                issue: issue_number,
                evidence,
            }
            .into());
        }

        // 2.95 Peer-claim guard (Issue #5789, upgrading #4028's soft claim from
        //      detection into enforcement). `self.peer_claimed_issues()` is a
        //      free, in-memory lookup — no `gh` round trip, no lock, nothing to
        //      unwind — fed by the safehouse coordination task's inbound read
        //      of peer advertisements. If a **peer** host (never this host's own
        //      ad — `PeerClaimView::observe_at` ignores self-claims) still has a
        //      live claim on this issue, dispatching here would duplicate a
        //      sweep another host already started. This closes the same gap for
        //      every `dispatch()` call site (work-finder, epic supervisor,
        //      IPC/CLI, all three watchdogs) that the work-finder's own
        //      pre-tick `peer_claimed()` filter only ever covered for ITS one
        //      route (#4028's original scope).
        //
        //      A no-op (empty set) when no view is attached (`safehouse.enabled`
        //      false) or the mutex is poisoned — same fail-open contract as
        //      `peer_claimed_issues` itself, so a safehouse outage can never
        //      wedge dispatch.
        if self.peer_claimed_issues().contains(&issue_number) {
            self.collision_count += 1;
            // #5921: the proof-of-mechanism counter — a peer-claim view can
            // be non-empty (received > 0) yet never actually prevent a
            // duplicate if this guard were ever bypassed; this increments
            // exactly when it did.
            if let Some(view) = &self.peer_claims {
                match view.lock() {
                    Ok(mut v) => v.record_dispatch_skipped(),
                    Err(poisoned) => poisoned.into_inner().record_dispatch_skipped(),
                }
            }
            log::warn!(
                "sweep_registry: BACKING OFF dispatch of issue #{issue_number} — a peer host's \
                 soft-claim advertisement is still live for this issue (#4028/#5789 peer-claim \
                 enforcement, running collision count={count}). Refusing to duplicate a sweep \
                 another host already claimed; no lock was acquired and no advertisement was \
                 published by this host for this issue.",
                count = self.collision_count,
            );
            return Err(CollisionDispatchError {
                issue: issue_number,
                source: CollisionSource::PeerClaim,
            }
            .into());
        }

        // 3. Acquire the claim lock atomically.
        let sweep_id = generate_sweep_id(kind);
        self.acquire_lock(issue_number, &sweep_id)?;

        // 3a. Soft cross-host claim (Issue #4028): advertise this claim over the
        //     shared safehouse room **before** the non-atomic label flip below,
        //     so a peer daemon backs off far faster than the `loom:building`
        //     label propagates. Best-effort, non-blocking, fail-open (a no-op
        //     when `safehouse.enabled` is false) — the room broadcast is a soft
        //     claim/fast backoff, NOT a mutex; the forge label remains the
        //     human-visible claim signal and Phase 2 is the atomic authority.
        self.publish_peer_claim(peer_claims::ClaimKind::Advertise, issue_number);

        // 4. Flip the forge label loom:issue -> loom:building (best-effort
        //    when the dispatcher has gh credentials; tests opt out via
        //    `skip_label_flip`).
        //
        // `episode_start` anchors the claim-then-verify-order tie-break below
        // (4c, Issue #6287) to THIS dispatch attempt's own local clock,
        // captured before the flip so it bounds "which lease comments belong
        // to this claim episode" without re-reading `Utc::now()` after the
        // round trips below have already spent wall-clock time.
        let episode_start = Utc::now();
        let mut lease_order_yield: Option<(String, String)> = None;
        if !self.config.skip_label_flip {
            // 4a. Cross-host collision guard (Issue #4085, Phase 0 of #4028;
            //     upgraded from detection-only to enforcement by #5789): read
            //     the pre-flip label state and, when it shows a peer host
            //     already claimed this issue, BACK OFF instead of proceeding.
            //     A no-op (returns `None`, nothing logged/counted) when
            //     detection is disabled (default off — this opt-in probe costs
            //     one extra `gh issue view` round trip), so the disabled flip
            //     path stays byte-for-byte unchanged. Must run BEFORE the flip:
            //     once this host flips, a collided issue is indistinguishable
            //     from a clean one — and this is also the fallback net for the
            //     race the cheaper 2.95 peer-claim guard above can miss (no
            //     safehouse view attached, or the peer's ad simply hadn't
            //     arrived yet): a host that proceeded past 2.95 still gets
            //     caught — and logged — here when the collision is confirmed
            //     against the forge's own label state.
            if let Some(CollisionClass::Collision { labels }) =
                self.detect_and_record_collision(issue_number)
            {
                log::warn!(
                    "sweep_registry: BACKING OFF dispatch of issue #{issue_number} — the \
                     pre-flip forge-label read confirmed another host already claimed it \
                     (#4085/#5789 enforcement); reverting the claim lock and peer-claim \
                     advertisement this host had already applied instead of flipping the label \
                     and duplicating the sweep."
                );
                self.publish_peer_claim(peer_claims::ClaimKind::Retract, issue_number);
                let _ = self.release_lock_owned(issue_number, &sweep_id);
                return Err(CollisionDispatchError {
                    issue: issue_number,
                    source: CollisionSource::ForgeLabel { labels },
                }
                .into());
            }
            match self.flip_label_to_building(issue_number) {
                Ok(()) => {
                    // 4b. Write the lease record (Issue #6179, Epic #6165
                    //     Phase 1): a best-effort forge comment documenting
                    //     which host/sweep now holds this claim, posted only
                    //     on a confirmed successful flip — a failed flip
                    //     means there is no claim to lease. Write-only: no
                    //     reclamation/dispatch logic reads this yet (Phase 2).
                    self.write_lease_comment(issue_number, &sweep_id);

                    // 4c. Claim-then-verify-order tie-break (Issue #6287,
                    //     Epic #6165 Phase 2): re-read every live lease
                    //     comment on the issue and check whether THIS
                    //     dispatcher's own comment is the earliest one, by
                    //     forge-assigned comment order — the closes-the-gap
                    //     mechanism for the exact race the label flip above
                    //     cannot itself prevent (it is unconditionally
                    //     idempotent, so two near-simultaneous dispatchers
                    //     both succeed at it). A losing verdict is recorded
                    //     here and acted on AFTER `note_label_flip` below, so
                    //     flap-detection bookkeeping for this real flip stays
                    //     unconditional either way.
                    if let LeaseOrderDecision::Yield {
                        earliest_host,
                        earliest_sweep_id,
                    } = self.resolve_lease_order(issue_number, &sweep_id, episode_start)
                    {
                        lease_order_yield = Some((earliest_host, earliest_sweep_id));
                    }
                }
                Err(e) => {
                    log::warn!(
                        "label flip for issue #{issue_number} failed (continuing dispatch): {e}"
                    );
                }
            }
            // Flap detection (#4485): count this claim write and warn if this
            // issue's label is being cycled far faster than a healthy
            // dispatch/complete rhythm can explain.
            self.note_label_flip(issue_number);
        }

        // 4d. Act on a lost claim-then-verify-order tie-break (Issue #6287):
        //     yield BEFORE any real work — no builder spawn, no worktree
        //     creation/entry — for the losing claim. The forge's
        //     `loom:building` label is deliberately left untouched (it is
        //     already correct: idempotent across both racing flips, and
        //     reverting it here would destroy the earlier claimant's only
        //     cross-host mutex out from under its still-live sweep — the
        //     loom#5270 failure mode). Only this host's own, purely local
        //     side effects are unwound: the peer-claim advertisement (3a)
        //     and the claim lock (3), mirroring the #5236/#4689 unwind
        //     branches below for every side effect this dispatch attempt
        //     itself controls exclusively.
        if let Some((earliest_host, earliest_sweep_id)) = lease_order_yield {
            log::warn!(
                "sweep_registry: YIELDING dispatch of issue #{issue_number} sweep_id={sweep_id} \
                 — a lease comment from host={earliest_host} sweep={earliest_sweep_id} has an \
                 earlier forge-assigned comment order (#6287 claim-then-verify-order tie-break, \
                 Epic #6165 Phase 2). Standing down before spawning a builder; the \
                 `loom:building` label is left in place since it already protects the earlier \
                 claimant's own winning lease."
            );
            self.publish_peer_claim(peer_claims::ClaimKind::Retract, issue_number);
            let _ = self.release_lock_owned(issue_number, &sweep_id);
            self.post_lease_yield_comment(
                issue_number,
                &sweep_id,
                &earliest_host,
                &earliest_sweep_id,
            );
            // #6350: a lost lease-order tie-break is a no-progress outcome for
            // THIS host exactly like the reaper's #4366 backstop below —
            // arming this host's own per-issue dispatch backoff (#4485) here
            // means the very next work-finder tick does not immediately
            // re-attempt (and re-lose) the identical race against the same
            // still-live earlier claimant. This is purely a redispatch-rate
            // damper: it never touches the quarantine tally (this issue is
            // not broken, it already has an owner), and it does not affect
            // the `loom:building` label the earlier claimant still holds.
            self.record_dispatch_failure(issue_number);
            return Err(LeaseOrderDispatchError {
                issue: issue_number,
                sweep_id: sweep_id.clone(),
                earliest_host,
                earliest_sweep_id,
            }
            .into());
        }

        // 5. Compute the log path and spawn the child.
        //
        // Serialize concurrent child startups (Issue #3887): enforce a minimum
        // wall-clock gap since the previous spawn so a burst of back-to-back
        // dispatches does not launch many `claude`/`mcp-loom` startups in the
        // same ~1s window (the 0-HTTPS MCP-init race). `dispatch` holds the
        // registry mutex here, so the brief stagger sleep also serializes the
        // contended startup step across concurrent dispatch callers. A zero
        // stagger (the default outside production / in tests) is a no-op.
        self.apply_dispatch_stagger();
        let log_path = self.compute_log_path(issue_number);
        let (child, header_anchor) = match self.spawn_child_process(
            kind,
            &log_path,
            &sweep_id,
            model,
            effort,
            depends_on,
            runtime_admission.as_ref(),
        ) {
            Ok(spawned) => spawned,
            Err(e) => {
                // Issue #5236: `spawn_child_process` can fail for reasons that
                // have nothing to do with the issue itself (e.g. a registered
                // workspace whose `.loom/scripts/` is missing
                // `spawn-worker.sh` — `resolve_spawn_bin` errors before any
                // process is ever spawned). Unlike the #4689 branch in
                // `finish_issue_dispatch`, this is a synchronous `Err`, not a
                // synchronously-observed dead child — but the side effects
                // already applied above (claim lock, peer-claim
                // advertisement, label flip) are identical, and leaving them
                // in place is exactly what wedges every retry: the leaked
                // lock's `owner_pid` is this daemon's own (still-alive) pid,
                // which the #4556 live-claim guard then reads as a
                // confirmed-live claim forever (until an operator manually
                // removes the lock dir). Unwind the same three side effects
                // the #4689 branch reverts, so a second dispatch attempt
                // starts from a clean slate instead of a permanent wedge.
                log::warn!(
                    "sweep_registry: issue #{issue_number} sweep_id={sweep_id} failed to spawn \
                     — reverting the claim lock, label, and peer-claim advertisement instead of \
                     leaking them (#5236): {e:#}"
                );
                if !self.config.skip_label_flip {
                    let _ = self.restore_label_to_ready(issue_number);
                    self.note_label_flip(issue_number); // #4485 flap detection
                }
                self.publish_peer_claim(peer_claims::ClaimKind::Retract, issue_number);
                let _ = self.release_lock_owned(issue_number, &sweep_id);
                return Err(e.context("failed to spawn sweep child"));
            }
        };

        Ok(BeginIssueDispatch::Spawned(Box::new(PreparedIssueDispatch {
            child,
            header_anchor,
            log_path,
            issue_number,
            sweep_id,
            kind: kind.clone(),
            idempotency_key,
            model: model.filter(|m| !m.is_empty()).map(String::from),
            effort: effort.filter(|e| !e.is_empty()).map(String::from),
            depends_on,
            runtime_admission,
        })))
    }

    /// Second, lock-scoped step of a split `Issue` dispatch (Issue #6592):
    /// record the outcome of a child spawned by
    /// [`Self::begin_issue_dispatch`] and already polled by
    /// [`poll_and_classify_spawned_child`] (which the caller must run
    /// WITHOUT the registry mutex held, between the two lock-scoped steps).
    ///
    /// On a confirmed preflight death (token selection failed), unwinds the
    /// claim lock / label flip / peer-claim advertisement exactly like
    /// `begin_issue_dispatch`'s own spawn-failure branch, and returns `Err`.
    /// On success, records the entry, the sweep journal, and the
    /// `sweep.global.dispatch` event, then returns the same
    /// [`DispatchOutcome`] shape [`Self::dispatch_inner`] has always
    /// returned.
    pub(crate) fn finish_issue_dispatch(
        &mut self,
        prepared: PreparedIssueDispatch,
        token_name: String,
        runtime: String,
        immediate_preflight_death: Option<&'static str>,
    ) -> Result<DispatchOutcome> {
        let PreparedIssueDispatch {
            child,
            header_anchor: _,
            log_path,
            issue_number,
            sweep_id,
            kind,
            idempotency_key,
            model,
            effort,
            depends_on,
            runtime_admission,
        } = prepared;

        // Issue #4689: the child already died — synchronously observed,
        // before this dispatch call has returned — from `spawn-claude.sh`'s
        // token-selection preflight step (exit 78 / `EX_CONFIG`). Absent this
        // check, dispatch would proceed exactly like a healthy launch: label
        // flipped to `loom:building`, a `Running` entry recorded, and the
        // caller told `Success` with `Token: unknown` — which reads as
        // cosmetic rather than as the hard failure it is. The operator then
        // has to grep the per-sweep log to discover nothing launched (the
        // reported bug). Bail out HERE, before any of the success-path
        // bookkeeping below (`self.children`/`self.entries` insert, sweep
        // journal record, `sweep.global.dispatch` event) has happened, so the
        // only side effects to unwind are the ones already applied by
        // `begin_issue_dispatch`: the peer-claim advertisement (3a), the
        // label flip (4), and the claim lock (3). Reverting those returns the
        // issue to exactly the pre-dispatch state, and the caller gets a real
        // `Err` — surfaced by both the CLI (`Daemon rejected the dispatch:
        // ...`) and `mcp__loom__dispatch_sweep` (`Failed`) — instead of a
        // false `Success`. Scoped deliberately narrow (only the specific
        // `preflight-token-selection-failed` class, not every preflight
        // death shape) to keep this synchronous fast-path change bounded;
        // other preflight deaths keep flowing through the existing
        // `reap_once`-driven classification/backoff/quarantine machinery
        // unchanged.
        if immediate_preflight_death == Some("preflight-token-selection-failed") {
            log::warn!(
                "sweep_registry: issue #{issue_number} sweep_id={sweep_id} child exited \
                 immediately after token selection failed (#4689) — reverting the claim and \
                 reporting dispatch as a failure instead of a misleading Success"
            );
            if !self.config.skip_label_flip {
                let _ = self.restore_label_to_ready(issue_number);
                self.note_label_flip(issue_number); // #4485 flap detection
            }
            self.publish_peer_claim(peer_claims::ClaimKind::Retract, issue_number);
            let _ = self.release_lock_owned(issue_number, &sweep_id);
            // Issue #6614: this path returns BEFORE any `entries` record exists,
            // so the reaper never sees this death — and therefore neither the
            // per-issue backoff (#4485) nor the cross-issue pre-flight streak
            // (#4386) was ever armed by it. Both are armed here instead, at the
            // only place that observes it:
            //   * per-issue  — so the very next work-finder tick does not
            //     immediately re-dispatch THIS issue into the same dead pool
            //     (the "no backoff" half of the reported crash-loop);
            //   * cross-issue — so N *different* issues dying this way trips
            //     the #4386/#5030 workspace hold with its one loud advisory,
            //     instead of the whole backlog re-cycling every tick forever.
            self.record_dispatch_failure(issue_number);
            self.record_token_selection_failure(issue_number);
            return Err(TokenSelectionDispatchError {
                issue: issue_number,
                log_path: log_path.clone(),
            }
            .into());
        }

        // Issue #6614: this dispatch got past token selection with a NAMED
        // account, which is direct proof the pool can still hand out a usable
        // credential — the one signal that clears the cross-issue empty-pool
        // brake, and (when it fires as the #5030 half-open probe) releases the
        // workspace hold with no operator action. Gated on a captured name
        // rather than mere survival: `UNKNOWN_TOKEN_NAME` also covers "the
        // child is simply slow to log its selection", which proves nothing.
        if token_name != crate::sweep_registry::UNKNOWN_TOKEN_NAME {
            self.clear_token_selection_failures();
        }

        let pid = child.id();
        // Issue #4980: capture the child's process group NOW, while it is alive
        // — `getpgid` cannot answer for a dead pid, so a group handle acquired
        // any later is unavailable in exactly the crash case that needs it most.
        let pgid = spawned_leader_pgid(pid);
        // Retain the handle so the reaper can `try_wait()` it (Issue #3801).
        self.children.insert(sweep_id.clone(), child);

        // Record the spawned child's PID in the lock (Issue #3808). The lock's
        // owner.json is written provisionally at `acquire_lock` time with the
        // daemon's own PID (the child does not exist yet), but the value that
        // matters for post-restart reconstruction is the *child's* PID: the
        // daemon PID is gone after any restart, so keeping it would make even a
        // still-live daemon-dispatched child look stale in `reconstruct()`'s
        // lock pass. Rewrite `owner_pid` now that the child exists.
        //
        // The same write persists the child's process group (#4980) so a
        // post-restart `reconstruct()` — and a fresh `loom-daemon cancel`
        // process, which never held the spawn-time `Child` handle — can still
        // tear down the WHOLE tree instead of orphaning it.
        if let Err(e) = self.record_child_pid_in_lock(issue_number, pid, pgid) {
            log::warn!(
                "failed to record child pid {pid} (pgid {pgid:?}) in lock for issue \
                 #{issue_number} (reconstruct may treat it as stale after a daemon restart, \
                 and a post-restart cancel may not reach the whole process group): {e}"
            );
        }

        // 6. Record the entry. The model is carried on the registry entry
        //    (#3482, Phase 3a observability) so `list_sweeps` /
        //    `get_sweep_status` can report which model a sweep runs. `model`
        //    / `effort` were already normalized (empty -> None) when
        //    `begin_issue_dispatch` built the `PreparedIssueDispatch`.
        let info = SweepInfo {
            sweep_id: sweep_id.clone(),
            kind: kind.clone(),
            pid,
            // Group handle for cancellation / crash-path reaping (#4980).
            pgid,
            token_name: token_name.clone(),
            runtime,
            runtime_source: runtime_admission.as_ref().map(|a| a.source.clone()),
            log_path: log_path.clone(),
            idempotency_key,
            started_at: Utc::now(),
            state: SweepState::Running,
            latest_phase: None,
            pr_number: None,
            model,
            effort,
            depends_on,
            // Stamp the owning workspace root (#3929) so list_sweeps /
            // get_sweep_status responses disambiguate this repo's issue #N from
            // another managed repo's identically-numbered issue.
            repo: Some(self.config.workspace_root.display().to_string()),
        };
        self.entries.insert(sweep_id.clone(), info);

        // 6b. Persist a liveness record to the machine-level sweep journal
        // (`~/.loom/sweeps.json`, Issue #3953). Unlike the in-memory entry
        // above, this file survives a daemon restart, giving
        // `loom-recover-orphans` an authoritative liveness source even when
        // this registry has just been recreated empty. Best-effort — a
        // journal-write hiccup must never fail dispatch.
        match self.config.resolve_journal_path() {
            Ok(journal_path) => {
                if let Err(e) = sweep_journal::record_sweep_at(
                    &journal_path,
                    &self.config.workspace_root.display().to_string(),
                    issue_number,
                    pid,
                    Utc::now(),
                ) {
                    log::warn!(
                        "sweep_journal: failed to record sweep for issue #{issue_number}: {e}"
                    );
                }
            }
            Err(e) => log::warn!(
                "sweep_journal: cannot resolve journal path for issue #{issue_number}: {e}"
            ),
        }

        // 7. Emit `sweep.global.dispatch` (best-effort — never block
        //    dispatch progress on the bus). If no subscribers are
        //    listening, the bus returns NoSubscribers; log at debug.
        self.emit_event(Event::SweepGlobalDispatch {
            sweep_id: sweep_id.clone(),
            kind: kind.clone(),
            runtime: runtime_admission.as_ref().map(|a| a.runtime.clone()),
            runtime_source: runtime_admission.as_ref().map(|a| a.source.clone()),
            // Stamped by `emit_event` -> `set_repo_if_absent` below (#4201),
            // matching the pattern already used for SweepPhase/Blocker/Exited/
            // Crashed — leave it `None` at construction.
            repo: None,
        });

        Ok(DispatchOutcome {
            sweep_id,
            pid,
            token_name,
            log_path,
            was_new: true,
        })
    }

    // ------------------------------------------------------------------------
    // PrSet dispatch (Issue #5342, Mode C — `/loom:sweep --prs n1 n2 ...`)
    // ------------------------------------------------------------------------

    /// Dispatch a PR-set sweep. Reached from [`Self::dispatch_inner`] step 2
    /// when `kind` is [`SweepKind::PrSet`].
    ///
    /// Deliberately a **separate, shorter** guard chain than the `Issue` path
    /// above: steps 2.5-2.9 (closed-issue, open-PR, park-label, dispatch
    /// backoff, live-claim) all key on "does issue N's forge state / claim
    /// history say this is a duplicate dispatch?" — a question that assumes a
    /// single claimed issue. A `PrSet` dispatch claims no issue at all (Mode C
    /// drives Judge/Doctor/Merge against PRs a Builder has already opened), so
    /// none of those guards has a coherent PR-set analogue yet. What DOES
    /// still apply, and is kept:
    ///
    /// - The idempotency dedup (step 1, in the caller).
    /// - The #4027 workspace-commands guard (2.4-equivalent below) — a
    ///   workspace missing the `/loom:sweep` slash command insta-crashes any
    ///   dispatch, `PrSet` included.
    /// - A NEW per-PR claim lock (`.loom/locks/pr-<N>/`,
    ///   [`Self::acquire_pr_lock`]) closing the "per-issue lock semantics"
    ///   gap the issue names: two concurrent `PrSet` dispatches that share a
    ///   PR number can no longer both proceed.
    fn dispatch_prset_inner(
        &mut self,
        prs: &[u32],
        kind: &SweepKind,
        idempotency_key: Option<String>,
        model: Option<&str>,
        effort: Option<&str>,
        runtime_admission: Option<crate::runtime_admission::ResolvedRuntime>,
    ) -> Result<DispatchOutcome> {
        if prs.is_empty() {
            return Err(anyhow!(
                "refusing to dispatch an empty PrSet: at least one PR number is required"
            ));
        }

        // Workspace-commands guard (Issue #4027), mirroring dispatch_inner's
        // step 2.4 — generic across `Issue`/`PrSet`, so applied here too.
        if !self.config.skip_label_flip && !self.config.has_sweep_command() {
            return Err(WorkspaceCommandsMissingDispatchError {
                workspace: self.config.workspace_root.clone(),
            }
            .into());
        }

        let sweep_id = generate_sweep_id(kind);

        // Per-PR claim lock (Issue #5342): acquired atomically, one mkdir per
        // PR, before any spawn side effect. On the first collision every
        // already-acquired lock in this set is rolled back so a refused
        // dispatch never leaves a partial claim behind.
        let mut acquired: Vec<u32> = Vec::new();
        for &pr in prs {
            if let Err(e) = self.acquire_pr_lock(pr, &sweep_id) {
                for done in &acquired {
                    let _ = self.release_pr_lock_owned(*done, &sweep_id);
                }
                return Err(e.context(format!(
                    "PR set {prs:?}: PR #{pr} is already claimed by another sweep"
                )));
            }
            acquired.push(pr);
        }

        self.apply_dispatch_stagger();
        let log_path = self.compute_prset_log_path(prs);
        let (child, token_name, runtime, immediate_preflight_death) = match self.spawn_child(
            kind,
            &log_path,
            &sweep_id,
            model,
            effort,
            None, // depends_on: stacked-PR chaining is Issue-only (#3729).
            runtime_admission.as_ref(),
        ) {
            Ok(spawned) => spawned,
            Err(e) => {
                for pr in &acquired {
                    let _ = self.release_pr_lock_owned(*pr, &sweep_id);
                }
                return Err(e.context("failed to spawn PrSet sweep child"));
            }
        };

        if immediate_preflight_death == Some("preflight-token-selection-failed") {
            log::warn!(
                "sweep_registry: PR set {prs:?} sweep_id={sweep_id} child exited immediately \
                 after token selection failed (#4689) — reverting the claim and reporting \
                 dispatch as a failure instead of a misleading Success"
            );
            for pr in &acquired {
                let _ = self.release_pr_lock_owned(*pr, &sweep_id);
            }
            return Err(anyhow!(
                "PR set {prs:?}: spawned child exited immediately — token selection failed (no \
                 usable OAuth token in the pool). Add accounts to ~/.claude-monitor/accounts.env \
                 then `loom-daemon tokens bootstrap`, or re-probe an existing pool with \
                 `loom-daemon tokens check --ranking`. See the sweep log for the exact failure: {}",
                log_path.display()
            ));
        }

        let pid = child.id();
        let pgid = spawned_leader_pgid(pid);
        self.children.insert(sweep_id.clone(), child);

        for pr in &acquired {
            if let Err(e) = self.record_child_pid_in_pr_lock(*pr, pid, pgid) {
                log::warn!(
                    "failed to record child pid {pid} (pgid {pgid:?}) in PR lock for #{pr} \
                     (a post-restart cancel may not reach the whole process group): {e}"
                );
            }
        }

        let info = SweepInfo {
            sweep_id: sweep_id.clone(),
            kind: kind.clone(),
            pid,
            pgid,
            token_name: token_name.clone(),
            runtime,
            runtime_source: runtime_admission.as_ref().map(|a| a.source.clone()),
            log_path: log_path.clone(),
            idempotency_key,
            started_at: Utc::now(),
            state: SweepState::Running,
            latest_phase: None,
            pr_number: None,
            model: model.filter(|m| !m.is_empty()).map(String::from),
            effort: effort.filter(|e| !e.is_empty()).map(String::from),
            depends_on: None,
            repo: Some(self.config.workspace_root.display().to_string()),
        };
        self.entries.insert(sweep_id.clone(), info);

        // Unlike `Issue` dispatch (step 6b), no machine-level sweep-journal
        // record is written: `sweep_journal` (#3953) is keyed on one issue
        // number and exists to let `loom-recover-orphans` re-arm a single
        // stranded `loom:building` claim — a `PrSet` dispatch claims no
        // issue, so there is nothing for that recovery path to re-arm.

        self.emit_event(Event::SweepGlobalDispatch {
            sweep_id: sweep_id.clone(),
            kind: kind.clone(),
            runtime: runtime_admission.as_ref().map(|a| a.runtime.clone()),
            runtime_source: runtime_admission.as_ref().map(|a| a.source.clone()),
            repo: None,
        });

        Ok(DispatchOutcome {
            sweep_id,
            pid,
            token_name,
            log_path,
            was_new: true,
        })
    }

    // ------------------------------------------------------------------------
    // Spawn
    // ------------------------------------------------------------------------

    pub(crate) fn compute_log_path(&self, issue: u32) -> PathBuf {
        self.config
            .logs_dir()
            .join(format!("sweep-issue-{issue}.log"))
    }

    /// The `PrSet` counterpart of [`Self::compute_log_path`] (Issue #5342):
    /// one log file per PR set, named after every member so an operator can
    /// tell two overlapping-but-distinct sets apart at a glance.
    pub(crate) fn compute_prset_log_path(&self, prs: &[u32]) -> PathBuf {
        let joined = prs
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("-");
        self.config
            .logs_dir()
            .join(format!("sweep-prs-{joined}.log"))
    }

    /// Enforce the configured dispatch stagger (Issue #3887): if less than
    /// `dispatch_stagger` has elapsed since the previous spawn, sleep the
    /// remainder, then record now as the latest spawn instant. A zero stagger
    /// is a no-op. Called under the registry mutex from `dispatch`, so it also
    /// serializes concurrent dispatch callers past the contended startup step.
    pub(crate) fn apply_dispatch_stagger(&mut self) {
        let wait = stagger_wait(self.last_spawn_at, self.dispatch_stagger, Instant::now());
        if !wait.is_zero() {
            log::debug!(
                "sweep_registry: staggering spawn by {}ms to avoid startup race (#3887)",
                wait.as_millis()
            );
            std::thread::sleep(wait);
        }
        self.last_spawn_at = Some(Instant::now());
    }

    /// Spawn the child AND poll it for its account-selection log line
    /// (Issue #3802), all in one synchronous call. This is the historical,
    /// self-contained shape — kept for the `PrSet` dispatch path
    /// ([`Self::dispatch_prset_inner`]) and any direct/test caller.
    ///
    /// Issue #6592: the `Issue`-kind dispatch path no longer calls this
    /// directly. It instead calls [`Self::spawn_child_process`] (this
    /// method's head, through `Command::spawn()` only) and the free function
    /// [`poll_and_classify_spawned_child`] (this method's tail) as two
    /// separate steps, so the IPC layer can release the registry mutex
    /// between them — see [`Self::begin_issue_dispatch`] /
    /// [`Self::finish_issue_dispatch`]'s doc comments for why: the poll below
    /// blocks for up to `TOKEN_NAME_CAPTURE_TIMEOUT` and must not run while
    /// holding the registry mutex a concurrent `DispatchSweep`/`ListSweeps`
    /// needs.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn spawn_child(
        &self,
        kind: &SweepKind,
        log_path: &Path,
        sweep_id: &str,
        model: Option<&str>,
        effort: Option<&str>,
        depends_on: Option<u32>,
        runtime_admission: Option<&crate::runtime_admission::ResolvedRuntime>,
    ) -> Result<(Child, String, String, Option<&'static str>)> {
        let (mut child, header_anchor) = self.spawn_child_process(
            kind,
            log_path,
            sweep_id,
            model,
            effort,
            depends_on,
            runtime_admission,
        )?;
        let (token_name, runtime, immediate_preflight_death) =
            poll_and_classify_spawned_child(&mut child, log_path, &header_anchor);
        Ok((child, token_name, runtime, immediate_preflight_death))
    }

    /// Build the child's `Command` and spawn it — everything `spawn_child`
    /// used to do EXCEPT the account-selection poll (Issue #6592). Fast:
    /// `Command::spawn()` forks+execs without waiting for the child to do
    /// anything. Returns the live [`Child`] handle plus the `header_anchor`
    /// (`sweep_id=<id>`) the caller needs to pass to
    /// [`poll_and_classify_spawned_child`] next.
    ///
    /// Deliberately still `&self` (no registry mutation) so this can run
    /// under the SAME lock scope the guard chain above it uses — preserving
    /// the #3887 dispatch-stagger invariant (`apply_dispatch_stagger` is
    /// called by the caller just before this, still lock-serialized) — while
    /// the *poll* that follows can be released to run unlocked.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn spawn_child_process(
        &self,
        kind: &SweepKind,
        log_path: &Path,
        sweep_id: &str,
        model: Option<&str>,
        effort: Option<&str>,
        depends_on: Option<u32>,
        runtime_admission: Option<&crate::runtime_admission::ResolvedRuntime>,
    ) -> Result<(Child, String)> {
        let spawn_bin = self.config.resolve_spawn_bin()?;

        // Ensure log dir exists.
        if let Some(parent) = log_path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("failed to create log dir {}", parent.display()))?;
        }

        // Header target description (Issue #5342): `Issue` names its single
        // issue number; `PrSet` has none, so it names the whole PR list.
        let target_desc = match kind {
            SweepKind::Issue(n) => format!("issue={n}"),
            SweepKind::PrSet(prs) => format!("prs={prs:?}"),
        };

        // Append a header so reruns are distinguishable. Mirrors
        // spawn-loop.sh:377-380.
        {
            use std::io::Write;
            if let Ok(mut f) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(log_path)
            {
                let _ = writeln!(
                    f,
                    "\n==== loom-daemon dispatch: {} sweep_id={sweep_id} {target_desc} ====",
                    Utc::now().to_rfc3339()
                );
            }
        }

        let log_file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(log_path)
            .with_context(|| format!("failed to open log {}", log_path.display()))?;
        let log_clone = log_file.try_clone()?;

        // Daemon self-claim marker, positional form (issue #4111): embed
        // `--claim-owned <N>` INSIDE the `-p` prompt text so it becomes part of
        // the `/loom:sweep` skill's own `$ARGUMENTS`, exactly like every other
        // skill-consumed flag (`--dry-run`, `--no-daemon`, `--depends-on`,
        // `--auto-stack`, `--prs`). It MUST NOT be appended as a sibling
        // `cmd.arg()`: `spawn-claude.sh` forwards every non-wrapper token
        // verbatim to the real `claude` CLI (`exec claude "$@"`), and none of
        // these are `claude` CLI flags — a sibling arg makes `claude` exit 1
        // (`error: unknown option '...'`) before any session starts, turning
        // every daemon dispatch into an immediate crash. Only text inside the
        // single `-p "<prompt>"` string ever reaches the skill's pre-flight.
        //
        // `Issue` claims exactly one issue (`--claim-owned <N>`, env var kept
        // for backward compatibility below — #3823/#3967) and optionally
        // chains a stacked-PR parent (`--depends-on <N>`, issue #3729 v1;
        // sibling-arg bug fixed in #4121). `PrSet` (Mode C, issue #5342) has
        // no issue to claim and no stacking — it drives `--prs <n1> <n2>
        // ...` against an existing PR set instead.
        let prompt = match kind {
            SweepKind::Issue(issue) => {
                let mut p = format!("/loom:sweep {issue} --claim-owned {issue}");
                if let Some(parent) = depends_on {
                    p.push_str(&format!(" --depends-on {parent}"));
                }
                p
            }
            SweepKind::PrSet(prs) => {
                let joined = prs
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(" ");
                format!("/loom:sweep --prs {joined}")
            }
        };
        let mut cmd = Command::new(&spawn_bin);
        cmd.arg("-p").arg(&prompt);
        // Model selection (issue #3477, Phase 1): the dispatch-param tier of
        // the precedence chain. Appended as an explicit `--model` arg (which
        // beats any ambient LOOM_MODEL env inside spawn-claude.sh). Empty
        // strings are treated as unset — `--model ""` must never be emitted.
        if let Some(m) = model {
            if !m.is_empty() {
                cmd.arg("--model").arg(m);
            }
        }
        // Reasoning-effort selection (issue #3716): the dispatch-param tier,
        // mirroring `--model` exactly. Appended as an explicit `--effort` arg
        // (which beats any ambient LOOM_EFFORT env inside spawn-claude.sh).
        // Empty strings are treated as unset — `--effort ""` must never be
        // emitted, so the session-default effort is preserved end-to-end.
        if let Some(e) = effort {
            if !e.is_empty() {
                cmd.arg("--effort").arg(e);
            }
        }
        // (Daemon self-claim marker `--claim-owned <N>` and the stacked-PR
        // `--depends-on <N>` marker are both embedded in the `-p` prompt text
        // above, not appended as sibling args — see #4111 / #4121.)
        // Unattended-permissions flag (issue #3824): a daemon-dispatched child
        // is a detached, non-interactive `claude -p` process — there is no
        // human to answer a permission prompt, so any tool call needing
        // approval (`.loom/` writes, `sweep-run-registry.sh`, the
        // `mcp__loom__list_sweeps` daemon probe) auto-denies and stalls the
        // build. Append `--dangerously-skip-permissions` so the child runs
        // non-interactively with hooks still firing — mirroring the established
        // unattended cron pattern (`.github/workflows/loom-*.yml`, which spawn
        // `claude -p "/<role>" --dangerously-skip-permissions`). Scoped to this
        // daemon-only dispatch path; `spawn-claude.sh` stays a generic
        // pass-through and never adds a permission flag of its own. Appended
        // AFTER `--model`/`--effort` (and the prompt-embedded `--claim-owned`
        // / `--depends-on`) so the positional argv contract is unchanged.
        cmd.arg("--dangerously-skip-permissions");
        // Transient-error recovery (issue #4255): route the child through
        // `claude-wrapper.sh` so a transient API death (rate-limit storm, 5xx,
        // overloaded, or the CLI's bare `Execution error`) is retried with
        // exponential backoff per `LOOM_MAX_RETRIES` instead of killing the
        // whole sweep on the first failure — the daemon dispatch path is the
        // unattended path that most needs it (21% of sweep logs died this way
        // before this flag). `spawn-claude.sh` consumes `--use-wrapper` (it is
        // NOT forwarded to `claude`) and execs the wrapper, which forwards the
        // daemon's `-p/--model/--effort/--dangerously-skip-permissions` argv
        // verbatim. Appended AFTER `--dangerously-skip-permissions` so the
        // positional prompt contract (#4111/#4121) is unchanged and existing
        // argv-prefix assertions still hold. Operators can force the legacy
        // single-shot path with `LOOM_USE_WRAPPER=0` (see
        // `wrapper_dispatch_enabled`).
        if wrapper_dispatch_enabled() {
            cmd.arg("--use-wrapper");
        }
        cmd.env("LOOM_TERMINAL_ID", format!("daemon-{sweep_id}"));
        // Claim-ownership marker (issue #3823): `dispatch()` flips
        // loom:issue -> loom:building on the forge BEFORE this child is
        // spawned (step 4, for immediate external visibility of the claim).
        // Without a signal, the child's own `/loom:sweep` pre-flight would
        // read that label and skip issue N as "already being built by
        // someone else" — self-skipping the daemon's OWN claim, so no
        // worktree, no build, no PR. Export the issue number this sweep
        // owns so the child's pre-flight recognises an existing
        // loom:building as ITS OWN daemon claim and proceeds to build.
        // Scoped to daemon-dispatched children only: an operator-run
        // `/loom:sweep N` never sets this env var, so the manual-terminal
        // skip rule (honor any loom:building) is unchanged.
        //
        // Issue #4111: this env var alone was proven insufficient — a
        // daemon-dispatched child reliably reasoned about loom:building
        // label timing / PID tables / `loom-daemon status` and
        // self-skipped its own claim without ever consulting it. The
        // `--claim-owned <N>` argv flag appended above is now the PRIMARY
        // signal (positional, in the model's context by construction);
        // this env var is kept for backward compatibility only —
        // spawn-claude.sh still logs it, and it is still asserted by the
        // producer-side tests below plus `work_finder.rs` / `ipc.rs`.
        //
        // Issue #5342: `PrSet` claims no single issue, so this marker is
        // Issue-only — a `PrSet` child's `/loom:sweep --prs ...` pre-flight
        // has no per-issue `loom:building` self-claim to recognise.
        if let SweepKind::Issue(issue) = kind {
            cmd.env("LOOM_SWEEP_CLAIM_OWNED", issue.to_string());
        }
        cmd
            // Always pin LOOM_WORKSPACE to the registry's configured root so
            // spawn-claude.sh resolves `.loom/tokens/` from the same place
            // the daemon thinks the workspace is — never inheriting an
            // ambient value that might point elsewhere.
            .env(WORKSPACE_ENV, &self.config.workspace_root)
            // Issue #3943: the child is a headless `claude -p "/loom:sweep N"`
            // session. In print mode the Claude Code harness terminates
            // still-running background tasks — the sweep's dispatched
            // Builder/Judge subagents — after a 600s ceiling and exits the
            // session, killing any role phase that runs >10 minutes mid-build
            // and causing loom:building<->loom:issue label ping-pong. Disable
            // the ceiling (0 = no cap) explicitly on the child env so a long
            // Builder/Judge phase runs to completion. `spawn-claude.sh` also
            // sets this (belt-and-suspenders), but we pin it here too so the
            // daemon dispatch path does not depend on the wrapper doing it.
            .env(BG_WAIT_CEILING_ENV, "0")
            // Issue #3730: pin the child's cwd to the resolved workspace root
            // so the child's relative `.loom/config.json` read
            // (loom_tools/sweep_experiment.py) and archive-transcripts.sh's
            // cwd-slug resolve deterministically, rather than depending on the
            // daemon's own cwd happening to be the workspace root.
            .current_dir(&self.config.workspace_root)
            .stdin(Stdio::null())
            .stdout(Stdio::from(log_file))
            .stderr(Stdio::from(log_clone));
        // Per-owner credential routing (#5401/#5431, gap closed for
        // role-runner children by #5508/#5522; this is the THIRD dispatch
        // path with the same gap, #6529): this sweep child is spawned with
        // `current_dir(&self.config.workspace_root)` above, so it must carry
        // the SAME per-owner `GH_CONFIG_DIR` every other per-repo `gh`/`git`
        // child-spawn call site already does — otherwise a workspace
        // registered under a non-default owner inherits whatever
        // `GH_CONFIG_DIR` the daemon process (or a PREVIOUSLY dispatched
        // sweep child for a DIFFERENT workspace) happens to have set, and
        // every forge call the spawned `/loom:sweep` session makes 404s —
        // silently, before the sweep's first checkpoint is written. A total
        // no-op for a single-owner fleet or the root owner's own repos — see
        // `apply_gh_config_for_root`'s doc comment.
        crate::credential_preflight::apply_gh_config_for_root(
            &mut cmd,
            &self.config.workspace_root,
        );
        if let Some(admission) = runtime_admission {
            cmd.env("LOOM_RUNTIME", &admission.runtime);
            // Issue #4768: pin the ALREADY-ADMITTED role alongside the runtime
            // it was admitted for. Without this, a Codex-runtime sweep child
            // reaches `spawn-codex.sh` with no `LOOM_ROLE` at all (bash env
            // vars only propagate what the parent process actually set — this
            // `Command` never set one), which `spawn-codex.sh` treats as an
            // ambiguous/unknown role and silently takes the READ-ONLY
            // sandbox-fallback path instead of the mutable-role hook-trust
            // preflight. `admission.role` is always `"sweep-lifecycle"` here
            // (a full sweep is modelled as one launch, admitted against
            // Builder's requirements — see runtime_admission.rs's module
            // doc), which `spawn-codex.sh` maps onto `builder` for its own
            // mutable-role check.
            cmd.env("LOOM_ROLE", &admission.role);
            log::info!(
                "sweep_registry: admitted role={} runtime={} source={}",
                admission.role,
                admission.runtime,
                admission.source
            );
            // #6201: same loud, at-selection divergence diagnostic as
            // `role_runner`'s standalone role ticks — see
            // `suggested_worker_type_mismatch_warning`'s doc comment.
            if let Some(msg) =
                crate::runtime_admission::suggested_worker_type_mismatch_warning(admission)
            {
                log::warn!("{msg}");
            }
        }

        // Issue #3800: put the sweep child in its OWN process group
        // (`setpgid(0, 0)` runs post-fork/pre-exec via `process_group(0)`,
        // stable since Rust 1.64). spawn-claude.sh ends in `exec claude`, so
        // the tracked PID becomes the `claude` process itself AND the leader
        // of a fresh group. `claude` forks real OS subprocesses for tool
        // execution (Bash-tool commands, MCP servers, git clones, …); those
        // descendants inherit this group. Making the child a group leader lets
        // `cancel()` signal the WHOLE group (`kill(-pgid, sig)`) so the entire
        // sweep subtree is torn down — instead of leaving orphans behind when
        // only the top-level PID is signalled.
        #[cfg(unix)]
        cmd.process_group(0);

        // Issue #3730: explicitly forward the experiment-related env vars to
        // the detached child via an EXPLICIT ALLOWLIST — never a blanket
        // env_clear/copy. Without this, `LOOM_MODEL_EXPERIMENT` /
        // `LOOM_MODEL_EXPERIMENT_CANARY` / `LOOM_TRANSCRIPT_ARCHIVE` only reach
        // the child if the daemon *itself* was launched with them; an operator
        // exporting them before dispatching would get a silent no-effect.
        //
        // `var_os` guards each name: an UNSET var is not forwarded, and an
        // empty-string value is not forwarded either (no empty-string
        // forwarding — mirrors the archiver / experiment-parser treatment of
        // empty as "unset"). This keeps the spawn a byte-for-byte no-op when
        // none of the vars are set.
        for name in EXPERIMENT_ENV_ALLOWLIST {
            if let Some(val) = std::env::var_os(name) {
                if !val.is_empty() {
                    cmd.env(name, val);
                }
            }
        }

        let child = cmd
            .spawn()
            .with_context(|| format!("failed to spawn {} -p '{}'", spawn_bin.display(), prompt))?;
        // Issue #3801: we RETAIN the `Child` handle (returned to `dispatch`,
        // which stores it in `self.children`) instead of dropping it. The
        // reaper `try_wait()`s it each tick so an exited child is reaped
        // (no `<defunct>` zombie) and the registry transitions to a terminal
        // state with the real exit status.
        //
        // Issue #3802: the caller polls this log for the `using OAuth
        // account '<name>'` marker via `poll_and_classify_spawned_child`
        // (Issue #6592 — split out of this method so that potentially
        // multi-second poll can run without the registry mutex held). The
        // scan is anchored to THIS dispatch's header line (`sweep_id=<id>`)
        // so a stale line from a previous dispatch appended to the same
        // per-issue log is never mistaken for the current selection.
        let header_anchor = format!("sweep_id={sweep_id}");
        Ok((child, header_anchor))
    }
}

/// Poll `child`'s log for its account-selection marker and classify an
/// immediate preflight death (Issue #6592 — split out of `spawn_child` so the
/// IPC layer can run this potentially multi-second wait WITHOUT holding the
/// registry mutex; see `SweepRegistry::begin_issue_dispatch` /
/// `finish_issue_dispatch`). Pure with respect to the registry: only touches
/// the child handle and its own log file, never `self`.
///
/// Falls back to `UNKNOWN_TOKEN_NAME` on timeout / no-selection — never
/// blocks longer than `TOKEN_NAME_CAPTURE_TIMEOUT` or fails dispatch.
///
/// Issue #4689: `poll_observability` already blocks (bounded by
/// `TOKEN_NAME_CAPTURE_TIMEOUT`) until either a token is captured, the child
/// logs `CLI_START_MARKER`, or the child exits — so by the time this
/// function returns it may already know, synchronously, that the child died
/// before ever selecting a token. `token_name == UNKNOWN_TOKEN_NAME` alone is
/// NOT sufficient signal — that's also the (far more common) "child is just
/// slow to log its selection, still alive" case, which must not be
/// misclassified as a failure. Only a CONFIRMED-dead child (`try_wait`
/// returns `Some`; cheap and side-effect-free here because
/// `poll_observability` already cached the exit status, per its own doc
/// comment) combined with an unknown token name is worth reading the log
/// tail for. The caller uses this to convert the specific "token selection
/// failed" preflight shape into a hard `Err` instead of a misleadingly-
/// `Success` `DispatchOutcome` with `Token: unknown`.
pub(crate) fn poll_and_classify_spawned_child(
    child: &mut Child,
    log_path: &Path,
    header_anchor: &str,
) -> (String, String, Option<&'static str>) {
    let (token_name, runtime) = poll_observability(child, log_path, header_anchor);

    let immediate_preflight_death =
        if token_name == UNKNOWN_TOKEN_NAME && matches!(child.try_wait(), Ok(Some(_))) {
            tail_lines(log_path, EXHAUSTION_LOG_TAIL_LINES)
                .ok()
                .map(|lines| lines.join("\n"))
                .and_then(|tail| classify_preflight_death(&tail))
        } else {
            None
        };

    (token_name, runtime, immediate_preflight_death)
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::panic,
    clippy::expect_used,
    unused_imports
)]
mod tests {
    use super::*;
    use crate::sweep_registry::test_support::*;
    use serial_test::serial;
    use std::os::unix::fs::PermissionsExt;
    use std::time::SystemTime;
    use tempfile::tempdir;

    /// RAII guard that clears the ambient `LOOM_RUNTIME` env var for the
    /// scope of a test and restores whatever value (if any) it previously
    /// had — including across a mid-test assertion panic, since Rust
    /// unwinds through `Drop`. Some host/dev-container shells export
    /// `LOOM_RUNTIME` (as the `spawn-worker.sh` runtime selector), and
    /// without this guard that ambient value silently outranks the
    /// `runtimes.default` config precedence this test exercises (#4739).
    struct ClearedLoomRuntimeEnv(Option<String>);

    impl ClearedLoomRuntimeEnv {
        fn new() -> Self {
            let prior = std::env::var("LOOM_RUNTIME").ok();
            std::env::remove_var("LOOM_RUNTIME");
            Self(prior)
        }
    }

    impl Drop for ClearedLoomRuntimeEnv {
        fn drop(&mut self) {
            match self.0.take() {
                Some(v) => std::env::set_var("LOOM_RUNTIME", v),
                None => std::env::remove_var("LOOM_RUNTIME"),
            }
        }
    }

    /// As [`ClearedLoomRuntimeEnv`] but for `GH_CONFIG_DIR` (#6529): the test
    /// process — or a PREVIOUS test in this same `#[serial]` suite — may
    /// itself have a `GH_CONFIG_DIR` set, which would otherwise leak into a
    /// spawned fixture child's environment and make the "unregistered root
    /// leaves GH_CONFIG_DIR untouched" test below observe an ambient value
    /// instead of a genuine absence. Mirrors `role_runner::tests::
    /// ClearedGhConfigDirEnv`, added for the same reason by #5522.
    struct ClearedGhConfigDirEnv(Option<String>);

    impl ClearedGhConfigDirEnv {
        fn new() -> Self {
            let prior = std::env::var("GH_CONFIG_DIR").ok();
            std::env::remove_var("GH_CONFIG_DIR");
            Self(prior)
        }
    }

    impl Drop for ClearedGhConfigDirEnv {
        fn drop(&mut self) {
            match self.0.take() {
                Some(v) => std::env::set_var("GH_CONFIG_DIR", v),
                None => std::env::remove_var("GH_CONFIG_DIR"),
            }
        }
    }

    #[test]
    #[serial]
    fn runtime_rejection_precedes_every_dispatch_side_effect() {
        let _env_guard = ClearedLoomRuntimeEnv::new();
        let dir = tempdir().unwrap();
        let workspace = dir.path();
        touch_sweep_command(workspace);
        let config_dir = workspace.join(".loom");
        std::fs::write(config_dir.join("config.json"), r#"{"runtimes":{"default":"codex"}}"#)
            .unwrap();
        std::fs::write(
            config_dir.join("runtimes/codex.json"),
            r#"{"runtime":"codex","capabilities":{"worktreeIsolation":"partial","mcp":"yes"}}"#,
        )
        .unwrap();
        let codex = config_dir.join("scripts/spawn-codex.sh");
        std::fs::write(&codex, "#!/bin/sh\nexit 0\n").unwrap();
        let mut perms = std::fs::metadata(&codex).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(codex, perms).unwrap();

        let gh_marker = workspace.join("gh-called");
        let fake_gh = workspace.join("fake-gh.sh");
        std::fs::write(&fake_gh, format!("#!/bin/sh\ntouch '{}'\nexit 0\n", gh_marker.display()))
            .unwrap();
        let mut perms = std::fs::metadata(&fake_gh).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&fake_gh, perms).unwrap();
        let spawn_marker = workspace.join("spawn-called");
        let fake_spawn = workspace.join("fake-spawn.sh");
        std::fs::write(
            &fake_spawn,
            format!("#!/bin/sh\ntouch '{}'\nexit 0\n", spawn_marker.display()),
        )
        .unwrap();
        let mut perms = std::fs::metadata(&fake_spawn).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&fake_spawn, perms).unwrap();

        let mut config = SweepRegistryConfig::new(workspace.to_path_buf());
        config.skip_label_flip = false;
        config.gh_bin = Some(fake_gh);
        config.spawn_bin = Some(fake_spawn);
        config.journal_path = Some(workspace.join("journal.json"));
        let mut registry = SweepRegistry::new(config);
        let bus = Arc::new(EventBus::with_capacity(8));
        let mut events = bus.subscribe(["sweep.global"]);
        registry.set_event_bus(bus);
        let error = registry
            .dispatch(&SweepKind::Issue(4494), None, None, None, None)
            .unwrap_err();
        let rejection = error
            .downcast_ref::<crate::runtime_admission::RuntimeRejection>()
            .unwrap();
        assert_eq!(rejection.runtime, "codex");
        assert_eq!(rejection.unmet_capabilities, vec!["worktreeIsolation"]);
        assert!(!gh_marker.exists(), "forge probe/mutation ran before admission");
        assert!(!spawn_marker.exists(), "child spawn ran before admission");
        assert!(!workspace.join(".loom/locks/issues/4494").exists(), "claim lock was created");
        assert!(!registry.compute_log_path(4494).exists(), "log header was created");
        assert!(registry.entries.is_empty(), "capacity/registry entry was consumed");

        // #4494: refused work IS represented on the bus — and only by the
        // rejection topic (never a `sweep.global.dispatch` for work that was
        // never admitted).
        let event = events.try_recv().expect("a rejection event was published");
        assert_eq!(event.topic(), "sweep.global.runtime_rejected");
        match event {
            Event::SweepGlobalRuntimeRejected {
                kind,
                role,
                runtime,
                runtime_source,
                unmet_capabilities,
                reason,
                repo,
            } => {
                assert_eq!(kind, SweepKind::Issue(4494));
                assert_eq!(role, "sweep-lifecycle");
                assert_eq!(runtime, "codex");
                assert_eq!(runtime_source, crate::types::RuntimeSource::DefaultConfig);
                assert_eq!(unmet_capabilities, vec!["worktreeIsolation"]);
                assert!(reason.contains("worktreeIsolation"), "{reason}");
                // Stamped centrally by `emit_event` (#4201's pattern).
                assert_eq!(repo.as_deref(), Some(workspace.display().to_string().as_str()));
            }
            other => panic!("expected SweepGlobalRuntimeRejected, got {other:?}"),
        }
        assert!(events.try_recv().is_err(), "no further events for refused work");
    }

    /// Build a temp-workspace registry with a fake spawn binary that
    /// records its argv + env into a log and exits immediately. This lets
    /// us assert on the dispatch behavior without invoking real `claude`.
    ///
    /// We invoke the fake via `bash -c '...'` (returned from
    /// `SweepRegistryConfig.spawn_bin`) rather than relying on a shebang +
    /// exec bit, because parallel-test load on macOS occasionally races the
    /// chmod with the child's posix_spawn exec call and the script silently
    /// fails to launch (no shebang resolution, no exec-bit yet).
    /// #4431: the reaper's peer-claim heartbeat must re-advertise every live
    /// (`Running`/`Pending`) Issue sweep's claim — and ONLY those (a
    /// terminal-state entry is a dead sweep whose claim must be allowed to
    /// expire), and be a publisher-less no-op (safehouse disabled).
    #[test]
    fn readvertise_republishes_live_issue_claims_only() {
        let dir = tempdir().unwrap();
        let (mut registry, _log) = fixture_registry(dir.path());

        // No publisher attached (safehouse.enabled false): a silent no-op.
        assert_eq!(registry.readvertise_peer_claims(), 0);

        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        registry.set_peer_claim_publisher(tx);

        let mk_info =
            |sweep_id: &str, issue: u32, state: SweepState, log_path: PathBuf| SweepInfo {
                pgid: None,
                sweep_id: sweep_id.to_string(),
                kind: SweepKind::Issue(issue),
                pid: 0,
                token_name: "unknown".into(),
                runtime: "unknown".into(),
                runtime_source: None,
                log_path,
                idempotency_key: None,
                started_at: Utc::now(),
                state,
                latest_phase: None,
                pr_number: None,
                model: None,
                effort: None,
                depends_on: None,
                repo: None,
            };
        let live_log = registry.compute_log_path(4431);
        let dead_log = registry.compute_log_path(999);
        registry.entries.insert(
            "sweep-live".to_string(),
            mk_info("sweep-live", 4431, SweepState::Running, live_log),
        );
        registry.entries.insert(
            "sweep-dead".to_string(),
            mk_info(
                "sweep-dead",
                999,
                SweepState::Exited {
                    code: None,
                    at: Utc::now(),
                },
                dead_log,
            ),
        );

        assert_eq!(registry.readvertise_peer_claims(), 1);
        let ad = rx.try_recv().expect("one re-advertisement published");
        assert_eq!(ad.issue, 4431);
        assert_eq!(ad.kind, crate::peer_claims::ClaimKind::Advertise);
        assert!(
            rx.try_recv().is_err(),
            "the terminal-state sweep's claim must NOT be re-advertised"
        );
    }

    #[test]
    #[serial]
    fn dispatch_happy_path_records_entry() {
        let dir = tempdir().unwrap();
        let (mut registry, record_log) = fixture_registry(dir.path());

        let outcome = registry
            .dispatch(&SweepKind::Issue(42), None, None, None, None)
            .expect("dispatch should succeed");

        assert!(outcome.was_new);
        assert!(outcome.pid > 0);
        assert_eq!(outcome.token_name, "unknown");
        assert_eq!(registry.len(), 1);

        let info = registry.get(&outcome.sweep_id).unwrap();
        assert!(matches!(info.kind, SweepKind::Issue(42)));
        assert!(matches!(info.state, SweepState::Running));

        // Wait for the fake spawn to record its invocation. We wait for
        // the final line (LOOM_TERMINAL_ID) so the assertion isn't racing
        // mid-write.
        let needle = format!("LOOM_TERMINAL_ID=daemon-{}", outcome.sweep_id);
        let recorded = assert_child_wrote(&record_log, &needle);
        assert!(
            recorded.contains("argv: -p /loom:sweep 42"),
            "expected argv in recorded log; got: {recorded}"
        );
        // Issue #3477 zero-behavior-change criterion: with model=None the
        // spawned command must NOT receive a --model flag at all.
        assert!(
            !recorded.contains("--model"),
            "model=None must not emit --model; got: {recorded}"
        );
        // Issue #3716: with effort=None the spawned command must likewise NOT
        // receive a --effort flag at all (byte-for-byte unchanged default).
        assert!(
            !recorded.contains("--effort"),
            "effort=None must not emit --effort; got: {recorded}"
        );
        // #3482: model=None dispatches record no model on the entry.
        assert_eq!(registry.get(&outcome.sweep_id).unwrap().model, None);
        // #3716: effort=None dispatches record no effort on the entry.
        assert_eq!(registry.get(&outcome.sweep_id).unwrap().effort, None);

        // The lock dir should exist while Running.
        let lock = dir.path().join(".loom").join("locks").join("issue-42");
        assert!(lock.exists(), "expected lock dir at {}", lock.display());

        // Issue #3824: every daemon-dispatched child must carry
        // --dangerously-skip-permissions (unattended, non-interactive).
        assert!(
            recorded.contains("--dangerously-skip-permissions"),
            "expected --dangerously-skip-permissions in argv; got: {recorded}"
        );
    }

    /// Issue #4028 fail-open: an unreachable safehouse coordination channel (its
    /// receiver dropped ⇒ `try_send` returns `Closed`) must NEVER block or fail a
    /// dispatch — the soft claim is an optimization, never a liveness dependency.
    /// This is the single most important #4028 test.
    #[test]
    fn dispatch_proceeds_when_peer_claim_channel_is_closed_fail_open() {
        let dir = tempdir().unwrap();
        let (mut registry, _record_log) = fixture_registry(dir.path());
        // Attach a publisher whose receiver is immediately dropped, so every
        // `try_send` fails — modeling an absent/refusing safehoused.
        let (tx, rx) = tokio::sync::mpsc::channel::<ClaimAd>(1);
        drop(rx);
        registry.set_peer_claim_publisher(tx);

        let outcome = registry
            .dispatch(&SweepKind::Issue(77), None, None, None, None)
            .expect("dispatch must proceed even when the safehouse channel is closed");
        assert!(outcome.was_new, "the sweep still starts");
        assert_eq!(registry.len(), 1);
    }

    /// Issue #4028: the work-finder's peer-claim skip set reflects the attached
    /// shared view, scoped to this registry's repo, and is empty when no view is
    /// attached (byte-for-byte no-op).
    #[test]
    fn peer_claimed_issues_reflects_the_attached_view() {
        let dir = tempdir().unwrap();
        let (mut registry, _record_log) = fixture_registry(dir.path());
        // No view attached ⇒ empty (the no-op default).
        assert!(registry.peer_claimed_issues().is_empty());

        let repo = peer_claims::repo_slug(&registry.config().workspace_root);
        let view =
            Arc::new(Mutex::new(PeerClaimView::new("self".into(), Duration::from_secs(120))));
        {
            let mut v = view.lock().unwrap();
            let now = Instant::now();
            v.observe_at(
                &ClaimAd::advertise(500, repo.clone(), "peer".into(), 1, "ts".into()),
                now,
            );
        }
        registry.set_peer_claims(view);
        assert!(registry.peer_claimed_issues().contains(&500));
    }

    /// Issue #5789: the 2.95 peer-claim guard turns the soft-claim
    /// advertisement from a passive "the work-finder happens to check this
    /// first" signal into an enforced gate inside `dispatch()` itself, so
    /// EVERY call site (not just the work-finder's own pre-tick filter) backs
    /// off on a live peer claim. Simulates two hosts racing on the same issue
    /// with two independent `SweepRegistry` instances bridged only by a
    /// manually-relayed `ClaimAd` — deterministic, no sleeps, mirroring
    /// `peer_claims::tests::two_hosts_one_issue_second_host_backs_off_deterministically`
    /// but exercised through the real `dispatch()` call path instead of the
    /// bare `PeerClaimView`.
    #[test]
    fn dispatch_backs_off_when_a_peer_hosts_claim_is_already_live() {
        let dir_a = tempdir().unwrap();
        let dir_b = tempdir().unwrap();

        // Host A dispatches first and advertises its claim.
        let (mut registry_a, _log_a) = fixture_registry(dir_a.path());
        let (tx_a, mut rx_a) = tokio::sync::mpsc::channel(8);
        registry_a.set_peer_claim_publisher(tx_a);

        let outcome_a = registry_a
            .dispatch(&SweepKind::Issue(9789), None, None, None, None)
            .expect("host A should win the race and dispatch normally");
        assert!(outcome_a.was_new);
        assert_eq!(registry_a.len(), 1, "host A recorded exactly one sweep");

        let ad = rx_a
            .try_recv()
            .expect("host A published an advertise ad for #9789");
        assert_eq!(ad.issue, 9789);
        assert_eq!(ad.kind, crate::peer_claims::ClaimKind::Advertise);

        // Host B's inbound safehouse coordination task observes A's ad before
        // B ever attempts its own dispatch — the peer-claim view this closes
        // over is exactly what `set_peer_claims` wires into a live daemon.
        let (mut registry_b, _log_b) = fixture_registry(dir_b.path());
        let repo_b = peer_claims::repo_slug(&registry_b.config().workspace_root);
        // The ad carries host A's own repo slug; re-key it under B's (in this
        // fixture the two temp dirs have different basenames, so re-derive
        // the ad with B's repo identity — a real fleet shares one `LOOM_REPO`
        // across hosts, which this mirrors).
        let relayed = ClaimAd::advertise(ad.issue, repo_b, ad.host, ad.pid, ad.ts);
        let view_b = Arc::new(Mutex::new(PeerClaimView::new(
            "hostB".into(),
            peer_claims::DEFAULT_PEER_CLAIM_TTL,
        )));
        view_b.lock().unwrap().observe_at(&relayed, Instant::now());
        registry_b.set_peer_claims(view_b);

        let err = registry_b
            .dispatch(&SweepKind::Issue(9789), None, None, None, None)
            .expect_err("host B must back off — a peer's claim is already live");
        assert!(
            err.downcast_ref::<CollisionDispatchError>()
                .is_some_and(|e| e.issue == 9789 && e.source == CollisionSource::PeerClaim),
            "expected a PeerClaim CollisionDispatchError, got: {err:#}"
        );
        assert_eq!(
            registry_b.len(),
            0,
            "host B must not record a sweep entry — only one host claims #9789"
        );
    }

    /// Issue #5789: a peer claim on a DIFFERENT issue never blocks this
    /// dispatch — the guard is scoped per-issue, not per-repo.
    #[test]
    fn dispatch_proceeds_when_the_peer_claim_is_for_a_different_issue() {
        let dir = tempdir().unwrap();
        let (mut registry, _log) = fixture_registry(dir.path());
        let repo = peer_claims::repo_slug(&registry.config().workspace_root);
        let view = Arc::new(Mutex::new(PeerClaimView::new(
            "self".into(),
            peer_claims::DEFAULT_PEER_CLAIM_TTL,
        )));
        view.lock().unwrap().observe_at(
            &ClaimAd::advertise(1, repo, "peer".into(), 1, "ts".into()),
            Instant::now(),
        );
        registry.set_peer_claims(view);

        let outcome = registry
            .dispatch(&SweepKind::Issue(2), None, None, None, None)
            .expect("a peer claim on a different issue must not block this dispatch");
        assert!(outcome.was_new);
    }

    /// Issue #5789: upgrades the #4085 pre-flip forge-label collision probe
    /// from detection-only into enforcement. Simulates the case the 2.95
    /// peer-claim guard cannot catch (no safehouse view attached, mirroring a
    /// fleet where the winning host's ad never reached this one) but the
    /// forge's own label state already shows the peer's `loom:building`
    /// flip — this host must back off instead of duplicating the sweep, MUST
    /// NOT reach `gh issue edit`, and MUST log the collision (the existing
    /// `detect_and_record_collision` diagnostic, reused verbatim).
    #[test]
    #[serial]
    fn dispatch_backs_off_on_confirmed_forge_label_collision() {
        let dir = tempdir().unwrap();
        let (mut registry, gh_log, spawn_log) = collision_dispatch_registry(
            dir.path(),
            r#"{"labels":[{"name":"loom:building"},{"name":"loom:curated"}]}"#,
        );
        registry.set_collision_detection(true);

        let err = registry
            .dispatch(&SweepKind::Issue(9790), None, None, None, None)
            .expect_err("a confirmed forge-label collision must back off dispatch");
        assert!(
            matches!(
                err.downcast_ref::<CollisionDispatchError>(),
                Some(CollisionDispatchError {
                    issue: 9790,
                    source: CollisionSource::ForgeLabel { .. },
                })
            ),
            "expected a ForgeLabel CollisionDispatchError, got: {err:#}"
        );
        assert_eq!(registry.collision_count(), 1);
        assert_eq!(registry.len(), 0, "no sweep entry must be recorded");

        let gh_calls = std::fs::read_to_string(&gh_log).unwrap_or_default();
        assert!(
            !gh_calls.contains("issue edit"),
            "the label flip must never be attempted on a confirmed collision; gh log: \
             {gh_calls}"
        );
        assert!(!spawn_log.exists(), "no child must ever be spawned on a confirmed collision");
    }

    /// Issue #5789: with collision detection at its default (disabled), the
    /// upgraded 4a guard is a pure no-op — a clean pre-flip label state
    /// (`loom:issue` present, `loom:building` absent) still dispatches
    /// normally, mirroring the pre-#5789 byte-for-byte-unchanged contract.
    #[test]
    #[serial]
    fn dispatch_proceeds_on_clean_preflip_labels_with_detection_enabled() {
        let dir = tempdir().unwrap();
        let (mut registry, gh_log, spawn_log) = collision_dispatch_registry(
            dir.path(),
            r#"{"labels":[{"name":"loom:issue"},{"name":"loom:curated"}]}"#,
        );
        registry.set_collision_detection(true);

        let outcome = registry
            .dispatch(&SweepKind::Issue(9791), None, None, None, None)
            .expect("a clean pre-flip read must not block dispatch");
        assert!(outcome.was_new);
        assert_eq!(registry.collision_count(), 0);

        let gh_calls = std::fs::read_to_string(&gh_log).unwrap_or_default();
        assert!(
            gh_calls.contains("issue edit"),
            "a clean collision read must still reach the label flip; gh log: {gh_calls}"
        );
        assert!(
            wait_for_contents(&spawn_log, "spawned", FIXTURE_CHILD_WAIT_MS),
            "the child must still spawn on a clean collision read"
        );
    }

    // ------------------------------------------------------------------------
    // Claim-then-verify-order dedup at dispatch time (Issue #6287, Epic
    // #6165 Phase 2)
    // ------------------------------------------------------------------------
    //
    // These two tests together model the "two near-simultaneous dispatches"
    // scenario from opposite perspectives, using
    // `lease_order_dispatch_registry`'s shared on-disk lease-comment store to
    // simulate the forge's own comment-ordering: one pre-seeds a peer's lease
    // comment (id 1) that already landed a moment before this dispatch's own
    // (id 2) — modeling the LOSING dispatcher — and the other pre-seeds
    // nothing, so this dispatch's own lease comment is unambiguously id 1 —
    // modeling the WINNING dispatcher. Exactly one of the two proceeds to
    // spawn a builder; the other never does.

    /// Issue #6287: when a peer's lease comment already exists on the issue
    /// (id 1, an earlier forge-assigned comment order) and this dispatcher's
    /// own lease write lands second (id 2), the claim-then-verify-order
    /// tie-break MUST make this dispatcher yield: no builder spawned, the
    /// claim lock released, the peer-claim advertisement retracted, and a
    /// `LeaseOrderDispatchError` naming the earlier host/sweep returned. The
    /// `loom:building` label flip itself (`issue edit`) MUST still have been
    /// attempted — the yield happens strictly after it, never instead of it.
    #[test]
    #[serial]
    fn dispatch_yields_claim_then_verify_order_tie_break_when_a_peer_lease_is_earlier() {
        let dir = tempdir().unwrap();
        let (mut registry, gh_log, spawn_log, comments_store) = lease_order_dispatch_registry(
            dir.path(),
            &["<!-- loom:lease host=peer-host sweep=sweep-issue-9820-peer -->"],
        );

        let err = registry
            .dispatch(&SweepKind::Issue(9820), None, None, None, None)
            .expect_err("losing the claim-then-verify-order tie-break must refuse dispatch");
        let lease_err = err
            .downcast_ref::<LeaseOrderDispatchError>()
            .unwrap_or_else(|| panic!("expected a LeaseOrderDispatchError, got: {err:#}"));
        assert_eq!(lease_err.issue, 9820);
        assert_eq!(lease_err.earliest_host, "peer-host");
        assert_eq!(lease_err.earliest_sweep_id, "sweep-issue-9820-peer");

        assert_eq!(registry.len(), 0, "no sweep entry must be recorded for a losing claim");
        assert!(
            !registry.config().locks_dir().join("issue-9820").exists(),
            "the claim lock this host acquired must be released on a losing tie-break"
        );
        assert!(!spawn_log.exists(), "no builder may ever be spawned for a losing claim");

        let gh_calls = std::fs::read_to_string(&gh_log).unwrap_or_default();
        assert!(
            gh_calls.contains("issue edit"),
            "the label flip must still have been attempted before the tie-break check; gh log: \
             {gh_calls}"
        );
        assert!(
            gh_calls.contains("loom:lease-yield"),
            "a standdown annotation must be posted when yielding; gh log: {gh_calls}"
        );

        // Both lease comments (the pre-seeded peer's and this dispatcher's
        // own, written by the real `write_lease_comment` call) must be
        // present in the shared store, proving the read-back actually saw
        // both — not just the peer's. A third record (the standdown
        // annotation just asserted above) also lands in the store, but its
        // `<!-- loom:lease-yield ...` marker does not match the
        // `<!-- loom:lease host=...` prefix `read_lease_comments`'s real
        // `--jq` filter selects on, so it must never count as a lease
        // record.
        let stored = std::fs::read_to_string(&comments_store).unwrap_or_default();
        assert!(stored.contains("peer-host"), "the peer's lease record must survive: {stored}");
        let lease_lines = stored
            .lines()
            .filter(|line| line.contains("\"body\":\"<!-- loom:lease host="))
            .count();
        assert!(
            stored.contains("sweep-issue-9820-peer") && lease_lines == 2,
            "this dispatcher's own lease write must also have landed, as the SECOND lease \
             record (excluding the non-matching standdown annotation): {stored}"
        );
    }

    /// Issue #6350 (Ask 2, generalizing #4485): losing the claim-then-
    /// verify-order tie-break is a no-progress outcome for THIS host exactly
    /// like the reaper's #4366 backstop, so it must arm the SAME per-issue
    /// dispatch backoff a failed dispatch does — otherwise nothing stops the
    /// very next work-finder tick from immediately re-losing the identical
    /// race against the same still-live earlier claimant (the "9 same-host
    /// re-acquisitions" shape observed on 2AMLogic/klayout-tools#994, where
    /// one of the two hosts' lease-order yields was this exact mechanism).
    #[test]
    #[serial]
    fn dispatch_arms_backoff_when_losing_claim_then_verify_order_tie_break() {
        let dir = tempdir().unwrap();
        let (mut registry, _gh_log, _spawn_log, _comments_store) = lease_order_dispatch_registry(
            dir.path(),
            &["<!-- loom:lease host=peer-host sweep=sweep-issue-9822-peer -->"],
        );

        assert_eq!(
            registry.dispatch_failure_count(9822),
            0,
            "no backoff must be armed before the first dispatch attempt"
        );

        let err = registry
            .dispatch(&SweepKind::Issue(9822), None, None, None, None)
            .expect_err("losing the claim-then-verify-order tie-break must refuse dispatch");
        assert!(
            err.downcast_ref::<LeaseOrderDispatchError>().is_some(),
            "expected a LeaseOrderDispatchError, got: {err:#}"
        );

        assert_eq!(
            registry.dispatch_failure_count(9822),
            1,
            "a lost lease-order tie-break must arm this issue's dispatch backoff (#6350) so the \
             next tick does not immediately repeat the same losing race"
        );
        assert!(
            registry
                .dispatch_backoff_remaining(9822, Utc::now())
                .is_some(),
            "the armed backoff must actually be in effect immediately after the yield"
        );
    }

    /// Issue #6287: the complementary case — no peer lease comment predates
    /// this dispatch's own, so its lease comment is (unambiguously) the
    /// earliest live one. The tie-break must be a no-op: dispatch succeeds
    /// exactly like the pre-#6287 behavior, and a builder is spawned.
    #[test]
    #[serial]
    fn dispatch_proceeds_when_this_sweeps_own_lease_is_the_earliest() {
        let dir = tempdir().unwrap();
        let (mut registry, gh_log, spawn_log, comments_store) =
            lease_order_dispatch_registry(dir.path(), &[]);

        let outcome = registry
            .dispatch(&SweepKind::Issue(9821), None, None, None, None)
            .expect("the earliest (only) lease comment must not block its own dispatch");
        assert!(outcome.was_new);

        assert!(
            wait_for_contents(&spawn_log, "spawned", FIXTURE_CHILD_WAIT_MS),
            "a winning tie-break must still spawn the builder"
        );

        let gh_calls = std::fs::read_to_string(&gh_log).unwrap_or_default();
        assert!(
            !gh_calls.contains("loom:lease-yield"),
            "a winning tie-break must never post a standdown annotation; gh log: {gh_calls}"
        );

        let stored = std::fs::read_to_string(&comments_store).unwrap_or_default();
        assert_eq!(
            stored.lines().count(),
            1,
            "exactly this dispatcher's own lease comment must exist: {stored}"
        );
    }

    // ------------------------------------------------------------------------
    // Epic #6165 Phase 2 combined regression suite (Issue #6288, Scenario 3):
    // "no duplicate builds when safehouse is down"
    // ------------------------------------------------------------------------
    //
    // Scenarios 1 (reclaim-race) and 2 (acquisition-race) already have
    // dedicated unit coverage of their own mechanism in isolation:
    // `reconcile_workspace_keeps_claim_when_lease_is_fresh_even_with_channel_absent`
    // (`claim_reconciliation.rs`, Issue #6286) and
    // `dispatch_yields_claim_then_verify_order_tie_break_when_a_peer_lease_is_earlier`
    // / `dispatch_proceeds_when_this_sweeps_own_lease_is_the_earliest` (this
    // file, Issue #6287). This test is the higher-level composition Issue
    // #6288 actually asks for: the exact shape of the historical duplicate-
    // build incidents (loom#6147, loom#6129, klayout-tools#939) — two hosts
    // racing to claim the same issue while the peer-claims/safehouse channel
    // gives neither of them any evidence about the other — with BOTH Phase 2
    // guards composed against the SAME lease-comment record, not two
    // independently-scripted fakes.

    /// Epic #6165's own end-to-end success criterion ("stopping `safehoused`
    /// on every host produces zero duplicate builds"), scoped to the Phase 2
    /// mechanisms (#6286 reclamation lease-freshness guard, #6287
    /// claim-then-verify-order dedup) — regression coverage for the
    /// duplicate-build shape behind loom#6147, loom#6129, and
    /// klayout-tools#939.
    ///
    /// Models two hosts racing to dispatch the SAME issue with the
    /// peer-claims/safehouse channel simulated fully absent for the whole
    /// scenario (`&|| None` below — "no evidence either way", the exact
    /// reading a host with `safehoused` killed produces, per
    /// `peer_claims::global_coordination_degraded_reason`'s doc comment —
    /// deliberately NOT `Some(reason)`, which would let the older #6157
    /// degraded-channel guard block reclamation on its own and prove
    /// nothing about whether #6286/#6287 are load-bearing WITHOUT it, per
    /// this epic's phase-2 issues' own framing):
    ///
    /// 1. A peer host's dispatcher already won the race: its lease comment
    ///    (id 1) is pre-seeded in the shared store, modeling a live, already-
    ///    dispatched sweep on that host.
    /// 2. This host's own dispatch of the identical issue writes its lease
    ///    comment second (id 2) and MUST yield before spawning a builder —
    ///    the claim-then-verify-order tie-break (#6287) — so at most one
    ///    builder is ever spawned for the issue.
    /// 3. A reclamation pass then runs against the peer's surviving claim,
    ///    with LOCAL evidence (a dead-pid journal entry) that would normally
    ///    fire an UNCONDITIONAL, immediate reclaim — and is refused anyway,
    ///    because the peer's lease record (the same id-1 comment this
    ///    dispatch's own tie-break just read) is still fresh — the
    ///    reclamation lease-freshness guard (#6286).
    ///
    /// Together: exactly one live claim survives the combined race, and the
    /// peer-claims/safehouse channel contributed no evidence to either
    /// decision.
    #[test]
    #[serial]
    fn combined_dispatch_and_reclaim_race_produces_no_duplicate_build_with_safehouse_down() {
        let dir = tempdir().unwrap();
        let repo_root = dir.path().join("repo");
        std::fs::create_dir_all(&repo_root).unwrap();
        let repo_str = repo_root.display().to_string();
        const ISSUE: u32 = 9830;

        // The reconciliation pass below resolves its journal via the
        // process-global env override, independent of this registry's own
        // `config.journal_path` -- point it at a scratch file for this test.
        let journal_path = dir.path().join("reconcile-sweeps.json");
        std::env::set_var(sweep_journal::JOURNAL_PATH_ENV, &journal_path);

        // Local evidence a reconciliation pass would normally act on
        // immediately and unconditionally: a journal entry recording a
        // now-dead pid (0) for the SAME issue the peer's lease claims,
        // exactly like the #6286 fixture this scenario composes with.
        let mut journal = sweep_journal::SweepJournal::default();
        journal.entries.push(sweep_journal::JournalEntry {
            repo: repo_str.clone(),
            issue: ISSUE,
            pid: 0,
            started_at: Utc::now(),
        });
        sweep_journal::save(&journal_path, &journal).unwrap();

        let label_updated_at = Utc::now().to_rfc3339();
        let (mut registry, gh_log, spawn_log, comments_store) = safehouse_down_combined_registry(
            &repo_root,
            &["<!-- loom:lease host=peer-host sweep=sweep-issue-9830-peer -->"],
            ISSUE,
            &label_updated_at,
        );

        // --- Step 1 (Scenario 2's mechanism): this host loses the
        //     acquisition race and yields before spawning a builder. ---
        let err = registry
            .dispatch(&SweepKind::Issue(ISSUE), None, None, None, None)
            .expect_err(
                "losing the claim-then-verify-order tie-break must refuse this host's dispatch",
            );
        let lease_err = err
            .downcast_ref::<LeaseOrderDispatchError>()
            .unwrap_or_else(|| panic!("expected a LeaseOrderDispatchError, got: {err:#}"));
        assert_eq!(lease_err.earliest_host, "peer-host");
        assert_eq!(lease_err.earliest_sweep_id, "sweep-issue-9830-peer");
        assert!(
            !spawn_log.exists(),
            "no builder may ever be spawned for the losing side of the acquisition race"
        );

        let stored_after_dispatch = std::fs::read_to_string(&comments_store).unwrap_or_default();
        let lease_lines = stored_after_dispatch
            .lines()
            .filter(|line| line.contains("\"body\":\"<!-- loom:lease host="))
            .count();
        assert_eq!(
            lease_lines, 2,
            "both the peer's pre-seeded lease and this host's own losing write must be in the \
             shared store the reclamation pass below reads: {stored_after_dispatch}"
        );

        // --- Step 2 (Scenario 1's mechanism, composed): a reclamation pass
        //     against the peer's surviving claim, with the safehouse channel
        //     simulated fully absent, must still refuse -- the peer's lease
        //     (the very comment this dispatch's own tie-break just read) is
        //     still fresh. ---
        let gh_bin = registry.config().gh_bin.clone().unwrap();
        let (checked, reclaimed) =
            crate::claim_reconciliation::forge::reconcile_workspace_with_coordination(
                &gh_bin,
                &repo_root,
                &|| None,
                false,
            );
        assert_eq!(
            checked, 1,
            "the surviving claim is still inspected -- only the ACTION is frozen"
        );
        assert_eq!(
            reclaimed, 0,
            "the peer's fresh lease must block reclamation even though the dead-pid journal \
             evidence alone would normally reclaim immediately, and even with the \
             peer-claims/safehouse channel contributing no evidence at all (#6286, composed \
             with #6287's own acquisition-race tie-break above -- Issue #6288 Scenario 3)"
        );

        let gh_calls = std::fs::read_to_string(&gh_log).unwrap_or_default();
        assert!(
            gh_calls.contains("issue edit"),
            "the label flip from the dispatch race must still have been attempted; gh log: \
             {gh_calls}"
        );
        assert!(
            !gh_calls.contains("--add-label loom:issue"),
            "no gh issue edit reverting the surviving claim's loom:building label may be issued \
             while its lease is fresh; gh log: {gh_calls}"
        );

        // Nothing was reclaimed, so the journal entry the reclamation pass
        // read from must survive untouched.
        let after = sweep_journal::load(&journal_path);
        assert!(sweep_journal::find(&after, &repo_str, ISSUE).is_some());

        std::env::remove_var(sweep_journal::JOURNAL_PATH_ENV);
    }

    /// Issue #3953: `dispatch` persists a liveness record to the sweep
    /// journal (repo/issue/pid/started_at), and the reaper's dead-PID path
    /// removes it once the child is confirmed dead — end-to-end wiring,
    /// not just the `sweep_journal` module's own unit tests.
    #[test]
    fn dispatch_and_reap_wire_the_sweep_journal() {
        let dir = tempdir().unwrap();
        let (mut registry, _record_log) = fixture_registry(dir.path());
        let journal_path = registry.config().journal_path.clone().unwrap();

        let outcome = registry
            .dispatch(&SweepKind::Issue(4600), None, None, None, None)
            .expect("dispatch should succeed");

        let journal = crate::sweep_journal::load(&journal_path);
        let repo = dir.path().display().to_string();
        let entry = crate::sweep_journal::find(&journal, &repo, 4600)
            .expect("dispatch should have recorded a journal entry for #4600");
        assert_eq!(entry.pid, outcome.pid);

        // The fixture's fake spawn-claude.sh exits immediately, so the pid is
        // dead almost immediately; wait for the reaper's dead-PID path. The
        // budget is deliberately generous (#3985) so host CPU starvation — not
        // a code fault — can never redden this via a missed deadline.
        let dead = wait_for_condition(FIXTURE_CHILD_WAIT_MS, || !is_pid_alive(outcome.pid));
        assert!(dead, "fixture child did not exit within the wait budget");

        registry.reap_once();

        let journal = crate::sweep_journal::load(&journal_path);
        assert!(
            crate::sweep_journal::find(&journal, &repo, 4600).is_none(),
            "reap_once should have removed the dead sweep's journal entry"
        );
    }

    /// Issue #3824: `spawn_child` unconditionally appends
    /// `--dangerously-skip-permissions` to the child argv so a detached,
    /// non-interactive `claude -p` sweep never stalls on a permission prompt.
    /// With no model/effort/depends-on the flag directly follows the
    /// `--claim-owned <N>` marker (#4111, always emitted for a daemon
    /// dispatch), appended AFTER it (verified by the exact positional form).
    #[test]
    #[serial]
    fn dispatch_appends_dangerously_skip_permissions() {
        let dir = tempdir().unwrap();
        let (mut registry, record_log) = fixture_registry(dir.path());

        let outcome = registry
            .dispatch(&SweepKind::Issue(4242), None, None, None, None)
            .expect("dispatch should succeed");

        let needle = format!("LOOM_TERMINAL_ID=daemon-{}", outcome.sweep_id);
        let recorded = assert_child_wrote(&record_log, &needle);
        assert!(
            recorded.contains(
                "argv: -p /loom:sweep 4242 --claim-owned 4242 --dangerously-skip-permissions"
            ),
            "expected --claim-owned then --dangerously-skip-permissions appended after the \
             prompt; got: {recorded}"
        );
    }

    /// Issue #4255: a daemon dispatch routes the child through
    /// `claude-wrapper.sh` by appending `--use-wrapper` immediately AFTER
    /// `--dangerously-skip-permissions`, so a transient API death (rate-limit /
    /// 5xx / overloaded / bare `Execution error`) is retried instead of killing
    /// the whole sweep on the first failure. Serialized on the named
    /// `loom_use_wrapper_env` lock shared with every other test that reads or
    /// mutates `LOOM_USE_WRAPPER` (this module + `role_runner`), so a concurrent
    /// opt-out test cannot flip the flag mid-run.
    #[test]
    #[serial(loom_use_wrapper_env)]
    fn dispatch_appends_use_wrapper_flag() {
        std::env::remove_var("LOOM_USE_WRAPPER");
        let dir = tempdir().unwrap();
        let (mut registry, record_log) = fixture_registry(dir.path());

        let outcome = registry
            .dispatch(&SweepKind::Issue(4255), None, None, None, None)
            .expect("dispatch should succeed");

        let needle = format!("LOOM_TERMINAL_ID=daemon-{}", outcome.sweep_id);
        let recorded = assert_child_wrote(&record_log, &needle);
        assert!(
            recorded.contains("--dangerously-skip-permissions --use-wrapper"),
            "expected --use-wrapper appended after --dangerously-skip-permissions; got: {recorded}"
        );
        // The flag must be its OWN argv token (spawn-claude.sh consumes it), not
        // folded into the prompt like --claim-owned.
        assert!(
            recorded.contains("arg: --use-wrapper"),
            "expected --use-wrapper as a standalone argv token; got: {recorded}"
        );
    }

    /// Issue #4255: the `LOOM_USE_WRAPPER=0` debug opt-out restores the legacy
    /// single-shot argv — no `--use-wrapper` token — so an operator can
    /// reproduce a raw first-shot failure. Shares the named `loom_use_wrapper_env`
    /// lock so it never races the presence tests that assume the wrapper-on default.
    #[test]
    #[serial(loom_use_wrapper_env)]
    fn dispatch_opt_out_omits_use_wrapper_flag() {
        std::env::set_var("LOOM_USE_WRAPPER", "0");
        let dir = tempdir().unwrap();
        let (mut registry, record_log) = fixture_registry(dir.path());

        let outcome = registry
            .dispatch(&SweepKind::Issue(4256), None, None, None, None)
            .expect("dispatch should succeed");

        let needle = format!("LOOM_TERMINAL_ID=daemon-{}", outcome.sweep_id);
        let recorded = assert_child_wrote(&record_log, &needle);
        std::env::remove_var("LOOM_USE_WRAPPER");
        assert!(
            !recorded.contains("--use-wrapper"),
            "LOOM_USE_WRAPPER=0 must suppress --use-wrapper; got: {recorded}"
        );
        // The rest of the argv contract is unchanged.
        assert!(
            recorded.contains("--dangerously-skip-permissions"),
            "opt-out must not drop --dangerously-skip-permissions; got: {recorded}"
        );
    }

    /// Issue #3823 (Option A): `spawn_child` exports the claim-ownership
    /// marker `LOOM_SWEEP_CLAIM_OWNED=<issue>` into the dispatched child so its
    /// `/loom:sweep` pre-flight recognises the daemon's own pre-dispatch
    /// loom:building flip as its OWN claim (and proceeds to build) rather than
    /// self-skipping. The value is exactly the dispatched issue number.
    #[test]
    #[serial]
    fn dispatch_exports_claim_ownership_marker() {
        let dir = tempdir().unwrap();
        let (mut registry, record_log) = fixture_registry(dir.path());

        let outcome = registry
            .dispatch(&SweepKind::Issue(4243), None, None, None, None)
            .expect("dispatch should succeed");

        let needle = format!("LOOM_TERMINAL_ID=daemon-{}", outcome.sweep_id);
        let recorded = assert_child_wrote(&record_log, &needle);
        assert!(
            recorded.contains("LOOM_SWEEP_CLAIM_OWNED=4243"),
            "expected claim-ownership marker for issue 4243; got: {recorded}"
        );
    }

    /// Issue #4111 (Option 1, the positional half of the fix): in addition to
    /// the `LOOM_SWEEP_CLAIM_OWNED` env var above, `spawn_child` appends
    /// `--claim-owned <issue>` to the child's own argv. This is the primary
    /// signal — positional in the model's context by construction — that
    /// `/loom:sweep`'s mandatory Step 1a pre-flight check consumes. Asserts
    /// BOTH channels are present on the same dispatch (belt-and-suspenders,
    /// per the issue's explicit "keep the env var exported regardless for
    /// backward compatibility" guidance) and that the flag carries exactly
    /// the dispatched issue number, unconditionally (unlike the
    /// optional --model/--effort/--depends-on flags, this one is never
    /// absent on a daemon dispatch).
    #[test]
    #[serial]
    fn dispatch_appends_claim_owned_flag() {
        let dir = tempdir().unwrap();
        let (mut registry, record_log) = fixture_registry(dir.path());

        let outcome = registry
            .dispatch(&SweepKind::Issue(4246), None, None, None, None)
            .expect("dispatch should succeed");

        let needle = format!("LOOM_TERMINAL_ID=daemon-{}", outcome.sweep_id);
        let recorded = assert_child_wrote(&record_log, &needle);
        assert!(
            recorded.contains("argv: -p /loom:sweep 4246 --claim-owned 4246"),
            "expected --claim-owned 4246 in argv immediately after the prompt; got: {recorded}"
        );
        // Regression for the #4120 review: `--claim-owned` MUST be embedded in
        // the `-p` prompt string, NOT appended as a sibling argv token. The
        // real `claude` CLI rejects `--claim-owned` as an unknown option and
        // exits 1 if it arrives as its own token; only text inside the single
        // `-p "<prompt>"` value reaches the `/loom:sweep` skill's `$ARGUMENTS`.
        // The fixture records each argv token on its own `arg: ` line, so we
        // can assert the flag is part of the prompt VALUE (one token that also
        // carries `/loom:sweep`) and NOT a standalone `arg: --claim-owned`
        // token — the `$*`-substring assertion above cannot tell these apart
        // (which is precisely how the original sibling-arg bug slipped through).
        assert!(
            recorded.contains("arg: /loom:sweep 4246 --claim-owned 4246"),
            "expected --claim-owned inside the single -p prompt token; got: {recorded}"
        );
        assert!(
            !recorded.contains("arg: --claim-owned"),
            "--claim-owned must NOT be a standalone argv token (the real claude CLI \
             rejects it as an unknown option); got: {recorded}"
        );
        // Belt-and-suspenders: the env var must still be present too (#3823
        // backward compatibility, per #4111's explicit guidance to keep it).
        assert!(
            recorded.contains("LOOM_SWEEP_CLAIM_OWNED=4246"),
            "expected the LOOM_SWEEP_CLAIM_OWNED env var alongside the flag; got: {recorded}"
        );
    }

    // -- #6529: per-owner GH_CONFIG_DIR forwarded to sweep-dispatch children --
    //
    // Same root-cause class already fixed for the daemon-internal call sites
    // (#5401/#5431) and role-runner-dispatched children (#5508/#5522): a
    // sweep child spawned by `spawn_child` inherits `current_dir(&self.config
    // .workspace_root)`, so it must carry the SAME per-owner `GH_CONFIG_DIR`
    // — otherwise it can inherit whatever `GH_CONFIG_DIR` the daemon process
    // (or a previously dispatched sweep child for a DIFFERENT workspace)
    // happens to have, and every forge call the spawned `/loom:sweep`
    // session makes 404s silently before the sweep's first checkpoint.

    /// A sweep child dispatched for a workspace registered under a
    /// non-default owner (mirrors a cross-owner managed repo, #5401/#5431)
    /// must carry that owner's `GH_CONFIG_DIR` on its spawned
    /// `spawn-claude.sh` process.
    #[test]
    #[serial]
    fn dispatch_forwards_owner_gh_config_dir_for_a_registered_root() {
        let dir = tempdir().unwrap();
        crate::credential_preflight::clear_owner_root_registry();
        let owner_dir = dir.path().join(".loom/gh-config-by-owner/2AMLogic");
        crate::credential_preflight::register_root_gh_config_dir(dir.path(), &owner_dir);

        let (mut registry, record_log) = fixture_registry(dir.path());
        let outcome = registry
            .dispatch(&SweepKind::Issue(6529), None, None, None, None)
            .expect("dispatch should succeed");

        let needle = format!("LOOM_TERMINAL_ID=daemon-{}", outcome.sweep_id);
        let recorded = assert_child_wrote(&record_log, &needle);
        crate::credential_preflight::clear_owner_root_registry();
        assert!(
            recorded.contains(&format!("GH_CONFIG_DIR={}", owner_dir.display())),
            "expected the registered owner's GH_CONFIG_DIR on the sweep child; got: {recorded}"
        );
    }

    /// The flip side: a workspace that is NOT registered under a non-default
    /// owner (the common single-owner fleet, or the root owner's own repos)
    /// must be a byte-identical no-op — the sweep child's `GH_CONFIG_DIR` is
    /// left untouched so it inherits the daemon's own process-global
    /// default, never a stale value left behind by a PREVIOUSLY dispatched
    /// sweep child for a different workspace.
    #[test]
    #[serial]
    fn dispatch_leaves_gh_config_dir_untouched_for_an_unregistered_root() {
        let _env_guard = ClearedGhConfigDirEnv::new();
        let dir = tempdir().unwrap();
        crate::credential_preflight::clear_owner_root_registry();

        let (mut registry, record_log) = fixture_registry(dir.path());
        let outcome = registry
            .dispatch(&SweepKind::Issue(6530), None, None, None, None)
            .expect("dispatch should succeed");

        let needle = format!("LOOM_TERMINAL_ID=daemon-{}", outcome.sweep_id);
        let recorded = assert_child_wrote(&record_log, &needle);
        assert!(
            recorded.contains("GH_CONFIG_DIR=unset"),
            "expected no GH_CONFIG_DIR forwarded for an unregistered root; got: {recorded}"
        );
    }

    /// Issue #3943: `spawn_child` pins
    /// `CLAUDE_CODE_PRINT_BG_WAIT_CEILING_MS=0` on the dispatched child env so
    /// the print-mode harness does not reap the sweep's long-running
    /// Builder/Judge background subagents at the 600s ceiling (which caused
    /// loom:building<->loom:issue label ping-pong). The value is exactly "0"
    /// (no cap).
    #[test]
    #[serial]
    fn dispatch_disables_print_bg_wait_ceiling() {
        let dir = tempdir().unwrap();
        let (mut registry, record_log) = fixture_registry(dir.path());

        let outcome = registry
            .dispatch(&SweepKind::Issue(4244), None, None, None, None)
            .expect("dispatch should succeed");

        let needle = format!("LOOM_TERMINAL_ID=daemon-{}", outcome.sweep_id);
        let recorded = assert_child_wrote(&record_log, &needle);
        assert!(
            recorded.contains("CLAUDE_CODE_PRINT_BG_WAIT_CEILING_MS=0"),
            "expected print-mode bg-wait ceiling disabled (=0); got: {recorded}"
        );
    }

    /// Issue #3477 (Phase 1): a `model` dispatch param threads through to
    /// the spawn command as an explicit `--model <value>` argument.
    #[test]
    #[serial]
    fn dispatch_with_model_appends_model_arg() {
        let dir = tempdir().unwrap();
        let (mut registry, record_log) = fixture_registry(dir.path());

        let outcome = registry
            .dispatch(&SweepKind::Issue(43), None, Some("claude-sonnet-4-6"), None, None)
            .expect("dispatch should succeed");

        let needle = format!("LOOM_TERMINAL_ID=daemon-{}", outcome.sweep_id);
        let recorded = assert_child_wrote(&record_log, &needle);
        assert!(
            recorded.contains("argv: -p /loom:sweep 43 --claim-owned 43 --model claude-sonnet-4-6"),
            "expected --model in argv; got: {recorded}"
        );
        // #3482 (Phase 3a): the dispatch model is carried on the registry
        // entry so list_sweeps / get_sweep_status report it.
        assert_eq!(
            registry.get(&outcome.sweep_id).unwrap().model.as_deref(),
            Some("claude-sonnet-4-6"),
            "dispatch model must be recorded on the SweepInfo entry"
        );
    }

    /// Issue #3477: an empty-string model is treated as unset — `--model ""`
    /// must never be emitted (acceptance criterion: no flag at all, not an
    /// empty flag).
    #[test]
    #[serial]
    fn dispatch_with_empty_model_emits_no_model_flag() {
        let dir = tempdir().unwrap();
        let (mut registry, record_log) = fixture_registry(dir.path());

        let outcome = registry
            .dispatch(&SweepKind::Issue(44), None, Some(""), None, None)
            .expect("dispatch should succeed");

        let needle = format!("LOOM_TERMINAL_ID=daemon-{}", outcome.sweep_id);
        let recorded = assert_child_wrote(&record_log, &needle);
        assert!(
            !recorded.contains("--model"),
            "empty model must not emit --model; got: {recorded}"
        );
        // #3482: empty-string model normalizes to None on the entry too.
        assert_eq!(
            registry.get(&outcome.sweep_id).unwrap().model,
            None,
            "empty model must be recorded as None on the SweepInfo entry"
        );
    }

    /// Issue #3716: an `effort` dispatch param threads through to the spawn
    /// command as an explicit `--effort <level>` argument, mirroring `--model`.
    #[test]
    #[serial]
    fn dispatch_with_effort_appends_effort_arg() {
        let dir = tempdir().unwrap();
        let (mut registry, record_log) = fixture_registry(dir.path());

        let outcome = registry
            .dispatch(&SweepKind::Issue(45), None, None, Some("xhigh"), None)
            .expect("dispatch should succeed");

        let needle = format!("LOOM_TERMINAL_ID=daemon-{}", outcome.sweep_id);
        let recorded = assert_child_wrote(&record_log, &needle);
        assert!(
            recorded.contains("argv: -p /loom:sweep 45 --claim-owned 45 --effort xhigh"),
            "expected --effort in argv; got: {recorded}"
        );
        // The dispatch effort is carried on the registry entry so
        // list_sweeps / get_sweep_status report it (mirrors #3482 for model).
        assert_eq!(
            registry.get(&outcome.sweep_id).unwrap().effort.as_deref(),
            Some("xhigh"),
            "dispatch effort must be recorded on the SweepInfo entry"
        );
    }

    /// Issue #3716: `model` + `effort` both set emit both flags, in the
    /// order `--model <m> --effort <e>` (effort appended right after model).
    #[test]
    #[serial]
    fn dispatch_with_model_and_effort_appends_both_args() {
        let dir = tempdir().unwrap();
        let (mut registry, record_log) = fixture_registry(dir.path());

        let outcome = registry
            .dispatch(&SweepKind::Issue(46), None, Some("claude-sonnet-4-6"), Some("xhigh"), None)
            .expect("dispatch should succeed");

        let needle = format!("LOOM_TERMINAL_ID=daemon-{}", outcome.sweep_id);
        let recorded = assert_child_wrote(&record_log, &needle);
        assert!(
            recorded.contains(
                "argv: -p /loom:sweep 46 --claim-owned 46 --model claude-sonnet-4-6 --effort xhigh"
            ),
            "expected --model then --effort in argv; got: {recorded}"
        );
        let entry = registry.get(&outcome.sweep_id).unwrap();
        assert_eq!(entry.model.as_deref(), Some("claude-sonnet-4-6"));
        assert_eq!(entry.effort.as_deref(), Some("xhigh"));
    }

    /// Issue #3716: an empty-string effort is treated as unset — `--effort ""`
    /// must never be emitted (no flag at all, not an empty flag).
    #[test]
    #[serial]
    fn dispatch_with_empty_effort_emits_no_effort_flag() {
        let dir = tempdir().unwrap();
        let (mut registry, record_log) = fixture_registry(dir.path());

        let outcome = registry
            .dispatch(&SweepKind::Issue(47), None, None, Some(""), None)
            .expect("dispatch should succeed");

        let needle = format!("LOOM_TERMINAL_ID=daemon-{}", outcome.sweep_id);
        let recorded = assert_child_wrote(&record_log, &needle);
        assert!(
            !recorded.contains("--effort"),
            "empty effort must not emit --effort; got: {recorded}"
        );
        // Empty-string effort normalizes to None on the entry too.
        assert_eq!(
            registry.get(&outcome.sweep_id).unwrap().effort,
            None,
            "empty effort must be recorded as None on the SweepInfo entry"
        );
    }

    /// Issue #3729 (stacked-PR v1): a `depends_on` dispatch param threads
    /// through to the spawn command as an explicit `--depends-on <N>`
    /// argument, mirroring `--model` / `--effort`. It is recorded on the
    /// SweepInfo entry so the reaper can block the subtree on parent failure.
    #[test]
    #[serial]
    fn dispatch_with_depends_on_appends_depends_on_arg() {
        let dir = tempdir().unwrap();
        let (mut registry, record_log) = fixture_registry(dir.path());

        let outcome = registry
            .dispatch(&SweepKind::Issue(50), None, None, None, Some(49))
            .expect("dispatch should succeed");

        let needle = format!("LOOM_TERMINAL_ID=daemon-{}", outcome.sweep_id);
        let recorded = assert_child_wrote(&record_log, &needle);
        assert!(
            recorded.contains("argv: -p /loom:sweep 50 --claim-owned 50 --depends-on 49"),
            "expected --depends-on in argv; got: {recorded}"
        );
        // Regression for #4121 (mirrors the #4120 `--claim-owned` review): the
        // flattened `argv:` assertion above renders byte-identically whether
        // `--depends-on 49` is embedded in the single `-p` prompt token or
        // appended as its own sibling argv token — so it cannot distinguish
        // the fixed and buggy forms. The real `claude` CLI rejects
        // `--depends-on` as an unknown option if it ever arrives as a
        // standalone token; only text inside the `-p "<prompt>"` value
        // reaches the `/loom:sweep` skill's `$ARGUMENTS`. Use the per-token
        // `arg: ` fixture lines (as `dispatch_appends_claim_owned_flag` does)
        // to assert the flag is part of the prompt VALUE and NOT a
        // standalone token.
        assert!(
            recorded.contains("arg: /loom:sweep 50 --claim-owned 50 --depends-on 49"),
            "expected --depends-on inside the single -p prompt token; got: {recorded}"
        );
        assert!(
            !recorded.contains("arg: --depends-on"),
            "--depends-on must NOT be a standalone argv token (the real claude CLI \
             rejects it as an unknown option); got: {recorded}"
        );
        assert_eq!(
            registry.get(&outcome.sweep_id).unwrap().depends_on,
            Some(49),
            "dispatch depends_on must be recorded on the SweepInfo entry"
        );
    }

    /// Issue #3729: absent `depends_on`, no `--depends-on` flag is emitted —
    /// byte-for-byte unchanged behavior (opt-in, no default-path regression).
    #[test]
    #[serial]
    fn dispatch_without_depends_on_emits_no_flag() {
        let dir = tempdir().unwrap();
        let (mut registry, record_log) = fixture_registry(dir.path());

        let outcome = registry
            .dispatch(&SweepKind::Issue(51), None, None, None, None)
            .expect("dispatch should succeed");

        let needle = format!("LOOM_TERMINAL_ID=daemon-{}", outcome.sweep_id);
        let recorded = assert_child_wrote(&record_log, &needle);
        assert!(
            !recorded.contains("--depends-on"),
            "depends_on=None must not emit --depends-on; got: {recorded}"
        );
        assert_eq!(
            registry.get(&outcome.sweep_id).unwrap().depends_on,
            None,
            "depends_on=None must be recorded as None on the SweepInfo entry"
        );
    }

    /// Issue #3730: when the experiment-related env vars are set in the daemon
    /// process, `spawn_child` forwards them (via the explicit allowlist) to the
    /// detached child, and pins the child's cwd to the workspace root.
    #[test]
    #[serial]
    fn dispatch_forwards_experiment_env_and_sets_cwd() {
        let dir = tempdir().unwrap();
        // Canonicalize because the fixture records `pwd -P` (symlink-resolved),
        // while tempdir() on macOS lives under a /var -> /private/var symlink.
        let expected_cwd = std::fs::canonicalize(dir.path()).unwrap();
        let (mut registry, record_log) = fixture_registry(dir.path());

        // Export the experiment vars into the daemon (test) process env just
        // before dispatch — this is exactly the operator scenario #3730 fixes.
        std::env::set_var("LOOM_MODEL_EXPERIMENT", "canary");
        std::env::set_var("LOOM_MODEL_EXPERIMENT_CANARY", "1");
        std::env::set_var("LOOM_TRANSCRIPT_ARCHIVE", "/tmp/loom-archive-3730");

        let outcome = registry
            .dispatch(&SweepKind::Issue(48), None, None, None, None)
            .expect("dispatch should succeed");

        // Clean up the process env immediately so a failure below can't leak
        // into sibling #[serial] tests.
        std::env::remove_var("LOOM_MODEL_EXPERIMENT");
        std::env::remove_var("LOOM_MODEL_EXPERIMENT_CANARY");
        std::env::remove_var("LOOM_TRANSCRIPT_ARCHIVE");

        let needle = format!("LOOM_TERMINAL_ID=daemon-{}", outcome.sweep_id);
        let recorded = assert_child_wrote(&record_log, &needle);
        assert!(
            recorded.contains("LOOM_MODEL_EXPERIMENT=canary"),
            "expected LOOM_MODEL_EXPERIMENT forwarded to child; got: {recorded}"
        );
        assert!(
            recorded.contains("LOOM_MODEL_EXPERIMENT_CANARY=1"),
            "expected LOOM_MODEL_EXPERIMENT_CANARY forwarded to child; got: {recorded}"
        );
        assert!(
            recorded.contains("LOOM_TRANSCRIPT_ARCHIVE=/tmp/loom-archive-3730"),
            "expected LOOM_TRANSCRIPT_ARCHIVE forwarded to child; got: {recorded}"
        );
        assert!(
            recorded.contains(&format!("PWD={}", expected_cwd.display())),
            "expected child cwd pinned to workspace root {}; got: {recorded}",
            expected_cwd.display()
        );
    }

    /// Issue #3730 no-op criterion: when none of the experiment env vars are
    /// set in the daemon process, `spawn_child` does NOT forward them to the
    /// child (the child observes them as unset). The cwd is still pinned to
    /// the workspace root regardless.
    #[test]
    #[serial]
    fn dispatch_does_not_forward_unset_experiment_env() {
        // Ensure a clean slate — a leaked value from another test would make
        // this a false pass.
        std::env::remove_var("LOOM_MODEL_EXPERIMENT");
        std::env::remove_var("LOOM_MODEL_EXPERIMENT_CANARY");
        std::env::remove_var("LOOM_TRANSCRIPT_ARCHIVE");

        let dir = tempdir().unwrap();
        let expected_cwd = std::fs::canonicalize(dir.path()).unwrap();
        let (mut registry, record_log) = fixture_registry(dir.path());

        let outcome = registry
            .dispatch(&SweepKind::Issue(49), None, None, None, None)
            .expect("dispatch should succeed");

        let needle = format!("LOOM_TERMINAL_ID=daemon-{}", outcome.sweep_id);
        let recorded = assert_child_wrote(&record_log, &needle);
        // The fixture prints `<VAR>=unset` when the child sees the var unset.
        assert!(
            recorded.contains("LOOM_MODEL_EXPERIMENT=unset"),
            "unset LOOM_MODEL_EXPERIMENT must not be forwarded; got: {recorded}"
        );
        assert!(
            recorded.contains("LOOM_MODEL_EXPERIMENT_CANARY=unset"),
            "unset LOOM_MODEL_EXPERIMENT_CANARY must not be forwarded; got: {recorded}"
        );
        assert!(
            recorded.contains("LOOM_TRANSCRIPT_ARCHIVE=unset"),
            "unset LOOM_TRANSCRIPT_ARCHIVE must not be forwarded; got: {recorded}"
        );
        // cwd is pinned unconditionally.
        assert!(
            recorded.contains(&format!("PWD={}", expected_cwd.display())),
            "expected child cwd pinned to workspace root {}; got: {recorded}",
            expected_cwd.display()
        );
    }

    #[test]
    #[serial]
    fn dispatch_lock_collision_rejected() {
        let dir = tempdir().unwrap();
        let (mut registry, _record_log) = fixture_registry(dir.path());

        let first = registry.dispatch(&SweepKind::Issue(7), None, None, None, None);
        assert!(first.is_ok());

        let second = registry.dispatch(&SweepKind::Issue(7), None, None, None, None);
        assert!(second.is_err(), "second dispatch for issue #7 should fail (lock collision)");
        let err = second.unwrap_err().to_string();
        assert!(err.contains("lock collision"), "expected lock collision error; got: {err}");
    }

    #[test]
    #[serial]
    fn dispatch_idempotency_returns_existing() {
        let dir = tempdir().unwrap();
        let (mut registry, _record_log) = fixture_registry(dir.path());

        let first = registry
            .dispatch(&SweepKind::Issue(99), Some("key-A".to_string()), None, None, None)
            .unwrap();
        assert!(first.was_new);

        // While still Running, a dispatch with the same key must dedup.
        // Issue #99 is the same kind, but we don't need a different issue —
        // the dedup is purely on the idempotency key.
        let second = registry
            .dispatch(&SweepKind::Issue(99), Some("key-A".to_string()), None, None, None)
            .unwrap();
        assert!(!second.was_new);
        assert_eq!(first.sweep_id, second.sweep_id);
    }

    /// Issue #5342: `PrSet` dispatch is no longer rejected — it spawns via
    /// the same `spawn_child` path `Issue` uses, is tracked in the registry
    /// exactly like an `Issue` sweep (`list`/`get_status` both see it), and
    /// its per-PR claim lock (`.loom/locks/pr-<N>/`, distinct from `Issue`'s
    /// `.loom/locks/issue-<N>/`) refuses a second dispatch that shares a PR
    /// number with an already-dispatched set.
    #[test]
    #[serial]
    fn pr_set_dispatch_succeeds_is_tracked_and_locks_per_pr() {
        let dir = tempdir().unwrap();
        let (mut registry, _record_log) = fixture_registry(dir.path());

        let outcome = registry
            .dispatch(&SweepKind::PrSet(vec![101, 202]), None, None, None, None)
            .expect("PrSet dispatch should succeed (#5342)");
        assert!(outcome.was_new);

        let listed = registry.list(None);
        assert_eq!(listed.len(), 1, "PrSet dispatch should produce exactly one tracked sweep");
        assert_eq!(listed[0].sweep_id, outcome.sweep_id);
        match &listed[0].kind {
            SweepKind::PrSet(prs) => assert_eq!(prs, &vec![101, 202]),
            other => panic!("expected SweepKind::PrSet, got {other:?}"),
        }
        assert!(
            registry.get_status(&outcome.sweep_id).is_some(),
            "get_sweep_status should find the PrSet sweep"
        );

        // A second PrSet dispatch sharing PR #101 collides on the per-PR lock
        // (Issue #5342's "per-issue lock semantics resolved" requirement).
        let collide = registry.dispatch(&SweepKind::PrSet(vec![101, 303]), None, None, None, None);
        let err = collide.unwrap_err().to_string();
        assert!(err.contains("PR #101"), "expected a PR #101 lock-collision error; got: {err}");

        // The sweep can be cancelled (Issue #5342's explicit test AC): the
        // fake spawn fixture exits almost immediately, so `cancel` may
        // observe an already-terminal child, but either way the call
        // succeeds and the sweep_id matches.
        let cancelled = registry
            .cancel(&outcome.sweep_id, Duration::from_millis(500))
            .expect("PrSet sweep should be cancellable");
        assert_eq!(cancelled.sweep_id, outcome.sweep_id);

        // Cancelling releases every PR's lock, so a fresh PrSet dispatch
        // reusing the same PR numbers is no longer blocked.
        let after = registry.dispatch(&SweepKind::PrSet(vec![101, 202]), None, None, None, None);
        assert!(after.is_ok(), "PR locks should be released after cancel: {after:?}");
    }

    /// Issue #5342: an empty PrSet is refused with a clear error instead of
    /// silently spawning a `/loom:sweep --prs` with nothing to sweep.
    #[test]
    fn pr_set_dispatch_rejects_empty_set() {
        let dir = tempdir().unwrap();
        let (mut registry, _record_log) = fixture_registry(dir.path());

        let outcome = registry.dispatch(&SweepKind::PrSet(vec![]), None, None, None, None);
        let err = outcome.unwrap_err().to_string();
        assert!(err.contains("empty PrSet"), "expected an empty-PrSet refusal; got: {err}");
    }

    /// The delay curve doubles per consecutive failure and clamps at `max`;
    /// a zero streak (and a zero base) is always no delay.
    #[test]
    fn backoff_delay_doubles_then_clamps_at_max() {
        let base = Duration::from_secs(60);
        let max = Duration::from_secs(900);
        assert_eq!(backoff_delay(0, base, max), Duration::ZERO, "no failures ⇒ no delay");
        assert_eq!(backoff_delay(1, base, max), Duration::from_secs(60));
        assert_eq!(backoff_delay(2, base, max), Duration::from_secs(120));
        assert_eq!(backoff_delay(3, base, max), Duration::from_secs(240));
        assert_eq!(backoff_delay(4, base, max), Duration::from_secs(480));
        assert_eq!(backoff_delay(5, base, max), Duration::from_secs(900), "clamped");
        assert_eq!(backoff_delay(50, base, max), max, "a long streak saturates, never overflows");
        assert_eq!(backoff_delay(3, Duration::ZERO, max), Duration::ZERO, "zero base disables");
    }

    /// AC: a minimum interval is enforced between same-issue dispatch attempts
    /// after a failed dispatch. The refusal carries the typed
    /// [`DispatchBackoffError`] (so the work finder can attribute it) and clears
    /// the moment the window is released.
    #[test]
    fn dispatch_refused_while_backoff_window_is_live() {
        let dir = tempdir().unwrap();
        let mut registry = backoff_registry(dir.path(), 60, 900);

        registry.record_dispatch_failure(4485);
        assert_eq!(registry.dispatch_failure_count(4485), 1);

        let err = registry
            .dispatch(&SweepKind::Issue(4485), None, None, None, None)
            .expect_err("a live backoff window must refuse dispatch");
        let typed = err
            .downcast_ref::<DispatchBackoffError>()
            .expect("refusal must carry the typed DispatchBackoffError");
        assert_eq!(typed.issue, 4485);
        assert_eq!(typed.consecutive, 1);
        assert!(typed.retry_after_secs > 0 && typed.retry_after_secs <= 60);

        // The refusal happens BEFORE the claim lock and the label flip, so
        // nothing was claimed and nothing was spawned.
        assert!(
            !registry.config.locks_dir().join("issue-4485").exists(),
            "a backoff refusal must not acquire the claim lock"
        );
        assert!(registry.entries.is_empty(), "a backoff refusal must not register a sweep");

        // Releasing the window (progress, or the operator's quarantine clear)
        // makes the issue immediately eligible again — the breaker never wedges.
        assert!(registry.clear_dispatch_backoff(4485));
        let outcome = registry
            .dispatch(&SweepKind::Issue(4485), None, None, None, None)
            .expect("dispatch proceeds once the window is cleared");
        assert!(outcome.was_new);
    }

    /// A disabled backoff is byte-for-byte the pre-#4485 path: no window is ever
    /// armed and dispatch is never refused.
    #[test]
    fn disabled_backoff_never_refuses_dispatch() {
        let dir = tempdir().unwrap();
        let (mut registry, _log) = fixture_registry(dir.path());
        registry.set_dispatch_backoff_config(DispatchBackoffConfig {
            enabled: false,
            ..DispatchBackoffConfig::default()
        });

        registry.record_dispatch_failure(77);
        assert_eq!(registry.dispatch_failure_count(77), 0, "disabled ⇒ nothing recorded");
        assert!(registry
            .dispatch_backoff_remaining(77, Utc::now())
            .is_none());
        assert!(registry
            .dispatch(&SweepKind::Issue(77), None, None, None, None)
            .is_ok());
    }

    /// Regression for the #4485 incident shape: an **account-exhaustion**
    /// insta-crash is deliberately NOT charged to the issue's quarantine tally
    /// (#4122), so three in a row never quarantine — yet before this change the
    /// work finder re-dispatched the issue on the very next tick, flapping
    /// `loom:issue`/`loom:building` indefinitely. The dispatch backoff must
    /// count those deaths and refuse the re-dispatch.
    #[test]
    fn exhaustion_insta_crashes_arm_backoff_even_though_quarantine_is_carved_out() {
        let dir = tempdir().unwrap();
        let mut registry = backoff_registry(dir.path(), 60, 900);
        seed_token_pool(dir.path(), "agent-9");

        for seq in 0..3 {
            insert_dead_running_with_log(
                &mut registry,
                4398,
                seq,
                "agent-9",
                "loom-daemon dispatch: start\nClaude: hit your weekly limit\n",
            );
            registry.reap_once();
        }

        // #4122's carve-out is intact — the issue was never blamed.
        assert_eq!(registry.insta_crash_count(4398), 0, "#4122 carve-out preserved");
        assert!(!registry.is_quarantined(4398), "#4122: exhaustion never quarantines the issue");

        // …but the retry cadence is now bounded regardless of blame.
        assert_eq!(registry.dispatch_failure_count(4398), 3);
        assert!(registry
            .dispatch_backoff_remaining(4398, Utc::now())
            .is_some());
        assert!(registry.dispatch_backoff_issues(Utc::now()).contains(&4398));
        let err = registry
            .dispatch(&SweepKind::Issue(4398), None, None, None, None)
            .expect_err("the re-dispatch that used to flap the label must be refused");
        assert!(err.downcast_ref::<DispatchBackoffError>().is_some(), "got: {err}");
    }

    /// AC (#4689): a child that exits immediately with the token-selection
    /// preflight failure must surface as a hard `Err` from `dispatch()` —
    /// never a `DispatchOutcome` (which `mcp__loom__dispatch_sweep` would
    /// render as `Success`, `Token: unknown`) — AND the claim taken before
    /// the child was spawned (the `loom:issue` -> `loom:building` label flip,
    /// the claim lock) must be fully reverted, so the issue is exactly as
    /// dispatchable as before this call.
    #[test]
    #[serial]
    fn dispatch_fails_fast_and_reverts_claim_on_immediate_token_selection_failure() {
        let tmp = tempdir().unwrap();
        let ws = tmp.path();
        std::env::remove_var("LOOM_REPO");
        let (mut reg, gh_log) = token_selection_failure_registry(ws);

        let err = reg
            .dispatch(&SweepKind::Issue(4689), None, None, None, None)
            .expect_err("an immediate token-selection death must fail dispatch, not Ok");
        assert!(
            err.to_string().contains("token selection failed"),
            "error must name the real cause, not a generic spawn failure; got: {err}"
        );

        let calls = std::fs::read_to_string(&gh_log).unwrap_or_default();
        assert!(
            calls.contains("issue edit 4689 --remove-label loom:issue --add-label loom:building"),
            "the claim WAS taken (label flip happened before the spawn) — got: {calls:?}"
        );
        assert!(
            calls.contains("issue edit 4689 --remove-label loom:building --add-label loom:issue"),
            "…and must be REVERTED on the synchronous failure path — got: {calls:?}"
        );

        // No phantom `Running` entry, no leaked lock, no retained child handle.
        assert!(
            running_issue_sweep_id(&reg, 4689).is_none(),
            "a failed dispatch must not leave a Running entry behind"
        );
        assert!(
            !ws.join(".loom/locks/issues/4689").exists(),
            "the claim lock must be released on the synchronous failure path"
        );
        assert!(reg.children.is_empty(), "no child handle should be retained for a dead child");
    }

    // ------------------------------------------------------------------------
    // Cross-issue empty-pool dispatch brake (Issue #6614)
    // ------------------------------------------------------------------------

    /// AC (#6614): N consecutive token-selection deaths across **different**
    /// issues pause dispatch fleet-wide with one loud signal — a state
    /// distinct from, and additional to, the per-issue #4485 backoff each of
    /// those dispatches also arms.
    ///
    /// End-to-end through the real `dispatch()` path (fake `gh` + a fake
    /// `spawn-claude.sh` that reproduces the exit-78 token-selection death),
    /// because the gap this closes is precisely that the synchronous #4689
    /// branch returns before any entry exists for the reaper to classify —
    /// a test that drove `reap_once` instead would pass against the broken
    /// pre-#6614 code.
    #[test]
    #[serial]
    fn distinct_issue_token_selection_deaths_pause_dispatch_beyond_per_issue_backoff() {
        let tmp = tempdir().unwrap();
        let ws = tmp.path();
        std::env::remove_var("LOOM_REPO");
        let (mut reg, _gh_log) = token_selection_failure_registry(ws);

        // Two different issues die at token selection. Each arms its OWN
        // per-issue backoff — but two is below the distinct-issue threshold,
        // so the fleet keeps dispatching (the "don't over-trigger" half).
        for issue in [66141_u32, 66142] {
            let err = reg
                .dispatch(&SweepKind::Issue(issue), None, None, None, None)
                .expect_err("an immediate token-selection death must fail dispatch");
            assert!(
                err.downcast_ref::<TokenSelectionDispatchError>().is_some(),
                "the failure must be the typed empty-pool error, not a stringly-typed one; \
                 got: {err}"
            );
            assert_eq!(
                reg.dispatch_failure_count(issue),
                1,
                "issue #{issue}'s own #4485 backoff must be armed by the synchronous path too"
            );
        }
        assert_eq!(reg.token_selection_failure_count(Utc::now()), 2);
        assert!(
            !reg.empty_pool_breaker_tripped(Utc::now()),
            "two distinct issues is below the threshold — the fleet must keep dispatching"
        );
        assert!(
            !reg.preflight_advisory().0,
            "no fleet-wide pause before the threshold is crossed"
        );

        // The third DIFFERENT issue crosses the threshold.
        let err = reg
            .dispatch(&SweepKind::Issue(66143), None, None, None, None)
            .expect_err("an immediate token-selection death must fail dispatch");
        assert!(err.downcast_ref::<TokenSelectionDispatchError>().is_some(), "got: {err}");

        assert_eq!(reg.token_selection_failure_count(Utc::now()), 3);
        assert!(reg.empty_pool_breaker_tripped(Utc::now()));

        // The pause is a WORKSPACE-level state — not merely three per-issue
        // backoffs — and it is the one the work-finder's #5030 hold reads.
        let (tripped, message) = reg.preflight_advisory();
        assert!(tripped, "the fleet-wide advisory must be tripped");
        let message = message.expect("a tripped advisory carries an operator-facing message");
        assert!(
            message.contains("token selection") && message.contains(".bad_tokens"),
            "the signal must name the empty-pool cause and the remedy; got: {message}"
        );
        assert_eq!(
            reg.preflight_dispatch_gate(Utc::now()),
            crate::sweep_registry::PreflightDispatchGate::Held,
            "new dispatch to this workspace must be HELD, not merely per-issue backed off"
        );
        // …and it is genuinely distinct from the per-issue backoff: a FOURTH,
        // never-seen issue has no backoff of its own, yet is covered by the
        // hold above.
        assert_eq!(reg.dispatch_failure_count(66144), 0);
        assert!(!reg.dispatch_backoff_issues(Utc::now()).contains(&66144));
    }

    /// AC (#6614), the anti-over-trigger case: ONE issue failing repeatedly —
    /// the shape its own #4485 backoff already governs — must never pause the
    /// fleet, however many times it fails. The counter is distinct issues, not
    /// failures.
    #[test]
    fn one_issue_failing_repeatedly_never_trips_the_fleet_pause() {
        let dir = tempdir().unwrap();
        let mut registry = backoff_registry(dir.path(), 60, 900);

        for _ in 0..10 {
            registry.record_token_selection_failure(6614);
        }

        assert_eq!(
            registry.token_selection_failure_count(Utc::now()),
            1,
            "ten failures on ONE issue is still one distinct issue"
        );
        assert!(!registry.empty_pool_breaker_tripped(Utc::now()));
        assert!(
            !registry.preflight_advisory().0,
            "one struggling issue must not pause the whole fleet (#6614 over-trigger guard)"
        );
    }

    /// AC (#6614): a dispatch that gets past token selection clears the brake
    /// and releases the hold — no operator action, mirroring how the #5030
    /// half-open probe already clears the pre-flight advisory.
    #[test]
    fn a_dispatch_past_token_selection_clears_the_pause() {
        let dir = tempdir().unwrap();
        let mut registry = backoff_registry(dir.path(), 60, 900);

        for issue in [1_u32, 2, 3] {
            registry.record_token_selection_failure(issue);
        }
        assert!(registry.empty_pool_breaker_tripped(Utc::now()));
        assert!(registry.preflight_advisory().0);

        assert!(registry.clear_token_selection_failures());

        assert_eq!(registry.token_selection_failure_count(Utc::now()), 0);
        assert!(!registry.empty_pool_breaker_tripped(Utc::now()));
        assert!(!registry.preflight_advisory().0, "the hold must release on the first success");
        assert_eq!(
            registry.preflight_dispatch_gate(Utc::now()),
            crate::sweep_registry::PreflightDispatchGate::Open
        );
        assert!(
            !registry.clear_token_selection_failures(),
            "clearing an already-empty brake is a no-op (no advisory churn)"
        );
    }

    /// AC (#6614): failures spread further apart than the trailing window are
    /// not a systemic fault — they must age out instead of accreting toward a
    /// pause over hours. Stale entries are injected directly rather than slept
    /// for, so the test is deterministic.
    #[test]
    fn token_selection_failures_older_than_the_window_stop_counting() {
        let dir = tempdir().unwrap();
        let mut registry = backoff_registry(dir.path(), 60, 900);

        let stale = Utc::now()
            - chrono::Duration::seconds(DEFAULT_EMPTY_POOL_BREAKER_WINDOW_SECS)
            - chrono::Duration::seconds(60);
        registry.token_selection_failures.insert(11, stale);
        registry.token_selection_failures.insert(22, stale);

        assert_eq!(
            registry.token_selection_failure_count(Utc::now()),
            0,
            "out-of-window failures do not count"
        );

        // A fresh third failure must NOT trip on the back of two stale ones —
        // and must physically prune them.
        registry.record_token_selection_failure(33);
        assert_eq!(registry.token_selection_failure_count(Utc::now()), 1);
        assert!(!registry.empty_pool_breaker_tripped(Utc::now()));
        assert_eq!(registry.token_selection_failures.len(), 1, "stale entries are pruned");
    }

    /// AC (#5236): when `spawn_child` itself returns `Err` — e.g. a
    /// registered workspace whose `spawn_bin` resolves to a nonexistent
    /// file, so `Command::spawn()` fails at the OS level before any child
    /// ever exists — `dispatch_inner` must unwind the same side effects the
    /// #4689 branch above reverts: the claim lock, the `loom:building`
    /// label flip, and (implicitly, since nothing was ever advertised
    /// without a peer-claim publisher configured in this fixture) the
    /// peer-claim advertisement. Without this, the leaked lock's
    /// `owner_pid` is this daemon's own (permanently alive, from its own
    /// point of view) pid, which the #4556 live-claim guard misreads as a
    /// confirmed-live claim on every retry — the exact wedge this issue
    /// reports.
    #[test]
    #[serial]
    fn dispatch_reverts_claim_lock_and_label_when_spawn_child_itself_fails() {
        let tmp = tempdir().unwrap();
        let ws = tmp.path();
        std::env::remove_var("LOOM_REPO");
        let (mut reg, gh_log) = spawn_bin_missing_registry(ws);

        let err = reg
            .dispatch(&SweepKind::Issue(5236), None, None, None, None)
            .expect_err("a spawn_child Err must fail dispatch, not Ok");
        assert!(err.to_string().contains("failed to spawn sweep child"), "got: {err}");

        let calls = std::fs::read_to_string(&gh_log).unwrap_or_default();
        assert!(
            calls.contains("issue edit 5236 --remove-label loom:issue --add-label loom:building"),
            "the claim WAS taken (label flip happens before spawn_child) — got: {calls:?}"
        );
        assert!(
            calls.contains("issue edit 5236 --remove-label loom:building --add-label loom:issue"),
            "…and must be REVERTED when spawn_child fails — got: {calls:?}"
        );

        assert!(
            running_issue_sweep_id(&reg, 5236).is_none(),
            "a failed dispatch must not leave a Running entry behind"
        );
        assert!(
            !ws.join(".loom/locks/issue-5236").exists(),
            "the claim lock must be released when spawn_child fails, not leaked (#5236)"
        );
        assert!(
            reg.children.is_empty(),
            "no child handle exists — spawn_child never got that far"
        );
    }

    /// The full repro from this issue's Test Plan: dispatch twice against a
    /// workspace whose `spawn_bin` never resolves to anything runnable. The
    /// first dispatch fails and — per the AC above — cleans up fully; the
    /// second dispatch must reach the SAME `spawn_child` failure again, NOT
    /// be refused by the #4556 live-claim guard reading a leaked lock whose
    /// `owner_pid` is this daemon's own (still-alive) pid as a confirmed-live
    /// claim.
    #[test]
    #[serial]
    fn second_dispatch_after_spawn_child_failure_is_not_refused_by_the_live_claim_guard() {
        let tmp = tempdir().unwrap();
        let ws = tmp.path();
        std::env::remove_var("LOOM_REPO");
        let (mut reg, _gh_log) = spawn_bin_missing_registry(ws);

        let first = reg
            .dispatch(&SweepKind::Issue(5236), None, None, None, None)
            .expect_err("first dispatch must fail — spawn_bin does not exist");
        assert!(
            first.downcast_ref::<LiveClaimDispatchError>().is_none(),
            "the FIRST dispatch must fail on the spawn error itself, not a live-claim refusal \
             (there is nothing to collide with yet) — got: {first}"
        );

        let second = reg
            .dispatch(&SweepKind::Issue(5236), None, None, None, None)
            .expect_err("second dispatch must also fail — spawn_bin still does not exist");
        assert!(
            second.downcast_ref::<LiveClaimDispatchError>().is_none(),
            "the SECOND dispatch must reach the same spawn_child failure again, not be wedged \
             behind a #4556 live-claim refusal from the first attempt's leaked lock — got: {second}"
        );
        assert!(second.to_string().contains("failed to spawn sweep child"), "got: {second}");
    }

    /// Issue #6615: pins the exact crash window this issue reports — a
    /// daemon that dies AFTER `begin_issue_dispatch` completes (claim lock
    /// taken, `loom:building` label flipped, child spawned) but BEFORE
    /// `finish_issue_dispatch` runs (which is where the sweep journal entry
    /// is written, `dispatch.rs`'s `sweep_journal::record_sweep_at` call).
    /// Reproduced deterministically by simply not calling
    /// `finish_issue_dispatch` — no threads, no timing dependence — exactly
    /// like `same_key_retry_during_the_unlocked_poll_window_is_refused_not_double_spawned`
    /// reproduces the same window for its own (different) assertion.
    ///
    /// This is a CHARACTERIZATION test: it pins that the gap exists (the
    /// label is flipped, but no journal entry and no registered entry exist
    /// yet) so a future change to `begin_issue_dispatch`/`finish_issue_dispatch`
    /// cannot silently close or reopen this window without a test noticing.
    /// The actual recovery is `claim_reconciliation`'s startup-only immediate
    /// reclaim (`ReclaimReason::NoRecordAtStartup`) — this test does not
    /// duplicate that coverage, it only pins the shape of evidence the crash
    /// leaves behind for that pass to act on.
    #[test]
    #[serial]
    fn crash_between_label_flip_and_journal_write_leaves_label_flipped_with_no_journal_entry() {
        let dir = tempdir().unwrap();
        let ws = dir.path();
        std::env::remove_var("LOOM_REPO");
        let (mut registry, gh_log) = crash_before_finish_registry(ws);

        let begin = registry
            .begin_issue_dispatch(&SweepKind::Issue(6615), None, None, None, None, None)
            .expect("begin_issue_dispatch must succeed — claim, flip, and spawn all work");
        let prepared = match begin {
            BeginIssueDispatch::Spawned(prepared) => prepared,
            BeginIssueDispatch::Done(result) => panic!("expected Spawned, got Done({result:?})"),
        };

        // The label WAS flipped — this is the pre-spawn claim the issue
        // describes, and it is durable forge state that survives the crash
        // this test simulates by stopping here.
        let calls = std::fs::read_to_string(&gh_log).unwrap_or_default();
        assert!(
            calls.contains("issue edit 6615 --remove-label loom:issue --add-label loom:building"),
            "the claim must be taken (label flip happens before spawn) — got: {calls:?}"
        );
        assert!(
            !calls.contains("--remove-label loom:building --add-label loom:issue"),
            "nothing has reverted the claim — the crash is simulated by simply not finishing, \
             not by any Err this registry ever saw — got: {calls:?}"
        );

        // NO journal entry exists yet — `finish_issue_dispatch` (never
        // called here) is the only place `sweep_journal::record_sweep_at`
        // runs.
        let journal_path = registry.config().resolve_journal_path().unwrap();
        let journal = sweep_journal::load(&journal_path);
        let repo = registry.config().workspace_root.display().to_string();
        assert!(
            sweep_journal::find(&journal, &repo, 6615).is_none(),
            "no journal entry must exist before finish_issue_dispatch runs — this IS the #6615 \
             gap claim_reconciliation's startup pass exists to close"
        );

        // NO in-memory Running entry either — `finish_issue_dispatch` is
        // also where `self.entries.insert` happens.
        assert!(
            running_issue_sweep_id(&registry, 6615).is_none(),
            "no Running entry must exist before finish_issue_dispatch runs"
        );

        // Clean up the still-sleeping child so the test process does not
        // leak it — `prepared.child` is a live std::process::Child this test
        // owns and never handed to the registry (finish_issue_dispatch never
        // ran), so it must be reaped directly rather than via the registry.
        let mut prepared = prepared;
        let _ = prepared.child.kill();
        let _ = prepared.child.wait();
    }

    /// Edge case (#4689): a child that DID log a token selection before
    /// exiting with `EX_CONFIG`/78 (should not happen from the real
    /// `spawn-claude.sh` — its token-selection step is the very first thing
    /// it does, before any CLI/runtime work — but is worth pinning
    /// explicitly) must NOT be misclassified as the synchronous
    /// "token-selection failed" fast path. `spawn_child`'s guard is keyed on
    /// `token_name == UNKNOWN_TOKEN_NAME`, so once a real account name was
    /// captured, `immediate_preflight_death` is unconditionally `None` and
    /// `dispatch()` returns its normal `Ok(DispatchOutcome)` carrying that
    /// token name — the exit-78 death (for whatever *other* reason) is left
    /// to flow through the existing async `reap_once`-driven classification
    /// exactly as it does today, unchanged by this issue.
    #[test]
    #[serial]
    fn dispatch_succeeds_when_token_was_logged_before_an_exit_78_death() {
        let dir = tempdir().unwrap();
        // Logs a real selection first (so `token_name` is captured), THEN
        // dies with the same exit code and log prose the genuine
        // token-selection preflight failure uses — deliberately an
        // unrealistic combination, to prove the guard reads `token_name`,
        // not the exit code alone.
        let script = "#!/usr/bin/env bash\n\
set -uo pipefail\n\
echo \"spawn-claude: using OAuth account 'agent3-2amlogic' (mode=random)\" >&2\n\
echo 'ERROR Token selection failed:' >&2\n\
exit 78\n";
        let mut registry = lifecycle_registry(dir.path(), script);

        let outcome = registry
            .dispatch(&SweepKind::Issue(46_890), None, None, None, None)
            .expect(
                "a child that logged a real token selection must dispatch Ok, even if it then \
                 exits 78 for an unrelated reason — only the no-selection-logged shape is the \
                 #4689 fast-fail case",
            );
        assert_eq!(
            outcome.token_name, "agent3-2amlogic",
            "the captured selection must be preserved, not overwritten by the exit-78 guard"
        );
    }

    /// The same property for the OTHER quarantine carve-out: a claude-wrapper
    /// pre-flight death (#4386) leaves the insta-crash tally untouched but must
    /// still bound its own re-dispatch cadence.
    #[test]
    fn preflight_death_arms_backoff_even_though_quarantine_is_carved_out() {
        let dir = tempdir().unwrap();
        let mut registry = backoff_registry(dir.path(), 60, 900);

        // No `# CLAUDE_CLI_START` in the tail ⇒ classified as a pre-flight death.
        insert_dead_running_with_log(
            &mut registry,
            4399,
            0,
            "agent-9",
            "==== loom-daemon dispatch: sweep-issue-4399-0 ====\nspawn-claude: preflight failed\n",
        );
        registry.reap_once();

        assert_eq!(registry.insta_crash_count(4399), 0, "#4386 carve-out preserved");
        assert_eq!(registry.dispatch_failure_count(4399), 1, "backoff still counts it");
        assert!(registry
            .dispatch_backoff_remaining(4399, Utc::now())
            .is_some());
    }

    // ------------------------------------------------------------------------
    // Durable terminal-outcome journal (Issue #4644)
    // ------------------------------------------------------------------------

    /// AC: a child that dies at `spawn-claude.sh`'s token-selection step
    /// (exit 78, `EX_CONFIG`) is (a) classified with the specific
    /// `preflight-token-selection-failed` death_class rather than the generic
    /// `preflight-no-cli-start` fallback, (b) still arms the #4485 dispatch
    /// backoff exactly like any other pre-flight death (confirming the
    /// existing backoff machinery already covers this shape — no extension
    /// needed), and (c) is written to the durable outcomes journal with the
    /// exit code, death_class, token_name, and duration a post-hoc reader
    /// needs, all without reading log prose.
    #[test]
    fn exit_78_token_selection_death_is_classified_backed_off_and_journaled() {
        let dir = tempdir().unwrap();
        let mut registry = backoff_registry(dir.path(), 60, 900);

        // The exact prose `defaults/scripts/spawn-claude.sh` logs immediately
        // before `exit 78` when `loom-daemon tokens select` itself fails
        // (typically because every account is exhausted/blocked) — no
        // `# CLAUDE_CLI_START` anywhere in the tail, because the CLI was
        // never reached.
        insert_dead_running_with_log(
            &mut registry,
            4644,
            0,
            "agent-9",
            "==== loom-daemon dispatch: sweep-issue-4644-0 ====\n\
             [2026-07-30T00:00:00Z] ERROR Token selection failed:\n\
             [2026-07-30T00:00:00Z] ERROR Run 'loom-daemon tokens bootstrap' to populate \
             <repo>/.loom/tokens/,\n",
        );
        registry.reap_once();

        // (a) Specific death_class, not the generic fallback.
        let path = registry.config().resolve_outcomes_journal_path();
        let records = sweep_outcomes::read_all(&path);
        let record = records
            .iter()
            .find(|r| r.issue == 4644)
            .expect("terminal outcome must be journaled");
        assert_eq!(
            record.death_class.as_deref(),
            Some("preflight-token-selection-failed"),
            "exit-78 token-selection death must carry the specific #4644 death_class"
        );
        assert_eq!(record.token_name, "agent-9");
        assert!(record.duration_sec >= 0);
        assert_eq!(record.sweep_id, "sweep-issue-4644-0");

        // (b) #4485 backoff already covers this shape — confirmed, not
        // re-implemented.
        assert_eq!(registry.insta_crash_count(4644), 0, "#4386 carve-out preserved");
        assert_eq!(registry.dispatch_failure_count(4644), 1, "backoff still counts it");
        assert!(registry
            .dispatch_backoff_remaining(4644, Utc::now())
            .is_some());
    }

    /// Issue #5697 (daemon-path half of #5687): a sweep killed by per-model
    /// credit exhaustion is journaled with a `crash_classification` distinct
    /// from a plan/quota (`TOKEN_EXHAUSTED`-family) exhaustion — the AC this
    /// issue exists to satisfy — while `death_class` (the UNRELATED
    /// pre-flight-workspace-tripwire classifier) correctly stays `None`:
    /// account exhaustion is deliberately excluded from that classifier
    /// (`classify_preflight_outcome`'s `Unknown` arm), so this exercises the
    /// two classifiers' independence, not just the new label.
    #[test]
    fn credit_exhaustion_death_is_journaled_with_a_distinct_crash_classification() {
        let dir = tempdir().unwrap();
        let mut registry = backoff_registry(dir.path(), 60, 900);

        insert_dead_running_with_log(
            &mut registry,
            5697,
            0,
            "agent-1",
            "==== loom-daemon dispatch: sweep-issue-5697-0 ====\n\
             # CLAUDE_CLI_START\n\
             Claude: You're out of usage credits for this model.\n",
        );
        registry.reap_once();

        let path = registry.config().resolve_outcomes_journal_path();
        let records = sweep_outcomes::read_all(&path);
        let record = records
            .iter()
            .find(|r| r.issue == 5697)
            .expect("terminal outcome must be journaled");
        assert_eq!(
            record.crash_classification.as_deref(),
            Some("account-exhausted:model-credits-exhausted"),
            "credit exhaustion must carry its own crash_classification, distinct from \
             account-exhausted:rate-limited"
        );
        assert_eq!(
            record.death_class, None,
            "account exhaustion is neutral to the UNRELATED pre-flight-death classifier"
        );
    }

    /// A cold streak restarts at 1: a failure whose predecessor is older than
    /// `max` is a fresh incident, not the continuation of an old one — so an
    /// issue that fails once in a blue moon never accretes a long backoff.
    #[test]
    fn stale_failure_streak_restarts_from_one() {
        let dir = tempdir().unwrap();
        let mut registry = backoff_registry(dir.path(), 60, 120);

        registry.record_dispatch_failure(31);
        registry.record_dispatch_failure(31);
        assert_eq!(registry.dispatch_failure_count(31), 2);

        // Age the recorded failure well past `max` (120s) and record again.
        let stale = Utc::now() - chrono::Duration::seconds(600);
        if let Some(state) = registry.dispatch_backoff.get_mut(&31) {
            state.last_failure_at = stale;
            state.until = stale;
        }
        registry.record_dispatch_failure(31);
        assert_eq!(registry.dispatch_failure_count(31), 1, "cold streak restarts");
    }

    /// An elapsed window reads as "no backoff" without any explicit expiry pass —
    /// the check is purely `until > now`, so a stale record can never hold an
    /// issue back (and a daemon restart clears the map entirely).
    #[test]
    fn elapsed_backoff_window_stops_refusing() {
        let dir = tempdir().unwrap();
        let mut registry = backoff_registry(dir.path(), 60, 900);
        registry.record_dispatch_failure(32);
        if let Some(state) = registry.dispatch_backoff.get_mut(&32) {
            state.until = Utc::now() - chrono::Duration::seconds(1);
        }
        assert!(registry
            .dispatch_backoff_remaining(32, Utc::now())
            .is_none());
        assert!(registry.dispatch_backoff_issues(Utc::now()).is_empty());
        assert!(registry
            .dispatch(&SweepKind::Issue(32), None, None, None, None)
            .is_ok());
    }

    /// A run that made real progress clears the window immediately, so a
    /// recovering issue is never held back by an earlier failure.
    #[test]
    fn checkpoint_progress_clears_the_backoff_window() {
        let dir = tempdir().unwrap();
        let mut registry = backoff_registry(dir.path(), 60, 900);
        registry.record_dispatch_failure(33);
        assert_eq!(registry.dispatch_failure_count(33), 1);

        // A dead run whose checkpoint was (re)written by THIS run = progress.
        let started_at = Utc::now() - chrono::Duration::seconds(5);
        insert_dead_running_at(&mut registry, 33, 9, started_at);
        let checkpoint_dir = registry.config.checkpoint_dir();
        std::fs::create_dir_all(&checkpoint_dir).unwrap();
        std::fs::write(checkpoint_dir.join("issue-33.json"), r#"{"phase":"builder-done"}"#)
            .unwrap();
        registry.reap_once();

        assert_eq!(registry.dispatch_failure_count(33), 0, "progress clears the streak");
        assert!(registry
            .dispatch_backoff_remaining(33, Utc::now())
            .is_none());
    }

    /// A **slow** checkpoint-less death does NOT arm the backoff: that shape is
    /// the mid-build-death (#3895) / review-stall (#3910) watchdogs' remit, each
    /// already bounded to one retry per issue, and arming a window there would
    /// risk a refusal burning that single allowed attempt.
    #[test]
    fn slow_checkpoint_less_death_does_not_arm_the_backoff() {
        let dir = tempdir().unwrap();
        let mut registry = backoff_registry(dir.path(), 60, 900);
        // `insta_crash_secs` defaults to 60s: start the run well outside it.
        let started_at = Utc::now() - chrono::Duration::seconds(600);
        insert_dead_running_at(&mut registry, 36, 0, started_at);
        registry.reap_once();

        assert_eq!(
            registry.dispatch_failure_count(36),
            0,
            "a slow death is the watchdogs' remit, not the flap breaker's"
        );
        assert!(registry
            .dispatch_backoff_remaining(36, Utc::now())
            .is_none());
    }

    /// Flap detection: this registry's own label writes are counted per issue and
    /// warn once the trailing window holds `DEFAULT_FLAP_THRESHOLD` of them. Below
    /// the threshold nothing is flagged (a healthy dispatch writes exactly 2).
    #[test]
    fn label_flip_flap_detector_flags_only_above_threshold() {
        let dir = tempdir().unwrap();
        let (mut registry, _log) = fixture_registry(dir.path());

        for _ in 0..(DEFAULT_FLAP_THRESHOLD - 1) {
            registry.note_label_flip(4398);
        }
        assert!(
            !registry.flap_warned_at.contains_key(&4398),
            "below threshold: a normal dispatch/complete rhythm never warns"
        );

        registry.note_label_flip(4398);
        assert!(
            registry.flap_warned_at.contains_key(&4398),
            "at threshold: the flap is surfaced in the daemon log"
        );
        let first_warn = registry.flap_warned_at[&4398];

        // A sustained flap warns at most once per window (no log spam).
        registry.note_label_flip(4398);
        assert_eq!(registry.flap_warned_at[&4398], first_warn);

        // Flips outside the trailing window are pruned, so an issue that flipped
        // long ago does not carry stale credit toward the threshold.
        let stale = Utc::now() - chrono::Duration::seconds(DEFAULT_FLAP_WINDOW_SECS + 60);
        registry
            .label_flip_log
            .insert(1234, std::iter::repeat_n(stale, DEFAULT_FLAP_THRESHOLD * 2).collect());
        registry.note_label_flip(1234);
        assert!(!registry.flap_warned_at.contains_key(&1234), "stale flips are pruned");
    }

    /// Config resolution honors precedence env > config > default (#4485), and a
    /// `maxSecs` below `baseSecs` is clamped up rather than inverting the curve.
    #[test]
    #[serial]
    fn resolve_dispatch_backoff_config_env_overrides() {
        let dir = tempdir().unwrap();
        for var in [
            DISPATCH_BACKOFF_ENABLE_ENV,
            DISPATCH_BACKOFF_BASE_ENV,
            DISPATCH_BACKOFF_MAX_ENV,
        ] {
            std::env::remove_var(var);
        }

        let base = resolve_dispatch_backoff_config(dir.path());
        assert!(base.enabled, "defaults ON — it is a safety backstop");
        assert_eq!(base.base, Duration::from_secs(DEFAULT_DISPATCH_BACKOFF_BASE_SECS));
        assert_eq!(base.max, Duration::from_secs(DEFAULT_DISPATCH_BACKOFF_MAX_SECS));

        std::env::set_var(DISPATCH_BACKOFF_ENABLE_ENV, "off");
        std::env::set_var(DISPATCH_BACKOFF_BASE_ENV, "30");
        std::env::set_var(DISPATCH_BACKOFF_MAX_ENV, "10");
        let resolved = resolve_dispatch_backoff_config(dir.path());
        for var in [
            DISPATCH_BACKOFF_ENABLE_ENV,
            DISPATCH_BACKOFF_BASE_ENV,
            DISPATCH_BACKOFF_MAX_ENV,
        ] {
            std::env::remove_var(var);
        }
        assert!(!resolved.enabled, "LOOM_DISPATCH_BACKOFF=off disables");
        assert_eq!(resolved.base, Duration::from_secs(30));
        assert_eq!(resolved.max, Duration::from_secs(30), "max clamped up to base");
    }

    /// Config-file parsing of `autonomous.workFinder.dispatchBackoff` (#4485),
    /// including the all-`None` (absent block) case that preserves env/default
    /// resolution for repos that never configure it.
    #[test]
    #[serial]
    fn read_dispatch_backoff_file_config_parses_block() {
        for var in [
            DISPATCH_BACKOFF_ENABLE_ENV,
            DISPATCH_BACKOFF_BASE_ENV,
            DISPATCH_BACKOFF_MAX_ENV,
        ] {
            std::env::remove_var(var);
        }

        let dir = tempdir().unwrap();
        let loom = dir.path().join(".loom");
        std::fs::create_dir_all(&loom).unwrap();
        std::fs::write(
            loom.join("config.json"),
            r#"{"autonomous":{"workFinder":{"dispatchBackoff":{"enabled":false,"baseSecs":15,"maxSecs":300}}}}"#,
        )
        .unwrap();

        let file = read_dispatch_backoff_file_config(dir.path());
        assert_eq!(file.enabled, Some(false));
        assert_eq!(file.base_secs, Some(15));
        assert_eq!(file.max_secs, Some(300));

        let resolved = resolve_dispatch_backoff_config(dir.path());
        assert!(!resolved.enabled);
        assert_eq!(resolved.base, Duration::from_secs(15));
        assert_eq!(resolved.max, Duration::from_secs(300));

        let empty = tempdir().unwrap();
        let absent = read_dispatch_backoff_file_config(empty.path());
        assert_eq!(absent, DispatchBackoffFileConfig::default());
    }

    /// The headline #4556 guard: a lock whose owner PID is **live** refuses a
    /// dispatch with the typed [`LiveClaimDispatchError`], before any lock or
    /// label write happens.
    #[test]
    fn dispatch_refuses_when_the_claim_lock_owner_is_live() {
        let dir = tempdir().unwrap();
        let (mut registry, _record_log) = fixture_registry(dir.path());
        let sweep = FakeSweep::spawn(4556);
        write_lock_owner(&registry, 4556, "sweep-issue-4556-live", sweep.pid());

        let err = registry
            .dispatch(&SweepKind::Issue(4556), None, None, None, None)
            .unwrap_err();
        let typed = err
            .downcast_ref::<LiveClaimDispatchError>()
            .expect("a live claim lock must surface the typed #4556 refusal");
        assert_eq!(typed.issue, 4556);
        assert!(matches!(typed.evidence, crate::live_claim::LiveClaimEvidence::ClaimLock { .. }));
        assert!(registry.entries.is_empty(), "a refused dispatch must not record an entry");
    }

    /// The gap #4463's ownership-checked *release* could not close: the lock is
    /// already gone (a watchdog / reaper released it on a false-dead verdict) but
    /// the sweep process is still alive. The machine-level journal survives that
    /// release, so the guard still refuses.
    #[test]
    fn dispatch_refuses_when_the_journal_records_a_live_sweep_and_the_lock_is_gone() {
        let dir = tempdir().unwrap();
        let (mut registry, _record_log) = fixture_registry(dir.path());
        assert!(
            !registry.config.locks_dir().join("issue-4557").exists(),
            "precondition: no lock — the released-lock state this guard covers"
        );
        let sweep = FakeSweep::spawn(4557);
        write_journal_entry(&registry, &dir.path().display().to_string(), 4557, sweep.pid());

        let err = registry
            .dispatch(&SweepKind::Issue(4557), None, None, None, None)
            .unwrap_err();
        assert!(matches!(
            err.downcast_ref::<LiveClaimDispatchError>()
                .map(|e| &e.evidence),
            Some(crate::live_claim::LiveClaimEvidence::Journal { .. })
        ));
    }

    /// A daemon rooted at `<repo>/.loom/worktrees/issue-N` — the stray
    /// debug-build instance that produced 3 of #4275's 7 dispatches — records
    /// its claims in the same machine-level journal under a *nested* repo path.
    /// The parent checkout's daemon must see that claim.
    #[test]
    fn dispatch_refuses_when_a_nested_worktree_daemon_holds_the_claim() {
        let dir = tempdir().unwrap();
        let (mut registry, _record_log) = fixture_registry(dir.path());
        let nested = dir
            .path()
            .join(".loom")
            .join("worktrees")
            .join("issue-4385")
            .display()
            .to_string();
        let sweep = FakeSweep::spawn(4558);
        write_journal_entry(&registry, &nested, 4558, sweep.pid());

        let err = registry
            .dispatch(&SweepKind::Issue(4558), None, None, None, None)
            .unwrap_err();
        assert!(
            err.downcast_ref::<LiveClaimDispatchError>().is_some(),
            "a nested-worktree daemon's live claim must refuse the parent's dispatch; got: {err}"
        );
    }

    /// Test Plan item 2 / the issue's headline acceptance criterion: **N**
    /// dispatch requests for one issue inside a bounded window produce exactly
    /// **one** live sweep. Here the live sweep is already running (its claim
    /// proven by the journal) and every one of the next five requests — the
    /// shape of #4275's storm, where the work-finder, the reconciler, and two
    /// watchdogs each fired — is refused without a lock, a label write, or an
    /// entry.
    #[test]
    fn n_dispatch_requests_for_a_live_issue_produce_zero_extra_sweeps() {
        let dir = tempdir().unwrap();
        let (mut registry, _record_log) = fixture_registry(dir.path());
        let sweep = FakeSweep::spawn(4559);
        write_journal_entry(&registry, &dir.path().display().to_string(), 4559, sweep.pid());

        for attempt in 1..=5 {
            let err = registry
                .dispatch(&SweepKind::Issue(4559), None, None, None, None)
                .unwrap_err();
            assert!(
                err.downcast_ref::<LiveClaimDispatchError>().is_some(),
                "attempt {attempt} must be refused by the live-claim guard; got: {err}"
            );
        }
        assert!(registry.entries.is_empty(), "no duplicate sweep may be recorded");
        assert!(
            !registry.config.locks_dir().join("issue-4559").exists(),
            "a refusal must cost no lock write"
        );
    }

    /// A dead recorded PID must NOT refuse — the guard fails open so a stale
    /// journal entry or an abandoned lock can never wedge an issue.
    #[test]
    fn dispatch_proceeds_when_every_recorded_claim_pid_is_dead() {
        let dir = tempdir().unwrap();
        let (mut registry, _record_log) = fixture_registry(dir.path());
        // PID 0 is always treated as dead by `is_pid_alive`.
        write_journal_entry(&registry, &dir.path().display().to_string(), 4560, 0);

        assert!(
            registry.live_claim_evidence(4560).is_none(),
            "a dead journal PID is not live-claim evidence"
        );
        let outcome = registry
            .dispatch(&SweepKind::Issue(4560), None, None, None, None)
            .expect("a dead recorded claim must not block a legitimate dispatch");
        assert!(outcome.was_new);
    }

    /// A journal claim for the SAME issue number in an UNRELATED repo must not
    /// refuse: issue numbers are per-repo, so a sibling checkout's #N is a
    /// different issue.
    #[test]
    fn dispatch_ignores_a_live_claim_from_an_unrelated_repo() {
        let dir = tempdir().unwrap();
        let (mut registry, _record_log) = fixture_registry(dir.path());
        let sweep = FakeSweep::spawn(4561);
        write_journal_entry(&registry, "/some/other/checkout", 4561, sweep.pid());

        assert!(registry.live_claim_evidence(4561).is_none());
        assert!(registry
            .dispatch(&SweepKind::Issue(4561), None, None, None, None)
            .is_ok());
    }

    /// Regression (Test Plan item 2, retained guard): two dispatch requests for
    /// the same issue racing in one tick produce exactly ONE sweep — the second
    /// `acquire_lock` collides on the atomic mkdir claim and is refused. Already
    /// passes today; kept so the cross-instance mutual-exclusion property does
    /// not silently regress.
    #[test]
    fn second_same_issue_dispatch_collides_on_lock() {
        let dir = tempdir().unwrap();
        let (registry, _record_log) = fixture_registry(dir.path());

        registry
            .acquire_lock(4468, "sweep-issue-4468-first")
            .unwrap();
        let err = registry
            .acquire_lock(4468, "sweep-issue-4468-second")
            .expect_err("a second same-issue claim must collide on the lock");
        assert!(
            err.to_string().contains("lock collision"),
            "the second dispatch must be refused with a lock collision; got: {err}"
        );
    }

    /// AC #3: assert that the spawned child receives a
    /// `CLAUDE_CODE_OAUTH_TOKEN` env var that came from `.loom/tokens/`.
    /// We achieve this with a fixture tokens dir and a fixture spawn-claude
    /// that selects from it. The real `spawn-claude.sh` would invoke the
    /// Python selector; here we substitute a thin shell that picks the
    /// first token file and exports it, so the test exercises the dispatch
    /// path end-to-end without depending on a working Python install.
    #[test]
    #[serial]
    fn dispatch_propagates_oauth_token_from_tokens_dir() {
        let dir = tempdir().unwrap();
        let workspace = dir.path();

        // Build a fixture tokens dir with one token.
        let tokens_dir = workspace.join(".loom").join("tokens");
        std::fs::create_dir_all(&tokens_dir).unwrap();
        let token_value = "sk-ant-oat01-fixture-token-value";
        let token_path = tokens_dir.join("agent-1.token");
        std::fs::write(&token_path, token_value).unwrap();
        let mut perms = std::fs::metadata(&token_path).unwrap().permissions();
        perms.set_mode(0o600);
        std::fs::set_permissions(&token_path, perms).unwrap();

        // Build a fake spawn-claude that selects the first token file and
        // records the exported CLAUDE_CODE_OAUTH_TOKEN. This is a stand-in
        // for the real wrapper's Python-backed selection — the assertion
        // is that the *registry's* dispatch path produces a child whose
        // OAuth token came from `.loom/tokens/`.
        let scripts_dir = workspace.join(".loom").join("scripts");
        std::fs::create_dir_all(&scripts_dir).unwrap();
        let fake_bin = scripts_dir.join("spawn-claude.sh");
        let record_log = workspace.join("oauth-record.log");
        let script = format!(
            r#"#!/usr/bin/env bash
set -euo pipefail
ws="${{LOOM_WORKSPACE:-{ws}}}"
tokens_dir="$ws/.loom/tokens"
token_file="$(ls "$tokens_dir"/*.token 2>/dev/null | head -n1)"
if [ -z "$token_file" ]; then
  echo "no token files in $tokens_dir" >&2
  exit 78
fi
export CLAUDE_CODE_OAUTH_TOKEN="$(cat "$token_file")"
{{
  echo "TOKEN_SOURCE=$token_file"
  echo "CLAUDE_CODE_OAUTH_TOKEN=$CLAUDE_CODE_OAUTH_TOKEN"
  echo "argv: $*"
}} >> "{rec}"
exit 0
"#,
            ws = workspace.display(),
            rec = record_log.display()
        );
        std::fs::write(&fake_bin, script).unwrap();
        let mut perms = std::fs::metadata(&fake_bin).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&fake_bin, perms).unwrap();

        let mut config = SweepRegistryConfig::new(workspace.to_path_buf());
        config.spawn_bin = Some(fake_bin);
        config.skip_label_flip = true;
        config.journal_path = Some(workspace.join("test-sweeps-journal.json"));
        let mut registry = SweepRegistry::new(config);

        let outcome = registry
            .dispatch(&SweepKind::Issue(123), None, None, None, None)
            .unwrap();
        assert!(outcome.was_new);

        let needle = format!("CLAUDE_CODE_OAUTH_TOKEN={token_value}");
        let recorded = assert_child_wrote(&record_log, &needle);
        assert!(
            recorded.contains(".loom/tokens/agent-1.token"),
            "expected TOKEN_SOURCE to point at .loom/tokens/; got: {recorded}"
        );
    }

    /// Issue #4768: the sweep child must receive `LOOM_ROLE=sweep-lifecycle`
    /// (the ALREADY-ADMITTED role), not just `LOOM_RUNTIME`. Without this, a
    /// Codex-runtime sweep child reaches `spawn-codex.sh`'s mutable-role
    /// hook-trust preflight with no role signal at all, which is
    /// indistinguishable there from an unrecognized role and silently takes
    /// the read-only fallback instead of failing closed.
    #[test]
    #[serial]
    fn dispatch_sets_loom_role_from_admitted_role() {
        let dir = tempdir().unwrap();
        let workspace = dir.path();
        // Installs the runtime-admission fixture (`.loom/roles/builder.json`
        // + `.loom/runtimes/claude.json`, satisfied by the built-in `claude`
        // runtime) AND the `/loom:sweep` command marker the 2.x guards
        // require with `skip_label_flip = false`.
        touch_sweep_command(workspace);

        // A permissive fake `gh` so every dispatch-path guard (closed-issue,
        // open-PR, park-label) passes and dispatch reaches `spawn_child`.
        let fake_gh = workspace.join("fake-gh.sh");
        let gh_script = format!(
            "#!/usr/bin/env bash\n\
             if [[ \"$1\" == \"api\" && \"$2\" == repos/* ]]; then\n\
             printf '%s\\n' '{state}'\n\
             exit 0\n\
             fi\n\
             if [[ \"$1\" == \"repo\" && \"$2\" == \"view\" ]]; then\n\
             printf 'rjwalters/loom\\n'\n\
             exit 0\n\
             fi\n\
             exit 0\n",
            state = state_probe_json("open", false),
        );
        std::fs::write(&fake_gh, &gh_script).unwrap();
        let mut perms = std::fs::metadata(&fake_gh).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&fake_gh, perms).unwrap();

        // Overwrite the fixture's stub spawn-claude.sh with one that records
        // LOOM_ROLE (and LOOM_RUNTIME, for good measure) to a log file.
        let scripts_dir = workspace.join(".loom").join("scripts");
        let fake_bin = scripts_dir.join("spawn-claude.sh");
        let record_log = workspace.join("role-record.log");
        let script = format!(
            r#"#!/usr/bin/env bash
{{
  printf 'LOOM_ROLE=%s\n' "${{LOOM_ROLE:-unset}}"
  printf 'LOOM_RUNTIME=%s\n' "${{LOOM_RUNTIME:-unset}}"
}} >> "{rec}"
exit 0
"#,
            rec = record_log.display()
        );
        std::fs::write(&fake_bin, script).unwrap();
        let mut perms = std::fs::metadata(&fake_bin).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&fake_bin, perms).unwrap();
        if let Ok(f) = std::fs::File::open(&fake_bin) {
            let _ = f.sync_all();
        }

        let mut config = SweepRegistryConfig::new(workspace.to_path_buf());
        // Bypass the runtime-dispatch seam (`resolve_spawn_bin()` would
        // otherwise find the REAL `defaults/scripts/spawn-worker.sh` and exec
        // through to the real `spawn-claude.sh`): point `spawn_bin` directly
        // at the recording fixture, exactly like every other fixture in this
        // module.
        config.spawn_bin = Some(fake_bin);
        config.gh_bin = Some(fake_gh);
        config.skip_label_flip = false;
        config.journal_path = Some(workspace.join("test-sweeps-journal.json"));
        let mut registry = SweepRegistry::new(config);

        let outcome = registry
            .dispatch(&SweepKind::Issue(4768), None, None, None, None)
            .unwrap();
        assert!(outcome.was_new);

        let recorded = assert_child_wrote(&record_log, "LOOM_ROLE=");
        assert!(
            recorded.contains("LOOM_ROLE=sweep-lifecycle"),
            "expected the admitted role (sweep-lifecycle) in the child env; got: {recorded}"
        );
        assert!(
            recorded.contains("LOOM_RUNTIME=claude"),
            "expected the admitted (built-in) runtime alongside it; got: {recorded}"
        );
    }

    /// End-to-end: a dispatched sweep whose (fake) `spawn-claude.sh` logs the
    /// `using OAuth account '<name>'` line records that account as the registry
    /// `token_name` — reported by both `DispatchOutcome` and the stored
    /// `SweepInfo` (which `list_sweeps` / `get_sweep_status` read from). This
    /// closes the "always unknown" gap (issue #3802). Mirrors the live-dispatch
    /// finding: issue #3780 selected account `agent3-2amlogic`.
    #[test]
    #[serial]
    fn dispatch_captures_selected_account_into_token_name() {
        let dir = tempdir().unwrap();
        // A fake wrapper that logs the selection to stderr exactly as the real
        // spawn-claude.sh does, then lingers briefly (mimicking `exec claude`,
        // which keeps running long after the selection is logged). The daemon
        // already captures this stderr into the per-sweep log.
        let script = "#!/usr/bin/env bash\n\
set -euo pipefail\n\
echo \"spawn-claude: using OAuth account 'agent3-2amlogic' (mode=random)\" >&2\n\
sleep 0.5\n\
exit 0\n";
        let mut registry = lifecycle_registry(dir.path(), script);

        let outcome = registry
            .dispatch(&SweepKind::Issue(3780), None, None, None, None)
            .unwrap();
        assert!(outcome.was_new);
        assert_eq!(
            outcome.token_name, "agent3-2amlogic",
            "DispatchOutcome should carry the selected account, not 'unknown'"
        );

        let info = registry
            .get_status(&outcome.sweep_id)
            .expect("dispatched sweep should be in the registry");
        assert_eq!(
            info.token_name, "agent3-2amlogic",
            "stored SweepInfo (what list_sweeps/get_sweep_status report) should \
             carry the selected account"
        );
    }

    /// The `LOOM_SPAWN_NO_EXPORT` bypass path selects no account, so nothing is
    /// logged — `token_name` must remain `unknown` (not a regression, the
    /// expected "nothing to report" case). Verified here with a fixture that
    /// exits without logging a selection: the `try_wait` early-exit means this
    /// resolves promptly rather than waiting out the capture timeout.
    #[test]
    #[serial]
    fn dispatch_token_name_unknown_when_no_selection_logged() {
        let dir = tempdir().unwrap();
        let script = "#!/usr/bin/env bash\nset -euo pipefail\nexit 0\n";
        let mut registry = lifecycle_registry(dir.path(), script);

        let outcome = registry
            .dispatch(&SweepKind::Issue(4242), None, None, None, None)
            .unwrap();
        assert_eq!(
            outcome.token_name, UNKNOWN_TOKEN_NAME,
            "no selection logged => token_name stays 'unknown'"
        );
    }

    // ===================================================================
    // Concurrent dispatch does not serialize on the account-selection poll
    // (Issue #6592)
    // ===================================================================

    /// Drives `begin_issue_dispatch` -> `poll_and_classify_spawned_child` ->
    /// `finish_issue_dispatch` the same way `ipc.rs`'s
    /// `dispatch_sweep_nonblocking` does: lock only for the two brief
    /// bookend steps, poll UNLOCKED in between. Proves — not merely asserts
    /// — that a burst of concurrent dispatches' polls overlap instead of
    /// serializing behind the registry mutex: the fixture spawn script stays
    /// alive and logs its account selection only after a deliberate delay,
    /// so each dispatch's poll genuinely blocks for that long. If the
    /// registry mutex were held across the poll (the pre-#6592 shape, still
    /// exercised by the plain `dispatch()` — see
    /// `dispatch_captures_selected_account_into_token_name` above), N
    /// concurrent dispatches would take N times as long; this test asserts
    /// the burst completes in well under that serialized bound.
    #[test]
    #[serial]
    fn concurrent_issue_dispatches_do_not_serialize_on_the_account_selection_poll() {
        let dir = tempdir().unwrap();
        let poll_delay = Duration::from_millis(700);
        let script = format!(
            "#!/usr/bin/env bash\nset -euo pipefail\nsleep {:.2}\n\
             echo \"spawn-claude: using OAuth account 'agent-burst' (mode=random)\" >&2\n\
             sleep 5\n",
            poll_delay.as_secs_f64()
        );
        let registry = Arc::new(Mutex::new(lifecycle_registry(dir.path(), &script)));

        const BURST: u32 = 10;
        let start = Instant::now();
        let handles: Vec<std::thread::JoinHandle<DispatchOutcome>> = (0..BURST)
            .map(|i| {
                let registry = Arc::clone(&registry);
                std::thread::spawn(move || {
                    let begin = {
                        let mut sr = registry.lock().unwrap();
                        sr.begin_issue_dispatch(
                            &SweepKind::Issue(81_000 + i),
                            None,
                            None,
                            None,
                            None,
                            None,
                        )
                        .expect("begin_issue_dispatch should succeed for a fresh issue")
                    };
                    let mut prepared = match begin {
                        BeginIssueDispatch::Spawned(prepared) => prepared,
                        BeginIssueDispatch::Done(result) => {
                            panic!("expected Spawned, got Done({result:?})")
                        }
                    };
                    // The genuinely multi-second wait — deliberately run
                    // WITHOUT the registry mutex held, mirroring
                    // `dispatch_sweep_nonblocking`'s unlocked phase.
                    let (token_name, runtime, immediate_preflight_death) =
                        poll_and_classify_spawned_child(
                            &mut prepared.child,
                            &prepared.log_path,
                            &prepared.header_anchor,
                        );
                    let mut sr = registry.lock().unwrap();
                    sr.finish_issue_dispatch(
                        *prepared,
                        token_name,
                        runtime,
                        immediate_preflight_death,
                    )
                    .expect("finish_issue_dispatch should succeed")
                })
            })
            .collect();

        let outcomes: Vec<DispatchOutcome> = handles
            .into_iter()
            .map(|h| h.join().expect("dispatch thread panicked"))
            .collect();
        let elapsed = start.elapsed();

        assert_eq!(outcomes.len(), BURST as usize);
        for outcome in &outcomes {
            assert_eq!(
                outcome.token_name, "agent-burst",
                "every dispatch in the burst should have captured the fixture's selection"
            );
        }

        // Serialized (pre-#6592: registry mutex held across the poll) would
        // take roughly BURST * poll_delay (7s for 10x700ms). Concurrent
        // (post-#6592) should complete close to ONE poll_delay plus
        // guard-chain/spawn overhead. Assert well under the serialized
        // bound — and well under the 30s client ack deadline (AC2) this
        // issue targets.
        let serialized_bound = poll_delay * BURST;
        assert!(
            elapsed < serialized_bound / 2,
            "burst of {BURST} concurrent dispatches took {elapsed:?} (poll_delay={poll_delay:?}) \
             — looks serialized behind the registry mutex (serialized bound ~{serialized_bound:?}), \
             not concurrent"
        );
        assert!(
            elapsed < Duration::from_secs(30),
            "burst took {elapsed:?}, at or over the 30s client ack deadline this issue targets"
        );

        // Best-effort cleanup — don't leak the fixture's long-lived children.
        for outcome in &outcomes {
            let mut sr = registry.lock().unwrap();
            let _ = sr.cancel(&outcome.sweep_id, Duration::from_millis(50));
        }
    }

    /// Regression coverage for the idempotency-key dedup contract this
    /// issue's fix must preserve (Issue #6592, test plan item 3):
    /// `find_running_by_key` must still short-circuit a same-key retry
    /// against a `Running` entry through the SPLIT `begin_issue_dispatch`
    /// path, exactly as it always has through `dispatch()` (see
    /// `dispatch_idempotency_returns_existing` /
    /// `n_dispatch_requests_for_a_live_issue_produce_zero_extra_sweeps`
    /// above for the pre-existing `dispatch()`-level coverage of the same
    /// contract) — no double-spawn, `was_new: false` on the retry.
    #[test]
    #[serial]
    fn begin_issue_dispatch_idempotency_hit_returns_done_without_spawning() {
        let dir = tempdir().unwrap();
        let script = "#!/usr/bin/env bash\nset -euo pipefail\n\
echo \"spawn-claude: using OAuth account 'agent-idem' (mode=random)\" >&2\nsleep 5\n";
        let mut registry = lifecycle_registry(dir.path(), script);

        // First dispatch: a real spawn, going through the full begin -> poll
        // -> finish split.
        let begin = registry
            .begin_issue_dispatch(
                &SweepKind::Issue(82_001),
                Some("retry-key-6592".to_string()),
                None,
                None,
                None,
                None,
            )
            .unwrap();
        let mut prepared = match begin {
            BeginIssueDispatch::Spawned(prepared) => prepared,
            BeginIssueDispatch::Done(result) => panic!("expected Spawned, got Done({result:?})"),
        };
        let (token_name, runtime, death) = poll_and_classify_spawned_child(
            &mut prepared.child,
            &prepared.log_path,
            &prepared.header_anchor,
        );
        let first = registry
            .finish_issue_dispatch(*prepared, token_name, runtime, death)
            .unwrap();
        assert!(first.was_new);

        // Second dispatch, SAME idempotency key: must hit the fast
        // `Done(Ok(..))` path — no `Spawned` variant, no second child.
        let retry = registry
            .begin_issue_dispatch(
                &SweepKind::Issue(82_001),
                Some("retry-key-6592".to_string()),
                None,
                None,
                None,
                None,
            )
            .unwrap();
        match retry {
            BeginIssueDispatch::Done(Ok(outcome)) => {
                assert!(!outcome.was_new, "same-key retry against a Running entry must not spawn");
                assert_eq!(outcome.sweep_id, first.sweep_id);
            }
            BeginIssueDispatch::Done(Err(e)) => panic!("expected an idempotent Ok, got Err: {e}"),
            BeginIssueDispatch::Spawned(_) => {
                panic!(
                    "same-key retry against a Running entry must not reach Spawned (double-spawn)"
                )
            }
        }

        let _ = registry.cancel(&first.sweep_id, Duration::from_millis(50));
    }

    /// Pins the *caller-visible* behavior of a same-idempotency-key retry that
    /// lands inside the begin/poll/finish window this issue's lock split opens
    /// (Issue #6592, Judge review of PR #6600).
    ///
    /// `find_running_by_key` matches only entries already in `self.entries`,
    /// and a dispatch's entry is not inserted until `finish_issue_dispatch`
    /// (post-poll, re-locked). So between `begin_issue_dispatch` returning
    /// `Spawned` and `finish_issue_dispatch` recording the entry — the window
    /// the registry mutex is deliberately released for, bounded by
    /// `TOKEN_NAME_CAPTURE_TIMEOUT` (~5s) — a same-key retry MISSES the
    /// idempotency short-circuit and falls through to the guard chain.
    ///
    /// **The safety property still holds — no double-spawn** — because the
    /// guard chain refuses the retry before any second `Command::spawn()`. Two
    /// independent mechanisms can do the refusing, and *which one wins is
    /// platform-dependent*, so this test deliberately accepts either:
    ///
    /// 1. The #4556 live-claim guard's process-scan leg
    ///    (`live_claim::live_sweep_process_in`), which matches the
    ///    just-spawned child by **argv** and so needs neither a tracked entry
    ///    nor lock ownership. It fires where the platform exposes another
    ///    process's argv (Linux `/proc`) — observed refusing this exact retry
    ///    on CI. The guard's *bookkeeping* legs are indeed blind here
    ///    (`has_tracked_sweep_for` is still false, and the lock's `owner.json`
    ///    still carries this daemon's own pid because
    ///    `record_child_pid_in_lock` runs in `finish_issue_dispatch`) — the
    ///    argv leg is not.
    /// 2. The atomic `acquire_lock` mkdir, which is unconditional and
    ///    platform-independent — the backstop that refuses the retry with a
    ///    `lock collision` wherever leg 1 cannot see the child (observed on
    ///    macOS).
    ///
    /// What *does* differ inside this window is the retry's caller-visible
    /// outcome: a hard `Err` either way, not the graceful `was_new: false` a
    /// retry gets before or after. This test pins that, so the distinction is
    /// a checked behavior rather than a surprise rediscovered later. It is
    /// deterministic: the window is reproduced by simply not calling
    /// `finish_issue_dispatch` yet — no threads, no timing dependence, and no
    /// dependence on which of the two guards happens to fire.
    ///
    /// Contrast `begin_issue_dispatch_idempotency_hit_returns_done_without_spawning`
    /// above, which covers a retry *after* completion (the realistic client
    /// case: a retry follows a 30s ack timeout, long past the ~5s window).
    #[test]
    #[serial]
    fn same_key_retry_during_the_unlocked_poll_window_is_refused_not_double_spawned() {
        let dir = tempdir().unwrap();
        let script = "#!/usr/bin/env bash\nset -euo pipefail\n\
echo \"spawn-claude: using OAuth account 'agent-race' (mode=random)\" >&2\nsleep 5\n";
        let mut registry = lifecycle_registry(dir.path(), script);

        // First dispatch: stop right after `begin` returns `Spawned`. The
        // child exists, but no entry has been recorded yet — this IS the
        // unlocked-poll window, reproduced without any timing dependence.
        let begin = registry
            .begin_issue_dispatch(
                &SweepKind::Issue(82_002),
                Some("race-key-6592".to_string()),
                None,
                None,
                None,
                None,
            )
            .unwrap();
        let mut prepared = match begin {
            BeginIssueDispatch::Spawned(prepared) => prepared,
            BeginIssueDispatch::Done(result) => panic!("expected Spawned, got Done({result:?})"),
        };

        // Same-key retry landing INSIDE that window. It misses the
        // idempotency-key short-circuit (no entry to match yet) and must be
        // REFUSED by the guard chain — never a second spawn. Either refusal
        // mechanism is acceptable and both are asserted for explicitly (see
        // this test's doc comment): the #4556 live-claim argv probe where the
        // platform exposes the child's argv, otherwise the atomic
        // `acquire_lock` mkdir.
        let racing = registry.begin_issue_dispatch(
            &SweepKind::Issue(82_002),
            Some("race-key-6592".to_string()),
            None,
            None,
            None,
            None,
        );
        match racing {
            Err(e) => {
                let msg = e.to_string();
                assert!(
                    msg.contains("lock collision") || msg.contains("#4556 live-claim guard"),
                    "a same-key retry inside the unlocked-poll window must be refused by the \
                     claim lock or the #4556 live-claim guard; got an unrecognized error: {msg}"
                );
            }
            Ok(BeginIssueDispatch::Spawned(_)) => panic!(
                "same-key retry inside the unlocked-poll window must NOT spawn a second child"
            ),
            Ok(BeginIssueDispatch::Done(result)) => panic!(
                "same-key retry inside the unlocked-poll window is expected to be refused by the \
                 guard chain, not to short-circuit: Done({result:?})"
            ),
        }

        // Now close the window and confirm it was genuinely transient: once
        // `finish_issue_dispatch` records the entry, the SAME retry gets the
        // graceful idempotent hand-back instead of the lock-collision error.
        let (token_name, runtime, death) = poll_and_classify_spawned_child(
            &mut prepared.child,
            &prepared.log_path,
            &prepared.header_anchor,
        );
        let first = registry
            .finish_issue_dispatch(*prepared, token_name, runtime, death)
            .unwrap();
        assert!(first.was_new);

        let after = registry
            .begin_issue_dispatch(
                &SweepKind::Issue(82_002),
                Some("race-key-6592".to_string()),
                None,
                None,
                None,
                None,
            )
            .unwrap();
        match after {
            BeginIssueDispatch::Done(Ok(outcome)) => {
                assert!(
                    !outcome.was_new,
                    "once the entry is recorded, a same-key retry dedups gracefully again"
                );
                assert_eq!(outcome.sweep_id, first.sweep_id);
            }
            BeginIssueDispatch::Done(Err(e)) => {
                panic!("expected a graceful idempotency hit after finish; got Err: {e}")
            }
            BeginIssueDispatch::Spawned(_) => {
                panic!("expected a graceful idempotency hit after finish; got a second spawn")
            }
        }

        let _ = registry.cancel(&first.sweep_id, Duration::from_millis(50));
    }

    // ===================================================================
    // Startup-race mitigation: stagger + watchdog (Issue #3887)
    // ===================================================================

    // --- stagger_wait pure function ---

    #[test]
    fn stagger_wait_zero_stagger_never_waits() {
        let now = Instant::now();
        assert_eq!(stagger_wait(None, Duration::ZERO, now), Duration::ZERO);
        assert_eq!(
            stagger_wait(Some(now), Duration::ZERO, now + Duration::from_secs(1)),
            Duration::ZERO
        );
    }

    #[test]
    fn stagger_wait_no_prior_spawn_never_waits() {
        let now = Instant::now();
        assert_eq!(stagger_wait(None, Duration::from_secs(2), now), Duration::ZERO);
    }

    #[test]
    fn stagger_wait_returns_remaining_gap() {
        let base = Instant::now();
        let stagger = Duration::from_millis(2000);
        // 500ms elapsed since the last spawn ⇒ 1500ms still to wait.
        let now = base + Duration::from_millis(500);
        assert_eq!(stagger_wait(Some(base), stagger, now), Duration::from_millis(1500));
    }

    #[test]
    fn stagger_wait_elapsed_past_stagger_is_zero() {
        let base = Instant::now();
        let stagger = Duration::from_millis(2000);
        // 3s elapsed ⇒ the full gap has passed, no wait.
        let now = base + Duration::from_millis(3000);
        assert_eq!(stagger_wait(Some(base), stagger, now), Duration::ZERO);
    }

    // --- set/get dispatch stagger ---

    #[test]
    fn dispatch_stagger_setter_roundtrips() {
        let tmp = tempdir().unwrap();
        let (mut reg, _rec) = fixture_registry(tmp.path());
        assert_eq!(reg.dispatch_stagger(), Duration::ZERO, "default is zero");
        reg.set_dispatch_stagger(Duration::from_millis(1500));
        assert_eq!(reg.dispatch_stagger(), Duration::from_millis(1500));
    }

    #[test]
    fn dispatch_applies_configured_stagger_between_spawns() {
        // With a small stagger, two back-to-back dispatches are spaced by at
        // least the stagger (the second waits out the gap in `dispatch`).
        let tmp = tempdir().unwrap();
        let (mut reg, rec) = fixture_registry(tmp.path());
        reg.set_dispatch_stagger(Duration::from_millis(400));

        let start = Instant::now();
        reg.dispatch(&SweepKind::Issue(8001), None, None, None, None)
            .unwrap();
        reg.dispatch(&SweepKind::Issue(8002), None, None, None, None)
            .unwrap();
        let elapsed = start.elapsed();

        assert!(
            elapsed >= Duration::from_millis(400),
            "second dispatch should have waited out the stagger; elapsed={elapsed:?}"
        );
        // Both fake children ran. Generous budget (#3985): under host CPU
        // starvation the children can be slow to be scheduled onto the record
        // log, so a tight 5s bound made this red for a host-load reason.
        assert!(wait_for_contents(&rec, "issue=8002", FIXTURE_CHILD_WAIT_MS) || rec.exists());
    }

    /// AC6: `dispatch` for a closed issue is refused, and it must NOT flip any
    /// labels (no `issue edit`) — a watchdog re-dispatch can never re-claim a
    /// closed/merged issue.
    #[test]
    #[serial]
    fn dispatch_refuses_closed_issue_without_flipping_labels() {
        let tmp = tempdir().unwrap();
        let ws = tmp.path();
        std::env::remove_var("LOOM_REPO");
        let (mut reg, gh_log) = closed_guard_registry(ws, &state_probe_json("closed", false), 0);

        let err = reg
            .dispatch(&SweepKind::Issue(4078), None, None, None, None)
            .expect_err("a closed issue must be refused");
        assert!(
            err.to_string().contains("closed"),
            "error explains the closed-issue guard; got: {err}"
        );

        let calls = std::fs::read_to_string(&gh_log).unwrap_or_default();
        assert!(
            calls.contains("api repos/rjwalters/loom/issues/4078"),
            "the guard probed issue state over REST; got: {calls:?}"
        );
        assert!(
            !calls.contains("issue edit"),
            "no label flip on a refused dispatch; got: {calls:?}"
        );
        // No lock was acquired and no entry recorded.
        assert!(running_issue_sweep_id(&reg, 4078).is_none());
    }

    /// #4504 case (b): a dispatch number that resolves to a **merged** pull
    /// request is refused. REST reports a merged PR as `state: "closed"` with a
    /// `pull_request` key, so this case is caught by BOTH legs of the guard —
    /// the point is that it can no longer reach the `_ => None` fail-open arm the
    /// way `gh issue view`'s GraphQL `MERGED` state did.
    #[test]
    #[serial]
    fn dispatch_refuses_merged_pr_number_without_flipping_labels() {
        let tmp = tempdir().unwrap();
        let ws = tmp.path();
        std::env::remove_var("LOOM_REPO");
        let (mut reg, gh_log) = closed_guard_registry(ws, &state_probe_json("closed", true), 0);

        let err = reg
            .dispatch(&SweepKind::Issue(4501), None, None, None, None)
            .expect_err("a merged PR number must be refused");
        assert!(
            err.to_string().contains("pull request"),
            "error names the PR-number case; got: {err}"
        );

        let calls = std::fs::read_to_string(&gh_log).unwrap_or_default();
        assert!(
            calls.contains("api repos/rjwalters/loom/issues/4501"),
            "the guard probed the number over REST; got: {calls:?}"
        );
        assert!(
            !calls.contains("issue edit"),
            "no label flip on a refused dispatch; got: {calls:?}"
        );
        assert!(running_issue_sweep_id(&reg, 4501).is_none(), "no lock, no entry");
    }

    /// #4504 case (c), the load-bearing one: a dispatch number that resolves to
    /// an **open** pull request is refused too. Its `state` is `"open"` — byte
    /// identical to an open issue's — so only the structural `pull_request`
    /// discriminator can catch it. A fix that merely appended `"MERGED"` to the
    /// old state-string match would dispatch this happily.
    #[test]
    #[serial]
    fn dispatch_refuses_open_pr_number_without_flipping_labels() {
        let tmp = tempdir().unwrap();
        let ws = tmp.path();
        std::env::remove_var("LOOM_REPO");
        let (mut reg, gh_log) = closed_guard_registry(ws, &state_probe_json("open", true), 0);

        let err = reg
            .dispatch(&SweepKind::Issue(4502), None, None, None, None)
            .expect_err("an open PR number must be refused");
        assert!(
            err.to_string().contains("pull request"),
            "error names the PR-number case; got: {err}"
        );

        let calls = std::fs::read_to_string(&gh_log).unwrap_or_default();
        assert!(
            calls.contains("api repos/rjwalters/loom/issues/4502"),
            "the guard probed the number over REST; got: {calls:?}"
        );
        assert!(
            !calls.contains("issue edit"),
            "no label flip on a refused dispatch; got: {calls:?}"
        );
        assert!(running_issue_sweep_id(&reg, 4502).is_none(), "no lock, no entry");
    }

    /// #4504 belt-and-suspenders: an Issue-shaped node that reports `MERGED` is
    /// terminal exactly like `CLOSED` — it must never fall through to the
    /// fail-open arm (the original #4088 bug).
    #[test]
    #[serial]
    fn dispatch_refuses_merged_state_on_issue_shaped_node() {
        let tmp = tempdir().unwrap();
        let ws = tmp.path();
        std::env::remove_var("LOOM_REPO");
        let (mut reg, gh_log) = closed_guard_registry(ws, &state_probe_json("MERGED", false), 0);

        let err = reg
            .dispatch(&SweepKind::Issue(4505), None, None, None, None)
            .expect_err("a MERGED state must be refused like CLOSED");
        assert!(
            err.to_string().contains("closed"),
            "error explains the closed-issue guard; got: {err}"
        );

        let calls = std::fs::read_to_string(&gh_log).unwrap_or_default();
        assert!(
            !calls.contains("issue edit"),
            "no label flip on a refused dispatch; got: {calls:?}"
        );
        assert!(running_issue_sweep_id(&reg, 4505).is_none(), "no lock, no entry");
    }

    /// AC6 fail-open: a forge lookup error (non-zero `gh`) must NOT wedge
    /// dispatch — the guard returns `None` and dispatch proceeds normally.
    #[test]
    #[serial]
    fn dispatch_fails_open_when_issue_state_lookup_errors() {
        let tmp = tempdir().unwrap();
        let ws = tmp.path();
        std::env::remove_var("LOOM_REPO");
        // The state probe exits non-zero ⇒ state unknown ⇒ fail open.
        let (mut reg, gh_log) = closed_guard_registry(ws, "", 1);

        let out = reg
            .dispatch(&SweepKind::Issue(4079), None, None, None, None)
            .expect("a gh outage must not wedge dispatch (fail-open)");
        assert!(wait_until_dead(out.pid, FIXTURE_CHILD_WAIT_MS));

        let calls = std::fs::read_to_string(&gh_log).unwrap_or_default();
        assert!(
            calls.contains("api repos/rjwalters/loom/issues/4079"),
            "the guard probed issue state; got: {calls:?}"
        );
        assert!(
            calls.contains("issue edit 4079"),
            "dispatch proceeded to the label flip after failing open; got: {calls:?}"
        );

        if let Some(id) = running_issue_sweep_id(&reg, 4079) {
            let _ = reg.cancel(&id, Duration::from_secs(2));
        }
    }

    /// AC6 fail-open (unparseable): a `gh` that exits 0 but emits output the
    /// probe cannot parse into `{state, is_pr}` is a genuine lookup failure, not
    /// a verdict — dispatch must proceed.
    #[test]
    #[serial]
    fn dispatch_fails_open_when_issue_state_output_is_unparseable() {
        let tmp = tempdir().unwrap();
        let ws = tmp.path();
        std::env::remove_var("LOOM_REPO");
        let (mut reg, gh_log) = closed_guard_registry(ws, "not json at all", 0);

        let out = reg
            .dispatch(&SweepKind::Issue(4080), None, None, None, None)
            .expect("an unparseable probe answer must not wedge dispatch (fail-open)");
        assert!(wait_until_dead(out.pid, FIXTURE_CHILD_WAIT_MS));

        let calls = std::fs::read_to_string(&gh_log).unwrap_or_default();
        assert!(
            calls.contains("issue edit 4080"),
            "dispatch proceeded to the label flip after failing open; got: {calls:?}"
        );

        if let Some(id) = running_issue_sweep_id(&reg, 4080) {
            let _ = reg.cancel(&id, Duration::from_secs(2));
        }
    }

    /// `dispatch` for an open issue that already has an open linked PR is refused
    /// with the typed [`OpenPrDispatchError`] (downcast-matchable, not string
    /// matching), and it must NOT acquire the claim lock or flip any labels — a
    /// re-dispatch of already-in-review work would duplicate it.
    #[test]
    #[serial]
    fn dispatch_refuses_open_pr_without_flipping_labels() {
        let tmp = tempdir().unwrap();
        let ws = tmp.path();
        std::env::set_var("LOOM_REPO", "rjwalters/loom");
        let (mut reg, gh_log) = open_pr_guard_registry(ws, "4200", 0, false);

        let err = reg
            .dispatch(&SweepKind::Issue(4123), None, None, None, None)
            .expect_err("an issue with an open linked PR must be refused");
        let typed = err
            .downcast_ref::<OpenPrDispatchError>()
            .expect("refusal must carry the typed OpenPrDispatchError");
        assert_eq!(typed.issue, 4123);
        assert_eq!(typed.pr, 4200);

        let calls = std::fs::read_to_string(&gh_log).unwrap_or_default();
        assert!(calls.contains("api graphql"), "the guard queried the closes-graph");
        assert!(
            !calls.contains("issue edit"),
            "no label flip on a refused dispatch; got: {calls:?}"
        );
        // No lock acquired, no entry recorded.
        assert!(running_issue_sweep_id(&reg, 4123).is_none());
        std::env::remove_var("LOOM_REPO");
    }

    /// Issue #6593: the refusal must not be a dead end. The candidate class the
    /// open-PR guard refuses is exactly the class `sweep.md`'s aggressive
    /// taxonomy says to drive to merge — but the child whose per-issue pre-flight
    /// would perform that routing never gets spawned, so the refusal text itself
    /// has to name the reachable alternative: a `PrSet` dispatch (#5342) for the
    /// very PR the guard found. Assert the rendered `Display` carries a
    /// copy-pasteable `kind={"PrSet":[<pr>]}` naming that PR.
    #[test]
    #[serial]
    fn open_pr_refusal_text_names_the_prset_alternative() {
        let tmp = tempdir().unwrap();
        let ws = tmp.path();
        std::env::set_var("LOOM_REPO", "rjwalters/loom");
        let (mut reg, _gh_log) = open_pr_guard_registry(ws, "4200", 0, false);

        let err = reg
            .dispatch(&SweepKind::Issue(4123), None, None, None, None)
            .expect_err("an issue with an open linked PR must be refused");
        let rendered = err.to_string();

        assert!(
            rendered.contains(r#"kind={"PrSet":[4200]}"#),
            "the refusal must hand the caller the exact PrSet dispatch for the PR it found \
             (#6593); got: {rendered:?}"
        );
        assert!(
            rendered.contains("#5342"),
            "the refusal must cite the PrSet dispatch support it points at (#5342); got: \
             {rendered:?}"
        );
        // The pre-#6593 diagnosis must survive alongside the new hint.
        assert!(
            rendered.contains("#4123 open-PR guard"),
            "the refusal must still name the guard that fired; got: {rendered:?}"
        );

        // The same text is what a downcast-matching caller renders, too.
        let typed = err
            .downcast_ref::<OpenPrDispatchError>()
            .expect("refusal must still carry the typed OpenPrDispatchError");
        assert_eq!(typed.to_string(), rendered);

        std::env::remove_var("LOOM_REPO");
    }

    /// Issue #6350 (Ask 1) regression: "host A opens a PR closing issue N;
    /// host B's work finder skips N on its next tick (no new lease...)". The
    /// open-PR guard's probe is a forge (GraphQL closes-graph) query, not any
    /// host-local state, so it refuses identically regardless of which host
    /// actually opened the linked PR — this test names that property
    /// explicitly, on top of the sibling test above's narrower "no label
    /// flip" assertion: a refused dispatch must never even reach
    /// `write_lease_comment` (`gh issue comment`), since that call only
    /// happens after a CONFIRMED successful label flip.
    #[test]
    #[serial]
    fn dispatch_refuses_open_pr_without_writing_a_lease_comment() {
        let tmp = tempdir().unwrap();
        let ws = tmp.path();
        std::env::set_var("LOOM_REPO", "rjwalters/loom");
        let (mut reg, gh_log) = open_pr_guard_registry(ws, "4200", 0, false);

        let err = reg
            .dispatch(&SweepKind::Issue(4123), None, None, None, None)
            .expect_err("an issue with an open linked PR must be refused, cross-host");
        assert!(err.downcast_ref::<OpenPrDispatchError>().is_some());

        let calls = std::fs::read_to_string(&gh_log).unwrap_or_default();
        assert!(
            !calls.contains("issue comment"),
            "no lease record may be written for a dispatch the open-PR guard refused; got: \
             {calls:?}"
        );
        std::env::remove_var("LOOM_REPO");
    }

    /// Fail-open (the single most safety-critical property): a forge error on the
    /// open-PR probe (non-zero `gh api graphql`) must NOT wedge dispatch — the
    /// guard returns `None` and dispatch proceeds to spawn + label flip.
    #[test]
    #[serial]
    fn dispatch_fails_open_when_open_pr_lookup_errors() {
        let tmp = tempdir().unwrap();
        let ws = tmp.path();
        std::env::set_var("LOOM_REPO", "rjwalters/loom");
        // `api graphql` exits non-zero ⇒ open-PR state unknown ⇒ fail open.
        let (mut reg, gh_log) = open_pr_guard_registry(ws, "", 1, false);

        let out = reg
            .dispatch(&SweepKind::Issue(4124), None, None, None, None)
            .expect("a forge error on the open-PR probe must not wedge dispatch (fail-open)");
        assert!(wait_until_dead(out.pid, FIXTURE_CHILD_WAIT_MS));

        let calls = std::fs::read_to_string(&gh_log).unwrap_or_default();
        assert!(calls.contains("api graphql"), "the guard attempted the closes-graph query");
        assert!(
            calls.contains("issue edit 4124"),
            "dispatch proceeded to the label flip after failing open; got: {calls:?}"
        );
        if let Some(id) = running_issue_sweep_id(&reg, 4124) {
            let _ = reg.cancel(&id, Duration::from_secs(2));
        }
        std::env::remove_var("LOOM_REPO");
    }

    /// An issue whose only linked PR is merged/closed is NOT blocked: the
    /// `state == "OPEN"` `--jq` filter yields no PR number, so the probe returns
    /// nothing and dispatch proceeds. Regression guard against an
    /// `includeClosedPrs` misconfiguration that would strand every issue whose
    /// PR ever merged.
    #[test]
    #[serial]
    fn dispatch_open_pr_guard_ignores_merged_only_pr() {
        let tmp = tempdir().unwrap();
        let ws = tmp.path();
        std::env::set_var("LOOM_REPO", "rjwalters/loom");
        // Empty post-`--jq` output = no OPEN-state PR (only merged/closed ones).
        let (mut reg, _gh_log) = open_pr_guard_registry(ws, "", 0, false);

        let out = reg
            .dispatch(&SweepKind::Issue(4125), None, None, None, None)
            .expect("an issue whose only linked PR is merged/closed must not be blocked");
        assert!(wait_until_dead(out.pid, FIXTURE_CHILD_WAIT_MS));
        if let Some(id) = running_issue_sweep_id(&reg, 4125) {
            let _ = reg.cancel(&id, Duration::from_secs(2));
        }
        std::env::remove_var("LOOM_REPO");
    }

    /// `skip_label_flip = true` bypasses the open-PR guard entirely (test-fixture
    /// path): even a fake `gh` that WOULD report an open PR is never consulted,
    /// and dispatch proceeds.
    #[test]
    #[serial]
    fn dispatch_skip_label_flip_bypasses_open_pr_guard() {
        let tmp = tempdir().unwrap();
        let ws = tmp.path();
        std::env::set_var("LOOM_REPO", "rjwalters/loom");
        let (mut reg, gh_log) = open_pr_guard_registry(ws, "4200", 0, true);

        let out = reg
            .dispatch(&SweepKind::Issue(4126), None, None, None, None)
            .expect("skip_label_flip must bypass the open-PR guard entirely");
        assert!(wait_until_dead(out.pid, FIXTURE_CHILD_WAIT_MS));

        let calls = std::fs::read_to_string(&gh_log).unwrap_or_default();
        assert!(
            !calls.contains("api graphql"),
            "no forge call at all when label flips are disabled; got: {calls:?}"
        );
        if let Some(id) = running_issue_sweep_id(&reg, 4126) {
            let _ = reg.cancel(&id, Duration::from_secs(2));
        }
        std::env::remove_var("LOOM_REPO");
    }

    /// The 2.5 closed-issue guard (#4088) fires BEFORE the 2.6 open-PR guard: a
    /// closed issue (with a merged PR) is refused by 2.5 and the open-PR probe
    /// never runs — no regression to the existing closed-issue path.
    #[test]
    #[serial]
    fn closed_issue_guard_fires_before_open_pr_guard() {
        let tmp = tempdir().unwrap();
        let ws = tmp.path();
        std::env::remove_var("LOOM_REPO");
        let (mut reg, gh_log) = closed_guard_registry(ws, &state_probe_json("closed", false), 0);

        let err = reg
            .dispatch(&SweepKind::Issue(4200), None, None, None, None)
            .expect_err("a closed issue must still be refused by the 2.5 guard");
        assert!(
            err.to_string().contains("closed"),
            "the 2.5 closed-issue guard wins; got: {err}"
        );
        let calls = std::fs::read_to_string(&gh_log).unwrap_or_default();
        assert!(
            !calls.contains("api graphql"),
            "the open-PR (2.6) probe must never run once 2.5 refuses; got: {calls:?}"
        );
    }

    /// AC: `dispatch` for an issue carrying `loom:blocked` is refused with the
    /// typed [`ParkedIssueDispatchError`], and it must NOT acquire the claim lock
    /// or flip any labels — a deliberate park must survive every dispatch route.
    /// The probe rides the REST bucket (`gh api repos/.../issues/N`), not
    /// GraphQL.
    #[test]
    #[serial]
    fn dispatch_refuses_blocked_issue_without_flipping_labels() {
        let tmp = tempdir().unwrap();
        let ws = tmp.path();
        std::env::set_var("LOOM_REPO", "rjwalters/loom");
        let (mut reg, gh_log) = park_guard_registry(ws, "loom:blocked", 0, "", false);

        let err = reg
            .dispatch(&SweepKind::Issue(4444), None, None, None, None)
            .expect_err("a parked issue must be refused");
        let typed = err
            .downcast_ref::<ParkedIssueDispatchError>()
            .expect("refusal must carry the typed ParkedIssueDispatchError");
        assert_eq!(typed.issue, 4444);
        assert_eq!(typed.label, "loom:blocked");

        let calls = std::fs::read_to_string(&gh_log).unwrap_or_default();
        assert!(
            calls.contains("api repos/rjwalters/loom/issues/4444 --jq .labels[].name"),
            "the guard must probe labels over REST, not GraphQL; got: {calls:?}"
        );
        assert!(
            !calls.contains("issue edit"),
            "no label flip on a refused dispatch; got: {calls:?}"
        );
        assert!(running_issue_sweep_id(&reg, 4444).is_none(), "no lock, no entry");
        std::env::remove_var("LOOM_REPO");
    }

    /// AC: `loom:operator-only` is the second park label and refuses identically
    /// — the daemon must never dispatch work a human has claimed for themselves.
    #[test]
    #[serial]
    fn dispatch_refuses_operator_only_issue() {
        let tmp = tempdir().unwrap();
        let ws = tmp.path();
        std::env::set_var("LOOM_REPO", "rjwalters/loom");
        let (mut reg, _gh_log) = park_guard_registry(ws, "loom:operator-only", 0, "", false);

        let err = reg
            .dispatch(&SweepKind::Issue(4445), None, None, None, None)
            .expect_err("an operator-only issue must be refused");
        let typed = err
            .downcast_ref::<ParkedIssueDispatchError>()
            .expect("refusal must carry the typed ParkedIssueDispatchError");
        assert_eq!(typed.label, "loom:operator-only");
        assert!(running_issue_sweep_id(&reg, 4445).is_none());
        std::env::remove_var("LOOM_REPO");
    }

    /// AC (the load-bearing exclusion): `loom:building` ALONE must NOT refuse.
    /// It is legitimately present on the daemon's own in-flight claim, so a guard
    /// keyed on the full `SKIP_LABELS` set would break the review-stall
    /// watchdog's cancel-and-re-dispatch and the reaper's checkpoint-resume.
    #[test]
    #[serial]
    fn dispatch_park_guard_allows_building_label_alone() {
        let tmp = tempdir().unwrap();
        let ws = tmp.path();
        std::env::set_var("LOOM_REPO", "rjwalters/loom");
        let (mut reg, gh_log) = park_guard_registry(ws, "loom:building loom:curated", 0, "", false);

        let out = reg
            .dispatch(&SweepKind::Issue(4446), None, None, None, None)
            .expect("loom:building alone must never refuse dispatch");
        assert!(wait_until_dead(out.pid, FIXTURE_CHILD_WAIT_MS));

        let calls = std::fs::read_to_string(&gh_log).unwrap_or_default();
        assert!(
            calls.contains("api repos/rjwalters/loom/issues/4446 --jq .labels[].name"),
            "the guard still probed; it just did not refuse; got: {calls:?}"
        );
        assert!(
            calls.contains("issue edit 4446"),
            "dispatch proceeded to the label flip; got: {calls:?}"
        );
        if let Some(id) = running_issue_sweep_id(&reg, 4446) {
            let _ = reg.cancel(&id, Duration::from_secs(2));
        }
        std::env::remove_var("LOOM_REPO");
    }

    /// AC (fail-open, the single most safety-critical property): a forge error on
    /// the REST label probe (non-zero `gh api`) must NOT wedge dispatch — the
    /// probe returns `None` and dispatch proceeds to spawn + label flip, exactly
    /// like the 2.5/2.6 guards.
    #[test]
    #[serial]
    fn dispatch_fails_open_when_park_label_probe_errors() {
        let tmp = tempdir().unwrap();
        let ws = tmp.path();
        std::env::set_var("LOOM_REPO", "rjwalters/loom");
        // The park label IS present, but the probe fails ⇒ unknown ⇒ fail open.
        let (mut reg, gh_log) = park_guard_registry(ws, "loom:blocked", 1, "", false);

        let out = reg
            .dispatch(&SweepKind::Issue(4447), None, None, None, None)
            .expect("a gh outage on the park probe must not wedge dispatch (fail-open)");
        assert!(wait_until_dead(out.pid, FIXTURE_CHILD_WAIT_MS));

        let calls = std::fs::read_to_string(&gh_log).unwrap_or_default();
        assert!(
            calls.contains("api repos/rjwalters/loom/issues/4447 --jq .labels[].name"),
            "the guard attempted the REST probe; got: {calls:?}"
        );
        assert!(
            calls.contains("issue edit 4447"),
            "dispatch proceeded to the label flip after failing open; got: {calls:?}"
        );
        if let Some(id) = running_issue_sweep_id(&reg, 4447) {
            let _ = reg.cancel(&id, Duration::from_secs(2));
        }
        std::env::remove_var("LOOM_REPO");
    }

    /// AC: `skip_label_flip = true` (test fixtures without `gh` credentials)
    /// never attempts the probe at all — not even the REST call — mirroring the
    /// 2.5/2.6 skip condition.
    #[test]
    #[serial]
    fn dispatch_skip_label_flip_bypasses_park_guard() {
        let tmp = tempdir().unwrap();
        let ws = tmp.path();
        std::env::set_var("LOOM_REPO", "rjwalters/loom");
        let (mut reg, gh_log) = park_guard_registry(ws, "loom:blocked", 0, "", true);

        let out = reg
            .dispatch(&SweepKind::Issue(4448), None, None, None, None)
            .expect("skip_label_flip must bypass the park guard entirely");
        assert!(wait_until_dead(out.pid, FIXTURE_CHILD_WAIT_MS));

        let calls = std::fs::read_to_string(&gh_log).unwrap_or_default();
        assert!(
            !calls.contains("api repos/"),
            "no forge call at all when label flips are disabled; got: {calls:?}"
        );
        if let Some(id) = running_issue_sweep_id(&reg, 4448) {
            let _ = reg.cancel(&id, Duration::from_secs(2));
        }
        std::env::remove_var("LOOM_REPO");
    }

    /// Guard ordering: 2.6 (open-PR) runs before 2.7 (park label), so an ordinary
    /// dispatch of a parked issue that ALSO has an open PR is attributed to the
    /// cheaper-to-explain open-PR refusal and never pays for the REST probe.
    #[test]
    #[serial]
    fn open_pr_guard_fires_before_park_guard() {
        let tmp = tempdir().unwrap();
        let ws = tmp.path();
        std::env::set_var("LOOM_REPO", "rjwalters/loom");
        let (mut reg, gh_log) = park_guard_registry(ws, "loom:blocked", 0, "4500", false);

        let err = reg
            .dispatch(&SweepKind::Issue(4449), None, None, None, None)
            .expect_err("an issue with an open linked PR must be refused");
        assert!(
            err.downcast_ref::<OpenPrDispatchError>().is_some(),
            "the 2.6 open-PR guard wins for an ordinary dispatch; got: {err}"
        );
        let calls = std::fs::read_to_string(&gh_log).unwrap_or_default();
        assert!(
            !calls.contains(".labels[].name"),
            "the 2.7 REST label probe must not run once 2.6 refuses; got: {calls:?}"
        );
        std::env::remove_var("LOOM_REPO");
    }

    /// AC (Test Plan item 2, regression): the recovery path must NOT weaken
    /// the #4123 guard for ordinary dispatches. After a reaper-driven resume
    /// has fired for an issue (so it now has a fresh Running sweep AND an
    /// open PR), a plain `dispatch()` call for the SAME issue — simulating an
    /// unrelated later work-finder tick — is still refused with the typed
    /// `OpenPrDispatchError`. The resume bypass is unreachable from the
    /// public `dispatch()` entry point.
    #[tokio::test]
    #[serial]
    async fn ordinary_dispatch_still_refused_after_a_resume() {
        let tmp = tempdir().unwrap();
        let ws = tmp.path();
        std::env::set_var("LOOM_REPO", "rjwalters/loom");
        let (mut reg, _gh_log) = open_pr_guard_registry(ws, "4302", 0, false);

        write_checkpoint(&reg, 4259, "doctor-done");
        insert_dead_running_entry(&mut reg, 4259, "sweep-issue-4259-crashed");
        let changed = reg.reap_once();
        assert!(changed >= 1);
        assert!(
            running_issue_sweep_id(&reg, 4259).is_some(),
            "the resume dispatch must have created a fresh Running entry first"
        );

        // A later, ordinary re-dispatch attempt for the same issue (e.g. a
        // stray work-finder tick, or a watchdog) must still be refused — the
        // open PR is still open, and this call carries no resume exemption.
        let err = reg
            .dispatch(&SweepKind::Issue(4259), None, None, None, None)
            .expect_err("an ordinary dispatch must still be refused by the #4123 guard");
        assert!(
            err.downcast_ref::<OpenPrDispatchError>().is_some(),
            "must be the typed OpenPrDispatchError, not some other failure; got: {err}"
        );
        std::env::remove_var("LOOM_REPO");
    }

    // --- workspace-commands dispatch guard (Issue #4027) ---

    /// A workspace that "looks like" a repo (`.git`/`.loom` present, so
    /// `looks_like_workspace()` in `workspace_registry.rs` would pass) but
    /// was never `loom-daemon init`-ed — the reproduction from #4027 (a
    /// second daemon host with a bare `git clone`). `dispatch` must refuse
    /// BEFORE spending any forge call or token: no `gh` invocation at all
    /// (not even the closed-issue probe), no spawned child, no registry
    /// entry, and the error must name the `loom-daemon init` remediation.
    #[test]
    #[serial]
    fn dispatch_refuses_workspace_missing_sweep_command() {
        let tmp = tempdir().unwrap();
        let ws = tmp.path();
        std::env::remove_var("LOOM_REPO");
        let (mut reg, gh_log) = closed_guard_registry(ws, &state_probe_json("open", false), 0);
        // `closed_guard_registry` installs the marker by default (so its own
        // AC6 tests reach the closed-issue guard under test there) — remove
        // it here to simulate the #4027 wedge scenario.
        std::fs::remove_file(
            ws.join(".claude")
                .join("commands")
                .join("loom")
                .join("sweep.md"),
        )
        .unwrap();

        let err = reg
            .dispatch(&SweepKind::Issue(4222), None, None, None, None)
            .expect_err("a workspace missing installed commands must be refused");
        assert!(
            err.to_string().contains("loom-daemon init"),
            "error names the remediation; got: {err}"
        );
        assert!(
            err.to_string().contains("sweep.md"),
            "error names the missing marker; got: {err}"
        );

        let calls = std::fs::read_to_string(&gh_log).unwrap_or_default();
        assert!(
            calls.is_empty(),
            "no forge call whatsoever (no closed-issue probe, no label flip) on a \
             workspace-commands-refused dispatch; got: {calls:?}"
        );
        assert!(
            running_issue_sweep_id(&reg, 4222).is_none(),
            "no registry entry recorded on a refused dispatch"
        );

        // Issue #6440: this refusal must be the typed, downcast-matchable
        // `WorkspaceCommandsMissingDispatchError` — not a string-matched
        // generic `anyhow!` — so `work_finder` can attribute it to its own
        // `workspace-commands-missing` counter and skip the whole workspace's
        // candidate batch instead of re-discovering the same refusal once per
        // ready issue every tick (the 865-refusals-in-an-hour incident).
        let typed = err
            .downcast_ref::<WorkspaceCommandsMissingDispatchError>()
            .expect("refusal must carry the typed WorkspaceCommandsMissingDispatchError");
        assert_eq!(typed.workspace, ws);
    }

    /// Regression guard: a workspace WITH the marker installed dispatches
    /// exactly as before — the #4027 guard is a pure no-op for a properly
    /// initialized workspace.
    #[test]
    #[serial]
    fn dispatch_proceeds_when_sweep_command_present() {
        let tmp = tempdir().unwrap();
        let ws = tmp.path();
        std::env::remove_var("LOOM_REPO");
        let (mut reg, gh_log) = closed_guard_registry(ws, &state_probe_json("open", false), 0);

        let out = reg
            .dispatch(&SweepKind::Issue(4223), None, None, None, None)
            .expect("a properly initialized workspace must dispatch normally");
        assert!(wait_until_dead(out.pid, FIXTURE_CHILD_WAIT_MS));

        let calls = std::fs::read_to_string(&gh_log).unwrap_or_default();
        assert!(
            calls.contains("issue edit 4223"),
            "dispatch reached the label flip; got: {calls:?}"
        );

        if let Some(id) = running_issue_sweep_id(&reg, 4223) {
            let _ = reg.cancel(&id, Duration::from_secs(2));
        }
    }
}
