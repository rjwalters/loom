//! The failed-roll guard (issue #10832; design
//! `docs/design/daemon-roll-pause-resume.md` §8).
//!
//! A roll pauses every agent on the host. If the binary it rolled to does not
//! take (it fails health and the host comes back on the old binary), the old
//! binary's very next auto-update tick sees the same target still above it and
//! would pause the host again, and again. This is the guard against that loop.
//!
//! H5 knows when it happened: the manifest names the version the roll was
//! going to (`roll.to_version`), and the process reading it is running a lower
//! one. It then records the target here with a retry time, and H3
//! ([`super::super::pause_roll::start_pause_roll`]) refuses to start a roll to
//! that same target before the retry time. A different target (a newer
//! release, or the same version under a new checksum) is not held back.
//!
//! # Scope: the minimal guard, not #10880
//!
//! #10880 specifies the full `roll_attempt` record inside
//! `auto_update_state.json`: written at arm time, judged on load, and gating
//! the auto-update tick *before* it fetches. This is the part of it that
//! pause-and-roll's rollback safety needs, kept in its own file so it does not
//! collide with #10880 or #10885, which rewrite that state file:
//!
//! - it is recorded when H5 **observes** the failed roll, not at arm time, so
//!   a roll whose new binary never reached H5 on any binary is not recorded;
//! - it gates the **pause** (H3), not the fetch: a held-back target may still
//!   be downloaded and staged again, but no agent is paused for it.
//!
//! The delays are #10880's: 15 minutes, doubling per failed attempt on the
//! same target, capped at 6 hours. Never terminal: the roll is retried.
//!
//! #10880's tick-side record has since landed
//! ([`crate::auto_update::roll_attempt`]); it shares [`delay`] and gates the
//! tick before it fetches. This guard stays as the pause-side backstop.

use std::path::{Path, PathBuf};

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};

/// File name under the auto-update state dir.
pub const FAILED_TARGET_FILE: &str = "roll-failed-target.json";
/// First retry delay.
pub const BASE_DELAY_MINUTES: i64 = 15;
/// Longest retry delay.
pub const MAX_DELAY_MINUTES: i64 = 6 * 60;

/// A roll target that did not take.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FailedTarget {
    /// The version the roll was going to.
    pub version: String,
    /// Its artifact checksum, when the roll had one.
    #[serde(default)]
    pub artifact_sha256: Option<String>,
    /// Failed rolls to this target so far.
    pub attempts: u32,
    /// The version that was running when the last failure was observed.
    #[serde(default)]
    pub running_version: Option<String>,
    pub failed_at: DateTime<Utc>,
    /// No roll to this target is started before this time.
    pub not_before: DateTime<Utc>,
}

/// The delay after the `attempts`-th failed roll to one target.
#[must_use]
pub fn delay(attempts: u32) -> Duration {
    let doublings = attempts.saturating_sub(1).min(16);
    let minutes = BASE_DELAY_MINUTES.saturating_mul(1_i64 << doublings);
    Duration::minutes(minutes.min(MAX_DELAY_MINUTES))
}

fn path(state_dir: &Path) -> PathBuf {
    state_dir.join(FAILED_TARGET_FILE)
}

fn bare(version: &str) -> &str {
    version.trim().trim_start_matches('v')
}

/// Whether `running` is a lower version than `target`. `false` when either is
/// not a plain `major.minor.patch` (a source rebuild has no version to judge).
#[must_use]
pub fn below(running: &str, target: &str) -> bool {
    use crate::fleet_store::floor::parse_triple;
    match (parse_triple(bare(running)), parse_triple(bare(target))) {
        (Some(r), Some(t)) => r < t,
        _ => false,
    }
}

/// The recorded failed target, if any. A record that does not parse is
/// ignored: the guard is an optimisation, never a reason to refuse a roll.
#[must_use]
pub fn load(state_dir: &Path) -> Option<FailedTarget> {
    serde_json::from_str(&std::fs::read_to_string(path(state_dir)).ok()?).ok()
}

/// Record that the roll to `version` did not take: this process, running
/// `running_version`, found its pause manifest. Counts up when the same
/// target failed before.
///
/// # Errors
/// When the record cannot be written.
pub fn record_failure(
    state_dir: &Path,
    version: &str,
    artifact_sha256: Option<&str>,
    running_version: &str,
    now: DateTime<Utc>,
) -> std::io::Result<FailedTarget> {
    let same = |f: &FailedTarget| {
        bare(&f.version) == bare(version) && f.artifact_sha256.as_deref() == artifact_sha256
    };
    let attempts = load(state_dir)
        .filter(same)
        .map_or(1, |f| f.attempts.saturating_add(1));
    let record = FailedTarget {
        version: version.to_string(),
        artifact_sha256: artifact_sha256.map(str::to_string),
        attempts,
        running_version: Some(running_version.to_string()),
        failed_at: now,
        not_before: now + delay(attempts),
    };
    let body = serde_json::to_vec_pretty(&record).map_err(std::io::Error::other)?;
    crate::roll_pause::write_atomic(&path(state_dir), &body)?;
    Ok(record)
}

