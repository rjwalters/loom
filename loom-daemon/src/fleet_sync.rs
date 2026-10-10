//! Startup + timer sync of the operator's fleet state store (Issue #9596).
//!
//! [`crate::fleet_store`] reads the store **on explicit command only**
//! (`loom-daemon fleet-config …`). With a store configured, a host therefore
//! converged on it only when a human ran `render` / `roster --apply` by hand: a
//! reviewed commit landed in the store and then sat unapplied, per host,
//! indefinitely.
//!
//! This module is the daemon's own consumer of that reader:
//!
//! - **At startup** ([`start`]), before the config-dependent loops are spawned:
//!   fetch, then render the machine tier and the host-local tier. The process
//!   that goes on to read `autonomous.*` is then the one the store configures.
//! - **On a timer** (`fleet.syncIntervalSecs`): fetch + render-check +
//!   roster-check. The steady state is one conditional `304` per tick (the
//!   fetch layer's own `If-None-Match` revalidation). Drift is logged,
//!   published on the event bus, and recorded in the host-level snapshot
//!   `loom-daemon status` renders — **without writing anything**, unless
//!   `fleet.autoApply` is on.
//! - **The fleet version floor** (`loom_min_version`, #10711): every pass reads
//!   it from the snapshot into a process-wide value ([`loom_min_version`]),
//!   not into the config tiers, so a change takes effect on the next tick with
//!   no restart. A malformed value keeps the last good floor and alerts. The
//!   self-update loop rolls a host below it (`auto_update::floor_roll`, #10712),
//!   reads it as three values ([`floor_knowledge`]) and is woken when a pass
//!   resolves a different floor ([`floor_wake`], #10885).
//! - **The workspace resync** ([`workspace_resync`], #10718): after each timer
//!   pass, and once at startup, every registered repo's installed Loom is
//!   compared with this daemon's own payload and, on a host in H0 with
//!   `fleet.autoApply` on, resynced under a per-repo claim. It runs on its
//!   own task, one pass at a time, and the timer never waits for it.
//! - **The checkout fast-forward** ([`checkout_ff`], #10869): each registered
//!   workspace's main checkout is fast-forwarded to its default branch when it
//!   is clean and strictly behind, so a resync that landed on the forge is the
//!   one this host dispatches from. It runs inside the startup pass (before
//!   any dispatch producer exists), and then on the workspace resync's own
//!   task, right after each resync pass, so the timer never waits for it
//!   either. It writes only with `fleet.autoApply` on.
//! - **Roster clones** ([`roster_clone`], #11218): with `fleet.autoApply` on, a
//!   timer pass clones a desired repo that is not cloned under `root` yet
//!   (HTTPS, bounded per pass) and registers it in the same pass.
//!
//! # Invariants this module keeps
//!
//! - **`fleet.repo` unset ⇒ byte-identical.** [`resolve_config`] returns
//!   `Ok(None)` and [`start`] does nothing at all (beyond removing a stale
//!   snapshot this module itself wrote on an earlier, configured run, so
//!   `status` never reports a store the host no longer reads).
//! - **An unreachable forge at startup must not block boot.** The config pass
//!   loads under [`Policy::AllowStale`] — the last good cached snapshot serves
//!   the render, with a staleness warning — and the whole pass is run on a
//!   blocking thread under a wall-clock cap
//!   ([`resolve_startup_timeout`]), because the `gh` child it shells out to has
//!   no timeout of its own. A failure or a timeout is recorded and boot
//!   continues.
//! - **The roster stays fail-closed.** [`roster_pass`] loads under
//!   [`Policy::FailClosed`]: the snapshot must have been confirmed current by
//!   the forge in this pass (a `304` counts), there is no cached fallback, and
//!   a both-flags (`fleet: true` + `firewall: true`) record is a hard error for
//!   the whole roster — [`crate::fleet_store::roster::parse`]'s behaviour,
//!   reused verbatim rather than re-implemented.
//! - **Writes are opt-in on the timer.** The startup pass renders (that is what
//!   "the process that starts is the one the store configured" means). Every
//!   *subsequent* pass only checks, unless `fleet.autoApply` is set: a roster
//!   apply deregisters workspaces, so it never happens by default.
//!
//! # Where the startup pass sits in `run_daemon`
//!
//! After the forge-credential preflight, before every `read_*_config` call that
//! gates a loop. It cannot move earlier: the store is read through `gh` under
//! the daemon's own credentials, so the credential preflight necessarily
//! resolves first — within one boot the fetch uses whatever credentials are
//! already on the host, and a credential change landing in the store takes
//! effect on the next start. That ordering is inherent to reading the store
//! from the forge, not a limitation of this module.

use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::fleet_state::{self, Enforcement, Enforcer, StatePass};
use crate::fleet_store::fetch::{self, Freshness, Policy, Transport};
use crate::fleet_store::floor::{self, FloorRead};
use crate::fleet_store::render::{self, Drift};
use crate::fleet_store::roster::{self, Change, Plan, Registered};
use crate::fleet_store::{self as store, StoreLocation};

pub mod checkout_ff;
mod floor_knowledge;
pub mod offline_floor;
pub mod roster_clone;
pub mod workspace_resync;

pub use floor_knowledge::{floor_knowledge, floor_wake, FloorKnowledge, FloorWake};

/// Config key for the timer cadence.
pub const SYNC_INTERVAL_KEY: &str = "fleet.syncIntervalSecs";
/// Env override for the timer cadence.
pub const SYNC_INTERVAL_ENV: &str = "LOOM_FLEET_SYNC_INTERVAL_SECS";
/// Config key for the opt-in write path.
pub const AUTO_APPLY_KEY: &str = "fleet.autoApply";
/// Env override for the opt-in write path.
pub const AUTO_APPLY_ENV: &str = "LOOM_FLEET_AUTO_APPLY";
/// Env override for the startup pass's wall-clock cap.
pub const STARTUP_TIMEOUT_ENV: &str = "LOOM_FLEET_SYNC_STARTUP_TIMEOUT_SECS";

/// Timer cadence when nothing configures one.
pub const DEFAULT_SYNC_INTERVAL_SECS: u64 = 300;
/// Floor on the timer cadence: a configured value below this is clamped up, so
/// a typo cannot turn a conditional read into a hot loop against the forge.
pub const MIN_SYNC_INTERVAL_SECS: u64 = 30;
/// Wall-clock cap on the startup pass when nothing configures one.
pub const DEFAULT_STARTUP_TIMEOUT_SECS: u64 = 60;

/// Name of the host-level snapshot under `<loom_dir>`.
pub const STATUS_FILENAME: &str = "fleet-sync-status.json";

/// Event-bus topic the timer pass publishes a drift/health record on.
pub const DRIFT_TOPIC: &str = "fleet.sync.drift";

/// Event-bus topic a run-state *transition* is published on (#9598). Only
/// transitions land here — the steady state of a paused host is silent.
pub const STATE_TOPIC: &str = "fleet.sync.state";

// ============================================================================
// Configuration
// ============================================================================

/// The resolved sync configuration for this host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncConfig {
    /// Where the store is.
    pub location: StoreLocation,
    /// Timer cadence.
    pub interval: Duration,
    /// Whether a pass may write (render) / apply (roster) on its own.
    pub auto_apply: bool,
}

