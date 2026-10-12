//! `loom.result = interrupted_by_roll` (#11263): a run a planned roll ended is
//! not a failure.
//!
//! A pause-and-roll (#10831) stops every in-flight sweep and role run. Before
//! this module the spans those runs left closed as `failure` (the role runner
//! saw its child die) or, after the restart, as `process_lost` (orphan
//! recovery found the owner and its supervisor gone). Every roll so added a
//! burst of failures that were not faults.
//!
//! Two paths now record the truth instead:
//!
//! - **In the old binary.** H4 notes each tree it stops ([`note_stopped`],
//!   keyed by the agent's pid) before it signals it. A role tick whose child
//!   was one of them closes as `interrupted_by_roll` ([`stopped`]).
//! - **In the new binary.** Orphan recovery ([`recover`]) checks the pause
//!   manifests on disk (the live one and the archived ones) before it writes
//!   `process_lost`. A journal with a span owned by a pid H4 stopped, opened
//!   before that pause began, closes as `interrupted_by_roll`.
//!
//! Membership is strict: an unplanned crash has no manifest entry and still
//! reads as `process_lost`, and an unreadable manifest falls back to
//! `process_lost` too (fail toward visibility, never toward hiding failures).
//!
//! A resumed role run carries [`RESUMED_FROM`] = the paused run's manifest item
//! id, which the interrupted span carries as [`ITEM_ID`], so a resumed success
//! joins the attempt it continues.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use chrono::{DateTime, Utc};

use super::{attributes, identity_gone, role_failure, Span};
use crate::auto_update::pause_manifest::{
    self, Disposition, ItemStatus, LoadOutcome, ManifestItem,
};
use crate::telemetry::trace::{
    journal::{ActiveSpan, Journal},
    TraceAttributes,
};

/// The `loom.result` of a run a planned roll ended.
pub const RESULT: &str = "interrupted_by_roll";
/// What the roll did with the run: [`RollDisposition`].
pub const DISPOSITION: &str = "loom.roll.disposition";
/// The interrupted run's pause-manifest item id.
pub const ITEM_ID: &str = "loom.roll.item_id";
/// On a resumed run: the [`ITEM_ID`] of the run it resumes.
pub const RESUMED_FROM: &str = "loom.resumed_from";
/// The exported span attributes this module adds.
pub const KEYS: [&str; 3] = [DISPOSITION, ITEM_ID, RESUMED_FROM];

/// The H4 reason an item was recorded with when it ended by itself.
const EXITED_BEFORE_SAFE_POINT: &str = "exited-before-safe-point";

/// What a roll did with a run it stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RollDisposition {
    /// Stopped at a safe point, to be resumed by the next binary.
    Paused,
    /// Torn down and handed back (young, or not resumable).
    Requeued,
    /// No safe point within the pause budget: torn down and handed back.
    BudgetMissed,
}

impl RollDisposition {
    /// The wire spelling.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Paused => "paused",
            Self::Requeued => "requeued",
            Self::BudgetMissed => "budget-missed",
        }
    }

    /// What a manifest item says the roll did with it. `None` when the roll
    /// did not end it: never stopped, or it exited by itself first.
    #[must_use]
    pub fn of_item(item: &ManifestItem) -> Option<Self> {
        if item.stopped_at.is_none()
            || item.status == ItemStatus::Exited
            || item.reason.as_deref() == Some(EXITED_BEFORE_SAFE_POINT)
        {
            return None;
        }
        Some(
            if item.reason.as_deref() == Some(crate::auto_update::pause_roll::REASON_BUDGET_MISSED)
            {
                Self::BudgetMissed
            } else if item.disposition == Disposition::Requeue {
                Self::Requeued
            } else {
                Self::Paused
            },
        )
    }
}

/// One run a roll ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Interruption {
    pub item_id: String,
    pub disposition: RollDisposition,
}

impl Interruption {
    /// Turn `close` into an interrupted close: the roll's attributes in, the
    /// failure attributes (class, exit code, status message) out.
    pub fn apply(&self, close: &mut TraceAttributes) {
        for key in role_failure::ROLE_FAILURE_ATTRIBUTE_KEYS {
            close.remove(*key);
        }
        close.insert("loom.result".into(), RESULT.into());
        close.insert(DISPOSITION.into(), self.disposition.as_str().into());
        close.insert(ITEM_ID.into(), self.item_id.clone());
    }
}

fn stopped_registry() -> &'static Mutex<HashMap<u32, Interruption>> {
    static STOPPED: OnceLock<Mutex<HashMap<u32, Interruption>>> = OnceLock::new();
    STOPPED.get_or_init(Mutex::default)
}

/// H4 is about to stop the tree led by `pid` (#11263). Called before any
/// signal, so the run's own close sees it.
pub fn note_stopped(pid: Option<u32>, item_id: &str, disposition: RollDisposition) {
    let Some(pid) = pid.filter(|p| *p > 0) else {
        return;
    };
    if let Ok(mut map) = stopped_registry().lock() {
        map.insert(
            pid,
            Interruption {
                item_id: item_id.to_owned(),
                disposition,
            },
        );
    }
}

