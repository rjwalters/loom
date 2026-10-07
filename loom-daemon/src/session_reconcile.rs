//! Periodic Codex session-container reconcile pass (issue #10453, Epic
//! #10452 Phase 1): a dead session container is restarted, passed over, or
//! reported, without an operator.
//!
//! Before this loop nothing in the daemon called `accounts session start`, so
//! a stopped or removed `loom-codex-session-<acct>` container stayed down
//! until someone recreated it by hand (~21.5 h on one worker on 2026-10-05).
//!
//! # Per pass
//!
//! For every **enabled** Codex account whose profile is session-managed
//! ([`is_session_managed`]) — deduplicated by account name across the
//! registered roots, since container names are host-global:
//!
//! | Container | Action |
//! |-----------|--------|
//! | held (operator `stop`) | nothing — skipped before any `docker` call |
//! | running, mounts match the registry | nothing |
//! | running, mount drift, idle | stop + rm, then recreate ([`recreate_container`]) with the current registry roots (#10364; see [`drift`]) |
//! | running, mount drift, in-flight exec | defer and re-check next pass — never killed |
//! | running, mounts a positively denied path, idle, and no start would be accepted | stop + rm, not recreated, recorded on disk; never repeated (fails closed) |
//! | running, drift that cannot be decided (registry unreadable or empty under its workspace, a root's directory missing, roster unreadable) | nothing — missing information is never a reason to stop a container |
//! | missing, and a recorded denial removal still stands | nothing until the denial is over or an operator starts it |
//! | restarting (1st pass) | nothing — Docker's `unless-stopped` policy is already retrying |
//! | restarting (2nd+ consecutive pass) | WARN once as a **crash loop**; never reused, never stopped |
//! | stopped, host-mounted | resume via [`SessionLifecycle::start_with_workspace`] (the `accounts session start` path) |
//! | missing, host-mounted | recreate ([`recreate_container`]) with the workspace/image of the last operator `start` ([`session_hold::LAST_START_FILE`]), else the label last seen on it, else (logged as a guess) [`default_mount_workspace`]; never `/` |
//! | stopped/missing, private-clone | skip and WARN once — never recreated host-mounted |
//!
//! After a resume/recreate the pass calls [`refresh_session_health_uncached`]
//! for the restarted accounts (bypassing the probe cache), so a dead
//! `auth.json` chain lands in `ReauthRequired` instead of being dispatched
//! into.
//!
//! # Guard rails
//!
//! * **Never interrupts work.** The `docker` mutations are `docker start` of
//!   a *stopped* container, `docker run` of a *missing* one, and, for mount
//!   drift only, a graceful `docker stop` + `rm` + `run` of a *running*
//!   container that `docker top` shows idle and whose dispatch lock no
//!   dispatch holds ([`drift`], [`session_dispatch_lock`]). A container with
//!   an in-flight `docker exec` (the #5119 contract), or a dispatch that is
//!   only starting, is never stopped. A restarting container is not touched either: `stop` without
//!   `--force` cannot judge one (its `docker top` in-flight check fails while
//!   Docker is between restarts).
//! * **Operator hold:** `loom-daemon accounts session stop <name>` writes
//!   [`session_hold::HOLD_FILE`] in the profile *before* `docker stop`, and
//!   only an operator `session start` (or `shell`) lifts it. A held account
//!   is skipped before any `docker` call (outcome `held (operator stop)`),
//!   and re-checked after the inspect and inside the start itself
//!   ([`SessionLifecycle::start_unless_held`]), so a pass racing a `stop`
//!   never `docker start`s the container `stop` is about to `rm`. The hold
//!   is on disk (survives daemon restarts) and per account: holds and
//!   `enabled=false` are collected across **every** registered root first
//!   ([`AccountIndex`]), so a root that still lists the account enabled
//!   cannot bypass them.
//! * **Docker unavailable:** a failed (or timed-out) container read means
//!   the runtime is unusable, not that an account is broken: the rest of the
//!   pass is skipped and the **pass as a whole** backs off
//!   ([`Outcome::DockerUnavailable`], then [`Outcome::BackingOff`] for every
//!   account until the retry time). No start is attempted and no
//!   per-account failure is counted. Holds are still read (from disk).
//! * **Backoff:** a failed start backs that account off exponentially
//!   ([`BACKOFF_BASE_SECS`] doubling to [`BACKOFF_MAX_SECS`]); the failure is
//!   WARNed once per distinct error, then logged at DEBUG. A start that
//!   "succeeds" but whose container is not running at the next pass
//!   (stopped, gone or `Restarting`) counts as a failed start, so a
//!   container that dies right after each start is retried on the backoff
//!   schedule, not every interval. The failure count resets only once the
//!   container is seen running. Every `docker` call is time-bounded
//!   ([`crate::tokens_pool::docker_cli`]). Any timed-out call — a read, a
//!   `docker start` or a `docker run` — counts as "Docker unavailable" above
//!   and ends the pass, so a wedged engine costs one budget per pass, not
//!   one per account.
//! * **No-op:** with no enabled session-managed account the pass makes zero
//!   `docker` calls, like [`refresh_session_health`].
//! * **Opt-out:** `LOOM_SESSION_RECONCILE=0` or
//!   `autonomous.sessionReconcile.enabled=false` (precedence env > config >
//!   default **on**); cadence `LOOM_SESSION_RECONCILE_INTERVAL_SECS` /
//!   `autonomous.sessionReconcile.intervalSecs` (default
//!   [`DEFAULT_SESSION_RECONCILE_INTERVAL_SECS`]). The pass runs once at
//!   daemon start, then every interval.
//!
//! # One read per pass
//!
//! The pass reads every session container once, through one bounded
//! [`session_state::snapshot`] (`docker ps -a` plus a single `docker
//! inspect`), taken lazily: a pass in which every account is held or backing
//! off makes no `docker` call. It is a **fresh** snapshot, not the watch
//! loop's cached [`session_state::latest`]: that one can predate this
//! reconciler's own last start (and so report a container it just started as
//! still down), and the pass acts on what it reads. [`Snapshot::Unavailable`]
//! is "Docker unavailable" above. The only other reads are the deliberate
//! pre-action re-checks: the start's own `docker inspect`
//! ([`SessionLifecycle::start_unless_held`]) and, before a drift recreate,
//! a fresh inspect (same container id) plus the `docker top` in-flight check.
//!
//! # Mount drift (#10364 Part B)
//!
//! A running host-mode container whose workspace mounts differ from what the
//! **current** registry says ([`session_state::mount_drift`]) is recreated
//! when idle; see [`drift`] for its safety rules (no action on missing
//! information, one acceptance check shared with `create`, a removal never
//! repeated, the dispatch lock), the ordering (`extra` first, across roots)
//! and the busy deferral. Private-clone containers are never
//! drift-recreated.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::tokens_pool::account_registry::{
    account_inventory_quiet, AccountDescriptor, AccountProvider,
};
use crate::tokens_pool::docker_cli::DockerTimedOut;
use crate::tokens_pool::private_workspace;
use crate::tokens_pool::session_hold::{self, LastStart, OperatorHeld};
use crate::tokens_pool::session_lifecycle::{
    container_name, is_session_managed, refresh_session_health_uncached, ContainerRunner,
    ContainerState, ProcessContainerRunner, SessionLifecycle, SessionStatus, SESSION_POSTURE,
    SESSION_POSTURE_LABEL,
};
use crate::tokens_pool::session_state::{self, MountDrift, Snapshot};
use crate::tokens_pool::{session_dispatch_lock, session_mount_gate};
use crate::workspace_registry::WorkspaceRegistry;
use serde_json::Value;