/// Resolve the sync configuration from `effective_config`, with `env` (a
/// lookup, so tests need not touch the process environment) taking precedence.
/// `Ok(None)` means `fleet.repo` is unset — the feature is off and the daemon's
/// behaviour is unchanged.
pub fn resolve_config(
    effective_config: &Value,
    env: &dyn Fn(&str) -> Option<String>,
) -> Result<Option<SyncConfig>> {
    let Some(location) = store::resolve_location(effective_config, env)? else {
        return Ok(None);
    };
    Ok(Some(SyncConfig {
        location,
        interval: resolve_interval(effective_config, env),
        auto_apply: resolve_auto_apply(effective_config, env),
    }))
}

/// Timer cadence: `LOOM_FLEET_SYNC_INTERVAL_SECS` over `fleet.syncIntervalSecs`
/// over [`DEFAULT_SYNC_INTERVAL_SECS`], clamped up to
/// [`MIN_SYNC_INTERVAL_SECS`].
#[must_use]
pub fn resolve_interval(
    effective_config: &Value,
    env: &dyn Fn(&str) -> Option<String>,
) -> Duration {
    let secs = env(SYNC_INTERVAL_ENV)
        .and_then(|s| s.trim().parse::<u64>().ok())
        .or_else(|| {
            crate::config_resolver::get_path(effective_config, SYNC_INTERVAL_KEY).and_then(|v| {
                v.as_u64()
                    .or_else(|| v.as_str().and_then(|s| s.trim().parse().ok()))
            })
        })
        .unwrap_or(DEFAULT_SYNC_INTERVAL_SECS);
    Duration::from_secs(secs.max(MIN_SYNC_INTERVAL_SECS))
}

/// The opt-in write path: `LOOM_FLEET_AUTO_APPLY` over `fleet.autoApply`, off
/// by default. Anything other than a truthy value reads as off.
#[must_use]
pub fn resolve_auto_apply(effective_config: &Value, env: &dyn Fn(&str) -> Option<String>) -> bool {
    if let Some(raw) = env(AUTO_APPLY_ENV) {
        return matches!(raw.trim(), "1" | "true" | "TRUE" | "yes" | "on");
    }
    crate::config_resolver::get_path(effective_config, AUTO_APPLY_KEY)
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

/// Wall-clock cap on the startup pass, from [`STARTUP_TIMEOUT_ENV`] over
/// [`DEFAULT_STARTUP_TIMEOUT_SECS`]. A `0` disables the cap.
#[must_use]
pub fn resolve_startup_timeout(env: &dyn Fn(&str) -> Option<String>) -> Option<Duration> {
    let secs = env(STARTUP_TIMEOUT_ENV)
        .and_then(|s| s.trim().parse::<u64>().ok())
        .unwrap_or(DEFAULT_STARTUP_TIMEOUT_SECS);
    (secs > 0).then(|| Duration::from_secs(secs))
}

// ============================================================================
// One pass over the config tiers
// ============================================================================

/// Whether a config pass may write what it renders.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Render and write drifting targets (startup, or `fleet.autoApply`).
    Write,
    /// Report drift only; write nothing.
    Check,
}

/// What one pass found for one config tier.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TierReport {
    /// `machine tier` / `host-local tier`.
    pub tier: String,
    /// The file the tier is read from.
    pub path: PathBuf,
    /// Whether the file differed from the store's render.
    pub drifted: bool,
    /// Whether this pass rewrote it.
    pub wrote: bool,
    /// One line describing the drift, when there was any.
    pub detail: Option<String>,
}

/// The outcome of one config pass.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfigPass {
    /// The store commit the pass read, when it read one.
    pub commit: Option<String>,
    /// Whether the snapshot came from the cache rather than the forge.
    pub cached: bool,
    /// Per-tier findings.
    pub tiers: Vec<TierReport>,
    /// Why the pass could not be completed, when it could not be.
    pub error: Option<String>,
}

impl ConfigPass {
    /// Whether any tier on disk differs from the store.
    #[must_use]
    pub fn drifted(&self) -> bool {
        self.tiers.iter().any(|t| t.drifted)
    }

    /// Whether this pass rewrote any tier.
    #[must_use]
    pub fn wrote(&self) -> bool {
        self.tiers.iter().any(|t| t.wrote)
    }
}

/// Fetch and render `host`'s config tiers, writing them when `mode` is
/// [`Mode::Write`].
///
/// Never returns `Err`: every failure is recorded in [`ConfigPass::error`], so
/// a caller on the boot path can log it and carry on. The load is
/// [`Policy::AllowStale`], so an unreachable forge degrades to the last good
/// cached snapshot rather than skipping the render.
#[allow(clippy::too_many_arguments)] // pure decision seam: each arg is a distinct injected evidence source, matching the convention in claim_reconciliation.rs
pub fn config_pass(
    transport: &dyn Transport,
    cache_dir: &Path,
    location: &StoreLocation,
    host: &str,
    machine_path: &Path,
    local_path: &Path,
    mode: Mode,
    now: DateTime<Utc>,
) -> ConfigPass {
    let loaded = match fetch::load(transport, cache_dir, location, Policy::AllowStale, now) {
        Ok(l) => l,
        Err(e) => {
            return ConfigPass {
                error: Some(format!("{e:#}")),
                ..ConfigPass::default()
            }
        }
    };
    let cached = matches!(loaded.freshness, Freshness::Cached { .. });
    let commit = Some(loaded.snapshot.manifest.commit.clone());
    let targets = match render::render(&loaded.snapshot, host, machine_path, local_path) {
        Ok(t) => t,
        Err(e) => {
            return ConfigPass {
                commit,
                cached,
                tiers: Vec::new(),
                error: Some(format!("{e:#}")),
            }
        }
    };
    let stamp = now.format("%Y%m%dT%H%M%SZ").to_string();
    let mut tiers = Vec::with_capacity(targets.len());
    let mut error = None;
    for target in &targets {
        let drift = render::drift(target);
        let drifted = drift != Drift::InSync;
        let mut wrote = false;
        let mut detail = describe_drift(&drift);
        if mode == Mode::Write && drifted {
            let lost = render::lost_top_level_keys(target);
            if lost.is_empty() {
                match render::write(target, &stamp) {
                    Ok((w, _backup)) => wrote = w,
                    Err(e) => {
                        error = Some(format!("writing {}: {e:#}", target.path.display()));
                    }
                }
            } else {
                // Lossy-reduction guard (2am#1653), automated-path half: no
                // operator is present on this path to answer an
                // `--allow-reduce` prompt, so the write is simply skipped and
                // the loss surfaced via `error`/`detail` instead.
                let msg = format!(
                    "{} would DROP top-level block(s) the file on disk carries: {} — \
                     refusing to write; the store never had them (`fleet-config propose \
                     adopt` proposes adding them to the store first)",
                    target.tier.name(),
                    lost.join(", ")
                );
                error = Some(msg.clone());
                detail = Some(msg);
            }
        }
        tiers.push(TierReport {
            tier: target.tier.name().to_string(),
            path: target.path.clone(),
            drifted,
            wrote,
            detail,
        });
    }
    ConfigPass {
        commit,
        cached,
        tiers,
        error,
    }
}

