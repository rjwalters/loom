//! Pressure-triggered **deep clean** of a repo's primary checkout (issue #5919)
//! — the pass that closes the last automatic-reclamation gap on a long-lived
//! host.
//!
//! # What was still leaking
//!
//! [`crate::worktree_reaper`] already reclaims disk on a cadence, in two ways:
//! it *removes* worktrees whose issue closed and whose PR merged
//! ([`crate::worktree_reaper::reap_worktrees`]), and — since #5187 — it strips
//! `target/`/`node_modules/` out of every **kept, idle** worktree
//! ([`crate::worktree_reaper::reclaim_kept_worktree_artifacts`]). Neither
//! touches the one directory that is neither a worktree nor removable: the
//! registered repo's **own primary checkout**.
//!
//! `clean_build_artifacts()` — wired to `CleanOptions::deep` inside
//! `run_clean()` — is the only code path that reclaims from `repo_root` itself,
//! and `reap_repo()` never called it (`reaper_clean_options()` hardcodes
//! `deep: false`). So on a long-lived host the primary checkout's `target/`
//! grew monotonically until a human ran `loom-daemon clean --deep --safe`. The
//! measured cost on the reporting fleet: 34 GB of regrowth in about a day, a
//! host down to 1.9 GiB free with its daemon unqueryable over IPC, and a
//! `dynamic_cap` of 5 against a configured 12 purely from unreclaimed
//! artifacts.
//!
//! # Shape: pressure-triggered, not "deep on every tick"
//!
//! The issue explicitly ruled out flipping `deep: true` on the existing reaper
//! — that turns a cheap 15-minute job into an expensive one, and deletes a
//! developer's warm build cache every quarter hour for no reason. Instead this
//! pass fires **only when free space on the repo's own volume crosses the
//! configured floor** (the same `diskWarnFreeGb` the reaper already warns at),
//! with a cooldown so a disk that stays full for some *other* reason cannot
//! turn into a delete/rebuild/delete loop. Reclamation happens exactly when it
//! matters and never otherwise.
//!
//! [`evaluate`] is a pure function of (enabled, free GB, floor, last-fired,
//! now, cooldown) — every trigger and non-trigger is unit-testable in
//! compressed time, with no host that has actually been up for days.
//!
//! # Four safety gates, all structural
//!
//! 1. **`safe: true` is enforced, not merely documented.** [`run_pass`] refuses
//!    to run at all when handed [`CleanOptions`] that are not `safe`, or that
//!    are `force`. A scheduled bare `--deep` is therefore unreachable by
//!    construction, not by convention — see [`deep_clean_options`].
//! 2. **It holds the machine-wide build slot** ([`crate::build_slot`]) for the
//!    duration of the removal, so it can never delete `target/` out from under
//!    a concurrent build gate (the daemon's own main-health gate and every
//!    sweep worktree's `build-gate.sh` take the same slot). If the slot cannot
//!    be taken, the pass **defers** to the next tick rather than proceeding
//!    unserialized.
//! 3. **It never deletes the directory holding the running binary**
//!    ([`exe_is_inside_artifacts`]) — a daemon running straight out of
//!    `<repo>/target/release/` does not saw off its own branch.
//! 4. **It only ever removes [`clean::PRIMARY_CHECKOUT_ARTIFACTS`]** —
//!    literally the same `target/` + `node_modules/` list `clean --deep`
//!    removes, through the same
//!    [`clean::sweep_primary_checkout_artifacts`] engine. The scheduled path
//!    cannot delete something the manual command would not.
//!
//! # Observability
//!
//! Every evaluation publishes to a process-global ([`publish`], mirroring
//! [`crate::auto_update`]'s status handle) that `loom-daemon status` renders
//! beside its disk-headroom line, so "is reclamation happening on this host?"
//! is answerable without reading logs — including the negative case ("last
//! evaluated 3m ago: 118G free ≥ 20G floor"). A firing pass additionally logs
//! at `warn` naming both what it reclaimed and why it fired.
//!
//! # Default-on
//!
//! Like the reaper it rides on, this is **default-on**: an unbounded disk leak
//! that silently caps concurrency is not a behavior anyone opts into. Opt out
//! with `LOOM_DEEP_CLEAN=0` or
//! `autonomous.worktreeReaper.deepClean.enabled=false`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use chrono::{DateTime, Utc};

use crate::worktree_ops::clean::{self, ArtifactOutcome, CleanOptions, ReclaimedArtifact};

// ============================================================================
// Constants
// ============================================================================

/// Master on/off env override. Default-on (see module docs): `0`/`false`/`no`/
/// `off` disables, `1`/`true`/`yes`/`on` force-enables even when config
/// disables it.
pub const DEEP_CLEAN_ENABLE_ENV: &str = "LOOM_DEEP_CLEAN";

/// Env override for the free-GB floor the pass fires below. Absent, the floor
/// is the reaper's own `diskWarnFreeGb` — one number for "this host is low on
/// disk" and "reclaim now", rather than two that can drift apart.
pub const DEEP_CLEAN_FREE_GB_ENV: &str = "LOOM_DEEP_CLEAN_FREE_GB";

/// Env override for the cooldown between deep passes (seconds).
pub const DEEP_CLEAN_MIN_INTERVAL_ENV: &str = "LOOM_DEEP_CLEAN_MIN_INTERVAL_SECS";

/// Default cooldown between deep passes: **6 hours**.
///
/// The pass is cheap and the trigger is already narrow, so this is not a rate
/// limit on cost — it is the anti-thrash gate. A host whose disk is full for a
/// reason build artifacts cannot fix (a huge dataset, another tenant) would
/// otherwise re-delete a freshly rebuilt `target/` on every 15-minute reaper
/// tick, which is strictly worse than leaving it alone.
pub const DEFAULT_DEEP_CLEAN_MIN_INTERVAL_SECS: u64 = 21_600;

/// How long the pass waits for the machine-wide build slot before deferring to
/// the next reaper tick. Deliberately short: unlike a build (which waits up to
/// [`crate::build_slot::DEFAULT_BUILD_SLOT_WAIT_SECS`] because it has work to
/// do), this pass is opportunistic — another tick is 15 minutes away and losing
/// one costs nothing, while blocking the reaper loop behind a multi-minute
/// build costs every other repo its pass.
pub const DEEP_CLEAN_SLOT_WAIT_SECS: u64 = 5;

