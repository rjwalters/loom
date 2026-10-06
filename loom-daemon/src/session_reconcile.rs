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
//! | running | nothing |
//! | restarting (1st pass) | nothing — Docker's `unless-stopped` policy is already retrying |
//! | restarting (2nd+ consecutive pass) | WARN once as a **crash loop**; never reused, never stopped |
//! | stopped, host-mounted | resume via [`SessionLifecycle::start_with_workspace`] (the `accounts session start` path) |
//! | missing, host-mounted | recreate via the same path, with the workspace/image last seen on this container, else [`default_mount_workspace`] |
//! | stopped/missing, private-clone | skip and WARN once — never recreated host-mounted |
//!
//! After a resume/recreate the pass calls [`refresh_session_health`] for the
//! restarted accounts, so a dead `auth.json` chain lands in `ReauthRequired`
//! instead of being dispatched into.
//!
//! # Guard rails
//!
//! * **Never stops, removes or restarts a container.** The only `docker`
//!   mutations are `docker start` of a *stopped* container and `docker run`
//!   of a *missing* one, so a container with an in-flight `docker exec` (the
//!   #5119 contract) is never touched. A restarting container is not touched
//!   either: `stop` without `--force` cannot judge one (its `docker top`
//!   in-flight check fails while Docker is between restarts).
//! * **Operator hold:** account `enabled=false` (`loom-daemon accounts
//!   disable <name>`). A disabled account is skipped before any `docker`
//!   call. No separate hold marker exists.
//! * **Backoff:** a failed inspect/start backs that account off exponentially
//!   ([`BACKOFF_BASE_SECS`] doubling to [`BACKOFF_MAX_SECS`]); the failure is
//!   WARNed once per distinct error, then logged at DEBUG.
//! * **No-op:** with no enabled session-managed account the pass makes zero
//!   `docker` calls, like [`refresh_session_health`].
//! * **Opt-out:** `LOOM_SESSION_RECONCILE=0` or
//!   `autonomous.sessionReconcile.enabled=false` (precedence env > config >
//!   default **on**); cadence `LOOM_SESSION_RECONCILE_INTERVAL_SECS` /
//!   `autonomous.sessionReconcile.intervalSecs` (default
//!   [`DEFAULT_SESSION_RECONCILE_INTERVAL_SECS`]). The pass runs once at
//!   daemon start, then every interval.
//!
//! Out of scope (Epic #10452 Phase 2): recreating for mount drift (#10364)
//! and restart policy.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::tokens_pool::account_registry::{
    account_inventory_quiet, AccountDescriptor, AccountProvider,
};
use crate::tokens_pool::private_workspace;
use crate::tokens_pool::session_lifecycle::{
    container_name, is_session_managed, refresh_session_health, ContainerRunner, ContainerState,
    ProcessContainerRunner, SessionLifecycle, SESSION_POSTURE, SESSION_POSTURE_LABEL,
};
use crate::workspace_registry::WorkspaceRegistry;

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
/// against when the pass never saw its label: the deepest common ancestor of
/// the registered roots (`~/GitHub` for `~/GitHub/{a,b}` — narrowed back to
/// exactly those roots by `workspace_mount_roots`), a lone registered root
/// itself, or `daemon_root` when nothing is registered.
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
    Running,
    Restarting,
    CrashLoop,
    Resumed,
    Recreated {
        workspace: PathBuf,
    },
    PrivateCloneSkipped,
    /// Skipped without any `docker` call until `retry_at` (unix secs).
    BackingOff {
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
        matches!(self, Self::Resumed | Self::Recreated { .. })
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
    /// has to be recreated.
    workspace: Option<PathBuf>,
    image: Option<String>,
    restarting_streak: u32,
    crash_loop_reported: bool,
    private_reported: bool,
    /// Started by a previous pass; confirm it on the next running sighting.
    awaiting_confirm: bool,
    failures: u32,
    retry_at: u64,
    last_error: Option<String>,
}

/// State carried across passes (in memory; a daemon restart starts fresh).
#[derive(Debug, Default)]
pub struct ReconcileState {
    accounts: HashMap<String, AccountMemory>,
}

