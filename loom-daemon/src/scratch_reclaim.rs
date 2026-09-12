//! Reclaim of the daemon's own `/tmp`-shaped per-agent scratch under
//! `<repo_root>/.loom/claude-config/<agent>/tmp` (#7512, item 3).
//!
//! # Why this convention, not bare `/tmp`
//!
//! #7512's curation pass grepped `loom-daemon/src` for a `/tmp/<sweep>*` or
//! pytest-`basetemp` convention the way #5919's `deep_clean` module found for
//! `target/`/`node_modules/`, and found none — the daemon does not itself
//! create sweep-scoped scratch directly under the OS `/tmp`. What it *does*
//! create, and own outright, is `TMPDIR` for every spawned agent session:
//! [`crate::agent_session::spawn`] sets `TMPDIR='<config_dir>/tmp'` where
//! `config_dir` is `<repo_root>/.loom/claude-config/<agent-name>` (see
//! [`crate::terminal`]'s `MUTABLE_DIRS`, which lists `"tmp"` as one of the
//! per-agent directories re-provisioned on every terminal/session init).
//! Every file a Claude Code session writes via its own `$TMPDIR` — which
//! includes exactly the ad hoc scratch a sweep creates mid-run — lands there,
//! not in the shared OS `/tmp`. That directory is therefore the daemon's real,
//! already-established "sweep scratch" convention; this module is what
//! reclaims it, rather than inventing a new naming scheme nobody writes to.
//!
//! The Observed section's "~1.8G of sweep scratch under `/tmp` (`*-review`,
//! `*-check`, `wave-spike`)" most likely came from ad hoc shell commands typed
//! *during* an agent session (a `mktemp -d` or similar) rather than a
//! daemon-tracked path — those are outside the daemon's ownership scope and
//! stay an operator's call, per CLAUDE.md's guard-hooks doctrine of never
//! touching a cache the daemon did not create. This module reclaims what the
//! daemon actually owns and can attribute: agent `TMPDIR` contents.
//!
//! # Safety: age + strict path scoping, no new mechanism for the removal itself
//!
//! [`is_scoped_scratch_path`] refuses anything that is not literally
//! `<repo_root>/.loom/claude-config/<agent>/tmp/...` — never a sibling mutable
//! dir (`projects/`, `session-env/`, ...), never anything outside
//! `.loom/claude-config` at all. [`is_reclaimable_age`] is the only other
//! gate: an entry younger than the configured floor (default 24h) is left
//! alone regardless of size, since a live or recently-finished sweep's scratch
//! is expected to be far younger than that. There is no liveness probe here
//! (unlike the worktree reaper's "is a process still using this?" check) —
//! the age floor is the entire safety boundary, which is why it defaults
//! generously above any plausible single-sweep duration.
//!
//! # Cadence
//!
//! Wired into [`crate::eager_reclaim::run_for`] (#7512) rather than the
//! scheduled 15-minute [`crate::worktree_reaper`] tick — it is new, so there
//! is no existing cadence to preserve, and running it opportunistically
//! alongside the eager disk-pressure trigger is simplest. It has its own
//! process-global, per-repo cooldown ([`DEFAULT_SCRATCH_MIN_INTERVAL_SECS`])
//! so a disk that stays below the floor for many consecutive dispatch ticks
//! does not re-walk `.loom/claude-config/*/tmp` every 60 seconds.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use chrono::{DateTime, Utc};

// ============================================================================
// Constants
// ============================================================================

/// Master on/off env override. Default-on: `0`/`false`/`no`/`off` disables,
/// `1`/`true`/`yes`/`on` force-enables even when config disables it.
pub const SCRATCH_RECLAIM_ENABLE_ENV: &str = "LOOM_SCRATCH_RECLAIM";

/// Env override for the reclaim age floor (hours).
pub const SCRATCH_RECLAIM_MAX_AGE_HOURS_ENV: &str = "LOOM_SCRATCH_RECLAIM_MAX_AGE_HOURS";

/// Env override for the cooldown between passes (seconds).
pub const SCRATCH_RECLAIM_MIN_INTERVAL_ENV: &str = "LOOM_SCRATCH_RECLAIM_MIN_INTERVAL_SECS";

/// Default age floor: 24 hours. Comfortably above any single sweep's
/// wall-clock duration, so a live or just-finished session's `TMPDIR`
/// contents are never in the blast radius.
pub const DEFAULT_SCRATCH_MAX_AGE_HOURS: u64 = 24;

