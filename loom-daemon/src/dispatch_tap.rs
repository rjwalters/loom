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

/// What an accepted dispatch should do to the per-issue record. Built by
/// [`validate_request`] with no side effects; applied by [`Planned::commit`]
/// only once the dispatch has passed the claim / open-PR / lease guards and
/// spawned, so a rejected duplicate or racing `dispatch N` can never clear or
/// overwrite the record of the sweep already in flight.
#[derive(Debug, PartialEq, Eq)]
pub enum Planned {
    /// No issue to key on (a tap-less PR-set sweep): nothing to do.
    Nothing,
    /// A tap-less dispatch: forget any stale override.
    Clear(u64),
    /// Record this validated tap.
    Save(u64, Tap),
}

/// The prior on-disk state of a record, so a dispatch that fails after
/// [`Planned::commit`] can put it back.
#[derive(Debug)]
pub struct Rollback {
    root: PathBuf,
    issue: u64,
    prior: Option<Vec<u8>>,
}

impl Rollback {
    /// Restore the record exactly as it was before the commit.
    pub fn restore(self) {
        match self.prior {
            Some(bytes) => {
                let _ = std::fs::create_dir_all(dir(&self.root));
                let _ = std::fs::write(path(&self.root, self.issue), bytes);
            }
            None => clear(&self.root, self.issue),
        }
    }
}

impl Planned {
    /// Apply the plan. Returns a [`Rollback`] when the record was touched.
    ///
    /// # Errors
    /// A ready-to-send [`Response`] if the record cannot be written.
    pub fn commit(self, root: &Path) -> Result<Option<Rollback>, Box<Response>> {
        let (issue, tap) = match self {
            Planned::Nothing => return Ok(None),
            Planned::Clear(issue) => (issue, None),
            Planned::Save(issue, tap) => (issue, Some(tap)),
        };
        let rollback = Rollback {
            root: root.to_path_buf(),
            issue,
            prior: std::fs::read(path(root, issue)).ok(),
        };
        match tap {
            None => clear(root, issue),
            Some(tap) => save(root, issue, &tap).map_err(|e| {
                Box::new(Response::Error {
                    message: format!(
                        "dispatch_sweep: cannot record runtime override for #{issue}: {e}"
                    ),
                })
            })?,
        }
        Ok(Some(rollback))
    }
}

/// Validate the per-dispatch tap of a `DispatchSweep` request WITHOUT any side
/// effect. `Ok` means dispatch may proceed; call [`Planned::commit`] once the
/// dispatch has been accepted.
///
/// # Errors
/// A boxed (keeps `Err` small, `clippy::result_large_err`) ready-to-send
/// [`Response`]: `Error` for a malformed or unsupported
/// request, `RuntimeRejected` when the tap is unavailable or unadmitted.
pub fn validate_request(
    root: &Path,
    kind: &SweepKind,
    runtime: Option<&str>,
    model_profile: Option<&str>,
) -> Result<Planned, Box<Response>> {
    let tap = parse_explicit_tap(runtime, model_profile).map_err(|message| {
        Box::new(Response::Error {
            message: format!("dispatch_sweep: {message}"),
        })
    })?;
    let issue = match (kind, &tap) {
        (SweepKind::Issue(issue), _) => u64::from(*issue),
        (SweepKind::PrSet(_), None) => return Ok(Planned::Nothing),
        (SweepKind::PrSet(_), Some(_)) => {
            return Err(Box::new(Response::Error {
                message: "dispatch_sweep: runtime/modelProfile is only supported for issue \
                          sweeps (a PR-set sweep has no issue to key the override on)"
                    .to_string(),
            }))
        }
    };
    let Some(tap) = tap else {
        return Ok(Planned::Clear(issue));
    };
    // The first coding phase is the one that has to launch; Doctor is judged
    // again when it launches, against the same recorded tap.
    resolve_for_dispatch_tap(root, "builder", &tap)
        .map_err(|e| Box::new(Response::RuntimeRejected(e)))?;
    Ok(Planned::Save(issue, tap))
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
        let r = validate_request(dir.path(), &SweepKind::Issue(1), None, Some("zai-flash"));
        assert!(matches!(r.as_ref().map_err(|e| &**e), Err(Response::Error { .. })), "{r:?}");
        let r = validate_request(dir.path(), &SweepKind::PrSet(vec![3]), Some("claude"), None);
        assert!(matches!(r.as_ref().map_err(|e| &**e), Err(Response::Error { .. })), "{r:?}");
    }

    #[test]
    fn unadmitted_tap_is_refused_and_nothing_is_recorded() {
        let dir = tempfile::tempdir().unwrap();
        let r = validate_request(dir.path(), &SweepKind::Issue(1), Some("no-such-rt"), None);
        assert!(
            matches!(r.as_ref().map_err(|e| &**e), Err(Response::RuntimeRejected(_))),
            "{r:?}"
        );
        assert_eq!(load(dir.path(), 1), None);
    }

    /// Review of #11493: validation alone never touches the record, so a
    /// dispatch the guards reject (or that fails to spawn) leaves it as it was.
    #[test]
    fn validation_has_no_side_effects_tapless_and_different_tap() {
        let dir = tempfile::tempdir().unwrap();
        save(dir.path(), 1, &tap()).unwrap();
        // Rejected duplicate with no tap: the in-flight record survives.
        let planned = validate_request(dir.path(), &SweepKind::Issue(1), None, None).unwrap();
        assert_eq!(planned, Planned::Clear(1));
        assert_eq!(load(dir.path(), 1), Some(tap()));
        // Rejected duplicate with a different tap: still the original.
        // (Admission of a real tap needs a runtime fixture; the plan type is
        // what carries it, and building it must not touch disk either.)
        let other = Tap::runtime("claude");
        let planned = Planned::Save(1, other);
        assert!(matches!(planned, Planned::Save(1, _)));
        assert_eq!(load(dir.path(), 1), Some(tap()));
    }

    #[test]
    fn failed_dispatch_leaves_no_record() {
        let dir = tempfile::tempdir().unwrap();
        // Dispatch refused/failed before acceptance: commit is never called.
        let planned = validate_request(dir.path(), &SweepKind::Issue(2), None, None).unwrap();
        drop(planned);
        assert_eq!(load(dir.path(), 2), None);
        // Failure after commit (post-spawn) rolls back to the prior state.
        let planned = Planned::Save(2, tap());
        let rollback = planned.commit(dir.path()).unwrap().unwrap();
        assert_eq!(load(dir.path(), 2), Some(tap()));
        rollback.restore();
        assert_eq!(load(dir.path(), 2), None);
    }

    #[test]
    fn commit_applies_and_rollback_restores_prior_record() {
        let dir = tempfile::tempdir().unwrap();
        save(dir.path(), 1, &tap()).unwrap();
        let rb = Planned::Clear(1).commit(dir.path()).unwrap().unwrap();
        assert_eq!(load(dir.path(), 1), None);
        rb.restore();
        assert_eq!(load(dir.path(), 1), Some(tap()));
        assert!(Planned::Nothing.commit(dir.path()).unwrap().is_none());
    }
}
