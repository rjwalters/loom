//! Autonomous self-update loop — the daemon rebuilds and restarts itself onto
//! a fresher binary when its source checkout has advanced past the commit it
//! was built from (Issue #4055, Phase 3 of #4017).
//!
//! # Why
//!
//! [`crate::self_update`] already answers the read-only question "is this
//! running binary stale vs. its source checkout?" and surfaces it in
//! `loom-daemon status`. But acting on it still required an operator to run
//! `loom-daemon-update.sh` by hand — the exact standing manual step a
//! long-lived daemon should own itself. This loop closes the self-repair cycle:
//! it *decides* and *sequences* the roll, reusing `loom-daemon-update.sh` for
//! the rebuild/provision and #4090's drain primitive
//! ([`crate::ipc::handle_drain_request`]) for the restart — it reimplements
//! neither.
//!
//! # Safety gates (from #4017 — the loop never ships without them)
//!
//! 1. **Verify before swap** — delivered by #4053: `loom-daemon-update.sh`
//!    asserts the freshly-built binary's embedded commit equals source HEAD
//!    *before* provisioning and re-verifies the destination after. A
//!    commit-identity mismatch (script exit `4`/`5`) is **terminal**, surfaced,
//!    and never retried.
//! 2. **Clean-tree gate** — `CARGO_MANIFEST_DIR` points at the operator's live
//!    working checkout, so an unattended `cargo build --release` there would
//!    compile whatever is uncommitted into the running daemon. The loop refuses
//!    to build unless [`crate::self_update::source_tree_clean`] is `Some(true)`.
//!    It never runs `git pull` on the operator's behalf.
//! 3. **Backoff on failure** — a source tree that does not compile must not
//!    retry every tick forever. Retryable build failures back off exponentially
//!    with a ceiling; the terminal give-up state is surfaced in `loom-daemon status`.
//! 4. **No build stampede** — a `cargo build --release` competes with every
//!    in-flight sweep's own build for CPU, so the loop defers the rebuild while
//!    [`crate::ipc::count_in_flight_sweeps`] reports any non-terminal sweep
//!    across every managed root (the "gated" policy). **Bounded** (#4929): the
//!    deferral is not open-ended — a host that runs at its dispatch cap around
//!    the clock never reaches zero in-flight sweeps, and an unconditional gate
//!    there starves the rebuild forever (observed: `last_roll: null` with
//!    `update_available: true` for a day+). After `deferDeadlineSecs` of
//!    continuous deferral the loop rebuilds anyway, at **reduced CPU priority**
//!    (`nice(19)`), so the stampede is mitigated rather than merely postponed.
//! 5. **In-flight sweeps survive** — the roll goes through #4090's **drain**
//!    path, not a bare restart, so dispatched sweeps finish first and stay in
//!    the registry rather than being orphaned as bare processes.
//! 6. **Flags replay exactly** — the restart exits into launchd
//!    `KeepAlive:SuccessfulExit`, which relaunches from the plist's persisted
//!    `ProgramArguments`/`EnvironmentVariables`, so the daemon comes back with
//!    exactly its prior autonomy flags, never wider.
//! 7. **Settle window** — the loop does not roll within `settleSecs` of first
//!    observing a stale commit, batching a burst of daemon commits into one
//!    roll (the timer resets whenever the source commit advances). **Bounded**
//!    (Issue #6261, mirroring #4929's bound on gate 4 below): a source
//!    checkout that keeps advancing more often than `settleSecs` apart would
//!    otherwise never reach "settled" — the 2026-08-14 incident's suspected
//!    cause (a 20-merge day, `settleSecs=600`). `SETTLE_CEILING_MULTIPLIER *
//!    settleSecs` after the FIRST stale observation in a streak, the loop
//!    proceeds past this gate regardless of how recently the last commit
//!    landed. Gate 4's own continuous-busy clock (`deferred_since`) was
//!    ALSO found to reset on every new commit (not just on host idle) —
//!    fixed the same issue: it no longer resets on a commit change, only on
//!    idle or a successful rebuild, so a host busy for hours with commits
//!    landing throughout still accumulates toward `deferDeadlineSecs`
//!    instead of restarting from zero every time.
//! 8. **Staleness surfaced, not just acted on** (Issue #6261) — every tick's
//!    decision (skip reason or rebuild) is logged, not only published to the
//!    latest-tick `daemon status` field (which a day-long incident can starve
//!    of readers); and when the running binary's staleness magnitude —
//!    [`crate::self_update::SelfUpdateStatus::commits_behind`] /
//!    `hours_behind` — crosses a warn threshold
//!    ([`crate::self_update::staleness_warning_default`]), that is logged
//!    too, independent of what this tick's gates decide.
//!
//! # Artifact-first, source-second (Issue #7609)
//!
//! Everything above describes the SOURCE path — rebuild this checkout when it
//! has advanced past the running binary. That path is now the **fallback**,
//! not the driver.
//!
//! Each tick first asks `loom-daemon-update.sh --resolve-json` what the latest
//! GitHub Release artifact for this host's platform is, and decides from that:
//!
//! | Observation | Decision |
//! |---|---|
//! | artifact version > installed version | fetch the artifact (never `cargo build`) |
//! | artifact version == installed, published sha256 ≠ installed binary's | fetch the artifact (converge onto the released bytes) |
//! | artifact version == installed, sha matches | up to date — nothing to do |
//! | no artifact resolves at all | fall back to the source path above, unchanged |
//!
//! **Source checkout presence, cleanliness, and staleness are not consulted on
//! the artifact path at all.** That is the whole point: on a four-host fleet on
//! 2026-09-13 the source gate was shut on *every* host — two for "no source
//! checkout / staleness undecidable" (`CARGO_MANIFEST_DIR` no longer resolving,
//! or a binary provisioned from a release by hand), one for a dirty tree
//! (two untracked stray files), one silent — so four hosts ran four different
//! daemon versions, one of them three weeks stale, while signed release
//! artifacts sat unconsumed. None of those four conditions says anything about
//! whether a newer signed binary exists.
//!
//! The artifact path reuses the update script's own `fetch_resolve_latest`
//! through the read-only `--resolve-json` mode rather than reimplementing
//! release resolution in Rust, and it invokes the roll as `--fetch`
//! (**rebuild fallback disabled**) so this path can never turn into a
//! `cargo build`. The settle window, backoff, and terminal gates all apply to
//! an artifact roll exactly as to a rebuild.
//!
//! **The in-flight sweep deferral (gate 4) does NOT** (Issue #8252). That gate
//! and its `deferDeadlineSecs` escape hatch exist to keep an unattended `cargo
//! build --release` off a host that is already saturated with sweep builds;
//! downloading a ~25 MB signed asset, verifying its checksum/signature, and
//! relaunching under the supervisor costs the host nothing comparable. Coupling
//! the two cost the fleet real availability: on 2026-09-18 a host sat on a
//! resolved 0.19.168 artifact for ~1.5 h (deferral deadline: up to ~4.5 h more)
//! while every `merge-pr.sh` invocation on it failed closed against a
//! subcommand the stale binary did not have. So an artifact fetch now proceeds
//! on the tick it is decided regardless of the in-flight count — still niced
//! when the host is busy, but never postponed. Only [`AutoUpdateState::decide_source`]
//! still defers.
//!
//! # `None` is never "stale"
//!
//! [`crate::self_update::SelfUpdateStatus::update_available`] is a tri-state:
//! only `Some(true)` triggers a rebuild. `None` (a tarball install with no
//! source checkout, or `BUILT_COMMIT == "unknown"`) means "do nothing" — never
//! "stale".
//!
//! # Process-global, not per-workspace
//!
//! Unlike [`crate::work_finder`] / [`crate::main_health_gate`] /
//! [`crate::token_ranking_refresh`], whose subject *is* a workspace, this loop's
//! subject is the **daemon process itself**: one binary, one source checkout,
//! one restart. So exactly **one** task runs per daemon regardless of how many
//! workspaces are registered — it is spawned alongside, not inside, the
//! per-workspace fan-outs. Config is read from the daemon's default workspace
//! (`sweep_workspace`), and gate 4's count is inherently cross-root via
//! [`crate::ipc::count_in_flight_sweeps`].

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};

use crate::event_bus::EventBus;
use crate::ipc::DrainState;
use crate::workspace_pool::WorkspacePool;

mod native_probe;
mod relaunch_verify_note;
mod stale_repo;

// ============================================================================
// Constants
// ============================================================================

/// Master on/off env override for the loop (precedence env > config >
/// default(**false**) — this loop has side effects on the running process, so
/// it is opt-in, like `workFinder`/`mainHealthGate`, not default-on).
pub const AUTO_UPDATE_ENABLE_ENV: &str = "LOOM_AUTO_UPDATE";

/// Env override for the staleness-check cadence (seconds).
pub const AUTO_UPDATE_INTERVAL_ENV: &str = "LOOM_AUTO_UPDATE_INTERVAL_SECS";

/// Env override for the settle window (seconds).
pub const AUTO_UPDATE_SETTLE_ENV: &str = "LOOM_AUTO_UPDATE_SETTLE_SECS";

/// Env override for gate 4's deferral deadline (seconds).
pub const AUTO_UPDATE_DEFER_DEADLINE_ENV: &str = "LOOM_AUTO_UPDATE_DEFER_DEADLINE_SECS";

/// Default cadence between staleness checks (15 minutes). A rebuild is far from
/// free, so this is deliberately coarse.
pub const DEFAULT_AUTO_UPDATE_INTERVAL_SECS: u64 = 900;

/// Default settle window (10 minutes): once a stale commit is first observed,
/// the loop waits this long — resetting on every further commit — before it
/// rolls, so a burst of merges collapses into a single roll.
pub const DEFAULT_AUTO_UPDATE_SETTLE_SECS: u64 = 600;

/// Default gate-4 deferral deadline (6 hours, #4929): once gate 4 has deferred
/// the rebuild continuously for this long — i.e. the host has had at least one
/// in-flight sweep at *every* check across that window — the loop stops
/// deferring and rebuilds at reduced CPU priority.
///
/// Chosen to be far longer than any healthy busy burst (a sweep is minutes to
/// low hours, and the gate re-arms the moment the host goes idle and the roll
/// happens normally), so the escape hatch only ever fires on a genuinely
/// *continuously* saturated host — never trading "never rebuilds" for
/// "stampedes on every busy period".
pub const DEFAULT_AUTO_UPDATE_DEFER_DEADLINE_SECS: u64 = 21_600;

/// `nice` value applied to the rebuild subprocess when it runs under the gate-4
/// deadline override, so a build forced onto a saturated host yields CPU to the
/// in-flight sweeps instead of competing with them. `19` is the maximum (lowest
/// priority) niceness on Linux and macOS.
const LOW_PRIORITY_NICE: i32 = 19;

/// First backoff delay after a retryable build failure. Subsequent failures
/// double it, capped at [`BACKOFF_CEILING`].
const BACKOFF_BASE: Duration = Duration::from_secs(60);

