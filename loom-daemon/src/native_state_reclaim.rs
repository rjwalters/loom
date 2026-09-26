//! Reclaim of guarded-native-harness host state that nothing else removes
//! (#8650): the periodic tick side of [`crate::native_tools::provision::reap`]
//! plus the pinned `TMPDIR` companion directories that live outside it.
//!
//! # Two directories, two owners
//!
//! Every guarded native harness launch (pi / opencode / kimi) gets a fresh
//! UUID-named session directory under
//! `~/.local/state/loom/native-tools/<workspace-hash>/<launch-uuid>` (or under
//! `$LOOM_NATIVE_TOOLS_DIR`) — [`crate::native_tools::provision::state`]. That
//! directory's own reclaim is [`crate::native_tools::provision::reap`]'s job
//! (#8663): it knows how to tell a live launch from a dead one (same-host pid
//! liveness, or age when that is unanswerable) and already runs at the start
//! of the next launch for the same workspace, plus the operator-driven
//! `loom-daemon clean`. Both of those are launch-triggered, though, so a
//! workspace that stops launching new sessions never gets swept again. This
//! module's first job is closing that gap: a periodic call into
//! [`crate::native_tools::provision::reap::reap_base`] on the host-level
//! reaper tick, so an idle workspace's stale sessions still age out. **This is
//! deliberately the *only* extra reclaim logic for that directory shape** — an
//! earlier revision of this module re-implemented its own age-only sweep of
//! the same directories, which raced #8705's liveness-aware reap: it would
//! delete a live, idle session's state that #8705 deliberately keeps, and nothing
//! stopped both landing in the same tick (#8693 review). One directory shape,
//! one reaper.
//!
//! The second directory this module owns end-to-end is the pinned `TMPDIR`
//! companion ([`pinned_tmp_base`], below) — a separate, short-path root that
//! [`crate::native_tools::provision::state::State`] points every guarded
//! launch's `TMPDIR` at, so a `bun --compile` harness's (OpenCode's) ~5.5 MB
//! embedded-native-addon extract lands somewhere Loom can attribute and remove
//! rather than in the shared OS `/tmp` under an anonymous name. One worker
//! accumulated **7.6 GB across 1,382 files in 40 hours** of scheduled
//! role-runner ticks this way, contributing to a live ENOSPC outage
//! (2AMLogic/2am#883).
//!
//! # Why the pinned `TMPDIR` is a separate, short-path root
//!
//! The obvious place to pin `TMPDIR` would be inside the launch's own session
//! directory (e.g. `<session>/tmp`) — the first version of this fix did
//! exactly that. It does not work: the full path,
//! `<home>/.local/state/loom/native-tools/<64-hex-sha256>/<launch-uuid>/tmp`,
//! runs to roughly 148 bytes on a typical host, and `sockaddr_un.sun_path` is
//! limited to 108 bytes on Linux and 104 on macOS. `TMPDIR` is inherited by
//! everything the harness runs, including `cargo test` binding a
//! `UnixListener` under a tempdir, Python multiprocessing, and Node IPC — all
//! of which fail to bind a socket once the inherited `TMPDIR` alone is that
//! long (#8693 review). [`pinned_tmp_base`] fixes the length at a constant,
//! short root (`/tmp/loom-nt` on Unix) regardless of `$HOME`'s length, leaving
//! comfortable headroom for a socket file name under either limit.
//!
//! Splitting the pinned `TMPDIR` out from the session directory means it needs
//! its own reclaim ([`sweep_pinned_tmp`]) — but not its own liveness or age
//! policy. A pinned `TMPDIR` entry can only exist for a launch whose session
//! directory was created first ([`crate::native_tools::provision::state::State::configure`]
//! runs after the session directory is provisioned), so once that session
//! directory is gone, its `TMPDIR` companion is unconditionally orphaned: no
//! separate age floor is needed; the companion is simply removed as soon as
//! its owner disappears. This sidesteps the age-floor-of-zero footgun the
//! first version of this fix had ([`crate::native_tools::provision::reap`]'s
//! own liveness check on the *session* directory is what actually protects a
//! live launch — an age gate on the companion could never see that liveness
//! signal on its own).
//!
//! # Cadence
//!
//! A host-level sibling pass on [`crate::worktree_reaper::reap_repo`]'s
//! 15-minute tick, alongside [`crate::docker_image_clean`] and
//! [`crate::tmpfs_reclaim`] — both directories this module reclaims live
//! outside every repo, so there is no per-repo cadence to ride. The cooldown
//! is host-wide for the same reason: a multi-repo host must not re-walk the
//! same bases once per registered repo per tick.

