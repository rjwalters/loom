//! The dispatch call path: `SweepRegistry::dispatch`, dispatch-backoff
//! bookkeeping, and peer-claim publishing.

#[path = "log_paths.rs"]
mod log_paths;

use super::*;

mod child_env_markers;

/// Issue #3943: print-mode background-task wait ceiling (milliseconds). A
/// daemon-spawned sweep child is a headless `claude -p` session; in print mode
/// the harness reaps still-running background tasks (the sweep's Builder/Judge
/// subagents) after a 600s ceiling. `spawn_child` pins this to `0` (no cap) on
/// the child env so a long role phase runs to completion instead of being
/// killed mid-build.
pub const BG_WAIT_CEILING_ENV: &str = "CLAUDE_CODE_PRINT_BG_WAIT_CEILING_MS";

/// Workspace-relative path of the sweep-owned lease-renewal helper the
/// dispatch path starts for its own `--claim-owned` child (Issue #7672).
pub(crate) const LEASE_RENEW_SCRIPT_REL: &str = ".loom/scripts/sweep-lease-renew.sh";

/// Capability marker exported into every `--claim-owned` child so `sweep.md`'s
/// Step 1a can tell, mechanically, whether the daemon that spawned it starts
/// the lease-renewal loop on its behalf (Issue #7672).
///
/// # Why a marker rather than an unconditional prose withdrawal
///
/// The installed prompt and the daemon binary do **not** roll together. A
/// plain `git pull` (or a `resync-installed.sh` pass) updates
/// `.claude/commands/loom/sweep.md` on a host whose `loom-daemon` binary is
/// only rebuilt by `loom update` — so "new prompt, pre-#7672 daemon" is a real,
/// reachable state, not a hypothetical. If Step 1a simply *stopped* telling the
/// session to start a renewal loop, every sweep dispatched in that window would
/// have no renewal loop at all from either side — reintroducing exactly the
/// stale-lease reclamation this issue exists to prevent, fleet-wide, for the
/// length of the skew.
///
/// Gating the withdrawal on this marker makes all three combinations safe:
///
/// | Prompt | Daemon | Outcome |
/// |---|---|---|
/// | new | ≥ #7672 (marker set) | session skips; the daemon already started it |
/// | new | pre-#7672 (no marker) | session starts it itself — pre-#7672 behavior |
/// | old | ≥ #7672 | session also starts one: a duplicate loop, harmless (an idempotent PATCH of the same comment, one extra call per interval) |
///
/// Set unconditionally at spawn time, so it advertises *this daemon's
/// capability*, not the outcome of the `start` — which has not run yet when
/// the child is spawned, and is best-effort even when it does (see
/// [`SweepRegistry::start_lease_renewal_loop`]).
pub(crate) const LEASE_RENEW_STARTED_ENV: &str = "LOOM_SWEEP_LEASE_RENEW_DISPATCHED";

/// How long [`run_lease_renewal_start`] waits for `sweep-lease-renew.sh start`
/// to return before abandoning (killing) it.
///
/// `start` does **no** network I/O — it resolves a watch PID, forks ONE
/// detached loop, `disown`s it and prints the loop's pid — so it returns in
/// milliseconds. The bound exists only so a pathological helper (a wedged
/// filesystem, a `bash` that never execs) cannot wait forever. That wait runs
/// on its own detached thread ([`SweepRegistry::start_lease_renewal_loop`]),
/// never on the caller's thread, so it holds neither the registry mutex nor a
/// tokio worker for its duration.
const LEASE_RENEW_START_TIMEOUT: Duration = Duration::from_secs(10);

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
/// no-op-cooldown guard (Issue #6670/#6917, step 2.75) refuses a dispatch
/// because a live cooldown window is armed for the target issue.
///
/// [`record_noop_release`](super::SweepRegistry::record_noop_release) (the
/// `RecordNoopRelease` IPC handler, `ipc.rs`) lets a sweep that concluded "no
/// actionable delta this pass" arm a cooldown so the SAME candidate is not
/// immediately re-offered. The tick-based work-finder loop already consults
/// this state before re-selecting a candidate
/// ([`WorkDispatcher::noop_cooldown`](crate::work_finder::WorkDispatcher::noop_cooldown),
/// `work_finder.rs`) — but until this guard existed, every OTHER dispatch
/// route (the IPC/CLI `{"Issue": <N>}` RPC behind `loom-daemon dispatch <N>` /
/// `--claim-owned <N>`, the epic supervisor, and all three watchdogs) funneled
/// through `begin_issue_dispatch` without ever reading it, so a direct
/// re-dispatch could re-claim the same issue within the very cooldown window
/// its last sweep deliberately armed.
///
/// Distinct, downcast-matchable type — same rationale as
/// [`DispatchBackoffError`]: a cooldown refusal is a *deliberate skip*, not a
/// dispatch failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NoopCooldownDispatchError {
    /// The issue whose dispatch was refused.
    pub issue: u32,
    /// Whole seconds remaining before the cooldown window elapses.
    pub retry_after_secs: u64,
}

impl std::fmt::Display for NoopCooldownDispatchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "refusing to dispatch issue #{}: a live no-op-release cooldown is armed, {}s \
             remaining (#6670/#6917 noop-cooldown guard); the last sweep found nothing had \
             changed since its previous check",
            self.issue, self.retry_after_secs
        )
    }
}

impl std::error::Error for NoopCooldownDispatchError {}

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
    /// ([`SweepRegistry::classify_preflip_labels`]) — a peer's
    /// claim label (`loom:building` / `loom:reviewing` / `loom:treating`)
    /// already landed on the forge before this host's own flip. Carries the
    /// observed pre-flip label set for diagnostics; the claim label(s) the
    /// refusal is actually founded on are extracted from it for the message
    /// (Issue #7873 — the absence of `loom:issue` alone never refuses).
    ForgeLabel { labels: Vec<String> },
}