/// Ceiling on the exponential backoff so a persistently-broken source tree
/// still retries hourly (a later commit may fix it) rather than never again.
const BACKOFF_CEILING: Duration = Duration::from_secs(3600);

/// Bound on how long *repeated* settle-window resets can defer the FIRST
/// rebuild attempt (Issue #6261). The settle window's quiet-period reset
/// (every new commit restarts the `settle` timer, batching a burst of
/// merges into one roll) is unbounded on its own: a source checkout that
/// keeps advancing more often than `settle` apart never reaches "settled" —
/// exactly the failure mode a 20-merge day can produce. `first_stale_since`
/// (unlike `stale_since`) is NOT reset by a new commit, so once
/// `SETTLE_CEILING_MULTIPLIER * settle` has elapsed since the FIRST stale
/// observation in the current streak, the tick proceeds regardless of how
/// recently the last commit landed. With the default 600s settle window this
/// bounds the worst case to 1 hour instead of an unbounded string of resets.
const SETTLE_CEILING_MULTIPLIER: u32 = 6;

/// How long to wait for `loom-daemon-update.sh` (which runs `cargo build
/// --release`) before killing it. A release build of the daemon plus a
/// provision step is minutes, not seconds; this is generous headroom without
/// letting a wedged build pin the loop forever.
const DEFAULT_REBUILD_TIMEOUT: Duration = Duration::from_secs(1800);

/// Poll granularity while waiting for the rebuild subprocess.
const REBUILD_POLL_INTERVAL: Duration = Duration::from_millis(500);

/// Max bytes of captured script output retained in a failure/roll log line.
const MAX_OUTPUT_TAIL_BYTES: usize = 2048;

/// The clean-tree gate's base refusal reason (Issue #7608), shared between
/// [`AutoUpdateState::decide`] (which has no path detail) and [`run_tick`]
/// (which appends the offending paths from [`AutoUpdateProbe::tree_dirty_paths`]
/// when it matches this exact prefix).
const DIRTY_TREE_REASON: &str =
    "source tree is dirty — refusing an unattended rebuild (never `git pull`)";

// ============================================================================
// Config (.loom/config.json → autonomous.autoUpdate)
// ============================================================================

/// The subset of `.loom/config.json → autonomous.autoUpdate` this module
/// consumes. Each field is `Option` so an absent key falls through to the
/// env-var / built-in default — precedence **env > config > default** for every
/// knob, matching every other migrated `autonomous.*` surface.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AutoUpdateConfig {
    /// `autonomous.autoUpdate.enabled` — whether to run the loop. `None` when
    /// absent (falls through to env / default(**false**)).
    pub enabled: Option<bool>,
    /// `autonomous.autoUpdate.intervalSecs` — staleness-check cadence in
    /// seconds (a zero/invalid value is dropped to `None`).
    pub interval_secs: Option<u64>,
    /// `autonomous.autoUpdate.settleSecs` — settle window in seconds (a
    /// zero/invalid value is dropped to `None`; `0` is intentionally *not* a
    /// meaningful "no settle" here — use a small positive value).
    pub settle_secs: Option<u64>,
    /// `autonomous.autoUpdate.deferDeadlineSecs` — how long gate 4 may defer
    /// the rebuild for in-flight sweeps before rebuilding anyway at reduced
    /// priority (#4929). A zero/invalid value is dropped to `None`; `0` is
    /// intentionally *not* "never defer" — use a small positive value.
    pub defer_deadline_secs: Option<u64>,
}

/// Read `.loom/config.json → autonomous.autoUpdate` through
/// [`crate::config_resolver`] (so the `.loom-project/` tier is honored like
/// every other migrated `autonomous.*` block, #4058), soft-failing every field
/// to `None` (env/default resolution) on a missing file, malformed JSON, or a
/// missing `autonomous` / `autoUpdate` block. Shape copied verbatim from
/// [`crate::token_ranking_refresh::read_token_ranking_refresh_config`].
#[must_use]
pub fn read_auto_update_config(repo_root: &Path) -> AutoUpdateConfig {
    let effective = crate::config_resolver::resolve_effective_config(repo_root);
    let Some(block) = crate::config_resolver::get_path(&effective, "autonomous.autoUpdate") else {
        return AutoUpdateConfig::default();
    };

    AutoUpdateConfig {
        enabled: block.get("enabled").and_then(serde_json::Value::as_bool),
        interval_secs: block
            .get("intervalSecs")
            .and_then(serde_json::Value::as_u64)
            .filter(|&s| s > 0),
        settle_secs: block
            .get("settleSecs")
            .and_then(serde_json::Value::as_u64)
            .filter(|&s| s > 0),
        defer_deadline_secs: block
            .get("deferDeadlineSecs")
            .and_then(serde_json::Value::as_u64)
            .filter(|&s| s > 0),
    }
}

/// Resolve whether the loop is enabled with precedence **env > config >
/// default(false)**. This loop is opt-in (side effects on the running process),
/// so an absent config leaves it **off**.
#[must_use]
pub fn resolve_enabled(config: &AutoUpdateConfig) -> bool {
    if let Ok(v) = std::env::var(AUTO_UPDATE_ENABLE_ENV) {
        return matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on");
    }
    config.enabled.unwrap_or(false)
}

/// Resolve the check cadence with precedence **env > config > default**. A zero
/// or unparseable env value falls through to `config`/the default rather than
/// producing a busy loop.
#[must_use]
pub fn resolve_interval(config: &AutoUpdateConfig) -> Duration {
    std::env::var(AUTO_UPDATE_INTERVAL_ENV)
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|&s| s > 0)
        .or(config.interval_secs)
        .map_or_else(|| Duration::from_secs(DEFAULT_AUTO_UPDATE_INTERVAL_SECS), Duration::from_secs)
}

/// Resolve the settle window with precedence **env > config > default**.
#[must_use]
pub fn resolve_settle(config: &AutoUpdateConfig) -> Duration {
    std::env::var(AUTO_UPDATE_SETTLE_ENV)
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|&s| s > 0)
        .or(config.settle_secs)
        .map_or_else(|| Duration::from_secs(DEFAULT_AUTO_UPDATE_SETTLE_SECS), Duration::from_secs)
}

/// Resolve gate 4's deferral deadline with precedence **env > config >
/// default** (#4929). There is deliberately no "defer forever" setting: an
/// unbounded gate 4 is exactly the starvation bug this knob fixes. To make the
/// escape hatch effectively unreachable on a host that must never build under
/// load, set a very large value.
#[must_use]
pub fn resolve_defer_deadline(config: &AutoUpdateConfig) -> Duration {
    std::env::var(AUTO_UPDATE_DEFER_DEADLINE_ENV)
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|&s| s > 0)
        .or(config.defer_deadline_secs)
        .map_or_else(
            || Duration::from_secs(DEFAULT_AUTO_UPDATE_DEFER_DEADLINE_SECS),
            Duration::from_secs,
        )
}

// ============================================================================
// Status (published to the process-global, read by build_daemon_status)
// ============================================================================

/// The publicly-observable auto-update state rendered by `loom-daemon status`
/// (mirrors the `auto_update_*` fields on [`crate::types::DaemonStatusReport`]).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AutoUpdateStatusSnapshot {
    /// Whether the loop is enabled this process.
    pub enabled: bool,
    /// Wall-clock time of the most recent staleness check.
    pub last_check: Option<DateTime<Utc>>,
    /// Wall-clock time of the most recent successful roll.
    pub last_roll: Option<DateTime<Utc>>,
    /// Consecutive retryable build failures (resets on success / commit
    /// advance).
    pub consecutive_failures: u32,
    /// Current backoff delay in seconds, or `None` when not backing off.
    pub backoff_secs: Option<u64>,
    /// Terminal give-up reason (non-retryable failure), or `None`.
    pub terminal_reason: Option<String>,
    /// Short human-readable note about the most recent tick.
    pub note: Option<String>,
    /// The version of the latest release artifact resolved for this host's
    /// platform on the most recent tick (Issue #7609), or `None` when none
    /// resolved (no Releases yet, an unreachable API, `--no-fetch`, an
    /// unbuilt platform). Rendered next to the installed version so an
    /// operator can see at a glance whether the fleet has a newer signed
    /// binary available to roll onto.
    pub artifact_version: Option<String>,
    /// That release's publish timestamp, verbatim from the forge (RFC-3339),
    /// when the forge reported one.
    pub artifact_published_at: Option<String>,
    /// [`stale_repo::StaleRepoStreak::ticks`] (#8513): consecutive ticks whose
    /// resolved release was OLDER than installed, i.e. a probable wrong repo.
    pub stale_repo_ticks: u32,
    /// The repo that streak's most recent tick queried.
    pub stale_repo: Option<String>,
}

/// Shared, thread-safe handle the loop publishes to and
/// [`crate::ipc::build_daemon_status`] reads from.
#[derive(Debug, Default)]
pub struct AutoUpdateStatus {
    inner: Mutex<AutoUpdateStatusSnapshot>,
}

// Allow expect_used: a poisoned status mutex means another thread panicked
// while holding it — unrecoverable, matching the crash-on-poison policy used
// across ipc.rs / the drain state.
#[allow(clippy::expect_used)]
impl AutoUpdateStatus {
    #[must_use]
    pub fn new(enabled: bool) -> Self {
        Self {
            inner: Mutex::new(AutoUpdateStatusSnapshot {
                enabled,
                ..AutoUpdateStatusSnapshot::default()
            }),
        }
    }

    /// A snapshot of the current status for rendering.
    #[must_use]
    pub fn snapshot(&self) -> AutoUpdateStatusSnapshot {
        self.inner
            .lock()
            .expect("auto-update status mutex poisoned")
            .clone()
    }

    /// Overwrite the published snapshot.
    fn publish(&self, snap: AutoUpdateStatusSnapshot) {
        *self
            .inner
            .lock()
            .expect("auto-update status mutex poisoned") = snap;
    }
}

/// Process-global status handle. The single spawned loop registers its handle
/// here so [`crate::ipc::build_daemon_status`] can read auto-update state
/// without threading an `Arc` through the whole IPC server. Unset (loop never
/// spawned) reads as the default disabled/never-checked snapshot.
static GLOBAL_STATUS: OnceLock<Arc<AutoUpdateStatus>> = OnceLock::new();

/// Register the loop's status handle as the process-global. Idempotent: only
/// the first registration wins (there is exactly one loop per process).
pub fn register_global_status(status: Arc<AutoUpdateStatus>) {
    let _ = GLOBAL_STATUS.set(status);
}

/// The process-global auto-update status snapshot, or the default
/// (disabled/never-checked) when the loop was never spawned.
#[must_use]
pub fn global_status_snapshot() -> AutoUpdateStatusSnapshot {
    GLOBAL_STATUS
        .get()
        .map_or_else(AutoUpdateStatusSnapshot::default, |s| s.snapshot())
}

// ============================================================================
// Probe + drain trigger (testable via traits, mirrors RankingRefreshRunner)
// ============================================================================

