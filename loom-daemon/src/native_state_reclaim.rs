//! Reclaim of per-launch guarded-native-harness state under
//! `~/.local/state/loom/native-tools/<workspace-hash>/<launch-uuid>` (#8650).
//!
//! # What was leaking
//!
//! [`crate::native_tools::provision`] allocates a **fresh UUID-named directory
//! per launch** for every guarded native harness (pi / opencode / kimi) and
//! pins the harness's `XDG_*` (and, since #8650, `TMPDIR`) at it, so two
//! concurrent launches can never share mutable state. Nothing has ever removed
//! those directories: `State` has no `Drop`, and there is no process left to
//! run one — `defaults/scripts/spawn-worker.sh` execs `loom-daemon
//! spawn-worker`, which in turn `exec()`s the harness binary itself, replacing
//! the process image. The launch chain is a continuous exec replacement with
//! no surviving parent, so "clean up when the session ends" cannot be
//! in-process here; it has to be a periodic pass over what the launches left
//! behind. That is this module.
//!
//! The volume driver was the OpenCode harness: a `bun --compile` binary
//! extracts its ~5.5 MB embedded native addon into the OS temp directory on
//! every launch. One worker accumulated **7.6 GB across 1,382 files in 40
//! hours** of scheduled role-runner ticks, contributing to a live ENOSPC
//! outage (2AMLogic/2am#883).
//!
//! # Why relocate-then-reclaim, not a `/tmp` pattern sweep
//!
//! The alternative considered in #8650 was to leave `TMPDIR` alone and have
//! the daemon delete `$TMPDIR/.*-0000000[0-9].{so,node}` by filename pattern.
//! That was rejected: the shared OS `/tmp` is a namespace the daemon does not
//! own, and matching by name alone cannot distinguish a stale extract from one
//! a *live* harness (or an unrelated tenant's bun program) is still mapping.
//! `crate::tmpfs_reclaim`'s module docs spell out the same tension, and
//! `crate::scratch_reclaim`'s docs state the rule this follows: the daemon
//! reclaims what it created and can attribute, never a cache it did not.
//!
//! Pinning `TMPDIR` into the launch's own state directory converts the
//! unattributable `/tmp` litter into exactly that — a directory the daemon
//! created, named with the launch UUID it allocated — and it retires the
//! *second*, pre-existing leak in the same move: the per-launch directories
//! themselves, which have been accumulating (small, but unboundedly) since
//! they were introduced.
//!
//! # Safety: shape + age floor, no liveness probe
//!
//! Two gates, both mandatory:
//!
//! 1. [`is_scoped_launch_state_path`] refuses anything that is not literally
//!    `<base>/<64-hex-sha256>/<uuid-v4>` — exactly the two-level shape
//!    `native_tools::provision::state::create` allocates. A stray file, a
//!    hand-made directory, the workspace-hash directory itself, and anything
//!    nested deeper are all left alone.
//! 2. [`is_reclaimable_age`] compares an age floor (default 24h) against the
//!    **newest** mtime found anywhere underneath the launch directory, not the
//!    directory's own mtime — a live harness writing into `state/` or `tmp/`
//!    keeps its whole subtree fresh, while the directory's own mtime would
//!    have frozen at launch time.
//!
//! There is deliberately no process-liveness probe (unlike
//! [`crate::tmpfs_reclaim`], whose orphans have no other evidence available):
//! the age floor is the entire safety boundary, mirroring
//! [`crate::scratch_reclaim`], which is why it defaults far above any
//! plausible harness session's duration.
//!
//! # Cadence
//!
//! A host-level sibling pass on [`crate::worktree_reaper::reap_repo`]'s
//! 15-minute tick, alongside [`crate::docker_image_clean`] and
//! [`crate::tmpfs_reclaim`] — the launch state lives outside every repo and is
//! keyed by workspace hash, so it has no per-repo cadence to ride. Its
//! cooldown is host-wide for the same reason: a multi-repo host must not
//! re-walk the same base directory once per registered repo per tick.