impl std::fmt::Display for CollisionSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CollisionSource::PeerClaim => {
                write!(f, "a peer host's live soft-claim advertisement")
            }
            CollisionSource::ForgeLabel { labels } => {
                f.write_str(&preflip_labels::describe_claim_evidence(labels))
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

/// Issue #6667 (2AMLogic/2am#410): build-cache env vars forwarded to the sweep
/// child under the same explicit-allowlist discipline as
/// [`EXPERIMENT_ENV_ALLOWLIST`] above. Kept as a separate const because the
/// two groups answer to different owners: the experiment names are verified
/// against `loom_tools/sweep_experiment.py`, while these are `sccache`'s own
/// documented variable names plus the AWS SDK's profile selector.
///
/// A fleet host wires these into the *daemon's supervisor* environment
/// (launchd plist / systemd `Environment=`), never a host-global shell
/// profile, so that only daemon-spawned builds share the cache. Forwarding
/// them here is what makes a sweep's `cargo build` reuse compiled objects
/// across worktrees and hosts instead of cold-compiling every time.
///
/// `AWS_PROFILE` names a profile in the host's `~/.aws/credentials`; the key
/// material stays in that file (mode 600, the daemon user's own), and no
/// name on this list carries a secret. That placement is the actual
/// protection, not this list: a `Command` with no `env_clear()` inherits the
/// daemon's whole environment anyway, so a credential exported onto the
/// daemon would reach every sweep child whether or not it appears here.
/// Keep host rollout to `AWS_PROFILE` for that reason.
/// `SCCACHE_SERVER_PORT` is on the list for a non-obvious reason worth
/// stating: `sccache` is a per-user *server*, and whichever process starts it
/// first fixes the storage backend for every later client. An operator who
/// ssh'es in and runs one `cargo build` (or even `sccache --show-stats`)
/// starts a local-disk server on the default port, and every daemon build
/// after that silently gets local disk instead of the shared bucket — a
/// degraded cache that looks identical to a working one. Pinning the daemon
/// to its own port keeps the two servers separate.
pub const BUILD_CACHE_ENV_ALLOWLIST: &[&str] = &[
    "RUSTC_WRAPPER",
    "SCCACHE_BUCKET",
    "SCCACHE_REGION",
    "SCCACHE_SERVER_PORT",
    "AWS_PROFILE",
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

/// One distinct *source* that observed the token pool being unable to satisfy
/// a spawn, for the cross-issue empty-pool brake's distinct-source count
/// (Issue #6614; role-tick variant added by #7607).
///
/// The brake's whole premise is that N **different** sources dying at the same
/// step cannot be explained by any one of them — it can only be the pool. That
/// argument is indifferent to what kind of thing the source is, so widening it
/// from "distinct issues" to "distinct issues **and** distinct `(workspace,
/// role)` role ticks" strengthens the signal without weakening the
/// over-trigger guarantee: one role looping on one workspace refreshes a
/// single key forever and can no more trip the brake alone than one issue
/// cycling through its own #4485 backoff can.
///
/// Why role ticks need their own variant rather than reusing a synthetic issue
/// number: a role tick has no issue, and after #7607 it does not even produce a
/// dispatch — its pre-spawn preflight reads the pool directly and skips, so the
/// `finish_issue_dispatch` path #6614 feeds from is never reached. Without this
/// the fleet-wide advisory would depend entirely on which discovery path
/// happened to see the exhausted pool first.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum TokenSelectionFailureSource {
    /// An issue dispatch that died synchronously in `spawn-claude.sh`'s
    /// token-selection step (the original #6614 feed, via
    /// [`SweepRegistry::finish_issue_dispatch`](crate::sweep_registry::SweepRegistry::finish_issue_dispatch)).
    Issue(u32),
    /// A role-runner tick that skipped its spawn because its pre-spawn
    /// preflight found the resolved pool present but with zero spawnable
    /// accounts (#7607). Keyed by `(workspace root, role)` so each role on
    /// each workspace counts once.
    RoleTick {
        /// The workspace root the role ticks for.
        root: PathBuf,
        /// The role name (`champion`, `curator`, …).
        role: String,
    },
}

impl std::fmt::Display for TokenSelectionFailureSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Issue(n) => write!(f, "issue #{n}"),
            Self::RoleTick { root, role } => write!(f, "role tick {role}@{}", root.display()),
        }
    }
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