/// Poll interval while waiting for the build slot.
const DEEP_CLEAN_SLOT_POLL: Duration = Duration::from_millis(500);

// ============================================================================
// Config (.loom/config.json → autonomous.worktreeReaper.deepClean)
// ============================================================================

/// The subset of `.loom/config.json → autonomous.worktreeReaper.deepClean` this
/// module consumes. Every field is `Option` so an absent key falls through to
/// the env-var / built-in-default resolution — precedence **env > config >
/// default**, matching [`crate::worktree_reaper::WorktreeReaperConfig`] and
/// every other `autonomous.*` surface.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DeepCleanConfig {
    /// `…deepClean.enabled` (default **true**).
    pub enabled: Option<bool>,
    /// `…deepClean.freeGb` — fire below this many free GB on the repo's own
    /// volume. Absent, the reaper's `diskWarnFreeGb` is used.
    pub free_gb: Option<u64>,
    /// `…deepClean.minIntervalSecs` — cooldown between passes (a zero/invalid
    /// value drops to `None`).
    pub min_interval_secs: Option<u64>,
}

/// Read `.loom/config.json → autonomous.worktreeReaper.deepClean`, soft-failing
/// every field to `None` (env/default resolution) on a missing file, malformed
/// JSON, or a missing block.
#[must_use]
pub fn read_deep_clean_config(repo_root: &Path) -> DeepCleanConfig {
    let effective = crate::config_resolver::resolve_effective_config(repo_root);
    let Some(block) =
        crate::config_resolver::get_path(&effective, "autonomous.worktreeReaper.deepClean")
    else {
        return DeepCleanConfig::default();
    };

    DeepCleanConfig {
        enabled: block.get("enabled").and_then(serde_json::Value::as_bool),
        free_gb: block.get("freeGb").and_then(serde_json::Value::as_u64),
        min_interval_secs: block
            .get("minIntervalSecs")
            .and_then(serde_json::Value::as_u64)
            .filter(|&s| s > 0),
    }
}

/// Resolve whether the pass runs — precedence **env > config > default(true)**.
#[must_use]
pub fn resolve_enabled(config: &DeepCleanConfig) -> bool {
    if let Ok(v) = std::env::var(DEEP_CLEAN_ENABLE_ENV) {
        return matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on");
    }
    config.enabled.unwrap_or(true)
}

/// Resolve the free-GB floor — precedence **env > config > `fallback_gb`**,
/// where `fallback_gb` is the reaper's already-resolved `diskWarnFreeGb`.
#[must_use]
pub fn resolve_free_gb(config: &DeepCleanConfig, fallback_gb: u64) -> u64 {
    std::env::var(DEEP_CLEAN_FREE_GB_ENV)
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .or(config.free_gb)
        .unwrap_or(fallback_gb)
}

/// Resolve the cooldown — precedence **env > config > default**. A zero or
/// unparseable env value falls through rather than disabling the anti-thrash
/// gate.
#[must_use]
pub fn resolve_min_interval_secs(config: &DeepCleanConfig) -> u64 {
    std::env::var(DEEP_CLEAN_MIN_INTERVAL_ENV)
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|&s| s > 0)
        .or(config.min_interval_secs)
        .unwrap_or(DEFAULT_DEEP_CLEAN_MIN_INTERVAL_SECS)
}

/// The [`CleanOptions`] the scheduled deep pass runs under: the exact semantics
/// of a hand-typed `loom-daemon clean --deep --safe`, never `--force`, never a
/// dry run.
///
/// `safe: true` is the whole safety boundary of the automatic path (it gates
/// worktree removal on issue-closed **and** PR-merged **and** grace period
/// **and** no uncommitted work, where a bare `--deep` gates on issue-closed
/// alone), so [`run_pass`] re-checks it at runtime rather than trusting this
/// constructor to be the only caller.
#[must_use]
pub fn deep_clean_options(grace_period_secs: i64) -> CleanOptions {
    CleanOptions {
        dry_run: false,
        deep: true,
        force: false,
        safe: true,
        grace_period_secs,
        // The primary checkout is not a worktree, so this pass is by definition
        // not `worktrees_only` — but it is also not a `run_clean()` caller: it
        // only ever reaches `clean::sweep_primary_checkout_artifacts`.
        worktrees_only: false,
        branches_only: false,
        tmux_only: false,
        require_managed_sentinel: true,
    }
}

// ============================================================================
// Trigger
// ============================================================================

/// Why a deep pass did — or did not — fire on one tick. Every variant is
/// rendered verbatim into the daemon log and `loom-daemon status`, so a
/// shrinking (or stubbornly full) disk is always attributable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeepCleanTrigger {
    /// `autonomous.worktreeReaper.deepClean.enabled=false` / `LOOM_DEEP_CLEAN=0`.
    Disabled,
    /// Free space on the repo's volume was unmeasurable. Unknown != zero
    /// (#4164): an unmeasurable probe never fires a deletion.
    FreeSpaceUnknown,
    /// There is headroom — the overwhelmingly common outcome.
    AboveFloor { free_gb: u64, floor_gb: u64 },
    /// Below the floor, but a pass already ran inside the cooldown window.
    Cooldown {
        free_gb: u64,
        floor_gb: u64,
        since_secs: i64,
        min_interval_secs: u64,
    },
    /// Below the floor and past the cooldown — **fires**.
    DiskPressure { free_gb: u64, floor_gb: u64 },
}

impl DeepCleanTrigger {
    /// Whether this outcome runs the reclaim.
    #[must_use]
    pub fn fires(&self) -> bool {
        matches!(self, Self::DiskPressure { .. })
    }