use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use chrono::{DateTime, Utc};

// ============================================================================
// Constants
// ============================================================================

/// Master on/off env override. Default-on: `0`/`false`/`no`/`off` disables,
/// `1`/`true`/`yes`/`on` force-enables even when config disables it.
pub const NATIVE_STATE_RECLAIM_ENABLE_ENV: &str = "LOOM_NATIVE_STATE_RECLAIM";

/// Env override for the reclaim age floor (hours).
pub const NATIVE_STATE_RECLAIM_MAX_AGE_HOURS_ENV: &str = "LOOM_NATIVE_STATE_RECLAIM_MAX_AGE_HOURS";

/// Env override for the host-wide cooldown between passes (seconds).
pub const NATIVE_STATE_RECLAIM_MIN_INTERVAL_ENV: &str =
    "LOOM_NATIVE_STATE_RECLAIM_MIN_INTERVAL_SECS";

/// The same env override `native_tools::provision::state::prepare` honors when
/// choosing where to allocate per-launch state, read here so a host that
/// relocates that base still gets it swept.
pub const NATIVE_TOOLS_DIR_ENV: &str = "LOOM_NATIVE_TOOLS_DIR";

/// Default age floor: 24 hours, matching [`crate::scratch_reclaim`]. Measured
/// against the newest mtime anywhere under a launch directory, so a harness
/// session would have to be completely idle — writing nothing at all, not even
/// through its pinned `TMPDIR` — for a full day before its state is a
/// candidate.
pub const DEFAULT_NATIVE_STATE_MAX_AGE_HOURS: u64 = 24;

/// Default host-wide cooldown between passes: 30 minutes, matching
/// [`crate::tmpfs_reclaim`] and [`crate::docker_image_clean`]. This is a local
/// filesystem walk with no shell-out, so the cooldown only keeps a multi-repo
/// host from re-walking the base once per repo per reaper tick.
pub const DEFAULT_NATIVE_STATE_MIN_INTERVAL_SECS: u64 = 1_800;

/// Path under a launch state directory that `State::configure` pins `TMPDIR`
/// at. Named here so the reclaim side and the launch side cannot drift apart
/// silently.
pub const LAUNCH_TMPDIR_LEAF: &str = "tmp";

// ============================================================================
// Config (.loom/config.json → autonomous.worktreeReaper.nativeStateReclaim)
// ============================================================================

/// The subset of `.loom/config.json →
/// autonomous.worktreeReaper.nativeStateReclaim` this module consumes. Every
/// field is `Option` so an absent key falls through to the env-var /
/// built-in-default resolution — precedence **env > config > default**,
/// matching every other `autonomous.*` surface.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NativeStateReclaimConfig {
    /// `…nativeStateReclaim.enabled` (default **true**).
    pub enabled: Option<bool>,
    /// `…nativeStateReclaim.maxAgeHours` — reclaim launch directories at least
    /// this old.
    pub max_age_hours: Option<u64>,
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
        max_age_hours: block.get("maxAgeHours").and_then(serde_json::Value::as_u64),
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