/// Default cooldown between passes: 30 minutes. This is a pure local
/// filesystem walk (no forge/`docker` shell-out), so the cooldown exists only
/// to keep the eager trigger's log line from repeating every 60s while a disk
/// stays below the floor, not to protect a scarce resource.
pub const DEFAULT_SCRATCH_MIN_INTERVAL_SECS: u64 = 1_800;

// ============================================================================
// Config (.loom/config.json → autonomous.worktreeReaper.scratchReclaim)
// ============================================================================

/// The subset of `.loom/config.json →
/// autonomous.worktreeReaper.scratchReclaim` this module consumes. Every
/// field is `Option` so an absent key falls through to the env-var /
/// built-in-default resolution — precedence **env > config > default**,
/// matching [`crate::deep_clean::DeepCleanConfig`] and every other
/// `autonomous.*` surface.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScratchReclaimConfig {
    /// `…scratchReclaim.enabled` (default **true**).
    pub enabled: Option<bool>,
    /// `…scratchReclaim.maxAgeHours` — reclaim entries at least this old.
    pub max_age_hours: Option<u64>,
    /// `…scratchReclaim.minIntervalSecs` — cooldown between passes (a
    /// zero/invalid value drops to `None`).
    pub min_interval_secs: Option<u64>,
}

/// Read `.loom/config.json → autonomous.worktreeReaper.scratchReclaim`,
/// soft-failing every field to `None` (env/default resolution) on a missing
/// file, malformed JSON, or a missing block.
#[must_use]
pub fn read_scratch_reclaim_config(repo_root: &Path) -> ScratchReclaimConfig {
    let effective = crate::config_resolver::resolve_effective_config(repo_root);
    let Some(block) =
        crate::config_resolver::get_path(&effective, "autonomous.worktreeReaper.scratchReclaim")
    else {
        return ScratchReclaimConfig::default();
    };

    ScratchReclaimConfig {
        enabled: block.get("enabled").and_then(serde_json::Value::as_bool),
        max_age_hours: block.get("maxAgeHours").and_then(serde_json::Value::as_u64),
        min_interval_secs: block
            .get("minIntervalSecs")
            .and_then(serde_json::Value::as_u64)
            .filter(|&s| s > 0),
    }
}

/// Resolve whether the pass runs — precedence **env > config > default(true)**.
#[must_use]
pub fn resolve_enabled(config: &ScratchReclaimConfig) -> bool {
    if let Ok(v) = std::env::var(SCRATCH_RECLAIM_ENABLE_ENV) {
        return matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on");
    }
    config.enabled.unwrap_or(true)
}

/// Resolve the age floor (hours) — precedence **env > config > default**.
#[must_use]
pub fn resolve_max_age_hours(config: &ScratchReclaimConfig) -> u64 {
    std::env::var(SCRATCH_RECLAIM_MAX_AGE_HOURS_ENV)
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .or(config.max_age_hours)
        .unwrap_or(DEFAULT_SCRATCH_MAX_AGE_HOURS)
}

/// Resolve the cooldown (seconds) — precedence **env > config > default**. A
/// zero or unparseable env value falls through rather than disabling the
/// anti-thrash gate.
#[must_use]
pub fn resolve_min_interval_secs(config: &ScratchReclaimConfig) -> u64 {
    std::env::var(SCRATCH_RECLAIM_MIN_INTERVAL_ENV)
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|&s| s > 0)
        .or(config.min_interval_secs)
        .unwrap_or(DEFAULT_SCRATCH_MIN_INTERVAL_SECS)
}

// ============================================================================
// Pure gates
// ============================================================================

/// Whether an entry whose modification time is `age_secs` old is eligible for
/// removal at the `max_age_secs` floor. Pure so the age gate is unit-testable
/// in compressed time — no real 24-hour-old file needed.
///
/// A negative `age_secs` (clock skew, or a file whose mtime is in the future)
/// is never reclaimable — `max_age_secs` is floored at `0` so a misconfigured
/// `0`-hour floor cannot turn this into "reclaim everything including brand
/// new files" via a negative comparison quirk.
#[must_use]
pub fn is_reclaimable_age(age_secs: i64, max_age_secs: i64) -> bool {
    age_secs >= 0 && age_secs >= max_age_secs.max(0)
}

