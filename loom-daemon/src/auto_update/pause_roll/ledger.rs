//! What one H4 run has put on disk and in shared state, kept where the
//! supervisor can reach it (issue #10831, Judge findings on #10974).
//!
//! [`super::run_h4`] runs on a blocking thread. Three things can go wrong
//! with that thread that it cannot fix from the inside:
//!
//! - **It fails** (a panic) before it stopped anything. Its pause requests are
//!   still on disk and its reaper holds still stand, so those agents are
//!   denied every tool call and their registry entries never reap.
//! - **It is superseded** and stands down late, after its replacement has
//!   written a manifest, raised requests and taken holds at the same paths.
//! - **It hangs** after the commit point. Dispatch is paused, an abort is
//!   refused, and nothing restarts the daemon.
//!
//! So the run records what it creates in a [`RunLedger`] its supervisor
//! shares, and everything it creates is owned by its **manifest id**:
//!
//! - [`RunLedger::undo`] removes exactly this run's artefacts (requests,
//!   safe-point records, reaper holds, the manifest file) and nothing a
//!   replacement run has created. The run's own stand-down, a failed H4 task
//!   and the uncommitted side of the deadline all use it.
//! - [`RunLedger::force_finish`] is the committed side of the deadline: it
//!   `SIGKILL`s the process group of every item not yet recorded as stopped,
//!   writes the manifest with what is known, and lets the caller restart.
//!
//! Creating and removing artefacts takes one process-wide lock
//! ([`artefacts`]), so a late stand-down's "is this still mine? then delete
//! it" cannot interleave with a replacement run's write.

use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, OnceLock};

use chrono::Utc;

use super::host::{Candidate, PauseHost};
use super::teardown;
use crate::auto_update::pause_manifest::{
    self, Disposition, ItemStatus, ManifestEvent, PauseManifest,
};
use crate::roll_pause;

/// The lock every pause run holds while it creates or removes pause
/// artefacts (manifest file, pause requests, safe-point records, holds).
pub(crate) fn artefacts() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[derive(Default)]
struct Inner {
    /// The snapshot, once step 2 has taken it.
    cands: Vec<Candidate>,
    /// The manifest as the run last handed it to [`RunLedger::save`].
    manifest: Option<PauseManifest>,
    /// Set by [`RunLedger::force_finish`]: the run's own later saves are
    /// dropped, so a thread that comes back cannot overwrite the forced
    /// manifest.
    forced: bool,
}

/// One H4 run's artefacts. See the module doc.
pub(crate) struct RunLedger {
    manifest_id: String,
    manifest_path: PathBuf,
    inner: Mutex<Inner>,
}

/// Whether the manifest file at `path` is the one run `manifest_id` wrote.
/// Reads the raw id, so a manifest the loader would reject (too old, a newer
/// schema) is still recognised as this run's or not.
fn manifest_on_disk_is(path: &Path, manifest_id: &str) -> bool {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok())
        .is_some_and(|v| v.get("manifest_id").and_then(|id| id.as_str()) == Some(manifest_id))
}

impl RunLedger {
    pub(crate) fn new(manifest_id: String, manifest_path: PathBuf) -> Self {
        Self {
            manifest_id,
            manifest_path,
            inner: Mutex::new(Inner::default()),
        }
    }

    /// The run's id: its manifest's, and the owner of everything it creates.
    pub(crate) fn id(&self) -> &str {
        &self.manifest_id
    }

    fn inner(&self) -> MutexGuard<'_, Inner> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Record the step-2 snapshot.
    pub(crate) fn set_candidates(&self, cands: &[Candidate]) {
        self.inner().cands = cands.to_vec();
    }

    /// Write `manifest` to the run's path and remember it. A save after
    /// [`Self::force_finish`] is dropped (`Ok`): the forced manifest stands.
    ///
    /// # Errors
    /// When the file cannot be written.
    pub(crate) fn save(&self, manifest: &PauseManifest) -> std::io::Result<()> {
        let _artefacts = artefacts();
        let mut inner = self.inner();
        if inner.forced {
            return Ok(());
        }
        inner.manifest = Some(manifest.clone());
        pause_manifest::save(&self.manifest_path, manifest)
    }