/// The tri-state staleness signal plus the source HEAD it compared against,
/// derived from [`crate::self_update::check`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateCheck {
    /// `Some(true)` stale, `Some(false)` current, `None` undecidable (no source
    /// checkout / `BUILT_COMMIT == "unknown"`).
    pub update_available: Option<bool>,
    /// The source checkout's current HEAD short commit, when resolvable.
    pub source_commit: Option<String>,
    /// Staleness magnitude (Issue #6261) —
    /// [`crate::self_update::SelfUpdateStatus::commits_behind`], carried
    /// through so a tick can log a staleness warning independent of whatever
    /// `decide()`'s gates choose to do this tick.
    pub commits_behind: Option<u32>,
    /// Staleness magnitude (Issue #6261) —
    /// [`crate::self_update::SelfUpdateStatus::hours_behind`].
    pub hours_behind: Option<u32>,
}

// ============================================================================
// Release-artifact resolution (Issue #7609)
// ============================================================================

/// The latest GitHub Release artifact resolved for this host's platform, plus
/// the installed binary it is being compared against — the parsed shape of
/// `loom-daemon-update.sh --resolve-json`'s single JSON object.
///
/// Every field except `version` is optional because the script reports
/// `null` (never a fabricated value) for anything it could not determine:
/// an older `gh` that does not report `publishedAt`, a release whose
/// `.sha256` asset could not be downloaded, a host with no resolvable
/// installed binary at all.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ArtifactInfo {
    /// The repo this release was resolved from (`owner/repo`, #8513), so the
    /// tick's log line can name which repo it actually queried.
    pub repo: String,
    /// The release tag (e.g. `v0.19.24`).
    pub tag: String,
    /// The semver parsed out of the tag (e.g. `0.19.24`).
    pub version: String,
    /// The release's publish timestamp, verbatim from the forge (RFC-3339).
    pub published_at: Option<String>,
    /// The sha256 published for this platform's binary, read from the
    /// release's own `<bin>.sha256` asset.
    pub asset_sha256: Option<String>,
    /// The release target triple this host resolved to.
    pub target: Option<String>,
    /// The installed binary's version, as `loom-daemon --version` reports it.
    pub installed_version: Option<String>,
    /// The installed binary's own sha256 (of the file on disk).
    pub installed_sha256: Option<String>,
}

/// The result of one artifact-resolution attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ArtifactResolution {
    /// A release artifact for this platform resolved.
    Resolved(ArtifactInfo),
    /// No artifact resolved — the reason (no Releases yet, an unreachable or
    /// rate-limited API, an unbuilt platform, `--no-fetch` on this host). The
    /// tick falls back to the source path, exactly as before #7609.
    Unresolved(String),
}

impl ArtifactResolution {
    /// Whether this resolution would make the tick take the artifact path
    /// (i.e. it resolved AND the artifact is not already installed).
    #[must_use]
    pub fn is_actionable(&self) -> bool {
        match self {
            Self::Resolved(info) => !matches!(
                classify_artifact(info),
                ArtifactVerdict::UpToDate { .. } | ArtifactVerdict::StaleRepo { .. }
            ),
            Self::Unresolved(_) => false,
        }
    }
}

/// The verdict types and the pure classification that produces them —
/// extracted to a sibling module (#8513) both because this file is over
/// `.loom/docs/file-size-policy.md`'s threshold and because the new
/// wrong-repo verdict belongs next to the comparison that derives it.
mod artifact_verdict;

pub use artifact_verdict::{classify_artifact, ArtifactVerdict};

/// A record of the last artifact this daemon actually installed, persisted so
/// it survives the restart the roll itself performs.
///
/// **Why this must be persistent**: provisioning can legitimately change the
/// installed file's bytes after the fetch — on macOS `provision-daemon.sh`
/// ad-hoc-signs a binary that carries no certificate-backed signature, which
/// rewrites it. Without this record, the very next tick would see
/// "same version, sha differs", fetch again, restart again, and loop forever
/// at the tick cadence. With it, a convergence fetch is attempted **once** per
/// published `(version, asset sha256)` pair; a still-differing local sha
/// afterwards is reported and left alone.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ArtifactRollRecord {
    /// The version installed from the release artifact.
    pub version: String,
    /// The release's published sha256 for that artifact — the convergence key.
    pub asset_sha256: String,
    /// When the roll happened (diagnostic only).
    pub rolled_at: DateTime<Utc>,
}

/// File name of [`ArtifactRollRecord`]'s on-disk home under the state dir.
const ARTIFACT_ROLL_RECORD_FILE: &str = "auto-update-artifact-roll.json";

/// Override for the directory [`ArtifactRollRecord`] is persisted in (default
/// `~/.loom`). Exists for tests and for a host whose state home is elsewhere.
pub const AUTO_UPDATE_STATE_DIR_ENV: &str = "LOOM_AUTO_UPDATE_STATE_DIR";

/// Resolve the artifact-roll record path: `$LOOM_AUTO_UPDATE_STATE_DIR` when
/// set and non-empty, else `~/.loom/`. `None` when neither resolves (no home
/// directory) — the loop then runs record-less, which only costs the
/// loop-suppression above, never correctness of the fetch itself.
#[must_use]
fn artifact_roll_record_path() -> Option<PathBuf> {
    if let Ok(dir) = std::env::var(AUTO_UPDATE_STATE_DIR_ENV) {
        if !dir.trim().is_empty() {
            return Some(PathBuf::from(dir.trim()).join(ARTIFACT_ROLL_RECORD_FILE));
        }
    }
    dirs::home_dir().map(|h| h.join(".loom").join(ARTIFACT_ROLL_RECORD_FILE))
}

/// Read the persisted artifact-roll record. Soft-fails to `None` on a missing
/// file, unreadable path, or malformed JSON — a corrupt record must never wedge
/// the loop, it just costs one extra convergence attempt.
#[must_use]
fn load_artifact_roll_record(path: Option<&Path>) -> Option<ArtifactRollRecord> {
    let path = path?;
    let raw = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&raw).ok()
}

/// Persist the artifact-roll record. Best-effort: a write failure is logged
/// and otherwise ignored (the loop degrades to the in-memory guard, which
/// still suppresses a repeat within this process's lifetime).
fn store_artifact_roll_record(path: Option<&Path>, record: &ArtifactRollRecord) {
    let Some(path) = path else { return };
    let Ok(serialized) = serde_json::to_string_pretty(record) else {
        return;
    };
    if let Some(parent) = path.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            log::warn!(
                "auto_update: could not create {} for the artifact-roll record: {e}",
                parent.display()
            );
            return;
        }
    }
    if let Err(e) = std::fs::write(path, serialized) {
        log::warn!(
            "auto_update: could not persist the artifact-roll record to {}: {e} (a repeated \
             same-version convergence fetch is now possible after a restart)",
            path.display()
        );
    }
}

/// The outcome of one rebuild+provision invocation of `loom-daemon-update.sh`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RebuildOutcome {
    /// Script exit `0` — rebuilt + provisioned (or already current).
    Success,
    /// Script exit `1` (or a spawn/timeout failure) — retryable; back off.
    Retryable(String),
    /// Script exit `4`/`5` — a build-verification / commit-identity mismatch
    /// (#4053). Retrying cannot fix it; terminal, surfaced, not backed off.
    Terminal(String),
}

/// The "world" the loop probes each tick — abstracted behind a trait so the
/// loop is testable with a scripted fake (no real `git`/`cargo`), exactly as
/// [`crate::token_ranking_refresh::RankingRefreshRunner`] makes that loop
/// testable.
pub trait AutoUpdateProbe: Send {
    /// The latest release artifact for this host's platform (Issue #7609),
    /// resolved read-only. Defaults to `Unresolved` so a probe that predates
    /// the artifact path (or a test that does not care about it) behaves
    /// exactly as before — the tick falls straight through to the source path.
    fn resolve_artifact(&self) -> ArtifactResolution {
        ArtifactResolution::Unresolved("this probe does not resolve release artifacts".to_string())
    }

    /// Fetch + verify + provision the resolved release artifact (Issue #7609)
    /// — `loom-daemon-update.sh --fetch --no-restart`, i.e. with the source
    /// rebuild fallback **disabled**: this path never runs `cargo build`. A
    /// resolution failure at fetch time is a plain retryable failure, not a
    /// silent downgrade to a source build.
    ///
    /// `low_priority` has the same meaning as on [`Self::rebuild`].
    fn fetch_artifact(&mut self, low_priority: bool) -> RebuildOutcome {
        let _ = low_priority;
        RebuildOutcome::Retryable("this probe cannot fetch release artifacts".to_string())
    }

    /// The current staleness tri-state + source commit.
    fn check(&self) -> UpdateCheck;
    /// Whether the source working tree is clean. `None` ⇒ "cannot prove clean"
    /// (no checkout / `git` failed); the loop treats that as not-clean.
    fn is_tree_clean(&self) -> Option<bool>;
    /// The dirty paths behind an `is_tree_clean() != Some(true)` verdict, for
    /// naming the offending paths in the clean-tree gate's refusal log line
    /// (Issue #7608). Defaults to empty (no path detail) so existing probes
    /// need no changes.
    fn tree_dirty_paths(&self) -> Vec<String> {
        Vec::new()
    }
    /// Cross-root in-flight (non-terminal) sweep count (gate 4).
    fn in_flight_sweeps(&self) -> usize;
    /// Run the rebuild + provision step (`loom-daemon-update.sh --no-restart`)
    /// and map its exit code to a [`RebuildOutcome`]. Never panics.
    ///
    /// `low_priority` requests that the build run niced (#4929) — set when the
    /// gate-4 deadline forced the rebuild while sweeps are still in flight, so
    /// the build yields CPU to them instead of competing for it.
    fn rebuild(&mut self, low_priority: bool) -> RebuildOutcome;
}

/// Triggers the roll through #4090's drain path — separated from
/// [`AutoUpdateProbe`] because in production it needs a tokio runtime handle to
/// spawn the drain supervisor. Returns `true` when the drain was accepted.
pub trait DrainTrigger: Send {
    fn trigger(&self) -> bool;

    /// Whether a drain-and-restart is **already** armed (Issue #6007).
    ///
    /// Since a refused roll deadline now *retains* its intent (dispatch stays
    /// paused and the restart re-arms itself when in-flight reaches zero), an
    /// auto-update tick that fires while that is pending has nothing useful to
    /// do: the fresh binary is already provisioned and the restart is already
    /// coming. Rebuilding again would burn CPU competing with the very in-flight
    /// sweeps the roll is waiting on — the #4929 nicing exists precisely because
    /// that competition is harmful.
    ///
    /// Defaults to `false` so a caller with no drain state (tests, alternative
    /// triggers) behaves exactly as before.
    fn roll_in_progress(&self) -> bool {
        false
    }
}