use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use chrono::{DateTime, Utc};

// ============================================================================
// Constants
// ============================================================================

/// Master on/off env override. Default-on: `0`/`false`/`no`/`off` disables,
/// `1`/`true`/`yes`/`on` force-enables even when config disables it.
pub const NATIVE_STATE_RECLAIM_ENABLE_ENV: &str = "LOOM_NATIVE_STATE_RECLAIM";

/// Env override for the host-wide cooldown between passes (seconds).
pub const NATIVE_STATE_RECLAIM_MIN_INTERVAL_ENV: &str =
    "LOOM_NATIVE_STATE_RECLAIM_MIN_INTERVAL_SECS";

/// The same env override `native_tools::provision::state::prepare` honors when
/// choosing where to allocate per-launch session state, read here so a host
/// that relocates that base still gets it swept.
pub const NATIVE_TOOLS_DIR_ENV: &str = "LOOM_NATIVE_TOOLS_DIR";

/// Env override for [`pinned_tmp_base`], test-only isolation seam (production
/// hosts use the fixed short default).
pub const NATIVE_PINNED_TMPDIR_BASE_ENV: &str = "LOOM_NATIVE_PINNED_TMPDIR_BASE";

/// Default host-wide cooldown between passes: 30 minutes, matching
/// [`crate::tmpfs_reclaim`] and [`crate::docker_image_clean`]. This is a local
/// filesystem walk with no shell-out, so the cooldown only keeps a multi-repo
/// host from re-walking the bases once per repo per reaper tick.
pub const DEFAULT_NATIVE_STATE_MIN_INTERVAL_SECS: u64 = 1_800;

/// Fixed leaf name for [`pinned_tmp_base`]'s default. Short and constant so
/// the pinned `TMPDIR` path length never depends on `$HOME`.
const PINNED_TMPDIR_LEAF: &str = "loom-nt";

// ============================================================================
// Config (.loom/config.json → autonomous.worktreeReaper.nativeStateReclaim)
// ============================================================================

/// The subset of `.loom/config.json →
/// autonomous.worktreeReaper.nativeStateReclaim` this module consumes. Every
/// field is `Option` so an absent key falls through to the env-var /
/// built-in-default resolution — precedence **env > config > default**,
/// matching every other `autonomous.*` surface.
///
/// There is deliberately no age-floor knob here (an earlier revision had
/// `maxAgeHours`): both directories this module reclaims now derive their
/// staleness from [`crate::native_tools::provision::reap`]'s own liveness
/// check rather than a standalone age policy — see the module docs.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NativeStateReclaimConfig {
    /// `…nativeStateReclaim.enabled` (default **true**).
    pub enabled: Option<bool>,
    /// `…nativeStateReclaim.minIntervalSecs` — host-wide cooldown between
    /// passes (a zero/invalid value drops to `None`).
    pub min_interval_secs: Option<u64>,
}

/// Read `.loom/config.json → autonomous.worktreeReaper.nativeStateReclaim`,
/// soft-failing every field to `None` (env/default resolution) on a missing
/// file, malformed JSON, or a missing block.
#[must_use]
pub fn read_native_state_reclaim_config(repo_root: &Path) -> NativeStateReclaimConfig {
    let effective = crate::config_resolver::resolve_effective_config(repo_root);
    let Some(block) = crate::config_resolver::get_path(
        &effective,
        "autonomous.worktreeReaper.nativeStateReclaim",
    ) else {
        return NativeStateReclaimConfig::default();
    };

    NativeStateReclaimConfig {
        enabled: block.get("enabled").and_then(serde_json::Value::as_bool),
        min_interval_secs: block
            .get("minIntervalSecs")
            .and_then(serde_json::Value::as_u64)
            .filter(|&s| s > 0),
    }
}

