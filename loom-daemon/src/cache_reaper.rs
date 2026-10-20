//! Report, and optionally quarantine, stray Codex profile directories under
//! the host-wide profile root (issue #12460, slice of #12453).
//!
//! `~/.loom/codex-profiles/` is host-wide and nothing removed directories from
//! it: one host carried 19 GB there. A profile directory is credential-bearing
//! (`auth.json`), and nothing can regenerate a credential, so this pass is
//! built differently from the cache GCs beside it ([`crate::target_dir_gc`],
//! [`crate::tmpfs_reclaim`]):
//!
//! * **Off by default.** `autonomous.worktreeReaper.codexProfileReclaim.enabled`
//!   (env `LOOM_CODEX_PROFILE_RECLAIM`) must be set. A fleet opts in per host.
//! * **Report first.** The default [`Mode`] once enabled is [`Mode::Report`]:
//!   the pass logs what it *would* quarantine and touches nothing.
//! * **Never `rm -rf` a profile.** [`Mode::Quarantine`] renames a candidate
//!   into the root's `.quarantine/` (the same place `accounts remove` parks a
//!   retired account) and leaves a `.cache-reaper.json` marker beside its
//!   files. The only deletion this module performs is of a quarantine entry
//!   that carries that marker and has sat there for the TTL
//!   (`quarantineTtlDays`); an operator's own `accounts remove` quarantine
//!   (`recovery.json`, no marker) is never purged here.
//! * **Fail closed on registration.** A directory is a candidate only when
//!   **no** registry the daemon can see names it: the ticking repo's, every
//!   registered workspace's (`~/.loom/workspaces.json`), and the shared one.
//!   A registered workspace *without* a registry file adopts every discovered
//!   directory as its pool, so one such workspace refuses the whole pass; so
//!   does any registry that cannot be read
//!   ([`unregistered_codex_profiles`]).
//!
//! A candidate is then still kept when **any** of these holds:
//!
//! 1. it is **session-managed** (`.session-managed.json`): a session container,
//!    running or stopped, owns it and the reconciler may `docker start` it
//!    again (#12777), so its mtime says nothing about liveness;
//! 2. a **private session identity** exists for it
//!    (`.private-sessions/<name>/workspace.json`, #10796);
//! 3. the daemon's own `CODEX_HOME` / `LOOM_CODEX_HOME` points into it;
//! 4. a process of this user has `CODEX_HOME` pointing into it (a running
//!    sweep). When `/proc` cannot be read, liveness is unknown and every
//!    candidate is kept, so a non-Linux host quarantines nothing;
//! 5. anything under it was modified within `minIdleDays` (default
//!    [`DEFAULT_MIN_IDLE_DAYS`]), or any part of it could not be read.
//!
//! Hidden directories (`.quarantine`, `.private-sessions`) and registered
//! profiles are never candidates. Growth *inside* a registered profile
//! (`sessions/` rollouts, `thread_history_*.sqlite`) is out of scope here and
//! is left alone -- measured on one operator host it was 90 % of the bytes.
//!
//! uv's cache is intentionally not pruned by the daemon: its bound is a host
//! tool setting provisioned from the worker spec, documented in
//! `troubleshooting.md` → "Bounding the uv cache".

use std::collections::HashSet;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use chrono::{DateTime, Utc};

use crate::tokens_pool::account_lifecycle::ensure_private_dir;
use crate::tokens_pool::account_registry::{unregistered_codex_profiles, UnregisteredProfiles};
use crate::tokens_pool::session_lifecycle::SESSION_MARKER_FILE;
use crate::workspace_registry::WorkspaceRegistry;

/// Master on/off env override (default **off**).
pub const ENABLE_ENV: &str = "LOOM_CODEX_PROFILE_RECLAIM";
/// Env override for the mode: `report` (default) or `quarantine`.
pub const MODE_ENV: &str = "LOOM_CODEX_PROFILE_RECLAIM_MODE";
/// Env override for the idle floor (days).
pub const MIN_IDLE_DAYS_ENV: &str = "LOOM_CODEX_PROFILE_RECLAIM_MIN_IDLE_DAYS";
/// Env override for the quarantine TTL (days).
pub const QUARANTINE_TTL_DAYS_ENV: &str = "LOOM_CODEX_PROFILE_RECLAIM_QUARANTINE_TTL_DAYS";
/// Env override for the host-wide cooldown (seconds).
pub const MIN_INTERVAL_ENV: &str = "LOOM_CODEX_PROFILE_RECLAIM_MIN_INTERVAL_SECS";