/// Forget the record: a roll took.
pub fn clear(state_dir: &Path) {
    let _ = std::fs::remove_file(path(state_dir));
}

/// Why a roll to `version` must not start now, if it must not.
///
/// The retry time is clamped to the longest delay after the failure was
/// recorded (itself clamped to now), so a clock that ran ahead cannot park a
/// roll indefinitely.
#[must_use]
pub fn gate(
    state_dir: &Path,
    version: Option<&str>,
    artifact_sha256: Option<&str>,
    now: DateTime<Utc>,
) -> Option<String> {
    let version = version?;
    let failed = load(state_dir)?;
    if bare(&failed.version) != bare(version) {
        return None;
    }
    // The same version re-published under another checksum is a new target.
    if let (Some(a), Some(b)) = (failed.artifact_sha256.as_deref(), artifact_sha256) {
        if a != b {
            return None;
        }
    }
    let recorded = failed.failed_at.min(now);
    let not_before = failed
        .not_before
        .min(recorded + Duration::minutes(MAX_DELAY_MINUTES));
    (now < not_before).then(|| {
        format!(
            "the last roll to {} did not take ({} failed attempt(s); this host came back on {}), \
             so no roll to it starts before {} ({}s from now). A different target is not held \
             back.",
            failed.version,
            failed.attempts,
            failed
                .running_version
                .as_deref()
                .unwrap_or("its previous binary"),
            not_before.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            (not_before - now).num_seconds()
        )
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn the_delay_doubles_from_fifteen_minutes_and_caps_at_six_hours() {
        let minutes: Vec<i64> = (1..=7).map(|n| delay(n).num_minutes()).collect();
        assert_eq!(minutes, vec![15, 30, 60, 120, 240, 360, 360]);
        assert_eq!(delay(0).num_minutes(), 15);
        assert_eq!(delay(u32::MAX).num_minutes(), 360);
    }

    #[test]
    fn only_a_lower_plain_version_is_below() {
        assert!(below("0.19.887", "0.19.900"));
        assert!(below("v0.19.887", "0.19.900"));
        assert!(!below("0.19.900", "0.19.900"));
        assert!(!below("0.19.901", "0.19.900"));
        assert!(!below("0.19.887", "unknown"));
    }

    #[test]
    fn a_failed_target_is_held_back_until_its_retry_time_and_counts_up() {
        let dir = tempfile::tempdir().unwrap();
        let now = Utc::now();
        assert_eq!(gate(dir.path(), Some("0.19.900"), None, now), None);

        let first = record_failure(dir.path(), "0.19.900", Some("abcd"), "0.19.887", now).unwrap();
        assert_eq!(first.attempts, 1);
        let why = gate(dir.path(), Some("0.19.900"), Some("abcd"), now).unwrap();
        assert!(why.contains("0.19.900") && why.contains("0.19.887"), "{why}");
        assert!(
            gate(dir.path(), Some("v0.19.900"), None, now).is_some(),
            "a `v` prefix is the same target"
        );
        // A different target is not held back.
        assert_eq!(gate(dir.path(), Some("0.19.901"), None, now), None);
        assert_eq!(gate(dir.path(), Some("0.19.900"), Some("ffff"), now), None);
        assert_eq!(gate(dir.path(), None, None, now), None, "a source rebuild has no version");
        // The retry time passes.
        let later = now + Duration::minutes(16);
        assert_eq!(gate(dir.path(), Some("0.19.900"), Some("abcd"), later), None);

        // The same target fails again: a longer delay.
        let second =
            record_failure(dir.path(), "0.19.900", Some("abcd"), "0.19.887", later).unwrap();
        assert_eq!(second.attempts, 2);
        assert_eq!((second.not_before - later).num_minutes(), 30);
        // Another target starts over.
        let other = record_failure(dir.path(), "0.19.950", None, "0.19.887", later).unwrap();
        assert_eq!(other.attempts, 1);

        clear(dir.path());
        assert_eq!(gate(dir.path(), Some("0.19.950"), None, later), None);
    }

    #[test]
    fn a_retry_time_from_a_clock_that_ran_ahead_is_clamped_and_garbage_is_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let now = Utc::now();
        let mut record = record_failure(dir.path(), "1.0.0", None, "0.9.0", now).unwrap();
        record.not_before = now + Duration::days(400);
        std::fs::write(path(dir.path()), serde_json::to_vec(&record).unwrap()).unwrap();
        assert!(gate(dir.path(), Some("1.0.0"), None, now).is_some());
        assert_eq!(gate(dir.path(), Some("1.0.0"), None, now + Duration::hours(7)), None);

        std::fs::write(path(dir.path()), "not json").unwrap();
        assert_eq!(gate(dir.path(), Some("1.0.0"), None, now), None);
    }
}
