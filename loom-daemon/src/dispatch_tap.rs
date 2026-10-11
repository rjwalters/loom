//! Per-dispatch explicit tap persistence and request handling (#11284).
//!
//! `dispatch_sweep` may carry an optional `runtime` / `modelProfile`. The
//! daemon validates it at dispatch time (refusing an unavailable or unadmitted
//! tap with the reason), then records it in a small per-issue sidecar under
//! `<workspace>/.loom/sweep-dispatch-tap/issue-<N>.json`. The Builder and
//! Doctor phase runner (`worker run`) reads that record when it resolves its
//! runtime, so the override survives a resumed sweep, a daemon roll and a
//! Doctor retry: none of those re-send the original request, and none depend
//! on environment that a respawned orchestrator may not inherit.
//!
//! Records expire ([`MAX_AGE_SECS`]) so a forgotten override cannot steer
//! unrelated future work on the same issue. A later `dispatch_sweep` for the
//! same issue without a tap clears the record; automatic re-dispatch paths
//! (work finder, watchdog, crash resume) never touch it.

use crate::runtime_preference::explicit_tap::{parse_explicit_tap, resolve_for_dispatch_tap};
use crate::runtime_preference::Tap;
use crate::types::{Response, SweepKind};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Records older than this are ignored (and removed on the next write).
pub const MAX_AGE_SECS: u64 = 7 * 24 * 3600;

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
struct Record {
    runtime: String,
    #[serde(default)]
    model_profile: Option<String>,
    created_at: u64,
}

fn dir(root: &Path) -> PathBuf {
    root.join(".loom").join("sweep-dispatch-tap")
}

fn path(root: &Path, issue: u64) -> PathBuf {
    dir(root).join(format!("issue-{issue}.json"))
}

fn now() -> u64 {
    u64::try_from(chrono::Utc::now().timestamp()).unwrap_or(0)
}

/// Persist `tap` for `issue`.
///
/// # Errors
/// An I/O or serialization error.
pub fn save(root: &Path, issue: u64, tap: &Tap) -> std::io::Result<()> {
    let record = Record {
        runtime: tap.runtime.clone(),
        model_profile: tap.model_profile.clone(),
        created_at: now(),
    };
    std::fs::create_dir_all(dir(root))?;
    let target = path(root, issue);
    let tmp = target.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec(&record)?)?;
    std::fs::rename(tmp, target)
}

/// Forget any override recorded for `issue`.
pub fn clear(root: &Path, issue: u64) {
    let _ = std::fs::remove_file(path(root, issue));
}

/// The live override for `issue`, if one was recorded and has not expired.
/// An unreadable record is treated as absent.
#[must_use]
pub fn load(root: &Path, issue: u64) -> Option<Tap> {
    load_at(root, issue, now())
}

fn load_at(root: &Path, issue: u64, now: u64) -> Option<Tap> {
    let bytes = std::fs::read(path(root, issue)).ok()?;
    let record: Record = serde_json::from_slice(&bytes).ok()?;
    if now.saturating_sub(record.created_at) > MAX_AGE_SECS {
        return None;
    }
    parse_explicit_tap(Some(&record.runtime), record.model_profile.as_deref())
        .ok()
        .flatten()
}

/// The sweep issue a phase belongs to: the explicit `--issue`, else the
/// daemon-exported `LOOM_SWEEP_CLAIM_OWNED` (which a Doctor, launched by `--pr`,
/// inherits from its orchestrator).
#[must_use]
pub fn phase_issue(issue: Option<u64>) -> Option<u64> {
    issue.or_else(|| {
        std::env::var("LOOM_SWEEP_CLAIM_OWNED")
            .ok()
            .and_then(|v| v.trim().parse().ok())
    })
}

/// Validate and record (or clear) the per-dispatch tap of a `DispatchSweep`
/// request. `Ok(())` means dispatch may proceed.
///
/// # Errors
/// A ready-to-send [`Response`]: `Error` for a malformed or unsupported
/// request, `RuntimeRejected` when the tap is unavailable or unadmitted.
pub fn apply_request(
    root: &Path,
    kind: &SweepKind,
    runtime: Option<&str>,
    model_profile: Option<&str>,
) -> Result<(), Response> {
    let tap = parse_explicit_tap(runtime, model_profile).map_err(|message| Response::Error {
        message: format!("dispatch_sweep: {message}"),
    })?;
    let issue = match (kind, &tap) {
        (SweepKind::Issue(issue), _) => u64::from(*issue),
        (SweepKind::PrSet(_), None) => return Ok(()),
        (SweepKind::PrSet(_), Some(_)) => {
            return Err(Response::Error {
                message: "dispatch_sweep: runtime/modelProfile is only supported for issue \
                          sweeps (a PR-set sweep has no issue to key the override on)"
                    .to_string(),
            })
        }
    };
    let Some(tap) = tap else {
        clear(root, issue);
        return Ok(());
    };
    // The first coding phase is the one that has to launch; Doctor is judged
    // again when it launches, against the same recorded tap.
    resolve_for_dispatch_tap(root, "builder", &tap).map_err(Response::RuntimeRejected)?;
    save(root, issue, &tap).map_err(|e| Response::Error {
        message: format!("dispatch_sweep: cannot record runtime override for #{issue}: {e}"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tap() -> Tap {
        Tap {
            runtime: "opencode".into(),
            model_profile: Some("zai-flash".into()),
        }
    }

    #[test]
    fn round_trips_and_clears() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(load(dir.path(), 7), None);
        save(dir.path(), 7, &tap()).unwrap();
        assert_eq!(load(dir.path(), 7), Some(tap()));
        assert_eq!(load(dir.path(), 8), None);
        clear(dir.path(), 7);
        assert_eq!(load(dir.path(), 7), None);
    }

    #[test]
    fn expired_and_corrupt_records_are_ignored() {
        let dir = tempfile::tempdir().unwrap();
        save(dir.path(), 7, &tap()).unwrap();
        assert_eq!(load_at(dir.path(), 7, now() + MAX_AGE_SECS + 10), None);
        std::fs::write(path(dir.path(), 7), b"{not json").unwrap();
        assert_eq!(load(dir.path(), 7), None);
    }

    #[test]
    fn malformed_request_is_an_error_response() {
        let dir = tempfile::tempdir().unwrap();
        let r = apply_request(dir.path(), &SweepKind::Issue(1), None, Some("zai-flash"));
        assert!(matches!(r, Err(Response::Error { .. })), "{r:?}");
        let r = apply_request(dir.path(), &SweepKind::PrSet(vec![3]), Some("claude"), None);
        assert!(matches!(r, Err(Response::Error { .. })), "{r:?}");
    }

    #[test]
    fn unadmitted_tap_is_refused_and_nothing_is_recorded() {
        let dir = tempfile::tempdir().unwrap();
        let r = apply_request(dir.path(), &SweepKind::Issue(1), Some("no-such-rt"), None);
        assert!(matches!(r, Err(Response::RuntimeRejected(_))), "{r:?}");
        assert_eq!(load(dir.path(), 1), None);
    }

    #[test]
    fn dispatch_without_a_tap_clears_a_stale_record() {
        let dir = tempfile::tempdir().unwrap();
        save(dir.path(), 1, &tap()).unwrap();
        apply_request(dir.path(), &SweepKind::Issue(1), None, None).unwrap();
        assert_eq!(load(dir.path(), 1), None);
    }
}
