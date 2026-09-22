//! The `tmpfs_visibility` health section (issue #8572, split from #8512):
//! tmpfs/`shared`-RAM usage and the cumulative kernel OOM-kill count.
//!
//! Split into its own file because `health.rs` sits at its
//! `.loom/docs/file-size-policy.md` ratchet: assessment logic that grows goes
//! in a sibling module and the parent keeps only the `mod` line plus the
//! dispatch call, exactly as [`super::transcript_ingest_section`] already
//! does.
//!
//! # Conditional, but for a different reason than `codesign_preflight`
//!
//! Unlike [`super::assess_codesign_identity`] (platform-gated) or
//! [`super::assess_limit_calibration`] (companion-tool-gated), this section's
//! `None` case is "nothing was measurable at all" —
//! [`crate::tmpfs_visibility::TmpfsVisibilitySnapshot::is_empty`] — which on a
//! real host means no readable `/proc/meminfo`/`/proc/vmstat`/`/proc/mounts`,
//! i.e. macOS or an unusual sandbox. That is a platform fact, not a fault, so
//! it renders no section rather than a non-green line reporting its own
//! absence (the same convention `limit_calibration` follows). A Linux host
//! with *partial* data (e.g. an old kernel with no `oom_kill` vmstat counter)
//! still renders, showing whatever it has.

use super::{HealthInputs, HealthSection, Verdict};

/// Assess the collected [`crate::tmpfs_visibility::TmpfsVisibilitySnapshot`]
/// into the `tmpfs_visibility` section. Pure: the collector (`cli/health.rs`)
/// has already done the I/O ([`crate::tmpfs_visibility::collect`]), so this
/// mapping to verdict/summary is unit-testable against a fixture snapshot,
/// never the live host.
#[must_use]
pub fn assess_tmpfs_visibility(inputs: &HealthInputs) -> Option<HealthSection> {
    const KEY: &str = "tmpfs_visibility";
    let snapshot = inputs.tmpfs_visibility.as_ref()?;
    if snapshot.is_empty() {
        return None;
    }

    let detail = serde_json::json!({
        "totalKb": snapshot.total_kb,
        "availableKb": snapshot.available_kb,
        "shmemKb": snapshot.shmem_kb,
        "shmemFraction": snapshot.shmem_fraction(),
        "oomKillCount": snapshot.oom_kill_count,
        "mounts": snapshot.mounts.iter().map(|m| serde_json::json!({
            "mountPoint": m.mount_point,
            "usedBytes": m.used_bytes,
        })).collect::<Vec<_>>(),
    });

    // A non-zero cumulative OOM-kill count is the single most diagnostic
    // number the #8512 incident narrative names — surfaced as Degraded
    // regardless of how old the kill is (the counter is since-boot, so a
    // fresh reboot clears it; there is no "stale but harmless" reading of a
    // non-zero count here worth suppressing).
    let verdict = if snapshot.oom_kill_count.unwrap_or(0) > 0 {
        Verdict::Degraded
    } else {
        Verdict::Green
    };

    Some(HealthSection {
        key: KEY,
        verdict,
        summary: snapshot.render_summary(),
        detail,
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::health::HealthInputs;
    use crate::tmpfs_visibility::{TmpfsMountUsage, TmpfsVisibilitySnapshot};
    use std::path::PathBuf;

    fn inputs_with(tmpfs_visibility: Option<TmpfsVisibilitySnapshot>) -> HealthInputs {
        HealthInputs {
            tmpfs_visibility,
            ..Default::default()
        }
    }

    #[test]
    fn not_collected_renders_no_section() {
        assert!(assess_tmpfs_visibility(&inputs_with(None)).is_none());
    }

    #[test]
    fn an_empty_snapshot_macos_shape_renders_no_section() {
        let section =
            assess_tmpfs_visibility(&inputs_with(Some(TmpfsVisibilitySnapshot::default())));
        assert!(
            section.is_none(),
            "an all-None snapshot (macOS) must degrade silently, not render"
        );
    }

    #[test]
    fn a_measured_snapshot_with_no_oom_kills_is_green() {
        let snap = TmpfsVisibilitySnapshot {
            total_kb: Some(16_000_000),
            available_kb: Some(8_000_000),
            shmem_kb: Some(500_000),
            oom_kill_count: Some(0),
            mounts: vec![],
        };
        let section = assess_tmpfs_visibility(&inputs_with(Some(snap))).unwrap();
        assert_eq!(section.verdict, Verdict::Green);
        assert!(section.summary.contains("shared"), "{}", section.summary);
    }

    #[test]
    fn a_nonzero_oom_kill_count_is_degraded() {
        let snap = TmpfsVisibilitySnapshot {
            total_kb: Some(16_000_000),
            shmem_kb: Some(6_500_000),
            oom_kill_count: Some(4),
            available_kb: None,
            mounts: vec![TmpfsMountUsage {
                mount_point: PathBuf::from("/dev/shm"),
                used_bytes: 6_200_000_000,
            }],
        };
        let section = assess_tmpfs_visibility(&inputs_with(Some(snap))).unwrap();
        assert_eq!(section.verdict, Verdict::Degraded);
        assert!(section.summary.contains("oom_kill=4"), "{}", section.summary);
        assert!(section.summary.contains("/dev/shm"), "{}", section.summary);
    }

    #[test]
    fn missing_oom_kill_reading_alone_does_not_force_degraded() {
        // An old kernel with no oom_kill vmstat counter still reports the
        // memory figures it has; a missing (not zero) reading must not be
        // conflated with "kills observed".
        let snap = TmpfsVisibilitySnapshot {
            total_kb: Some(16_000_000),
            shmem_kb: Some(500_000),
            oom_kill_count: None,
            available_kb: None,
            mounts: vec![],
        };
        let section = assess_tmpfs_visibility(&inputs_with(Some(snap))).unwrap();
        assert_eq!(section.verdict, Verdict::Green);
    }

    #[test]
    fn assess_renders_the_section_and_promotes_overall_when_degraded() {
        let snap = TmpfsVisibilitySnapshot {
            total_kb: Some(16_000_000),
            shmem_kb: Some(6_500_000),
            oom_kill_count: Some(1),
            available_kb: None,
            mounts: vec![],
        };
        let report = crate::health::assess(&inputs_with(Some(snap)));
        let section = report
            .sections
            .iter()
            .find(|s| s.key == "tmpfs_visibility")
            .expect("assess must render the tmpfs_visibility section");
        assert_eq!(section.verdict, Verdict::Degraded);
        assert_eq!(report.overall, Verdict::Degraded);
    }

    #[test]
    fn assess_omits_the_section_entirely_when_nothing_was_measurable() {
        let report = crate::health::assess(&inputs_with(Some(TmpfsVisibilitySnapshot::default())));
        assert!(report.sections.iter().all(|s| s.key != "tmpfs_visibility"));
    }
}
