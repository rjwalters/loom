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
//! `cargo build`. Every gate in the list above — settle window, in-flight
//! sweep deferral, defer deadline, backoff, terminal — applies to an artifact
//! roll exactly as to a rebuild.
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

/// How long to wait for the read-only `--resolve-json` artifact query (Issue
/// #7609) before killing it. It makes two or three `gh` calls and downloads a
/// ~65-byte checksum asset, so a minute is generous; the point of the bound is
/// that a hung/rate-limited forge call must degrade to "no artifact resolved"
/// (⇒ source path) instead of parking the tick indefinitely.
const ARTIFACT_RESOLVE_TIMEOUT: Duration = Duration::from_secs(60);

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
            Self::Resolved(info) => {
                !matches!(classify_artifact(info), ArtifactVerdict::UpToDate { .. })
            }
            Self::Unresolved(_) => false,
        }
    }
}

/// What the resolved artifact means for the installed binary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ArtifactVerdict {
    /// The release is a newer version than what is installed.
    Newer {
        /// The installed version, or `None` when no binary was resolvable.
        installed: Option<String>,
        /// The release's version.
        artifact: String,
    },
    /// Same version, different bytes — the host built this version from source
    /// before the release existed (or the binary was re-signed locally after
    /// install). Fetching converges it onto the released, verified bytes.
    ShaDiffers {
        /// The (shared) version.
        version: String,
        /// The release's published sha256 — also the convergence key recorded
        /// in [`ArtifactRollRecord`], so one unsuccessful convergence cannot
        /// turn into a fetch/restart loop.
        asset_sha256: String,
        /// The installed binary's own sha256.
        installed_sha256: String,
    },
    /// Nothing to do.
    UpToDate {
        /// The release's version.
        version: String,
        /// Why there is nothing to do (sha matched, release is older, or the
        /// comparison could not be made).
        why: String,
    },
}

/// Compare two dotted-numeric versions the same way the update script's
/// `semver_compare` does: up to three components, non-numeric characters
/// stripped defensively, missing components treated as `0`. Deliberately NOT a
/// full semver implementation — the daemon's own versions are always
/// `MAJOR.MINOR.PATCH`, and disagreeing with the shell comparison that drives
/// the actual fetch would be worse than being simplistic.
#[must_use]
fn compare_versions(a: &str, b: &str) -> std::cmp::Ordering {
    fn component(s: Option<&str>) -> u64 {
        s.map(|part| {
            part.chars()
                .filter(char::is_ascii_digit)
                .collect::<String>()
        })
        .and_then(|digits| digits.parse::<u64>().ok())
        .unwrap_or(0)
    }
    let mut left = a.split('.');
    let mut right = b.split('.');
    for _ in 0..3 {
        let ord = component(left.next()).cmp(&component(right.next()));
        if ord != std::cmp::Ordering::Equal {
            return ord;
        }
    }
    std::cmp::Ordering::Equal
}

/// Classify a resolved artifact against the installed binary. Pure — no I/O,
/// no state — so the whole decision matrix is unit-testable with plain values.
#[must_use]
pub fn classify_artifact(info: &ArtifactInfo) -> ArtifactVerdict {
    let installed = info
        .installed_version
        .as_deref()
        .map(str::trim)
        .filter(|v| !v.is_empty());
    let Some(installed) = installed else {
        // No resolvable installed binary at all — any published artifact is
        // strictly better than nothing (this is the update script's own
        // "no loom-daemon binary currently resolvable ⇒ update needed" rule).
        return ArtifactVerdict::Newer {
            installed: None,
            artifact: info.version.clone(),
        };
    };
    match compare_versions(&info.version, installed) {
        std::cmp::Ordering::Greater => ArtifactVerdict::Newer {
            installed: Some(installed.to_string()),
            artifact: info.version.clone(),
        },
        std::cmp::Ordering::Less => ArtifactVerdict::UpToDate {
            version: info.version.clone(),
            why: format!(
                "latest release {} is OLDER than the installed {installed} — nothing to fetch",
                info.version
            ),
        },
        std::cmp::Ordering::Equal => {
            let asset = info
                .asset_sha256
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty());
            let local = info
                .installed_sha256
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty());
            match (asset, local) {
                (Some(asset), Some(local)) if asset.eq_ignore_ascii_case(local) => {
                    ArtifactVerdict::UpToDate {
                        version: info.version.clone(),
                        why: "artifact == installed, sha matches".to_string(),
                    }
                }
                (Some(asset), Some(local)) => ArtifactVerdict::ShaDiffers {
                    version: info.version.clone(),
                    asset_sha256: asset.to_string(),
                    installed_sha256: local.to_string(),
                },
                // One side's checksum is unknown, so "same bytes?" cannot be
                // answered. Treat as converged rather than guessing: a wrong
                // "differs" here would re-fetch (and restart) on every single
                // tick forever, which is far worse than a missed convergence.
                _ => ArtifactVerdict::UpToDate {
                    version: info.version.clone(),
                    why: "artifact == installed, but no published/installed checksum is available \
                          to compare — assuming converged"
                        .to_string(),
                },
            }
        }
    }
}

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

    /// The checkout the update script is *invoked from* on the ARTIFACT path
    /// (Issue #7609): the build-time source checkout when it is still present,
    /// else this daemon's own workspace root.
    ///
    /// The fallback matters precisely because of the hosts this issue exists
    /// for: a daemon provisioned from a release artifact — or one whose
    /// `CARGO_MANIFEST_DIR` checkout has moved — has NO
    /// [`crate::self_update::source_checkout_root`], and would otherwise have
    /// no script to run even though fetching a newer artifact needs nothing
    /// from a source tree but the script itself. It is deliberately NOT used
    /// for [`Self::rebuild`]: a source build must happen in the checkout the
    /// binary was built from, never in some other repo that merely happens to
    /// be registered.
    fn script_root(&self) -> Option<PathBuf> {
        if let Some(root) = self.source_root.clone() {
            if Self::resolve_script(&root).is_some() {
                return Some(root);
            }
        }
        if Self::resolve_script(&self.fallback_root).is_some() {
            return Some(self.fallback_root.clone());
        }
        None
    }
}

impl AutoUpdateProbe for ScriptAutoUpdateProbe {
    fn resolve_artifact(&self) -> ArtifactResolution {
        let Some(root) = self.script_root() else {
            return ArtifactResolution::Unresolved(
                "no checkout with a loom-daemon-update.sh could be resolved (neither the \
                 build-time source checkout nor this daemon's workspace root)"
                    .to_string(),
            );
        };
        let Some(script) = Self::resolve_script(&root) else {
            return ArtifactResolution::Unresolved(format!(
                "loom-daemon-update.sh not found under {}",
                root.display()
            ));
        };
        match run_resolve_json(&script, &root, ARTIFACT_RESOLVE_TIMEOUT) {
            Ok(stdout) => parse_resolve_json(&stdout),
            Err(reason) => ArtifactResolution::Unresolved(reason),
        }
    }

    fn fetch_artifact(&mut self, low_priority: bool) -> RebuildOutcome {
        let Some(root) = self.script_root() else {
            return RebuildOutcome::Retryable(
                "no checkout with a loom-daemon-update.sh could be resolved — cannot fetch"
                    .to_string(),
            );
        };
        let Some(script) = Self::resolve_script(&root) else {
            return RebuildOutcome::Retryable(format!(
                "loom-daemon-update.sh not found under {}",
                root.display()
            ));
        };
        run_update_script_with(&script, &root, self.timeout, low_priority, &["--fetch"])
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
    run_update_script_with(script, cwd, timeout, low_priority, &[])
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
fn run_update_script_with(
    script: &Path,
    cwd: &Path,
    timeout: Duration,
    low_priority: bool,
    extra_args: &[&str],
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

/// Run `loom-daemon-update.sh --resolve-json` in `cwd` and return its stdout
/// (the single JSON object) — or an `Err` reason when it could not be run.
///
/// Read-only by contract on the script's side (no download of the binary, no
/// `git fetch`, no build/provision/restart), so this is safe to call on every
/// tick. stdout is captured to a temp file rather than a pipe for the same
/// reason [`run_update_script`] does: a chatty child on a pipe with nobody
/// draining it deadlocks. **The exit code is deliberately ignored** — the
/// script exits `1` for the entirely ordinary "no release resolved" case and
/// still prints the JSON, so the JSON is the contract, not the status.
fn run_resolve_json(script: &Path, cwd: &Path, timeout: Duration) -> Result<String, String> {
    let out_path = std::env::temp_dir()
        .join(format!("loom-auto-update-resolve-{}.json", uuid::Uuid::new_v4()));
    let out_file = std::fs::File::create(&out_path)
        .map_err(|e| format!("could not create the resolve output file: {e}"))?;

    let mut command = Command::new(script);
    command
        .arg("--resolve-json")
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::from(out_file))
        .stderr(Stdio::null());

    let mut child = match command.spawn() {
        Ok(c) => c,
        Err(e) => {
            let _ = std::fs::remove_file(&out_path);
            return Err(format!("could not spawn `{} --resolve-json`: {e}", script.display()));
        }
    };

    let start = Instant::now();
    let result = loop {
        match child.try_wait() {
            Ok(Some(_status)) => break Ok(()),
            Ok(None) => {
                if start.elapsed() >= timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    break Err(format!(
                        "`{} --resolve-json` timed out after {}s",
                        script.display(),
                        timeout.as_secs()
                    ));
                }
                std::thread::sleep(REBUILD_POLL_INTERVAL);
            }
            Err(e) => break Err(format!("could not poll `{}`: {e}", script.display())),
        }
    };
    let stdout = std::fs::read_to_string(&out_path).unwrap_or_default();
    let _ = std::fs::remove_file(&out_path);
    result.map(|()| stdout)
}

/// Parse `--resolve-json`'s single JSON object into an [`ArtifactResolution`].
/// Any shape surprise (unparseable, `ok:false`, a missing version) becomes
/// `Unresolved` with a reason rather than an error: "we could not learn about
/// a newer artifact" must always degrade to the source path, never to a
/// failure that stalls the loop.
#[must_use]
fn parse_resolve_json(stdout: &str) -> ArtifactResolution {
    let line = stdout
        .lines()
        .map(str::trim)
        .find(|l| l.starts_with('{'))
        .unwrap_or("");
    if line.is_empty() {
        return ArtifactResolution::Unresolved(
            "`loom-daemon-update.sh --resolve-json` printed no JSON object".to_string(),
        );
    }
    let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
        return ArtifactResolution::Unresolved(
            "`loom-daemon-update.sh --resolve-json` printed unparseable JSON".to_string(),
        );
    };
    let string_field = |key: &str| {
        value
            .get(key)
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    if value.get("ok").and_then(serde_json::Value::as_bool) != Some(true) {
        return ArtifactResolution::Unresolved(
            string_field("reason").unwrap_or_else(|| "no release artifact resolved".to_string()),
        );
    }
    let Some(version) = string_field("version") else {
        return ArtifactResolution::Unresolved(
            "release resolution reported ok but no version".to_string(),
        );
    };
    ArtifactResolution::Resolved(ArtifactInfo {
        tag: string_field("tag").unwrap_or_else(|| version.clone()),
        version,
        published_at: string_field("published_at"),
        asset_sha256: string_field("asset_sha256"),
        target: string_field("target"),
        // The script reports the literal string "unknown" for a commit it
        // could not read; a version it could not read is already `null`.
        installed_version: string_field("installed_version").filter(|v| v != "unknown"),
        installed_sha256: string_field("installed_sha256"),
    })
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
        /// Same meaning as on [`Self::Rebuild`].
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
                self.decide_artifact(now, info, in_flight, settle, defer_deadline)
            }
            ArtifactResolution::Unresolved(reason) => {
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
    /// terminal / backoff / settle / in-flight gates a rebuild goes through.
    ///
    /// The clean-tree gate is deliberately NOT applied here: it exists because
    /// an unattended `cargo build --release` would compile whatever is
    /// uncommitted in the operator's checkout into the running daemon. A fetch
    /// of a published, checksum-verified artifact reads nothing from the
    /// working tree, so a stray untracked file there has no bearing on it.
    fn decide_artifact(
        &mut self,
        now: Instant,
        info: &ArtifactInfo,
        in_flight: usize,
        settle: Duration,
        defer_deadline: Duration,
    ) -> TickDecision {
        let (target, why) = match classify_artifact(info) {
            ArtifactVerdict::UpToDate { version, why } => {
                self.clear_tracking();
                return TickDecision::Skip(format!("artifact {version}: {why} → up to date"));
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
        if in_flight == 0 {
            self.deferred_since = None;
        }
        if let Some(skip) = self.terminal_or_backoff_gate(now) {
            return skip;
        }
        if let Some(skip) = self.settle_gate(now, settle) {
            return skip;
        }
        match self.in_flight_gate(now, in_flight, defer_deadline) {
            Err(skip) => skip,
            Ok(low_priority) => TickDecision::FetchArtifact {
                version: info.version.clone(),
                tag: info.tag.clone(),
                why,
                low_priority,
            },
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
                "{in_flight} in-flight sweep(s) — deferring rebuild to avoid a build stampede \
                 (forcing a low-priority rebuild in ~{left}s if the host stays busy)"
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
            note = with_relaunch_verify_note(note, drain_accepted);
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
                    " (host busy past the gate-4 deadline; running at reduced priority)"
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
                    "{note} [forced past the in-flight gate after the defer deadline; fetched at \
                     reduced priority]"
                );
            }
            note = with_relaunch_verify_note(note, drain_accepted);
            log_roll_outcome(&outcome, &note);
            note
        }
    };

    status.publish(state.snapshot(true, last_check, note, &artifact));
}

/// Append the expected relaunch path + detached-verifier bound (Issue #6969
/// AC2) to a roll's outcome note when this tick actually triggered a
/// drain-and-restart — so the auto-update roll's OWN "drain-and-restart
/// triggered" log line states which relaunch mechanism is expected and by
/// when a future gap (like the ~4-minute launchd observation that motivated
/// this) becomes attributable from the log alone, without having to
/// cross-reference the later `run_drain_supervisor` drain-complete line.
///
/// A no-op when `drain_accepted` is `false` (nothing was triggered — the note
/// already says so) or the host has no recognized supervisor (nothing to
/// verify against, mirroring every other best-effort branch in this module).
fn with_relaunch_verify_note(note: String, drain_accepted: bool) -> String {
    if !drain_accepted {
        return note;
    }
    let Some(supervisor) = crate::ipc::detect_supervisor() else {
        return note;
    };
    let verify_poll_secs = crate::restart_verify::resolve_secs(
        std::env::var(crate::restart_verify::POLL_SECS_ENV)
            .ok()
            .as_deref(),
        crate::restart_verify::DEFAULT_POLL_SECS,
    );
    format!("{note} {}", crate::ipc::relaunch_verify_note(&supervisor, verify_poll_secs))
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
mod tests {
    use super::*;
    use serial_test::serial;
    use std::fs;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn write_config(root: &Path, contents: &str) {
        fs::create_dir_all(root.join(".loom")).unwrap();
        fs::write(root.join(".loom").join("config.json"), contents).unwrap();
    }

    fn write_project_config(root: &Path, contents: &str) {
        let full = root.join(crate::config_resolver::PROJECT_CONFIG_REL);
        fs::create_dir_all(full.parent().unwrap()).unwrap();
        fs::write(full, contents).unwrap();
    }

    fn stale(commit: &str) -> UpdateCheck {
        UpdateCheck {
            update_available: Some(true),
            source_commit: Some(commit.to_string()),
            commits_behind: None,
            hours_behind: None,
        }
    }

    fn stale_with_lag(commit: &str, commits_behind: u32, hours_behind: u32) -> UpdateCheck {
        UpdateCheck {
            commits_behind: Some(commits_behind),
            hours_behind: Some(hours_behind),
            ..stale(commit)
        }
    }

    // ===================================================================
    // Config surface — autonomous.autoUpdate (soft-fail + happy path)
    // ===================================================================

    #[test]
    #[serial(loom_config_env)]
    fn test_config_missing_file_is_default() {
        std::env::set_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV, "");
        let tmp = tempfile::tempdir().unwrap();
        let cfg = read_auto_update_config(tmp.path());
        std::env::remove_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV);
        assert_eq!(cfg, AutoUpdateConfig::default());
    }