/// One line summarising a drift result; `None` when in sync.
#[must_use]
pub fn describe_drift(drift: &Drift) -> Option<String> {
    match drift {
        Drift::InSync => None,
        Drift::Missing => Some("does not exist".to_string()),
        Drift::Unparseable(e) => Some(format!("not valid JSON ({e})")),
        Drift::Differs(lines) => Some(lines.join("; ")),
    }
}

// ============================================================================
// One pass over the roster
// ============================================================================

/// The outcome of one roster pass.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RosterPass {
    /// One line per planned change, in apply order.
    pub drift: Vec<String>,
    /// Changes this pass actually applied.
    pub applied: usize,
    /// Desired repos that could not be applied (not cloned under `root`).
    pub unapplied: usize,
    /// Missing clones this pass cloned or tried to (#11218).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub clones: Vec<roster_clone::CloneAttempt>,
    /// Why the pass could not be completed, when it could not be.
    pub error: Option<String>,
    /// Why the roster was not consulted at all this pass — set when the
    /// config half was served from the cache (the forge was unreachable), in
    /// which case a fail-closed roster read could only fail. Reported as a
    /// *skip*, never as drift and never as an error: "not asked" and "asked
    /// and the answer was no" are different facts about a firewall input.
    pub skipped: Option<String>,
}

impl RosterPass {
    /// Whether the registry differs from the store.
    #[must_use]
    pub fn drifted(&self) -> bool {
        !self.drift.is_empty()
    }
}

/// Diff the store's roster against `registered`, fail-closed.
///
/// Unlike [`config_pass`] this **can** fail: `Policy::FailClosed` means the
/// snapshot must have been confirmed current by the forge in this very pass,
/// with no cached fallback — the roster carries the fleet's firewall inputs, so
/// an unconfirmable roster is an error, never a stale answer.
#[allow(clippy::too_many_arguments)] // pure decision seam: each arg is a distinct injected evidence source, matching the convention in claim_reconciliation.rs
pub fn roster_pass(
    transport: &dyn Transport,
    cache_dir: &Path,
    location: &StoreLocation,
    home: &Path,
    registered: &[Registered],
    normalize: &dyn Fn(&Path) -> PathBuf,
    is_cloned: &dyn Fn(&Path) -> bool,
    now: DateTime<Utc>,
) -> Result<Plan> {
    let loaded = fetch::load(transport, cache_dir, location, Policy::FailClosed, now)?;
    let parsed = roster::from_snapshot(&loaded.snapshot, home)?
        .ok_or_else(|| anyhow!(roster::missing_message(&loaded.snapshot)))?;
    Ok(roster::plan(&parsed, registered, normalize, is_cloned))
}

/// Turn a [`Plan`] into a [`RosterPass`], applying it through `apply` when
/// `auto_apply` is set, and never cloning: a missing clone is counted
/// unapplied. `apply` is a seam so tests never touch a real registry;
/// production passes [`apply_change`]. The timer clones through
/// [`roster_clone::summarize`] instead (#11218).
pub fn summarize_roster(
    plan: &Plan,
    auto_apply: bool,
    apply: &mut dyn FnMut(&Change) -> Result<()>,
) -> RosterPass {
    roster_clone::summarize(plan, auto_apply, &roster_clone::Clones::off(), apply)
}

/// Apply one planned change to the machine-level workspace registry at
/// `registry_path` — the same registry `loom-daemon workspace add/remove/
/// set-priority` edits, through the same [`crate::workspace_registry`] methods
/// those verbs use. Loads and saves per change so a mid-plan failure leaves the
/// earlier changes durably applied, exactly as the CLI's per-verb loop does.
pub fn apply_change(registry_path: &Path, change: &Change) -> Result<()> {
    use crate::workspace_registry::{MaintainOnlySource, WorkspaceRegistry};
    let mut registry = WorkspaceRegistry::load(registry_path)
        .with_context(|| format!("reading {}", registry_path.display()))?;
    // #11186: the store's maintain-only mark. An add carries it in the same
    // write, so the workspace is never registered dispatchable first.
    let mode = |on: bool| on.then_some(MaintainOnlySource::FleetStore);
    let changed = match change {
        Change::Add {
            path,
            priority,
            maintain_only,
            ..
        } => {
            let claude_state = crate::terminal::claude_config_state_path();
            registry.add_and_trust(path, None, *priority, &claude_state)?;
            registry.set_maintain_only(path, mode(*maintain_only), Utc::now());
            true
        }
        Change::Remove { path, .. } => registry.remove(path),
        Change::SetPriority { path, to, .. } => registry.set_priority(path, *to),
        Change::SetMaintainOnly { path, to, .. } => registry
            .set_maintain_only(path, mode(*to), Utc::now())
            .unwrap_or(false),
        // Never reachable: a missing clone is counted unapplied, or cloned
        // and then applied as an `Add` (`roster_clone`, #11218).
        Change::MissingClone { .. } => false,
    };
    if changed {
        registry
            .save(registry_path)
            .with_context(|| format!("saving {}", registry_path.display()))?;
    }
    Ok(())
}

// ============================================================================
// The fleet version floor (#10711)
// ============================================================================

/// What one pass resolved for the fleet-wide minimum Loom version
/// (`loom_min_version`, [`crate::fleet_store::floor`]).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FloorPass {
    /// The floor in force after this pass, `X.Y.Z`; `None` when unset.
    pub floor: Option<String>,
    /// The store path the floor was read from this pass (`fleet.json` /
    /// `repos.yml`); `None` when unset or carried over.
    pub source: Option<String>,
    /// Whether [`FloorPass::floor`] is the last good floor carried over
    /// because this pass could not read a valid one (see `error`).
    pub carried: bool,
    /// Why this pass could not read a valid floor, when it could not. Raised
    /// as an alert: a malformed floor is never read as "no floor".
    pub error: Option<String>,
}

impl FloorPass {
    /// Nothing to report: no floor and no error. A snapshot in this state
    /// omits the field entirely, so a host with no floor writes the same
    /// `fleet-sync-status.json` it did before #10711.
    #[must_use]
    pub fn is_unset(&self) -> bool {
        self.floor.is_none() && self.error.is_none()
    }
}

/// Decide the floor in force from this pass's `read` and the `last_good` floor.
///
/// - absent: unset (the operator removed it);
/// - valid: that floor;
/// - malformed, or the snapshot could not be read at all (`Err`): keep
///   `last_good` (unset on a first-ever bad read) and report why.
#[must_use]
pub fn resolve_floor(read: Result<FloorRead, String>, last_good: Option<&str>) -> FloorPass {
    let keep = |why: String| FloorPass {
        floor: last_good.map(str::to_string),
        source: None,
        carried: last_good.is_some(),
        error: Some(match last_good {
            Some(f) => format!("{why} — keeping the last good floor {f}"),
            None => format!("{why} — no previous floor, so none is in force"),
        }),
    };
    match read {
        Ok(FloorRead::Absent) => FloorPass::default(),
        Ok(FloorRead::Valid { version, source }) => FloorPass {
            floor: Some(version),
            source: Some(source.to_string()),
            ..FloorPass::default()
        },
        Ok(FloorRead::Malformed { source, detail }) => {
            keep(format!("{source}: malformed `{}`: {detail}", floor::KEY))
        }
        Err(e) => keep(format!("could not read `{}`: {e}", floor::KEY)),
    }
}

