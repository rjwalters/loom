//! Telling the room that a registered watch fired (Issue #8762, Phase 4 of
//! #4196).
//!
//! # The half that was missing
//!
//! [`crate::watch_registry`] lets an operator register a durable watch on an
//! issue or PR — from the room, via 3a's `watch <issue>` verb, which is one of
//! the five the concierge may relay. When the watch reaches terminal state the
//! monitor appends a [`WatchResult`] line to `~/.loom/logs/watch-results.log`
//! and drops the watch.
//!
//! A log file is the right *durable* answer (it is why #3971 exists: the
//! operator's session had died). It is not an answer to the operator who asked
//! for the watch **in the room**, who has no reason to know that file exists.
//! This module is the other half: the same resolution, said out loud where it
//! was asked for.
//!
//! # Why a cursor over the log, and not a callback from the monitor
//!
//! The obvious wiring is a hook inside
//! [`crate::watch_registry::run_one_tick`]: resolve a watch, send a room line.
//! This module deliberately reads the **durable log** instead, and the reason is
//! the same one that put the log there in the first place:
//!
//! - **A send that fails is not a resolution that did not happen.** safehoused
//!   is an optional, separately-running peer; the monitor's own doc calls all of
//!   its I/O best-effort. A push hook that fires while safehoused is down loses
//!   that narration permanently, because the watch has already been dropped
//!   from the registry. A cursor that has not advanced simply narrates it on the
//!   next pass.
//! - **The log is already the durable record.** Deriving the room line from it
//!   means the room and `tail watch-results.log` can never disagree, and a
//!   narration cannot exist for a resolution that was never recorded.
//! - **Exactly-once is a property of the cursor, not of a code path.** See
//!   [`Cursor`].
//!
//! The cost is latency: a resolution is narrated on the next narration pass
//! rather than within milliseconds. For "issue #6193 closed" that is the right
//! trade.
//!
//! # What is *not* trusted here
//!
//! A [`WatchResult`]'s `note` is operator-typed free text and its `repo` is a
//! forge slug; both are carried verbatim through the log and into [`render`].
//! Neither can make the rendered body an addressed command —
//! [`super::room::emit`] runs the same `vet_say` gate `say` uses, on the
//! finished body, and refuses it if 3a would hear it as addressed. [`render`]
//! additionally collapses the text to one line
//! ([`super::room::one_line`]) so embedded newlines cannot make one narration
//! *look* like two room messages.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::watch_registry::WatchResult;

/// Cursor location, relative to a workspace root. Beside the budget ledger and
/// the digest state, in the same gitignored machine-local directory.
pub const CURSOR_REL: &str = ".loom/concierge/watch-narration.json";

/// How many recently-narrated ids the cursor remembers.
///
/// The ids are the belt to [`Cursor::lines`]'s braces: `lines` is what makes a
/// growing log cheap to skip, and `ids` is what keeps exactly-once honest when
/// `lines` is not trustworthy (a rotated or truncated log). 64 is far above the
/// number of watches a human registers at once.
const REMEMBERED_IDS: usize = 64;

/// What has already been said into the room.
///
/// # Exactly-once
///
/// The results log is append-only, so "already narrated" is a prefix plus a
/// deduplication set:
///
/// - [`Self::lines`] — how many log lines were consumed by a pass in which
///   **every** pending result was narrated successfully. Advanced only then, so
///   a pass that dies halfway re-reads its own tail next time rather than
///   skipping it.
/// - [`Self::ids`] — the last [`REMEMBERED_IDS`] narrated watch ids, which is
///   what stops a re-read from becoming a re-narration.
///
/// Both are needed. `lines` alone breaks if the log is rotated (the count no
/// longer means what it meant). `ids` alone would require parsing the whole log
/// forever and would silently re-narrate anything that aged out of the ring.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Cursor {
    /// Log lines consumed by a fully-successful pass.
    pub lines: u64,
    /// Recently narrated watch ids, oldest first.
    pub ids: Vec<String>,
}

impl Cursor {
    /// Record that `result` reached the room.
    pub fn mark_narrated(&mut self, result: &WatchResult) {
        if !self.ids.contains(&result.id) {
            self.ids.push(result.id.clone());
        }
        if self.ids.len() > REMEMBERED_IDS {
            let excess = self.ids.len() - REMEMBERED_IDS;
            self.ids.drain(..excess);
        }
    }

    /// Advance the consumed-lines mark to the whole of `log`.
    ///
    /// **Only sound when every result [`pending`] returned was narrated.** A
    /// caller that stopped early must not call this; leaving `lines` where it
    /// was is what makes the un-narrated tail get another chance.
    pub fn seal(&mut self, log: &Path) {
        self.lines = line_count(log);
    }
}

/// Cursor path for a workspace root.
#[must_use]
pub fn cursor_path(repo_root: &Path) -> PathBuf {
    repo_root.join(CURSOR_REL)
}

/// Read the cursor. A missing/unreadable/malformed file means "nothing has been
/// narrated", which costs at most a one-time replay of the log's tail.
#[must_use]
pub fn read_cursor(path: &Path) -> Cursor {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default()
}

/// Persist the cursor.
///
/// # Errors
///
/// Any filesystem error. A caller that has already sent narrations must log
/// this rather than treat it as a refusal — the messages are in the room
/// either way, and an unwritten cursor means at most a repeat next pass.
pub fn write_cursor(path: &Path, cursor: &Cursor) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let body = serde_json::to_string_pretty(cursor)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, body)?;
    std::fs::rename(&tmp, path)
}

/// The resolved watches in `log` that `cursor` has not narrated yet, in log
/// order.
///
/// Never fails: a missing log is nothing pending, and an unparseable line is
/// skipped rather than aborting the pass (the log is append-only JSONL written
/// by one writer, so a torn line means a crash mid-append, not a format
/// disagreement).
#[must_use]
pub fn pending(log: &Path, cursor: &Cursor) -> Vec<WatchResult> {
    let Ok(contents) = std::fs::read_to_string(log) else {
        return Vec::new();
    };
    let lines: Vec<&str> = contents.lines().collect();
    // A log shorter than the mark was rotated or truncated: the mark no longer
    // describes a prefix of *this* file, so re-read from the start and let the
    // id ring do the deduplication.
    let skip = if lines.len() as u64 >= cursor.lines {
        usize::try_from(cursor.lines).unwrap_or(usize::MAX)
    } else {
        0
    };
    lines
        .into_iter()
        .skip(skip)
        .filter_map(|line| serde_json::from_str::<WatchResult>(line.trim()).ok())
        .filter(|result| !cursor.ids.contains(&result.id))
        .collect()
}

/// Lines currently in `log` (0 when it does not exist).
fn line_count(log: &Path) -> u64 {
    std::fs::read_to_string(log)
        .map(|contents| contents.lines().count() as u64)
        .unwrap_or(0)
}

/// Render one resolved watch as a single room line.
///
/// Reuses the log's own pre-rendered `summary` (which already embeds the target
/// label, the outcome word and the operator's note) so the room and a `tail` of
/// the log say the same thing — then collapses it to one line. The `watch
/// resolved` prefix is what makes it read as a report rather than as an echo of
/// the `watch <issue>` verb that started it.
#[must_use]
pub fn render(result: &WatchResult) -> String {
    format!("watch resolved — {}", super::room::one_line(&result.summary))
}
