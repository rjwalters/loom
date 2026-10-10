//! The value that crosses a split `Issue` dispatch's begin -> poll -> finish
//! seam (Issue #6592), moved out of the ratcheted `mod.rs`.

use std::path::PathBuf;
use std::process::Child;

use super::roll_gate;
use crate::types::{SweepId, SweepKind};

/// Everything [`finish_issue_dispatch`](super::SweepRegistry::finish_issue_dispatch)
/// needs to record a dispatch after the caller has polled the spawned child
/// OUTSIDE the registry mutex (Issue #6592). Produced by
/// [`begin_issue_dispatch`](super::SweepRegistry::begin_issue_dispatch).
pub struct PreparedIssueDispatch {
    /// The live child handle. `poll_and_classify_spawned_child` takes this
    /// by `&mut` to poll its log/exit status; `finish_issue_dispatch` then
    /// takes ownership to record it in `self.children`.
    pub(crate) child: Child,
    /// `sweep_id=<id>` — anchors the log scan to this dispatch's own header
    /// line (see `spawn_child_process`'s doc comment).
    pub(crate) header_anchor: String,
    pub(crate) log_path: PathBuf,
    pub(crate) issue_number: u32,
    pub(crate) sweep_id: SweepId,
    pub(crate) kind: SweepKind,
    pub(crate) idempotency_key: Option<String>,
    /// Already normalized: empty strings collapsed to `None`, matching the
    /// spawn-side rule that `--model ""` / `--effort ""` are never emitted.
    pub(crate) model: Option<String>,
    pub(crate) effort: Option<String>,
    pub(crate) depends_on: Option<u32>,
    /// What this dispatch resolved onto, and the metered backstop slot that
    /// choosing it took (#8555). Carried here — rather than parked in shared
    /// state keyed on the resolving thread — because this box is precisely the
    /// value that crosses the resolve→spawn seam on EVERY dispatch path,
    /// including the two that do not stay on one thread: `ipc.rs`'s
    /// `spawn_blocking(...).await` between begin and finish, and the reaper's
    /// batch of pending resumes prepared in one pass and finished in another.
    /// Dropping this box releases the slot, so an abandoned dispatch cannot
    /// leak one. See `runtime_preference::handoff`.
    pub(crate) admission: crate::runtime_preference::DispatchAdmission,
    /// The issue's story-point size (#9432), resolved at guard-chain step 2.71
    /// from the label set the #4444 park guard already fetched, and carried
    /// across the begin→poll→finish seam so `finish_issue_dispatch` can stamp
    /// it on the `sweep.global.dispatch` event without re-reading the forge.
    /// `None` for an unsized issue, a defective points label set, or a skipped
    /// label read — never `0`.
    pub(crate) story_points: Option<u32>,
    /// `explicit` when the dispatch request named the model, else `default`
    /// (#11280, `telemetry::model_source`).
    pub(crate) model_source: &'static str,
    /// #10974: counts this dispatch as mid-spawn until it is dropped (`roll_gate`).
    pub(crate) mid_spawn: roll_gate::MidSpawn,
}
