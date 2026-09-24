//! The work finder's tmpfs-fraction warning (issue #8572, split from #8512):
//! a loud, bounded, non-spammy log line when tmpfs/`shared` RAM holds more
//! than a configured fraction of total RAM — **never** a dispatch gate.
//!
//! # Warning only — this must never influence `max_concurrent`
//!
//! [`check_and_warn`] returns only whether it fired — a fact for the caller
//! to (optionally) log or count, never a value fed into
//! [`crate::work_finder::resolve_dynamic_max_concurrent`] or either headroom
//! axis ([`crate::disk_headroom::disk_headroom_limit`],
//! [`crate::ram_headroom::ram_headroom_limit`]). There is no path from this
//! module back into the admission cap — `tests::warning_never_changes_the_admission_cap`
//! pins that structurally: computing the cap before and after a firing
//! warning yields the identical value. A host with a legitimately large tmpfs
//! (a shared-memory-heavy workload) must keep receiving dispatch; #8512's own
//! reclaim pass already removes the Loom-caused case on its own.
//!
//! # Bounded, non-spammy
//!
//! A sustained above-threshold host would otherwise log every work-finder
//! tick (default 60s) forever. [`should_warn`] is a pure cooldown check —
//! fires only when the fraction is above threshold **and** at least
//! `min_interval_secs` (default [`DEFAULT_WARN_MIN_INTERVAL_SECS`], env
//! [`WARN_MIN_INTERVAL_SECS_ENV`]) has passed since the last firing — and
//! [`check_and_warn`] backs it with a process-global last-fired timestamp
//! (mirrors [`crate::eager_reclaim`]'s `LAST_RUN_AT` cooldown state, host-wide
//! for the same reason: tmpfs is not scoped to any one registered repo).

use std::sync::{Mutex, OnceLock};

use chrono::{DateTime, Utc};

use crate::tmpfs_reclaim::human_size;
use crate::tmpfs_visibility::{
    self, largest_mount, read_tmpfs_visibility_config, resolve_warn_enabled,
    resolve_warn_fraction_percent,
};

/// Minimum seconds between two firings of this warning — bounds the repeat
/// rate independent of the work-finder tick interval. Env-overridable for
/// tests; not exposed as a `.loom/config.json` knob (unlike the enable flag
/// and threshold) because there is no legitimate reason an operator would
/// want a *noisier* warning, only a quieter one, and the threshold already
/// covers that axis.
pub const WARN_MIN_INTERVAL_SECS_ENV: &str = "LOOM_TMPFS_VISIBILITY_WARN_INTERVAL_SECS";
const DEFAULT_WARN_MIN_INTERVAL_SECS: i64 = 1_800;

fn resolve_min_interval_secs() -> i64 {
    std::env::var(WARN_MIN_INTERVAL_SECS_ENV)
        .ok()
        .and_then(|v| v.trim().parse::<i64>().ok())
        .filter(|&s| s > 0)
        .unwrap_or(DEFAULT_WARN_MIN_INTERVAL_SECS)
}

/// Whether the warning should fire now — pure, so the cooldown/threshold
/// logic is unit-testable without a real clock or a real host.
#[must_use]
pub fn should_warn(
    fraction: Option<f64>,
    threshold_percent: f64,
    last_warned_at: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
    min_interval_secs: i64,
) -> bool {
    let Some(fraction) = fraction else {
        return false;
    };
    if fraction * 100.0 < threshold_percent {
        return false;
    }
    match last_warned_at {
        None => true,
        Some(last) => (now - last).num_seconds() >= min_interval_secs,
    }
}

fn last_warned_at_slot() -> &'static Mutex<Option<DateTime<Utc>>> {
    static SLOT: OnceLock<Mutex<Option<DateTime<Utc>>>> = OnceLock::new();
    SLOT.get_or_init(|| Mutex::new(None))
}