/// Read the floor from the cached snapshot the config half of this pass just
/// refreshed. [`Policy::Offline`]: no extra forge request — the state and
/// config halves already revalidated the cache (or fell back to it).
fn floor_half(
    cache_dir: &Path,
    location: &StoreLocation,
    last_good: Option<&str>,
    now: DateTime<Utc>,
) -> FloorPass {
    let read = fetch::load(&NoTransport, cache_dir, location, Policy::Offline, now)
        .map(|loaded| floor::read(&loaded.snapshot))
        .map_err(|e| format!("{e:#}"));
    resolve_floor(read, last_good)
}

/// The transport [`Policy::Offline`] never calls.
struct NoTransport;

impl Transport for NoTransport {
    fn get(&self, api_path: &str, _: Option<&str>, _: Option<&str>) -> Result<fetch::Reply> {
        Err(anyhow!("offline read attempted a forge request ({api_path})"))
    }
}

/// The process-wide floor: `None` until a pass has resolved one.
fn floor_cell() -> &'static Mutex<Option<Option<String>>> {
    static CELL: OnceLock<Mutex<Option<Option<String>>>> = OnceLock::new();
    CELL.get_or_init(|| Mutex::new(None))
}

#[cfg(test)]
fn set_floor(floor: Option<String>) {
    if let Ok(mut guard) = floor_cell().lock() {
        *guard = Some(floor);
    }
}

/// The fleet-wide minimum Loom version (`loom_min_version`, `X.Y.Z`) in force
/// on this host, as of the last fleet-sync pass; `None` when the store sets
/// none, when no pass has run, or when `fleet.repo` is unset. Updated every
/// tick without a restart. The self-update loop reads it every tick to drive
/// floor rolls ([`crate::auto_update::floor_roll`], #10712).
#[must_use]
pub fn loom_min_version() -> Option<String> {
    floor_cell().lock().ok().and_then(|g| g.clone().flatten())
}

/// #10719: the `repo_ahead_target` roll demand, beside the floor. The highest
/// daemon version a registered workspace NEEDS and this host does not run.
pub use crate::workspace_hold::repo_ahead_min;

/// The last good floor for this pass to fall back on: this process's value
/// once a pass has resolved one, else what the previous process recorded in
/// its snapshot — so a malformed value right after a restart still cannot
/// silently drop a floor.
fn last_good_floor() -> Option<String> {
    if let Some(resolved) = floor_cell().lock().ok().and_then(|g| g.clone()) {
        return resolved;
    }
    cached_status()
        .or_else(probe_status)
        .and_then(|s| s.floor.floor)
}

// ============================================================================
// The host-level snapshot `loom-daemon status` renders
// ============================================================================

/// What the last sync pass on this host found. Written to
/// `<loom_dir>/fleet-sync-status.json` and kept in process memory.
///
/// A host-level file rather than a field of the IPC `DaemonStatusReport`, for
/// the same reason `status` already probes `daemon_install_state` client-side:
/// this is a fact about the *host's* config tiers, and it must stay readable
/// when the daemon it describes is not answering — including the case where the
/// startup pass itself is what went wrong.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FleetSyncStatus {
    /// The store, `OWNER/REPO`.
    pub repo: String,
    /// The ref read.
    pub reference: String,
    /// The host id resolved in the store.
    pub host: String,
    /// `startup` or `timer`.
    pub pass: String,
    /// When the pass ran.
    pub at: DateTime<Utc>,
    /// Timer cadence in force.
    pub interval_secs: u64,
    /// Whether the write path is armed.
    pub auto_apply: bool,
    /// The config tiers.
    pub config: ConfigPass,
    /// The roster diff.
    pub roster: RosterPass,
    /// This host's desired run state and what was done about it (#9598).
    /// `#[serde(default)]` so a snapshot written by a pre-#9598 daemon still
    /// reads back.
    #[serde(default)]
    pub state: crate::fleet_state::StatePass,
    /// What this host is **actually** doing about that state (#9598) — the
    /// *actual* half of `status`'s desired-vs-actual pair, against
    /// [`StatePass::desired`]'s *desired* half.
    ///
    /// [`run_pass`] seeds it with what the state *requires*
    /// ([`StatePass::enforcement`]); the timer's [`enforce`] then overwrites it
    /// with what actually happened, which differs only when a `stopped` host's
    /// drain-and-exit was refused and it merely held dispatch instead.
    #[serde(default = "default_enforced")]
    pub enforced: Enforcement,
    /// The fleet version floor this pass resolved (#10711). `#[serde(default)]`
    /// so a pre-#10711 snapshot still reads back, and omitted when unset so a
    /// host with no floor writes the same snapshot it did before.
    #[serde(default, skip_serializing_if = "FloorPass::is_unset")]
    pub floor: FloorPass,
    /// Each registered workspace's installed-Loom state (#10718). Filled by
    /// the timer after each pass ([`run_pass`] carries the previous findings
    /// forward); omitted until a workspace pass has run.
    #[serde(
        default,
        skip_serializing_if = "workspace_resync::WorkspacePass::is_unset"
    )]
    pub workspaces: workspace_resync::WorkspacePass,
    /// Each registered workspace's main checkout, as the last checkout pass
    /// found it (#10869). `#[serde(default)]` so an older snapshot still reads
    /// back, and omitted until a checkout pass has run.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub checkouts: Vec<checkout_ff::CheckoutReport>,
    /// Why the last checkout step checked nothing, when it did not
    /// ([`checkout_ff::BUSY_NOTE`]); `checkouts` are then from before it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checkouts_note: Option<String>,
}

fn default_enforced() -> Enforcement {
    Enforcement::Proceed
}

impl FleetSyncStatus {
    /// Whether anything is out of sync with the store.
    #[must_use]
    pub fn drifted(&self) -> bool {
        self.config.drifted() || self.roster.drifted()
    }

    /// Whether either half of the pass failed, or the floor could not be read
    /// (#10711) — which routes a malformed floor through the same warn log and
    /// [`DRIFT_TOPIC`] alert as every other fleet-sync failure.
    #[must_use]
    pub fn errored(&self) -> bool {
        self.config.error.is_some() || self.roster.error.is_some() || self.floor.error.is_some()
    }
}

fn cell() -> &'static Mutex<Option<FleetSyncStatus>> {
    static CELL: OnceLock<Mutex<Option<FleetSyncStatus>>> = OnceLock::new();
    CELL.get_or_init(|| Mutex::new(None))
}

/// Resolve `<loom_dir>` the same way [`crate::daemon_heartbeat`] and
/// [`crate::autonomy_marker`] do: the parent of `LOOM_SOCKET_PATH` when set (so
/// a test daemon pointed at a tempdir socket never writes into the operator's
/// real `~/.loom`), else `~/.loom`.
fn resolve_loom_dir() -> Option<PathBuf> {
    if let Ok(path) = std::env::var("LOOM_SOCKET_PATH") {
        return PathBuf::from(path).parent().map(Path::to_path_buf);
    }
    dirs::home_dir().map(|h| h.join(".loom"))
}

/// Path of the host-level snapshot, or `None` when no loom dir resolves.
#[must_use]
pub fn status_path() -> Option<PathBuf> {
    resolve_loom_dir().map(|d| d.join(STATUS_FILENAME))
}