#[path = "session_reconcile_drift.rs"]
pub mod drift;
pub use drift::DeferReason;

/// Master on/off override (`0`/`false`/`no`/`off` disables).
pub const SESSION_RECONCILE_ENABLE_ENV: &str = "LOOM_SESSION_RECONCILE";
/// Cadence override in seconds.
pub const SESSION_RECONCILE_INTERVAL_ENV: &str = "LOOM_SESSION_RECONCILE_INTERVAL_SECS";
/// Default cadence: one `docker inspect` per session-managed account a minute.
pub const DEFAULT_SESSION_RECONCILE_INTERVAL_SECS: u64 = 60;
/// First retry delay after a failed inspect/start (two default intervals, so
/// a failing `docker run` is never re-run on the very next tick).
pub const BACKOFF_BASE_SECS: u64 = 120;
/// Cap on the exponential backoff.
pub const BACKOFF_MAX_SECS: u64 = 1800;
/// Consecutive passes a container must be seen `Restarting` before it is
/// reported as a crash loop (the #10455 signal).
pub const CRASH_LOOP_PASSES: u32 = 2;

// ============================================================================
// Config (.loom/config.json -> autonomous.sessionReconcile)
// ============================================================================

/// `autonomous.sessionReconcile`; each field falls through to env / default
/// when absent.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionReconcileConfig {
    pub enabled: Option<bool>,
    pub interval_secs: Option<u64>,
}

/// Read `autonomous.sessionReconcile`, soft-failing to defaults on a missing
/// file, malformed JSON or a missing block.
#[must_use]
pub fn read_config(repo_root: &Path) -> SessionReconcileConfig {
    let effective = crate::config_resolver::resolve_effective_config(repo_root);
    let Some(block) = crate::config_resolver::get_path(&effective, "autonomous.sessionReconcile")
    else {
        return SessionReconcileConfig::default();
    };
    SessionReconcileConfig {
        enabled: block.get("enabled").and_then(serde_json::Value::as_bool),
        interval_secs: block
            .get("intervalSecs")
            .and_then(serde_json::Value::as_u64)
            .filter(|&s| s > 0),
    }
}

/// env > config > default(**true**). A set env var decides outright: truthy
/// enables, anything else (`0`) disables.
#[must_use]
pub fn resolve_enabled(config: &SessionReconcileConfig) -> bool {
    resolve_enabled_from(std::env::var(SESSION_RECONCILE_ENABLE_ENV).ok().as_deref(), config)
}

fn resolve_enabled_from(env: Option<&str>, config: &SessionReconcileConfig) -> bool {
    match env {
        Some(v) => matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on"),
        None => config.enabled.unwrap_or(true),
    }
}

/// env > config > default; a zero/unparseable env value falls through.
#[must_use]
pub fn resolve_interval(config: &SessionReconcileConfig) -> Duration {
    let secs = std::env::var(SESSION_RECONCILE_INTERVAL_ENV)
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|&s| s > 0)
        .or(config.interval_secs)
        .unwrap_or(DEFAULT_SESSION_RECONCILE_INTERVAL_SECS);
    Duration::from_secs(secs)
}