/// Resolve whether the pass runs — precedence **env > config > default(true)**.
#[must_use]
pub fn resolve_enabled(config: &NativeStateReclaimConfig) -> bool {
    if let Ok(v) = std::env::var(NATIVE_STATE_RECLAIM_ENABLE_ENV) {
        return matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on");
    }
    config.enabled.unwrap_or(true)
}

/// Resolve the cooldown (seconds) — precedence **env > config > default**. A
/// zero or unparseable env value falls through rather than disabling the
/// anti-thrash gate.
#[must_use]
pub fn resolve_min_interval_secs(config: &NativeStateReclaimConfig) -> u64 {
    std::env::var(NATIVE_STATE_RECLAIM_MIN_INTERVAL_ENV)
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|&s| s > 0)
        .or(config.min_interval_secs)
        .unwrap_or(DEFAULT_NATIVE_STATE_MIN_INTERVAL_SECS)
}

// ============================================================================
// Pure gates
// ============================================================================

/// Whether `name` is a per-launch directory: the lowercase hyphenated form of
/// a v4 UUID, as written by `native_tools::provision::state::create`.
#[must_use]
pub fn is_launch_id(name: &str) -> bool {
    let groups: Vec<&str> = name.split('-').collect();
    if groups.len() != 5 {
        return false;
    }
    let widths = [8usize, 4, 4, 4, 12];
    groups.iter().zip(widths).all(|(group, width)| {
        group.len() == width
            && group
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    })
}

// ============================================================================
// I/O — session-directory bases (reclaimed via `reap::reap_base`)
// ============================================================================

/// Every base directory that may hold per-launch session state on this host:
/// `$LOOM_NATIVE_TOOLS_DIR` when set, **replacing** (never adding to) the
/// default `~/.local/state/loom/native-tools` — this deliberately mirrors
/// `native_tools::provision::state::create`'s own override resolution (an
/// explicit `base` replaces the home-derived default there too), since a
/// launch that honors the override never writes under the real default once
/// it is set. It also means a test that overrides `LOOM_NATIVE_TOOLS_DIR` to
/// a private tempdir gets **only** that tempdir swept — never the real,
/// live `~/.local/state/loom/native-tools` on the host running the test.
/// An earlier revision swept both unconditionally, which let a test fixture
/// make every real per-launch directory on the host a sweep candidate and
/// delete them out from under concurrently running harness launches (#8650 —
/// caught before merge). Non-existent candidates are dropped.
#[must_use]
pub fn base_dirs() -> Vec<PathBuf> {
    let candidate = match std::env::var_os(NATIVE_TOOLS_DIR_ENV).filter(|value| !value.is_empty()) {
        Some(value) => PathBuf::from(value),
        None => match dirs::home_dir() {
            Some(home) => home.join(".local/state/loom/native-tools"),
            None => return Vec::new(),
        },
    };
    let resolved = candidate.canonicalize().unwrap_or(candidate);
    if resolved.is_dir() {
        vec![resolved]
    } else {
        Vec::new()
    }
}

// ============================================================================
// I/O — the pinned-TMPDIR companion base (reclaimed here)
// ============================================================================

#[cfg(unix)]
fn platform_tmp_root() -> PathBuf {
    PathBuf::from("/tmp")
}

#[cfg(not(unix))]
fn platform_tmp_root() -> PathBuf {
    std::env::temp_dir()
}

/// Base directory for every guarded launch's pinned `TMPDIR`
/// ([`crate::native_tools::provision::state::State::configure`] joins a
/// launch's own uuid onto this). See the module docs for why this is a fixed
/// short root rather than a path nested inside the launch's session
/// directory.
#[must_use]
pub fn pinned_tmp_base() -> PathBuf {
    std::env::var_os(NATIVE_PINNED_TMPDIR_BASE_ENV)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| platform_tmp_root().join(PINNED_TMPDIR_LEAF))
}