/// Record `status` in process memory and in the host-level snapshot file.
/// Best-effort on the file: a failure is logged, never propagated — the sync
/// pass itself succeeded or failed on its own terms.
pub fn publish(status: &FleetSyncStatus) {
    if let Ok(mut guard) = cell().lock() {
        *guard = Some(status.clone());
    }
    let Some(path) = status_path() else {
        return;
    };
    if let Err(e) = write_status(&path, status) {
        log::warn!("fleet_sync: could not write {}: {e:#}", path.display());
    }
}

/// Put a finished workspace pass (#10718) on the current snapshot. Before the
/// first snapshot exists there is nothing to update; the timer's next publish
/// carries it.
fn publish_workspaces(found: &workspace_resync::WorkspacePass) {
    let updated = cell().lock().ok().and_then(|mut guard| {
        let status = guard.as_mut()?;
        status.workspaces.clone_from(found);
        Some(status.clone())
    });
    let (Some(status), Some(path)) = (updated, status_path()) else {
        return;
    };
    if let Err(e) = write_status(&path, &status) {
        log::warn!("fleet_sync: could not write {}: {e:#}", path.display());
    }
}

/// Put a finished checkout step (#10869) on the current snapshot, as
/// [`publish_workspaces`] does for the workspace pass: its reports, or (with
/// `found` `None`) only the note saying why it checked nothing.
fn publish_checkouts(found: Option<&[checkout_ff::CheckoutReport]>, note: Option<&str>) {
    let updated = cell().lock().ok().and_then(|mut guard| {
        let status = guard.as_mut()?;
        if let Some(found) = found {
            status.checkouts = found.to_vec();
        }
        status.checkouts_note = note.map(str::to_string);
        Some(status.clone())
    });
    let (Some(status), Some(path)) = (updated, status_path()) else {
        return;
    };
    if let Err(e) = write_status(&path, &status) {
        log::warn!("fleet_sync: could not write {}: {e:#}", path.display());
    }
}

fn write_status(path: &Path, status: &FleetSyncStatus) -> Result<()> {
    let mut body = serde_json::to_vec_pretty(status)?;
    body.push(b'\n');
    fetch::write_atomic(path, &body)
}

/// The last pass this process recorded, if any.
#[must_use]
pub fn cached_status() -> Option<FleetSyncStatus> {
    cell().lock().ok().and_then(|g| g.clone())
}