/// A candidate must have had no modification anywhere under it for this long.
pub const DEFAULT_MIN_IDLE_DAYS: u64 = 14;
/// Lowest idle floor any setting may select; a smaller value falls through to
/// the default. A pooled account held out for quota recovers within days.
pub const MIN_IDLE_DAYS_FLOOR: u64 = 7;
/// Default time a quarantined profile is kept before the pass purges it.
pub const DEFAULT_QUARANTINE_TTL_DAYS: u64 = 30;
/// Lowest TTL any setting may select; a smaller value falls through.
pub const QUARANTINE_TTL_DAYS_FLOOR: u64 = 7;
/// Default host-wide cooldown between passes: one hour. The pass walks every
/// candidate tree and `/proc`, and a multi-repo host must not do that once per
/// repo per 15-minute tick.
pub const DEFAULT_MIN_INTERVAL_SECS: u64 = 3_600;
/// Name of the quarantine subdirectory under the profile root, shared with
/// `accounts remove`.
pub const QUARANTINE_DIR: &str = ".quarantine";
/// Marker this pass leaves in a quarantine entry it created. Only entries with
/// this marker are ever purged by the TTL.
pub const REAPER_MARKER_FILE: &str = ".cache-reaper.json";

/// What an enabled pass does with a candidate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Mode {
    /// Log the verdict; mutate nothing.
    #[default]
    Report,
    /// Rename into `.quarantine/` and purge marked entries past the TTL.
    Quarantine,
}

impl Mode {
    fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "report" | "dry-run" | "dry_run" => Some(Self::Report),
            "quarantine" => Some(Self::Quarantine),
            _ => None,
        }
    }

    const fn as_str(self) -> &'static str {
        match self {
            Self::Report => "report",
            Self::Quarantine => "quarantine",
        }
    }
}

// ============================================================================
// Config (.loom/config.json → autonomous.worktreeReaper.codexProfileReclaim)
// ============================================================================

/// Precedence for every field is env > config > default.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CacheReaperConfig {
    pub enabled: Option<bool>,
    pub mode: Option<Mode>,
    pub min_idle_days: Option<u64>,
    pub quarantine_ttl_days: Option<u64>,
    pub min_interval_secs: Option<u64>,
}

#[must_use]
pub fn read_config(repo_root: &Path) -> CacheReaperConfig {
    let effective = crate::config_resolver::resolve_effective_config(repo_root);
    let Some(block) = crate::config_resolver::get_path(
        &effective,
        "autonomous.worktreeReaper.codexProfileReclaim",
    ) else {
        return CacheReaperConfig::default();
    };
    CacheReaperConfig {
        enabled: block.get("enabled").and_then(serde_json::Value::as_bool),
        mode: block
            .get("mode")
            .and_then(serde_json::Value::as_str)
            .and_then(Mode::parse),
        min_idle_days: block.get("minIdleDays").and_then(serde_json::Value::as_u64),
        quarantine_ttl_days: block
            .get("quarantineTtlDays")
            .and_then(serde_json::Value::as_u64),
        min_interval_secs: block
            .get("minIntervalSecs")
            .and_then(serde_json::Value::as_u64)
            .filter(|&s| s > 0),
    }
}

/// Default **off**: a host opts in explicitly.
#[must_use]
pub fn resolve_enabled(config: &CacheReaperConfig) -> bool {
    if let Ok(v) = std::env::var(ENABLE_ENV) {
        return matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on");
    }
    config.enabled.unwrap_or(false)
}

/// Default [`Mode::Report`]; an unrecognised value falls through to it.
#[must_use]
pub fn resolve_mode(config: &CacheReaperConfig) -> Mode {
    std::env::var(MODE_ENV)
        .ok()
        .and_then(|v| Mode::parse(&v))
        .or(config.mode)
        .unwrap_or_default()
}

fn resolve_days(env: &str, configured: Option<u64>, floor: u64, default: u64) -> u64 {
    std::env::var(env)
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .or(configured)
        .filter(|&d| d >= floor)
        .unwrap_or(default)
}

#[must_use]
pub fn resolve_min_idle_days(config: &CacheReaperConfig) -> u64 {
    resolve_days(
        MIN_IDLE_DAYS_ENV,
        config.min_idle_days,
        MIN_IDLE_DAYS_FLOOR,
        DEFAULT_MIN_IDLE_DAYS,
    )
}

#[must_use]
pub fn resolve_quarantine_ttl_days(config: &CacheReaperConfig) -> u64 {
    resolve_days(
        QUARANTINE_TTL_DAYS_ENV,
        config.quarantine_ttl_days,
        QUARANTINE_TTL_DAYS_FLOOR,
        DEFAULT_QUARANTINE_TTL_DAYS,
    )
}

#[must_use]
pub fn resolve_min_interval_secs(config: &CacheReaperConfig) -> u64 {
    std::env::var(MIN_INTERVAL_ENV)
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|&s| s > 0)
        .or(config.min_interval_secs)
        .unwrap_or(DEFAULT_MIN_INTERVAL_SECS)
}

// ============================================================================
// Report
// ============================================================================

/// What one pass did (or would do), for logging and tests.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct CacheReapReport {
    pub enabled: bool,
    pub mode: Mode,
    /// Set when the host-wide cooldown skipped this evaluation.
    pub deferred: Option<String>,
    /// Set when registration could not be determined; nothing was evaluated.
    pub refused: Option<String>,
    /// Candidates that passed every gate, in [`Mode::Report`].
    pub would_quarantine: Vec<PathBuf>,
    /// Candidates renamed into `.quarantine/`, in [`Mode::Quarantine`].
    pub quarantined: Vec<PathBuf>,
    /// Marked quarantine entries removed after the TTL.
    pub purged: Vec<PathBuf>,
    /// Candidates kept, each with the gate that kept it.
    pub kept: Vec<(PathBuf, String)>,
}