    /// One-line human-readable rationale, used identically by the log line and
    /// the `status` renderer.
    #[must_use]
    pub fn reason(&self) -> String {
        match self {
            Self::Disabled => "disabled (autonomous.worktreeReaper.deepClean.enabled=false or \
                 LOOM_DEEP_CLEAN unset-falsy)"
                .to_string(),
            Self::FreeSpaceUnknown => {
                "free space on the repo volume is unmeasurable — skipping (unknown != zero)"
                    .to_string()
            }
            Self::AboveFloor { free_gb, floor_gb } => {
                format!("{free_gb}G free >= {floor_gb}G floor — no disk pressure")
            }
            Self::Cooldown {
                free_gb,
                floor_gb,
                since_secs,
                min_interval_secs,
            } => format!(
                "{free_gb}G free < {floor_gb}G floor, but a deep pass ran {since_secs}s ago \
                 (cooldown {min_interval_secs}s)"
            ),
            Self::DiskPressure { free_gb, floor_gb } => {
                format!("DISK PRESSURE — {free_gb}G free < {floor_gb}G floor")
            }
        }
    }
}

/// Decide whether a deep pass should run, purely from its inputs.
///
/// Split out from all I/O on purpose: "a long-lived host does not accumulate
/// unboundedly" is a statement about *time*, and this makes the whole trigger
/// testable in compressed time — a `last_fired` seven hours before `now` is one
/// line, not a seven-hour test.
///
/// Ordering of the gates is itself a safety property: `enabled` before anything
/// else, and `free_gb == None` (unmeasurable) resolves to a skip before the
/// comparison against `floor_gb` can treat "unknown" as "zero free".
#[must_use]
pub fn evaluate(
    enabled: bool,
    free_gb: Option<u64>,
    floor_gb: u64,
    last_fired: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
    min_interval_secs: u64,
) -> DeepCleanTrigger {
    if !enabled {
        return DeepCleanTrigger::Disabled;
    }
    let Some(free_gb) = free_gb else {
        return DeepCleanTrigger::FreeSpaceUnknown;
    };
    if free_gb >= floor_gb {
        return DeepCleanTrigger::AboveFloor { free_gb, floor_gb };
    }
    if let Some(last) = last_fired {
        let since_secs = (now - last).num_seconds();
        // A negative delta (clock stepped backwards) is treated as "just ran" —
        // the conservative read for a gate whose job is to prevent thrash.
        if since_secs < i64::try_from(min_interval_secs).unwrap_or(i64::MAX) {
            return DeepCleanTrigger::Cooldown {
                free_gb,
                floor_gb,
                since_secs,
                min_interval_secs,
            };
        }
    }
    DeepCleanTrigger::DiskPressure { free_gb, floor_gb }
}

// ============================================================================
// The pass
// ============================================================================

/// What the removal half of the pass did. Separate from [`DeepCleanTrigger`]
/// because "we decided to reclaim" and "we were able to reclaim" are different
/// facts, and an operator needs both.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SweepOutcome {
    /// The artifacts were swept (possibly an empty list — nothing was there).
    Swept(Vec<ReclaimedArtifact>),
    /// A safety gate deferred the removal to a later tick, with the reason.
    Deferred(String),
}

/// The removal step, injected so [`run_pass`] is unit-testable without a build
/// slot, a real multi-GB `target/`, or a host under genuine disk pressure.
pub type SweepFn<'a> = &'a dyn Fn(&Path) -> SweepOutcome;

/// One deep pass over one repo's primary checkout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeepCleanReport {
    /// The repo whose primary checkout was evaluated.
    pub repo_root: PathBuf,
    /// Why the pass did or did not fire.
    pub trigger: DeepCleanTrigger,
    /// Present when the trigger fired but a safety gate stopped the removal.
    pub deferred: Option<String>,
    /// What was reclaimed (empty when the trigger did not fire, or when the
    /// checkout had no artifacts to reclaim).
    pub reclaimed: Vec<ReclaimedArtifact>,
    /// Free GB on the repo's volume as measured for the trigger.
    pub free_gb: Option<u64>,
    /// When this evaluation happened.
    pub at: DateTime<Utc>,
}

impl DeepCleanReport {
    /// Whether artifacts were actually removed this pass.
    #[must_use]
    pub fn reclaimed_anything(&self) -> bool {
        !self.reclaimed.is_empty()
    }

    /// `"target/ (34.1G), node_modules/ (1.2G)"`, or `"nothing"`.
    #[must_use]
    pub fn reclaimed_summary(&self) -> String {
        if self.reclaimed.is_empty() {
            return "nothing".to_string();
        }
        self.reclaimed
            .iter()
            .map(|a| format!("{}/ ({})", a.name, a.size_human))
            .collect::<Vec<_>>()
            .join(", ")
    }
}

/// Everything one [`run_pass`] needs besides its two injected I/O seams —
/// bundled so the resolved-config half of the call stays readable (and so this
/// stays one struct to extend when a future knob lands).
#[derive(Debug, Clone, Copy)]
pub struct DeepCleanInputs<'a> {
    /// The clean semantics the pass runs under; **must** be `safe`, non-`force`
    /// (see [`deep_clean_options`] and [`run_pass`]'s refusal).
    pub opts: &'a CleanOptions,
    /// Resolved `deepClean.enabled`.
    pub enabled: bool,
    /// Resolved free-GB floor.
    pub floor_gb: u64,
    /// Resolved anti-thrash cooldown.
    pub min_interval_secs: u64,
    /// When a pass last fired for this repo, or `None`.
    pub last_fired: Option<DateTime<Utc>>,
    /// Evaluation timestamp.
    pub now: DateTime<Utc>,
}