// ============================================================================
// Decision
// ============================================================================

/// What one pass does about one account's container.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// Running: nothing to do.
    Healthy,
    /// First consecutive `Restarting` observation: Docker is retrying it.
    Restarting,
    /// `Restarting` across [`CRASH_LOOP_PASSES`] consecutive passes.
    CrashLoop,
    /// Stopped but present, host-mounted: `docker start` it against the
    /// workspace it was created with (`None`: a pre-#7389 unlabelled one).
    Resume { workspace: Option<PathBuf> },
    /// Missing, host-mounted: `docker run` it against `workspace`.
    Recreate { workspace: PathBuf },
    /// Stopped or missing, private-clone: never recreated host-mounted.
    PrivateClone,
}

/// Whether `state` is a private-clone container (its workspace label is the
/// private clone's in-container path).
fn is_private_container(state: &ContainerState) -> bool {
    state.workspace.as_deref() == Some(Path::new(private_workspace::REPO))
}

/// The pure decision. `private_clone` says the account is configured for
/// private-clone mode (or that could not be ruled out); `restarting_streak`
/// counts the consecutive *prior* passes that saw it restarting;
/// `recreate_workspace` is what a missing host-mounted container is
/// recreated against.
#[must_use]
pub fn decide(
    state: Option<&ContainerState>,
    private_clone: bool,
    restarting_streak: u32,
    recreate_workspace: &Path,
) -> Decision {
    match state {
        Some(s) if s.restarting => {
            if restarting_streak + 1 >= CRASH_LOOP_PASSES {
                Decision::CrashLoop
            } else {
                Decision::Restarting
            }
        }
        Some(s) if s.running => Decision::Healthy,
        _ if private_clone || state.is_some_and(is_private_container) => Decision::PrivateClone,
        Some(s) => Decision::Resume {
            workspace: s.workspace.clone(),
        },
        None => Decision::Recreate {
            workspace: recreate_workspace.to_path_buf(),
        },
    }
}

/// The `--mount-workspace` a missing host-mounted container is recreated
/// against when no operator start is recorded for it and the pass never saw
/// its label — a guess, logged as one. The deepest common ancestor of
/// the registered roots (`~/GitHub` for `~/GitHub/{a,b}` — narrowed back to
/// exactly those roots by `workspace_mount_roots`), a lone registered root
/// itself, or `daemon_root` when nothing is registered. Roots that share no
/// deeper parent yield `/`, which [`recreate_container`] refuses.
#[must_use]
pub fn default_mount_workspace(registered: &[PathBuf], daemon_root: &Path) -> PathBuf {
    let Some((first, rest)) = registered.split_first() else {
        return daemon_root.to_path_buf();
    };
    let mut common = first.clone();
    for root in rest {
        while !root.starts_with(&common) {
            if !common.pop() {
                return daemon_root.to_path_buf();
            }
        }
    }
    common
}

/// Exponential backoff after `failures` consecutive failures (>= 1).
#[must_use]
pub fn backoff_secs(failures: u32) -> u64 {
    let shift = failures.saturating_sub(1).min(16);
    BACKOFF_BASE_SECS
        .saturating_mul(1u64 << shift)
        .min(BACKOFF_MAX_SECS)
}

// ============================================================================
// Pass
// ============================================================================

/// What the pass did for one account.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// An operator `stop` holds it down; no `docker` call was made.
    Held,
    Running,
    Restarting,
    CrashLoop,
    Resumed,
    Recreated {
        workspace: PathBuf,
    },
    PrivateCloneSkipped,
    /// Running with mount drift and idle: removed and recreated against
    /// `workspace` with the current registry roots (#10364).
    DriftRecreated {
        workspace: PathBuf,
        drift: MountDrift,
    },
    /// Running with mount drift but not recreated this pass; re-checked on
    /// the next one.
    DriftDeferred {
        reason: DeferReason,
    },
    /// Running with mount drift a recreate cannot fix (a fresh container
    /// still drifted, or it only lacks mounts and the intended set is
    /// refused): left as is, WARNed once, and not retried until the drift
    /// itself changes.
    DriftUnachievable,
    /// Running, idle, with `extra` drift (it mounts something it must not)
    /// and no container `session start` would allow in its place: stopped
    /// and removed, not recreated. Fails closed; the pass's missing-container
    /// path recreates it once a start is allowed again.
    DriftRemoved {
        drift: MountDrift,
    },
    /// Missing, and a recorded [`Outcome::DriftRemoved`] still stands (the
    /// denial applies, or cannot be re-checked): not recreated.
    DriftRemovalStands,
    /// Skipped without any `docker` call until `retry_at` (unix secs).
    BackingOff {
        retry_at: u64,
    },
    /// Reading this container failed: Docker is unusable. The rest of the
    /// pass was skipped and the whole pass backs off until `retry_at`.
    DockerUnavailable {
        error: String,
        retry_at: u64,
    },
    Failed {
        error: String,
        retry_at: u64,
    },
}