// ============================================================================
// Gates
// ============================================================================

/// Newest mtime anywhere under `path` (symlinks are not followed). Any error
/// anywhere in the walk is the caller's signal to keep the directory: an
/// unreadable subtree may hold the write that proves it live.
fn newest_mtime(path: &Path) -> io::Result<SystemTime> {
    let meta = fs::symlink_metadata(path)?;
    let mut newest = meta.modified()?;
    if meta.is_dir() {
        for entry in fs::read_dir(path)? {
            let entry = entry?;
            newest = newest.max(newest_mtime(&entry.path())?);
        }
    }
    Ok(newest)
}

/// `CODEX_HOME` values in the environments of this user's processes, or `None`
/// when `/proc` cannot be enumerated (callers must then assume "in use").
/// Only the one variable is kept; the rest of each environment is dropped
/// unread and nothing here is ever logged.
fn running_codex_homes() -> Option<Vec<PathBuf>> {
    let mut homes = Vec::new();
    for entry in fs::read_dir("/proc").ok()?.flatten() {
        if !entry
            .file_name()
            .to_string_lossy()
            .bytes()
            .all(|b| b.is_ascii_digit())
        {
            continue;
        }
        // Other users' environ is unreadable (and cannot be our sweeps').
        let Ok(environ) = fs::read(entry.path().join("environ")) else {
            continue;
        };
        for var in environ.split(|b| *b == 0) {
            if let Some(value) = var.strip_prefix(b"CODEX_HOME=") {
                let value = PathBuf::from(String::from_utf8_lossy(value).into_owned());
                homes.push(value.canonicalize().unwrap_or(value));
            }
        }
    }
    Some(homes)
}

/// The daemon's own Codex homes: anything it launches inherits them.
fn ambient_codex_homes() -> Vec<PathBuf> {
    ["CODEX_HOME", "LOOM_CODEX_HOME"]
        .iter()
        .filter_map(|key| std::env::var_os(key))
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .map(|value| value.canonicalize().unwrap_or(value))
        .collect()
}

/// Everything the pure core needs to decide about one candidate list.
pub struct Gates<'a> {
    pub root: &'a Path,
    pub now: SystemTime,
    /// `None` when process liveness is unknown, which keeps everything.
    pub in_use: Option<&'a [PathBuf]>,
    pub ambient: &'a [PathBuf],
    pub min_idle: Duration,
    pub mode: Mode,
}

/// The gate that keeps `dir`, if any. Pure apart from the filesystem reads.
fn keep_reason(name: &str, dir: &Path, gates: &Gates<'_>) -> Option<String> {
    if dir.join(SESSION_MARKER_FILE).is_file() {
        return Some("session-managed (a session container owns it)".into());
    }
    if gates
        .root
        .join(".private-sessions")
        .join(name)
        .join("workspace.json")
        .is_file()
    {
        return Some("private session identity exists".into());
    }
    if gates.ambient.iter().any(|home| home.starts_with(dir)) {
        return Some("the daemon's own CODEX_HOME".into());
    }
    let Some(in_use) = gates.in_use else {
        return Some("process liveness unknown".into());
    };
    if in_use.iter().any(|home| home.starts_with(dir)) {
        return Some("CODEX_HOME of a running process".into());
    }
    match newest_mtime(dir) {
        Err(error) => Some(format!("unreadable: {error}")),
        Ok(newest) => match gates.now.duration_since(newest) {
            Err(_) => Some("modified in the future".into()),
            Ok(age) if age < gates.min_idle => Some(format!(
                "modified {}d ago (floor {}d)",
                age.as_secs() / 86_400,
                gates.min_idle.as_secs() / 86_400
            )),
            Ok(_) => None,
        },
    }
}

/// Rename `dir` into the root's quarantine and leave this pass's marker in it.
/// The marker is written *after* the rename: a failed marker leaves an entry
/// the TTL never purges, which is the safe side.
fn quarantine(name: &str, dir: &Path, root: &Path, now: SystemTime) -> anyhow::Result<PathBuf> {
    let quarantine = root.join(QUARANTINE_DIR);
    ensure_private_dir(&quarantine)?;
    let since_epoch = now.duration_since(UNIX_EPOCH)?;
    let destination =
        quarantine.join(format!("{name}-{}-{}", since_epoch.as_secs(), since_epoch.subsec_nanos()));
    if destination.exists() {
        anyhow::bail!("quarantine destination {} already exists", destination.display());
    }
    fs::rename(dir, &destination)?;
    let marker = serde_json::json!({
        "version": 1,
        "source": "cache_reaper",
        "name": name,
        "quarantined_at_unix": since_epoch.as_secs(),
        "reason": "unregistered idle codex profile (#12460)",
    });
    let body = serde_json::to_vec_pretty(&marker)?;
    fs::write(destination.join(REAPER_MARKER_FILE), body)?;
    Ok(destination)
}