/// Reconcile `accounts` (already resolved from `lifecycle`'s registry root).
/// Only enabled, session-managed Codex accounts are considered, and nothing
/// reaches `lifecycle`'s runner for any other account.
pub fn reconcile_accounts<R: ContainerRunner>(
    lifecycle: &mut SessionLifecycle<R>,
    accounts: &[AccountDescriptor],
    is_private_clone: &dyn Fn(&AccountDescriptor) -> anyhow::Result<bool>,
    fallback_workspace: &Path,
    state: &mut ReconcileState,
    now: u64,
) -> Vec<AccountOutcome> {
    let mut out = Vec::new();
    for account in accounts.iter().filter(|a| {
        a.id.provider == AccountProvider::Codex
            && a.enabled
            && is_session_managed(&a.credential_reference)
    }) {
        let name = account.id.name.clone();
        let container = container_name(&name);
        let mem = state.accounts.entry(name.clone()).or_default();
        let outcome = if mem.retry_at > now {
            Outcome::BackingOff {
                retry_at: mem.retry_at,
            }
        } else {
            match reconcile_one(
                lifecycle,
                account,
                &container,
                is_private_clone,
                fallback_workspace,
                mem,
            ) {
                Ok(outcome) => {
                    if mem.failures > 0 {
                        log::info!("session_reconcile: {container}: reconciled after failures");
                    }
                    mem.failures = 0;
                    mem.retry_at = 0;
                    mem.last_error = None;
                    outcome
                }
                Err(error) => record_failure(&container, mem, &format!("{error:#}"), now),
            }
        };
        out.push(AccountOutcome {
            name,
            container,
            outcome,
        });
    }
    out
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

fn reconcile_one<R: ContainerRunner>(
    lifecycle: &mut SessionLifecycle<R>,
    account: &AccountDescriptor,
    container: &str,
    is_private_clone: &dyn Fn(&AccountDescriptor) -> anyhow::Result<bool>,
    fallback_workspace: &Path,
    mem: &mut AccountMemory,
) -> anyhow::Result<Outcome> {
    let name = account.id.name.as_str();
    let state = lifecycle.runner().inspect(container)?;
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
        match is_private_clone(account) {
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
    let recreate_workspace = mem
        .workspace
        .clone()
        .unwrap_or_else(|| fallback_workspace.to_path_buf());
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
    Ok(match decision {
        Decision::Healthy => {
            if std::mem::take(&mut mem.awaiting_confirm) {
                log::info!("session_reconcile: {container}: running after reconcile");
            }
            Outcome::Running
        }
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
            lifecycle.start_with_workspace(name, workspace.as_deref())?;
            mem.awaiting_confirm = true;
            log::warn!("session_reconcile: {container}: was stopped; resumed it (docker start)");
            Outcome::Resumed
        }
        Decision::Recreate { workspace } => {
            lifecycle.set_image(mem.image.clone());
            let started = lifecycle.start_with_workspace(name, Some(&workspace));
            lifecycle.set_image(None);
            started?;
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

/// One production pass over every registered root's Codex accounts.
pub fn run_tick(fallback_root: &Path, state: &mut ReconcileState, now: u64) -> Vec<AccountOutcome> {
    let registry = WorkspaceRegistry::load_default().unwrap_or_else(|e| {
        log::warn!("session_reconcile: could not load workspace registry ({e}); using fallback");
        WorkspaceRegistry::default()
    });
    let fallback_workspace = default_mount_workspace(&registry.roots(), fallback_root);
    let mut seen = HashSet::new();
    let mut all = Vec::new();
    for root in registry.effective_roots(fallback_root) {
        let Ok(inventory) = account_inventory_quiet(&root, AccountProvider::Codex) else {
            continue;
        };
        let accounts: Vec<AccountDescriptor> = inventory
            .into_iter()
            .filter(|a| {
                a.enabled
                    && is_session_managed(&a.credential_reference)
                    && seen.insert(a.id.name.clone())
            })
            .collect();
        if accounts.is_empty() {
            continue;
        }
        let mut lifecycle = SessionLifecycle::new(root.clone(), ProcessContainerRunner, None);
        let outcomes = reconcile_accounts(
            &mut lifecycle,
            &accounts,
            &|a| private_workspace::configured(&root, &a.id.name),
            &fallback_workspace,
            state,
            now,
        );
        let started: Vec<AccountDescriptor> = accounts
            .into_iter()
            .filter(|a| {
                outcomes
                    .iter()
                    .any(|o| o.name == a.id.name && o.outcome.started())
            })
            .collect();
        if !started.is_empty() {
            refresh_session_health(&root, &started, now);
        }
        all.extend(outcomes);
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