impl Outcome {
    /// The container was (re)started by this pass.
    #[must_use]
    pub fn started(&self) -> bool {
        matches!(self, Self::Resumed | Self::Recreated { .. } | Self::DriftRecreated { .. })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountOutcome {
    pub name: String,
    pub container: String,
    pub outcome: Outcome,
}

/// What the loop remembers about one account between passes.
#[derive(Debug, Default, Clone)]
struct AccountMemory {
    /// Last workspace label / image seen on the container, reused when it
    /// has to be recreated and no operator start is recorded on disk.
    workspace: Option<PathBuf>,
    image: Option<String>,
    restarting_streak: u32,
    crash_loop_reported: bool,
    private_reported: bool,
    held_reported: bool,
    /// Started by a previous pass; confirm it on the next running sighting.
    awaiting_confirm: bool,
    failures: u32,
    retry_at: u64,
    last_error: Option<String>,
    /// Mount-drift bookkeeping ([`drift`]).
    drift: drift::DriftMemory,
}

/// State carried across passes (in memory; a daemon restart starts fresh —
/// what must survive one, the hold and the last operator start, is on disk
/// in [`session_hold`]).
#[derive(Debug, Default)]
pub struct ReconcileState {
    accounts: HashMap<String, AccountMemory>,
    /// The registry read error already WARNed about, and when to repeat it.
    registry_error: Option<(String, u32, u64)>,
    /// Pass-level backoff while Docker is unavailable (never per account).
    docker_failures: u32,
    docker_retry_at: u64,
    docker_last_error: Option<String>,
}

/// The container read failed: the runtime, not the account, is the problem.
#[derive(Debug)]
struct DockerUnavailable(anyhow::Error);

impl std::fmt::Display for DockerUnavailable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:#}", self.0)
    }
}

impl std::error::Error for DockerUnavailable {}

/// Per-account facts gathered across **every** registered root before a
/// pass acts on any of them (issue #10453): an account disabled in one root,
/// or held by an operator `stop` recorded in any of its profile directories,
/// is held everywhere.
#[derive(Debug, Default, Clone)]
pub struct AccountIndex {
    disabled: HashSet<String>,
    profiles: HashMap<String, Vec<PathBuf>>,
}

impl AccountIndex {
    #[must_use]
    pub fn from_inventories(inventories: &[&[AccountDescriptor]]) -> Self {
        let mut index = Self::default();
        for account in inventories.iter().flat_map(|inv| inv.iter()) {
            if account.id.provider != AccountProvider::Codex {
                continue;
            }
            let name = account.id.name.clone();
            if !account.enabled {
                index.disabled.insert(name.clone());
            }
            let profiles = index.profiles.entry(name).or_default();
            if !profiles.contains(&account.credential_reference) {
                profiles.push(account.credential_reference.clone());
            }
        }
        index
    }

    fn profiles(&self, name: &str) -> &[PathBuf] {
        self.profiles.get(name).map_or(&[], Vec::as_slice)
    }

    /// `enabled=false` in at least one root.
    #[must_use]
    pub fn is_disabled(&self, name: &str) -> bool {
        self.disabled.contains(name)
    }

    /// An operator hold is in force (read from disk on every call).
    #[must_use]
    pub fn is_held(&self, name: &str) -> bool {
        session_hold::held_across(self.profiles(name))
    }

    /// The last operator start recorded for `name` in any root.
    #[must_use]
    pub fn last_start(&self, name: &str) -> Option<LastStart> {
        session_hold::latest_start(self.profiles(name))
    }
}

/// What one pass reads besides the containers themselves.
pub struct PassInputs<'a> {
    /// Holds and `enabled=false` across every registered root.
    pub index: &'a AccountIndex,
    /// Whether an account is configured for private-clone mode (`Err`:
    /// cannot be ruled out, treated as private).
    pub is_private_clone: &'a dyn Fn(&AccountDescriptor) -> anyhow::Result<bool>,
    /// The guessed workspace for a missing container with no recorded start.
    pub fallback_workspace: &'a Path,
    /// The workspace registry's roots **now**: what a drifted container's
    /// mounts are compared against (#10364). `None` when the registry could
    /// not be read: no drift decision is made this pass.
    pub registered: Option<&'a [PathBuf]>,
    /// What `session start --mount-workspace <workspace>` refuses to mount
    /// whatever the registry says (home, `firewall: true` repositories). A
    /// running container that mounts one has `extra` drift. `Err`: cannot
    /// be decided, which is neither "denied" nor "allowed".
    pub denials_for: &'a dyn Fn(&Path) -> anyhow::Result<drift::Denials>,
    /// Whether `create` would accept a container for this workspace now —
    /// in production the very function `create` calls
    /// ([`session_mount_gate::create_roots`]).
    pub would_create_accept: &'a dyn Fn(&Path) -> anyhow::Result<()>,
    /// Where the per-container dispatch locks live
    /// ([`session_dispatch_lock`]); `None`: unknown, so nothing is stopped.
    pub dispatch_locks: Option<&'a Path>,
    /// Act only on accounts of this [`drift::priority`] class (`run_tick`
    /// walks the classes in order across every root); `None`: all, sorted.
    pub class: Option<u8>,
}

/// The pass's single container read: one [`Snapshot`] from `take`, taken on
/// first use and reused for every account after that (issue #10364 Part B).
pub struct PassSnapshot<'a> {
    take: &'a mut dyn FnMut() -> Snapshot,
    taken: Option<Snapshot>,
}

impl<'a> PassSnapshot<'a> {
    pub fn new(take: &'a mut dyn FnMut() -> Snapshot) -> Self {
        Self { take, taken: None }
    }