/// Remove quarantine entries **this pass** created more than `ttl` ago. An
/// entry without [`REAPER_MARKER_FILE`], or whose marker does not parse, is
/// somebody else's and is left alone.
fn purge_expired(root: &Path, now: SystemTime, ttl: Duration) -> Vec<PathBuf> {
    let mut purged = Vec::new();
    let Ok(entries) = fs::read_dir(root.join(QUARANTINE_DIR)) else {
        return purged;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(meta) = fs::symlink_metadata(&path) else {
            continue;
        };
        if !meta.is_dir() {
            continue;
        }
        let Ok(bytes) = fs::read(path.join(REAPER_MARKER_FILE)) else {
            continue;
        };
        let Ok(marker) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
            continue;
        };
        if marker["source"].as_str() != Some("cache_reaper") {
            continue;
        }
        let Some(at) = marker["quarantined_at_unix"].as_u64() else {
            continue;
        };
        let quarantined_at = UNIX_EPOCH + Duration::from_secs(at);
        let expired = now
            .duration_since(quarantined_at)
            .is_ok_and(|age| age >= ttl);
        if !expired {
            continue;
        }
        match fs::remove_dir_all(&path) {
            Ok(()) => purged.push(path),
            Err(error) => log::warn!(
                "cache_reaper: could not purge expired quarantine entry {}: {error}",
                path.display()
            ),
        }
    }
    purged
}

/// Core: decide about every candidate and act per `gates.mode`. Never deletes
/// a candidate; the only removal is [`purge_expired`], run by the caller.
pub fn prune(candidates: &[(String, PathBuf)], gates: &Gates<'_>) -> CacheReapReport {
    let mut report = CacheReapReport {
        enabled: true,
        mode: gates.mode,
        ..CacheReapReport::default()
    };
    for (name, dir) in candidates {
        if let Some(reason) = keep_reason(name, dir, gates) {
            report.kept.push((dir.clone(), reason));
            continue;
        }
        match gates.mode {
            Mode::Report => report.would_quarantine.push(dir.clone()),
            Mode::Quarantine => match quarantine(name, dir, gates.root, gates.now) {
                Ok(_) => report.quarantined.push(dir.clone()),
                Err(error) => report
                    .kept
                    .push((dir.clone(), format!("quarantine failed: {error:#}"))),
            },
        }
    }
    report
}

// ============================================================================
// Host-wide cooldown (mirrors `tmpfs_reclaim`)
// ============================================================================

static LAST_EVALUATED_AT: OnceLock<Mutex<Option<DateTime<Utc>>>> = OnceLock::new();

fn last_evaluated_slot() -> &'static Mutex<Option<DateTime<Utc>>> {
    LAST_EVALUATED_AT.get_or_init(|| Mutex::new(None))
}

fn cooldown_elapsed(now: DateTime<Utc>, min_interval_secs: u64) -> bool {
    let guard = last_evaluated_slot()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    match *guard {
        None => true,
        Some(last) => {
            let since = (now - last).num_seconds();
            since < 0 || since >= i64::try_from(min_interval_secs).unwrap_or(i64::MAX)
        }
    }
}

fn record_evaluated(now: DateTime<Utc>) {
    *last_evaluated_slot()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(now);
}

/// Drop cooldown state. Test-only seam.
#[doc(hidden)]
pub fn reset_state_for_test() {
    *last_evaluated_slot()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
}

// ============================================================================
// Production entry point
// ============================================================================

/// Every workspace whose dispatch could select a profile: the ticking repo
/// plus the whole workspace registry. `Err` when the registry cannot be read
/// -- the caller refuses, because an unlisted workspace could own a profile.
fn workspaces_to_consult(repo_root: &Path) -> anyhow::Result<Vec<PathBuf>> {
    let registry = WorkspaceRegistry::load_default()?;
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for root in std::iter::once(repo_root.to_path_buf()).chain(registry.roots()) {
        let key = root.canonicalize().unwrap_or_else(|_| root.clone());
        if seen.insert(key) {
            out.push(root);
        }
    }
    Ok(out)
}

fn log_report(report: &CacheReapReport) {
    if let Some(reason) = &report.deferred {
        log::debug!("cache_reaper: deferred: {reason}");
        return;
    }
    if let Some(reason) = &report.refused {
        log::info!("cache_reaper: refusing to evaluate the profile root: {reason}");
        return;
    }
    for dir in &report.would_quarantine {
        log::info!(
            "cache_reaper: would quarantine unregistered idle codex profile {} (mode={})",
            dir.display(),
            report.mode.as_str()
        );
    }
    for dir in &report.quarantined {
        log::info!(
            "cache_reaper: quarantined unregistered idle codex profile {} under {QUARANTINE_DIR}/",
            dir.display()
        );
    }
    for dir in &report.purged {
        log::info!("cache_reaper: purged expired quarantine entry {}", dir.display());
    }
    for (dir, reason) in &report.kept {
        log::info!("cache_reaper: kept {} ({reason})", dir.display());
    }
}