/// Bytes held by everything under `path`. Best effort — an unreadable entry
/// contributes zero rather than aborting a pass whose purpose is to free
/// space.
fn dir_size(path: &Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(path) else {
        return 0;
    };
    let mut total = 0u64;
    for entry in entries.flatten() {
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        if meta.is_symlink() {
            continue;
        }
        total += if meta.is_dir() {
            dir_size(&entry.path())
        } else {
            meta.len()
        };
    }
    total
}

/// Whether a launch's session directory (named `uuid`) still exists under any
/// workspace-hash directory beneath `session_base`. `reap::reap_base`'s own
/// pid-liveness check is what actually decides whether a session directory
/// survives a pass — this only asks whether that decision has already been
/// made, so the pinned-`TMPDIR` companion never needs a liveness or age
/// policy of its own.
fn session_directory_exists(session_base: &Path, uuid: &str) -> bool {
    let Ok(workspaces) = std::fs::read_dir(session_base) else {
        return false;
    };
    workspaces
        .flatten()
        .any(|workspace| workspace.path().join(uuid).is_dir())
}

/// Remove every pinned-`TMPDIR` entry under `pinned_tmp_base` whose owning
/// session directory (searched across `session_bases`) no longer exists.
/// Returns `(directories_removed, bytes_freed)`.
///
/// Fails safe when `session_bases` is empty: with no session base to check
/// against, "the owner is gone" cannot be verified, so nothing is removed
/// rather than guessing every entry is orphaned.
#[must_use]
pub fn sweep_pinned_tmp(pinned_tmp_base: &Path, session_bases: &[PathBuf]) -> (usize, u64) {
    if session_bases.is_empty() {
        return (0, 0);
    }
    let Ok(entries) = std::fs::read_dir(pinned_tmp_base) else {
        return (0, 0);
    };
    let mut removed_count = 0usize;
    let mut removed_bytes = 0u64;
    for entry in entries.flatten() {
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if !is_launch_id(&name) {
            continue;
        }
        if !entry.file_type().is_ok_and(|kind| kind.is_dir()) {
            continue;
        }
        if session_bases
            .iter()
            .any(|base| session_directory_exists(base, &name))
        {
            continue; // the launch's session directory is still present
        }
        let path = entry.path();
        let bytes = dir_size(&path);
        match std::fs::remove_dir_all(&path) {
            Ok(()) => {
                removed_count += 1;
                removed_bytes += bytes;
            }
            Err(error) => {
                log::debug!(
                    "native_state_reclaim: could not remove orphaned pinned tmpdir {}: {error}",
                    path.display()
                );
            }
        }
    }
    (removed_count, removed_bytes)
}

// ============================================================================
// The pass
// ============================================================================

/// One native-state-reclaim pass's outcome.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NativeStateReclaimReport {
    /// The session-state base directories evaluated (empty when none exist).
    pub session_bases: Vec<PathBuf>,
    /// The pinned-tmpdir base directory evaluated.
    pub pinned_tmp_base: PathBuf,
    /// Whether the pass was enabled for this evaluation.
    pub enabled: bool,
    /// Session directories removed by the delegated `reap::reap_base` call.
    pub sessions_removed: usize,
    /// Shared binding trees removed by the delegated `reap::reap_base` call.
    pub bindings_removed: usize,
    /// Orphaned pinned-tmpdir companions removed.
    pub pinned_tmp_removed: usize,
    /// Total bytes freed across every removal above.
    pub removed_bytes: u64,
    /// Present when the host-wide cooldown skipped this evaluation.
    pub deferred: Option<String>,
    /// Diagnostics from the delegated reap pass; a reap failure never fails
    /// the tick that triggered it.
    pub errors: Vec<String>,
    /// When this evaluation ran.
    pub at: DateTime<Utc>,
}