/// The production [`AutoUpdateProbe`]: reads [`crate::self_update`] for
/// staleness + clean-tree, [`crate::ipc::count_in_flight_sweeps`] for gate 4,
/// and shells out to `loom-daemon-update.sh --no-restart` for the rebuild.
pub struct ScriptAutoUpdateProbe {
    /// Cross-root sweep count needs the pool + a fallback root.
    workspace_pool: Arc<WorkspacePool>,
    fallback_root: PathBuf,
    /// The source checkout to rebuild in (resolved once at spawn). `None` when
    /// there is no checkout — but the loop never rebuilds in that case anyway
    /// (`update_available` is `None`).
    source_root: Option<PathBuf>,
    timeout: Duration,
}

impl ScriptAutoUpdateProbe {
    #[must_use]
    pub fn new(workspace_pool: Arc<WorkspacePool>, fallback_root: PathBuf) -> Self {
        Self {
            workspace_pool,
            fallback_root,
            source_root: crate::self_update::source_checkout_root(),
            timeout: DEFAULT_REBUILD_TIMEOUT,
        }
    }

    /// Resolve the `loom-daemon-update.sh` path inside the source checkout:
    /// prefer the installed `.loom/scripts/cli/` copy, else the in-repo
    /// `defaults/scripts/cli/` source.
    fn resolve_script(root: &Path) -> Option<PathBuf> {
        let installed = root.join(".loom/scripts/cli/loom-daemon-update.sh");
        if installed.exists() {
            return Some(installed);
        }
        let source = root.join("defaults/scripts/cli/loom-daemon-update.sh");
        if source.exists() {
            return Some(source);
        }
        None
    }

    /// The candidate checkout roots the update script is searched under, in
    /// fallback order, on the ARTIFACT path (Issue #7609 + #7964):
    ///
    /// 1. The build-time source checkout, when still present.
    /// 2. This daemon's own workspace root.
    /// 3. The machine-level mirrored `defaults/` payload
    ///    ([`crate::init::git::machine_level_defaults_path`],
    ///    `~/.local/share/loom-daemon/defaults` by default,
    ///    `LOOM_DAEMON_DEFAULTS_DIR`-overridable) that `scripts/install-loom.sh`
    ///    / `loom update` keep current on every host.
    ///
    /// The fallback matters precisely because of the hosts this issue exists
    /// for: a daemon provisioned from a release artifact — or one whose
    /// `CARGO_MANIFEST_DIR` checkout has moved — has NO
    /// [`crate::self_update::source_checkout_root`], and would otherwise have
    /// no script to run even though fetching a newer artifact needs nothing
    /// from a source tree but the script itself. Candidate 3 additionally
    /// covers a host whose candidate-1/2 copy of `loom-daemon-update.sh` is
    /// simply stale (predates a script flag this daemon relies on) even
    /// though the file itself resolves — the per-machine mirror is kept
    /// current independent of any one workspace's checkout age.
    fn candidate_roots(&self) -> Vec<PathBuf> {
        let mut candidates = Vec::new();
        if let Some(root) = self.source_root.clone() {
            candidates.push(root);
        }
        candidates.push(self.fallback_root.clone());
        if let Some(mirror) = crate::init::git::machine_level_defaults_path() {
            candidates.push(mirror);
        }
        candidates
    }

    /// The checkout the update script is *invoked from*: the first of
    /// [`Self::candidate_roots`] whose `loom-daemon-update.sh` actually
    /// resolves. It is deliberately NOT used for [`Self::rebuild`]: a source
    /// build must happen in the checkout the binary was built from, never in
    /// some other repo (or the mirrored defaults payload, which has no
    /// buildable source at all) that merely happens to be registered.
    fn script_root(&self) -> Option<PathBuf> {
        self.candidate_roots()
            .into_iter()
            .find(|root| Self::resolve_script(root).is_some())
    }

    /// The "no artifact resolved" reason when [`Self::script_root`] finds
    /// nothing under ANY candidate — names every path tried so the log can
    /// distinguish "no checkout at all" from "checkouts exist but none carry
    /// the script" (Issue #7964).
    fn no_script_root_reason(&self) -> String {
        let tried: Vec<String> = self
            .candidate_roots()
            .iter()
            .map(|root| root.display().to_string())
            .collect();
        format!(
            "no checkout with a loom-daemon-update.sh could be resolved (tried: {})",
            tried.join(", ")
        )
    }
}

impl AutoUpdateProbe for ScriptAutoUpdateProbe {
    fn resolve_artifact(&self) -> ArtifactResolution {
        let Some(root) = self.script_root() else {
            return ArtifactResolution::Unresolved(self.no_script_root_reason());
        };
        let Some(script) = Self::resolve_script(&root) else {
            return ArtifactResolution::Unresolved(format!(
                "loom-daemon-update.sh not found under {}",
                root.display()
            ));
        };
        // #7810 PR 5: resolution is native. The script is still located above
        // because `fetch_artifact` below genuinely needs one; resolution only
        // needed it to borrow the checkout's git remote, which `root` supplies.
        let _ = script;
        native_probe::native_resolution(&root)
    }

    fn fetch_artifact(&mut self, low_priority: bool) -> RebuildOutcome {
        let Some(root) = self.script_root() else {
            return RebuildOutcome::Retryable(format!(
                "{} — cannot fetch",
                self.no_script_root_reason()
            ));
        };
        let Some(script) = Self::resolve_script(&root) else {
            return RebuildOutcome::Retryable(format!(
                "loom-daemon-update.sh not found under {}",
                root.display()
            ));
        };
        // #8513: pin the child to the repo THIS resolver chose. The script
        // resolves the release repo independently (its cwd's `origin`), so on
        // a host whose workspace is not the Loom checkout it would look for
        // the artifact we found in Loom's releases in the workspace's own
        // project and hard-fail `--fetch`. `LOOM_DAEMON_UPDATE_GH_REPO` is the
        // script's own documented override, and when the operator already set
        // it, `resolve_repo`'s tier 1 hands back that same value.
        let repo = native_probe::fetch_repo(&root);
        run_update_script_with(
            &script,
            &root,
            self.timeout,
            low_priority,
            &["--fetch"],
            repo.as_deref(),
        )
    }

    fn check(&self) -> UpdateCheck {
        let status = crate::self_update::check();
        UpdateCheck {
            update_available: status.update_available,
            source_commit: status.source_commit,
            commits_behind: status.commits_behind,
            hours_behind: status.hours_behind,
        }
    }

    fn is_tree_clean(&self) -> Option<bool> {
        crate::self_update::source_tree_clean()
    }

    fn tree_dirty_paths(&self) -> Vec<String> {
        crate::self_update::source_tree_dirty_paths()
    }

    fn in_flight_sweeps(&self) -> usize {
        crate::ipc::count_in_flight_sweeps(&self.workspace_pool, &self.fallback_root)
    }

    fn rebuild(&mut self, low_priority: bool) -> RebuildOutcome {
        let Some(root) = self.source_root.clone() else {
            return RebuildOutcome::Retryable(
                "no source checkout resolved — cannot rebuild".to_string(),
            );
        };
        let Some(script) = Self::resolve_script(&root) else {
            return RebuildOutcome::Retryable(format!(
                "loom-daemon-update.sh not found under {}",
                root.display()
            ));
        };
        run_update_script(&script, &root, self.timeout, low_priority)
    }
}

/// The production [`DrainTrigger`]: calls [`crate::ipc::handle_drain_request`]
/// (the #4090 primitive) inside a captured runtime handle so the supervisor it
/// spawns resolves a runtime even when invoked from a blocking thread.
pub struct IpcDrainTrigger {
    drain: Arc<DrainState>,
    workspace_pool: Arc<WorkspacePool>,
    fallback_root: PathBuf,
    event_bus: Arc<EventBus>,
    handle: tokio::runtime::Handle,
}

impl IpcDrainTrigger {
    #[must_use]
    pub fn new(
        drain: Arc<DrainState>,
        workspace_pool: Arc<WorkspacePool>,
        fallback_root: PathBuf,
        event_bus: Arc<EventBus>,
        handle: tokio::runtime::Handle,
    ) -> Self {
        Self {
            drain,
            workspace_pool,
            fallback_root,
            event_bus,
            handle,
        }
    }
}

impl DrainTrigger for IpcDrainTrigger {
    fn trigger(&self) -> bool {
        // Enter the runtime so `handle_drain_request`'s internal `tokio::spawn`
        // of the drain supervisor resolves a runtime from a blocking thread.
        // `timeout_secs=None` uses the default drain deadline;
        // `force_after_timeout=false` is the fail-safe — if in-flight sweeps do
        // not drain by the deadline the roll is refused and dispatch resumes,
        // never killing a sweep.
        let _guard = self.handle.enter();
        // `then_exit=false`: this is the #4090 roll trigger — the daemon must
        // restart (relaunch into the freshly-rebuilt binary), never stop for
        // good. `then_exit: true` is `fleet drain`'s (#4343) teardown-only path.
        let resp = crate::ipc::handle_drain_request(
            &self.drain,
            &self.workspace_pool,
            &self.fallback_root,
            &self.event_bus,
            None,
            false,
            false,
        );
        // Issue #4521: the reply's `then_exit` reports the ACTIVE drain's
        // terminal action, not this request's. `true` here means an operator
        // teardown drain (`--drain --then-exit`) was already in flight, so this
        // roll piggybacks on a drain that will STOP the daemon rather than
        // relaunch it into the freshly-built binary. That is intentional
        // (then-exit is never downgraded — the host is being torn down), but it
        // must not be silent: the new binary will not be picked up until the
        // daemon is started again.
        if let crate::types::Response::DaemonDrain {
            accepted: true,
            then_exit: true,
            ..
        } = &resp
        {
            log::warn!(
                "auto-update roll joined an in-progress then-exit (teardown) drain: the daemon \
                 will STOP when drained and will NOT relaunch into the rebuilt binary. Start it \
                 again to pick up the update."
            );
        }
        matches!(resp, crate::types::Response::DaemonDrain { accepted: true, .. })
    }

    fn roll_in_progress(&self) -> bool {
        // `is_draining()` covers both an in-progress first-attempt drain and a
        // retained (pending) roll — in either case a restart is already armed.
        self.drain.is_draining()
    }
}