/// Whether `path` sits inside `<repo_root>/.loom/claude-config/<agent>/tmp/…`
/// — the *only* shape this pass is ever allowed to touch (#7512). Refuses a
/// sibling mutable dir under the same agent (`projects/`, `session-env/`,
/// `shell-snapshots/`, …), the agent directory itself, and anything outside
/// `.loom/claude-config` entirely — including a bare OS `/tmp` path, which
/// this module never scans in the first place (see module docs for why).
#[must_use]
pub fn is_scoped_scratch_path(repo_root: &Path, path: &Path) -> bool {
    let base = repo_root.join(".loom").join("claude-config");
    let Ok(rel) = path.strip_prefix(&base) else {
        return false;
    };
    let mut components = rel.components();
    // First component: the agent name (must be present, any name).
    if components.next().is_none() {
        return false;
    }
    // Second component must be literally "tmp".
    matches!(components.next(), Some(std::path::Component::Normal(c)) if c == "tmp")
}

// ============================================================================
// I/O
// ============================================================================

/// Enumerate every `<repo_root>/.loom/claude-config/<agent>/tmp` directory
/// that currently exists. Missing `.loom/claude-config` (no agents ever
/// spawned here) resolves to an empty list, not an error.
fn candidate_tmp_dirs(repo_root: &Path) -> Vec<PathBuf> {
    let base = repo_root.join(".loom").join("claude-config");
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(&base) else {
        return out;
    };
    for entry in entries.flatten() {
        let agent_dir = entry.path();
        if agent_dir.is_dir() {
            let tmp = agent_dir.join("tmp");
            if tmp.is_dir() {
                out.push(tmp);
            }
        }
    }
    out
}

/// Total size in bytes of everything under `path`, recursively. A read
/// failure at any level contributes `0` for that subtree rather than
/// propagating an error — this is an informational figure for the log line,
/// not a removal decision.
fn dir_size_bytes(path: &Path) -> u64 {
    let mut total = 0u64;
    let Ok(entries) = std::fs::read_dir(path) else {
        return total;
    };
    for entry in entries.flatten() {
        if let Ok(ft) = entry.file_type() {
            if ft.is_dir() {
                total += dir_size_bytes(&entry.path());
            } else if let Ok(meta) = entry.metadata() {
                total += meta.len();
            }
        }
    }
    total
}

/// Age of `path` in seconds relative to `now`, or `None` if its metadata /
/// modification time is unreadable (treated as "not reclaimable", never as
/// "infinitely old").
fn entry_age_secs(path: &Path, now: DateTime<Utc>) -> Option<i64> {
    let modified: DateTime<Utc> = std::fs::metadata(path).ok()?.modified().ok()?.into();
    Some((now - modified).num_seconds())
}

/// Sweep every candidate `tmp/` directory's top-level entries, removing every
/// one at least `max_age_secs` old, injected with `now` so this is testable
/// in compressed time. Returns `(entries_removed, bytes_freed)`.
///
/// Every candidate this walks is, by construction of [`candidate_tmp_dirs`],
/// already `<repo_root>/.loom/claude-config/<agent>/tmp/<entry>` — the
/// [`is_scoped_scratch_path`] check below is a defensive re-assertion of that
/// invariant, not the only thing enforcing it, so a future refactor of the
/// enumeration cannot silently widen the blast radius without this also
/// tripping.
fn sweep_scratch(repo_root: &Path, max_age_secs: i64, now: DateTime<Utc>) -> (usize, u64) {
    let mut removed_count = 0usize;
    let mut removed_bytes = 0u64;
    for tmp_dir in candidate_tmp_dirs(repo_root) {
        let Ok(entries) = std::fs::read_dir(&tmp_dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if !is_scoped_scratch_path(repo_root, &path) {
                continue;
            }
            let Some(age_secs) = entry_age_secs(&path, now) else {
                continue;
            };
            if !is_reclaimable_age(age_secs, max_age_secs) {
                continue;
            }
            let is_dir = path.is_dir();
            let size = if is_dir {
                dir_size_bytes(&path)
            } else {
                std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0)
            };
            let result = if is_dir {
                std::fs::remove_dir_all(&path)
            } else {
                std::fs::remove_file(&path)
            };
            match result {
                Ok(()) => {
                    removed_count += 1;
                    removed_bytes += size;
                }
                Err(e) => {
                    log::debug!("scratch_reclaim: could not remove {}: {e}", path.display());
                }
            }
        }
    }
    (removed_count, removed_bytes)
}

// ============================================================================
// The pass
// ============================================================================