/// The host-level snapshot on disk, `Ok(None)` when there is none (the feature
/// has never run here) and `Err` only when one exists but cannot be read.
pub fn read_status(path: &Path) -> Result<Option<FleetSyncStatus>> {
    let raw = match std::fs::read_to_string(path) {
        Ok(r) => r,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    serde_json::from_str(&raw)
        .map(Some)
        .with_context(|| format!("parsing {}", path.display()))
}

/// The host-level snapshot at the default location; `None` when absent or
/// unreadable (a client-side renderer has nothing better to do than say so).
#[must_use]
pub fn probe_status() -> Option<FleetSyncStatus> {
    read_status(&status_path()?).ok().flatten()
}

/// Remove a snapshot left behind by an earlier, configured run. Called when
/// `fleet.repo` is no longer set, so `loom-daemon status` never reports a store
/// this host has stopped reading.
///
/// Also forgets the process-wide floor (#10792): with no store there is no
/// floor, so [`loom_min_version`] returns `None` and [`last_good_floor`] finds
/// neither a cell value nor a file. The cell goes back to "no pass has run",
/// not `Some(None)`. A config *error* (`Err` from `resolve_config`) does not
/// come through here: keeping the last good floor is the safe failure.
pub fn clear_status() {
    if let Ok(mut guard) = floor_cell().lock() {
        *guard = None;
    }
    // The in-memory snapshot also carries the floor ([`last_good_floor`] reads
    // it), so it goes too; otherwise the "forgotten" floor would come back.
    if let Ok(mut guard) = cell().lock() {
        *guard = None;
    }
    let Some(path) = status_path() else {
        return;
    };
    match std::fs::remove_file(&path) {
        Ok(()) => log::info!(
            "fleet_sync: no fleet store configured — removed the stale snapshot {}",
            path.display()
        ),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => log::debug!("fleet_sync: could not remove {}: {e:#}", path.display()),
    }
}

/// The one-line summary for `loom-daemon status`, or `None` when this host has
/// no snapshot at all (the feature is off — `status` output is then unchanged).
#[must_use]
pub fn render_line(status: Option<&FleetSyncStatus>, now: DateTime<Utc>) -> Option<String> {
    let s = status?;
    let age = crate::health::format_window((now - s.at).num_seconds().max(0) as u64);
    let commit = s
        .config
        .commit
        .as_deref()
        .map_or_else(|| "unknown".to_string(), |c| c[..c.len().min(12)].to_string());
    let mut head = format!(
        "Fleet store: {} @ {} commit {commit} (host {}, {} pass {age} ago",
        s.repo, s.reference, s.host, s.pass
    );
    if s.config.cached {
        head.push_str(", CACHED snapshot");
    }
    if s.auto_apply {
        head.push_str(", autoApply on");
    }
    head.push(')');
    let mut lines = vec![head];
    lines.extend(state_lines(s));
    lines.extend(floor_lines(&s.floor));
    let interval = std::time::Duration::from_secs(s.interval_secs);
    lines.extend(s.workspaces.lines_at(Some((now, interval))));
    lines.extend(checkout_ff::lines(&s.checkouts));
    lines.extend(
        s.checkouts_note
            .iter()
            .map(|note| format!("  checkouts: {note}")),
    );
    for tier in &s.config.tiers {
        let detail = tier.detail.as_deref().unwrap_or("in sync");
        let verb = if tier.wrote {
            "rendered"
        } else if tier.drifted {
            "DRIFT"
        } else {
            "in sync"
        };
        lines.push(format!("  {}: {verb} — {} ({detail})", tier.tier, tier.path.display()));
    }
    if let Some(e) = &s.config.error {
        lines.push(format!("  config: ERROR — {e}"));
    }
    if let Some(why) = &s.roster.skipped {
        lines.push(format!("  roster: not checked — {why}"));
    } else if s.roster.drift.is_empty() && s.roster.error.is_none() {
        lines.push("  roster: in sync".to_string());
    }
    for d in &s.roster.drift {
        lines.push(format!("  roster: DRIFT — {d}"));
    }
    lines.extend(roster_clone::lines(&s.roster.clones));
    if s.roster.applied > 0 || s.roster.unapplied > 0 {
        lines.push(format!(
            "  roster: {} applied, {} unapplied",
            s.roster.applied, s.roster.unapplied
        ));
    }
    if let Some(e) = &s.roster.error {
        lines.push(format!("  roster: ERROR (fail-closed) — {e}"));
    }
    if !s.drifted() && !s.errored() {
        lines.push("  host matches the store".to_string());
    } else if !s.auto_apply && s.drifted() {
        lines.push(
            "  nothing was written — set fleet.autoApply=true (or run `loom-daemon fleet-config \
             render` / `roster --apply`) to converge"
                .to_string(),
        );
    }
    Some(lines.join("\n"))
}

/// The floor lines of the `Fleet store:` block (#10711). Empty when no floor
/// is set and none was malformed, so `status` is unchanged for such a host.
fn floor_lines(f: &FloorPass) -> Vec<String> {
    let mut lines = Vec::new();
    if let Some(floor) = &f.floor {
        let from = match (&f.source, f.carried) {
            (_, true) => " (LAST GOOD floor, carried over)".to_string(),
            (Some(src), false) => format!(" (from {src})"),
            (None, false) => String::new(),
        };
        lines.push(format!("  {}: {floor}{from}", floor::KEY));
    }
    if let Some(e) = &f.error {
        lines.push(format!("  {}: ERROR — {e}", floor::KEY));
    }
    lines
}

/// The run-state lines of the `Fleet store:` block (#9598) — the *desired*
/// state the store named, and the *actual* enforcement this host applied.
///
/// Empty when the pass resolved no state at all (a store with no
/// `fleet/state.yml`, a first boot behind an unreachable forge), except that a
/// read *error* is always reported: "the state could not be read" is the one
/// thing an operator must not have to infer from silence.
fn state_lines(s: &FleetSyncStatus) -> Vec<String> {
    let mut lines = Vec::new();
    if let Some(desired) = s.state.desired {
        let from = match s.state.source.as_deref() {
            Some("host") => "host entry",
            Some(_) => "fleet default",
            None => "unknown entry",
        };
        let how = if s.state.from_last_recorded {
            ", from this host's LAST RECORDED state — no snapshot was readable"
        } else if s.state.cached {
            ", from a CACHED snapshot"
        } else {
            ""
        };
        let actual = match s.enforced {
            Enforcement::Proceed => "dispatching normally",
            Enforcement::Hold => "new dispatch HELD, in-flight work finishes",
            Enforcement::Stop => "refusing to start / draining to exit",
        };
        lines.push(format!("  run state: desired {} ({from}{how}) -> {actual}", desired.as_str()));
        for (k, v) in [
            ("by", &s.state.by),
            ("since", &s.state.since),
            ("reason", &s.state.reason),
        ] {
            if let Some(v) = v {
                lines.push(format!("    {k}: {v}"));
            }
        }
    }
    if let Some(e) = &s.state.error {
        lines.push(format!("  run state: NOT ENFORCED — {e}"));
    }
    lines
}

// ============================================================================
// The daemon's startup pass and timer loop
// ============================================================================

/// Everything one pass needs, cheap to clone into the timer task.
#[derive(Debug, Clone)]
struct PassInputs {
    workspace: PathBuf,
    location: StoreLocation,
    host: String,
    cache: PathBuf,
    interval: Duration,
    auto_apply: bool,
    clones: roster_clone::CloneConfig,
}

/// Run one complete pass (config tiers + roster) against the real forge. Pure
/// blocking work — shells out to `gh` — so callers run it on a blocking
/// thread. Never panics and never returns `Err`: every failure lands in the
/// returned snapshot.
fn run_pass(inputs: &PassInputs, pass: &str, mode: Mode, now: DateTime<Utc>) -> FleetSyncStatus {
    let transport =
        crate::fleet_store::gh::GhTransport::new(&inputs.workspace, &inputs.location.repo);
    // #9598: the desired run state, read first so the config half below cannot
    // make a `stopped` host look like it converged before it was told to stop.
    // The fallback is this host's *last recorded* state (see `state_pass`), so a
    // forge outage with a wiped cache still cannot resume a stopped host.
    let state = fleet_state::state_pass(
        &transport,
        &inputs.cache,
        &inputs.location,
        &inputs.host,
        last_recorded_state(),
        now,
    );
    let enforced = state.enforcement();
    let local_path = inputs
        .workspace
        .join(crate::config_resolver::LOCAL_CONFIG_REL);
    let config = match crate::config_resolver::private_defaults_path() {
        Some(machine_path) => config_pass(
            &transport,
            &inputs.cache,
            &inputs.location,
            &inputs.host,
            &machine_path,
            &local_path,
            mode,
            now,
        ),
        None => ConfigPass {
            error: Some(format!(
                "the machine tier is disabled ({} is set to the empty string)",
                crate::config_resolver::PRIVATE_DEFAULTS_ENV
            )),
            ..ConfigPass::default()
        },
    };
    // #11218: only a timer pass on a host free to dispatch clones.
    let clone = pass == "timer" && enforced == Enforcement::Proceed;
    let roster = roster_half(inputs, &transport, &config, (mode, clone), now);
    // #10711: the floor, from the snapshot the halves above just refreshed,
    // into the process-wide value — never the config tiers, whose
    // `autonomous.autoUpdate` changes need a restart.
    let floor = floor_half(&inputs.cache, &inputs.location, last_good_floor().as_deref(), now);
    // #10885: a floor that differs from the previous pass's wakes the
    // self-update loop, so it acts now and not at its next interval.
    floor_knowledge::record_floor(&floor);
    FleetSyncStatus {
        repo: inputs.location.repo.clone(),
        reference: inputs.location.reference.clone(),
        host: inputs.host.clone(),
        pass: pass.to_string(),
        at: now,
        interval_secs: inputs.interval.as_secs(),
        auto_apply: inputs.auto_apply,
        config,
        roster,
        state,
        enforced,
        floor,
        // The last workspace pass's findings, until this tick's replace them.
        workspaces: cached_status().map(|s| s.workspaces).unwrap_or_default(),
        // Likewise the last checkout pass's (#10869).
        checkouts: cached_status().map(|s| s.checkouts).unwrap_or_default(),
        checkouts_note: cached_status().and_then(|s| s.checkouts_note),
    }
}

/// This host's last recorded desired run state (#9598): from the pass held in
/// process memory, else from the snapshot the previous *process* left on disk —
/// which is the one the startup pass reads. `None` on a host that has never
/// successfully resolved a state.
fn last_recorded_state() -> Option<crate::fleet_store::state::RunState> {
    cached_status()
        .or_else(probe_status)
        .and_then(|s| s.state.desired)
}

/// The roster half of [`run_pass`]. Skipped outright when the config half was
/// served from the cache: the roster is fail-closed with no cached fallback, so
/// a forge that could not confirm the config commit cannot confirm the roster
/// either, and a second failing fetch would only turn a known-unreachable forge
/// into a misleading roster *error*.
fn roster_half(
    inputs: &PassInputs,
    transport: &dyn Transport,
    config: &ConfigPass,
    (mode, clone): (Mode, bool),
    now: DateTime<Utc>,
) -> RosterPass {
    if config.cached {
        return RosterPass {
            skipped: Some(
                "the forge was unreachable this pass and the roster is fail-closed (no cached \
                 fallback)"
                    .to_string(),
            ),
            ..RosterPass::default()
        };
    }
    let registry_path = match crate::workspace_registry::default_registry_path() {
        Ok(p) => p,
        Err(e) => {
            return RosterPass {
                error: Some(format!("{e:#}")),
                ..RosterPass::default()
            }
        }
    };
    let registry = match crate::workspace_registry::WorkspaceRegistry::load(&registry_path) {
        Ok(r) => r,
        Err(e) => {
            return RosterPass {
                error: Some(format!("reading {}: {e:#}", registry_path.display())),
                ..RosterPass::default()
            }
        }
    };
    let registered: Vec<Registered> = registry
        .workspaces
        .iter()
        .map(|w| Registered {
            root: w.root.clone(),
            priority: w.priority,
            maintain_only: w.maintain_only.is_some(),
        })
        .collect();
    let Some(home) = dirs::home_dir() else {
        return RosterPass {
            error: Some("no home directory".to_string()),
            ..RosterPass::default()
        };
    };
    let plan = roster_pass(
        transport,
        &inputs.cache,
        &inputs.location,
        &home,
        &registered,
        &crate::workspace_registry::normalize_path,
        &|p: &Path| p.join(".git").exists(),
        now,
    );
    let clones = if clone {
        roster_clone::Clones::on(&roster_clone::GitCloner, inputs.clones)
    } else {
        roster_clone::Clones::off()
    };
    let write = mode == Mode::Write && inputs.auto_apply;
    match plan {
        Ok(plan) => {
            roster_clone::summarize(&plan, write, &clones, &mut |c| apply_change(&registry_path, c))
        }
        Err(e) => RosterPass {
            error: Some(format!("{e:#}")),
            ..RosterPass::default()
        },
    }
}

/// Log one pass at the level its findings deserve, and publish a drift record
/// on the event bus when there is something to say.
fn report(status: &FleetSyncStatus, bus: Option<&crate::event_bus::EventBus>) {
    let summary = render_line(Some(status), status.at).unwrap_or_default();
    if status.errored() {
        log::warn!("fleet_sync: {summary}");
    } else if status.drifted() {
        log::info!("fleet_sync: {summary}");
    } else {
        log::debug!("fleet_sync: {summary}");
    }
    roster_clone::announce(status, bus);
    let (Some(bus), true) = (bus, status.drifted() || status.errored()) else {
        return;
    };
    let payload = serde_json::to_value(status).unwrap_or(Value::Null);
    let _ = bus.publish_generic(DRIFT_TOPIC, payload);
}

/// What [`start`] left for the caller: the `paused` hold this boot must begin
/// under, plus everything [`Started::spawn_timer`] needs to arm the
/// drift/enforcement timer.
///
/// A `stopped` state is **already handled** by the time this exists — `start`
/// exits the process. What cannot be handled there is `paused`: enforcing it
/// needs [`crate::ipc::DrainState`], which does not exist yet at the point in
/// boot where the startup config render has to happen. So the hold travels to
/// the caller as a note, and so does the timer, which enforces later changes
/// through the same drain state. [`crate::fleet_state::wire`] is the one
/// production caller that does both.
pub struct Started {
    /// The drain note for a `paused` host, or `None` for every other state —
    /// what [`crate::ipc::DrainState::with_fleet_hold`] takes.
    pub hold_note: Option<String>,
    /// The state read this boot, as persisted for `loom-daemon status`.
    pub state: StatePass,
    /// This host's id in the store.
    pub host: String,
    /// The store, `OWNER/REPO`.
    pub repo: String,
    inputs: PassInputs,
    bus: Option<std::sync::Arc<crate::event_bus::EventBus>>,
}

impl Started {
    /// The daemon workspace this host syncs from — the fallback sweep root an
    /// [`crate::fleet_state::IpcEnforcer`]'s drain request needs.
    #[must_use]
    pub fn workspace(&self) -> &Path {
        &self.inputs.workspace
    }

    /// Arm the `fleet.syncIntervalSecs` timer. `enforcer` acts on a *change* of
    /// desired run state; `None` keeps the pre-#9598 report-only timer, which is
    /// what a caller with no drain state (a test, a future read-only consumer)
    /// wants.
    #[must_use]
    pub fn spawn_timer(
        self,
        enforcer: Option<std::sync::Arc<dyn Enforcer>>,
    ) -> tokio::task::JoinHandle<()> {
        spawn_timer(self.inputs, self.bus, enforcer)
    }
}

/// Start fleet-store syncing for the daemon workspace at `workspace`.
///
/// Returns `None` — having done nothing at all — when `fleet.repo` is unset,
/// which is every host that has not opted in. Otherwise runs the **startup
/// pass** to completion (bounded by [`resolve_startup_timeout`]) before
/// returning, so the caller can spawn its config-dependent loops against the
/// config the store just rendered, and returns the `paused` hold plus the means
/// to arm the timer ([`Started`]).
///
/// # This call does not return on a `stopped` host (#9598)
///
/// The desired run state is resolved by the same startup pass, and a `stopped`
/// state is acted on **here** — the earliest point at which it is known, and
/// before any dispatch producer, IPC listener or role loop exists to drain.
/// [`crate::fleet_state::enforce_at_boot`] prints the refusal and exits
/// [`crate::fleet_state::EXIT_FLEET_STOPPED`]. That is deliberate: deferring
/// the decision to the caller means every caller has to remember to make it.
pub async fn start(
    workspace: &Path,
    bus: Option<std::sync::Arc<crate::event_bus::EventBus>>,
) -> Option<Started> {
    let effective = crate::config_resolver::resolve_effective_config(workspace);
    let config = match resolve_config(&effective, &|k| std::env::var(k).ok()) {
        Ok(Some(c)) => c,
        Ok(None) => {
            log::debug!(
                "fleet_sync: disabled (no {} / {} configured for {})",
                store::FLEET_REPO_KEY,
                store::FLEET_REPO_ENV,
                workspace.display()
            );
            clear_status();
            // #11029: "unset" is only trustworthy when every config tier was
            // readable. A tier that failed to read may be the one carrying
            // `fleet.repo`; running as `NoStore` would chase the latest
            // release. Treat that as a fleet host with an unknown floor.
            let dead = crate::config_resolver::unreadable_tiers(workspace);
            if let Some((path, why)) = dead.first() {
                let why = format!(
                    "config tier {} is unreadable ({why}), so whether this host reads a fleet \
                     store is not known",
                    path.display()
                );
                log::warn!("fleet_sync: {why} — treating the fleet floor as unknown");
                floor_knowledge::set_unreadable_config(why);
                return None;
            }
            // #10885: not a fleet host, so it has no floor by definition.
            floor_knowledge::set_store_mode(floor_knowledge::StoreMode::Absent);
            return None;
        }
        Err(e) => {
            // #10885: a store is named but unusable. Still a fleet host: its
            // floor is unknown, never absent.
            floor_knowledge::set_store_mode(floor_knowledge::StoreMode::Configured);
            log::warn!("fleet_sync: disabled — {e:#}");
            return None;
        }
    };
    // #10885: from here on this is a fleet host, whether or not syncing can
    // start. Until a pass resolves a floor it is unknown, never absent.
    floor_knowledge::set_store_mode(floor_knowledge::StoreMode::Configured);
    let host = crate::sweep_registry::host_identity();
    if let Err(e) = store::validate_host(&host) {
        log::warn!("fleet_sync: disabled — {e:#}");
        return None;
    }
    let cache = match store::default_cache_dir(&config.location) {
        Ok(c) => c,
        Err(e) => {
            log::warn!("fleet_sync: disabled — no store cache directory: {e:#}");
            return None;
        }
    };
    // A repo that delegates daemon administration elsewhere must not have its
    // workspace registry rewritten from here either — the same refusal
    // `loom-daemon workspace add/remove/set-priority` makes (#5345), applied to
    // the unattended path.
    let auto_apply = match crate::config_resolver::daemon_delegated_to(workspace) {
        Some(delegate) if config.auto_apply => {
            log::warn!(
                "fleet_sync: {AUTO_APPLY_KEY} is on but daemon admin is delegated to {delegate} — \
                 syncing read-only from here"
            );
            false
        }
        _ => config.auto_apply,
    };
    let inputs = PassInputs {
        workspace: workspace.to_path_buf(),
        location: config.location,
        host,
        cache,
        interval: config.interval,
        auto_apply,
        clones: roster_clone::resolve_config(&effective, &|k| std::env::var(k).ok()),
    };
    log::info!(
        "fleet_sync: enabled — store {} @ {}, host {}, every {}s, autoApply={auto_apply}",
        inputs.location.repo,
        inputs.location.reference,
        inputs.host,
        inputs.interval.as_secs()
    );
    workspace_resync::mark_boot();
    let state = startup_pass(&inputs, bus.as_deref()).await;
    // Diverges on `stopped`: this host is not meant to be up at all.
    let hold_note = fleet_state::enforce_at_boot(&state, &inputs.host, &inputs.location.repo).await;
    Some(Started {
        hold_note,
        state,
        host: inputs.host.clone(),
        repo: inputs.location.repo.clone(),
        inputs,
        bus,
    })
}

/// The startup pass: render (write) the config tiers before any
/// config-dependent loop is spawned, under a wall-clock cap so an unreachable
/// (or, worse, silently hanging) forge cannot hold up boot.
///
/// Returns the desired run state it read (#9598). A pass that panicked or blew
/// its cap returns an empty [`StatePass`], whose enforcement is `Proceed`: a
/// timed-out fetch is not evidence of a `stopped` host, and refusing to boot on
/// it would let one slow forge take a fleet down. The **timer** re-reads within
/// `fleet.syncIntervalSecs` and enforces then.
async fn startup_pass(inputs: &PassInputs, bus: Option<&crate::event_bus::EventBus>) -> StatePass {
    let owned = inputs.clone();
    let join = tokio::task::spawn_blocking(move || {
        let mut status = run_pass(&owned, "startup", Mode::Write, Utc::now());
        // #10869: inside the startup pass, so a daemon that just rolled
        // dispatches from the checkout its new installed files are in.
        let moved = checkout_ff::startup(&owned, &mut status);
        (status, moved)
    });
    let capped = match resolve_startup_timeout(&|k| std::env::var(k).ok()) {
        Some(cap) => tokio::time::timeout(cap, join).await.map_err(|_| cap),
        None => Ok(join.await),
    };
    match capped {
        Ok(Ok((status, moved))) => {
            workspace_resync::mark_verified();
            publish(&status);
            report(&status, bus);
            checkout_ff::announce(&moved, bus);
            status.state
        }
        Ok(Err(e)) => {
            log::warn!("fleet_sync: the startup pass panicked: {e}");
            StatePass::default()
        }
        Err(cap) => {
            log::warn!(
                "fleet_sync: the startup pass did not finish within {}s — continuing boot with \
                 the config already on disk, and with the desired run state NOT enforced this \
                 boot (the timer re-reads it within {}s). Set {STARTUP_TIMEOUT_ENV} to retune, 0 \
                 to wait indefinitely. The pass is still running and will finish in the \
                 background; its render takes effect on the next start.",
                cap.as_secs(),
                inputs.interval.as_secs(),
            );
            StatePass::default()
        }
    }
}

/// The timer loop: one pass per `fleet.syncIntervalSecs`. Checks only, unless
/// `fleet.autoApply` armed the write path.
///
/// Run-state enforcement (#9598) is **not** gated on `fleet.autoApply`: that
/// flag guards writes to this host's config files and workspace registry, a
/// different and more invasive act than honouring the run state an operator
/// committed. A host that reads the store at all obeys `paused` / `stopped`.
fn spawn_timer(
    inputs: PassInputs,
    bus: Option<std::sync::Arc<crate::event_bus::EventBus>>,
    enforcer: Option<std::sync::Arc<dyn Enforcer>>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mode = if inputs.auto_apply {
            Mode::Write
        } else {
            Mode::Check
        };
        // #10718: the workspace resync also runs once at startup, now that the
        // startup pass is done and the drain state exists. It runs on its own
        // task (one at a time), so nothing here ever waits for git.
        //
        // #10869: the checkout half runs on that same task, right after the
        // resync pass, so a resync this host just pushed is fast-forwarded to
        // in the same pass and the two never run at once.
        let then = || checkout_ff::after_resync(&inputs, &enforcer, &bus);
        let (step, after) = then();
        workspace_resync::spawn_pass(&inputs, mode, &enforcer, &bus, step, after);
        loop {
            tokio::time::sleep(inputs.interval).await;
            let owned = inputs.clone();
            match tokio::task::spawn_blocking(move || run_pass(&owned, "timer", mode, Utc::now()))
                .await
            {
                Ok(mut status) => {
                    // Enforce BEFORE publishing: `enforce` corrects `enforced`
                    // to what this host actually ended up doing, and the
                    // snapshot `loom-daemon status` reads must carry that, not
                    // the pre-enforcement requirement.
                    if let Some(e) = enforcer.as_deref() {
                        enforce(&mut status, e, bus.as_deref());
                    }
                    status.workspaces = workspace_resync::latest();
                    publish(&status);
                    report(&status, bus.as_deref());
                    // #10718: last, so the floor is current and a hold this
                    // pass just placed is already in the drain flag. Not
                    // awaited: see `workspace_resync::host`.
                    workspace_resync::mark_verified();
                    let (step, after) = then();
                    workspace_resync::spawn_pass(&inputs, mode, &enforcer, &bus, step, after);
                }
                Err(e) => log::warn!("fleet_sync: a timer pass panicked: {e}"),
            }
        }
    })
}