impl NativeStateReclaimReport {
    /// Total directories removed across both reclaimed locations.
    #[must_use]
    pub fn removed_count(&self) -> usize {
        self.sessions_removed + self.bindings_removed + self.pinned_tmp_removed
    }

    /// Human-readable freed size, e.g. `"7.6G"` or `"nothing"`.
    #[must_use]
    pub fn removed_human(&self) -> String {
        if self.removed_bytes == 0 {
            return "nothing".to_string();
        }
        crate::tmpfs_reclaim::human_size(self.removed_bytes)
    }
}

/// Log one pass's outcome. A pass that removed something logs at `info` (an
/// operator chasing disk growth must see multi-GB reclaims); a no-op,
/// disabled, or cooldown-skipped pass logs at `debug`.
pub fn log_report(report: &NativeStateReclaimReport) {
    if !report.enabled {
        log::debug!(
            "native_state_reclaim: disabled \
             (autonomous.worktreeReaper.nativeStateReclaim.enabled=false or \
             LOOM_NATIVE_STATE_RECLAIM unset-falsy)"
        );
        return;
    }
    if let Some(reason) = &report.deferred {
        log::debug!("native_state_reclaim: skipped: {reason}");
        return;
    }
    let total = report.removed_count();
    if total > 0 {
        log::info!(
            "native_state_reclaim: removed {total} stale native-harness director{} ({} \
             session{}, {} binding{}, {} pinned-tmp; {}) from {:?} / {}",
            if total == 1 { "y" } else { "ies" },
            report.sessions_removed,
            if report.sessions_removed == 1 {
                ""
            } else {
                "s"
            },
            report.bindings_removed,
            if report.bindings_removed == 1 {
                ""
            } else {
                "s"
            },
            report.pinned_tmp_removed,
            report.removed_human(),
            report.session_bases,
            report.pinned_tmp_base.display(),
        );
    } else {
        log::debug!(
            "native_state_reclaim: nothing to reclaim from {:?} / {}",
            report.session_bases,
            report.pinned_tmp_base.display(),
        );
    }
    for error in &report.errors {
        log::debug!("native_state_reclaim: {error}");
    }
}

// ============================================================================
// Host-wide cooldown state (mirrors `tmpfs_reclaim`)
// ============================================================================

/// Process-global "when did a pass last actually walk the bases" — host-wide
/// (not per-repo): both directories this module reclaims are keyed outside
/// any one repo, so a host with several registered repos ticking on the same
/// reaper cadence must not re-walk them once per repo.
static LAST_EVALUATED_AT: OnceLock<Mutex<Option<DateTime<Utc>>>> = OnceLock::new();

fn last_evaluated_slot() -> &'static Mutex<Option<DateTime<Utc>>> {
    LAST_EVALUATED_AT.get_or_init(|| Mutex::new(None))
}

#[must_use]
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
    let mut guard = last_evaluated_slot()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    *guard = Some(now);
}

/// Drop cooldown state. Test-only seam (the process-global would otherwise
/// leak between `#[serial]` tests in the same binary).
#[doc(hidden)]
pub fn reset_state_for_test() {
    let mut guard = last_evaluated_slot()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    *guard = None;
}