/// Run one deep pass, with the free-space probe and the removal both injected.
///
/// # Safety refusal
///
/// The first thing this does is re-check `inputs.opts`: a scheduled deep clean
/// without `safe: true` (or with `force: true`) is refused outright and logged
/// at `error`. [`deep_clean_options`] is the only production constructor and
/// already sets them, so this can only fire if a future caller hand-rolls
/// `CleanOptions` — which is exactly the regression the acceptance criteria ask
/// to make impossible rather than merely discouraged.
pub fn run_pass(
    repo_root: &Path,
    inputs: &DeepCleanInputs<'_>,
    free_gb_probe: &dyn Fn(&Path) -> Option<u64>,
    sweep: SweepFn<'_>,
) -> DeepCleanReport {
    let &DeepCleanInputs {
        opts,
        enabled,
        floor_gb,
        min_interval_secs,
        last_fired,
        now,
    } = inputs;

    if !opts.safe || opts.force {
        log::error!(
            "deep_clean: {} REFUSING to run a scheduled deep pass with safe={} force={} — a \
             scheduled bare `--deep` gates on issue-closed alone, checking neither PR-merge \
             status nor uncommitted work. Nothing was removed.",
            repo_root.display(),
            opts.safe,
            opts.force
        );
        return DeepCleanReport {
            repo_root: repo_root.to_path_buf(),
            trigger: DeepCleanTrigger::Disabled,
            deferred: Some("refused: scheduled deep pass requires safe:true, force:false".into()),
            reclaimed: Vec::new(),
            free_gb: None,
            at: now,
        };
    }

    let free_gb = if enabled {
        free_gb_probe(repo_root)
    } else {
        None
    };
    let trigger = evaluate(enabled, free_gb, floor_gb, last_fired, now, min_interval_secs);

    let (reclaimed, deferred) = if trigger.fires() {
        match sweep(repo_root) {
            SweepOutcome::Swept(artifacts) => (artifacts, None),
            SweepOutcome::Deferred(reason) => (Vec::new(), Some(reason)),
        }
    } else {
        (Vec::new(), None)
    };

    DeepCleanReport {
        repo_root: repo_root.to_path_buf(),
        trigger,
        deferred,
        reclaimed,
        free_gb,
        at: now,
    }
}

/// Whether `exe` lives inside a directory this pass would delete — the gate
/// that stops a daemon running straight out of `<repo>/target/release/` from
/// deleting its own binary (and every consequent `self_update` / restart path)
/// the first time its host gets tight on disk.
///
/// `None` (an unresolvable `current_exe`) is treated as *not* inside: the
/// probe failing is not evidence of danger, and the build-slot gate above is
/// the primary protection.
#[must_use]
pub fn exe_is_inside_artifacts(repo_root: &Path, exe: Option<&Path>) -> bool {
    let Some(exe) = exe else {
        return false;
    };
    clean::PRIMARY_CHECKOUT_ARTIFACTS
        .iter()
        .any(|name| exe.starts_with(repo_root.join(name)))
}

/// The production removal step: take the machine-wide build slot, confirm the
/// running binary is not inside what we are about to delete, then sweep exactly
/// [`clean::PRIMARY_CHECKOUT_ARTIFACTS`].
///
/// Deferring (rather than proceeding unserialized) when the slot is unavailable
/// is deliberate. [`crate::build_slot::acquire_in`] degrades open by design —
/// correct for a *build*, which must eventually run — but a deletion that
/// cannot prove exclusivity should simply wait for the next reaper tick. A
/// consequence worth naming: `LOOM_BUILD_SLOTS=0` (build-slot serialization
/// turned off machine-wide) also turns off scheduled deep cleans, and says so
/// in the log.
#[must_use]
pub fn production_sweep(repo_root: &Path) -> SweepOutcome {
    if exe_is_inside_artifacts(repo_root, std::env::current_exe().ok().as_deref()) {
        return SweepOutcome::Deferred(format!(
            "the running binary lives inside {}'s own build artifacts — refusing to delete it",
            repo_root.display()
        ));
    }

    let Some(slot_dir) = crate::build_slot::slot_dir() else {
        return SweepOutcome::Deferred(
            "no machine build-slot directory is resolvable (no home directory)".to_string(),
        );
    };
    let lease = crate::build_slot::acquire_in(
        &slot_dir,
        crate::build_slot::resolve_slots(),
        Duration::from_secs(DEEP_CLEAN_SLOT_WAIT_SECS),
        DEEP_CLEAN_SLOT_POLL,
        crate::build_slot::resolve_stale(),
        "deep-clean",
    );
    if !lease.holds_slot() {
        let why = match lease.kind() {
            crate::build_slot::LeaseKind::DegradedOpen { reason } => reason.clone(),
            other => other.as_str().to_string(),
        };
        return SweepOutcome::Deferred(format!(
            "could not hold the machine build slot ({why}) — deferring so this can never delete \
             target/ under a running build"
        ));
    }

    let outcomes = clean::sweep_primary_checkout_artifacts(repo_root, false);
    // The lease drops (releasing the slot) at the end of this scope.
    drop(lease);

    let mut reclaimed = Vec::new();
    for outcome in outcomes {
        match outcome {
            ArtifactOutcome::Reclaimed(a) => reclaimed.push(a),
            ArtifactOutcome::Failed(name) => log::warn!(
                "deep_clean: {} could not remove {name}/ from the primary checkout",
                repo_root.display()
            ),
            ArtifactOutcome::Absent(_) => {}
        }
    }
    SweepOutcome::Swept(reclaimed)
}

/// Log one pass's outcome. A firing pass logs at `warn` (a disk shrinking by
/// tens of GB should be visible at default verbosity and attributable to this
/// exact decision); everything else is `debug`.
pub fn log_report(report: &DeepCleanReport) {
    let root = report.repo_root.display();
    if let Some(reason) = &report.deferred {
        log::warn!("deep_clean: {root} deferred ({}): {reason}", report.trigger.reason());
        return;
    }
    if report.trigger.fires() {
        log::warn!(
            "deep_clean: {root} {} — reclaimed {} from the primary checkout \
             (`clean --deep --safe` equivalent)",
            report.trigger.reason(),
            report.reclaimed_summary()
        );
    } else {
        log::debug!("deep_clean: {root} no pass: {}", report.trigger.reason());
    }
}

/// Run one production deep pass for `repo_root` and publish/log it.
///
/// `disk_warn_free_gb` is the reaper's already-resolved `diskWarnFreeGb`, used
/// as the floor unless `deepClean.freeGb` / [`DEEP_CLEAN_FREE_GB_ENV`] override
/// it. Blocking (it runs `df` and `remove_dir_all`) — callers must be on the
/// blocking pool, which the reaper's `spawn_blocking` already provides.
pub fn run_for(repo_root: &Path, disk_warn_free_gb: u64) -> DeepCleanReport {
    let config = read_deep_clean_config(repo_root);
    let opts = deep_clean_options(clean::DEFAULT_GRACE_PERIOD_SECS);
    let inputs = DeepCleanInputs {
        opts: &opts,
        enabled: resolve_enabled(&config),
        floor_gb: resolve_free_gb(&config, disk_warn_free_gb),
        min_interval_secs: resolve_min_interval_secs(&config),
        last_fired: last_fired_at(repo_root),
        now: Utc::now(),
    };
    let report = run_pass(
        repo_root,
        &inputs,
        &|p| crate::disk_headroom::path_free_gb(p),
        &production_sweep,
    );
    log_report(&report);
    publish(&report);
    report
}