/// One scratch-reclaim pass's outcome.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScratchReclaimReport {
    /// The repo whose `.loom/claude-config/*/tmp` was evaluated.
    pub repo_root: PathBuf,
    /// Whether the pass was enabled for this evaluation.
    pub enabled: bool,
    /// How many entries were removed.
    pub removed_count: usize,
    /// Total bytes freed.
    pub removed_bytes: u64,
    /// Present when the cooldown skipped this evaluation.
    pub deferred: Option<String>,
    /// When this evaluation ran.
    pub at: DateTime<Utc>,
}

impl ScratchReclaimReport {
    /// Human-readable freed size, e.g. `"1.8G"` or `"nothing"`.
    #[must_use]
    pub fn removed_human(&self) -> String {
        if self.removed_count == 0 {
            return "nothing".to_string();
        }
        human_size(self.removed_bytes)
    }
}

fn human_size(bytes: u64) -> String {
    if bytes >= 1024 * 1024 * 1024 {
        format!("{:.1}G", bytes as f64 / (1024.0 * 1024.0 * 1024.0))
    } else if bytes >= 1024 * 1024 {
        format!("{:.1}M", bytes as f64 / (1024.0 * 1024.0))
    } else if bytes >= 1024 {
        format!("{:.1}K", bytes as f64 / 1024.0)
    } else {
        format!("{bytes}B")
    }
}

/// Log one pass's outcome. A pass that actually removed something logs at
/// `info` (an operator should see multi-hundred-MB scratch reclaims); a
/// no-op, disabled, or cooldown-skipped pass logs at `debug`.
pub fn log_report(report: &ScratchReclaimReport) {
    if !report.enabled {
        log::debug!(
            "scratch_reclaim: {} disabled (autonomous.worktreeReaper.scratchReclaim.enabled=false \
             or LOOM_SCRATCH_RECLAIM unset-falsy)",
            report.repo_root.display()
        );
        return;
    }
    if let Some(reason) = &report.deferred {
        log::debug!("scratch_reclaim: {} skipped: {reason}", report.repo_root.display());
        return;
    }
    if report.removed_count > 0 {
        log::info!(
            "scratch_reclaim: {} removed {} stale entr{} ({}) from .loom/claude-config/*/tmp",
            report.repo_root.display(),
            report.removed_count,
            if report.removed_count == 1 {
                "y"
            } else {
                "ies"
            },
            report.removed_human()
        );
    } else {
        log::debug!("scratch_reclaim: {} nothing to reclaim", report.repo_root.display());
    }
}

// ============================================================================
// Process-global per-repo cooldown state
// ============================================================================

static LAST_RUN_AT: OnceLock<Mutex<BTreeMap<PathBuf, DateTime<Utc>>>> = OnceLock::new();

fn last_run_slot() -> &'static Mutex<BTreeMap<PathBuf, DateTime<Utc>>> {
    LAST_RUN_AT.get_or_init(|| Mutex::new(BTreeMap::new()))
}

fn last_run_at(repo_root: &Path) -> Option<DateTime<Utc>> {
    last_run_slot()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(repo_root)
        .copied()
}

fn record_run(repo_root: &Path, now: DateTime<Utc>) {
    last_run_slot()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(repo_root.to_path_buf(), now);
}

/// Drop cooldown state. Test-only seam (the process-global would otherwise
/// leak between `#[serial]` tests in the same binary).
#[doc(hidden)]
pub fn reset_state_for_test() {
    last_run_slot()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clear();
}