    /// The pass's snapshot, taking it on the first call.
    pub fn get(&mut self) -> &Snapshot {
        self.taken.get_or_insert_with(|| (self.take)())
    }
}

/// Whether this pass will read `name`'s container: not held, and neither the
/// pass nor the account is backing off.
fn will_read(state: &ReconcileState, index: &AccountIndex, name: &str, now: u64) -> bool {
    state.docker_retry_at <= now
        && state.accounts.get(name).is_none_or(|m| m.retry_at <= now)
        && !index.is_held(name)
}

/// Reconcile `accounts` (already resolved from `lifecycle`'s registry root).
/// Only session-managed Codex accounts that `inputs.index` says are enabled
/// in every root and not held are acted on, and nothing reaches
/// `lifecycle`'s runner (or `observe`) for any other account.
///
/// Accounts whose running container still mounts something the registry no
/// longer lists (`extra` drift, a containment gap) are handled first, then
/// those only missing a mount, then the rest, so a pass spent waiting on
/// `docker stop`/`run` closes the gap before anything else.
pub fn reconcile_accounts<R: ContainerRunner>(
    lifecycle: &mut SessionLifecycle<R>,
    accounts: &[AccountDescriptor],
    inputs: &PassInputs<'_>,
    observe: &mut PassSnapshot<'_>,
    state: &mut ReconcileState,
    now: u64,
) -> Vec<AccountOutcome> {
    let index = inputs.index;
    let mut eligible: Vec<&AccountDescriptor> = accounts
        .iter()
        .filter(|a| {
            a.id.provider == AccountProvider::Codex
                && a.enabled
                && !index.is_disabled(&a.id.name)
                && is_session_managed(&a.credential_reference)
        })
        .collect();
    if eligible
        .iter()
        .any(|a| will_read(state, index, &a.id.name, now))
    {
        let snapshot = observe.get();
        let class =
            |a: &&AccountDescriptor| drift::priority(snapshot, &container_name(&a.id.name), inputs);
        match inputs.class {
            Some(only) => eligible.retain(|a| class(a) == only),
            // Stable: accounts without drift keep their registry order.
            None => eligible.sort_by_cached_key(class),
        }
    } else if inputs.class.is_some_and(|only| only != drift::LAST_CLASS) {
        // Nothing will be read, so nothing is classified: report these
        // accounts once, with the last class.
        eligible.clear();
    }
    let mut out = Vec::new();
    for account in eligible {
        let name = account.id.name.clone();
        let container = container_name(&name);
        let docker_retry_at = state.docker_retry_at;
        let mem = state.accounts.entry(name.clone()).or_default();
        let outcome = if index.is_held(&name) {
            note_held(&container, mem)
        } else if docker_retry_at > now {
            Outcome::BackingOff {
                retry_at: docker_retry_at,
            }
        } else if mem.retry_at > now {
            mem.held_reported = false;
            Outcome::BackingOff {
                retry_at: mem.retry_at,
            }
        } else {
            mem.held_reported = false;
            match reconcile_one(lifecycle, account, &container, inputs, observe.get(), mem) {
                Ok(outcome) => {
                    if outcome == Outcome::Running {
                        if mem.failures > 0 {
                            log::info!("session_reconcile: {container}: reconciled after failures");
                        }
                        mem.failures = 0;
                        mem.last_error = None;
                    }
                    mem.retry_at = 0;
                    outcome
                }
                Err(error) if error.downcast_ref::<OperatorHeld>().is_some() => {
                    note_held(&container, mem)
                }
                Err(error)
                    if error.downcast_ref::<DockerUnavailable>().is_some()
                        || error.downcast_ref::<DockerTimedOut>().is_some() =>
                {
                    let outcome = docker_unavailable(state, &format!("{error:#}"), now);
                    out.push(AccountOutcome {
                        name,
                        container,
                        outcome,
                    });
                    // Skip the rest of the pass: no read, no start, no
                    // per-account failure for anyone else either.
                    break;
                }
                Err(error) => record_failure(&container, mem, &format!("{error:#}"), now),
            }
        };
        if !matches!(outcome, Outcome::Held | Outcome::BackingOff { .. }) {
            docker_available(state);
        }
        out.push(AccountOutcome {
            name,
            container,
            outcome,
        });
    }
    out
}

fn docker_unavailable(state: &mut ReconcileState, error: &str, now: u64) -> Outcome {
    state.docker_failures += 1;
    let delay = backoff_secs(state.docker_failures);
    state.docker_retry_at = now + delay;
    if state.docker_last_error.as_deref() == Some(error) {
        log::debug!("session_reconcile: Docker still unavailable (next pass in {delay}s): {error}");
    } else {
        log::warn!(
            "session_reconcile: Docker unavailable; skipping this pass and backing off {delay}s \
             (no container touched): {error}"
        );
    }
    state.docker_last_error = Some(error.to_string());
    Outcome::DockerUnavailable {
        error: error.to_string(),
        retry_at: state.docker_retry_at,
    }
}

fn docker_available(state: &mut ReconcileState) {
    if state.docker_failures > 0 {
        log::info!("session_reconcile: Docker reachable again");
    }
    state.docker_failures = 0;
    state.docker_retry_at = 0;
    state.docker_last_error = None;
}