/// Run one production native-state-reclaim pass, honoring the host-wide
/// cooldown, and log the result. `repo_root` selects which repo's
/// `.loom/config.json` supplies the knobs (the pass itself is host-scoped).
pub fn run_for(repo_root: &Path) -> NativeStateReclaimReport {
    let config = read_native_state_reclaim_config(repo_root);
    let enabled = resolve_enabled(&config);
    let now = Utc::now();
    let pinned_tmp_base = pinned_tmp_base();

    if !enabled {
        let report = NativeStateReclaimReport {
            pinned_tmp_base,
            enabled: false,
            at: now,
            ..Default::default()
        };
        log_report(&report);
        return report;
    }

    let min_interval_secs = resolve_min_interval_secs(&config);
    if !cooldown_elapsed(now, min_interval_secs) {
        let report = NativeStateReclaimReport {
            pinned_tmp_base,
            enabled: true,
            deferred: Some(format!("a pass ran inside the {min_interval_secs}s cooldown")),
            at: now,
            ..Default::default()
        };
        log_report(&report);
        return report;
    }

    let session_bases = base_dirs();
    let policy = crate::native_tools::provision::reap::Policy::default();
    let mut sessions_removed = 0usize;
    let mut bindings_removed = 0usize;
    let mut removed_bytes = 0u64;
    let mut errors = Vec::new();
    for base in &session_bases {
        let reap_report = crate::native_tools::provision::reap::reap_base(base, &policy, false);
        sessions_removed += reap_report.sessions.len();
        bindings_removed += reap_report.bindings.len();
        removed_bytes += reap_report.bytes;
        errors.extend(reap_report.errors);
    }

    let (pinned_tmp_removed, pinned_tmp_bytes) = sweep_pinned_tmp(&pinned_tmp_base, &session_bases);
    removed_bytes += pinned_tmp_bytes;

    record_evaluated(now);

    let report = NativeStateReclaimReport {
        session_bases,
        pinned_tmp_base,
        enabled: true,
        sessions_removed,
        bindings_removed,
        pinned_tmp_removed,
        removed_bytes,
        deferred: None,
        errors,
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

    const LAUNCH: &str = "3f2504e0-4f89-41d3-9a0c-0305e82c3301";
    const HASH: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    // ===================================================================
    // Shape gate
    // ===================================================================

    #[test]
    fn launch_id_shape_is_recognized() {
        assert!(is_launch_id(LAUNCH));
        assert!(is_launch_id(&uuid::Uuid::new_v4().to_string()));
    }

    #[test]
    fn foreign_names_are_not_launch_ids() {
        assert!(!is_launch_id("not-a-uuid"));
        assert!(!is_launch_id(&LAUNCH.to_uppercase()));
        assert!(!is_launch_id(&LAUNCH.replace('-', "")));
    }

    // ===================================================================
    // pinned_tmp_base — short, constant-length regardless of overrides
    // ===================================================================

    #[test]
    #[serial(native_state_reclaim_env)]
    fn pinned_tmp_base_default_leaves_room_for_a_socket_name() {
        std::env::remove_var(NATIVE_PINNED_TMPDIR_BASE_ENV);
        let path = pinned_tmp_base().join(LAUNCH);
        // 108 bytes is the tightest sockaddr_un.sun_path limit (Linux); leave
        // generous headroom for a socket file name on top of the launch dir.
        assert!(
            path.as_os_str().len() < 90,
            "pinned tmp path {} is too long for a Unix socket path",
            path.display()
        );
    }

    // ===================================================================
    // sweep_pinned_tmp — orphan-by-owner-absence, no age policy
    // ===================================================================

    #[test]
    fn sweep_pinned_tmp_keeps_an_entry_whose_session_directory_still_exists() {
        let temp = tempfile::tempdir().unwrap();
        let session_base = temp.path().join("native-tools");
        let pinned_base = temp.path().join("pinned-tmp");
        std::fs::create_dir_all(session_base.join(HASH).join(LAUNCH)).unwrap();
        std::fs::create_dir_all(pinned_base.join(LAUNCH)).unwrap();

        let (removed, bytes) = sweep_pinned_tmp(&pinned_base, &[session_base]);

        assert_eq!(removed, 0);
        assert_eq!(bytes, 0);
        assert!(pinned_base.join(LAUNCH).is_dir());
    }

    #[test]
    fn sweep_pinned_tmp_removes_an_entry_whose_session_directory_is_gone() {
        let temp = tempfile::tempdir().unwrap();
        let session_base = temp.path().join("native-tools");
        let pinned_base = temp.path().join("pinned-tmp");
        std::fs::create_dir_all(&session_base).unwrap();
        let entry = pinned_base.join(LAUNCH);
        std::fs::create_dir_all(&entry).unwrap();
        std::fs::write(entry.join("extract.node"), vec![0u8; 4096]).unwrap();

        let (removed, bytes) = sweep_pinned_tmp(&pinned_base, &[session_base]);

        assert_eq!(removed, 1);
        assert!(bytes >= 4096);
        assert!(!entry.exists());
    }

    #[test]
    fn sweep_pinned_tmp_ignores_foreign_names_and_files() {
        let temp = tempfile::tempdir().unwrap();
        let session_base = temp.path().join("native-tools");
        let pinned_base = temp.path().join("pinned-tmp");
        std::fs::create_dir_all(&session_base).unwrap();
        std::fs::create_dir_all(pinned_base.join("not-a-uuid")).unwrap();
        std::fs::write(pinned_base.join(LAUNCH), b"not a directory").unwrap();
        std::fs::create_dir_all(&pinned_base).unwrap();

        let (removed, _bytes) = sweep_pinned_tmp(&pinned_base, &[session_base]);

        assert_eq!(removed, 0, "neither a foreign name nor a plain file is a candidate");
        assert!(pinned_base.join("not-a-uuid").exists());
        assert!(pinned_base.join(LAUNCH).exists());
    }

    #[test]
    fn sweep_pinned_tmp_with_no_session_bases_removes_nothing() {
        let temp = tempfile::tempdir().unwrap();
        let pinned_base = temp.path().join("pinned-tmp");
        std::fs::create_dir_all(pinned_base.join(LAUNCH)).unwrap();

        let (removed, bytes) = sweep_pinned_tmp(&pinned_base, &[]);

        assert_eq!(removed, 0, "cannot verify orphan status with no session base to check");
        assert_eq!(bytes, 0);
        assert!(pinned_base.join(LAUNCH).exists());
    }

    #[test]
    fn sweep_pinned_tmp_of_a_missing_base_is_a_noop() {
        let temp = tempfile::tempdir().unwrap();
        let (removed, bytes) =
            sweep_pinned_tmp(&temp.path().join("absent"), &[temp.path().join("native-tools")]);
        assert_eq!(removed, 0);
        assert_eq!(bytes, 0);
    }

    // ===================================================================
    // base_dirs — override REPLACES, never adds to, the real default
    // ===================================================================

    #[test]
    #[serial(native_state_reclaim_env)]
    fn override_replaces_rather_than_adds_to_the_real_default() {
        let temp = tempfile::tempdir().unwrap();
        let base = temp.path().join("native-tools");
        std::fs::create_dir_all(&base).unwrap();

        std::env::set_var(NATIVE_TOOLS_DIR_ENV, &base);
        let bases = base_dirs();
        std::env::remove_var(NATIVE_TOOLS_DIR_ENV);

        assert_eq!(
            bases,
            vec![base.canonicalize().unwrap()],
            "an override must be the ONLY base swept, never appended to the real default"
        );
    }

    // ===================================================================
    // Config resolution
    // ===================================================================

    #[test]
    fn resolve_defaults() {
        let config = NativeStateReclaimConfig::default();
        assert!(resolve_enabled(&config));
        assert_eq!(resolve_min_interval_secs(&config), DEFAULT_NATIVE_STATE_MIN_INTERVAL_SECS);
    }

    #[test]
    fn config_overrides_defaults() {
        let config = NativeStateReclaimConfig {
            enabled: Some(false),
            min_interval_secs: Some(60),
        };
        assert!(!resolve_enabled(&config));
        assert_eq!(resolve_min_interval_secs(&config), 60);
    }

    #[test]
    fn read_config_soft_fails_to_defaults_on_malformed_json() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(temp.path().join(".loom")).unwrap();
        std::fs::write(temp.path().join(".loom/config.json"), "{ not json").unwrap();
        assert_eq!(
            read_native_state_reclaim_config(temp.path()),
            NativeStateReclaimConfig::default()
        );
    }

    // ===================================================================
    // run_for — enable gate, host-wide cooldown, and delegation to `reap`
    // ===================================================================

    #[test]
    #[serial(native_state_reclaim_env)]
    fn run_for_disabled_removes_nothing() {
        reset_state_for_test();
        let temp = tempfile::tempdir().unwrap();
        let session_base = temp.path().join("native-tools");
        let pinned_base = temp.path().join("pinned-tmp");
        std::fs::create_dir_all(&session_base).unwrap();
        let orphan = pinned_base.join(LAUNCH);
        std::fs::create_dir_all(&orphan).unwrap();

        std::env::set_var(NATIVE_STATE_RECLAIM_ENABLE_ENV, "0");
        std::env::set_var(NATIVE_TOOLS_DIR_ENV, &session_base);
        std::env::set_var(NATIVE_PINNED_TMPDIR_BASE_ENV, &pinned_base);
        let report = run_for(temp.path());
        std::env::remove_var(NATIVE_STATE_RECLAIM_ENABLE_ENV);
        std::env::remove_var(NATIVE_TOOLS_DIR_ENV);
        std::env::remove_var(NATIVE_PINNED_TMPDIR_BASE_ENV);
        reset_state_for_test();

        assert!(!report.enabled);
        assert_eq!(report.removed_count(), 0);
        assert!(orphan.exists());
    }

    #[test]
    #[serial(native_state_reclaim_env)]
    fn run_for_reclaims_an_orphaned_pinned_tmpdir_then_defers_inside_the_cooldown() {
        reset_state_for_test();
        let temp = tempfile::tempdir().unwrap();
        let session_base = temp.path().join("native-tools");
        let pinned_base = temp.path().join("pinned-tmp");
        std::fs::create_dir_all(&session_base).unwrap();
        // No matching session directory under session_base -> orphaned.
        let first_orphan = pinned_base.join(LAUNCH);
        std::fs::create_dir_all(&first_orphan).unwrap();

        std::env::set_var(NATIVE_TOOLS_DIR_ENV, &session_base);
        std::env::set_var(NATIVE_PINNED_TMPDIR_BASE_ENV, &pinned_base);

        let first = run_for(temp.path());
        let second_orphan = pinned_base.join("3f2504e0-4f89-41d3-9a0c-0305e82c3302");
        std::fs::create_dir_all(&second_orphan).unwrap();
        let second = run_for(temp.path());

        std::env::remove_var(NATIVE_TOOLS_DIR_ENV);
        std::env::remove_var(NATIVE_PINNED_TMPDIR_BASE_ENV);
        reset_state_for_test();

        assert_eq!(first.pinned_tmp_removed, 1, "the first call past cooldown reclaims");
        assert!(!first_orphan.exists());
        assert_eq!(second.removed_count(), 0, "the cooldown suppresses the second call");
        assert!(second.deferred.is_some());
        assert!(second_orphan.exists(), "a cooldown-skipped pass leaves new orphans untouched");
    }

    #[test]
    #[serial(native_state_reclaim_env)]
    fn run_for_keeps_a_pinned_tmpdir_whose_session_directory_is_still_present() {
        reset_state_for_test();
        let temp = tempfile::tempdir().unwrap();
        let session_base = temp.path().join("native-tools");
        let pinned_base = temp.path().join("pinned-tmp");
        // A live-looking session directory (no liveness record -> reap::reap_base
        // treats it as an orphan on its own age policy, but that is a 6h floor,
        // so a freshly created directory in this test survives either way).
        std::fs::create_dir_all(session_base.join(HASH).join(LAUNCH)).unwrap();
        std::fs::create_dir_all(pinned_base.join(LAUNCH)).unwrap();

        std::env::set_var(NATIVE_TOOLS_DIR_ENV, &session_base);
        std::env::set_var(NATIVE_PINNED_TMPDIR_BASE_ENV, &pinned_base);
        let report = run_for(temp.path());
        std::env::remove_var(NATIVE_TOOLS_DIR_ENV);
        std::env::remove_var(NATIVE_PINNED_TMPDIR_BASE_ENV);
        reset_state_for_test();

        assert_eq!(report.pinned_tmp_removed, 0);
        assert!(pinned_base.join(LAUNCH).is_dir());
    }
}