/// What [`note_stopped`] recorded for `pid`, without consuming it.
#[cfg(test)]
pub(crate) fn noted(pid: u32) -> Option<Interruption> {
    stopped_registry().lock().ok()?.get(&pid).cloned()
}

fn take_stopped(pid: u32) -> Option<Interruption> {
    if pid == 0 {
        return None;
    }
    stopped_registry().lock().ok()?.remove(&pid)
}

/// Whether the roll stopped the child that owns `span` (its root's current
/// owner pid). Consumes the note.
pub(super) fn stopped(span: &Span) -> Option<Interruption> {
    let active = span.journal.active().ok()?;
    let owner = active
        .iter()
        .find(|a| a.record.context == *span.context())?
        .owner_pid;
    take_stopped(owner)
}

thread_local! {
    static RESUMED: RefCell<Option<String>> = const { RefCell::new(None) };
}

/// The role tick about to run on this thread resumes the paused run
/// `item_id` (#11263). Its `loom.role_attempt` close carries [`RESUMED_FROM`].
pub fn set_resumed_from(item_id: &str) {
    RESUMED.with(|slot| *slot.borrow_mut() = Some(item_id.to_owned()));
}

/// Take this thread's [`set_resumed_from`] note, so it never leaks into the
/// next tick.
pub(super) fn take_resumed_from() -> Option<String> {
    RESUMED.with(|slot| slot.borrow_mut().take())
}

/// One stopped item, as a pause manifest on disk records it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RollRecord {
    pub pid: u32,
    pub paused_at: DateTime<Utc>,
    pub interruption: Interruption,
}

/// The roll records recovery checks, read from the state dir at most once
/// per backfill pass, and only when some journal is actually orphaned.
pub struct Records {
    dir: Option<PathBuf>,
    loaded: Option<Vec<RollRecord>>,
}

impl Records {
    /// The records under the auto-update state dir.
    #[must_use]
    pub fn from_state_dir() -> Self {
        Self::at(crate::auto_update::state_dir())
    }

    /// The records under `dir` (`None`: there are none).
    #[must_use]
    pub fn at(dir: Option<PathBuf>) -> Self {
        Self { dir, loaded: None }
    }

    fn get(&mut self) -> &[RollRecord] {
        let dir = self.dir.as_deref();
        self.loaded
            .get_or_insert_with(|| dir.map(load).unwrap_or_default())
    }
}

/// Every stopped item in the live pause manifest and its archives under
/// `dir`. A file that cannot be read or parsed contributes nothing.
#[must_use]
pub fn load(dir: &Path) -> Vec<RollRecord> {
    let now = Utc::now();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut records = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let archived = name.starts_with("roll-pause-manifest.") && name.ends_with(".done.json");
        if name != pause_manifest::MANIFEST_FILE && !archived {
            continue;
        }
        let manifest = match pause_manifest::load(&entry.path(), now) {
            LoadOutcome::Loaded(m) | LoadOutcome::Stale(m) => m,
            _ => continue,
        };
        let paused_at = manifest.roll.pause_started_at;
        records.extend(manifest.items.iter().filter_map(|item| {
            Some(RollRecord {
                pid: item.pid.filter(|p| *p > 0)?,
                paused_at,
                interruption: Interruption {
                    item_id: item.id.clone(),
                    disposition: RollDisposition::of_item(item)?,
                },
            })
        }));
    }
    records
}

/// The roll that ended this journal's work, when one did: a span whose owner
/// is a pid a roll stopped, and that was already open when that pause began.
#[must_use]
pub fn matching<'a>(records: &'a [RollRecord], active: &[ActiveSpan]) -> Option<&'a Interruption> {
    records
        .iter()
        .find(|r| {
            active
                .iter()
                .any(|span| span.owner_pid == r.pid && span.record.started_at <= r.paused_at)
        })
        .map(|r| &r.interruption)
}

/// Close a journal whose owners and supervisors are all gone. A worker exit
/// precedes authoritative reaping/verification, so the live supervisor still
/// owns that gap even when every worker PID is gone.
pub(super) fn recover(journal: &Journal, records: &mut Records) {
    let abandoned = |span: &ActiveSpan| {
        identity_gone(span.owner_pid, span.owner_observed_at)
            && span
                .supervisor_pid
                .is_some_and(|pid| identity_gone(pid, span.supervisor_observed_at))
    };
    let Ok(active) = journal.active() else {
        return;
    };
    if active.is_empty() || !active.iter().all(abandoned) {
        return;
    }
    let mut close = attributes(&[
        ("loom.result", "process_lost"),
        // #10640: what recovery knows — the child and its supervisor both
        // exited before either recorded a close. Status stays unset.
        (role_failure::FAILURE_CLASS, "supervisor-lost"),
        ("loom.recovered", "true"),
        ("loom.timing_source", "recovery_observed"),
    ]);
    if let Some(roll) = matching(records.get(), &active) {
        roll.apply(&mut close);
    }
    let _ = journal.finish_abandoned(abandoned, close);
}

#[cfg(test)]
#[path = "roll_interruption/tests.rs"]
mod tests;