// ============================================================================
// Process-global status (published here, read by `build_daemon_status`)
// ============================================================================

/// The publicly-observable per-repo deep-clean state rendered by
/// `loom-daemon status` (mirrors [`crate::auto_update::AutoUpdateStatusSnapshot`]).
///
/// Deliberately *not* persisted to disk: the only thing a restart loses is the
/// cooldown, and re-firing right after a restart is harmless (the artifacts are
/// already gone, so the pass reclaims nothing and re-arms). A state file would
/// buy nothing and add a new untracked path to every consumer repo.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DeepCleanState {
    /// When the most recent evaluation ran (fired or not).
    pub last_evaluated_at: Option<DateTime<Utc>>,
    /// That evaluation's rationale — [`DeepCleanTrigger::reason`], or the
    /// deferral reason when a safety gate stopped a firing pass.
    pub last_reason: Option<String>,
    /// Free GB measured at the most recent evaluation.
    pub last_free_gb: Option<u64>,
    /// When a pass most recently *fired* this daemon process.
    pub last_fired_at: Option<DateTime<Utc>>,
    /// What that firing pass reclaimed ([`DeepCleanReport::reclaimed_summary`]).
    pub last_reclaimed: Option<String>,
}

type StateMap = BTreeMap<PathBuf, DeepCleanState>;

/// Process-global per-repo state. `BTreeMap` so [`snapshot`] renders in a
/// stable order regardless of registry iteration.
static GLOBAL_STATE: OnceLock<Mutex<StateMap>> = OnceLock::new();

fn global_state() -> &'static Mutex<StateMap> {
    GLOBAL_STATE.get_or_init(|| Mutex::new(StateMap::new()))
}

/// Record `report` against its repo. Called after every evaluation, not only
/// after a firing one — "last evaluated 3m ago, 118G free" is the answer to
/// "is reclamation happening?" on a healthy host.
pub fn publish(report: &DeepCleanReport) {
    let mut guard = global_state()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let entry = guard.entry(report.repo_root.clone()).or_default();
    entry.last_evaluated_at = Some(report.at);
    entry.last_reason = Some(report.deferred.clone().map_or_else(
        || report.trigger.reason(),
        |d| format!("{} — deferred: {d}", report.trigger.reason()),
    ));
    entry.last_free_gb = report.free_gb;
    // A deferred pass never removed anything, so it must not re-arm the
    // cooldown — otherwise a repo that keeps losing the build-slot race would
    // go 6h between *attempts* instead of retrying next tick.
    if report.trigger.fires() && report.deferred.is_none() {
        entry.last_fired_at = Some(report.at);
        entry.last_reclaimed = Some(report.reclaimed_summary());
    }
}

/// When a deep pass last fired for `repo_root` in this process — the cooldown
/// input for the next [`evaluate`].
#[must_use]
pub fn last_fired_at(repo_root: &Path) -> Option<DateTime<Utc>> {
    global_state()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(repo_root)
        .and_then(|s| s.last_fired_at)
}

/// Every repo's current state, root-sorted, for `build_daemon_status`.
#[must_use]
pub fn snapshot() -> Vec<(PathBuf, DeepCleanState)> {
    global_state()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect()
}