    #[test]
    #[serial(loom_config_env)]
    fn test_config_malformed_json_is_default() {
        std::env::set_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV, "");
        let tmp = tempfile::tempdir().unwrap();
        write_config(tmp.path(), "{not valid json");
        let cfg = read_auto_update_config(tmp.path());
        std::env::remove_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV);
        assert_eq!(cfg, AutoUpdateConfig::default());
    }

    #[test]
    #[serial(loom_config_env)]
    fn test_config_missing_block_is_default() {
        std::env::set_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV, "");
        let tmp = tempfile::tempdir().unwrap();
        write_config(tmp.path(), r#"{"autonomous": {"workFinder": {"enabled": true}}}"#);
        let cfg = read_auto_update_config(tmp.path());
        std::env::remove_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV);
        assert_eq!(cfg, AutoUpdateConfig::default());
    }

    #[test]
    fn test_config_reads_all_fields() {
        let tmp = tempfile::tempdir().unwrap();
        write_config(
            tmp.path(),
            r#"{"autonomous": {"autoUpdate": {"enabled": true, "intervalSecs": 120, "settleSecs": 30, "deferDeadlineSecs": 7200}}}"#,
        );
        assert_eq!(
            read_auto_update_config(tmp.path()),
            AutoUpdateConfig {
                enabled: Some(true),
                interval_secs: Some(120),
                settle_secs: Some(30),
                defer_deadline_secs: Some(7200),
            }
        );
    }

    #[test]
    fn test_config_zero_values_dropped_to_none() {
        let tmp = tempfile::tempdir().unwrap();
        write_config(
            tmp.path(),
            r#"{"autonomous": {"autoUpdate": {"intervalSecs": 0, "settleSecs": 0, "deferDeadlineSecs": 0}}}"#,
        );
        let cfg = read_auto_update_config(tmp.path());
        assert_eq!(cfg.interval_secs, None);
        assert_eq!(cfg.settle_secs, None);
        assert_eq!(cfg.defer_deadline_secs, None);
    }

    // ===================================================================
    // config_resolver migration (#4058) — .loom-project/ tier
    // ===================================================================

    #[test]
    #[serial(loom_config_env)]
    fn test_config_project_tier_only_is_honored() {
        std::env::set_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV, "");
        let tmp = tempfile::tempdir().unwrap();
        write_project_config(
            tmp.path(),
            r#"{"autonomous": {"autoUpdate": {"enabled": true, "intervalSecs": 120}}}"#,
        );
        let cfg = read_auto_update_config(tmp.path());
        std::env::remove_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV);
        assert_eq!(cfg.enabled, Some(true));
        assert_eq!(cfg.interval_secs, Some(120));
    }

    #[test]
    #[serial(loom_config_env)]
    fn test_config_project_tier_overrides_legacy() {
        std::env::set_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV, "");
        let tmp = tempfile::tempdir().unwrap();
        write_config(
            tmp.path(),
            r#"{"autonomous": {"autoUpdate": {"enabled": true, "settleSecs": 600}}}"#,
        );
        write_project_config(tmp.path(), r#"{"autonomous": {"autoUpdate": {"settleSecs": 30}}}"#);
        let cfg = read_auto_update_config(tmp.path());
        std::env::remove_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV);
        // Overlapping settleSecs -> project tier wins; non-overlapping enabled
        // still supplied by the legacy tier.
        assert_eq!(cfg.settle_secs, Some(30));
        assert_eq!(cfg.enabled, Some(true));
    }

    // ===================================================================
    // Precedence — env > config > default
    // ===================================================================

    #[test]
    #[serial]
    fn test_resolve_enabled_default_is_false() {
        std::env::remove_var(AUTO_UPDATE_ENABLE_ENV);
        assert!(
            !resolve_enabled(&AutoUpdateConfig::default()),
            "absent config + unset env ⇒ default OFF (opt-in loop)"
        );
    }

    #[test]
    #[serial]
    fn test_resolve_enabled_config_then_env() {
        std::env::remove_var(AUTO_UPDATE_ENABLE_ENV);
        assert!(resolve_enabled(&AutoUpdateConfig {
            enabled: Some(true),
            ..AutoUpdateConfig::default()
        }));
        // Env forces OFF over config-on.
        std::env::set_var(AUTO_UPDATE_ENABLE_ENV, "0");
        assert!(!resolve_enabled(&AutoUpdateConfig {
            enabled: Some(true),
            ..AutoUpdateConfig::default()
        }));
        // Env forces ON over config-off.
        std::env::set_var(AUTO_UPDATE_ENABLE_ENV, "1");
        assert!(resolve_enabled(&AutoUpdateConfig {
            enabled: Some(false),
            ..AutoUpdateConfig::default()
        }));
        std::env::remove_var(AUTO_UPDATE_ENABLE_ENV);
    }

    #[test]
    #[serial]
    fn test_resolve_interval_and_settle_precedence() {
        std::env::remove_var(AUTO_UPDATE_INTERVAL_ENV);
        std::env::remove_var(AUTO_UPDATE_SETTLE_ENV);

        // Defaults.
        assert_eq!(
            resolve_interval(&AutoUpdateConfig::default()),
            Duration::from_secs(DEFAULT_AUTO_UPDATE_INTERVAL_SECS)
        );
        assert_eq!(
            resolve_settle(&AutoUpdateConfig::default()),
            Duration::from_secs(DEFAULT_AUTO_UPDATE_SETTLE_SECS)
        );

        // Config alone.
        let cfg = AutoUpdateConfig {
            enabled: None,
            interval_secs: Some(300),
            settle_secs: Some(45),
            defer_deadline_secs: None,
        };
        assert_eq!(resolve_interval(&cfg), Duration::from_secs(300));
        assert_eq!(resolve_settle(&cfg), Duration::from_secs(45));

        // Env overrides config.
        std::env::set_var(AUTO_UPDATE_INTERVAL_ENV, "77");
        std::env::set_var(AUTO_UPDATE_SETTLE_ENV, "11");
        assert_eq!(resolve_interval(&cfg), Duration::from_secs(77));
        assert_eq!(resolve_settle(&cfg), Duration::from_secs(11));

        // Zero/garbage env falls through to config, not the default.
        std::env::set_var(AUTO_UPDATE_INTERVAL_ENV, "0");
        std::env::set_var(AUTO_UPDATE_SETTLE_ENV, "garbage");
        assert_eq!(resolve_interval(&cfg), Duration::from_secs(300));
        assert_eq!(resolve_settle(&cfg), Duration::from_secs(45));

        std::env::remove_var(AUTO_UPDATE_INTERVAL_ENV);
        std::env::remove_var(AUTO_UPDATE_SETTLE_ENV);
    }

    /// Gate 4's deferral deadline (#4929) resolves **env > config > default**
    /// like every other knob on this block.
    #[test]
    #[serial]
    fn test_resolve_defer_deadline_precedence() {
        std::env::remove_var(AUTO_UPDATE_DEFER_DEADLINE_ENV);
        assert_eq!(
            resolve_defer_deadline(&AutoUpdateConfig::default()),
            Duration::from_secs(DEFAULT_AUTO_UPDATE_DEFER_DEADLINE_SECS)
        );

        let cfg = AutoUpdateConfig {
            defer_deadline_secs: Some(1800),
            ..AutoUpdateConfig::default()
        };
        assert_eq!(resolve_defer_deadline(&cfg), Duration::from_secs(1800));

        std::env::set_var(AUTO_UPDATE_DEFER_DEADLINE_ENV, "60");
        assert_eq!(resolve_defer_deadline(&cfg), Duration::from_secs(60));

        // Zero/garbage env falls through to config, never to "defer forever".
        std::env::set_var(AUTO_UPDATE_DEFER_DEADLINE_ENV, "0");
        assert_eq!(resolve_defer_deadline(&cfg), Duration::from_secs(1800));
        std::env::set_var(AUTO_UPDATE_DEFER_DEADLINE_ENV, "garbage");
        assert_eq!(resolve_defer_deadline(&cfg), Duration::from_secs(1800));

        std::env::remove_var(AUTO_UPDATE_DEFER_DEADLINE_ENV);
    }

    // ===================================================================
    // Backoff math
    // ===================================================================

    #[test]
    fn test_backoff_is_exponential_with_ceiling() {
        assert_eq!(backoff_delay(1), BACKOFF_BASE);
        assert_eq!(backoff_delay(2), Duration::from_secs(120));
        assert_eq!(backoff_delay(3), Duration::from_secs(240));
        // Eventually clamps at the ceiling and never overflows.
        assert_eq!(backoff_delay(10), BACKOFF_CEILING);
        assert_eq!(backoff_delay(u32::MAX), BACKOFF_CEILING);
    }

    // ===================================================================
    // Decision logic — the settle/clean/gate matrix
    // ===================================================================

    const SETTLE: Duration = Duration::from_secs(60);
    /// Gate 4's deferral deadline for the decision tests: long enough that the
    /// existing gate-4 cases still exercise the *deferring* branch.
    const DEFER: Duration = Duration::from_secs(3600);

    #[test]
    fn test_decide_up_to_date_is_skip() {
        let mut st = AutoUpdateState::new();
        let now = Instant::now();
        let check = UpdateCheck {
            update_available: Some(false),
            source_commit: Some("abc".into()),
            commits_behind: None,
            hours_behind: None,
        };
        assert!(matches!(
            st.decide_source(now, &check, true, 0, SETTLE, DEFER),
            TickDecision::Skip(_)
        ));
    }

    #[test]
    fn test_decide_undecidable_none_is_skip() {
        let mut st = AutoUpdateState::new();
        let now = Instant::now();
        let check = UpdateCheck {
            update_available: None,
            source_commit: None,
            commits_behind: None,
            hours_behind: None,
        };
        assert!(matches!(
            st.decide_source(now, &check, true, 0, SETTLE, DEFER),
            TickDecision::Skip(_)
        ));
    }

    #[test]
    fn test_decide_stale_dirty_tree_is_skip() {
        let mut st = AutoUpdateState::new();
        let base = Instant::now();
        // First observe (starts settle timer), then advance past settle.
        st.decide_source(base, &stale("c1"), false, 0, SETTLE, DEFER);
        let later = base + SETTLE + Duration::from_secs(1);
        let d = st.decide_source(later, &stale("c1"), false, 0, SETTLE, DEFER);
        assert!(matches!(d, TickDecision::Skip(reason) if reason.contains("dirty")));
    }

    #[test]
    fn test_decide_stale_clean_within_settle_is_skip() {
        let mut st = AutoUpdateState::new();
        let base = Instant::now();
        let d = st.decide_source(base, &stale("c1"), true, 0, SETTLE, DEFER);
        assert!(matches!(d, TickDecision::Skip(reason) if reason.contains("settle")));
    }

    #[test]
    fn test_decide_stale_clean_settled_zero_inflight_is_rebuild() {
        let mut st = AutoUpdateState::new();
        let base = Instant::now();
        st.decide_source(base, &stale("c1"), true, 0, SETTLE, DEFER);
        let later = base + SETTLE + Duration::from_secs(1);
        assert_eq!(
            st.decide_source(later, &stale("c1"), true, 0, SETTLE, DEFER),
            TickDecision::Rebuild {
                low_priority: false
            }
        );
    }

    #[test]
    fn test_decide_gate4_inflight_sweeps_blocks_rebuild() {
        let mut st = AutoUpdateState::new();
        let base = Instant::now();
        st.decide_source(base, &stale("c1"), true, 3, SETTLE, DEFER);
        let later = base + SETTLE + Duration::from_secs(1);
        let d = st.decide_source(later, &stale("c1"), true, 3, SETTLE, DEFER);
        assert!(matches!(d, TickDecision::Skip(reason) if reason.contains("in-flight")));
    }

    // ===================================================================
    // Gate 4's deferral deadline (#4929) — a permanently saturated host must
    // still converge instead of deferring the rebuild forever.
    // ===================================================================

    /// The starvation case from #4929: the host never reaches zero in-flight
    /// sweeps, so gate 4 defers at every check. Before the deadline it keeps
    /// deferring (unchanged behavior); once the deadline elapses it rebuilds
    /// anyway, at low priority — so `last_roll` can finally become non-null.
    #[test]
    fn test_decide_gate4_deadline_forces_low_priority_rebuild() {
        let mut st = AutoUpdateState::new();
        let base = Instant::now();
        // First observation starts both the settle timer and (once settled) the
        // gate-4 deferral clock.
        st.decide_source(base, &stale("c1"), true, 13, SETTLE, DEFER);

        // Settled, but still busy: deferral begins here.
        let settled = base + SETTLE + Duration::from_secs(1);
        let d = st.decide_source(settled, &stale("c1"), true, 13, SETTLE, DEFER);
        assert!(
            matches!(&d, TickDecision::Skip(reason) if reason.contains("in-flight")),
            "still within the deadline ⇒ defer, got {d:?}"
        );

        // Just short of the deadline: still deferring, and the note counts down.
        let almost = settled + DEFER - Duration::from_secs(1);
        let d = st.decide_source(almost, &stale("c1"), true, 13, SETTLE, DEFER);
        assert!(
            matches!(&d, TickDecision::Skip(reason) if reason.contains("low-priority rebuild in")),
            "one second short of the deadline must still defer, got {d:?}"
        );

        // Past the deadline with the host STILL saturated: rebuild anyway.
        let past = settled + DEFER + Duration::from_secs(1);
        assert_eq!(
            st.decide_source(past, &stale("c1"), true, 13, SETTLE, DEFER),
            TickDecision::Rebuild { low_priority: true },
            "a continuously saturated host must eventually rebuild (#4929)"
        );
    }

    /// The deadline measures *continuous* deferral: a host that dips to zero
    /// in-flight sweeps re-arms it, so a long series of short busy bursts never
    /// accumulates into a forced build-under-load (the "do not trade
    /// never-rebuilds for stampede-on-every-busy-period" edge case).
    #[test]
    fn test_decide_gate4_deadline_resets_when_host_goes_idle() {
        let mut st = AutoUpdateState::new();
        let base = Instant::now();
        st.decide_source(base, &stale("c1"), true, 2, SETTLE, DEFER);

        // Busy for most of the deadline...
        let busy = base + SETTLE + DEFER - Duration::from_secs(1);
        assert!(matches!(
            st.decide_source(busy, &stale("c1"), true, 2, SETTLE, DEFER),
            TickDecision::Skip(_)
        ));

        // ...then one idle observation: that tick rolls normally, at NORMAL
        // priority, because the host is quiescent.
        let idle = busy + Duration::from_secs(1);
        assert_eq!(
            st.decide_source(idle, &stale("c1"), true, 0, SETTLE, DEFER),
            TickDecision::Rebuild {
                low_priority: false
            }
        );

        // And the deferral clock restarted, so a later busy tick defers again
        // rather than immediately forcing a build.
        let busy_again = idle + Duration::from_secs(1);
        assert!(
            matches!(
                st.decide_source(busy_again, &stale("c1"), true, 2, SETTLE, DEFER),
                TickDecision::Skip(reason) if reason.contains("in-flight")
            ),
            "an idle observation must re-arm the gate-4 deadline"
        );
    }

    /// Issue #6261 fix: a new source commit landing while the host has been
    /// CONTINUOUSLY busy must NOT restart gate 4's deferral clock (the
    /// pre-fix behavior this test used to assert, under the name
    /// `test_decide_gate4_deadline_resets_on_new_commit` — that reset is
    /// exactly the bug: a host busy for hours with commits landing
    /// throughout never accumulated toward `deferDeadlineSecs` at all).
    /// `first_stale_since` (also not reset by a new commit) lets the
    /// settle-ceiling carry the tick straight past the settle gate too, so
    /// the already-overdue rebuild fires on the SAME tick the new commit is
    /// observed, rather than deferring for another full settle + deadline.
    #[test]
    fn test_decide_gate4_deadline_persists_across_new_commit_when_continuously_busy() {
        let mut st = AutoUpdateState::new();
        let base = Instant::now();
        st.decide_source(base, &stale("c1"), true, 4, SETTLE, DEFER);
        // The deferral clock only starts once a tick actually reaches gate 4
        // (i.e. past the settle window), so take one settled-but-busy tick.
        let settled = base + SETTLE + Duration::from_secs(1);
        st.decide_source(settled, &stale("c1"), true, 4, SETTLE, DEFER);
        let deep = settled + DEFER + Duration::from_secs(1);
        // c1 would force a rebuild now...
        assert_eq!(
            st.decide_source(deep, &stale("c1"), true, 4, SETTLE, DEFER),
            TickDecision::Rebuild { low_priority: true }
        );
        // ...and a new commit landing on the SAME tick, with the host STILL
        // busy throughout, does not reset the clock: the rebuild it was
        // already overdue for fires immediately instead of deferring again.
        assert_eq!(
            st.decide_source(deep, &stale("c2"), true, 4, SETTLE, DEFER),
            TickDecision::Rebuild { low_priority: true },
            "a new commit while continuously busy must not restart the deferral clock (#6261)"
        );
    }

    /// A successful rebuild re-arms the deadline, so a roll whose drain was
    /// refused does not re-force a build-under-load on every subsequent tick.
    #[test]
    fn test_successful_rebuild_rearms_gate4_deadline() {
        let mut st = AutoUpdateState::new();
        let base = Instant::now();
        st.decide_source(base, &stale("c1"), true, 5, SETTLE, DEFER);
        // One settled-but-busy tick starts gate 4's deferral clock.
        let settled = base + SETTLE + Duration::from_secs(1);
        st.decide_source(settled, &stale("c1"), true, 5, SETTLE, DEFER);
        let past = settled + DEFER + Duration::from_secs(1);
        assert_eq!(
            st.decide_source(past, &stale("c1"), true, 5, SETTLE, DEFER),
            TickDecision::Rebuild { low_priority: true }
        );
        // Provisioned, but the drain was refused ⇒ still reported stale.
        st.record_rebuild(past, &RebuildOutcome::Success, false);
        assert!(st.last_roll.is_some(), "#4929: last_roll must go non-null under saturation");

        let next = past + Duration::from_secs(900);
        let d = st.decide_source(next, &stale("c1"), true, 5, SETTLE, DEFER);
        assert!(
            matches!(&d, TickDecision::Skip(reason) if reason.contains("in-flight")),
            "the next forced rebuild must wait another full deadline, got {d:?}"
        );
    }

    #[test]
    fn test_new_commit_resets_settle_window() {
        let mut st = AutoUpdateState::new();
        let base = Instant::now();
        st.decide_source(base, &stale("c1"), true, 0, SETTLE, DEFER);
        // Settled for c1...
        let later = base + SETTLE + Duration::from_secs(1);
        // ...but a NEW commit lands: the settle timer restarts, so this tick is
        // within-settle again (not a rebuild).
        let d = st.decide_source(later, &stale("c2"), true, 0, SETTLE, DEFER);
        assert!(matches!(d, TickDecision::Skip(reason) if reason.contains("settle")));
    }

    /// Issue #6261: a stream of commits landing MORE OFTEN than the settle
    /// window apart (the 2026-08-14 incident's suspected shape — a 20-merge
    /// day against a 600s settle window) must still converge on a rebuild
    /// within a bounded worst case, rather than deferring the first attempt
    /// forever via repeated `stale_since` resets.
    #[test]
    fn test_repeated_commits_within_settle_still_converge_via_ceiling() {
        let mut st = AutoUpdateState::new();
        let base = Instant::now();
        let mut t = base;
        let mut last = None;
        // Each iteration lands a NEW commit strictly inside the quiet-period
        // window, so the quiet-period test alone would never pass.
        for i in 0..20u32 {
            t += SETTLE - Duration::from_secs(1);
            let commit = format!("c{i}");
            let d = st.decide_source(t, &stale(&commit), true, 0, SETTLE, DEFER);
            let is_rebuild = matches!(d, TickDecision::Rebuild { .. });
            last = Some(d);
            if is_rebuild {
                break;
            }
        }
        assert_eq!(
            last,
            Some(TickDecision::Rebuild {
                low_priority: false
            }),
            "a stream of sub-settle-interval commits must still converge via the ceiling"
        );
        // Bounded: must not take dramatically longer than the documented
        // ceiling (SETTLE_CEILING_MULTIPLIER * SETTLE from the FIRST stale
        // observation), not merely "eventually". `first_stale_since` is set
        // on the FIRST `decide()` call (at `base + (SETTLE - 1s)`, not
        // `base` itself), and the sub-settle-interval step size means the
        // ceiling can be crossed up to one step late — so allow two extra
        // settle windows of slack on top of the ceiling rather than an exact
        // bound.
        assert!(
            t.duration_since(base) <= SETTLE * (SETTLE_CEILING_MULTIPLIER + 2),
            "converged too slowly: {:?} vs. the {:?} ceiling",
            t.duration_since(base),
            SETTLE * SETTLE_CEILING_MULTIPLIER
        );
    }

    // ===================================================================
    // Backoff + terminal state transitions
    // ===================================================================

    #[test]
    fn test_retryable_failures_back_off_then_reset_on_success() {
        let mut st = AutoUpdateState::new();
        let t0 = Instant::now();
        // Establish a tracked commit + settle so backoff_until is meaningful.
        st.decide_source(t0, &stale("c1"), true, 0, SETTLE, DEFER);

        st.record_rebuild(t0, &RebuildOutcome::Retryable("boom".into()), false);
        assert_eq!(st.consecutive_failures, 1);
        assert_eq!(st.backoff, Some(backoff_delay(1)));

        st.record_rebuild(t0, &RebuildOutcome::Retryable("boom".into()), false);
        assert_eq!(st.consecutive_failures, 2);
        assert_eq!(st.backoff, Some(backoff_delay(2)));

        // A success (with an accepted drain) resets the counter + clears backoff.
        st.record_rebuild(t0, &RebuildOutcome::Success, true);
        assert_eq!(st.consecutive_failures, 0);
        assert_eq!(st.backoff, None);
        assert!(st.last_roll.is_some());
    }

    #[test]
    fn test_backing_off_blocks_rebuild_until_delay_elapses() {
        let mut st = AutoUpdateState::new();
        let t0 = Instant::now();
        st.decide_source(t0, &stale("c1"), true, 0, SETTLE, DEFER);
        // A settled rebuild fails at t1; backoff_until = t1 + backoff_delay(1) (60s).
        let t1 = t0 + SETTLE;
        st.record_rebuild(t1, &RebuildOutcome::Retryable("boom".into()), false);
        // t2 is settled (past t0+SETTLE) but still inside the 60s backoff window.
        let t2 = t1 + Duration::from_secs(30);
        let d = st.decide_source(t2, &stale("c1"), true, 0, SETTLE, DEFER);
        assert!(matches!(d, TickDecision::Skip(reason) if reason.contains("backing off")));
        // Once the backoff elapses, the same settled/clean/idle state rebuilds.
        let t3 = t1 + backoff_delay(1) + Duration::from_secs(1);
        assert_eq!(
            st.decide_source(t3, &stale("c1"), true, 0, SETTLE, DEFER),
            TickDecision::Rebuild {
                low_priority: false
            }
        );
    }

    #[test]
    fn test_terminal_is_sticky_until_commit_changes() {
        let mut st = AutoUpdateState::new();
        let t0 = Instant::now();
        st.decide_source(t0, &stale("c1"), true, 0, SETTLE, DEFER);
        st.record_rebuild(t0, &RebuildOutcome::Terminal("commit mismatch (exit 4)".into()), false);
        assert!(st.terminal_reason.is_some());

        // Same commit, fully settled + clean + idle: still skipped (terminal).
        let later = t0 + SETTLE + Duration::from_secs(1);
        let d = st.decide_source(later, &stale("c1"), true, 0, SETTLE, DEFER);
        assert!(matches!(d, TickDecision::Skip(reason) if reason.contains("terminal")));

        // A NEW commit clears the terminal state (fresh attempt).
        st.decide_source(later, &stale("c2"), true, 0, SETTLE, DEFER);
        assert!(st.terminal_reason.is_none());
        assert_eq!(st.consecutive_failures, 0);
    }

    // ===================================================================
    // Exit-code → RebuildOutcome mapping (#4053 exit 4/5 terminal)
    // ===================================================================

    fn write_fake_script(dir: &Path, body: &str) -> PathBuf {
        fs::create_dir_all(dir).unwrap();
        let path = dir.join("fake-update.sh");
        fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = fs::metadata(&path).unwrap().permissions();
            perms.set_mode(0o755);
            fs::set_permissions(&path, perms).unwrap();
        }
        path
    }

    #[test]
    fn test_run_update_script_exit_0_is_success() {
        let tmp = tempfile::tempdir().unwrap();
        let s = write_fake_script(tmp.path(), "echo built; exit 0");
        assert_eq!(
            run_update_script(&s, tmp.path(), Duration::from_secs(10), false),
            RebuildOutcome::Success
        );
    }

    #[test]
    fn test_run_update_script_exit_1_is_retryable() {
        let tmp = tempfile::tempdir().unwrap();
        let s = write_fake_script(tmp.path(), "echo compile error; exit 1");
        let o = run_update_script(&s, tmp.path(), Duration::from_secs(10), false);
        assert!(matches!(o, RebuildOutcome::Retryable(m) if m.contains("compile error")));
    }

    #[test]
    fn test_run_update_script_exit_4_is_terminal() {
        let tmp = tempfile::tempdir().unwrap();
        let s = write_fake_script(tmp.path(), "echo commit mismatch; exit 4");
        let o = run_update_script(&s, tmp.path(), Duration::from_secs(10), false);
        assert!(matches!(o, RebuildOutcome::Terminal(m) if m.contains("commit mismatch")));
    }

    #[test]
    fn test_run_update_script_exit_5_is_terminal() {
        let tmp = tempfile::tempdir().unwrap();
        let s = write_fake_script(tmp.path(), "exit 5");
        assert!(matches!(
            run_update_script(&s, tmp.path(), Duration::from_secs(10), false),
            RebuildOutcome::Terminal(_)
        ));
    }

    #[test]
    fn test_run_update_script_timeout_is_retryable() {
        let tmp = tempfile::tempdir().unwrap();
        let s = write_fake_script(tmp.path(), "sleep 30");
        let o = run_update_script(&s, tmp.path(), Duration::from_millis(300), false);
        assert!(matches!(o, RebuildOutcome::Retryable(m) if m.contains("timed out")));
    }

    #[test]
    fn test_run_update_script_spawn_failure_is_retryable() {
        let tmp = tempfile::tempdir().unwrap();
        let bogus = tmp.path().join("does-not-exist.sh");
        assert!(matches!(
            run_update_script(&bogus, tmp.path(), Duration::from_secs(10), false),
            RebuildOutcome::Retryable(_)
        ));
    }

    // ===================================================================
    // Loop wiring — fake probe + trigger drive run_tick end to end
    // ===================================================================

    struct FakeProbe {
        check: UpdateCheck,
        tree_clean: Option<bool>,
        /// The paths `tree_dirty_paths()` reports (Issue #7608); `vec![]` at
        /// most call sites, which don't exercise the dirty-path detail.
        dirty_paths: Vec<String>,
        in_flight: usize,
        rebuild_outcome: RebuildOutcome,
        rebuild_calls: Arc<AtomicUsize>,
        /// How many of those rebuilds asked for the niced/low-priority build
        /// (the gate-4 deadline override, #4929).
        low_priority_calls: Arc<AtomicUsize>,
    }

    impl AutoUpdateProbe for FakeProbe {
        fn check(&self) -> UpdateCheck {
            self.check.clone()
        }
        fn is_tree_clean(&self) -> Option<bool> {
            self.tree_clean
        }
        fn tree_dirty_paths(&self) -> Vec<String> {
            self.dirty_paths.clone()
        }
        fn in_flight_sweeps(&self) -> usize {
            self.in_flight
        }
        fn rebuild(&mut self, low_priority: bool) -> RebuildOutcome {
            self.rebuild_calls.fetch_add(1, Ordering::SeqCst);
            if low_priority {
                self.low_priority_calls.fetch_add(1, Ordering::SeqCst);
            }
            self.rebuild_outcome.clone()
        }
    }

    struct FakeTrigger {
        accepted: bool,
        calls: Arc<AtomicUsize>,
    }

    impl DrainTrigger for FakeTrigger {
        fn trigger(&self) -> bool {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.accepted
        }
    }

    /// A settled, clean, idle, stale probe rolls exactly once and triggers a
    /// drain — the full happy path through `run_tick`.
    #[test]
    fn test_run_tick_rolls_and_triggers_drain_when_settled() {
        let rebuild_calls = Arc::new(AtomicUsize::new(0));
        let trigger_calls = Arc::new(AtomicUsize::new(0));
        let mut probe = FakeProbe {
            check: stale("c1"),
            tree_clean: Some(true),
            dirty_paths: Vec::new(),
            in_flight: 0,
            rebuild_outcome: RebuildOutcome::Success,
            rebuild_calls: rebuild_calls.clone(),
            low_priority_calls: Arc::new(AtomicUsize::new(0)),
        };
        let trigger = FakeTrigger {
            accepted: true,
            calls: trigger_calls.clone(),
        };
        let status = AutoUpdateStatus::new(true);
        let mut state = AutoUpdateState::new();
        // A zero settle window makes the very first observed-stale tick settled.
        let settle = Duration::from_secs(0);

        run_tick(&mut state, &status, &mut probe, &trigger, settle, DEFER);
        assert_eq!(rebuild_calls.load(Ordering::SeqCst), 1);
        assert_eq!(trigger_calls.load(Ordering::SeqCst), 1);
        let snap = status.snapshot();
        assert!(snap.last_roll.is_some());
        assert_eq!(snap.consecutive_failures, 0);
    }

    /// Issue #6261: `commits_behind`/`hours_behind` are a purely diagnostic
    /// signal (logged when they cross a warn threshold) — they never gate
    /// `decide()`'s logic, so a probe reporting extreme lag rolls through
    /// the exact same gates as one reporting none.
    #[test]
    fn test_run_tick_staleness_lag_does_not_affect_decide_gates() {
        let rebuild_calls = Arc::new(AtomicUsize::new(0));
        let trigger_calls = Arc::new(AtomicUsize::new(0));
        let mut probe = FakeProbe {
            check: stale_with_lag("c1", 500, 900),
            tree_clean: Some(true),
            dirty_paths: Vec::new(),
            in_flight: 0,
            rebuild_outcome: RebuildOutcome::Success,
            rebuild_calls: rebuild_calls.clone(),
            low_priority_calls: Arc::new(AtomicUsize::new(0)),
        };
        let trigger = FakeTrigger {
            accepted: true,
            calls: trigger_calls.clone(),
        };
        let status = AutoUpdateStatus::new(true);
        let mut state = AutoUpdateState::new();
        let settle = Duration::from_secs(0);

        run_tick(&mut state, &status, &mut probe, &trigger, settle, DEFER);
        assert_eq!(rebuild_calls.load(Ordering::SeqCst), 1);
        assert_eq!(trigger_calls.load(Ordering::SeqCst), 1);
    }

    /// Issue #6007 — while a roll is already armed (in particular one *retained*
    /// across a refused deadline: dispatch paused, restart re-arming itself at
    /// quiescence) the loop must not rebuild or re-trigger. The binary is already
    /// provisioned, and a redundant `cargo build` would compete for CPU with the
    /// very in-flight sweeps the pending roll is waiting on.
    #[test]
    fn test_run_tick_skips_while_a_roll_is_already_armed() {
        struct PendingRollTrigger {
            calls: Arc<AtomicUsize>,
        }
        impl DrainTrigger for PendingRollTrigger {
            fn trigger(&self) -> bool {
                self.calls.fetch_add(1, Ordering::SeqCst);
                true
            }
            fn roll_in_progress(&self) -> bool {
                true
            }
        }

        let rebuild_calls = Arc::new(AtomicUsize::new(0));
        let trigger_calls = Arc::new(AtomicUsize::new(0));
        let mut probe = FakeProbe {
            check: stale("c1"),
            tree_clean: Some(true),
            dirty_paths: Vec::new(),
            // Busy host — exactly the shape that made the roll go pending.
            in_flight: 3,
            rebuild_outcome: RebuildOutcome::Success,
            rebuild_calls: rebuild_calls.clone(),
            low_priority_calls: Arc::new(AtomicUsize::new(0)),
        };
        let trigger = PendingRollTrigger {
            calls: trigger_calls.clone(),
        };
        let status = AutoUpdateStatus::new(true);
        let mut state = AutoUpdateState::new();

        run_tick(&mut state, &status, &mut probe, &trigger, Duration::from_secs(0), DEFER);

        assert_eq!(rebuild_calls.load(Ordering::SeqCst), 0, "no redundant rebuild");
        assert_eq!(trigger_calls.load(Ordering::SeqCst), 0, "no redundant drain trigger");
        let snap = status.snapshot();
        assert!(
            snap.note
                .as_deref()
                .is_some_and(|n| n.contains("already armed")),
            "the skip must be explained in status, got: {:?}",
            snap.note
        );
    }

    #[test]
    fn test_run_tick_none_never_rebuilds() {
        let rebuild_calls = Arc::new(AtomicUsize::new(0));
        let trigger_calls = Arc::new(AtomicUsize::new(0));
        let mut probe = FakeProbe {
            check: UpdateCheck {
                update_available: None,
                source_commit: None,
                commits_behind: None,
                hours_behind: None,
            },
            tree_clean: Some(true),
            dirty_paths: Vec::new(),
            in_flight: 0,
            rebuild_outcome: RebuildOutcome::Success,
            rebuild_calls: rebuild_calls.clone(),
            low_priority_calls: Arc::new(AtomicUsize::new(0)),
        };
        let trigger = FakeTrigger {
            accepted: true,
            calls: trigger_calls.clone(),
        };
        let status = AutoUpdateStatus::new(true);
        let mut state = AutoUpdateState::new();
        run_tick(&mut state, &status, &mut probe, &trigger, Duration::from_secs(0), DEFER);
        assert_eq!(rebuild_calls.load(Ordering::SeqCst), 0, "None must never rebuild");
        assert_eq!(trigger_calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn test_run_tick_dirty_tree_never_rebuilds() {
        let rebuild_calls = Arc::new(AtomicUsize::new(0));
        let mut probe = FakeProbe {
            check: stale("c1"),
            tree_clean: Some(false),
            dirty_paths: Vec::new(),
            in_flight: 0,
            rebuild_outcome: RebuildOutcome::Success,
            rebuild_calls: rebuild_calls.clone(),
            low_priority_calls: Arc::new(AtomicUsize::new(0)),
        };
        let trigger = FakeTrigger {
            accepted: true,
            calls: Arc::new(AtomicUsize::new(0)),
        };
        let status = AutoUpdateStatus::new(true);
        let mut state = AutoUpdateState::new();
        run_tick(&mut state, &status, &mut probe, &trigger, Duration::from_secs(0), DEFER);
        assert_eq!(rebuild_calls.load(Ordering::SeqCst), 0, "dirty tree must never rebuild");
    }

    /// Issue #7608: a dirty-tree refusal names the offending paths (first
    /// three, plus a count) in the published note, so `loom-daemon health`
    /// (which surfaces `auto_update_note` verbatim, #7584) can say exactly
    /// what blocked the rebuild instead of a bare "dirty" with no detail.
    #[test]
    fn test_run_tick_dirty_tree_note_names_offending_paths() {
        let rebuild_calls = Arc::new(AtomicUsize::new(0));
        let mut probe = FakeProbe {
            check: stale("c1"),
            tree_clean: Some(false),
            dirty_paths: vec![
                "loom-daemon/src/foo.rs".to_string(),
                "Cargo.lock".to_string(),
            ],
            in_flight: 0,
            rebuild_outcome: RebuildOutcome::Success,
            rebuild_calls: rebuild_calls.clone(),
            low_priority_calls: Arc::new(AtomicUsize::new(0)),
        };
        let trigger = FakeTrigger {
            accepted: true,
            calls: Arc::new(AtomicUsize::new(0)),
        };
        let status = AutoUpdateStatus::new(true);
        let mut state = AutoUpdateState::new();
        run_tick(&mut state, &status, &mut probe, &trigger, Duration::from_secs(0), DEFER);
        assert_eq!(rebuild_calls.load(Ordering::SeqCst), 0, "dirty tree must never rebuild");
        let note = status.snapshot().note.expect("note published");
        assert!(note.contains("loom-daemon/src/foo.rs"), "note must name paths: {note}");
        assert!(note.contains("Cargo.lock"), "note must name paths: {note}");
        assert!(note.contains("(2)"), "note must include the dirty-path count: {note}");
    }

    #[test]
    fn with_dirty_paths_empty_returns_reason_unchanged() {
        assert_eq!(with_dirty_paths("dirty", Vec::new()), "dirty");
    }

    #[test]
    fn with_dirty_paths_shows_first_three_plus_remainder_count() {
        let paths: Vec<String> = ["a", "b", "c", "d", "e"]
            .iter()
            .map(|s| (*s).to_string())
            .collect();
        let note = with_dirty_paths("dirty", paths);
        assert!(note.contains("(5): a, b, c"), "got: {note}");
        assert!(note.contains("+2 more"), "got: {note}");
    }

    #[test]
    fn test_run_tick_terminal_exit_not_retried_next_tick() {
        let rebuild_calls = Arc::new(AtomicUsize::new(0));
        let mut probe = FakeProbe {
            check: stale("c1"),
            tree_clean: Some(true),
            dirty_paths: Vec::new(),
            in_flight: 0,
            rebuild_outcome: RebuildOutcome::Terminal("commit mismatch (exit 4)".into()),
            rebuild_calls: rebuild_calls.clone(),
            low_priority_calls: Arc::new(AtomicUsize::new(0)),
        };
        let trigger = FakeTrigger {
            accepted: true,
            calls: Arc::new(AtomicUsize::new(0)),
        };
        let status = AutoUpdateStatus::new(true);
        let mut state = AutoUpdateState::new();
        let settle = Duration::from_secs(0);
        // Tick 1: rebuilds, hits terminal.
        run_tick(&mut state, &status, &mut probe, &trigger, settle, DEFER);
        assert_eq!(rebuild_calls.load(Ordering::SeqCst), 1);
        assert!(status.snapshot().terminal_reason.is_some());
        // Tick 2 (same commit): must NOT rebuild again.
        run_tick(&mut state, &status, &mut probe, &trigger, settle, DEFER);
        assert_eq!(rebuild_calls.load(Ordering::SeqCst), 1, "terminal must not retry same commit");
    }

    #[test]
    fn test_run_tick_gate4_defers_rebuild_with_inflight_sweeps() {
        let rebuild_calls = Arc::new(AtomicUsize::new(0));
        let mut probe = FakeProbe {
            check: stale("c1"),
            tree_clean: Some(true),
            dirty_paths: Vec::new(),
            in_flight: 2,
            rebuild_outcome: RebuildOutcome::Success,
            rebuild_calls: rebuild_calls.clone(),
            low_priority_calls: Arc::new(AtomicUsize::new(0)),
        };
        let trigger = FakeTrigger {
            accepted: true,
            calls: Arc::new(AtomicUsize::new(0)),
        };
        let status = AutoUpdateStatus::new(true);
        let mut state = AutoUpdateState::new();
        run_tick(&mut state, &status, &mut probe, &trigger, Duration::from_secs(0), DEFER);
        assert_eq!(rebuild_calls.load(Ordering::SeqCst), 0, "in-flight sweeps must gate the build");
    }

    /// End-to-end #4929: a host whose in-flight count NEVER drops to zero still
    /// converges. The first check defers (gate 4 intact); once the deferral
    /// deadline elapses the loop rebuilds anyway — niced — the drain-and-restart
    /// is triggered, and `last_roll` finally goes non-null.
    #[test]
    fn test_run_tick_saturated_host_eventually_rolls_after_defer_deadline() {
        let rebuild_calls = Arc::new(AtomicUsize::new(0));
        let low_priority_calls = Arc::new(AtomicUsize::new(0));
        let trigger_calls = Arc::new(AtomicUsize::new(0));
        let mut probe = FakeProbe {
            check: stale("c1"),
            tree_clean: Some(true),
            dirty_paths: Vec::new(),
            // Permanently saturated — the sweep count never reaches 0, which is
            // exactly what starved the updater on robb-STUDIO.
            in_flight: 13,
            rebuild_outcome: RebuildOutcome::Success,
            rebuild_calls: rebuild_calls.clone(),
            low_priority_calls: low_priority_calls.clone(),
        };
        let trigger = FakeTrigger {
            accepted: true,
            calls: trigger_calls.clone(),
        };
        let status = AutoUpdateStatus::new(true);
        let mut state = AutoUpdateState::new();
        // `run_tick` reads the real monotonic clock, so use a short deadline and
        // sleep past it. The FIRST tick can never fire the override (it starts
        // the deferral clock at its own `now`), so this is robust in both
        // directions regardless of machine speed.
        let settle = Duration::from_secs(0);
        let deadline = Duration::from_millis(50);

        run_tick(&mut state, &status, &mut probe, &trigger, settle, deadline);
        assert_eq!(
            rebuild_calls.load(Ordering::SeqCst),
            0,
            "first busy check must still defer (gate 4 intact)"
        );
        assert!(status.snapshot().last_roll.is_none());

        std::thread::sleep(Duration::from_millis(120));

        run_tick(&mut state, &status, &mut probe, &trigger, settle, deadline);
        assert_eq!(
            rebuild_calls.load(Ordering::SeqCst),
            1,
            "a permanently saturated host must rebuild once the deadline elapses (#4929)"
        );
        assert_eq!(
            low_priority_calls.load(Ordering::SeqCst),
            1,
            "the forced build must run at reduced priority, not compete head-on"
        );
        assert_eq!(trigger_calls.load(Ordering::SeqCst), 1, "the roll still goes through drain");
        let snap = status.snapshot();
        assert!(snap.last_roll.is_some(), "#4929 acceptance: last_roll must become non-null");
        assert!(
            snap.note.unwrap_or_default().contains("reduced priority"),
            "the forced build must be visible in `loom-daemon status`"
        );
    }

    // ===================================================================
    // IpcDrainTrigger — the roll routes through #4090's drain primitive
    // ===================================================================

    /// The production trigger genuinely calls [`crate::ipc::handle_drain_request`]
    /// (the #4090 drain path), not a bare restart: on an **unsupervised** host it
    /// is refused (`accepted: false`) and — critically — dispatch is NOT paused
    /// (`is_draining()` stays false), exactly the drain primitive's contract.
    /// The supervised happy path (`is_draining()` true, `evaluate_drain_tick`
    /// completing only at 0 in-flight) is covered by #4090's own ipc.rs tests;
    /// exercising it here would `process::exit` the test runner.
    #[tokio::test]
    #[serial(loom_daemon_supervisor)]
    async fn test_ipc_drain_trigger_routes_through_drain_primitive() {
        std::env::remove_var("LOOM_DAEMON_SUPERVISOR");
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        // Force an empty workspace registry so count_in_flight_sweeps == 0 and
        // the only variable under test is the supervisor refusal.
        std::env::set_var(
            crate::workspace_registry::REGISTRY_PATH_ENV,
            root.join("no-such-workspaces.json"),
        );
        let bus = Arc::new(EventBus::new());
        let pool = Arc::new(WorkspacePool::new(bus.clone(), tokio::runtime::Handle::current()));
        let drain = Arc::new(DrainState::new());
        let trigger =
            IpcDrainTrigger::new(drain.clone(), pool, root, bus, tokio::runtime::Handle::current());

        let accepted = trigger.trigger();
        std::env::remove_var(crate::workspace_registry::REGISTRY_PATH_ENV);

        assert!(!accepted, "unsupervised host must refuse the drain (no bare restart fallback)");
        assert!(!drain.is_draining(), "a refused drain must not pause dispatch");
        assert_eq!(drain.generation(), 0, "a refused drain must not bump the drain generation");
    }

    // ===================================================================
    // with_relaunch_verify_note — Issue #6969 AC2/AC3
    // ===================================================================

    /// AC2: when a roll actually triggered a drain-and-restart on a recognized
    /// supervisor, the note names the expected relaunch mechanism AND the
    /// detached verifier's bound — the two pieces of information an operator
    /// needs to tell "still within the expected window" apart from "this is
    /// the ~4-minute-gap shape the issue observed".
    #[test]
    #[serial(loom_daemon_supervisor)]
    fn test_relaunch_verify_note_appended_when_triggered_on_recognized_supervisor() {
        std::env::set_var("LOOM_DAEMON_SUPERVISOR", "launchd");
        let note = with_relaunch_verify_note(
            "rebuilt + provisioned; drain-and-restart triggered".to_string(),
            true,
        );
        std::env::remove_var("LOOM_DAEMON_SUPERVISOR");
        assert!(note.contains("drain-and-restart triggered"));
        assert!(note.contains("launchd"));
        assert!(note.contains("KeepAlive"));
        assert!(note.contains("verify-only"));
        assert!(note.contains("30s"), "default poll bound must be named: {note}");
    }

    /// AC3-adjacent: a fake supervisor that never confirms a relaunch is the
    /// scenario the module's decision logic must not silently paper over — the
    /// note is still produced (nothing here blocks on the eventual poll), but
    /// it must name the SAME bound `restart_verify::verify_and_heal` itself
    /// polls against, so the two can never disagree about "how long is too
    /// long".
    #[test]
    #[serial(loom_daemon_supervisor)]
    fn test_relaunch_verify_note_bound_matches_restart_verify_default() {
        std::env::set_var("LOOM_DAEMON_SUPERVISOR", "systemd");
        let note = with_relaunch_verify_note(
            "fetched release artifact + provisioned; drain-and-restart triggered".to_string(),
            true,
        );
        std::env::remove_var("LOOM_DAEMON_SUPERVISOR");
        assert!(note.contains(&format!("{}s", crate::restart_verify::DEFAULT_POLL_SECS)));
        assert!(note.contains("Restart=on-success"));
    }

    /// Nothing was triggered (`drain_accepted == false`) ⇒ nothing is
    /// appended, regardless of supervisor — the note already says the drain
    /// was refused/not attempted, and appending a relaunch-verify sentence to
    /// that would be actively misleading.
    #[test]
    #[serial(loom_daemon_supervisor)]
    fn test_relaunch_verify_note_untouched_when_not_triggered() {
        std::env::set_var("LOOM_DAEMON_SUPERVISOR", "launchd");
        let original = "rebuilt + provisioned, but drain-and-restart was refused (no supervisor?) \
                         — restart manually to run the fresh binary"
            .to_string();
        let note = with_relaunch_verify_note(original.clone(), false);
        std::env::remove_var("LOOM_DAEMON_SUPERVISOR");
        assert_eq!(note, original);
    }

    /// An unsupervised host has nothing to verify against — the note is
    /// unchanged even when `drain_accepted` is (degenerately) true.
    #[test]
    #[serial(loom_daemon_supervisor)]
    fn test_relaunch_verify_note_untouched_when_unsupervised() {
        std::env::remove_var("LOOM_DAEMON_SUPERVISOR");
        let original = "rebuilt + provisioned; drain-and-restart triggered".to_string();
        let note = with_relaunch_verify_note(original.clone(), true);
        assert_eq!(note, original);
    }

    // ===================================================================
    // Global status handle
    // ===================================================================

    #[test]
    fn test_global_status_defaults_when_unset() {
        // Not registering leaves the default (this may race other tests that
        // DO register, so only assert structural defaults on a fresh snapshot).
        let snap = AutoUpdateStatus::new(false).snapshot();
        assert!(!snap.enabled);
        assert!(snap.last_check.is_none());
    }

    // ===================================================================
    // Artifact-first tick (Issue #7609)
    // ===================================================================

    const SHA_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const SHA_B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    /// A resolved artifact for `version`, with the published/installed
    /// checksums and installed version spelled out.
    fn artifact(
        version: &str,
        installed_version: Option<&str>,
        asset_sha: Option<&str>,
        installed_sha: Option<&str>,
    ) -> ArtifactInfo {
        ArtifactInfo {
            tag: format!("v{version}"),
            version: version.to_string(),
            published_at: Some("2026-09-13T12:00:00Z".to_string()),
            asset_sha256: asset_sha.map(str::to_string),
            target: Some("aarch64-apple-darwin".to_string()),
            installed_version: installed_version.map(str::to_string),
            installed_sha256: installed_sha.map(str::to_string),
        }
    }

    fn resolved(info: ArtifactInfo) -> ArtifactResolution {
        ArtifactResolution::Resolved(info)
    }

    fn unresolved() -> ArtifactResolution {
        ArtifactResolution::Unresolved("no releases yet".to_string())
    }

    /// One tick's readings, bundled for [`AutoUpdateState::decide`].
    fn inputs<'a>(
        artifact: &'a ArtifactResolution,
        check: &'a UpdateCheck,
        tree_clean: bool,
        in_flight: usize,
    ) -> TickInputs<'a> {
        TickInputs {
            artifact,
            check,
            tree_clean,
            in_flight,
        }
    }

    /// A state whose artifact-roll record lives in a throwaway dir, so the
    /// convergence guard never reads or writes the ambient `~/.loom`.
    fn state_with_record_dir(dir: &Path) -> AutoUpdateState {
        AutoUpdateState::new_with_record_path(Some(dir.join(ARTIFACT_ROLL_RECORD_FILE)))
    }

    // ---- version comparison -------------------------------------------

    #[test]
    fn test_compare_versions_orders_numerically_not_lexically() {
        use std::cmp::Ordering;
        assert_eq!(compare_versions("0.19.24", "0.19.21"), Ordering::Greater);
        // Lexically "0.19.9" > "0.19.24"; numerically it is not.
        assert_eq!(compare_versions("0.19.24", "0.19.9"), Ordering::Greater);
        assert_eq!(compare_versions("0.19.21", "0.19.21"), Ordering::Equal);
        assert_eq!(compare_versions("0.18.121", "0.19.0"), Ordering::Less);
        // A `v` prefix / trailing junk is stripped defensively, matching the
        // update script's own semver_compare.
        assert_eq!(compare_versions("v0.19.24", "0.19.24"), Ordering::Equal);
        // Missing components default to 0.
        assert_eq!(compare_versions("0.19", "0.19.0"), Ordering::Equal);
        assert_eq!(compare_versions("1", "0.99.99"), Ordering::Greater);
    }

    // ---- classification ------------------------------------------------

    #[test]
    fn test_classify_newer_artifact() {
        let verdict =
            classify_artifact(&artifact("0.19.24", Some("0.19.21"), Some(SHA_A), Some(SHA_B)));
        assert_eq!(
            verdict,
            ArtifactVerdict::Newer {
                installed: Some("0.19.21".to_string()),
                artifact: "0.19.24".to_string()
            }
        );
    }

    #[test]
    fn test_classify_equal_version_differing_sha_is_convergence() {
        let verdict =
            classify_artifact(&artifact("0.19.24", Some("0.19.24"), Some(SHA_A), Some(SHA_B)));
        assert_eq!(
            verdict,
            ArtifactVerdict::ShaDiffers {
                version: "0.19.24".to_string(),
                asset_sha256: SHA_A.to_string(),
                installed_sha256: SHA_B.to_string(),
            }
        );
    }

    #[test]
    fn test_classify_equal_version_same_sha_is_up_to_date() {
        let verdict =
            classify_artifact(&artifact("0.19.24", Some("0.19.24"), Some(SHA_A), Some(SHA_A)));
        assert!(matches!(verdict, ArtifactVerdict::UpToDate { .. }));
        // Case-insensitively — a hex digest's case is not identity.
        let upper = SHA_A.to_uppercase();
        let verdict =
            classify_artifact(&artifact("0.19.24", Some("0.19.24"), Some(&upper), Some(SHA_A)));
        assert!(matches!(verdict, ArtifactVerdict::UpToDate { .. }));
    }

    #[test]
    fn test_classify_older_release_is_up_to_date() {
        let verdict =
            classify_artifact(&artifact("0.19.0", Some("0.19.21"), Some(SHA_A), Some(SHA_B)));
        assert!(matches!(verdict, ArtifactVerdict::UpToDate { .. }));
    }

    #[test]
    fn test_classify_missing_checksum_is_up_to_date_not_a_fetch_loop() {
        // Equal versions with either checksum unknown must NOT fetch: a wrong
        // "differs" would re-fetch and restart on every tick forever.
        assert!(matches!(
            classify_artifact(&artifact("0.19.24", Some("0.19.24"), None, Some(SHA_A))),
            ArtifactVerdict::UpToDate { .. }
        ));
        assert!(matches!(
            classify_artifact(&artifact("0.19.24", Some("0.19.24"), Some(SHA_A), None)),
            ArtifactVerdict::UpToDate { .. }
        ));
    }

    #[test]
    fn test_classify_no_installed_version_is_newer() {
        let verdict = classify_artifact(&artifact("0.19.24", None, Some(SHA_A), None));
        assert_eq!(
            verdict,
            ArtifactVerdict::Newer {
                installed: None,
                artifact: "0.19.24".to_string()
            }
        );
    }

    // ---- decide(): the AC decision matrix -------------------------------

    #[test]
    fn test_decide_newer_artifact_fetches_and_never_rebuilds() {
        let tmp = tempfile::tempdir().unwrap();
        let mut st = state_with_record_dir(tmp.path());
        let now = Instant::now();
        // Deliberately hostile source-side inputs: a DIRTY tree and an
        // undecidable staleness — neither may block the artifact path.
        let art = resolved(artifact("0.19.24", Some("0.19.21"), Some(SHA_A), Some(SHA_B)));
        let undecidable = UpdateCheck {
            update_available: None,
            source_commit: None,
            commits_behind: None,
            hours_behind: None,
        };
        let d =
            st.decide(now, &inputs(&art, &undecidable, false, 0), Duration::from_secs(0), DEFER);
        match d {
            TickDecision::FetchArtifact {
                version,
                why,
                low_priority,
                ..
            } => {
                assert_eq!(version, "0.19.24");
                assert!(why.contains("artifact 0.19.24 > installed 0.19.21"), "why: {why}");
                assert!(!low_priority);
            }
            other => panic!("expected FetchArtifact, got {other:?}"),
        }
    }

    #[test]
    fn test_decide_equal_version_differing_sha_fetches() {
        let tmp = tempfile::tempdir().unwrap();
        let mut st = state_with_record_dir(tmp.path());
        let art = resolved(artifact("0.19.24", Some("0.19.24"), Some(SHA_A), Some(SHA_B)));
        let d = st.decide(
            Instant::now(),
            &inputs(&art, &stale("c1"), true, 0),
            Duration::from_secs(0),
            DEFER,
        );
        match d {
            TickDecision::FetchArtifact { why, .. } => {
                assert!(why.contains("sha differs"), "why: {why}");
            }
            other => panic!("expected FetchArtifact, got {other:?}"),
        }
    }

    #[test]
    fn test_decide_equal_version_same_sha_skips() {
        let tmp = tempfile::tempdir().unwrap();
        let mut st = state_with_record_dir(tmp.path());
        // A stale source checkout must NOT produce a rebuild once an artifact
        // resolves and says the installed binary is already the released one.
        let art = resolved(artifact("0.19.24", Some("0.19.24"), Some(SHA_A), Some(SHA_A)));
        let d = st.decide(
            Instant::now(),
            &inputs(&art, &stale("c1"), true, 0),
            Duration::from_secs(0),
            DEFER,
        );
        match d {
            TickDecision::Skip(reason) => {
                assert!(reason.contains("sha matches"), "reason: {reason}");
                assert!(reason.contains("up to date"), "reason: {reason}");
            }
            other => panic!("expected Skip, got {other:?}"),
        }
    }

    #[test]
    fn test_decide_no_artifact_falls_back_to_stale_source_rebuild() {
        let tmp = tempfile::tempdir().unwrap();
        let mut st = state_with_record_dir(tmp.path());
        let d = st.decide(
            Instant::now(),
            &inputs(&unresolved(), &stale("c1"), true, 0),
            Duration::from_secs(0),
            DEFER,
        );
        assert!(
            matches!(
                d,
                TickDecision::Rebuild {
                    low_priority: false
                }
            ),
            "got {d:?}"
        );
    }

    #[test]
    fn test_decide_no_artifact_dirty_source_still_refuses() {
        let tmp = tempfile::tempdir().unwrap();
        let mut st = state_with_record_dir(tmp.path());
        let d = st.decide(
            Instant::now(),
            &inputs(&unresolved(), &stale("c1"), false, 0),
            Duration::from_secs(0),
            DEFER,
        );
        match d {
            TickDecision::Skip(reason) => {
                assert!(reason.ends_with(DIRTY_TREE_REASON), "reason: {reason}");
                // The log line must name which path was taken and why the
                // artifact path was not (Issue #7609's logging AC).
                assert!(
                    reason.starts_with("no artifact (no releases yet) → source path:"),
                    "reason: {reason}"
                );
            }
            other => panic!("expected Skip, got {other:?}"),
        }
    }

    #[test]
    fn test_decide_artifact_respects_settle_window() {
        let tmp = tempfile::tempdir().unwrap();
        let mut st = state_with_record_dir(tmp.path());
        let settle = Duration::from_secs(600);
        let base = Instant::now();
        let info = resolved(artifact("0.19.24", Some("0.19.21"), Some(SHA_A), Some(SHA_B)));
        let first = st.decide(base, &inputs(&info, &stale("c1"), true, 0), settle, DEFER);
        assert!(
            matches!(first, TickDecision::Skip(ref r) if r.contains("settle")),
            "got {first:?}"
        );
        let later = base + settle + Duration::from_secs(1);
        let second = st.decide(later, &inputs(&info, &stale("c1"), true, 0), settle, DEFER);
        assert!(matches!(second, TickDecision::FetchArtifact { .. }), "got {second:?}");
    }

    #[test]
    fn test_decide_artifact_defers_for_in_flight_sweeps_then_forces_low_priority() {
        let tmp = tempfile::tempdir().unwrap();
        let mut st = state_with_record_dir(tmp.path());
        let settle = Duration::from_secs(0);
        let deadline = Duration::from_secs(100);
        let base = Instant::now();
        let info = resolved(artifact("0.19.24", Some("0.19.21"), Some(SHA_A), Some(SHA_B)));

        let deferred = st.decide(base, &inputs(&info, &stale("c1"), true, 3), settle, deadline);
        assert!(
            matches!(deferred, TickDecision::Skip(ref r) if r.contains("in-flight sweep(s)")),
            "got {deferred:?}"
        );
        let past = base + deadline + Duration::from_secs(1);
        let forced = st.decide(past, &inputs(&info, &stale("c1"), true, 3), settle, deadline);
        assert!(
            matches!(
                forced,
                TickDecision::FetchArtifact {
                    low_priority: true,
                    ..
                }
            ),
            "got {forced:?}"
        );
    }

    #[test]
    fn test_decide_artifact_honors_backoff_and_terminal() {
        let tmp = tempfile::tempdir().unwrap();
        let mut st = state_with_record_dir(tmp.path());
        let settle = Duration::from_secs(0);
        let base = Instant::now();
        let info = resolved(artifact("0.19.24", Some("0.19.21"), Some(SHA_A), Some(SHA_B)));

        assert!(matches!(
            st.decide(base, &inputs(&info, &stale("c1"), true, 0), settle, DEFER),
            TickDecision::FetchArtifact { .. }
        ));
        // A retryable fetch failure backs off exactly as a rebuild failure does.
        let note = st.record_artifact_roll(
            base,
            &RebuildOutcome::Retryable("download failed".to_string()),
            false,
            &artifact("0.19.24", Some("0.19.21"), Some(SHA_A), Some(SHA_B)),
        );
        assert!(note.starts_with("artifact fetch failed"), "note: {note}");
        let d = st.decide(base, &inputs(&info, &stale("c1"), true, 0), settle, DEFER);
        assert!(matches!(d, TickDecision::Skip(ref r) if r.contains("backing off")), "got {d:?}");

        // A terminal failure is sticky until the target changes.
        st.record_artifact_roll(
            base,
            &RebuildOutcome::Terminal("verification failed".to_string()),
            false,
            &artifact("0.19.24", Some("0.19.21"), Some(SHA_A), Some(SHA_B)),
        );
        let d = st.decide(base, &inputs(&info, &stale("c1"), true, 0), settle, DEFER);
        assert!(matches!(d, TickDecision::Skip(ref r) if r.contains("terminal")), "got {d:?}");
        // A NEWER release clears it — a new artifact is a fresh attempt.
        let newer = resolved(artifact("0.19.25", Some("0.19.21"), Some(SHA_B), Some(SHA_A)));
        let d = st.decide(base, &inputs(&newer, &stale("c1"), true, 0), settle, DEFER);
        assert!(matches!(d, TickDecision::FetchArtifact { .. }), "got {d:?}");
    }

    // ---- the convergence guard (no fetch/restart loop) -------------------

    #[test]
    fn test_recorded_roll_suppresses_a_repeat_same_version_convergence() {
        let tmp = tempfile::tempdir().unwrap();
        let mut st = state_with_record_dir(tmp.path());
        let now = Instant::now();
        let info = artifact("0.19.24", Some("0.19.24"), Some(SHA_A), Some(SHA_B));

        // First observation: converge onto the published bytes.
        assert!(matches!(
            st.decide(
                now,
                &inputs(&resolved(info.clone()), &stale("c1"), true, 0),
                Duration::from_secs(0),
                DEFER
            ),
            TickDecision::FetchArtifact { .. }
        ));
        st.record_artifact_roll(now, &RebuildOutcome::Success, true, &info);

        // The host re-signed the binary at provision time, so its sha STILL
        // differs from the release's. Without the record this would fetch (and
        // restart) again every tick, forever.
        let d = st.decide(
            now,
            &inputs(&resolved(info.clone()), &stale("c1"), true, 0),
            Duration::from_secs(0),
            DEFER,
        );
        match d {
            TickDecision::Skip(reason) => {
                assert!(reason.contains("already installed from this release"), "reason: {reason}");
                assert!(reason.contains("not re-fetching"), "reason: {reason}");
            }
            other => panic!("expected Skip, got {other:?}"),
        }

        // The record survives a process restart (it is on disk, not in memory).
        let mut restarted = state_with_record_dir(tmp.path());
        assert!(matches!(
            restarted.decide(
                now,
                &inputs(&resolved(info), &stale("c1"), true, 0),
                Duration::from_secs(0),
                DEFER
            ),
            TickDecision::Skip(_)
        ));
    }

    #[test]
    fn test_recorded_roll_does_not_suppress_a_republished_release() {
        let tmp = tempfile::tempdir().unwrap();
        let mut st = state_with_record_dir(tmp.path());
        let now = Instant::now();
        st.record_artifact_roll(
            now,
            &RebuildOutcome::Success,
            true,
            &artifact("0.19.24", Some("0.19.24"), Some(SHA_A), Some(SHA_A)),
        );
        // Same version, but the release now publishes DIFFERENT bytes — a
        // re-cut release must still converge.
        let republished = resolved(artifact("0.19.24", Some("0.19.24"), Some(SHA_B), Some(SHA_A)));
        let d = st.decide(
            now,
            &inputs(&republished, &stale("c1"), true, 0),
            Duration::from_secs(0),
            DEFER,
        );
        assert!(matches!(d, TickDecision::FetchArtifact { .. }), "got {d:?}");
    }

    #[test]
    fn test_artifact_roll_record_round_trips_on_disk() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("nested").join(ARTIFACT_ROLL_RECORD_FILE);
        let record = ArtifactRollRecord {
            version: "0.19.24".to_string(),
            asset_sha256: SHA_A.to_string(),
            rolled_at: Utc::now(),
        };
        store_artifact_roll_record(Some(&path), &record);
        assert_eq!(load_artifact_roll_record(Some(&path)), Some(record));
        // A corrupt record soft-fails to None rather than wedging the loop.
        std::fs::write(&path, "{not json").unwrap();
        assert_eq!(load_artifact_roll_record(Some(&path)), None);
        assert_eq!(load_artifact_roll_record(None), None);
    }

    // ---- --resolve-json parsing -----------------------------------------

    #[test]
    fn test_parse_resolve_json_happy_path() {
        let stdout = r#"{"ok":true,"reason":null,"repo":"rjwalters/loom","target":"aarch64-apple-darwin","tag":"v0.19.24","version":"0.19.24","published_at":"2026-09-13T12:00:00Z","asset_sha256":"abc123","installed_bin":"/x/loom-daemon","installed_version":"0.19.21","installed_commit":"deadbee","installed_sha256":"def456","source_version":"0.19.25","source_commit":"88116c7"}"#;
        match parse_resolve_json(stdout) {
            ArtifactResolution::Resolved(info) => {
                assert_eq!(info.version, "0.19.24");
                assert_eq!(info.tag, "v0.19.24");
                assert_eq!(info.published_at.as_deref(), Some("2026-09-13T12:00:00Z"));
                assert_eq!(info.asset_sha256.as_deref(), Some("abc123"));
                assert_eq!(info.installed_version.as_deref(), Some("0.19.21"));
                assert_eq!(info.installed_sha256.as_deref(), Some("def456"));
                assert_eq!(info.target.as_deref(), Some("aarch64-apple-darwin"));
            }
            other => panic!("expected Resolved, got {other:?}"),
        }
    }

    #[test]
    fn test_parse_resolve_json_not_ok_carries_the_reason() {
        let stdout =
            r#"{"ok":false,"reason":"'gh release view' found no latest release","version":null}"#;
        match parse_resolve_json(stdout) {
            ArtifactResolution::Unresolved(reason) => {
                assert!(reason.contains("no latest release"), "reason: {reason}");
            }
            other => panic!("expected Unresolved, got {other:?}"),
        }
    }

    #[test]
    fn test_parse_resolve_json_garbage_is_unresolved_not_a_panic() {
        assert!(matches!(parse_resolve_json(""), ArtifactResolution::Unresolved(_)));
        assert!(matches!(
            parse_resolve_json("not json at all"),
            ArtifactResolution::Unresolved(_)
        ));
        assert!(matches!(parse_resolve_json("{oops"), ArtifactResolution::Unresolved(_)));
        // ok:true but no version — a shape surprise must degrade, not fetch.
        assert!(matches!(
            parse_resolve_json(r#"{"ok":true,"version":null}"#),
            ArtifactResolution::Unresolved(_)
        ));
        // The script's "unknown" installed-commit sentinel must not become a
        // plausible-looking installed VERSION.
        match parse_resolve_json(r#"{"ok":true,"version":"0.19.24","installed_version":"unknown"}"#)
        {
            ArtifactResolution::Resolved(info) => assert_eq!(info.installed_version, None),
            other => panic!("expected Resolved, got {other:?}"),
        }
    }

    #[test]
    fn test_parse_resolve_json_ignores_leading_noise_lines() {
        // Defensive: a shell that leaks a line onto stdout before the JSON
        // must not break resolution.
        let stdout = "warning: something\n{\"ok\":true,\"version\":\"0.19.24\"}\n";
        assert!(matches!(parse_resolve_json(stdout), ArtifactResolution::Resolved(_)));
    }

    // ---- run_tick end to end on the artifact path ------------------------

    /// A probe that resolves a scripted artifact and records whether the tick
    /// fetched or rebuilt. Separate from [`FakeProbe`] so the existing
    /// source-path tests keep exercising the no-artifact default verbatim.
    struct ArtifactFakeProbe {
        artifact: ArtifactResolution,
        check: UpdateCheck,
        tree_clean: Option<bool>,
        in_flight: usize,
        fetch_outcome: RebuildOutcome,
        fetch_calls: Arc<AtomicUsize>,
        rebuild_calls: Arc<AtomicUsize>,
    }

    impl AutoUpdateProbe for ArtifactFakeProbe {
        fn resolve_artifact(&self) -> ArtifactResolution {
            self.artifact.clone()
        }
        fn fetch_artifact(&mut self, _low_priority: bool) -> RebuildOutcome {
            self.fetch_calls.fetch_add(1, Ordering::SeqCst);
            self.fetch_outcome.clone()
        }
        fn check(&self) -> UpdateCheck {
            self.check.clone()
        }
        fn is_tree_clean(&self) -> Option<bool> {
            self.tree_clean
        }
        fn in_flight_sweeps(&self) -> usize {
            self.in_flight
        }
        fn rebuild(&mut self, _low_priority: bool) -> RebuildOutcome {
            self.rebuild_calls.fetch_add(1, Ordering::SeqCst);
            RebuildOutcome::Success
        }
    }

    #[test]
    fn test_run_tick_fetches_the_artifact_and_never_rebuilds() {
        let fetch_calls = Arc::new(AtomicUsize::new(0));
        let rebuild_calls = Arc::new(AtomicUsize::new(0));
        let trigger_calls = Arc::new(AtomicUsize::new(0));
        let mut probe = ArtifactFakeProbe {
            artifact: resolved(artifact("0.19.24", Some("0.19.21"), Some(SHA_A), Some(SHA_B))),
            // The exact fleet shape this issue exists for: no source checkout
            // AND (therefore) an unprovable-clean tree.
            check: UpdateCheck {
                update_available: None,
                source_commit: None,
                commits_behind: None,
                hours_behind: None,
            },
            tree_clean: None,
            in_flight: 0,
            fetch_outcome: RebuildOutcome::Success,
            fetch_calls: fetch_calls.clone(),
            rebuild_calls: rebuild_calls.clone(),
        };
        let trigger = FakeTrigger {
            accepted: true,
            calls: trigger_calls.clone(),
        };
        let status = AutoUpdateStatus::new(true);
        let tmp = tempfile::tempdir().unwrap();
        let mut state = state_with_record_dir(tmp.path());

        run_tick(&mut state, &status, &mut probe, &trigger, Duration::from_secs(0), DEFER);

        assert_eq!(fetch_calls.load(Ordering::SeqCst), 1, "the artifact must be fetched");
        assert_eq!(rebuild_calls.load(Ordering::SeqCst), 0, "no cargo build on the artifact path");
        assert_eq!(
            trigger_calls.load(Ordering::SeqCst),
            1,
            "a successful fetch triggers the drain"
        );
        let snap = status.snapshot();
        assert!(snap.last_roll.is_some());
        assert_eq!(snap.artifact_version.as_deref(), Some("0.19.24"));
        assert_eq!(snap.artifact_published_at.as_deref(), Some("2026-09-13T12:00:00Z"));
        assert!(
            snap.note
                .as_deref()
                .unwrap_or_default()
                .contains("fetched release artifact"),
            "note: {:?}",
            snap.note
        );
        // The roll was recorded, so a re-signed binary cannot re-trigger it.
        assert!(tmp.path().join(ARTIFACT_ROLL_RECORD_FILE).exists());
    }

    #[test]
    fn test_run_tick_up_to_date_artifact_does_not_rebuild_a_stale_checkout() {
        let fetch_calls = Arc::new(AtomicUsize::new(0));
        let rebuild_calls = Arc::new(AtomicUsize::new(0));
        let mut probe = ArtifactFakeProbe {
            artifact: resolved(artifact("0.19.24", Some("0.19.24"), Some(SHA_A), Some(SHA_A))),
            // A stale, clean source checkout — pre-#7609 this would rebuild.
            check: stale("c1"),
            tree_clean: Some(true),
            in_flight: 0,
            fetch_outcome: RebuildOutcome::Success,
            fetch_calls: fetch_calls.clone(),
            rebuild_calls: rebuild_calls.clone(),
        };
        let trigger = FakeTrigger {
            accepted: true,
            calls: Arc::new(AtomicUsize::new(0)),
        };
        let status = AutoUpdateStatus::new(true);
        let tmp = tempfile::tempdir().unwrap();
        let mut state = state_with_record_dir(tmp.path());

        run_tick(&mut state, &status, &mut probe, &trigger, Duration::from_secs(0), DEFER);

        assert_eq!(fetch_calls.load(Ordering::SeqCst), 0);
        assert_eq!(rebuild_calls.load(Ordering::SeqCst), 0);
        let snap = status.snapshot();
        assert_eq!(snap.artifact_version.as_deref(), Some("0.19.24"));
        assert!(
            snap.note
                .as_deref()
                .unwrap_or_default()
                .contains("up to date"),
            "note: {:?}",
            snap.note
        );
    }

    #[test]
    fn test_run_tick_without_an_artifact_still_rebuilds_from_source() {
        let fetch_calls = Arc::new(AtomicUsize::new(0));
        let rebuild_calls = Arc::new(AtomicUsize::new(0));
        let mut probe = ArtifactFakeProbe {
            artifact: unresolved(),
            check: stale("c1"),
            tree_clean: Some(true),
            in_flight: 0,
            fetch_outcome: RebuildOutcome::Success,
            fetch_calls: fetch_calls.clone(),
            rebuild_calls: rebuild_calls.clone(),
        };
        let trigger = FakeTrigger {
            accepted: true,
            calls: Arc::new(AtomicUsize::new(0)),
        };
        let status = AutoUpdateStatus::new(true);
        let tmp = tempfile::tempdir().unwrap();
        let mut state = state_with_record_dir(tmp.path());

        run_tick(&mut state, &status, &mut probe, &trigger, Duration::from_secs(0), DEFER);

        assert_eq!(fetch_calls.load(Ordering::SeqCst), 0);
        assert_eq!(
            rebuild_calls.load(Ordering::SeqCst),
            1,
            "the source path is preserved verbatim"
        );
        assert_eq!(status.snapshot().artifact_version, None);
    }

    #[test]
    fn test_run_tick_fetch_failure_backs_off_without_falling_back_to_a_build() {
        let fetch_calls = Arc::new(AtomicUsize::new(0));
        let rebuild_calls = Arc::new(AtomicUsize::new(0));
        let mut probe = ArtifactFakeProbe {
            artifact: resolved(artifact("0.19.24", Some("0.19.21"), Some(SHA_A), Some(SHA_B))),
            check: stale("c1"),
            tree_clean: Some(true),
            in_flight: 0,
            fetch_outcome: RebuildOutcome::Retryable(
                "exit 1: no usable release artifact".to_string(),
            ),
            fetch_calls: fetch_calls.clone(),
            rebuild_calls: rebuild_calls.clone(),
        };
        let trigger = FakeTrigger {
            accepted: true,
            calls: Arc::new(AtomicUsize::new(0)),
        };
        let status = AutoUpdateStatus::new(true);
        let tmp = tempfile::tempdir().unwrap();
        let mut state = state_with_record_dir(tmp.path());

        run_tick(&mut state, &status, &mut probe, &trigger, Duration::from_secs(0), DEFER);
        // A failed fetch must NOT silently become a source build — the tick
        // backs off and retries the artifact path instead.
        assert_eq!(fetch_calls.load(Ordering::SeqCst), 1);
        assert_eq!(rebuild_calls.load(Ordering::SeqCst), 0);
        let snap = status.snapshot();
        assert_eq!(snap.consecutive_failures, 1);
        assert!(snap.backoff_secs.is_some());
        assert!(
            !tmp.path().join(ARTIFACT_ROLL_RECORD_FILE).exists(),
            "a failed roll records nothing"
        );
    }

    // ---- script-root resolution (the no-source-checkout host) -------------

    #[tokio::test]
    async fn test_script_root_falls_back_to_the_workspace_root() {
        // The host shape this issue exists for: no build-time source checkout
        // resolvable, but the daemon's own workspace root has the script.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        std::fs::create_dir_all(root.join(".loom/scripts/cli")).unwrap();
        std::fs::write(root.join(".loom/scripts/cli/loom-daemon-update.sh"), "#!/bin/sh\n")
            .unwrap();
        let bus = Arc::new(EventBus::new());
        let pool = Arc::new(WorkspacePool::new(bus, tokio::runtime::Handle::current()));
        let mut probe = ScriptAutoUpdateProbe::new(pool, root.clone());
        probe.source_root = None;
        assert_eq!(probe.script_root(), Some(root));
    }

    #[tokio::test]
    async fn test_script_root_is_none_without_any_script() {
        let tmp = tempfile::tempdir().unwrap();
        let bus = Arc::new(EventBus::new());
        let pool = Arc::new(WorkspacePool::new(bus, tokio::runtime::Handle::current()));
        let mut probe = ScriptAutoUpdateProbe::new(pool, tmp.path().to_path_buf());
        probe.source_root = None;
        assert_eq!(probe.script_root(), None);
        // …and a probe with no script resolves no artifact rather than erroring.
        assert!(matches!(probe.resolve_artifact(), ArtifactResolution::Unresolved(_)));
    }
}