    /// Remove everything this run created and nothing else: each item's pause
    /// request and safe-point record if they are this run's, each reaper hold
    /// this run holds, and the manifest file if it is still this run's.
    /// Idempotent, and safe to call from the supervisor while the run's own
    /// thread is stuck.
    pub(crate) fn undo(&self, host: &dyn PauseHost) {
        let _artefacts = artefacts();
        let inner = self.inner();
        for c in &inner.cands {
            if let Some(dir) = &c.pause_dir {
                roll_pause::stand_down(dir, &self.manifest_id);
            }
            host.hold(c, false, &self.manifest_id);
        }
        if manifest_on_disk_is(&self.manifest_path, &self.manifest_id) {
            let _ = std::fs::remove_file(&self.manifest_path);
        }
    }

    /// Finish a committed pause whose run cannot (the H4 deadline passed, or
    /// its task failed): `SIGKILL` the process group of every item the last
    /// saved manifest does not record as stopped, mark those items
    /// `requeue` / `planned` / `pause-budget-missed`, withdraw their pause
    /// requests, and write the manifest. Returns the manifest id and how many
    /// items were forced.
    ///
    /// The manifest stays `phase = pausing`: the kills are not verified and no
    /// forge write was done for the forced items, which is exactly the state
    /// the next start finishes from a `pausing` manifest (design §8). Locks,
    /// journal entries and checkpoints are untouched, as everywhere in H4.
    ///
    /// `why` is recorded on the manifest's `h4_forced` event.
    pub(crate) fn force_finish(&self, why: &str, from_version: &str) -> (String, usize) {
        let _artefacts = artefacts();
        let mut inner = self.inner();
        inner.forced = true;
        let Inner {
            cands, manifest, ..
        } = &mut *inner;
        let Some(manifest) = manifest.as_mut() else {
            // Committed without a saved manifest cannot happen (step 3 writes
            // it before anything is stopped); there is nothing to record.
            return (self.manifest_id.clone(), 0);
        };
        let now = Utc::now();
        let at = now.to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
        let mut forced = 0usize;
        for item in &mut manifest.items {
            let settled = item.stopped_at.is_some()
                || matches!(item.status, ItemStatus::Exited | ItemStatus::Requeued);
            if settled {
                continue;
            }
            let cand = cands.iter().find(|c| c.id == item.id);
            let killed = cand.is_some_and(|c| teardown::force_kill_group(&c.tree_spec()));
            if let Some(dir) = cand.and_then(|c| c.pause_dir.as_ref()) {
                roll_pause::withdraw_for(dir, &self.manifest_id);
            }
            item.disposition = Disposition::Requeue;
            item.status = ItemStatus::Planned;
            item.reason = Some(super::REASON_BUDGET_MISSED.to_string());
            item.stopped_at = Some(now);
            forced += 1;
            manifest.events.push(ManifestEvent {
                at: at.clone(),
                by_version: Some(from_version.to_string()),
                item: Some(item.id.clone()),
                event: "forced".to_string(),
                detail: Some(
                    if killed {
                        "SIGKILL sent to the process group"
                    } else {
                        "no process group left to signal"
                    }
                    .to_string(),
                ),
            });
        }
        manifest.events.push(ManifestEvent {
            at,
            by_version: Some(from_version.to_string()),
            item: None,
            event: "h4_forced".to_string(),
            detail: Some(format!("{why}; {forced} item(s) forced")),
        });
        if let Err(e) = pause_manifest::save(&self.manifest_path, manifest) {
            log::error!(
                "pause_roll: could not write the forced pause manifest {} ({e}); the last written \
                 manifest still records every agent",
                self.manifest_path.display()
            );
        }
        (self.manifest_id.clone(), forced)
    }
}