/// Run `loom-daemon-update.sh --no-restart` in `cwd`, capturing combined output
/// to a temp file (never a pipe — `cargo build --release` is long and chatty,
/// exactly the pipe-buffer-deadlock case) and killing it after `timeout`.
/// Maps the script's documented exit codes to a [`RebuildOutcome`]:
/// `0`→Success, `4`/`5`→Terminal, everything else (incl. spawn/timeout)
/// →Retryable.
///
/// `low_priority` (#4929) niced the whole build subtree to
/// [`LOW_PRIORITY_NICE`] via a `pre_exec` `setpriority(2)` — inherited by
/// `cargo`/`rustc`, so a rebuild forced past gate 4's deadline yields CPU to
/// the in-flight sweeps rather than stampeding them.
///
/// # Artifact-fetch precedence (Epic #4990 Phase 3, #5020)
///
/// No flag is passed here to select fetch-vs-build: `loom-daemon-update.sh`
/// prefers a verified GitHub Release artifact for the host's platform
/// automatically (default "auto" precedence, opt-out via `--no-fetch` /
/// `LOOM_DAEMON_UPDATE_FETCH=0`) whenever one resolves, and softly falls back
/// to this same `cargo build --release` path otherwise — so a saturated host
/// with no Rust toolchain converges on a release alone (AC1) with *zero*
/// daemon-side awareness required. This call site deliberately does not opt
/// in with `--fetch` (which would hard-fail instead of falling back): the
/// auto-updater's whole purpose is unattended convergence, and a resolution
/// hiccup (an unreachable GitHub API, a release missing this platform's
/// artifact) must degrade to the existing rebuild path, not go Terminal.
/// The exit-code contract above is UNCHANGED by the fetch path: a checksum
/// or signature-verification failure on a resolved artifact maps to exit `1`
/// (Retryable, same bucket as a `cargo build` failure — plausibly transient,
/// e.g. a network blip), so `classify_exit` below needs no new cases.
fn run_update_script(
    script: &Path,
    cwd: &Path,
    timeout: Duration,
    low_priority: bool,
) -> RebuildOutcome {
    run_update_script_with(script, cwd, timeout, low_priority, &[], None)
}

/// [`run_update_script`] with `extra_args` inserted alongside `--no-restart`.
///
/// The one caller that passes anything is the ARTIFACT path (Issue #7609),
/// which passes `--fetch`: that mode REQUIRES a verified release artifact and
/// hard-fails (exit `1`, i.e. [`RebuildOutcome::Retryable`]) instead of
/// silently falling back to `cargo build --release`. That is the opposite of
/// the default-`auto` reasoning documented above, and deliberately so: the
/// source path is reached by the *tick's own* decision (no artifact resolved),
/// never by a mid-run downgrade inside the script that the daemon cannot see.
///
/// `repo` (#8513), when `Some`, is exported to the child as
/// `LOOM_DAEMON_UPDATE_GH_REPO` — the script's own documented override — so
/// the fetch downloads from the repo THIS process resolved rather than
/// re-deriving one from its cwd's `origin` remote. The source path passes
/// `None`: a `cargo build` reads no releases at all.
fn run_update_script_with(
    script: &Path,
    cwd: &Path,
    timeout: Duration,
    low_priority: bool,
    extra_args: &[&str],
    repo: Option<&str>,
) -> RebuildOutcome {
    let log_path =
        std::env::temp_dir().join(format!("loom-auto-update-{}.log", uuid::Uuid::new_v4()));
    let out_file = match std::fs::File::create(&log_path) {
        Ok(f) => f,
        Err(e) => return RebuildOutcome::Retryable(format!("could not create output file: {e}")),
    };
    let stderr_file = match out_file.try_clone() {
        Ok(f) => f,
        Err(e) => {
            let _ = std::fs::remove_file(&log_path);
            return RebuildOutcome::Retryable(format!("could not clone output handle: {e}"));
        }
    };

    let mut command = Command::new(script);
    command
        .arg("--no-restart")
        .args(extra_args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::from(out_file))
        .stderr(Stdio::from(stderr_file));
    if let Some(repo) = repo {
        command.env("LOOM_DAEMON_UPDATE_GH_REPO", repo);
    }
    if low_priority {
        nice_child(&mut command);
    }

    let mut child = match command.spawn() {
        Ok(c) => c,
        Err(e) => {
            let _ = std::fs::remove_file(&log_path);
            return RebuildOutcome::Retryable(format!(
                "could not spawn `{}`: {e}",
                script.display()
            ));
        }
    };

    let start = Instant::now();
    let outcome = loop {
        match child.try_wait() {
            Ok(Some(status)) => break classify_exit(status.code(), &log_path),
            Ok(None) => {
                if start.elapsed() >= timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    break RebuildOutcome::Retryable(format!(
                        "`{}` timed out after {}s",
                        script.display(),
                        timeout.as_secs()
                    ));
                }
                std::thread::sleep(REBUILD_POLL_INTERVAL);
            }
            Err(e) => {
                break RebuildOutcome::Retryable(format!(
                    "could not poll `{}`: {e}",
                    script.display()
                ))
            }
        }
    };
    let _ = std::fs::remove_file(&log_path);
    outcome
}

/// Nice the child (and, by inheritance, the `cargo`/`rustc` processes it
/// spawns) down to [`LOW_PRIORITY_NICE`] before `exec`. Best-effort: a failing
/// `setpriority` is deliberately ignored — a build at normal priority is far
/// better than no build at all, which is the starvation this whole path exists
/// to end (#4929).
#[cfg(unix)]
fn nice_child(command: &mut Command) {
    use std::os::unix::process::CommandExt;
    // SAFETY: `pre_exec` runs between fork and exec, where only
    // async-signal-safe work is permitted. `setpriority(2)` is a bare syscall
    // wrapper — it allocates nothing, takes no locks, and touches no libc
    // global state — so it is safe in that window.
    unsafe {
        command.pre_exec(|| {
            libc::setpriority(libc::PRIO_PROCESS, 0, LOW_PRIORITY_NICE);
            Ok(())
        });
    }
}

/// Non-unix hosts have no `setpriority`; the build simply runs at normal
/// priority (the daemon's supervised install targets are macOS/Linux).
#[cfg(not(unix))]
fn nice_child(_command: &mut Command) {}

/// Map a `loom-daemon-update.sh` exit code to a [`RebuildOutcome`], attaching a
/// tail of the captured output on any non-success.
fn classify_exit(code: Option<i32>, log_path: &Path) -> RebuildOutcome {
    let tail = || truncate_tail(&std::fs::read_to_string(log_path).unwrap_or_default());
    match code {
        Some(0) => RebuildOutcome::Success,
        // #4053: exit 4 (build-verification) and 5 (post-provision
        // verification) are commit-identity defects retrying cannot fix.
        Some(4) => {
            RebuildOutcome::Terminal(format!("build verification failed (exit 4): {}", tail()))
        }
        Some(5) => RebuildOutcome::Terminal(format!(
            "post-provision verification failed (exit 5): {}",
            tail()
        )),
        Some(other) => RebuildOutcome::Retryable(format!("exit {other}: {}", tail())),
        None => RebuildOutcome::Retryable(format!("killed by signal: {}", tail())),
    }
}

/// Keep only the last [`MAX_OUTPUT_TAIL_BYTES`] bytes of captured output,
/// trimmed, on a char boundary.
fn truncate_tail(s: &str) -> String {
    if s.len() <= MAX_OUTPUT_TAIL_BYTES {
        return s.trim().to_string();
    }
    let start = s.len() - MAX_OUTPUT_TAIL_BYTES;
    let start = (start..s.len())
        .find(|&i| s.is_char_boundary(i))
        .unwrap_or(s.len());
    s[start..].trim().to_string()
}

// ============================================================================
// Loop state + pure decision logic
// ============================================================================

/// The per-tick decision the loop reaches from the current state + probe
/// readings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TickDecision {
    /// Do nothing this tick; the string is the human-readable reason surfaced in
    /// `loom-daemon status`.
    Skip(String),
    /// Do nothing, but at WARN rather than [`Self::Skip`]'s INFO: a
    /// [`ArtifactVerdict::StaleRepo`] tick (#8513), where "nothing to do" is
    /// itself the symptom rather than a healthy host.
    SkipWarn(String),
    /// All gates passed — run the rebuild.
    Rebuild {
        /// `true` when gate 4's deferral deadline forced this rebuild while
        /// sweeps are still in flight (#4929), so the build must run niced.
        /// `false` on the normal quiescent-host path.
        low_priority: bool,
    },
    /// All gates passed — fetch + provision the resolved release artifact
    /// (Issue #7609). **Never** a `cargo build`: the roll runs the update
    /// script with the rebuild fallback disabled.
    FetchArtifact {
        /// The release version being installed.
        version: String,
        /// The release tag (for the log line).
        tag: String,
        /// Why this fetch was chosen — `"artifact 0.19.24 > installed
        /// 0.19.21"` or the same-version sha-convergence reason — so the tick
        /// log names the path AND the cause.
        why: String,
        /// `true` when the host had in-flight sweeps at decision time, so the
        /// fetch runs niced and yields CPU to them. Unlike [`Self::Rebuild`]'s
        /// flag this is NOT a post-deadline escape hatch: the fetch was never
        /// deferred in the first place (Issue #8252) — it just runs politely.
        low_priority: bool,
    },
}

/// One tick's probe readings, as [`AutoUpdateState::decide`] consumes them.
/// Bundled rather than passed loose so the artifact reading (Issue #7609) sits
/// alongside the source-side readings it takes precedence over, in one place.
#[derive(Debug, Clone, Copy)]
pub struct TickInputs<'a> {
    /// The latest release artifact resolved for this host's platform — the
    /// FIRST thing consulted; `Unresolved` is what makes the tick fall back to
    /// the source readings below.
    pub artifact: &'a ArtifactResolution,
    /// The source-checkout staleness tri-state.
    pub check: &'a UpdateCheck,
    /// Whether the source working tree is provably clean (source path only).
    pub tree_clean: bool,
    /// Cross-root in-flight (non-terminal) sweep count (gate 4).
    pub in_flight: usize,
}

/// The loop's mutable bookkeeping: settle-window tracking, backoff, and the
/// terminal give-up state. Kept separate from any I/O so the deciding logic is
/// unit-testable with plain values.
#[derive(Debug, Default)]
pub struct AutoUpdateState {
    /// The thing this streak is trying to roll onto, tracked for the settle
    /// window: the stale source commit on the source path, or an
    /// `artifact:<version>:<sha>` identity on the artifact path (Issue #7609).
    /// Both paths share the field so a host that switches between them (a
    /// release appears mid-streak) restarts its settle window exactly as it
    /// would for a new commit.
    tracked_target: Option<String>,
    /// When the currently-tracked stale commit was first observed (settle
    /// timer origin). Monotonic — never wall-clock — for correct durations.
    /// Resets on EVERY new commit (the quiet-period timer) — see
    /// `first_stale_since` for the reset-proof ceiling anchor.
    stale_since: Option<Instant>,
    /// When the CURRENT continuous-stale streak began (Issue #6261) —
    /// monotonic, set once per streak and, unlike `stale_since`, NOT reset by
    /// a later commit landing mid-streak. Cleared only when the loop catches
    /// up (`update_available` leaves `Some(true)`) or a rebuild succeeds.
    /// [`AutoUpdateState::decide`]'s settle gate uses this as a hard ceiling
    /// on how long repeated `stale_since` resets can defer the first
    /// attempt (`SETTLE_CEILING_MULTIPLIER * settle`).
    first_stale_since: Option<Instant>,
    /// Consecutive retryable build failures for the tracked commit.
    consecutive_failures: u32,
    /// Instant until which the loop is backing off after a retryable failure.
    backoff_until: Option<Instant>,
    /// The current backoff delay (for status), or `None` when not backing off.
    backoff: Option<Duration>,
    /// When gate 4 first deferred the rebuild for the current continuous-busy
    /// run (monotonic). Cleared whenever the host is observed idle
    /// (`in_flight == 0`) or a rebuild succeeds. Issue #6261: this is
    /// DELIBERATELY *not* reset when the tracked commit changes — gate 4
    /// measures "how long has this host been continuously saturated", which
    /// is orthogonal to which specific stale commit is being targeted; a
    /// commit landing mid-defer must not restart the clock, or a host busy
    /// for 5 continuous hours with commits landing throughout never
    /// accumulates toward `deferDeadlineSecs` at all (the #4929 escape hatch
    /// this field exists for would then never fire).
    deferred_since: Option<Instant>,
    /// A terminal give-up reason for the tracked commit (sticky until the
    /// source commit advances).
    terminal_reason: Option<String>,
    /// Wall-clock time of the last successful roll (for status).
    last_roll: Option<DateTime<Utc>>,
    /// Where [`ArtifactRollRecord`] is persisted, or `None` to run
    /// record-less (no home directory resolvable; tests that do not exercise
    /// the convergence guard).
    artifact_record_path: Option<PathBuf>,
    /// The last artifact this daemon installed (Issue #7609) — loaded from
    /// `artifact_record_path` at construction so it survives the restart the
    /// roll itself performs, and the reason a same-version convergence fetch
    /// cannot become a fetch/restart loop.
    last_artifact_roll: Option<ArtifactRollRecord>,
    /// The consecutive-wrong-repo-resolution streak (#8513) that feeds
    /// [`crate::health::assess_auto_update`]'s "no progress for N ticks"
    /// surface — a persistently stale-repo host is stuck exactly as a
    /// terminal/backoff one is, just for a different reason.
    stale_repo: stale_repo::StaleRepoStreak,
}