/// Apply one timer pass's run-state decision (#9598), and publish the
/// transition when there was one.
///
/// Only *transitions* are acted on and logged: the steady state of a paused host
/// is one silent `None` action per tick, so enforcement adds no log volume and
/// no repeated drain requests.
fn enforce(
    status: &mut FleetSyncStatus,
    enforcer: &dyn Enforcer,
    bus: Option<&crate::event_bus::EventBus>,
) {
    let required = status.enforced;
    let action = fleet_state::timer_action(required, enforcer.is_held());
    let applied =
        fleet_state::apply(action, required, enforcer, &status.state, &status.host, &status.repo);
    // `enforced` is the *actual* half of the desired-vs-actual pair, so correct
    // it when the host could not do what the store asked (a refused
    // drain-and-exit degrades `stop` to `hold`). `status` must not report a
    // stop that did not happen.
    status.enforced = applied.effective;
    let Some(line) = applied.log else {
        return;
    };
    log::warn!("fleet_sync: {line}");
    if let Some(bus) = bus {
        let _ = bus.publish_generic(
            STATE_TOPIC,
            serde_json::json!({
                "host": status.host,
                "repo": status.repo,
                "desired": status.state.desired,
                "enforced": status.enforced.as_str(),
                "detail": line,
            }),
        );
    }
}

#[cfg(test)]
#[path = "fleet_sync/tests.rs"]
mod tests;