/// Resolve the age floor (hours) — precedence **env > config > default**.
#[must_use]
pub fn resolve_max_age_hours(config: &NativeStateReclaimConfig) -> u64 {
    std::env::var(NATIVE_STATE_RECLAIM_MAX_AGE_HOURS_ENV)
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .or(config.max_age_hours)
        .unwrap_or(DEFAULT_NATIVE_STATE_MAX_AGE_HOURS)
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

/// Whether an entry whose newest mtime is `age_secs` old is eligible for
/// removal at the `max_age_secs` floor. Pure so the age gate is unit-testable
/// in compressed time — no real 24-hour-old directory needed.
///
/// A negative `age_secs` (clock skew, or an mtime in the future) is never
/// reclaimable, and `max_age_secs` is floored at `0` so a misconfigured
/// negative floor cannot invert the comparison into "reclaim everything".
#[must_use]
pub fn is_reclaimable_age(age_secs: i64, max_age_secs: i64) -> bool {
    age_secs >= 0 && age_secs >= max_age_secs.max(0)
}

/// Whether `name` is a workspace-hash directory: the lowercase hex SHA-256 of
/// the canonical workspace root, as written by
/// `native_tools::provision::state::create`.
#[must_use]
pub fn is_workspace_hash(name: &str) -> bool {
    name.len() == 64
        && name
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

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

/// Whether `path` is literally `<base>/<workspace-hash>/<launch-uuid>` — the
/// *only* shape this pass is ever allowed to remove. Refuses the base itself,
/// a workspace-hash directory (shared across launches, and cheap to keep), a
/// path nested deeper than a launch directory, and any name that does not
/// match what `create` allocates.
#[must_use]
pub fn is_scoped_launch_state_path(base: &Path, path: &Path) -> bool {
    let Ok(rel) = path.strip_prefix(base) else {
        return false;
    };
    let mut components = rel.components();
    let Some(std::path::Component::Normal(hash)) = components.next() else {
        return false;
    };
    let Some(std::path::Component::Normal(launch)) = components.next() else {
        return false;
    };
    if components.next().is_some() {
        return false;
    }
    hash.to_str().is_some_and(is_workspace_hash) && launch.to_str().is_some_and(is_launch_id)
}

// ============================================================================
// I/O
// ============================================================================

/// Every base directory that may hold per-launch native state on this host:
/// `$LOOM_NATIVE_TOOLS_DIR` when set, **replacing** (never adding to) the
/// default `~/.local/state/loom/native-tools` — this deliberately mirrors
/// `native_tools::provision::state::create`'s own override resolution (an
/// explicit `base` replaces the home-derived default there too), since a
/// launch that honors the override never writes under the real default once
/// it is set. It also means a test that overrides `LOOM_NATIVE_TOOLS_DIR` to
/// a private tempdir gets **only** that tempdir swept — never the real,
/// live `~/.local/state/loom/native-tools` on the host running the test.
/// An earlier revision swept both unconditionally, which let a
/// `maxAgeHours=0` test fixture make every real per-launch directory on the
/// host "reclaimable" and delete them out from under concurrently running
/// harness launches (#8650 — caught before merge). Non-existent candidates
/// are dropped.
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

/// Total size in bytes and newest modification time under `path`, walking it
/// recursively without following symlinks. A read failure at any level
/// contributes nothing for that subtree rather than propagating: the size is
/// informational (a log line), and an unreadable subtree must never make a
/// directory look *older* than it is, which is why the newest mtime seen so
/// far is simply carried forward.
fn size_and_newest_mtime(path: &Path) -> (u64, Option<DateTime<Utc>>) {
    fn bump(newest: &mut Option<DateTime<Utc>>, candidate: Option<DateTime<Utc>>) {
        if let Some(candidate) = candidate {
            if newest.is_none() || newest.is_some_and(|current| candidate > current) {
                *newest = Some(candidate);
            }
        }
    }
    fn mtime(meta: &std::fs::Metadata) -> Option<DateTime<Utc>> {
        meta.modified().ok().map(DateTime::<Utc>::from)
    }

    let mut total = 0u64;
    let mut newest: Option<DateTime<Utc>> = None;
    if let Ok(meta) = std::fs::symlink_metadata(path) {
        bump(&mut newest, mtime(&meta));
    }
    let Ok(entries) = std::fs::read_dir(path) else {
        return (total, newest);
    };
    for entry in entries.flatten() {
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        if meta.is_symlink() {
            continue;
        }
        if meta.is_dir() {
            let (bytes, child_newest) = size_and_newest_mtime(&entry.path());
            total += bytes;
            bump(&mut newest, child_newest);
        } else {
            total += meta.len();
            bump(&mut newest, mtime(&meta));
        }
    }
    (total, newest)
}

/// Remove every `<base>/<workspace-hash>/<launch-uuid>` directory whose newest
/// mtime is at least `max_age_secs` old, injected with `now` so this is
/// testable in compressed time. Returns `(directories_removed, bytes_freed)`.
///
/// The [`is_scoped_launch_state_path`] check is applied to every candidate
/// even though the enumeration below only ever produces that shape — a
/// defensive re-assertion, so a future refactor of the walk cannot silently
/// widen the blast radius without this also tripping.
#[must_use]
pub fn sweep(base: &Path, max_age_secs: i64, now: DateTime<Utc>) -> (usize, u64) {
    let mut removed_count = 0usize;
    let mut removed_bytes = 0u64;
    let Ok(workspaces) = std::fs::read_dir(base) else {
        return (removed_count, removed_bytes);
    };
    for workspace in workspaces.flatten() {
        if !workspace
            .file_name()
            .to_str()
            .is_some_and(is_workspace_hash)
        {
            continue;
        }
        let Ok(launches) = std::fs::read_dir(workspace.path()) else {
            continue;
        };
        for launch in launches.flatten() {
            let path = launch.path();
            if !is_scoped_launch_state_path(base, &path) {
                continue;
            }
            if !launch.file_type().is_ok_and(|kind| kind.is_dir()) {
                continue;
            }
            let (bytes, newest) = size_and_newest_mtime(&path);
            // An unreadable mtime is "not reclaimable", never "infinitely old".
            let Some(newest) = newest else {
                continue;
            };
            if !is_reclaimable_age((now - newest).num_seconds(), max_age_secs) {
                continue;
            }
            match std::fs::remove_dir_all(&path) {
                Ok(()) => {
                    removed_count += 1;
                    removed_bytes += bytes;
                }
                Err(error) => {
                    log::debug!(
                        "native_state_reclaim: could not remove {}: {error}",
                        path.display()
                    );
                }
            }
        }
    }
    (removed_count, removed_bytes)
}

// ============================================================================
// The pass
// ============================================================================

/// One native-state-reclaim pass's outcome.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativeStateReclaimReport {
    /// The base directories evaluated (empty when none exist on this host).
    pub bases: Vec<PathBuf>,
    /// Whether the pass was enabled for this evaluation.
    pub enabled: bool,
    /// How many per-launch directories were removed.
    pub removed_count: usize,
    /// Total bytes freed.
    pub removed_bytes: u64,
    /// Present when the host-wide cooldown skipped this evaluation.
    pub deferred: Option<String>,
    /// When this evaluation ran.
    pub at: DateTime<Utc>,
}

impl NativeStateReclaimReport {
    /// Human-readable freed size, e.g. `"7.6G"` or `"nothing"`.
    #[must_use]
    pub fn removed_human(&self) -> String {
        if self.removed_count == 0 {
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
    if report.removed_count > 0 {
        log::info!(
            "native_state_reclaim: removed {} stale per-launch harness state director{} ({}) from {:?}",
            report.removed_count,
            if report.removed_count == 1 { "y" } else { "ies" },
            report.removed_human(),
            report.bases
        );
    } else {
        log::debug!("native_state_reclaim: nothing to reclaim from {:?}", report.bases);
    }
}

// ============================================================================
// Host-wide cooldown state (mirrors `tmpfs_reclaim`)
// ============================================================================

/// Process-global "when did a pass last actually walk the base directories" —
/// host-wide (not per-repo): per-launch native state is keyed by workspace
/// hash under one shared base, so a host with several registered repos ticking
/// on the same reaper cadence must not re-walk it once per repo.
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

    if !enabled {
        let report = NativeStateReclaimReport {
            bases: Vec::new(),
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
    if !cooldown_elapsed(now, min_interval_secs) {
        let report = NativeStateReclaimReport {
            bases: Vec::new(),
            enabled: true,
            removed_count: 0,
            removed_bytes: 0,
            deferred: Some(format!("a pass ran inside the {min_interval_secs}s cooldown")),
            at: now,
        };
        log_report(&report);
        return report;
    }

    let max_age_secs =
        i64::try_from(resolve_max_age_hours(&config).saturating_mul(3600)).unwrap_or(i64::MAX);
    let bases = base_dirs();
    let mut removed_count = 0usize;
    let mut removed_bytes = 0u64;
    for base in &bases {
        let (count, bytes) = sweep(base, max_age_secs, now);
        removed_count += count;
        removed_bytes += bytes;
    }
    record_evaluated(now);

    let report = NativeStateReclaimReport {
        bases,
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

    const HASH: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    const LAUNCH: &str = "3f2504e0-4f89-41d3-9a0c-0305e82c3301";

    /// A per-launch directory shaped exactly like `State::configure` leaves
    /// one, including the pinned `TMPDIR` holding a bun-style native extract.
    fn launch_dir(base: &Path, hash: &str, launch: &str) -> PathBuf {
        let dir = base.join(hash).join(launch);
        std::fs::create_dir_all(dir.join(LAUNCH_TMPDIR_LEAF)).unwrap();
        std::fs::create_dir_all(dir.join("data/opencode")).unwrap();
        std::fs::write(
            dir.join(LAUNCH_TMPDIR_LEAF)
                .join(".abcdef0123456789-00000001.node"),
            vec![0u8; 4096],
        )
        .unwrap();
        dir
    }

    // ===================================================================
    // Shape gate
    // ===================================================================

    #[test]
    fn workspace_hash_and_launch_id_shapes_are_recognized() {
        assert!(is_workspace_hash(HASH));
        assert!(is_launch_id(LAUNCH));
        assert!(is_launch_id(&uuid::Uuid::new_v4().to_string()));
    }

    #[test]
    fn foreign_names_are_not_launch_state() {
        assert!(!is_workspace_hash("short"));
        assert!(!is_workspace_hash(&HASH.to_uppercase()));
        assert!(!is_workspace_hash(&format!("{HASH}0")));
        assert!(!is_launch_id("not-a-uuid"));
        assert!(!is_launch_id(&LAUNCH.to_uppercase()));
        assert!(!is_launch_id(&LAUNCH.replace('-', "")));
    }

    #[test]
    fn only_the_two_level_launch_shape_is_in_scope() {
        let base = Path::new("/state/native-tools");
        assert!(is_scoped_launch_state_path(base, &base.join(HASH).join(LAUNCH)));
        // The workspace-hash directory itself, and anything under a launch.
        assert!(!is_scoped_launch_state_path(base, &base.join(HASH)));
        assert!(!is_scoped_launch_state_path(base, &base.join(HASH).join(LAUNCH).join("tmp")));
        // Foreign names at either level, and anything outside the base.
        assert!(!is_scoped_launch_state_path(base, &base.join("scratch").join(LAUNCH)));
        assert!(!is_scoped_launch_state_path(base, &base.join(HASH).join("credentials")));
        assert!(!is_scoped_launch_state_path(
            base,
            &Path::new("/other/native-tools").join(HASH).join(LAUNCH)
        ));
        assert!(!is_scoped_launch_state_path(base, Path::new("/tmp/anything")));
    }

    // ===================================================================
    // Age gate
    // ===================================================================

    #[test]
    fn age_floor_keeps_young_entries_and_never_trusts_a_future_mtime() {
        assert!(!is_reclaimable_age(3_600, 86_400));
        assert!(is_reclaimable_age(86_400, 86_400));
        assert!(is_reclaimable_age(200_000, 86_400));
        assert!(!is_reclaimable_age(-10, 0));
    }

    // ===================================================================
    // sweep — real filesystem, injected `now` for compressed time
    // ===================================================================

    #[test]
    fn sweep_removes_the_whole_stale_launch_directory_including_the_native_extract() {
        let temp = tempfile::tempdir().unwrap();
        let base = temp.path();
        let dir = launch_dir(base, HASH, LAUNCH);
        let extract = dir
            .join(LAUNCH_TMPDIR_LEAF)
            .join(".abcdef0123456789-00000001.node");
        assert!(extract.exists());

        let far_future = Utc::now() + chrono::Duration::hours(48);
        let (removed, bytes) = sweep(base, 24 * 3600, far_future);

        assert_eq!(removed, 1);
        assert!(bytes >= 4096, "freed bytes must account for the extracted addon");
        assert!(!extract.exists(), "the leaked native extract must be gone");
        assert!(!dir.exists(), "the whole per-launch directory must be gone");
        assert!(base.join(HASH).is_dir(), "the workspace directory itself is kept");
    }

    #[test]
    fn sweep_keeps_a_launch_whose_subtree_was_touched_more_recently_than_its_own_mtime() {
        let temp = tempfile::tempdir().unwrap();
        let base = temp.path();
        let dir = launch_dir(base, HASH, LAUNCH);

        // A launch directory's OWN mtime freezes at launch time — a live
        // harness only keeps writing *underneath* it (session state, and the
        // native extract in its pinned TMPDIR). Separate the two in wall-clock
        // time, then pick a floor that the directory's own mtime would clear
        // but the subtree's newest write does not.
        std::thread::sleep(std::time::Duration::from_millis(1_100));
        let live_write = dir.join(LAUNCH_TMPDIR_LEAF).join(".abcdef-00000002.node");
        std::fs::write(&live_write, vec![0u8; 16]).unwrap();

        let mtime = |path: &Path| -> DateTime<Utc> {
            std::fs::metadata(path).unwrap().modified().unwrap().into()
        };
        let now = mtime(&live_write) + chrono::Duration::seconds(1);
        let floor = (now - mtime(&dir)).num_seconds();
        assert!(
            (now - mtime(&live_write)).num_seconds() < floor,
            "fixture must place the live write inside the floor and the dir mtime outside it"
        );

        let (removed, bytes) = sweep(base, floor, now);
        assert_eq!(removed, 0, "a launch still being written to is never reclaimed");
        assert_eq!(bytes, 0);
        assert!(live_write.exists());
        assert!(dir.exists());
    }

    #[test]
    fn sweep_never_touches_anything_outside_the_launch_shape() {
        let temp = tempfile::tempdir().unwrap();
        let base = temp.path();
        launch_dir(base, HASH, LAUNCH);
        // A non-hash sibling of the workspace directory, a non-uuid sibling of
        // the launch directory, and a stray file directly under the base.
        let foreign_workspace = base.join("credentials");
        std::fs::create_dir_all(foreign_workspace.join(LAUNCH)).unwrap();
        std::fs::write(foreign_workspace.join(LAUNCH).join("auth.json"), b"{}").unwrap();
        let foreign_launch = base.join(HASH).join("shared-cache");
        std::fs::create_dir_all(&foreign_launch).unwrap();
        std::fs::write(foreign_launch.join("blob"), b"x").unwrap();
        std::fs::write(base.join("README"), b"x").unwrap();

        let far_future = Utc::now() + chrono::Duration::hours(48);
        let (removed, _bytes) = sweep(base, 24 * 3600, far_future);

        assert_eq!(removed, 1, "only the launch-shaped directory is a candidate");
        assert!(foreign_workspace.join(LAUNCH).join("auth.json").exists());
        assert!(foreign_launch.join("blob").exists());
        assert!(base.join("README").exists());
    }

    #[test]
    fn sweep_of_a_missing_base_is_a_noop() {
        let temp = tempfile::tempdir().unwrap();
        let (removed, bytes) = sweep(&temp.path().join("absent"), 24 * 3600, Utc::now());
        assert_eq!(removed, 0);
        assert_eq!(bytes, 0);
    }

    // ===================================================================
    // base_dirs — override REPLACES, never adds to, the real default
    // ===================================================================

    /// The override must fully replace the home-derived default, not add to
    /// it. This is the fix for a real incident during development of #8650:
    /// an earlier revision swept both unconditionally, so a test that set
    /// `LOOM_NATIVE_TOOLS_DIR` to an isolated tempdir with `maxAgeHours=0`
    /// (to make its own fixture instantly reclaimable) *also* made every
    /// real per-launch directory under the live
    /// `~/.local/state/loom/native-tools` on the host running the test
    /// "reclaimable", and began deleting them. Asserting exactly one base is
    /// returned — never two — is what keeps that fixed, regardless of
    /// whether the real default directory happens to exist on the host
    /// running this test.
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
        assert_eq!(resolve_max_age_hours(&config), DEFAULT_NATIVE_STATE_MAX_AGE_HOURS);
        assert_eq!(resolve_min_interval_secs(&config), DEFAULT_NATIVE_STATE_MIN_INTERVAL_SECS);
    }

    #[test]
    fn config_overrides_defaults() {
        let config = NativeStateReclaimConfig {
            enabled: Some(false),
            max_age_hours: Some(6),
            min_interval_secs: Some(60),
        };
        assert!(!resolve_enabled(&config));
        assert_eq!(resolve_max_age_hours(&config), 6);
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
    // run_for — enable gate and host-wide cooldown
    // ===================================================================

    #[test]
    #[serial(native_state_reclaim_env)]
    fn run_for_disabled_removes_nothing() {
        reset_state_for_test();
        let temp = tempfile::tempdir().unwrap();
        let base = temp.path().join("native-tools");
        std::fs::create_dir_all(&base).unwrap();
        let dir = launch_dir(&base, HASH, LAUNCH);

        std::env::set_var(NATIVE_STATE_RECLAIM_ENABLE_ENV, "0");
        std::env::set_var(NATIVE_TOOLS_DIR_ENV, &base);
        std::env::set_var(NATIVE_STATE_RECLAIM_MAX_AGE_HOURS_ENV, "0");
        let report = run_for(temp.path());
        std::env::remove_var(NATIVE_STATE_RECLAIM_ENABLE_ENV);
        std::env::remove_var(NATIVE_TOOLS_DIR_ENV);
        std::env::remove_var(NATIVE_STATE_RECLAIM_MAX_AGE_HOURS_ENV);
        reset_state_for_test();

        assert!(!report.enabled);
        assert_eq!(report.removed_count, 0);
        assert!(dir.exists());
    }

    #[test]
    #[serial(native_state_reclaim_env)]
    fn run_for_reclaims_then_defers_inside_the_host_wide_cooldown() {
        reset_state_for_test();
        let temp = tempfile::tempdir().unwrap();
        let base = temp.path().join("native-tools");
        std::fs::create_dir_all(&base).unwrap();
        let first_dir = launch_dir(&base, HASH, LAUNCH);

        std::env::set_var(NATIVE_TOOLS_DIR_ENV, &base);
        // A 0h floor makes every entry, however fresh, older than the floor.
        std::env::set_var(NATIVE_STATE_RECLAIM_MAX_AGE_HOURS_ENV, "0");

        let first = run_for(temp.path());
        let second_dir = launch_dir(&base, HASH, "3f2504e0-4f89-41d3-9a0c-0305e82c3302");
        let second = run_for(temp.path());

        std::env::remove_var(NATIVE_TOOLS_DIR_ENV);
        std::env::remove_var(NATIVE_STATE_RECLAIM_MAX_AGE_HOURS_ENV);
        reset_state_for_test();

        assert_eq!(first.removed_count, 1, "the first call past cooldown reclaims");
        assert!(!first_dir.exists());
        assert_eq!(second.removed_count, 0, "the cooldown suppresses the second call");
        assert!(second.deferred.is_some());
        assert!(second_dir.exists(), "a cooldown-skipped launch dir is untouched");
    }
}