impl AutoUpdateState {
    #[must_use]
    pub fn new() -> Self {
        let artifact_record_path = artifact_roll_record_path();
        Self {
            last_artifact_roll: load_artifact_roll_record(artifact_record_path.as_deref()),
            artifact_record_path,
            ..Self::default()
        }
    }

    /// [`Self::new`] with an explicit artifact-roll record path (tests, and
    /// any caller that must not touch the ambient state home).
    #[must_use]
    pub fn new_with_record_path(path: Option<PathBuf>) -> Self {
        Self {
            last_artifact_roll: load_artifact_roll_record(path.as_deref()),
            artifact_record_path: path,
            ..Self::default()
        }
    }

    /// Decide this tick, artifact first (Issue #7609).
    ///
    /// A resolved release artifact decides the tick on its own — source
    /// checkout presence, cleanliness, and staleness are not consulted at all.
    /// Only when NO artifact resolves does this fall through to
    /// [`Self::decide_source`], today's behavior unchanged, with the
    /// resolution failure named in the skip reason so the log always says
    /// which path was taken and why.
    pub fn decide(
        &mut self,
        now: Instant,
        inputs: &TickInputs<'_>,
        settle: Duration,
        defer_deadline: Duration,
    ) -> TickDecision {
        let TickInputs {
            artifact,
            check,
            tree_clean,
            in_flight,
        } = *inputs;
        match artifact {
            ArtifactResolution::Resolved(info) => {
                // No `defer_deadline`: the artifact path does not defer at all
                // (Issue #8252), so it has no deadline to bound.
                self.decide_artifact(now, info, in_flight, settle)
            }
            ArtifactResolution::Unresolved(reason) => {
                // No artifact resolved this tick at all — any stale-repo
                // streak from a previous tick no longer applies (#8513).
                self.stale_repo.reset();
                match self.decide_source(now, check, tree_clean, in_flight, settle, defer_deadline)
                {
                    TickDecision::Skip(source_reason) => TickDecision::Skip(format!(
                        "no artifact ({reason}) → source path: {source_reason}"
                    )),
                    other => other,
                }
            }
        }
    }

    /// The ARTIFACT path (Issue #7609): decide purely from the resolved
    /// release artifact vs. the installed binary, then apply the same
    /// terminal / backoff / settle gates a rebuild goes through.
    ///
    /// The clean-tree gate is deliberately NOT applied here: it exists because
    /// an unattended `cargo build --release` would compile whatever is
    /// uncommitted in the operator's checkout into the running daemon. A fetch
    /// of a published, checksum-verified artifact reads nothing from the
    /// working tree, so a stray untracked file there has no bearing on it.
    ///
    /// Neither is the in-flight sweep deferral (gate 4, Issue #8252) — see the
    /// module doc: that gate protects the host from a `cargo build --release`
    /// stampede, and a fetch is not a build. A busy host still gets the fetch
    /// *niced* (`low_priority`), it just no longer gets it *postponed*.
    fn decide_artifact(
        &mut self,
        now: Instant,
        info: &ArtifactInfo,
        in_flight: usize,
        settle: Duration,
    ) -> TickDecision {
        let verdict = classify_artifact(info);
        // Issue #8513: the streak counts only genuinely CONSECUTIVE
        // stale-repo ticks, so anything else this tick resolved drops it —
        // but the reset must not run before the `StaleRepo` arm increments,
        // or the counter would be re-zeroed every tick and never reach the
        // health threshold.
        if !matches!(verdict, ArtifactVerdict::StaleRepo { .. }) {
            self.stale_repo.reset();
        }
        let (target, why) = match verdict {
            ArtifactVerdict::UpToDate { version, why } => {
                self.clear_tracking();
                return TickDecision::Skip(format!("artifact {version}: {why} → up to date"));
            }
            ArtifactVerdict::StaleRepo {
                artifact,
                installed,
                repo,
            } => {
                // #8513: tracked for health's "no progress for N ticks"
                // surface and logged at WARN, naming the repo queried.
                self.clear_tracking();
                self.stale_repo.record(repo.clone());
                return TickDecision::SkipWarn(stale_repo::warn_reason(
                    &artifact, &installed, &repo,
                ));
            }
            ArtifactVerdict::Newer {
                installed,
                artifact,
            } => (
                artifact_target_id(&artifact, info),
                format!(
                    "artifact {artifact} > installed {} → fetching",
                    installed.as_deref().unwrap_or("<none>")
                ),
            ),
            ArtifactVerdict::ShaDiffers {
                version,
                asset_sha256,
                installed_sha256,
            } => {
                // The convergence guard (see [`ArtifactRollRecord`]): this
                // daemon already installed exactly these published bytes, so a
                // still-differing local sha is post-install mutation on this
                // host (macOS ad-hoc re-signing is the known one), not a stale
                // binary. Re-fetching would reinstall the same artifact and
                // restart the daemon on every tick, forever.
                if self.already_converged(&version, &asset_sha256) {
                    self.clear_tracking();
                    return TickDecision::Skip(format!(
                        "artifact {version} was already installed from this release (published \
                         sha {}), but the installed binary's bytes still differ (sha {}) — local \
                         post-install signing, not a stale binary; not re-fetching",
                        short_sha(&asset_sha256),
                        short_sha(&installed_sha256)
                    ));
                }
                (
                    artifact_target_id(&version, info),
                    format!(
                        "artifact {version} == installed {version} but sha differs (published {} \
                         vs installed {}) → fetching",
                        short_sha(&asset_sha256),
                        short_sha(&installed_sha256)
                    ),
                )
            }
        };

        self.track_target(now, Some(target));
        // Shared bookkeeping with the source path: an idle observation re-arms
        // gate 4's continuous-busy clock for whichever path consults it next.
        // This path no longer consults it at all (Issue #8252), but the clock
        // is state shared with `decide_source`, so keep it honest.
        if in_flight == 0 {
            self.deferred_since = None;
        }
        if let Some(skip) = self.terminal_or_backoff_gate(now) {
            return skip;
        }
        if let Some(skip) = self.settle_gate(now, settle) {
            return skip;
        }
        // NO in-flight gate here (Issue #8252): a fetch is not a build, so it
        // is never deferred behind the build-stampede guard — only niced when
        // the host is busy, exactly as the post-deadline path used to do,
        // minus the wait. `defer_deadline` is consumed by `decide_source` only.
        TickDecision::FetchArtifact {
            version: info.version.clone(),
            tag: info.tag.clone(),
            why,
            low_priority: in_flight > 0,
        }
    }

    /// The SOURCE path — rebuild this checkout when it has advanced past the
    /// running binary. Unchanged since #6261 in every respect; it is simply no
    /// longer the first thing a tick consults (Issue #7609), only the fallback
    /// for when no release artifact resolves at all.
    ///
    /// Mutates settle-window, gate-4-deferral, and commit-identity bookkeeping
    /// (resetting backoff/terminal when the source commit advances) but
    /// performs no I/O.
    pub fn decide_source(
        &mut self,
        now: Instant,
        check: &UpdateCheck,
        tree_clean: bool,
        in_flight: usize,
        settle: Duration,
        defer_deadline: Duration,
    ) -> TickDecision {
        // Only `Some(true)` is actionable. `Some(false)` (current) and `None`
        // (undecidable — tarball / unknown built commit) both clear any pending
        // settle state and do nothing.
        if check.update_available != Some(true) {
            self.clear_tracking();
            return TickDecision::Skip(match check.update_available {
                Some(false) => "up to date with source HEAD".to_string(),
                _ => "no source checkout / staleness undecidable — nothing to do".to_string(),
            });
        }

        self.track_target(now, check.source_commit.clone());

        // Gate 4's deadline only accumulates while the host is genuinely busy:
        // any idle observation re-arms it from scratch, so a healthy host that
        // dips to zero in-flight sweeps rolls the normal way and never
        // approaches the override.
        if in_flight == 0 {
            self.deferred_since = None;
        }

        if let Some(skip) = self.terminal_or_backoff_gate(now) {
            return skip;
        }

        // Clean-tree gate — refuse an unattended build of a dirty checkout.
        if !tree_clean {
            return TickDecision::Skip(DIRTY_TREE_REASON.to_string());
        }

        if let Some(skip) = self.settle_gate(now, settle) {
            return skip;
        }

        match self.in_flight_gate(now, in_flight, defer_deadline) {
            Err(skip) => skip,
            Ok(low_priority) => TickDecision::Rebuild { low_priority },
        }
    }

    /// Drop all per-target tracking (settle window, gate-4 clock) — the
    /// "nothing to do this tick" reset shared by both paths.
    fn clear_tracking(&mut self) {
        self.tracked_target = None;
        self.stale_since = None;
        self.first_stale_since = None;
        self.deferred_since = None;
    }

    /// Whether the recorded artifact roll already installed exactly these
    /// published bytes (Issue #7609's fetch/restart-loop guard).
    fn already_converged(&self, version: &str, asset_sha256: &str) -> bool {
        self.last_artifact_roll.as_ref().is_some_and(|rec| {
            rec.version == version && rec.asset_sha256.eq_ignore_ascii_case(asset_sha256)
        })
    }