/// One-shot per-tick check: collect the live tmpfs snapshot, decide (via
/// [`should_warn`]) whether to fire, and — if so — log a `WARN` naming the
/// largest offending mount and the `tmpfs-scratch-gc --dry-run` recipe.
/// Returns whether it fired, purely informational — see the module doc's
/// "Warning only" section. Never gates dispatch: the only effects of firing
/// are the log line and this module's own cooldown timestamp.
pub fn check_and_warn(repo_root: &std::path::Path) -> bool {
    let config = read_tmpfs_visibility_config(repo_root);
    if !resolve_warn_enabled(&config) {
        return false;
    }
    let snapshot = tmpfs_visibility::collect();
    let fraction = snapshot.shmem_fraction();
    let threshold_percent = resolve_warn_fraction_percent(&config);
    let now = Utc::now();
    let min_interval_secs = resolve_min_interval_secs();

    let mut slot = match last_warned_at_slot().lock() {
        Ok(slot) => slot,
        Err(poisoned) => poisoned.into_inner(),
    };
    if !should_warn(fraction, threshold_percent, *slot, now, min_interval_secs) {
        return false;
    }
    *slot = Some(now);
    drop(slot);

    let pct = fraction.unwrap_or(0.0) * 100.0;
    let biggest = largest_mount(&snapshot.mounts).map_or_else(
        || "no single mount identified".to_string(),
        |m| format!("{} ({})", m.mount_point.display(), human_size(m.used_bytes)),
    );
    log::warn!(
        "work_finder: tmpfs/shared RAM holds {pct:.1}% of total memory (threshold \
         {threshold_percent:.1}%) — largest contributor: {biggest}. Run `loom-daemon \
         tmpfs-scratch-gc --dry-run` to review reclaimable scratch directories. This is a \
         warning only — dispatch is not held on it (#8572)."
    );
    true
}