/// Why a per-issue dispatch-backoff window (Issue #4485) was most recently
/// (re-)armed. Purely observational metadata riding alongside
/// [`DispatchBackoffState`] — it does not change the doubling/expiry math at
/// all, it only lets a reader distinguish "this issue's *dispatches* keep
/// failing" from "this issue's open-PR guard keeps refusing it" (Issue
/// #7606), so the work-finder can attribute a pre-filtered skip to its own
/// `pr-open-backoff` tick-summary counter instead of the generic
/// `backoff-skip`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DispatchBackoffCause {
    /// A dispatch attempt failed outright (spawn error, token-selection
    /// death, a lost claim-then-verify-order lease race, etc.) — the sole
    /// cause before #7606.
    Generic,
    /// The open-PR guard (#4123, step 2.5/2.6) refused dispatch because the
    /// issue already has a verified open linked PR (#7606).
    OpenPrGuard,
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
    /// What most recently (re-)armed this window (Issue #7606). Purely
    /// observational — see [`DispatchBackoffCause`].
    cause: DispatchBackoffCause,
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
    /// kind this method was never designed to carry. The same applies to
    /// [`peer_claims::ClaimKind::FilingLock`]/`FilingUnlock` (Issue #6714):
    /// those are published by the **filer itself** — the shell half of
    /// [`crate::filing_lock`] (`lib/filing-lock.sh`, via `fleet-send.sh`) at
    /// the instant it takes/releases the machine-wide lock, because a
    /// 30s-cadence reaper republish is far too coarse for a burst that lasts
    /// seconds. The daemon's role in that lane is purely the *receive* side
    /// ([`crate::safehouse::PeerClaimSink`] + its on-disk mirror).
    pub(crate) fn publish_peer_claim(&self, kind: peer_claims::ClaimKind, issue: u32) {
        if kind == peer_claims::ClaimKind::Completed || kind.is_filing_lock_lane() {
            log::warn!(
                "sweep_registry: publish_peer_claim called with a non-dispatch ClaimKind for \
                 issue #{issue} — this is a dispatch-only path (#6352/#6714); dropping"
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
            // Unreachable: the early return above already handles `Completed`
            // and the filing-lock lane. The cooldown lane (Issue #7477) has
            // its own dedicated publisher, `publish_peer_cooldown_claim`,
            // since it carries a `remaining_secs` payload this method's
            // signature has no parameter for.
            peer_claims::ClaimKind::Completed
            | peer_claims::ClaimKind::FilingLock
            | peer_claims::ClaimKind::FilingUnlock
            | peer_claims::ClaimKind::NoopCooldownArmed
            | peer_claims::ClaimKind::DispatchBackoffArmed => return,
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

    /// Publish a fleet-wide cooldown/backoff advertisement (Issue #7477) —
    /// the [`Self::publish_peer_claim`] sibling for
    /// [`peer_claims::ClaimKind::NoopCooldownArmed`]/
    /// [`peer_claims::ClaimKind::DispatchBackoffArmed`], reusing the exact
    /// same outbound channel and fail-open contract: a no-op without a
    /// publisher (`safehouse.enabled` false), and a full/closed channel
    /// drops the ad without blocking the caller.
    ///
    /// One-shot, not re-advertised: unlike a live sweep's in-flight claim
    /// (refreshed every reaper tick by [`Self::readvertise_peer_claims`]), a
    /// cooldown/backoff window is armed once per record call and its
    /// receiver computes its own local expiry from `remaining` — there is no
    /// ongoing local state that needs a heartbeat to stay live. A repeat
    /// `record_noop_release`/`record_dispatch_failure` call (each pass that
    /// still finds "nothing to do") naturally re-broadcasts and refreshes
    /// every peer's local expiry, mirroring the local re-arm semantics
    /// exactly.
    pub(crate) fn publish_peer_cooldown_claim(
        &self,
        kind: peer_claims::ClaimKind,
        issue: u32,
        remaining: Duration,
    ) {
        let Some(tx) = &self.peer_claim_publisher else {
            return;
        };
        let repo = peer_claims::repo_slug(&self.config.workspace_root);
        let host = host_identity();
        let pid = std::process::id();
        let ts = Utc::now().to_rfc3339();
        let remaining_secs = remaining.as_secs();
        let ad = match kind {
            peer_claims::ClaimKind::NoopCooldownArmed => {
                ClaimAd::noop_cooldown_armed(issue, repo, host, pid, ts, remaining_secs)
            }
            peer_claims::ClaimKind::DispatchBackoffArmed => {
                ClaimAd::dispatch_backoff_armed(issue, repo, host, pid, ts, remaining_secs)
            }
            // Unreachable: every call site below passes one of the two kinds
            // above.
            _ => {
                log::warn!(
                    "sweep_registry: publish_peer_cooldown_claim called with a non-cooldown-lane \
                     kind for issue #{issue} — this is a cooldown/backoff-only path (#7477); \
                     dropping"
                );
                return;
            }
        };
        if let Err(e) = tx.try_send(ad) {
            // Fail-open, mirroring `publish_peer_claim`: the fleet-wide
            // broadcast is an optimization on top of the still-correct local
            // cooldown/backoff, never a liveness dependency.
            log::debug!(
                "sweep_registry: cooldown/backoff advertisement for issue #{issue} dropped \
                 ({e}); local cooldown/backoff unaffected (#7477)"
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
        self.record_dispatch_backoff(issue, DispatchBackoffCause::Generic);
    }

    /// Arm `issue`'s #4485 backoff ladder because the **open-PR guard**
    /// (#4123, step 2.5/2.6) refused it, not because a dispatch attempt
    /// itself failed (Issue #7606).
    ///
    /// Every in-memory dedup signal a guarded issue relies on dies with its
    /// own sweep, so — absent this — the SAME guarded issue is re-submitted
    /// to `dispatch()` (and re-probed, or re-served from the #6788 memo) on
    /// every work-finder tick for as long as its PR stays open. Reusing the
    /// existing ladder rather than inventing a second one means: the same
    /// exponential growth (60s -> 120 -> 240 -> 480 -> 900), the same
    /// pre-`dispatch()` work-finder short-circuit
    /// ([`WorkDispatcher::backed_off`](crate::work_finder::WorkDispatcher::backed_off)),
    /// and — because [`DispatchBackoffConfig::max`] defaults to 900s, the same
    /// as [`super::guards::OPEN_PR_MEMO_FRESH`] — a cap that never outlives
    /// the memo's own freshness window on default config.
    ///
    /// Tagged [`DispatchBackoffCause::OpenPrGuard`] (purely observational) so
    /// the work-finder can attribute the pre-filtered skip this arms to its
    /// own `pr-open-backoff` tick-summary counter instead of the generic
    /// `backoff-skip`.
    pub(crate) fn record_open_pr_guard_backoff(&mut self, issue: u32) {
        self.record_dispatch_backoff(issue, DispatchBackoffCause::OpenPrGuard);
    }

    /// Shared implementation behind [`Self::record_dispatch_failure`] and
    /// [`Self::record_open_pr_guard_backoff`] (Issue #7606) — identical
    /// doubling/expiry math regardless of `cause`; only the stamped
    /// [`DispatchBackoffCause`] differs.
    fn record_dispatch_backoff(&mut self, issue: u32, cause: DispatchBackoffCause) {
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
                cause,
            },
        );
        log::info!(
            "sweep_registry: issue #{issue} dispatch backoff armed — {consecutive} consecutive \
             failed dispatch(es), next attempt allowed in {}s (#4485; cause={cause:?})",
            delay.as_secs()
        );
        // Issue #7477: broadcast the armed window fleet-wide so a peer host
        // does not immediately re-attempt the same failing candidate this
        // host just backed off on — see `publish_peer_cooldown_claim`'s doc
        // comment for why this is one-shot rather than re-advertised.
        self.publish_peer_cooldown_claim(
            peer_claims::ClaimKind::DispatchBackoffArmed,
            issue,
            delay,
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
    ///
    /// Fleet-wide as of Issue #7477: unions this host's own local backoff
    /// state with any live backoff window a **peer** host has broadcast (see
    /// [`Self::fleet_dispatch_backoff_issues`]) — the fix for the fleet-scope
    /// gap that let a multi-host fleet round-robin a claim/release bail loop
    /// faster than a single-host backoff was designed to prevent.
    #[must_use]
    pub fn dispatch_backoff_issues(&self, now: DateTime<Utc>) -> HashSet<u32> {
        if !self.dispatch_backoff_config.enabled {
            return HashSet::new();
        }
        let mut set: HashSet<u32> = self
            .dispatch_backoff
            .iter()
            .filter(|(_, s)| s.until > now)
            .map(|(issue, _)| *issue)
            .collect();
        set.extend(self.fleet_dispatch_backoff_issues());
        set
    }

    /// The subset of [`Self::dispatch_backoff_issues`] whose CURRENT
    /// (unexpired) window was armed by the open-PR guard rather than a
    /// generic dispatch failure (Issue #7606) — what the work-finder attributes
    /// to its `pr-open-backoff` tick-summary counter instead of the generic
    /// `backoff-skip` when a candidate is filtered out before `dispatch()` is
    /// even called.
    ///
    /// This host's own local backoff state only — unlike
    /// [`Self::dispatch_backoff_issues`], it does NOT union in a peer host's
    /// fleet-broadcast window: [`peer_claims::ClaimKind::DispatchBackoffArmed`]
    /// does not carry a cause, so a peer-armed window's true cause is unknown
    /// here. Undercounting `pr-open-backoff` for a peer-armed window is a
    /// purely cosmetic gap (the peer's own host still counts it correctly, and
    /// the peer-armed window still refuses dispatch via `dispatch_backoff_issues`
    /// either way) — never a correctness one.
    #[must_use]
    pub fn open_pr_backoff_issues(&self, now: DateTime<Utc>) -> HashSet<u32> {
        if !self.dispatch_backoff_config.enabled {
            return HashSet::new();
        }
        self.dispatch_backoff
            .iter()
            .filter(|(_, s)| s.until > now && s.cause == DispatchBackoffCause::OpenPrGuard)
            .map(|(issue, _)| *issue)
            .collect()
    }

    /// Issues with a live fleet-wide dispatch-backoff window armed by a
    /// **peer** host (Issue #7477) — empty when no peer-claim view is
    /// attached (`safehouse.enabled` false), mirroring
    /// [`Self::peer_claimed_issues`]'s disabled-state contract.
    #[must_use]
    fn fleet_dispatch_backoff_issues(&self) -> HashSet<u32> {
        let Some(view) = &self.peer_claims else {
            return HashSet::new();
        };
        let repo = peer_claims::repo_slug(&self.config.workspace_root);
        match view.lock() {
            Ok(v) => v.dispatch_backoff_issues_at(&repo, Instant::now()),
            Err(poisoned) => {
                log::error!("sweep_registry: peer-claim view mutex poisoned ({poisoned:?})");
                HashSet::new()
            }
        }
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
        self.record_token_selection_failure_from(&TokenSelectionFailureSource::Issue(issue));
    }

    /// Record that a role-runner tick for `role` on this registry's workspace
    /// skipped its spawn because the resolved token pool was present but had
    /// zero spawnable accounts — the **role-tick** feed into the same #6614
    /// brake (Issue #7607).
    ///
    /// Called from the role runner's pre-spawn preflight, which after #7607
    /// never spawns (and therefore never produces a dispatch
    /// `finish_issue_dispatch` could observe) once it has read the pool as
    /// exhausted. Without this feed the fleet-wide advisory would depend on
    /// whether a sweep or a role happened to notice the exhausted pool first:
    /// on a host whose work finder is idle and whose role loops are the only
    /// traffic, it would simply never trip.
    ///
    /// Recording the same `(root, role)` twice refreshes its timestamp but does
    /// not advance the distinct-source count — see
    /// [`TokenSelectionFailureSource`] for why that keeps #6614's over-trigger
    /// guarantee intact.
    pub fn record_role_tick_pool_exhausted(&mut self, role: &str) {
        let source = TokenSelectionFailureSource::RoleTick {
            root: self.config.workspace_root.clone(),
            role: role.to_string(),
        };
        self.record_token_selection_failure_from(&source);
    }

    /// Shared body of the two #6614 feeds (issue dispatch, #7607 role tick):
    /// prune the trailing window, record/refresh this source's timestamp, and
    /// re-evaluate the workspace pre-flight advisory — so crossing the
    /// distinct-source threshold trips the existing #4386/#5030 hold +
    /// `PreflightAdvisory` event rather than a second, parallel breaker.
    fn record_token_selection_failure_from(&mut self, source: &TokenSelectionFailureSource) {
        let now = Utc::now();
        let window = chrono::Duration::seconds(resolve_empty_pool_breaker_window_secs());
        self.token_selection_failures
            .retain(|_, at| now - *at <= window);
        self.token_selection_failures.insert(source.clone(), now);
        let distinct = self.token_selection_failures.len();
        let threshold = resolve_empty_pool_breaker_threshold();
        log::warn!(
            "sweep_registry: {source} could not obtain a token at selection (empty/unusable token \
             pool) — {distinct} distinct source(s) in the last {}s have now hit the same wall \
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

    /// How many DISTINCT sources ([`TokenSelectionFailureSource`]: issue
    /// dispatches, plus role ticks since #7607) hit an unsatisfiable token
    /// selection inside the trailing window at `now` (Issue #6614).
    /// Side-effect-free: stale entries are
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
    /// or mismatched caller can never silently disable the guard. The bypass
    /// (`resume_bypass_pr: Some(_)`, threaded through to
    /// [`Self::begin_issue_dispatch`]) is **only** reachable from
    /// [`Self::reap_once`]'s own dispatch decision — either via this
    /// synchronous wrapper (direct/test callers, e.g.
    /// `reaper_resume_dispatch_bypasses_the_backoff`) or, in production,
    /// via `reap_once_impl`'s own inlined `begin_issue_dispatch` call
    /// (Issue #6691, which needs the intermediate [`BeginIssueDispatch`]
    /// value to defer the poll — see `reaper.rs::reap_once_releasing_poll_lock`).
    /// No other call site (work-finder, IPC/CLI dispatch, epic supervisor,
    /// watchdogs) can pass a bypass, so the anti-duplicate property of
    /// #4123 is unchanged for every other dispatch path.
    ///
    /// Kept as this fully synchronous, self-contained wrapper — unlike its
    /// production reaper call site, this composes `dispatch_inner` end to
    /// end and so holds `&mut self` (and, behind a `Mutex`, the lock)
    /// across the whole account-selection poll — for direct callers with no
    /// `Arc<Mutex<Self>>` to release, chiefly unit tests exercising the
    /// resume decision in isolation.
    pub(crate) fn dispatch_resume_after_crash(
        &mut self,
        issue: u32,
        resume_pr: u32,
    ) -> Result<DispatchOutcome> {
        self.dispatch_inner(
            &SweepKind::Issue(issue),
            None,
            DispatchModel::Resolved(None),
            None,
            None,
            Some(resume_pr),
        )
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
        model: DispatchModel<'_>,
        effort: Option<&str>,
        depends_on: Option<u32>,
        resume_bypass_pr: Option<u32>,
    ) -> Result<DispatchOutcome> {
        match self.begin_issue_dispatch_with_model(
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
    pub(crate) fn begin_issue_dispatch_with_model(
        &mut self,
        kind: &SweepKind,
        idempotency_key: Option<String>,
        model: DispatchModel<'_>,
        effort: Option<&str>,
        depends_on: Option<u32>,
        resume_bypass_pr: Option<u32>,
    ) -> Result<BeginIssueDispatch> {
        // Runtime admission is deliberately the first dispatch decision:
        // before idempotency/account selection, claim lock, forge mutation,
        // log header, or child spawn. A full sweep remains one runtime and is
        // checked against Builder's (strongest lifecycle) requirements.
        let admission = if self.config.skip_label_flip {
            // hermetic unit fixtures do not install runtime manifests
            crate::runtime_preference::DispatchAdmission::none()
        } else {
            // The admitted runtime and optional backstop reservation travel
            // together through begin/poll/finish; early returns release the slot.
            match crate::runtime_preference::resolve_for_dispatch(
                &self.config.workspace_root,
                "sweep-lifecycle",
                None,
            ) {
                Ok(admission) => admission,
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

        // Resolve implicit defaults/experiments only after the ONE runtime
        // admission above; explicit pins remain explicit even after fallback.
        let resolved_model = model.resolve(&self.config, kind, admission.admitted.as_ref());
        let model = resolved_model.as_deref();

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
                    admission,
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
        //
        //     Issue #7606: before spending THIS REST round trip, consult the
        //     2.6 guard's own verified-open-PR memo (#6788,
        //     `fresh_open_pr_memo`) — a still-fresh entry means a *previous*
        //     dispatch attempt already verified an open linked PR for this
        //     issue, so 2.6 would refuse it again regardless of what this
        //     probe answers. Refusing right here instead skips BOTH this
        //     REST call and 2.6's own probe attempt, at zero forge cost — the
        //     memo lookup is a pure in-memory read. Fail-open is unaffected:
        //     `fresh_open_pr_memo` returns `None` on a missing/expired/disabled
        //     memo, which falls straight through to the unchanged 2.5/2.6
        //     checks below, so a memo MISS can never be mistaken for a
        //     verified refusal. The refusal also arms this issue's #4485
        //     backoff ladder (`record_open_pr_guard_backoff`) so a
        //     work-finder-driven re-dispatch is filtered out via
        //     `WorkDispatcher::backed_off` before `dispatch()` (and this
        //     REST call) is even attempted again — see that method's doc
        //     comment for the full rationale.
        if !self.config.skip_label_flip {
            if let Some(memo) = self.fresh_open_pr_memo(issue_number, Utc::now()) {
                if resume_bypass_pr != Some(memo.pr) {
                    self.record_open_pr_guard_backoff(issue_number);
                    return Err(OpenPrDispatchError {
                        issue: issue_number,
                        pr: memo.pr,
                    }
                    .into());
                }
            }

            if self.issue_is_closed_or_pr(issue_number) == Some(true) {
                return Err(anyhow!(
                    "refusing to dispatch issue #{issue_number}: it is closed on the forge, or \
                     the number resolves to a pull request rather than an open issue (#4088/#4504 \
                     closed-issue guard). A watchdog re-dispatch must not re-claim a closed/merged \
                     issue or a PR number."
                ));
            }
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
        //     Issue #4256: `resume_bypass_pr` — set only via
        //     `dispatch_resume_after_crash` (direct/test callers) or
        //     `reap_once_impl`'s own inlined `begin_issue_dispatch` call
        //     (production, Issue #6691) — exempts a resume of THIS issue's own
        //     crashed sweep from the guard, but only when it names the exact PR
        //     the guard would find; any other PR (or none) still refuses
        //     normally, so a stale/mismatched resume can never widen into a
        //     blanket bypass. Both call sites trace back to `Self::reap_once`
        //     (directly, or via its lock-releasing sibling
        //     `reap_once_releasing_poll_lock`); no other caller can set it.
        if !self.config.skip_label_flip {
            // Fail-open (#4452): only a VERIFIED `Open(pr)` blocks; both
            // `NoneOpen` and `ProbeFailed` fall through and proceed, so a forge
            // outage can never wedge dispatch (unchanged pre-#4452 behavior).
            if let OpenPrProbe::Open(pr) = self.probe_open_linked_pr(issue_number) {
                if resume_bypass_pr != Some(pr) {
                    // Issue #7606: a verified refusal here is exactly the
                    // same event the 2.5-position memo short-circuit above
                    // arms the ladder for — see
                    // `record_open_pr_guard_backoff`'s doc comment.
                    self.record_open_pr_guard_backoff(issue_number);
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

        // 2.75 Noop-cooldown guard (Issue #6917, follow-up to #6670/#6740).
        //      `record_noop_release` (`noop_cooldown.rs`, exposed over IPC as
        //      `RecordNoopRelease`) lets a sweep that concluded "no
        //      actionable delta this pass" arm a cooldown so the work finder
        //      does not immediately re-offer the same candidate. The
        //      tick-based work-finder loop already consults this state
        //      before re-selecting a candidate (`work_finder.rs`,
        //      `WorkDispatcher::noop_cooldown`) — but every OTHER dispatch
        //      route funnels through THIS method (the IPC/CLI
        //      `{"Issue": <N>}` RPC behind `loom-daemon dispatch <N>` /
        //      `--claim-owned <N>`, the epic supervisor, and all three
        //      watchdogs) without ever reading it, so a direct re-dispatch
        //      could re-claim the same issue within the very cooldown window
        //      its last sweep deliberately armed.
        //
        //      `noop_cooldown_remaining` is a pure in-memory lookup with no
        //      `gh` dependency — like the 2.8 backoff guard immediately
        //      below, this is NOT gated on `skip_label_flip`, and a refusal
        //      costs no lock, no label write, and no forge round trip.
        //
        //      Placed AFTER the 2.7 park-label guard, deliberately: when an
        //      issue is both parked and mid-cooldown, the park is the more
        //      actionable, durable operator decision and must win — this
        //      guard never runs for an issue 2.7 already refused, so guard
        //      ordering here cannot change that outcome.
        //
        //      Not exempted by `resume_bypass_pr`, mirroring 2.7's own
        //      rationale: a cooldown armed by the issue's own last (possibly
        //      since-crashed) sweep still means "nothing has changed since
        //      that check" and a resume must respect it exactly like a fresh
        //      dispatch would.
        if let Some(remaining) = self.noop_cooldown_remaining(issue_number, Utc::now()) {
            log::info!(
                "sweep_registry: refusing to dispatch issue #{issue_number} — a live no-op \
                 release cooldown is armed, {}s remaining (#6670/#6917 noop-cooldown guard); \
                 the last sweep found nothing had changed since its previous check.",
                remaining.as_secs()
            );
            return Err(NoopCooldownDispatchError {
                issue: issue_number,
                retry_after_secs: remaining.as_secs(),
            }
            .into());
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
            admission.admitted.as_ref(),
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
            admission,
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
            mut admission,
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
        // Issue #8555: hand this dispatch's metered backstop slot (the one
        // `resolve_for_dispatch` took back in `begin_issue_dispatch`, carried
        // here on `PreparedIssueDispatch`) to the child that will spend it, so
        // the per-host ceiling counts live sweeps rather than resolutions.
        // Placed AFTER the #4689 preflight-death branch above, which returns
        // `Err` for a child that is already dead: attaching there would pin a
        // slot to a pid that no longer exists — and returning there drops the
        // reservation, which releases it. A no-op when no ceiling is configured
        // or the walk never fell through to a governed tap — which is every
        // pre-#8555 fleet.
        crate::runtime_preference::handoff::attach(admission.backstop.take(), pid);
        // Issue #4980: capture the child's process group NOW, while it is alive
        // — `getpgid` cannot answer for a dead pid, so a group handle acquired
        // any later is unavailable in exactly the crash case that needs it most.
        let pgid = spawned_leader_pgid(pid);

        // Issue #7672: hand the just-spawned `--claim-owned` child its lease
        // renewal loop, from dispatch code — `sweep.md`'s Step 1a used to ask
        // the spawned SESSION to run this command, and one session skipping it
        // cost the fleet 2.5h of claim/yield thrash plus a near-miss
        // double-claim on a shared worktree. See
        // [`Self::start_lease_renewal_loop`] for why this is not the
        // daemon-owned renewal #6129 forbids (the loop is detached and watches
        // `pid`, not this daemon).
        //
        // Placed HERE rather than beside the `Command::spawn()` in
        // `begin_issue_dispatch` for one reason: the #4689 preflight-death
        // branch immediately above returns `Err` and unwinds the whole claim
        // for a child that is already dead. Starting the loop before that
        // check would aim it at a pid that no longer exists. The cost is a
        // bounded gap (the account-selection poll, <= `TOKEN_NAME_CAPTURE_
        // TIMEOUT`) between the lease being written and the loop starting —
        // during which the lease comment is seconds old and nowhere near any
        // reclamation TTL. Both are still ONE dispatch call: there is no
        // deferred tick a daemon restart could drop on the floor.
        //
        // The returned `JoinHandle` is deliberately dropped: the `start`
        // handshake runs on its own thread precisely so this lock-scoped
        // function stays O(1) (see `start_lease_renewal_loop`'s "Why the
        // `start` handshake runs on its own thread"), and nothing in this
        // dispatch depends on its result — it is best-effort in both
        // directions.
        if matches!(kind, SweepKind::Issue(_)) {
            drop(self.start_lease_renewal_loop(issue_number, &sweep_id, pid, &log_path));
        }

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
        //
        // The same write persists the dispatched model/effort (#8056) so a
        // post-restart `reconstruct()` can restore them onto the adopted entry
        // — otherwise every `sweep.outcome` telemetry record written after a
        // restart reports a null model and effort for a sweep that had both.
        if let Err(e) = self.record_child_pid_in_lock(
            issue_number,
            pid,
            pgid,
            model.as_deref(),
            effort.as_deref(),
        ) {
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
            runtime_source: admission.admitted.as_ref().map(|a| a.source.clone()),
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
            runtime: admission.admitted.as_ref().map(|a| a.runtime.clone()),
            runtime_source: admission.admitted.as_ref().map(|a| a.source.clone()),
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
        mut admission: crate::runtime_preference::DispatchAdmission,
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
            admission.admitted.as_ref(),
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
        // #8555: hand this dispatch's metered backstop slot (the one
        // `resolve_for_dispatch` took, carried down from `begin_issue_dispatch`)
        // to the child that will spend it. Every `return` above drops it, which
        // releases it — a refused PR-set dispatch holds nothing.
        crate::runtime_preference::handoff::attach(admission.backstop.take(), pid);
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
            runtime_source: admission.admitted.as_ref().map(|a| a.source.clone()),
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
            runtime: admission.admitted.as_ref().map(|a| a.runtime.clone()),
            runtime_source: admission.admitted.as_ref().map(|a| a.source.clone()),
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
        // The two `Issue`-scoped child markers — `LOOM_SWEEP_CLAIM_OWNED`
        // (#3823/#4111/#5342) and the lease-renewal capability marker
        // (#7672) — are set for an `Issue` dispatch and *cleared* for a
        // `PrSet` one. Both the rationale and the clearing (#7915) live in
        // [`child_env_markers::apply_issue_scoped_markers`].
        child_env_markers::apply_issue_scoped_markers(&mut cmd, kind);
        crate::observability::tracing::prepare_child(
            &mut cmd,
            &self.config.workspace_root,
            sweep_id,
        );
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
        // no-op for a single-owner fleet or the root owner's own repos.
        //
        // #6722: the plain lookup (`apply_gh_config_for_root`) is a pure
        // registry read, so it is also a silent no-op for a cross-owner root
        // that genuinely needs a per-owner credential but was never
        // registered — `daemon_service.rs`'s registration pass runs exactly
        // ONCE, at startup, so a workspace root added to an already-running
        // daemon (`loom-daemon workspace add`) is invisible to it, and the
        // spawn path here has no `gh` call of its own to 404 on and trigger
        // the existing `forge_listing.rs`-only reactive recovery. Use the
        // recovery-aware wrapper instead, which attempts one eager mint
        // before falling through when the registry misses AND the root's
        // owner genuinely differs from this daemon's own — see
        // `apply_gh_config_for_root_with_recovery`'s doc comment.
        crate::credential_preflight::apply_gh_config_for_root_with_recovery(
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
        //
        // Issue #6667 adds the build-cache group on the same terms: a fleet
        // host sets them on the daemon's supervisor (launchd/systemd), and a
        // sweep's `cargo build` reuses S3-cached objects instead of
        // cold-compiling the workspace in every fresh worktree. Forwarding is
        // explicit here rather than left to plain process inheritance so the
        // guarantee survives any future `env_clear()` on this `Command` — and
        // so the set of names that may cross into a sweep child stays
        // reviewable in one place.
        for name in EXPERIMENT_ENV_ALLOWLIST
            .iter()
            .chain(BUILD_CACHE_ENV_ALLOWLIST.iter())
        {
            if let Some(val) = std::env::var_os(name) {
                if !val.is_empty() {
                    cmd.env(name, val);
                }
            }
        }

        let child = crate::observability::lifecycle::spawn_child(
            &mut cmd,
            &self.config.workspace_root,
            sweep_id,
        )
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

    // ------------------------------------------------------------------------
    // Lease renewal hand-off (Issue #7672)
    // ------------------------------------------------------------------------

    /// Start the sweep-owned lease-renewal loop for a `--claim-owned` child
    /// this dispatch just spawned (Issue #7672) — **once**, synchronously,
    /// from dispatch code rather than from the spawned session's own prose.
    ///
    /// # Why the daemon issues the `start`, and why that is not #6129
    ///
    /// Epic #6165 gives a `loom:building` claim a liveness dimension: the
    /// dispatch writes a lease comment ([`Self::write_lease_comment`], #6179)
    /// and *something* must keep re-touching it, or a peer host's reclamation
    /// gate (#6286) correctly concludes the claim is dead and reclaims live
    /// work. Until this issue, that "something" was `sweep.md`'s **Step 1a**:
    /// prose instructing the spawned LLM session to run
    /// `sweep-lease-renew.sh start "$N"` itself. One session skipping that one
    /// step produced ~25 claim/yield cycles over 2.5h and a near-miss
    /// double-claim on a shared worktree (2AMLogic/klayout-tools#1658) — a
    /// mechanism that only works when a model remembers a sentence is not a
    /// mechanism. Issuing the `start` here makes it structural.
    ///
    /// This does **not** make the daemon the renewer, which is the hazard
    /// `sweep-lease-renew.sh`'s own header warns about (#6129: role agents run
    /// as transient `systemd --user` scopes and routinely outlive the daemon
    /// that spawned them, so daemon-owned renewal would let a live sweep's
    /// lease expire across an ordinary daemon restart). Ownership of *renewal*
    /// still sits with the work process:
    ///
    /// - `start` forks ONE loop, `disown`s it, and returns — the loop is a
    ///   fully detached process, not a child this daemon supervises. Nothing
    ///   in this registry tracks, ticks, or waits on it.
    /// - `--watch-pid <child_pid>` pins the loop's lifetime to the **sweep
    ///   child's** pid (overriding `resolve_liveness_pid`'s ancestor walk,
    ///   which exists for the in-session caller that has no such handle). The
    ///   loop stops when the sweep stops — never when the daemon does.
    ///
    /// So the only thing that moved into the daemon is the one-shot
    /// *invocation*. A daemon restart one second later leaves the loop running
    /// untouched (it is not in this daemon's supervision tree, and
    /// [`process_group(0)`](std::os::unix::process::CommandExt::process_group)
    /// below also keeps a process-group-targeted teardown of the daemon — e.g.
    /// launchd stopping its job — from reaching it).
    ///
    /// # Why the `start` handshake runs on its own thread
    ///
    /// Every caller of [`Self::finish_issue_dispatch`] — `ipc.rs`'s
    /// `dispatch_sweep_nonblocking` Phase 3, `dispatch_model_releasing_poll_lock`'s
    /// Phase 3, `dispatch_inner`, and the reaper's resume path — invokes it
    /// **holding the registry's `Arc<Mutex<SweepRegistry>>`**, and the first two
    /// do so directly on a tokio worker thread. So this method must be O(1) on
    /// the calling thread: it reads what it needs out of `self` (both cheap,
    /// in-memory) and hands the subprocess spawn plus the bounded
    /// `LEASE_RENEW_START_TIMEOUT` wait to a detached OS thread. In the normal
    /// case that handshake is sub-millisecond, but a pathological helper (a
    /// wedged filesystem, a `bash` that never execs) would otherwise pin the
    /// *global* registry mutex for up to 10s on every `Issue` dispatch —
    /// starving `list_sweeps`, `cancel`, and every concurrent dispatch behind
    /// it. That is the same hazard class the #6592/#7307 split moved out from
    /// under the lock for the account-selection poll
    /// ([`poll_and_classify_spawned_child`], run via `spawn_blocking` with the
    /// lock released); doing it here inside the registry method covers all four
    /// call sites at once rather than one caller's phase.
    ///
    /// Returns the thread's [`JoinHandle`](std::thread::JoinHandle) (yielding
    /// the detached loop's pid, or `None`) purely so tests can wait for the
    /// handshake deterministically. Production drops it: the thread is
    /// fire-and-forget, exactly like the loop it starts.
    ///
    /// # Best-effort, exactly like the lease write it renews
    ///
    /// Every failure mode (no helper script in this workspace, a non-zero
    /// `start`, a spawn error, even a failure to spawn the thread) only logs: a
    /// dispatch must never fail because the lease it documents could not be
    /// kept fresh. No loop degrades to exactly the pre-#7672 behavior for that
    /// sweep — the lease ages out — so this is strictly additive to the claim's
    /// safety.
    pub(crate) fn start_lease_renewal_loop(
        &self,
        issue: u32,
        sweep_id: &str,
        child_pid: u32,
        log_path: &Path,
    ) -> Option<std::thread::JoinHandle<Option<u32>>> {
        // Captured here, under the caller's lock, because both are pure
        // in-memory reads of this registry's own state: the workspace root is a
        // `PathBuf` clone, and `published_host_id()` is a hostname read plus a
        // hash. Everything that can block — the `is_file()` stat, the spawn,
        // the wait — lives in the thread body below.
        let workspace_root = self.config.workspace_root.clone();
        let host = self.published_host_id();
        let sweep_id = sweep_id.to_string();
        let log_path = log_path.to_path_buf();
        match std::thread::Builder::new()
            .name(format!("lease-renew-start-{issue}"))
            .spawn(move || {
                run_lease_renewal_start(
                    issue,
                    &sweep_id,
                    child_pid,
                    &log_path,
                    &workspace_root,
                    &host,
                )
            }) {
            Ok(handle) => Some(handle),
            Err(e) => {
                log::warn!(
                    "sweep_registry: could not spawn the lease-renewal start thread for issue \
                     #{issue} (#7672, best-effort — dispatch continues, the lease will age \
                     out): {e}"
                );
                None
            }
        }
    }
}

/// The blocking half of [`SweepRegistry::start_lease_renewal_loop`]: spawn
/// `sweep-lease-renew.sh start` and wait (bounded by
/// [`LEASE_RENEW_START_TIMEOUT`]) for its one-shot handshake to return the
/// detached loop's pid.
///
/// Free function, and deliberately takes no `&SweepRegistry`: it runs on a
/// detached thread *after* the registry mutex has been left behind, so it must
/// not be able to touch registry state at all. Returns the loop's pid when one
/// was started, `None` otherwise; every failure mode only logs (see the caller's
/// best-effort contract).
///
/// `stderr` is pointed at the sweep's own log file rather than discarded:
/// `start` dups its stderr onto fd 9 for the detached loop (#6541), so a
/// renewal failure *mid-sweep* lands in the same log an operator already reads
/// for that sweep. It must be a **file**, never a pipe — the loop holds its
/// inherited copy open for the sweep's whole lifetime, so a piped stderr would
/// make any read-to-EOF wait here hang for hours.
fn run_lease_renewal_start(
    issue: u32,
    sweep_id: &str,
    child_pid: u32,
    log_path: &Path,
    workspace_root: &Path,
    host: &str,
) -> Option<u32> {
    let script = workspace_root.join(LEASE_RENEW_SCRIPT_REL);
    if !script.is_file() {
        log::debug!(
            "sweep_registry: no {LEASE_RENEW_SCRIPT_REL} in {} — skipping the dispatch-time \
             lease-renewal start for issue #{issue} (#7672); the lease will age out as it \
             did before this hand-off existed",
            workspace_root.display()
        );
        return None;
    }
    let mut cmd = Command::new(&script);
    cmd.arg("start")
        .arg(issue.to_string())
        .arg("--watch-pid")
        .arg(child_pid.to_string())
        // Exact-match targeting (#6485): without BOTH of these the loop
        // falls back to "newest lease wins" and can spend the sweep
        // renewing a PEER dispatcher's lease comment while this claim's
        // own `updated_at` never advances. The daemon knows both values
        // exactly — it published them itself in `write_lease_comment`.
        .arg("--host")
        .arg(host)
        .arg("--sweep-id")
        .arg(sweep_id)
        // Same workspace every other forge mutation in this registry runs
        // in, so `gh` resolves this repo in a multi-workspace daemon
        // (#3928/#3937).
        .current_dir(workspace_root)
        .stdin(Stdio::null())
        // Piped and read below purely to capture the loop pid `start`
        // prints. Safe to read to EOF: the detached loop redirects its OWN
        // stdout to /dev/null, so nothing holds this pipe open past
        // `start`'s return.
        .stdout(Stdio::piped());
    match std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path)
    {
        Ok(f) => {
            cmd.stderr(Stdio::from(f));
        }
        Err(e) => {
            log::debug!(
                "sweep_registry: could not open {} for the lease-renewal loop's stderr \
                 ({e}); discarding it instead (#7672)",
                log_path.display()
            );
            cmd.stderr(Stdio::null());
        }
    }
    // Its own process group, for the same reason the sweep child gets one
    // (#3800/#4980) and for one more: a supervisor that tears the daemon
    // down by process group must not take the renewal loop of a still-live
    // sweep with it.
    #[cfg(unix)]
    cmd.process_group(0);
    // #5401: a cross-owner managed repo needs its own owner's
    // installation-token `GH_CONFIG_DIR` for the `gh api` PATCHes the loop
    // makes — a no-op for single-owner fleets / the root owner.
    crate::credential_preflight::apply_gh_config_for_root(&mut cmd, workspace_root);

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            log::warn!(
                "sweep_registry: failed to start the lease-renewal loop for issue #{issue} \
                 sweep_id={sweep_id} (#7672, best-effort — dispatch continues, the lease \
                 will age out): {e}"
            );
            return None;
        }
    };
    let deadline = Instant::now() + LEASE_RENEW_START_TIMEOUT;
    let output = loop {
        match child.try_wait() {
            Ok(Some(_)) => break child.wait_with_output().ok(),
            Ok(None) => {}
            Err(e) => {
                log::warn!(
                    "sweep_registry: lease-renewal start for issue #{issue} could not be \
                     waited on (#7672): {e}"
                );
                break None;
            }
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            log::warn!(
                "sweep_registry: lease-renewal start for issue #{issue} exceeded {}s and was \
                 killed (#7672) — the claim keeps its label but its lease will age out",
                LEASE_RENEW_START_TIMEOUT.as_secs()
            );
            break None;
        }
        std::thread::sleep(REAP_GH_POLL_INTERVAL);
    };

    let output = output?;
    if !output.status.success() {
        log::warn!(
            "sweep_registry: lease-renewal start for issue #{issue} sweep_id={sweep_id} \
             exited {:?} (#7672, best-effort — see the sweep log for the helper's own \
             stderr)",
            output.status.code()
        );
        return None;
    }
    let loop_pid = String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse::<u32>()
        .ok();
    match loop_pid {
        Some(p) => log::info!(
            "sweep_registry: issue #{issue} sweep_id={sweep_id} — started detached lease \
             renewal loop pid {p} watching child pid {child_pid} (#7672; the loop outlives \
             this daemon and stops with the sweep, never with the daemon)"
        ),
        None => log::debug!(
            "sweep_registry: lease-renewal start for issue #{issue} succeeded but printed no \
             loop pid (#7672)"
        ),
    }
    loop_pid
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

/// Synchronous, lock-releasing composition of
/// [`SweepRegistry::begin_issue_dispatch`] -> [`poll_and_classify_spawned_child`]
/// -> [`SweepRegistry::finish_issue_dispatch`] (Issue #6688), extending the
/// #6592 split — proven for the IPC `DispatchSweep` handler
/// (`ipc.rs::dispatch_sweep_nonblocking`) — to a caller with no `async`
/// context of its own: the work-finder's
/// [`crate::work_finder::RegistryDispatcher::dispatch`] (`WorkDispatcher::dispatch`
/// is a synchronous trait method, invoked from the synchronous
/// `tick_multi_with_saturation_brake`, itself called directly — not via
/// `spawn_blocking` — inside the async task `spawn_multi_work_finder_task`
/// spawns).
///
/// Before this split, `RegistryDispatcher::dispatch` called
/// [`SweepRegistry::dispatch`] -> [`SweepRegistry::dispatch_inner`], which
/// (by its own doc comment) holds the registry mutex across the FULL
/// account-selection poll (bounded by `TOKEN_NAME_CAPTURE_TIMEOUT`, up to 5s)
/// — the exact hazard #6592 fixed for the IPC path, left in place here
/// because `dispatch_inner`'s doc comment explicitly scoped that fix to
/// `dispatch()` / `dispatch_resume_after_crash()` / unit tests, "where lock
/// contention is irrelevant." It IS relevant here: every `DaemonStatus`/
/// `health` IPC call's per-root `registry.lock()`
/// (`ipc.rs::build_daemon_status`) queues behind this same mutex, so a
/// work-finder dispatch in flight on any one registered root could stall a
/// fleet-wide status query for up to 5s per contended root.
///
/// Releases the mutex for the poll exactly as `dispatch_sweep_nonblocking`
/// does. Unlike that function, this has no `async fn` signature to `.await`
/// a `spawn_blocking` handle from, so it uses
/// [`tokio::task::block_in_place`] instead — synchronously equivalent to
/// `spawn_blocking` + `.await` for a caller already running inside a
/// multi-threaded Tokio runtime worker thread (which every production call
/// site is: `#[tokio::main]` defaults to the multi-thread flavor, and
/// `block_in_place` panics only on a `current_thread` runtime or outside any
/// runtime at all). Every unit test that calls this function directly (no
/// `#[tokio::test]`, no runtime entered) takes the `else` branch instead —
/// behaviorally identical to `dispatch_inner`'s pre-existing shape for those
/// tests, since no concurrent lock consumer ever competes with a synchronous
/// `#[test]` anyway.
pub(crate) fn dispatch_model_releasing_poll_lock(
    registry: &Arc<Mutex<SweepRegistry>>,
    kind: &SweepKind,
    idempotency_key: Option<String>,
    model: DispatchModel<'_>,
    effort: Option<&str>,
    depends_on: Option<u32>,
) -> Result<DispatchOutcome> {
    // Phase 1 (lock-scoped): idempotency dedup, full guard chain, claim
    // lock, label flip, dispatch stagger, `Command::spawn()`.
    let begin_outcome = {
        let mut sr = registry
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        sr.begin_issue_dispatch_with_model(kind, idempotency_key, model, effort, depends_on, None)
    };

    let mut prepared = match begin_outcome? {
        BeginIssueDispatch::Done(result) => return result,
        BeginIssueDispatch::Spawned(prepared) => prepared,
    };

    // Phase 2 (UNLOCKED): poll the child for its account-selection log line.
    let (token_name, runtime, immediate_preflight_death) =
        if tokio::runtime::Handle::try_current().is_ok() {
            tokio::task::block_in_place(|| {
                poll_and_classify_spawned_child(
                    &mut prepared.child,
                    &prepared.log_path,
                    &prepared.header_anchor,
                )
            })
        } else {
            poll_and_classify_spawned_child(
                &mut prepared.child,
                &prepared.log_path,
                &prepared.header_anchor,
            )
        };

    // Phase 3 (lock-scoped): record the outcome.
    let mut sr = registry
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    sr.finish_issue_dispatch(*prepared, token_name, runtime, immediate_preflight_death)
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::panic,
    clippy::expect_used,
    unused_imports
)]
mod tests;