/// A held account: forget in-flight start/backoff bookkeeping (the operator
/// owns it now) and say so once per hold.
fn note_held(container: &str, mem: &mut AccountMemory) -> Outcome {
    mem.awaiting_confirm = false;
    mem.drift = drift::DriftMemory::default();
    mem.failures = 0;
    mem.retry_at = 0;
    mem.last_error = None;
    if !mem.held_reported {
        log::info!(
            "session_reconcile: {container}: held (operator stop); not reconciled until \
             `loom-daemon accounts session start`"
        );
        mem.held_reported = true;
    }
    Outcome::Held
}

fn record_failure(container: &str, mem: &mut AccountMemory, error: &str, now: u64) -> Outcome {
    mem.failures += 1;
    let delay = backoff_secs(mem.failures);
    mem.retry_at = now + delay;
    if mem.last_error.as_deref() == Some(error) {
        log::debug!(
            "session_reconcile: {container}: still failing (attempt {}, next retry in {delay}s): {error}",
            mem.failures
        );
    } else {
        log::warn!(
            "session_reconcile: {container}: could not reconcile the session container \
             (attempt {}, backing off {delay}s): {error}",
            mem.failures
        );
    }
    mem.last_error = Some(error.to_string());
    Outcome::Failed {
        error: error.to_string(),
        retry_at: mem.retry_at,
    }
}

/// One container as the pass's [`Snapshot`] holds it: its raw `docker
/// inspect` object (`None`: missing). `Err` when the snapshot is
/// [`Snapshot::Unavailable`] — Docker could not be asked, which says nothing
/// about the container.
pub fn observe_container<'s>(
    snapshot: &'s Snapshot,
    container: &str,
) -> anyhow::Result<Option<&'s Value>> {
    match snapshot {
        Snapshot::Available(_) => Ok(snapshot.inspect_of(container)),
        Snapshot::Unavailable(reason) => Err(anyhow::anyhow!("{reason}")),
    }
}

/// The lifecycle's view of one `docker inspect` object. Docker's `Running &&
/// Restarting` is **not** running ([`session_state::container_running`]).
#[must_use]
pub fn container_state(inspect: &Value) -> ContainerState {
    let text = |v: &Value| v.as_str().filter(|s| !s.is_empty()).map(str::to_string);
    ContainerState {
        id: inspect["Id"].as_str().unwrap_or_default().to_string(),
        running: session_state::container_running(inspect),
        restarting: inspect["State"]["Restarting"] == Value::Bool(true),
        started_at: text(&inspect["State"]["StartedAt"]),
        image: text(&inspect["Config"]["Image"]),
        workspace: session_state::workspace_label(inspect).map(Path::to_path_buf),
    }
}

/// Recreate `name`'s missing host-mounted session container against
/// `workspace` (and `image`, `None` = the default image) — the reusable
/// "recreate this account's container" path (the #10364 drift fix calls it
/// after removing a drifted container). It refuses a held account
/// ([`OperatorHeld`], checked before and again inside the start) and a
/// workspace of `/`, and never lifts a hold or records an operator choice.
pub fn recreate_container<R: ContainerRunner>(
    lifecycle: &mut SessionLifecycle<R>,
    name: &str,
    workspace: &Path,
    image: Option<String>,
    is_held: &dyn Fn() -> bool,
) -> anyhow::Result<SessionStatus> {
    if is_held() {
        return Err(OperatorHeld.into());
    }
    if workspace.parent().is_none() {
        anyhow::bail!(
            "refusing to recreate the session container against {} (it would mount the \
             whole filesystem); start it by hand: `loom-daemon accounts session start {name} \
             --mount-workspace <checkout parent>`",
            workspace.display()
        );
    }
    lifecycle.set_image(image);
    let started = lifecycle.start_unless_held(name, Some(workspace), is_held);
    lifecycle.set_image(None);
    started
}

fn describe(state: Option<&ContainerState>) -> &'static str {
    match state {
        None => "missing",
        Some(s) if s.restarting => "restarting",
        Some(s) if s.running => "running",
        Some(_) => "stopped",
    }
}