/// Run one production scratch-reclaim pass for `repo_root`, honoring its own
/// cooldown, and log the result.
#[must_use]
pub fn run_for(repo_root: &Path) -> ScratchReclaimReport {
    let config = read_scratch_reclaim_config(repo_root);
    let enabled = resolve_enabled(&config);
    let now = Utc::now();

    if !enabled {
        let report = ScratchReclaimReport {
            repo_root: repo_root.to_path_buf(),
            enabled: false,
            removed_count: 0,
            removed_bytes: 0,
            deferred: None,
            at: now,
        };
        log_report(&report);
        return report;
    }

    let min_interval_secs = resolve_min_interval_secs(&config);
    if let Some(last) = last_run_at(repo_root) {
        let since_secs = (now - last).num_seconds();
        if since_secs >= 0 && since_secs < i64::try_from(min_interval_secs).unwrap_or(i64::MAX) {
            let report = ScratchReclaimReport {
                repo_root: repo_root.to_path_buf(),
                enabled: true,
                removed_count: 0,
                removed_bytes: 0,
                deferred: Some(format!(
                    "a pass ran {since_secs}s ago (cooldown {min_interval_secs}s)"
                )),
                at: now,
            };
            log_report(&report);
            return report;
        }
    }

    let max_age_secs =
        i64::try_from(resolve_max_age_hours(&config).saturating_mul(3600)).unwrap_or(i64::MAX);
    let (removed_count, removed_bytes) = sweep_scratch(repo_root, max_age_secs, now);
    record_run(repo_root, now);

    let report = ScratchReclaimReport {
        repo_root: repo_root.to_path_buf(),
        enabled: true,
        removed_count,
        removed_bytes,
        deferred: None,
        at: now,
    };
    log_report(&report);
    report
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use serial_test::serial;

    // ===================================================================
    // is_reclaimable_age
    // ===================================================================

    #[test]
    fn test_is_reclaimable_age_below_floor_kept() {
        assert!(!is_reclaimable_age(3600, 86_400)); // 1h old, 24h floor
    }

    #[test]
    fn test_is_reclaimable_age_at_floor_reclaimed() {
        assert!(is_reclaimable_age(86_400, 86_400));
    }

    #[test]
    fn test_is_reclaimable_age_above_floor_reclaimed() {
        assert!(is_reclaimable_age(200_000, 86_400));
    }

    #[test]
    fn test_is_reclaimable_age_negative_never_reclaimed() {
        // Clock skew / future mtime — never reclaim, regardless of floor.
        assert!(!is_reclaimable_age(-10, 0));
    }

    // ===================================================================
    // is_scoped_scratch_path
    // ===================================================================

    #[test]
    fn test_is_scoped_scratch_path_direct_child_of_tmp() {
        let repo = Path::new("/repo");
        let path = Path::new("/repo/.loom/claude-config/builder-1/tmp/scratch-file");
        assert!(is_scoped_scratch_path(repo, path));
    }

    #[test]
    fn test_is_scoped_scratch_path_nested_under_tmp() {
        let repo = Path::new("/repo");
        let path = Path::new("/repo/.loom/claude-config/builder-1/tmp/nested/deep/file");
        assert!(is_scoped_scratch_path(repo, path));
    }

    #[test]
    fn test_is_scoped_scratch_path_rejects_sibling_mutable_dir() {
        let repo = Path::new("/repo");
        let path = Path::new("/repo/.loom/claude-config/builder-1/projects/foo");
        assert!(!is_scoped_scratch_path(repo, path));
    }

    #[test]
    fn test_is_scoped_scratch_path_rejects_agent_dir_itself() {
        let repo = Path::new("/repo");
        let path = Path::new("/repo/.loom/claude-config/builder-1");
        assert!(!is_scoped_scratch_path(repo, path));
    }

    #[test]
    fn test_is_scoped_scratch_path_rejects_outside_claude_config() {
        let repo = Path::new("/repo");
        assert!(!is_scoped_scratch_path(repo, Path::new("/repo/.loom/worktrees/issue-1")));
        assert!(!is_scoped_scratch_path(repo, Path::new("/tmp/some-scratch-dir")));
        assert!(!is_scoped_scratch_path(
            repo,
            Path::new("/other-repo/.loom/claude-config/a/tmp/f")
        ));
    }

    // ===================================================================
    // sweep_scratch — real filesystem, injected `now` for compressed time
    // ===================================================================

    #[test]
    fn test_sweep_scratch_removes_old_entries_keeps_young_ones() {
        let tmp = tempfile::tempdir().unwrap();
        let repo_root = tmp.path();
        let agent_tmp = repo_root.join(".loom/claude-config/builder-1/tmp");
        std::fs::create_dir_all(&agent_tmp).unwrap();
        std::fs::write(agent_tmp.join("stale.log"), b"stale content").unwrap();
        std::fs::write(agent_tmp.join("fresh.log"), b"fresh content").unwrap();

        // "now" is far enough in the future that both files' real (just-now)
        // mtimes look 48h old relative to it; the fresh one is written with a
        // "now" only seconds ahead so it looks brand new.
        let far_future = Utc::now() + chrono::Duration::hours(48);
        let (removed, bytes) =
            sweep_scratch(repo_root, /* max_age_secs = */ 24 * 3600, far_future);
        assert_eq!(removed, 2, "both entries are >= 48h old relative to far_future");
        assert!(bytes > 0);
        assert!(!agent_tmp.join("stale.log").exists());
        assert!(!agent_tmp.join("fresh.log").exists());

        // Re-create and sweep with "now" == actual now: both are seconds old,
        // well under the 24h floor, so nothing is removed.
        std::fs::write(agent_tmp.join("stale.log"), b"stale content").unwrap();
        let (removed2, bytes2) = sweep_scratch(repo_root, 24 * 3600, Utc::now());
        assert_eq!(removed2, 0);
        assert_eq!(bytes2, 0);
        assert!(agent_tmp.join("stale.log").exists());
    }

    #[test]
    fn test_sweep_scratch_never_touches_sibling_mutable_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let repo_root = tmp.path();
        let agent_dir = repo_root.join(".loom/claude-config/builder-1");
        let tmp_dir = agent_dir.join("tmp");
        let projects_dir = agent_dir.join("projects");
        std::fs::create_dir_all(&tmp_dir).unwrap();
        std::fs::create_dir_all(&projects_dir).unwrap();
        std::fs::write(tmp_dir.join("scratch"), b"x").unwrap();
        std::fs::write(projects_dir.join("state.json"), b"{}").unwrap();

        let far_future = Utc::now() + chrono::Duration::hours(48);
        let (removed, _bytes) = sweep_scratch(repo_root, 24 * 3600, far_future);
        assert_eq!(removed, 1, "only the tmp/ entry is ever a candidate");
        assert!(!tmp_dir.join("scratch").exists());
        assert!(projects_dir.join("state.json").exists(), "sibling mutable dir is untouched");
    }

    #[test]
    fn test_sweep_scratch_missing_claude_config_is_a_noop() {
        let tmp = tempfile::tempdir().unwrap();
        let (removed, bytes) = sweep_scratch(tmp.path(), 24 * 3600, Utc::now());
        assert_eq!(removed, 0);
        assert_eq!(bytes, 0);
    }

    // ===================================================================
    // Config resolution
    // ===================================================================

    #[test]
    fn test_resolve_defaults() {
        let config = ScratchReclaimConfig::default();
        assert!(resolve_enabled(&config));
        assert_eq!(resolve_max_age_hours(&config), DEFAULT_SCRATCH_MAX_AGE_HOURS);
        assert_eq!(resolve_min_interval_secs(&config), DEFAULT_SCRATCH_MIN_INTERVAL_SECS);
    }

    #[test]
    fn test_resolve_config_overrides_defaults() {
        let config = ScratchReclaimConfig {
            enabled: Some(false),
            max_age_hours: Some(6),
            min_interval_secs: Some(60),
        };
        assert!(!resolve_enabled(&config));
        assert_eq!(resolve_max_age_hours(&config), 6);
        assert_eq!(resolve_min_interval_secs(&config), 60);
    }

    // ===================================================================
    // run_for — cooldown, end to end
    // ===================================================================

    #[test]
    #[serial]
    fn test_run_for_disabled_removes_nothing() {
        reset_state_for_test();
        std::env::set_var(SCRATCH_RECLAIM_ENABLE_ENV, "0");
        let tmp = tempfile::tempdir().unwrap();
        let report = run_for(tmp.path());
        std::env::remove_var(SCRATCH_RECLAIM_ENABLE_ENV);
        assert!(!report.enabled);
        assert_eq!(report.removed_count, 0);
    }

    #[test]
    #[serial]
    fn test_run_for_respects_its_own_cooldown() {
        reset_state_for_test();
        let tmp = tempfile::tempdir().unwrap();
        let repo_root = tmp.path();
        let agent_tmp = repo_root.join(".loom/claude-config/builder-1/tmp");
        std::fs::create_dir_all(&agent_tmp).unwrap();
        std::fs::write(agent_tmp.join("scratch"), b"x").unwrap();

        // Force everything to look ancient by setting the age floor to 0h —
        // any mtime, however fresh, is >= a 0-second floor.
        std::env::set_var(SCRATCH_RECLAIM_MAX_AGE_HOURS_ENV, "0");
        let first = run_for(repo_root);
        assert_eq!(first.removed_count, 1, "first call past cooldown reclaims the entry");

        // Recreate the entry and call again immediately: the cooldown must
        // suppress a second pass even though a fresh candidate exists.
        std::fs::write(agent_tmp.join("scratch2"), b"y").unwrap();
        let second = run_for(repo_root);
        assert_eq!(second.removed_count, 0, "cooldown suppresses the second call");
        assert!(second.deferred.is_some());
        assert!(agent_tmp.join("scratch2").exists(), "cooldown-skipped entry is untouched");

        std::env::remove_var(SCRATCH_RECLAIM_MAX_AGE_HOURS_ENV);
    }
}
