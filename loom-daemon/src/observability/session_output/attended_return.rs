//! **When a subagent has returned** (#10125): end a foreground subagent's run
//! as soon as its parent records the subagent's result, instead of 30 minutes
//! later at the idle limit.
//!
//! The watched pid is the operator's `claude` process, which outlives every
//! subagent it starts, so it says nothing about when one is done. The parent's
//! transcript does: Claude Code writes the `Agent` call's `tool_result` there,
//! under the `toolUseId` the subagent's `.meta.json` records.
//!
//! Measured on this host's transcripts (377 foreground subagents, 2026-10-04):
//! the parent writes that result 0.04 s (median) to 0.17 s (max) after the
//! subagent's last line, so ending at it loses nothing. Two results came
//! *before* later subagent lines; both were a coordinator resuming the
//! finished agent with a new message, which is the agent's next task and
//! ends the run anyway. A **background** subagent's result is written when it
//! is launched, before it does anything (23 of 23 measured), so only
//! `requestShape = foreground` is watched. An agent whose metadata does not
//! say `foreground` keeps the older end conditions.
//!
//! Only results written after the tailer attaches count: a result already in
//! the parent belongs to an earlier task the agent was resumed from.

use std::io::{Read as _, Seek as _, SeekFrom};
use std::path::{Path, PathBuf};

use serde_json::Value;

/// Bytes of the parent transcript read per pass. Only paces a burst.
const POLL_BUDGET: u64 = 1024 * 1024;

/// The parent transcript a foreground subagent returns its result to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReturnWatch {
    parent: PathBuf,
    tool_use_id: String,
    /// The next unread byte of `parent`, always a line start once past the
    /// first line.
    offset: u64,
}

impl ReturnWatch {
    /// Watch for `transcript`'s result from the parent's current end, when
    /// `transcript` is a foreground subagent's.
    #[must_use]
    pub fn attach(transcript: &Path) -> Option<Self> {
        let (parent, tool_use_id) = parent_call(transcript)?;
        let offset = std::fs::metadata(&parent).ok()?.len();
        Some(ReturnWatch {
            parent,
            tool_use_id,
            offset,
        })
    }

    /// Read what the parent appended since the last pass: `true` once it
    /// holds this subagent's result.
    pub fn poll(&mut self) -> bool {
        let Some(bytes) = read_from(&self.parent, self.offset, POLL_BUDGET) else {
            return false;
        };
        let needle = self.tool_use_id.as_bytes();
        let mut at = 0_usize;
        while let Some(newline) = bytes[at..].iter().position(|byte| *byte == b'\n') {
            let line = &bytes[at..at + newline];
            at += newline + 1;
            // The id is checked as bytes first, so most lines are never parsed.
            if contains(line, needle) && is_result_of(line, &self.tool_use_id) {
                self.offset += at as u64;
                return true;
            }
        }
        // A line longer than the whole budget would stall every later pass:
        // skip past it. A result that long is missed, and the run ends by the
        // older conditions instead.
        if at == 0 && bytes.len() as u64 >= POLL_BUDGET {
            at = bytes.len();
        }
        self.offset += at as u64;
        false
    }
}

/// The parent transcript and `Agent` call id of `transcript`, read from its
/// `.meta.json`, when it is a foreground subagent's.
fn parent_call(transcript: &Path) -> Option<(PathBuf, String)> {
    let subagents = transcript.parent()?;
    if subagents.file_name()? != "subagents" {
        return None;
    }
    let meta: Value =
        serde_json::from_slice(&std::fs::read(transcript.with_extension("meta.json")).ok()?)
            .ok()?;
    if meta.get("requestShape").and_then(Value::as_str) != Some("foreground") {
        return None;
    }
    let tool_use_id = meta.get("toolUseId")?.as_str()?.to_string();
    // A nested subagent returns to the subagent that started it.
    let parent = match meta.get("parentAgentId").and_then(Value::as_str) {
        Some(agent) if !agent.is_empty() => subagents.join(format!("agent-{agent}.jsonl")),
        _ => {
            let session = subagents.parent()?;
            let name = session.file_name()?.to_string_lossy().into_owned();
            session.parent()?.join(format!("{name}.jsonl"))
        }
    };
    Some((parent, tool_use_id))
}

/// Whether one parent line is the `tool_result` for call `tool_use_id`.
#[must_use]
pub fn is_result_of(line: &[u8], tool_use_id: &str) -> bool {
    let Ok(value) = serde_json::from_slice::<Value>(line) else {
        return false;
    };
    if value.get("type").and_then(Value::as_str) != Some("user") {
        return false;
    }
    value
        .get("message")
        .and_then(|m| m.get("content"))
        .and_then(Value::as_array)
        .is_some_and(|blocks| {
            blocks.iter().any(|block| {
                block.get("type").and_then(Value::as_str) == Some("tool_result")
                    && block.get("tool_use_id").and_then(Value::as_str) == Some(tool_use_id)
            })
        })
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty()
        && haystack
            .windows(needle.len())
            .any(|window| window == needle)
}

/// Up to `max` bytes of `path` from `offset`.
fn read_from(path: &Path, offset: u64, max: u64) -> Option<Vec<u8>> {
    let mut file = std::fs::File::open(path).ok()?;
    file.seek(SeekFrom::Start(offset)).ok()?;
    let mut buffer = Vec::new();
    file.take(max).read_to_end(&mut buffer).ok()?;
    Some(buffer)
}