/// Drop all published state. Test-only seam (the process-global would otherwise
/// leak between `#[serial]` tests in the same binary).
#[doc(hidden)]
pub fn reset_state_for_test() {
    global_state()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clear();
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use serial_test::serial;
    use std::fs;

    fn t(secs: i64) -> DateTime<Utc> {
        Utc.timestamp_opt(1_800_000_000 + secs, 0).unwrap()
    }

    fn artifact(name: &str) -> ReclaimedArtifact {
        ReclaimedArtifact {
            name: name.to_string(),
            size_human: "34.1G".to_string(),
        }
    }

    /// Firing inputs (below the floor, no prior pass) around `opts`.
    fn inputs<'a>(opts: &'a CleanOptions) -> DeepCleanInputs<'a> {
        DeepCleanInputs {
            opts,
            enabled: true,
            floor_gb: 20,
            min_interval_secs: 21_600,
            last_fired: None,
            now: t(0),
        }
    }

    /// The real primary-checkout removal, without the build-slot acquisition
    /// (which is machine-global and belongs to `production_sweep`).
    fn real_sweep(p: &Path) -> SweepOutcome {
        SweepOutcome::Swept(
            clean::sweep_primary_checkout_artifacts(p, false)
                .into_iter()
                .filter_map(|o| match o {
                    ArtifactOutcome::Reclaimed(a) => Some(a),
                    _ => None,
                })
                .collect(),
        )
    }

    // ===================================================================
    // Trigger (the "long-lived host", in compressed time)
    // ===================================================================

    #[test]
    fn headroom_above_the_floor_does_not_fire() {
        let trigger = evaluate(true, Some(118), 20, None, t(0), 21_600);
        assert!(!trigger.fires());
        assert_eq!(
            trigger,
            DeepCleanTrigger::AboveFloor {
                free_gb: 118,
                floor_gb: 20
            }
        );
    }

    #[test]
    fn free_space_exactly_at_the_floor_does_not_fire() {
        // The floor is the warn threshold, and the reaper warns *below* it —
        // `>=` keeps the two consistent rather than firing one GB early.
        assert!(!evaluate(true, Some(20), 20, None, t(0), 21_600).fires());
    }

    #[test]
    fn crossing_the_floor_fires_when_no_pass_has_ever_run() {
        let trigger = evaluate(true, Some(2), 20, None, t(0), 21_600);
        assert!(trigger.fires());
        assert!(trigger.reason().contains("DISK PRESSURE"), "{}", trigger.reason());
    }

    #[test]
    fn disabled_never_fires_however_full_the_disk_is() {
        assert_eq!(evaluate(false, Some(0), 20, None, t(0), 21_600), DeepCleanTrigger::Disabled);
    }

    #[test]
    fn unmeasurable_free_space_never_fires() {
        // #4164's contract: unknown != zero. A `df` failure must not read as
        // "0G free" and delete a developer's build cache.
        assert_eq!(
            evaluate(true, None, 20, None, t(0), 21_600),
            DeepCleanTrigger::FreeSpaceUnknown
        );
    }

    #[test]
    fn a_recent_pass_holds_the_cooldown() {
        let trigger = evaluate(true, Some(2), 20, Some(t(0)), t(600), 21_600);
        assert!(!trigger.fires());
        assert!(matches!(
            trigger,
            DeepCleanTrigger::Cooldown {
                since_secs: 600,
                ..
            }
        ));
    }

    #[test]
    fn the_cooldown_expires_and_the_pass_fires_again() {
        assert!(evaluate(true, Some(2), 20, Some(t(0)), t(21_601), 21_600).fires());
    }

    #[test]
    fn a_backwards_clock_step_holds_the_cooldown_rather_than_thrashing() {
        let trigger = evaluate(true, Some(2), 20, Some(t(1_000)), t(0), 21_600);
        assert!(!trigger.fires(), "{trigger:?}");
    }

    /// The regression test the issue asks for, time-compressed: simulate a
    /// long-lived host tick-by-tick and assert the disk is reclaimed **without
    /// an operator or an external timer**, and that it does not thrash.
    #[test]
    fn a_long_lived_host_reclaims_on_its_own_and_does_not_thrash() {
        let floor = 20;
        let cooldown = 21_600; // 6h
        let tick = 900; // the reaper's 15m cadence

        let mut free_gb: i64 = 60;
        let mut last_fired: Option<DateTime<Utc>> = None;
        let mut fires = 0;

        // Four simulated days at a 15-minute cadence, with `target/` regrowing
        // ~1G per tick (the reported fleet's ~34G/day) and a firing pass
        // reclaiming it.
        for i in 0..(4 * 24 * 4) {
            let now = t(i * tick);
            free_gb -= 1;
            let trigger = evaluate(
                true,
                Some(u64::try_from(free_gb.max(0)).unwrap()),
                floor,
                last_fired,
                now,
                cooldown,
            );
            if trigger.fires() {
                fires += 1;
                last_fired = Some(now);
                free_gb += 40; // one deep pass reclaims the accumulated target/
            }
            assert!(
                free_gb > 0,
                "host ran out of disk at tick {i} — the pass never closed the loop"
            );
        }

        // It fired repeatedly (the loop IS closed) but nowhere near every tick
        // (the cooldown holds): 4 days at 40G reclaimed per ~40 ticks of 1G.
        assert!(fires >= 3, "expected the pass to fire on its own, fired {fires}x");
        assert!(fires <= 16, "pass thrashed: fired {fires}x in 4 simulated days");
    }

    #[test]
    fn a_disk_full_for_a_non_artifact_reason_does_not_delete_every_tick() {
        // Nothing to reclaim (a huge dataset fills the volume). The cooldown is
        // the only thing standing between this and a delete/rebuild loop.
        let mut last_fired: Option<DateTime<Utc>> = None;
        let mut fires = 0;
        for i in 0..96 {
            let now = t(i * 900);
            let trigger = evaluate(true, Some(3), 20, last_fired, now, 21_600);
            if trigger.fires() {
                fires += 1;
                last_fired = Some(now);
            }
        }
        // 96 ticks = 24h at 15m ⇒ at most 24h/6h = 4 passes, plus the first.
        assert!(fires <= 5, "fired {fires}x in 24 simulated hours");
    }

    // ===================================================================
    // Options: `safe: true` on every scheduled path
    // ===================================================================

    #[test]
    fn scheduled_deep_options_are_deep_and_safe_and_never_forced() {
        let opts = deep_clean_options(3600);
        assert!(opts.deep, "the scheduled pass must be a deep pass");
        assert!(opts.safe, "a scheduled bare --deep must never exist");
        assert!(!opts.force, "the scheduled pass must never --force");
        assert!(!opts.dry_run);
        assert!(opts.require_managed_sentinel);
        assert_eq!(opts.grace_period_secs, 3600);
    }

    #[test]
    fn run_pass_refuses_options_that_are_not_safe() {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir_all(tmp.path().join("target")).unwrap();
        let mut opts = deep_clean_options(0);
        opts.safe = false; // a hand-rolled bare `--deep`

        let swept = std::cell::Cell::new(false);
        let sweep = |_: &Path| {
            swept.set(true);
            SweepOutcome::Swept(vec![artifact("target")])
        };
        let report = run_pass(tmp.path(), &inputs(&opts), &|_| Some(1), &sweep);

        assert!(!swept.get(), "an unsafe scheduled pass must never reach the removal");
        assert!(report.deferred.is_some());
        assert!(tmp.path().join("target").is_dir(), "nothing may be removed");
    }

    #[test]
    fn run_pass_refuses_forced_options() {
        let tmp = tempfile::tempdir().unwrap();
        let mut opts = deep_clean_options(0);
        opts.force = true;
        let sweep = |_: &Path| SweepOutcome::Swept(vec![artifact("target")]);
        let report = run_pass(tmp.path(), &inputs(&opts), &|_| Some(1), &sweep);
        assert!(report.reclaimed.is_empty());
        assert!(report.deferred.unwrap().contains("safe:true"));
    }

    // ===================================================================
    // The pass end-to-end (injected probe + removal)
    // ===================================================================

    #[test]
    fn a_pressured_pass_reclaims_the_primary_checkouts_artifacts() {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir_all(tmp.path().join("target/release")).unwrap();
        fs::write(tmp.path().join("target/release/blob"), vec![0u8; 4096]).unwrap();
        fs::create_dir_all(tmp.path().join("node_modules/pkg")).unwrap();
        // A worktree under the same root must be untouched by THIS pass (it is
        // `reclaim_kept_worktree_artifacts`'s job, and it has its own gates).
        fs::create_dir_all(tmp.path().join(".loom/worktrees/issue-1/target")).unwrap();

        let opts = deep_clean_options(0);
        let report = run_pass(tmp.path(), &inputs(&opts), &|_| Some(2), &real_sweep);

        assert!(report.trigger.fires());
        assert!(!tmp.path().join("target").exists());
        assert!(!tmp.path().join("node_modules").exists());
        assert!(
            tmp.path().join(".loom/worktrees/issue-1/target").is_dir(),
            "the primary-checkout pass must not reach into worktrees"
        );
        assert_eq!(report.reclaimed.len(), 2);
        assert!(report.reclaimed_summary().contains("target/"));
    }

    #[test]
    fn an_unpressured_pass_removes_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir_all(tmp.path().join("target")).unwrap();

        let opts = deep_clean_options(0);
        let report = run_pass(tmp.path(), &inputs(&opts), &|_| Some(500), &real_sweep);

        assert!(!report.trigger.fires());
        assert!(tmp.path().join("target").is_dir(), "no pressure ⇒ no deletion");
        assert!(!report.reclaimed_anything());
    }

    #[test]
    fn a_deferred_sweep_reports_the_reason_and_removes_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir_all(tmp.path().join("target")).unwrap();
        let opts = deep_clean_options(0);
        let report = run_pass(tmp.path(), &inputs(&opts), &|_| Some(1), &|_| {
            SweepOutcome::Deferred("could not hold the machine build slot".to_string())
        });
        assert!(report.trigger.fires());
        assert_eq!(report.deferred.as_deref(), Some("could not hold the machine build slot"));
        assert!(tmp.path().join("target").is_dir());
    }

    // ===================================================================
    // Build-artifact self-preservation
    // ===================================================================

    #[test]
    fn a_binary_running_from_the_repos_own_target_is_never_deleted() {
        let root = Path::new("/srv/loom");
        assert!(exe_is_inside_artifacts(
            root,
            Some(Path::new("/srv/loom/target/release/loom-daemon"))
        ));
        assert!(exe_is_inside_artifacts(
            root,
            Some(Path::new("/srv/loom/node_modules/.bin/thing"))
        ));
        assert!(!exe_is_inside_artifacts(
            root,
            Some(Path::new("/home/u/.local/bin/loom-daemon"))
        ));
        assert!(!exe_is_inside_artifacts(root, None));
        assert!(!exe_is_inside_artifacts(
            root,
            Some(Path::new("/srv/other/target/release/loom-daemon"))
        ));
    }

    // ===================================================================
    // Published status
    // ===================================================================
    //
    // Every test below this point mutates process-global state — either the
    // published `GLOBAL_STATE` map or the `LOOM_DEEP_CLEAN*` env vars — and
    // `run_for` touches both at once. They therefore all share ONE serial
    // group (`loom_config_env`, the repo-wide key for env/config-touching
    // tests) rather than splitting into two groups that could interleave and
    // let a `reset_state_for_test()` wipe another test's published entry.

    #[test]
    #[serial(loom_config_env)]
    fn publishing_records_both_evaluations_and_firings() {
        reset_state_for_test();
        let root = PathBuf::from("/srv/repo-a");

        // A quiet tick: evaluated, not fired.
        publish(&DeepCleanReport {
            repo_root: root.clone(),
            trigger: DeepCleanTrigger::AboveFloor {
                free_gb: 118,
                floor_gb: 20,
            },
            deferred: None,
            reclaimed: Vec::new(),
            free_gb: Some(118),
            at: t(0),
        });
        assert_eq!(last_fired_at(&root), None);
        let state = snapshot();
        assert_eq!(state.len(), 1);
        assert_eq!(state[0].1.last_evaluated_at, Some(t(0)));
        assert!(state[0]
            .1
            .last_reason
            .as_ref()
            .unwrap()
            .contains("no disk pressure"));

        // A firing tick.
        publish(&DeepCleanReport {
            repo_root: root.clone(),
            trigger: DeepCleanTrigger::DiskPressure {
                free_gb: 2,
                floor_gb: 20,
            },
            deferred: None,
            reclaimed: vec![artifact("target")],
            free_gb: Some(2),
            at: t(60),
        });
        assert_eq!(last_fired_at(&root), Some(t(60)));
        let state = snapshot();
        assert_eq!(state[0].1.last_reclaimed.as_deref(), Some("target/ (34.1G)"));
        reset_state_for_test();
    }

    #[test]
    #[serial(loom_config_env)]
    fn a_deferred_pass_does_not_arm_the_cooldown() {
        reset_state_for_test();
        let root = PathBuf::from("/srv/repo-b");
        publish(&DeepCleanReport {
            repo_root: root.clone(),
            trigger: DeepCleanTrigger::DiskPressure {
                free_gb: 2,
                floor_gb: 20,
            },
            deferred: Some("could not hold the machine build slot".to_string()),
            reclaimed: Vec::new(),
            free_gb: Some(2),
            at: t(0),
        });
        assert_eq!(
            last_fired_at(&root),
            None,
            "a deferred pass reclaimed nothing — it must retry next tick, not wait out a cooldown"
        );
        assert!(snapshot()[0]
            .1
            .last_reason
            .as_ref()
            .unwrap()
            .contains("deferred"));
        reset_state_for_test();
    }

    // ===================================================================
    // Config resolution
    // ===================================================================

    fn write_config(root: &Path, json: &str) {
        fs::create_dir_all(root.join(".loom")).unwrap();
        fs::write(root.join(".loom/config.json"), json).unwrap();
    }

    /// Isolate config/env resolution from the ambient host: disable the
    /// private/shared defaults tier (same guard the reaper's own config tests
    /// use) and clear the three `LOOM_DEEP_CLEAN*` overrides, which a
    /// dispatched agent's environment may already carry.
    fn clear_ambient_overrides() {
        std::env::set_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV, "");
        std::env::remove_var(DEEP_CLEAN_ENABLE_ENV);
        std::env::remove_var(DEEP_CLEAN_FREE_GB_ENV);
        std::env::remove_var(DEEP_CLEAN_MIN_INTERVAL_ENV);
    }

    fn restore_ambient_overrides() {
        std::env::remove_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV);
        std::env::remove_var(DEEP_CLEAN_ENABLE_ENV);
        std::env::remove_var(DEEP_CLEAN_FREE_GB_ENV);
        std::env::remove_var(DEEP_CLEAN_MIN_INTERVAL_ENV);
    }

    #[test]
    #[serial(loom_config_env)]
    fn config_absent_means_enabled_with_the_reapers_own_floor() {
        clear_ambient_overrides();
        let tmp = tempfile::tempdir().unwrap();
        let cfg = read_deep_clean_config(tmp.path());
        assert_eq!(cfg, DeepCleanConfig::default());
        assert!(resolve_enabled(&cfg));
        assert_eq!(resolve_free_gb(&cfg, 20), 20);
        assert_eq!(resolve_min_interval_secs(&cfg), DEFAULT_DEEP_CLEAN_MIN_INTERVAL_SECS);
        restore_ambient_overrides();
    }

    #[test]
    #[serial(loom_config_env)]
    fn config_block_is_read() {
        clear_ambient_overrides();
        let tmp = tempfile::tempdir().unwrap();
        write_config(
            tmp.path(),
            r#"{"autonomous":{"worktreeReaper":{"deepClean":{"enabled":false,"freeGb":50,"minIntervalSecs":900}}}}"#,
        );
        let cfg = read_deep_clean_config(tmp.path());
        assert_eq!(cfg.enabled, Some(false));
        assert_eq!(resolve_free_gb(&cfg, 20), 50);
        assert_eq!(resolve_min_interval_secs(&cfg), 900);
        assert!(!resolve_enabled(&cfg));
        restore_ambient_overrides();
    }

    #[test]
    #[serial(loom_config_env)]
    fn a_zero_min_interval_in_config_falls_through_to_the_default() {
        clear_ambient_overrides();
        let tmp = tempfile::tempdir().unwrap();
        write_config(
            tmp.path(),
            r#"{"autonomous":{"worktreeReaper":{"deepClean":{"minIntervalSecs":0}}}}"#,
        );
        let cfg = read_deep_clean_config(tmp.path());
        assert_eq!(cfg.min_interval_secs, None);
        assert_eq!(resolve_min_interval_secs(&cfg), DEFAULT_DEEP_CLEAN_MIN_INTERVAL_SECS);
        restore_ambient_overrides();
    }

    #[test]
    #[serial(loom_config_env)]
    fn the_env_opt_out_beats_a_config_that_enables_the_pass() {
        // `LOOM_DEEP_CLEAN=0` is the escape hatch the daemon reference
        // documents for a host that must keep its build cache. It has to win
        // over `enabled: true` in config, or the documented opt-out is a lie
        // on any repo that has explicitly turned the pass on.
        clear_ambient_overrides();
        let cfg = DeepCleanConfig {
            enabled: Some(true),
            ..DeepCleanConfig::default()
        };
        for falsy in ["0", "false", "no", "off"] {
            std::env::set_var(DEEP_CLEAN_ENABLE_ENV, falsy);
            assert!(!resolve_enabled(&cfg), "LOOM_DEEP_CLEAN={falsy} must disable the pass");
        }
        // And the inverse: the env var force-enables over a disabling config.
        std::env::set_var(DEEP_CLEAN_ENABLE_ENV, "1");
        assert!(resolve_enabled(&DeepCleanConfig {
            enabled: Some(false),
            ..DeepCleanConfig::default()
        }));
        restore_ambient_overrides();
    }

    #[test]
    #[serial(loom_config_env)]
    fn env_overrides_beat_config_for_the_floor_and_the_cooldown() {
        clear_ambient_overrides();
        let cfg = DeepCleanConfig {
            enabled: None,
            free_gb: Some(50),
            min_interval_secs: Some(900),
        };
        std::env::set_var(DEEP_CLEAN_FREE_GB_ENV, "7");
        std::env::set_var(DEEP_CLEAN_MIN_INTERVAL_ENV, "60");
        assert_eq!(resolve_free_gb(&cfg, 20), 7);
        assert_eq!(resolve_min_interval_secs(&cfg), 60);

        // A garbage or zero env value falls THROUGH to config rather than
        // disabling the anti-thrash gate or fabricating a 0G floor.
        std::env::set_var(DEEP_CLEAN_FREE_GB_ENV, "not-a-number");
        std::env::set_var(DEEP_CLEAN_MIN_INTERVAL_ENV, "0");
        assert_eq!(resolve_free_gb(&cfg, 20), 50);
        assert_eq!(resolve_min_interval_secs(&cfg), 900);
        restore_ambient_overrides();
    }

    // ===================================================================
    // `run_for` — the production entry point the reaper calls each tick
    // ===================================================================

    #[test]
    #[serial(loom_config_env)]
    fn run_for_publishes_a_non_firing_evaluation_without_touching_the_checkout() {
        // Pins the whole production path the reaper wires (`read_deep_clean_config`
        // -> resolve -> `run_pass` -> `log_report` -> `publish`) without depending
        // on how full the CI host's disk happens to be: a 0G floor cannot be
        // crossed by any measurable free-space value, so this asserts the
        // evaluation is published while proving nothing was deleted.
        clear_ambient_overrides();
        reset_state_for_test();
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir_all(tmp.path().join("target/release")).unwrap();
        std::env::set_var(DEEP_CLEAN_FREE_GB_ENV, "0");

        let report = run_for(tmp.path(), 20);

        assert!(!report.trigger.fires(), "{:?}", report.trigger);
        assert!(
            tmp.path().join("target").is_dir(),
            "an unpressured tick must never touch the primary checkout"
        );
        let published = snapshot();
        assert_eq!(published.len(), 1, "every tick publishes, firing or not");
        assert_eq!(published[0].0, tmp.path());
        assert!(published[0].1.last_evaluated_at.is_some());
        assert_eq!(published[0].1.last_fired_at, None);

        reset_state_for_test();
        restore_ambient_overrides();
    }

    #[test]
    #[serial(loom_config_env)]
    fn run_for_honors_the_config_opt_out_end_to_end() {
        clear_ambient_overrides();
        reset_state_for_test();
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir_all(tmp.path().join("target")).unwrap();
        write_config(
            tmp.path(),
            r#"{"autonomous":{"worktreeReaper":{"deepClean":{"enabled":false}}}}"#,
        );

        let report = run_for(tmp.path(), 20);

        assert_eq!(report.trigger, DeepCleanTrigger::Disabled);
        assert_eq!(report.free_gb, None, "a disabled pass must not even probe the filesystem");
        assert!(tmp.path().join("target").is_dir());
        assert!(snapshot()[0]
            .1
            .last_reason
            .as_ref()
            .unwrap()
            .contains("disabled"));

        reset_state_for_test();
        restore_ambient_overrides();
    }
}