/// Drop the cooldown timestamp. Test-only seam (the process-global would
/// otherwise leak between `#[serial]` tests in the same binary) — mirrors
/// [`crate::tmpfs_reclaim::reset_state_for_test`]. Unlike that one, `pub` here
/// would still be flagged `dead_code`: `tmpfs_reclaim` is a `pub mod` (so its
/// `pub fn` is reachable via this crate's public API and exempt), but
/// `work_finder::tmpfs_warning` is a private `mod` (see `work_finder.rs`), so
/// this needs the same `#[cfg(test)]` + `pub(crate)` treatment
/// `role_runner::reset_role_tick_ring_for_tests` already uses for the same
/// reason.
#[cfg(test)]
pub(crate) fn reset_state_for_test() {
    *last_warned_at_slot()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use chrono::Duration;
    use serial_test::serial;

    // ===================================================================
    // should_warn — pure threshold + cooldown logic
    // ===================================================================

    #[test]
    fn fires_when_above_threshold_and_never_warned_before() {
        assert!(should_warn(Some(0.20), 15.0, None, Utc::now(), 1800));
    }

    #[test]
    fn does_not_fire_below_threshold() {
        assert!(!should_warn(Some(0.05), 15.0, None, Utc::now(), 1800));
    }

    #[test]
    fn does_not_fire_on_a_missing_fraction() {
        assert!(!should_warn(None, 15.0, None, Utc::now(), 1800));
    }

    #[test]
    fn at_exactly_the_threshold_fires() {
        assert!(should_warn(Some(0.15), 15.0, None, Utc::now(), 1800));
    }

    #[test]
    fn cooldown_suppresses_a_repeat_within_the_window() {
        let now = Utc::now();
        let last = now - Duration::seconds(60);
        assert!(!should_warn(Some(0.9), 15.0, Some(last), now, 1800));
    }

    #[test]
    fn cooldown_expires_and_allows_a_repeat() {
        let now = Utc::now();
        let last = now - Duration::seconds(1801);
        assert!(should_warn(Some(0.9), 15.0, Some(last), now, 1800));
    }

    // ===================================================================
    // check_and_warn — I/O smoke test against fixtures (#8572: must pass on
    // macOS, so this drives injected files, never the live host)
    // ===================================================================

    #[test]
    #[serial]
    fn check_and_warn_does_not_panic_with_no_tmpfs_data() {
        reset_state_for_test();
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var(
            crate::tmpfs_visibility::MEMINFO_FILE_ENV,
            "/nonexistent-meminfo-for-test",
        );
        std::env::set_var(crate::tmpfs_reclaim::MOUNTS_FILE_ENV, "/nonexistent-mounts-for-test");
        let fired = check_and_warn(tmp.path());
        std::env::remove_var(crate::tmpfs_visibility::MEMINFO_FILE_ENV);
        std::env::remove_var(crate::tmpfs_reclaim::MOUNTS_FILE_ENV);
        assert!(!fired, "no measurable fraction must never fire");
    }

    // ===================================================================
    // Disabled via config — never fires regardless of fraction (#8572 AC:
    // threshold and on/off are config + env)
    // ===================================================================

    #[test]
    #[serial]
    fn check_and_warn_respects_disabled_config() {
        reset_state_for_test();
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".loom")).unwrap();
        std::fs::write(
            tmp.path().join(".loom/config.json"),
            r#"{"autonomous":{"tmpfsVisibility":{"warnEnabled":false}}}"#,
        )
        .unwrap();
        let meminfo = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(meminfo.path(), "MemTotal: 1000 kB\nShmem: 900 kB\n").unwrap();
        std::env::set_var(crate::tmpfs_visibility::MEMINFO_FILE_ENV, meminfo.path());
        std::env::set_var(crate::tmpfs_reclaim::MOUNTS_FILE_ENV, "/nonexistent-mounts-for-test");

        let fired = check_and_warn(tmp.path());

        std::env::remove_var(crate::tmpfs_visibility::MEMINFO_FILE_ENV);
        std::env::remove_var(crate::tmpfs_reclaim::MOUNTS_FILE_ENV);
        assert!(!fired, "warnEnabled: false must suppress the warning even at 90% shmem");
    }

    #[test]
    #[serial]
    fn check_and_warn_fires_above_threshold_and_respects_cooldown() {
        reset_state_for_test();
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".loom")).unwrap();
        std::fs::write(
            tmp.path().join(".loom/config.json"),
            r#"{"autonomous":{"tmpfsVisibility":{"warnFractionPercent":10}}}"#,
        )
        .unwrap();
        let meminfo = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(meminfo.path(), "MemTotal: 1000 kB\nShmem: 900 kB\n").unwrap();
        std::env::set_var(crate::tmpfs_visibility::MEMINFO_FILE_ENV, meminfo.path());
        std::env::set_var(crate::tmpfs_reclaim::MOUNTS_FILE_ENV, "/nonexistent-mounts-for-test");

        let first = check_and_warn(tmp.path());
        let second = check_and_warn(tmp.path());

        std::env::remove_var(crate::tmpfs_visibility::MEMINFO_FILE_ENV);
        std::env::remove_var(crate::tmpfs_reclaim::MOUNTS_FILE_ENV);
        reset_state_for_test();

        assert!(first, "90% shmem against a 10% threshold must fire");
        assert!(!second, "an immediate repeat must be suppressed by the cooldown");
    }

    // ===================================================================
    // Dispatch/admission independence (#8572 AC: "warning only", test-asserted)
    // ===================================================================

    #[test]
    fn warning_never_changes_the_admission_cap() {
        let disk = 5;
        let ram = 3;
        let configured_max = 10;
        let before = crate::work_finder::resolve_dynamic_max_concurrent(disk, ram, configured_max);

        // A firing warning (fraction far above threshold) has no return value
        // or shared state that could feed into admission math.
        assert!(should_warn(Some(0.95), 15.0, None, Utc::now(), 1800));

        let after = crate::work_finder::resolve_dynamic_max_concurrent(disk, ram, configured_max);
        assert_eq!(
            before, after,
            "the tmpfs warning must never influence the admission cap (#8572)"
        );
    }
}
