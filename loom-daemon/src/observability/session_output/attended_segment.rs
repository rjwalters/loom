//! **Which lines are this run's** (#10116): the segment of a subagent's
//! transcript that an attended run publishes.
//!
//! A subagent's transcript is one agent's work, but not always on one issue.
//! A coordinator can send it a new task, and one agent can claim a second
//! issue. So a run owns only the lines from its claim step up to whichever
//! comes first:
//!
//! - **The agent's next task.** A prompt reaches it from outside: a
//!   coordinator's message, or a person's prompt or interrupt. Lines the
//!   harness adds within the same task do not count: tool results, the
//!   agent's own background-task notices, its skill expansions, and stop-hook
//!   feedback. Measured on two weeks of this host's subagent transcripts,
//!   those are the only shapes later `user` lines take.
//! - **A newer claim on the same transcript.** Every start records its claim
//!   in a per-transcript claim file. A run whose claim is no longer the newest
//!   ends at the newer claim's line. The newer run starts at that same line,
//!   so neither run publishes the other's lines.
//!
//! The scanner checks each line before the cursor may read it
//! ([`Segment::limit`]). A line past the boundary is never read under this
//! run's identity, whatever the tick timing.

use std::io::{Read as _, Seek as _, SeekFrom};
use std::path::Path;

use serde_json::Value;

use super::EndReason;

/// Bytes checked per pass. The cursor never reads past what was checked, so
/// this only paces a burst; it cannot let a line through unchecked.
const SCAN_BUDGET: u64 = 1024 * 1024;

/// Whether one transcript line hands the agent new direction from outside its
/// current task.
#[must_use]
pub fn starts_next_task(line: &[u8]) -> bool {
    let Ok(value) = serde_json::from_slice::<Value>(line) else {
        return false;
    };
    if value.get("type").and_then(Value::as_str) != Some("user") {
        return false;
    }
    let content = value.get("message").and_then(|m| m.get("content"));
    let only_tool_results = content.and_then(Value::as_array).is_some_and(|blocks| {
        !blocks.is_empty()
            && blocks
                .iter()
                .all(|block| block.get("type").and_then(Value::as_str) == Some("tool_result"))
    });
    if only_tool_results {
        return false;
    }
    if value.get("isMeta").and_then(Value::as_bool) != Some(true) {
        // A prompt typed or sent into the conversation, or an interrupt.
        return true;
    }
    // A meta line is the harness speaking. With no origin it is part of the
    // current task (a skill expansion, hook feedback, an image). With one, it
    // came from outside the agent, unless it reports the agent's own
    // background task. An origin this code does not know ends the run: a
    // shorter feed, never a mislabeled one.
    value
        .get("origin")
        .and_then(|origin| origin.get("kind"))
        .and_then(Value::as_str)
        .is_some_and(|kind| kind != "task-notification")
}

/// The checked extent of one run's segment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Segment {
    /// Bytes from the claim line on that were checked and belong to the run.
    checked: u64,
    /// Where the run's lines stop, once known, and why.
    end: Option<(u64, EndReason)>,
}

impl Segment {
    #[must_use]
    pub fn starting_at(from: u64) -> Self {
        Segment {
            checked: from,
            end: None,
        }
    }

    /// Check the complete lines appended since the last pass, stopping at the
    /// first line of the agent's next task.
    pub fn scan(&mut self, path: &Path) {
        let stop = self.end.map(|(at, _)| at);
        if stop.is_some_and(|stop| self.checked >= stop) {
            return;
        }
        let Some(bytes) = read_from(path, self.checked, SCAN_BUDGET) else {
            return;
        };
        let mut at = 0_usize;
        while let Some(newline) = bytes[at..].iter().position(|byte| *byte == b'\n') {
            let line_start = self.checked + at as u64;
            if stop.is_some_and(|stop| line_start >= stop) {
                break;
            }
            if starts_next_task(&bytes[at..at + newline]) {
                self.end = Some((line_start, EndReason::NextTask));
                break;
            }
            at += newline + 1;
        }
        self.checked += at as u64;
    }

    /// A newer claim on this transcript starts at byte `at`, so this run's
    /// lines end there.
    pub fn supersede(&mut self, at: u64) {
        if self.end.is_none_or(|(end, _)| at < end) {
            self.end = Some((at, EndReason::Superseded));
        }
    }

    /// The furthest byte the run's cursor may read.
    #[must_use]
    pub fn limit(&self) -> u64 {
        self.end
            .map_or(self.checked, |(end, _)| self.checked.min(end))
    }

    /// Why the run is over, once its cursor has read everything it owns.
    /// `end` is always a line start, and every line before it is complete, so
    /// the cursor reaches it exactly.
    #[must_use]
    pub fn finished(&self, cursor_offset: u64) -> Option<EndReason> {
        let (end, reason) = self.end?;
        (cursor_offset >= end).then_some(reason)
    }
}

/// Up to `max` bytes of `path` from `offset`.
fn read_from(path: &Path, offset: u64, max: u64) -> Option<Vec<u8>> {
    let mut file = std::fs::File::open(path).ok()?;
    file.seek(SeekFrom::Start(offset)).ok()?;
    let mut buffer = Vec::new();
    file.take(max).read_to_end(&mut buffer).ok()?;
    Some(buffer)
}

// ============================================================================
// The claim file: which claim owns a transcript now
// ============================================================================

/// One claim step on a transcript: the issue, and the byte offset of the
/// transcript line holding the claim call.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Claim {
    pub issue: u32,
    pub from: u64,
}

/// The transcript's newest claim, if one is recorded.
#[must_use]
pub fn read_claim(path: &Path) -> Option<Claim> {
    serde_json::from_slice(&std::fs::read(path).ok()?).ok()
}

/// Record `claim` as the transcript's newest, atomically.
///
/// # Errors
///
/// The write or rename failure.
pub fn write_claim(path: &Path, claim: Claim) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let staging = path.with_extension(format!("claim.{}.tmp", std::process::id()));
    std::fs::write(&staging, serde_json::to_vec(&claim)?)?;
    std::fs::rename(&staging, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&staging);
    })
}

/// Remove the claim file if it still records `claim`, so a finished run
/// never deletes a newer run's claim.
pub fn clear_claim(path: &Path, claim: Claim) {
    if read_claim(path) == Some(claim) {
        let _ = std::fs::remove_file(path);
    }
}

/// Where a newer claim than `own` starts, if the claim file records one.
#[must_use]
pub fn newer_claim(path: &Path, own: Claim) -> Option<u64> {
    read_claim(path)
        .filter(|claim| *claim != own && claim.from >= own.from)
        .map(|claim| claim.from)
}