/// One production pass for `repo_root`. Never fails the reaper tick: every
/// error path evaluates nothing and says so in the report.
pub fn run_for(repo_root: &Path) -> CacheReapReport {
    let config = read_config(repo_root);
    let mode = resolve_mode(&config);
    if !resolve_enabled(&config) {
        return CacheReapReport {
            enabled: false,
            mode,
            ..CacheReapReport::default()
        };
    }
    let now_utc = Utc::now();
    let min_interval_secs = resolve_min_interval_secs(&config);
    if !cooldown_elapsed(now_utc, min_interval_secs) {
        let report = CacheReapReport {
            enabled: true,
            mode,
            deferred: Some(format!(
                "host-wide cooldown active (min interval {min_interval_secs}s)"
            )),
            ..CacheReapReport::default()
        };
        log_report(&report);
        return report;
    }
    record_evaluated(now_utc);

    let workspaces = match workspaces_to_consult(repo_root) {
        Ok(workspaces) => workspaces,
        Err(error) => {
            let report = CacheReapReport {
                enabled: true,
                mode,
                refused: Some(format!("workspace registry unreadable: {error:#}")),
                ..CacheReapReport::default()
            };
            log_report(&report);
            return report;
        }
    };
    let shared = crate::tokens_pool::paths::shared_accounts_root();
    let UnregisteredProfiles {
        candidates,
        refused,
    } = unregistered_codex_profiles(&workspaces, shared.as_deref());
    if let Some(reason) = refused {
        let report = CacheReapReport {
            enabled: true,
            mode,
            refused: Some(reason),
            ..CacheReapReport::default()
        };
        log_report(&report);
        return report;
    }
    let Some(root) = crate::tokens_pool::paths::codex_profile_root() else {
        return CacheReapReport {
            enabled: true,
            mode,
            refused: Some("Codex profile root is disabled".into()),
            ..CacheReapReport::default()
        };
    };

    let now = SystemTime::now();
    let homes = running_codex_homes();
    let ambient = ambient_codex_homes();
    let gates = Gates {
        root: &root,
        now,
        in_use: homes.as_deref(),
        ambient: &ambient,
        min_idle: Duration::from_secs(resolve_min_idle_days(&config) * 86_400),
        mode,
    };
    let mut report = prune(&candidates, &gates);
    if mode == Mode::Quarantine {
        let ttl = Duration::from_secs(resolve_quarantine_ttl_days(&config) * 86_400);
        report.purged = purge_expired(&root, now, ttl);
    }
    log_report(&report);
    report
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::tokens_pool::paths::SHARED_ACCOUNTS_ROOT_ENV;
    use crate::tokens_pool::profile_root_env::ProfileRootEnv;
    use crate::workspace_registry::REGISTRY_PATH_ENV;
    use serial_test::serial;

    const REGISTRY: &str = r#"{"version":1,"accounts":[
        {"provider":"codex","name":"live","credential_kind":"codex_home","credential_reference":"live"}]}"#;

    const ENV_KEYS: &[&str] = &[
        ENABLE_ENV,
        MODE_ENV,
        MIN_IDLE_DAYS_ENV,
        QUARANTINE_TTL_DAYS_ENV,
        MIN_INTERVAL_ENV,
        SHARED_ACCOUNTS_ROOT_ENV,
        REGISTRY_PATH_ENV,
        "CODEX_HOME",
        "LOOM_CODEX_HOME",
    ];

    struct Fixture {
        workspace: tempfile::TempDir,
        root: tempfile::TempDir,
        shared: tempfile::TempDir,
        _env: ProfileRootEnv,
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            for key in ENV_KEYS {
                std::env::remove_var(key);
            }
            reset_state_for_test();
        }
    }

    fn write_registry(workspace: &Path, body: &str) {
        fs::create_dir_all(workspace.join(".loom")).unwrap();
        fs::write(workspace.join(".loom/accounts.json"), body).unwrap();
    }

    fn fixture(registry: Option<&str>) -> Fixture {
        let workspace = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let shared = tempfile::tempdir().unwrap();
        let env = ProfileRootEnv::set(root.path());
        for key in ENV_KEYS {
            std::env::remove_var(key);
        }
        reset_state_for_test();
        std::env::set_var(SHARED_ACCOUNTS_ROOT_ENV, shared.path());
        // An empty workspace registry file: `run_for` must never read the
        // operator's real `~/.loom/workspaces.json` from a test.
        let registry_file = shared.path().join("workspaces.json");
        fs::write(&registry_file, "").unwrap();
        std::env::set_var(REGISTRY_PATH_ENV, &registry_file);
        if let Some(body) = registry {
            write_registry(workspace.path(), body);
        }
        for name in ["live", "stray", ".quarantine", ".private-sessions"] {
            fs::create_dir_all(root.path().join(name)).unwrap();
            fs::write(root.path().join(name).join("auth.json"), "{}").unwrap();
        }
        Fixture {
            workspace,
            root,
            shared,
            _env: env,
        }
    }

    fn candidates(f: &Fixture) -> Vec<(String, PathBuf)> {
        let answer =
            unregistered_codex_profiles(&[f.workspace.path().to_path_buf()], Some(f.shared.path()));
        assert_eq!(answer.refused, None, "fixture must be decidable");
        answer.candidates
    }

    fn far_future() -> SystemTime {
        SystemTime::now() + Duration::from_secs(60 * 24 * 60 * 60)
    }

    fn gates<'a>(
        f: &'a Fixture,
        now: SystemTime,
        in_use: Option<&'a [PathBuf]>,
        mode: Mode,
    ) -> Gates<'a> {
        Gates {
            root: f.root.path(),
            now,
            in_use,
            ambient: &[],
            min_idle: Duration::from_secs(DEFAULT_MIN_IDLE_DAYS * 86_400),
            mode,
        }
    }

    fn quarantine_entries(f: &Fixture) -> Vec<PathBuf> {
        let mut out: Vec<PathBuf> = fs::read_dir(f.root.path().join(QUARANTINE_DIR))
            .unwrap()
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.is_dir())
            .collect();
        out.sort();
        out
    }

    // ---- default off, report mode -----------------------------------------

    #[test]
    #[serial]
    fn disabled_by_default_and_touches_nothing() {
        let f = fixture(Some(REGISTRY));
        assert!(!resolve_enabled(&CacheReaperConfig::default()));
        let report = run_for(f.workspace.path());
        assert!(!report.enabled);
        assert!(report.would_quarantine.is_empty() && report.quarantined.is_empty());
        assert!(f.root.path().join("stray/auth.json").exists());
    }

    #[test]
    #[serial]
    fn enabled_defaults_to_report_mode_and_mutates_nothing() {
        let f = fixture(Some(REGISTRY));
        assert_eq!(resolve_mode(&CacheReaperConfig::default()), Mode::Report);
        let report = prune(&candidates(&f), &gates(&f, far_future(), Some(&[]), Mode::Report));
        let stray = f.root.path().canonicalize().unwrap().join("stray");
        assert_eq!(report.would_quarantine, vec![stray]);
        assert!(report.quarantined.is_empty());
        assert!(
            f.root.path().join("stray/auth.json").exists(),
            "report mode must not move anything"
        );
        assert!(quarantine_entries(&f).is_empty());
    }

    #[test]
    #[serial]
    fn env_enables_and_selects_quarantine_mode() {
        let f = fixture(Some(REGISTRY));
        std::env::set_var(ENABLE_ENV, "1");
        std::env::set_var(MODE_ENV, "quarantine");
        let config = CacheReaperConfig::default();
        assert!(resolve_enabled(&config));
        assert_eq!(resolve_mode(&config), Mode::Quarantine);
        std::env::set_var(MODE_ENV, "delete");
        assert_eq!(resolve_mode(&config), Mode::Report, "an unknown mode is report");
        std::env::set_var(MIN_IDLE_DAYS_ENV, "1");
        assert_eq!(
            resolve_min_idle_days(&config),
            DEFAULT_MIN_IDLE_DAYS,
            "below the floor falls through"
        );
        std::env::set_var(QUARANTINE_TTL_DAYS_ENV, "0");
        assert_eq!(resolve_quarantine_ttl_days(&config), DEFAULT_QUARANTINE_TTL_DAYS);
        drop(f);
    }

    // ---- quarantine instead of rm -rf ---------------------------------------

    #[test]
    #[serial]
    fn quarantine_mode_renames_into_quarantine_with_a_marker_and_deletes_nothing() {
        let f = fixture(Some(REGISTRY));
        let report = prune(&candidates(&f), &gates(&f, far_future(), Some(&[]), Mode::Quarantine));
        let stray = f.root.path().canonicalize().unwrap().join("stray");
        assert_eq!(report.quarantined, vec![stray]);
        assert!(!f.root.path().join("stray").exists());
        let entries = quarantine_entries(&f);
        assert_eq!(entries.len(), 1);
        assert!(entries[0]
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with("stray-"));
        assert!(entries[0].join("auth.json").exists(), "the credential travels with the profile");
        assert!(entries[0].join(REAPER_MARKER_FILE).exists());
        assert!(f.root.path().join("live/auth.json").exists());
        assert!(f.root.path().join(".quarantine/auth.json").exists());
        assert!(f.root.path().join(".private-sessions/auth.json").exists());
    }

    #[test]
    #[serial]
    fn ttl_purges_only_this_pass_s_expired_entries() {
        let f = fixture(Some(REGISTRY));
        let quarantine = f.root.path().join(QUARANTINE_DIR);
        let now = SystemTime::now();
        let secs = |t: SystemTime| t.duration_since(UNIX_EPOCH).unwrap().as_secs();
        let marked = |name: &str, at: SystemTime| {
            let dir = quarantine.join(name);
            fs::create_dir_all(&dir).unwrap();
            fs::write(dir.join("auth.json"), "{}").unwrap();
            fs::write(
                dir.join(REAPER_MARKER_FILE),
                format!(r#"{{"version":1,"source":"cache_reaper","name":"x","quarantined_at_unix":{}}}"#, secs(at)),
            )
            .unwrap();
            dir
        };
        let expired = marked("old-1-1", now - Duration::from_secs(31 * 86_400));
        let young = marked("young-1-1", now - Duration::from_secs(2 * 86_400));
        // An `accounts remove` quarantine: recovery.json, no marker.
        let operator = quarantine.join("retired-1-1");
        fs::create_dir_all(&operator).unwrap();
        fs::write(operator.join("recovery.json"), "{}").unwrap();
        // Our marker but a foreign source.
        let foreign = quarantine.join("foreign-1-1");
        fs::create_dir_all(&foreign).unwrap();
        fs::write(
            foreign.join(REAPER_MARKER_FILE),
            r#"{"version":1,"source":"someone","quarantined_at_unix":0}"#,
        )
        .unwrap();

        let purged = purge_expired(f.root.path(), now, Duration::from_secs(30 * 86_400));
        assert_eq!(purged, vec![expired.clone()]);
        assert!(!expired.exists());
        assert!(young.exists() && operator.exists() && foreign.exists());
    }

    // ---- fail closed on registration ---------------------------------------

    #[test]
    #[serial]
    fn no_registry_file_in_any_consulted_workspace_refuses() {
        let f = fixture(None);
        let answer =
            unregistered_codex_profiles(&[f.workspace.path().to_path_buf()], Some(f.shared.path()));
        assert!(answer.candidates.is_empty());
        assert!(answer
            .refused
            .as_deref()
            .unwrap()
            .contains("no .loom/accounts.json"));

        // The ticking repo has one, but another registered workspace does not.
        let other = tempfile::tempdir().unwrap();
        write_registry(f.workspace.path(), REGISTRY);
        let answer = unregistered_codex_profiles(
            &[f.workspace.path().to_path_buf(), other.path().to_path_buf()],
            Some(f.shared.path()),
        );
        assert!(answer.candidates.is_empty());
        assert!(answer.refused.is_some());
    }

    #[test]
    #[serial]
    fn unreadable_registry_refuses() {
        let f = fixture(Some("{not json"));
        let answer =
            unregistered_codex_profiles(&[f.workspace.path().to_path_buf()], Some(f.shared.path()));
        assert!(answer.candidates.is_empty());
        assert!(answer.refused.as_deref().unwrap().contains("unreadable"));

        // Same for the shared registry.
        write_registry(f.workspace.path(), REGISTRY);
        write_registry(f.shared.path(), "[]");
        let answer =
            unregistered_codex_profiles(&[f.workspace.path().to_path_buf()], Some(f.shared.path()));
        assert!(answer.refused.is_some());
    }

    #[test]
    #[serial]
    fn a_name_in_any_other_workspace_or_shared_registry_is_registered() {
        let f = fixture(Some(REGISTRY));
        let other = tempfile::tempdir().unwrap();
        write_registry(other.path(), &REGISTRY.replace("live", "stray"));
        let answer = unregistered_codex_profiles(
            &[f.workspace.path().to_path_buf(), other.path().to_path_buf()],
            Some(f.shared.path()),
        );
        assert_eq!(answer, UnregisteredProfiles::default());

        write_registry(f.shared.path(), &REGISTRY.replace("live", "stray"));
        let answer =
            unregistered_codex_profiles(&[f.workspace.path().to_path_buf()], Some(f.shared.path()));
        assert_eq!(answer, UnregisteredProfiles::default());
    }

    #[test]
    #[serial]
    fn a_directory_named_by_credential_reference_is_registered() {
        let f = fixture(Some(
            r#"{"version":1,"accounts":[
            {"provider":"codex","name":"live","credential_kind":"codex_home","credential_reference":"live"},
            {"provider":"codex","name":"alias","credential_kind":"codex_home","credential_reference":"stray"}]}"#,
        ));
        assert!(candidates(&f).is_empty());
    }

    #[test]
    #[serial]
    fn a_missing_workspace_root_is_skipped_but_none_consulted_refuses() {
        let f = fixture(Some(REGISTRY));
        let gone = f.root.path().join("no-such-workspace");
        let answer = unregistered_codex_profiles(
            &[f.workspace.path().to_path_buf(), gone.clone()],
            Some(f.shared.path()),
        );
        assert_eq!(answer.candidates.len(), 1);
        let answer = unregistered_codex_profiles(&[gone], Some(f.shared.path()));
        assert!(answer
            .refused
            .as_deref()
            .unwrap()
            .contains("no workspace registry"));
    }

    #[test]
    #[serial]
    fn disabled_root_refuses() {
        let f = fixture(Some(REGISTRY));
        let _off = ProfileRootEnv::set("");
        let answer =
            unregistered_codex_profiles(&[f.workspace.path().to_path_buf()], Some(f.shared.path()));
        assert!(answer.candidates.is_empty() && answer.refused.is_some());
    }

    #[test]
    #[serial]
    fn run_for_refuses_when_the_workspace_registry_is_unreadable() {
        let f = fixture(Some(REGISTRY));
        std::env::set_var(ENABLE_ENV, "1");
        std::env::set_var(MODE_ENV, "quarantine");
        std::env::set_var(REGISTRY_PATH_ENV, f.shared.path().join("workspaces.json"));
        fs::write(f.shared.path().join("workspaces.json"), "{not json").unwrap();
        let report = run_for(f.workspace.path());
        assert!(report.enabled);
        assert!(report
            .refused
            .as_deref()
            .unwrap()
            .contains("workspace registry"));
        assert!(f.root.path().join("stray/auth.json").exists());
    }

    #[test]
    #[serial]
    fn run_for_consults_every_registered_workspace() {
        let f = fixture(Some(REGISTRY));
        std::env::set_var(ENABLE_ENV, "1");
        std::env::set_var(MODE_ENV, "quarantine");
        // A second registered workspace without a registry file: refuse.
        let other = tempfile::tempdir().unwrap();
        let mut registry = WorkspaceRegistry::default();
        registry.add(other.path(), None).unwrap();
        registry
            .save(&f.shared.path().join("workspaces.json"))
            .unwrap();
        let report = run_for(f.workspace.path());
        assert!(report
            .refused
            .as_deref()
            .unwrap()
            .contains("no .loom/accounts.json"));
        assert!(f.root.path().join("stray/auth.json").exists());
    }

    // ---- liveness gates ------------------------------------------------------

    #[test]
    #[serial]
    fn recent_in_use_ambient_and_unknown_liveness_keep_the_profile() {
        let f = fixture(Some(REGISTRY));
        let c = candidates(&f);
        let fresh = prune(&c, &gates(&f, SystemTime::now(), Some(&[]), Mode::Quarantine));
        assert!(fresh.quarantined.is_empty(), "fresh profile must be kept: {fresh:?}");
        let home = c[0].1.join("sessions");
        let busy = prune(
            &c,
            &gates(&f, far_future(), Some(std::slice::from_ref(&home)), Mode::Quarantine),
        );
        assert!(busy.quarantined.is_empty(), "profile with a live CODEX_HOME must be kept");
        let unknown = prune(&c, &gates(&f, far_future(), None, Mode::Quarantine));
        assert!(unknown.quarantined.is_empty(), "unknown liveness must keep everything");
        let mut ambient = gates(&f, far_future(), Some(&[]), Mode::Quarantine);
        let homes = [home];
        ambient.ambient = &homes;
        let own = prune(&c, &ambient);
        assert!(own.quarantined.is_empty(), "the daemon's own CODEX_HOME must be kept");
        assert!(f.root.path().join("stray/auth.json").exists());
    }

    #[test]
    #[serial]
    fn session_managed_and_private_session_profiles_are_kept() {
        let f = fixture(Some(REGISTRY));
        let c = candidates(&f);
        fs::write(f.root.path().join("stray").join(SESSION_MARKER_FILE), "{}").unwrap();
        let report = prune(&c, &gates(&f, far_future(), Some(&[]), Mode::Quarantine));
        assert!(report.quarantined.is_empty());
        assert!(report.kept[0].1.contains("session-managed"));
        fs::remove_file(f.root.path().join("stray").join(SESSION_MARKER_FILE)).unwrap();

        let identity = f.root.path().join(".private-sessions/stray");
        fs::create_dir_all(&identity).unwrap();
        fs::write(identity.join("workspace.json"), "{}").unwrap();
        let report = prune(&c, &gates(&f, far_future(), Some(&[]), Mode::Quarantine));
        assert!(report.quarantined.is_empty());
        assert!(report.kept[0].1.contains("private session"));
        assert!(f.root.path().join("stray/auth.json").exists());
    }

    #[cfg(unix)]
    #[test]
    #[serial]
    fn an_unreadable_subtree_keeps_the_profile() {
        use std::os::unix::fs::PermissionsExt;
        let f = fixture(Some(REGISTRY));
        let c = candidates(&f);
        let sealed = f.root.path().join("stray/sessions");
        fs::create_dir_all(&sealed).unwrap();
        fs::set_permissions(&sealed, fs::Permissions::from_mode(0o000)).unwrap();
        if fs::read_dir(&sealed).is_ok() {
            // root reads everything; the gate cannot be exercised as uid 0.
            fs::set_permissions(&sealed, fs::Permissions::from_mode(0o700)).unwrap();
            return;
        }
        let report = prune(&c, &gates(&f, far_future(), Some(&[]), Mode::Quarantine));
        fs::set_permissions(&sealed, fs::Permissions::from_mode(0o700)).unwrap();
        assert!(report.quarantined.is_empty());
        assert!(report.kept[0].1.starts_with("unreadable"));
        assert!(f.root.path().join("stray/auth.json").exists());
    }
}