    /// Adopt `target` (a source commit, or an artifact version+sha identity)
    /// as the thing this streak is trying to roll onto.
    ///
    /// A new (or first) target resets the quiet-period settle timer AND clears
    /// any backoff/terminal state — a later commit/release is a fresh attempt
    /// that may well fix a previously-broken roll. It does NOT reset
    /// `first_stale_since` (only set once per streak, via `get_or_insert`) or
    /// `deferred_since` (Issue #6261 — see the field doc comment on
    /// `deferred_since`): gate 4's continuous-busy clock and the settle ceiling
    /// are both deliberately reset-proof against a new target landing
    /// mid-streak.
    fn track_target(&mut self, now: Instant, target: Option<String>) {
        if self.tracked_target != target {
            self.tracked_target = target;
            self.stale_since = Some(now);
            self.first_stale_since.get_or_insert(now);
            self.consecutive_failures = 0;
            self.backoff_until = None;
            self.backoff = None;
            self.terminal_reason = None;
        }
    }

    /// Terminal-state and backoff gates, shared by both paths.
    fn terminal_or_backoff_gate(&self, now: Instant) -> Option<TickDecision> {
        if let Some(reason) = &self.terminal_reason {
            return Some(TickDecision::Skip(format!(
                "terminal — not retrying until a new commit: {reason}"
            )));
        }
        if let Some(until) = self.backoff_until {
            if now < until {
                let secs = until.saturating_duration_since(now).as_secs();
                return Some(TickDecision::Skip(format!(
                    "backing off after build failure (~{secs}s left)"
                )));
            }
        }
        None
    }

    /// Settle window — batch a burst of commits (or a burst of releases) into
    /// one roll: quiet-period test (no new target within `settle`), OR (Issue
    /// #6261) the reset-proof ceiling — `SETTLE_CEILING_MULTIPLIER * settle`
    /// elapsed since the FIRST observation in this streak — so a source
    /// checkout that keeps advancing more often than `settle` apart still
    /// converges on a bounded worst-case wait instead of deferring the first
    /// attempt indefinitely.
    fn settle_gate(&self, now: Instant, settle: Duration) -> Option<TickDecision> {
        let quiet_settled = self
            .stale_since
            .is_some_and(|s| now.duration_since(s) >= settle);
        let ceiling = settle.saturating_mul(SETTLE_CEILING_MULTIPLIER);
        let ceiling_settled = self
            .first_stale_since
            .is_some_and(|s| now.duration_since(s) >= ceiling);
        if !quiet_settled && !ceiling_settled {
            return Some(TickDecision::Skip(
                "within settle window — waiting for commits to settle".to_string(),
            ));
        }
        if ceiling_settled && !quiet_settled {
            log::warn!(
                "auto_update: settle window has been reset by new commits continuously for {}s \
                 (exceeding the {}s ceiling, {SETTLE_CEILING_MULTIPLIER}x the {}s settle window) \
                 — proceeding past the settle gate anyway so the loop is not starved by a busy \
                 merge day",
                self.first_stale_since
                    .map_or(0, |s| now.saturating_duration_since(s).as_secs()),
                ceiling.as_secs(),
                settle.as_secs()
            );
        }
        None
    }

    /// Gate 4 — do not stampede in-flight sweep builds. Bounded (#4929): a
    /// host that runs at its dispatch cap around the clock never reaches zero
    /// in-flight sweeps, and an open-ended defer there starves the roll
    /// forever. After `defer_deadline` of *continuous* deferral the loop rolls
    /// anyway, niced, so the update still converges.
    ///
    /// **Rebuild path only** (Issue #8252): [`Self::decide_source`] is the sole
    /// caller. [`Self::decide_artifact`] does not defer — a checksum-verified
    /// download is not a `cargo build --release` and has no stampede to avoid —
    /// so every skip reason produced here is unambiguously about a *rebuild*.
    ///
    /// `Ok(low_priority)` ⇒ proceed; `Err(skip)` ⇒ defer this tick.
    fn in_flight_gate(
        &mut self,
        now: Instant,
        in_flight: usize,
        defer_deadline: Duration,
    ) -> Result<bool, TickDecision> {
        if in_flight == 0 {
            return Ok(false);
        }
        let since = *self.deferred_since.get_or_insert(now);
        let waited = now.saturating_duration_since(since);
        if waited < defer_deadline {
            let left = defer_deadline.saturating_sub(waited).as_secs();
            return Err(TickDecision::Skip(format!(
                "{in_flight} in-flight sweep(s) — deferring the source rebuild to avoid a build \
                 stampede (forcing a low-priority rebuild in ~{left}s if the host stays busy; a \
                 release-artifact fetch would not be deferred, #8252)"
            )));
        }
        log::warn!(
            "auto_update: gate 4 has deferred the rebuild for {}s with {in_flight} in-flight \
             sweep(s) — exceeding the {}s deadline; rebuilding at reduced priority so the \
             update is not starved by a permanently saturated host",
            waited.as_secs(),
            defer_deadline.as_secs()
        );
        Ok(true)
    }

    /// Record the outcome of a rebuild attempt (and, for a success, whether the
    /// subsequent drain was accepted). Updates backoff / terminal / last-roll.
    /// `now` is the same monotonic clock [`Self::decide`] reads, so backoff
    /// deadlines and the settle window share one time base. Returns the note to
    /// surface for this tick.
    pub fn record_rebuild(
        &mut self,
        now: Instant,
        outcome: &RebuildOutcome,
        drain_accepted: bool,
    ) -> String {
        self.record_roll(now, outcome, drain_accepted, RollKind::Rebuild)
    }

    /// [`Self::record_rebuild`] for an ARTIFACT roll (Issue #7609): identical
    /// backoff/terminal/last-roll bookkeeping, plus — on success — persisting
    /// the `(version, published sha256)` pair that suppresses a repeated
    /// same-version convergence fetch (see [`ArtifactRollRecord`]).
    ///
    /// The record is written on **every** successful artifact roll, not only a
    /// convergence one: a newer-version roll lands the same released bytes,
    /// and it is exactly that roll which can leave a post-install-signed
    /// binary whose sha no longer matches the release.
    pub fn record_artifact_roll(
        &mut self,
        now: Instant,
        outcome: &RebuildOutcome,
        drain_accepted: bool,
        info: &ArtifactInfo,
    ) -> String {
        let note = self.record_roll(now, outcome, drain_accepted, RollKind::ArtifactFetch);
        if matches!(outcome, RebuildOutcome::Success) {
            if let Some(asset_sha256) = info.asset_sha256.clone().filter(|s| !s.is_empty()) {
                let record = ArtifactRollRecord {
                    version: info.version.clone(),
                    asset_sha256,
                    rolled_at: Utc::now(),
                };
                store_artifact_roll_record(self.artifact_record_path.as_deref(), &record);
                self.last_artifact_roll = Some(record);
            }
        }
        note
    }

    fn record_roll(
        &mut self,
        now: Instant,
        outcome: &RebuildOutcome,
        drain_accepted: bool,
        kind: RollKind,
    ) -> String {
        let installed = kind.installed_verb();
        let attempt = kind.attempt_noun();
        match outcome {
            RebuildOutcome::Success => {
                // A completed build re-arms gate 4's deadline (#4929): if this
                // roll did not restart the process (refused drain), the next
                // forced-under-load rebuild waits a full deadline again rather
                // than repeating every tick on a saturated host.
                self.deferred_since = None;
                if drain_accepted {
                    self.consecutive_failures = 0;
                    self.backoff_until = None;
                    self.backoff = None;
                    self.last_roll = Some(Utc::now());
                    format!("{installed} + provisioned; drain-and-restart triggered")
                } else {
                    // The binary IS provisioned, but the drain was refused
                    // (e.g. no supervisor). Do not treat as a build failure —
                    // launchd will pick up the fresh binary on the next
                    // supervised restart. Surface it without backing off.
                    self.last_roll = Some(Utc::now());
                    format!(
                        "{installed} + provisioned, but drain-and-restart was refused (no \
                         supervisor?) — restart manually to run the fresh binary"
                    )
                }
            }
            RebuildOutcome::Retryable(msg) => {
                self.consecutive_failures = self.consecutive_failures.saturating_add(1);
                let delay = backoff_delay(self.consecutive_failures);
                self.backoff = Some(delay);
                self.backoff_until = Some(now + delay);
                format!(
                    "{attempt} failed (attempt {}, backing off {}s): {msg}",
                    self.consecutive_failures,
                    delay.as_secs()
                )
            }
            RebuildOutcome::Terminal(msg) => {
                self.terminal_reason = Some(msg.clone());
                self.backoff_until = None;
                self.backoff = None;
                format!("{attempt} TERMINALLY failed — not retrying until a new commit: {msg}")
            }
        }
    }

    /// Build the status snapshot published after this tick.
    fn snapshot(
        &self,
        enabled: bool,
        last_check: DateTime<Utc>,
        note: String,
        artifact: &ArtifactResolution,
    ) -> AutoUpdateStatusSnapshot {
        let (artifact_version, artifact_published_at) = match artifact {
            ArtifactResolution::Resolved(info) => {
                (Some(info.version.clone()), info.published_at.clone())
            }
            ArtifactResolution::Unresolved(_) => (None, None),
        };
        AutoUpdateStatusSnapshot {
            enabled,
            last_check: Some(last_check),
            last_roll: self.last_roll,
            consecutive_failures: self.consecutive_failures,
            backoff_secs: self.backoff.map(|d| d.as_secs()),
            terminal_reason: self.terminal_reason.clone(),
            note: Some(note),
            artifact_version,
            artifact_published_at,
            stale_repo_ticks: self.stale_repo.ticks(),
            stale_repo: self.stale_repo.repo(),
        }
    }
}

/// Which kind of roll a [`RebuildOutcome`] came from — it changes only the
/// wording of the surfaced note (`rebuilt` vs. `fetched`), never the
/// backoff/terminal/last-roll bookkeeping, which is identical by design
/// (Issue #7609: "the existing settle / in-flight-sweep deferral /
/// defer-deadline / backoff / terminal logic applies to artifact rolls exactly
/// as to rebuilds").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RollKind {
    /// `cargo build --release` in the source checkout.
    Rebuild,
    /// Fetch + verify + provision a published release artifact.
    ArtifactFetch,
}

impl RollKind {
    /// Past-tense verb for a successful roll's note.
    fn installed_verb(self) -> &'static str {
        match self {
            Self::Rebuild => "rebuilt",
            Self::ArtifactFetch => "fetched release artifact",
        }
    }

    /// Noun naming the attempt in a failure note.
    fn attempt_noun(self) -> &'static str {
        match self {
            Self::Rebuild => "rebuild",
            Self::ArtifactFetch => "artifact fetch",
        }
    }
}

/// The settle-window / backoff identity of one resolved artifact: version plus
/// the published sha256 when known (so a re-published release under the same
/// tag counts as a new target), else the tag.
#[must_use]
fn artifact_target_id(version: &str, info: &ArtifactInfo) -> String {
    let discriminator = info
        .asset_sha256
        .as_deref()
        .filter(|s| !s.is_empty())
        .unwrap_or(&info.tag);
    format!("artifact:{version}:{discriminator}")
}