fn reconcile_one<R: ContainerRunner>(
    lifecycle: &mut SessionLifecycle<R>,
    account: &AccountDescriptor,
    container: &str,
    ctx: &PassInputs<'_>,
    snapshot: &Snapshot,
    mem: &mut AccountMemory,
) -> anyhow::Result<Outcome> {
    let name = account.id.name.as_str();
    let inspect = observe_container(snapshot, container).map_err(DockerUnavailable)?;
    let state = inspect.map(container_state);
    if ctx.index.is_held(name) {
        // `stop` wrote its hold while we were inspecting.
        return Err(OperatorHeld.into());
    }
    // A recorded fail-closed removal is finished, never undone: a container
    // still present under it is removed, never started (#10364).
    if inspect.is_some() {
        if let Some(outcome) = drift::finish_recorded_removal(lifecycle, account, ctx, mem)? {
            return Ok(outcome);
        }
    }
    if std::mem::take(&mut mem.awaiting_confirm) {
        if !state.as_ref().is_some_and(|s| s.running) {
            anyhow::bail!(
                "container did not stay running after the last reconcile start (now {})",
                describe(state.as_ref())
            );
        }
        log::info!("session_reconcile: {container}: running after reconcile");
    }
    if let Some(s) = &state {
        if s.workspace.is_some() {
            mem.workspace.clone_from(&s.workspace);
        }
        if s.image.is_some() {
            mem.image.clone_from(&s.image);
        }
    }
    let needs_mode = state.as_ref().is_none_or(|s| !s.running && !s.restarting);
    let private = needs_mode && {
        let remembered_private =
            mem.workspace.as_deref() == Some(Path::new(private_workspace::REPO));
        match (ctx.is_private_clone)(account) {
            Ok(private) => private || remembered_private,
            Err(error) => {
                if !mem.private_reported {
                    log::warn!(
                        "session_reconcile: {container}: cannot rule out private-clone mode \
                         ({error:#}); not recreating it host-mounted"
                    );
                    mem.private_reported = true;
                }
                true
            }
        }
    };
    let recorded = ctx.index.last_start(name);
    let (recreate_workspace, recreate_image, guessed) = match (&recorded, &mem.workspace) {
        (Some(start), _) => (start.workspace.clone(), Some(start.image.clone()), false),
        (None, Some(seen)) => (seen.clone(), mem.image.clone(), false),
        (None, None) => (ctx.fallback_workspace.to_path_buf(), mem.image.clone(), true),
    };
    let decision = decide(state.as_ref(), private, mem.restarting_streak, &recreate_workspace);
    if !matches!(decision, Decision::Restarting | Decision::CrashLoop) {
        if mem.crash_loop_reported {
            log::warn!("session_reconcile: {container}: no longer restarting (crash loop over)");
        }
        mem.restarting_streak = 0;
        mem.crash_loop_reported = false;
    }
    if decision != Decision::PrivateClone {
        mem.private_reported = false;
    }
    let is_held = || ctx.index.is_held(name);
    Ok(match decision {
        Decision::Healthy => match inspect {
            Some(inspect) => drift::reconcile_running(lifecycle, account, inspect, ctx, mem)?,
            None => Outcome::Running,
        },
        Decision::Restarting => {
            mem.restarting_streak += 1;
            log::debug!("session_reconcile: {container}: restarting (Docker is retrying it)");
            Outcome::Restarting
        }
        Decision::CrashLoop => {
            mem.restarting_streak += 1;
            if mem.crash_loop_reported {
                log::debug!(
                    "session_reconcile: {container}: still crash-looping ({} passes)",
                    mem.restarting_streak
                );
            } else {
                log::warn!(
                    "session_reconcile: crash loop: session container {container} (account \
                     {name}) has been restarting for {} consecutive passes; not reusing it. \
                     Inspect `docker logs {container}`, then recreate it: `loom-daemon \
                     accounts session stop {name} --force` and `accounts session start {name} \
                     --mount-workspace <checkout parent>`",
                    mem.restarting_streak
                );
                mem.crash_loop_reported = true;
            }
            Outcome::CrashLoop
        }
        Decision::PrivateClone => {
            if !mem.private_reported {
                log::warn!(
                    "session_reconcile: {container} (account {name}) is a private-clone session \
                     and is not running; it is never recreated host-mounted. Restart it with \
                     `loom-daemon accounts session start {name} --private-clone <URL> --base \
                     <BRANCH>`"
                );
                mem.private_reported = true;
            }
            Outcome::PrivateCloneSkipped
        }
        Decision::Resume { workspace } => {
            // Never `docker start` a container that mounts a denied path.
            if let Some(inspect) = inspect {
                if let Some(out) =
                    drift::denied_while_stopped(lifecycle, account, inspect, ctx, mem)?
                {
                    return Ok(out);
                }
            }
            lifecycle.start_unless_held(name, workspace.as_deref(), &is_held)?;
            mem.awaiting_confirm = true;
            log::warn!("session_reconcile: {container}: was stopped; resumed it (docker start)");
            Outcome::Resumed
        }
        Decision::Recreate { workspace } => {
            if let Some(stands) = drift::removal_stands(name, &workspace, ctx, mem) {
                return Ok(stands);
            }
            if guessed {
                log::warn!(
                    "session_reconcile: {container}: no operator start is recorded for {name}; \
                     recreating against the guessed workspace {} (common parent of the \
                     registered roots)",
                    workspace.display()
                );
            }
            recreate_container(lifecycle, name, &workspace, recreate_image, &is_held)?;
            mem.awaiting_confirm = true;
            log::warn!(
                "session_reconcile: {container}: was missing; recreated it host-mounted \
                 (--mount-workspace {}, {SESSION_POSTURE_LABEL}={SESSION_POSTURE})",
                workspace.display()
            );
            Outcome::Recreated { workspace }
        }
    })
}

// ============================================================================
// Runtime wiring
// ============================================================================

/// The registry could not be read: say so (WARN, repeated on the backoff
/// schedule while the same error lasts) and make no drift decision.
fn registry_unreadable(state: &mut ReconcileState, error: &str, now: u64) {
    let (failures, warn_at) = match &state.registry_error {
        Some((last, failures, warn_at)) if last == error => (*failures, *warn_at),
        _ => (0, 0),
    };
    if now < warn_at {
        log::debug!("session_reconcile: workspace registry still unreadable: {error}");
        return;
    }
    let again = backoff_secs(failures + 1);
    log::warn!(
        "session_reconcile: the workspace registry cannot be read ({error}); no mount-drift \
         decision is made (no container is stopped, removed or recreated for drift) until it \
         can. Next reminder in {again}s"
    );
    state.registry_error = Some((error.to_string(), failures + 1, now + again));
}