/// First 12 hex characters of a sha256, for log lines. Short shas are returned
/// whole rather than padded.
#[must_use]
fn short_sha(sha: &str) -> &str {
    sha.get(..12).unwrap_or(sha)
}

/// Exponential backoff with a ceiling: `min(BASE * 2^(failures-1), CEILING)`.
/// Saturating — a large failure count never overflows the shift.
#[must_use]
fn backoff_delay(failures: u32) -> Duration {
    if failures <= 1 {
        return BACKOFF_BASE;
    }
    let shift = failures - 1;
    // Cap the shift so `BASE.as_secs() << shift` never overflows; the ceiling
    // clamps anything past it anyway.
    let base = BACKOFF_BASE.as_secs();
    let scaled = base.checked_shl(shift).unwrap_or(u64::MAX);
    Duration::from_secs(scaled.min(BACKOFF_CEILING.as_secs()))
}

/// Append the first three `paths` (plus a count of any remainder) to `reason`
/// (Issue #7608) — e.g. `"<reason> — dirty paths (2): foo.rs, bar.rs"`, or
/// `"... (5): a, b, c, +2 more"` past three. Returns `reason` unchanged when
/// `paths` is empty (no path detail available, e.g. the probe couldn't
/// re-resolve them).
#[must_use]
fn with_dirty_paths(reason: &str, paths: Vec<String>) -> String {
    if paths.is_empty() {
        return reason.to_string();
    }
    let shown: Vec<&str> = paths.iter().take(3).map(String::as_str).collect();
    let remainder = paths.len().saturating_sub(shown.len());
    let suffix = if remainder > 0 {
        format!(", +{remainder} more")
    } else {
        String::new()
    };
    format!("{reason} — dirty paths ({}): {}{suffix}", paths.len(), shown.join(", "))
}

// ============================================================================
// Runtime wiring
// ============================================================================

/// Run one full tick: publish `last_check`, probe, decide, and — on a `Rebuild`
/// decision — rebuild and (on success) trigger the drain-and-restart. Pure of
/// spawning concerns so tests can drive it directly. Returns the note surfaced.
fn run_tick<P: AutoUpdateProbe, T: DrainTrigger>(
    state: &mut AutoUpdateState,
    status: &AutoUpdateStatus,
    probe: &mut P,
    trigger: &T,
    settle: Duration,
    defer_deadline: Duration,
) {
    let now = Instant::now();
    let last_check = Utc::now();
    // Issue #6007: cooperate with the drain rather than racing it. A roll that is
    // already armed — including one *retained* across a refused deadline
    // (dispatch paused, restart re-arming itself at quiescence) — needs no second
    // rebuild; the binary is provisioned and the restart is coming.
    if trigger.roll_in_progress() {
        let note = "a drain-and-restart roll is already armed (dispatch paused, waiting for \
                    in-flight sweeps to reach zero) — skipping this tick"
            .to_string();
        log::info!("auto_update: {note}");
        status.publish(state.snapshot(
            true,
            last_check,
            note,
            &ArtifactResolution::Unresolved(
                "roll already armed — not resolved this tick".to_string(),
            ),
        ));
        return;
    }
    // Issue #7609: the artifact question is asked FIRST and answered
    // independently of the source checkout — it is the whole point that a
    // missing/dirty/stale checkout says nothing about whether a newer signed
    // binary exists.
    let artifact = probe.resolve_artifact();
    let check = probe.check();
    let tree_clean = probe.is_tree_clean().unwrap_or(false);
    // Only pay for the in-flight count once a roll is actually on the table (it
    // loads the workspace registry from disk); a cheap pre-filter avoids that
    // read on every up-to-date tick.
    let in_flight = if artifact.is_actionable() || check.update_available == Some(true) {
        probe.in_flight_sweeps()
    } else {
        0
    };

    // Issue #6261: the 2026-08-14 incident's diagnostic gap wasn't just the
    // gate-reset bugs above — it was that NOTHING surfaced the staleness
    // proactively. `daemon status`'s `Self-update:` line (client-side,
    // `crate::self_update::check()`) already renders this when queried live,
    // but a day-long incident needs a signal that reaches the daemon's own
    // log without anyone asking. Logged every tick the threshold is crossed
    // (bounded by `interval`, default 900s — not spammy).
    if let Some(warning) =
        crate::self_update::staleness_warning_default(check.commits_behind, check.hours_behind)
    {
        log::warn!("auto_update: {warning}");
    }

    let inputs = TickInputs {
        artifact: &artifact,
        check: &check,
        tree_clean,
        in_flight,
    };
    let note = match state.decide(now, &inputs, settle, defer_deadline) {
        TickDecision::Skip(reason) => {
            // Issue #7608: name the offending paths behind a dirty-tree
            // refusal — the generic reason alone gave no way to tell an
            // actual tracked-input change from unrelated untracked litter
            // (both looked identical in the log before #7608), which is how
            // a stray `pnpm-lock.yaml` stalled unattended rebuilds for weeks.
            // `ends_with` rather than `==` because the source path's reasons
            // are now prefixed with the artifact-resolution failure (#7609).
            let reason = if !tree_clean && reason.ends_with(DIRTY_TREE_REASON) {
                with_dirty_paths(&reason, probe.tree_dirty_paths())
            } else {
                reason
            };
            // Issue #6261: every tick's decision is now logged, not just a
            // `Rebuild`'s — the 2026-08-14 incident's daemon log had ZERO
            // evidence of why the loop never rolled across a 20-merge day,
            // because a `Skip` only ever reached `daemon status`'s
            // latest-tick `note` field (overwritten every tick, useless
            // unless read live at exactly the right moment).
            log::info!("auto_update: {reason}");
            reason
        }
        TickDecision::SkipWarn(reason) => {
            // Issue #8513: a stale-repo resolution is "do nothing" like a
            // `Skip`, but it is NOT healthy — WARN rather than INFO, so it
            // reaches the daemon's own log at a level an operator actually
            // notices, exactly like the staleness warning above.
            log::warn!("auto_update: {reason}");
            reason
        }
        TickDecision::Rebuild { low_priority } => {
            if low_priority {
                log::info!(
                    "auto_update: source is stale and settled but the host has stayed busy past \
                     the gate-4 deadline ({in_flight} in-flight) — rebuilding at reduced priority"
                );
            } else {
                log::info!(
                    "auto_update: source is stale and settled with 0 in-flight sweeps — rebuilding"
                );
            }
            let outcome = probe.rebuild(low_priority);
            let drain_accepted = matches!(outcome, RebuildOutcome::Success) && trigger.trigger();
            let mut note = state.record_rebuild(now, &outcome, drain_accepted);
            if low_priority {
                note = format!(
                    "{note} [forced past the in-flight gate after the defer deadline; built at \
                     reduced priority]"
                );
            }
            note = relaunch_verify_note::with_relaunch_verify_note(note, drain_accepted);
            log_roll_outcome(&outcome, &note);
            note
        }
        TickDecision::FetchArtifact {
            version,
            tag,
            why,
            low_priority,
        } => {
            // Issue #7609's per-decision log line: names the path (artifact),
            // the cause (`why`), and — because this is the line an operator
            // reads when a host is stuck — that no source build can happen
            // here even if the checkout is dirty or absent.
            log::info!(
                "auto_update: {why} — fetching release artifact {tag} ({version}) with the \
                 source-rebuild fallback disabled{}",
                if low_priority {
                    // Issue #8252: a busy host no longer postpones the fetch
                    // behind the rebuild-stampede gate — it only nices it.
                    " (host busy; fetching now at reduced priority rather than deferring — the \
                     build-stampede gate applies to rebuilds only)"
                } else {
                    ""
                }
            );
            let outcome = probe.fetch_artifact(low_priority);
            let drain_accepted = matches!(outcome, RebuildOutcome::Success) && trigger.trigger();
            let info = match &artifact {
                ArtifactResolution::Resolved(info) => info.clone(),
                // Unreachable: a `FetchArtifact` decision is only ever
                // produced from a `Resolved` artifact.
                ArtifactResolution::Unresolved(_) => ArtifactInfo::default(),
            };
            let mut note = state.record_artifact_roll(now, &outcome, drain_accepted, &info);
            if low_priority {
                note = format!(
                    "{note} [{in_flight} in-flight sweep(s): fetched immediately at reduced \
                     priority — the in-flight gate defers rebuilds only]"
                );
            }
            note = relaunch_verify_note::with_relaunch_verify_note(note, drain_accepted);
            log_roll_outcome(&outcome, &note);
            note
        }
    };

    status.publish(state.snapshot(true, last_check, note, &artifact));
}

/// Log a roll's outcome at the severity its kind warrants — a terminal failure
/// is an error, everything else a warning (a successful roll is a warning
/// because it means the daemon is about to restart).
fn log_roll_outcome(outcome: &RebuildOutcome, note: &str) {
    match outcome {
        RebuildOutcome::Success | RebuildOutcome::Retryable(_) => {
            log::warn!("auto_update: {note}");
        }
        RebuildOutcome::Terminal(_) => log::error!("auto_update: {note}"),
    }
}

/// Spawn the **single** process-global auto-update loop on the shared daemon
/// runtime (Issue #4055). Registers `status` as the process-global so
/// `loom-daemon status` can render it, then ticks every `interval`, moving the
/// per-tick blocking work (git/cargo subprocesses, registry reads) onto
/// `spawn_blocking` so it never parks a runtime worker.
///
/// Unlike the sibling autonomous loops this is **not** a `spawn_multi_*`
/// per-workspace fan-out: the daemon has one binary and one source checkout, so
/// exactly one loop runs regardless of how many workspaces are registered.
pub fn spawn_auto_update_task<P, T>(
    mut probe: P,
    mut trigger: T,
    status: Arc<AutoUpdateStatus>,
    interval: Duration,
    settle: Duration,
    defer_deadline: Duration,
) -> tokio::task::JoinHandle<()>
where
    P: AutoUpdateProbe + Send + 'static,
    T: DrainTrigger + Send + Sync + 'static,
{
    register_global_status(status.clone());
    log::info!(
        "auto_update: starting loop (interval={}s, settle={}s, deferDeadline={}s)",
        interval.as_secs(),
        settle.as_secs(),
        defer_deadline.as_secs()
    );
    tokio::spawn(async move {
        let mut state = AutoUpdateState::new();
        let mut ticker = tokio::time::interval(interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            ticker.tick().await;
            let status_task = status.clone();
            let joined = tokio::task::spawn_blocking(move || {
                run_tick(&mut state, &status_task, &mut probe, &trigger, settle, defer_deadline);
                (state, probe, trigger)
            })
            .await;
            match joined {
                Ok((s, p, t)) => {
                    state = s;
                    probe = p;
                    trigger = t;
                }
                Err(e) => {
                    log::error!(
                        "auto_update: tick task panicked ({e}); stopping loop (the running \
                         daemon is left untouched)"
                    );
                    return;
                }
            }
        }
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests;