/// One production pass over every registered root's Codex accounts.
pub fn run_tick(fallback_root: &Path, state: &mut ReconcileState, now: u64) -> Vec<AccountOutcome> {
    let (registry, readable) = match WorkspaceRegistry::load_default() {
        Ok(registry) => {
            if state.registry_error.take().is_some() {
                log::info!("session_reconcile: workspace registry readable again");
            }
            (registry, true)
        }
        Err(error) => {
            registry_unreadable(state, &format!("{error:#}"), now);
            (WorkspaceRegistry::default(), false)
        }
    };
    let registered = registry.roots();
    let fallback_workspace = default_mount_workspace(&registered, fallback_root);
    let inventories: Vec<(PathBuf, Vec<AccountDescriptor>)> = registry
        .effective_roots(fallback_root)
        .into_iter()
        .filter_map(|root| {
            let inventory = account_inventory_quiet(&root, AccountProvider::Codex).ok()?;
            Some((root, inventory))
        })
        .collect();
    // Holds and `enabled=false` count across every root before any root acts.
    let index = AccountIndex::from_inventories(
        &inventories
            .iter()
            .map(|(_, inv)| inv.as_slice())
            .collect::<Vec<_>>(),
    );
    // One container read for the whole pass (every root), taken lazily.
    let mut take =
        || session_state::snapshot("docker", &registered, session_state::SNAPSHOT_DEADLINE);
    let mut observe = PassSnapshot::new(&mut take);
    let dispatch_locks = session_dispatch_lock::lock_dir();
    let mut seen = HashSet::new();
    let roots: Vec<(PathBuf, Vec<AccountDescriptor>)> = inventories
        .into_iter()
        .map(|(root, inventory)| {
            let accounts = inventory
                .into_iter()
                .filter(|a| {
                    a.enabled
                        && is_session_managed(&a.credential_reference)
                        && seen.insert(a.id.name.clone())
                })
                .collect();
            (root, accounts)
        })
        .collect();
    let mut all: Vec<AccountOutcome> = Vec::new();
    // `extra` drift first, then missing-only, then the rest — across every
    // root, not root by root.
    let mut done: HashSet<String> = HashSet::new();
    for class in 0..=drift::LAST_CLASS {
        for (root, accounts) in &roots {
            // Each account is acted on (and reported) once per pass.
            let accounts: Vec<AccountDescriptor> = accounts
                .iter()
                .filter(|a| !done.contains(&a.id.name))
                .cloned()
                .collect();
            if accounts.is_empty() {
                continue;
            }
            let mut lifecycle = SessionLifecycle::new(root.clone(), ProcessContainerRunner, None);
            let is_private_clone =
                |a: &AccountDescriptor| private_workspace::configured(root, &a.id.name);
            let inputs = PassInputs {
                index: &index,
                is_private_clone: &is_private_clone,
                fallback_workspace: &fallback_workspace,
                registered: readable.then_some(registered.as_slice()),
                denials_for: &drift::Denials::load,
                would_create_accept: &|workspace| {
                    session_mount_gate::create_roots(workspace).map(drop)
                },
                dispatch_locks: dispatch_locks.as_deref(),
                class: Some(class),
            };
            let outcomes =
                reconcile_accounts(&mut lifecycle, &accounts, &inputs, &mut observe, state, now);
            done.extend(outcomes.iter().map(|o| o.name.clone()));
            let started: Vec<AccountDescriptor> = accounts
                .iter()
                .filter(|a| {
                    outcomes
                        .iter()
                        .any(|o| o.name == a.id.name && o.outcome.started())
                })
                .cloned()
                .collect();
            if !started.is_empty() {
                refresh_session_health_uncached(root, &started, now);
            }
            all.extend(outcomes);
        }
    }
    all
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// Spawn the loop on the shared runtime. The first tick fires immediately, so
/// the pass also runs once at daemon start.
pub fn spawn_task(fallback_root: PathBuf, interval: Duration) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut state = ReconcileState::default();
        loop {
            ticker.tick().await;
            let root = fallback_root.clone();
            let mut carried = std::mem::take(&mut state);
            let joined = tokio::task::spawn_blocking(move || {
                run_tick(&root, &mut carried, unix_now());
                carried
            })
            .await;
            match joined {
                Ok(next) => state = next,
                Err(e) => log::error!(
                    "session_reconcile: pass panicked ({e}); continuing with fresh state"
                ),
            }
        }
    })
}

/// Daemon entry point: resolve the opt-out from `workspace`'s config and
/// start the loop, or return `None` when disabled.
pub fn spawn_from_config(workspace: &Path) -> Option<tokio::task::JoinHandle<()>> {
    let config = read_config(workspace);
    if !resolve_enabled(&config) {
        log::debug!(
            "session_reconcile: disabled (LOOM_SESSION_RECONCILE=0 or \
             autonomous.sessionReconcile.enabled=false)"
        );
        return None;
    }
    let interval = resolve_interval(&config);
    log::info!(
        "session_reconcile: enabled (interval={}s; no-op without session-managed Codex accounts)",
        interval.as_secs()
    );
    Some(spawn_task(workspace.to_path_buf(), interval))
}

#[cfg(test)]
#[path = "session_reconcile_tests.rs"]
mod tests;
