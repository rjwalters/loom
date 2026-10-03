//! The **Claude** live-output adapter (#9764): turns the incremental tail of
//! a Claude Code session transcript into [`SessionOutputRecord`]s.
//!
//! # What it reads, and what it refuses to read
//!
//! A Claude Code transcript is JSONL, one record per line, appended as the
//! session runs. This adapter reads only the bytes appended since its last
//! pass and maps **three** record shapes:
//!
//! | source | becomes | carries |
//! |---|---|---|
//! | `assistant` → `content[].text` | `output` | the text (scrubbed, bounded) |
//! | `assistant` → `content[].tool_use` | `tool_start` | the tool **name** |
//! | `user` → `content[].tool_result` | `tool_finish` | the tool name + `is_error` |
//!
//! `thinking` blocks are never emitted. They are *counted* (never copied) and
//! surfaced as a `thinking_withheld` gap because Claude Code can file
//! user-visible narration as `thinking`; see [`THINKING_WITHHELD_REASON`].
//!
//! Everything else is dropped at parse time and has no representation on the
//! wire: user prompts, `thinking` blocks, `tool_use.input` (arguments),
//! `tool_result.content` (raw results), attachments, and every internal
//! bookkeeping type (`queue-operation`, `last-prompt`, …). This is the
//! content boundary — it is enforced by *not constructing a record*, not by
//! filtering one afterwards, so there is no ordering in which a prompt could
//! leak. Redaction ([`super::super::super::telemetry::kinds::session_output::redact`])
//! is a second, independent layer over the text that does get through.
//!
//! # Ordering, identity and loss
//!
//! [`Cursor::next_sequence`] is the 0-based **line index** in the transcript,
//! so it is a property of the source file rather than of when this process
//! happened to read it: a re-read after a restart reproduces the same
//! sequence, hence the same `event_id`. A partially-written trailing line is
//! held in [`Cursor::pending`] and never parsed until its newline arrives,
//! which is what keeps a torn write from being reported as a malformed record.
//!
//! Three situations produce an explicit gap rather than a silent hole:
//! attaching to a transcript that already has history
//! (`backlog_skipped`), a transcript that shrank under us
//! (`source_truncated`), and a first attach to a file too large to index
//! (`backlog_too_large`).

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde_json::Value;

use crate::telemetry::kinds::session_output::{RunIdentity, SessionOutputRecord};

/// Bytes read from one transcript in one pass. A busier stream simply catches
/// up over the next passes; this bounds the work (and the records produced)
/// per tick so one chatty session cannot starve the others.
pub const MAX_BYTES_PER_PASS: u64 = 256 * 1024;

/// Content events kept when first attaching to a transcript that already has
/// history. Older ones are reported as a `backlog_skipped` gap rather than
/// replayed — a live feed wants the tail, and replaying a full session would
/// flood the bounded export queue on every daemon restart.
pub const ATTACH_TAIL_EVENTS: usize = 20;

/// A first attach will index a transcript up to this size to recover exact
/// line numbering. Above it the adapter jumps to the end and says so
/// (`backlog_too_large`) instead of spending unbounded time on a file whose
/// history it is going to discard anyway.
pub const MAX_ATTACH_SCAN_BYTES: u64 = 32 * 1024 * 1024;

/// Gap reason reported when `thinking` blocks were withheld, so a viewer can
/// say "narration may be missing" (#10124).
///
/// No discriminator exists: inspected transcripts from Claude Code 2.1.288
/// carry `thinking` blocks with exactly the keys `type`, `thinking`,
/// `signature`; `thinking` is empty and `signature` populated on every block
/// whether the turn ends in `tool_use` or `end_turn`. A narration-as-thinking
/// block is structurally identical to real reasoning, so nothing can be
/// emitted safely (#9764 privacy rules).
pub const THINKING_WITHHELD_REASON: &str = "thinking_withheld";

/// Per-transcript read position and the little state needed to pair a
/// `tool_result` back to the `tool_use` that started it.
#[derive(Debug, Default)]
pub struct Cursor {
    /// Bytes of this file already consumed.
    offset: u64,
    /// Line index of the next record — the [`SessionOutputRecord::sequence`]
    /// the next parsed line will carry.
    next_sequence: u64,
    /// `tool_use_id` → tool name, so a later `tool_result` can be labelled.
    /// Bounded: an id is removed when its result arrives, and the map is
    /// cleared wholesale if it ever exceeds [`MAX_PENDING_TOOLS`].
    tools: HashMap<String, String>,
    /// A trailing line without its newline yet.
    pending: String,
    /// Whether this cursor has ever read from the file.
    attached: bool,
    /// `thinking` blocks seen in the line being parsed (a count only).
    thinking_seen: u64,
}

/// Ceiling on unmatched `tool_use` ids held for pairing. A session that ends
/// mid-tool leaves entries behind; clearing wholesale past this bound costs
/// only the tool *name* on a few `tool_finish` records.
const MAX_PENDING_TOOLS: usize = 512;

/// What one pass over one transcript produced.
#[derive(Debug, Default)]
pub struct Pass {
    /// Records to publish, in source order.
    pub records: Vec<SessionOutputRecord>,
    /// Source events this pass could not deliver, with the reason. `None`
    /// when nothing was lost.
    pub gap: Option<(String, u64)>,
    /// `thinking` blocks withheld this pass (#10124). Only a count — never
    /// any content. Claude Code (seen on 2.1.288) sometimes files
    /// user-visible pre-tool-call narration as `thinking`, and nothing in the
    /// record reliably separates it from real reasoning (see
    /// [`THINKING_WITHHELD_REASON`]), so every block is dropped and the loss
    /// is reported as a coverage gap instead.
    pub thinking_withheld: u64,
    /// Whether the file had more bytes than [`MAX_BYTES_PER_PASS`] allowed —
    /// not a loss (the next pass continues), but useful for a caller that
    /// wants to tick again immediately.
    pub more_available: bool,
}

impl Cursor {
    /// Read whatever has been appended to `path` since the last pass and map
    /// it to records under `identity`.
    ///
    /// `stream_id` is the logical stream key these records are ordered within
    /// — the transcript's stable identity, not its absolute path (which
    /// embeds a host-specific home directory).
    ///
    /// Blocking file I/O; call from a blocking context. Every failure
    /// degrades to an empty pass rather than propagating: a live feed that
    /// stops on a transient read error is worse than one that skips a tick.
    pub fn advance(
        &mut self,
        path: &Path,
        stream_id: &str,
        identity: &RunIdentity,
        now: DateTime<Utc>,
    ) -> Pass {
        let Ok(metadata) = std::fs::metadata(path) else {
            return Pass::default();
        };
        let len = metadata.len();
        let mut pass = Pass::default();

        if !self.attached {
            self.attached = true;
            if len > MAX_ATTACH_SCAN_BYTES {
                // Too big to index. Start clean at the end and say so.
                self.offset = len;
                pass.gap = Some(("backlog_too_large".to_string(), 0));
                return pass;
            }
            if len > 0 {
                // Index the existing history so `sequence` keeps meaning
                // "line number", then keep only the tail.
                let skipped = self.index_existing(path, len);
                if skipped > 0 {
                    pass.gap = Some(("backlog_skipped".to_string(), skipped));
                }
            }
        } else if len < self.offset {
            // The file shrank: rotated, or rewritten by a resumed session.
            self.offset = 0;
            self.pending.clear();
            self.tools.clear();
            pass.gap = Some(("source_truncated".to_string(), 0));
        }

        if len <= self.offset {
            return pass;
        }
        let budget = (len - self.offset).min(MAX_BYTES_PER_PASS);
        pass.more_available = len - self.offset > budget;
        let Some(chunk) = read_chunk(path, self.offset, budget) else {
            return pass;
        };
        self.offset += chunk.len() as u64;
        let mut buffer = std::mem::take(&mut self.pending);
        buffer.push_str(&String::from_utf8_lossy(&chunk));
        // A chunk that does not end in a newline has a partial trailing line;
        // hold it for the next pass rather than parsing a torn record.
        let complete_to = buffer.rfind('\n').map_or(0, |i| i + 1);
        self.pending = buffer[complete_to..].to_string();
        for line in buffer[..complete_to].lines() {
            let sequence = self.next_sequence;
            self.next_sequence += 1;
            pass.records
                .extend(self.records_for_line(line, stream_id, sequence, identity, now));
            pass.thinking_withheld += std::mem::take(&mut self.thinking_seen);
        }
        pass
    }

    /// Walk the transcript that already exists, advancing `next_sequence` past
    /// every line and leaving `offset` just before the retained tail. Returns
    /// how many lines were skipped outright.
    fn index_existing(&mut self, path: &Path, len: u64) -> u64 {
        let Ok(file) = std::fs::File::open(path) else {
            self.offset = len;
            return 0;
        };
        // Byte offset of the start of each line, so the retained tail can be
        // re-read normally by `advance` instead of being parsed here.
        let mut line_starts: Vec<u64> = Vec::new();
        let mut at = 0_u64;
        let mut reader = BufReader::new(file);
        let mut raw = Vec::new();
        loop {
            raw.clear();
            match reader.read_until(b'\n', &mut raw) {
                Ok(0) => break,
                Ok(n) => {
                    // A trailing chunk with no newline is an incomplete line:
                    // leave it to `advance`, which will hold it as `pending`.
                    if raw.last() != Some(&b'\n') {
                        break;
                    }
                    line_starts.push(at);
                    at += n as u64;
                }
                Err(_) => break,
            }
        }
        let total = line_starts.len();
        let keep_from = total.saturating_sub(ATTACH_TAIL_EVENTS);
        self.next_sequence = keep_from as u64;
        self.offset = line_starts.get(keep_from).copied().unwrap_or(at);
        keep_from as u64
    }

    /// Map one transcript line to zero or more records. Unparseable lines and
    /// every non-content record type yield nothing.
    fn records_for_line(
        &mut self,
        line: &str,
        stream_id: &str,
        sequence: u64,
        identity: &RunIdentity,
        now: DateTime<Utc>,
    ) -> Vec<SessionOutputRecord> {
        let line = line.trim();
        if line.is_empty() {
            return Vec::new();
        }
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            return Vec::new();
        };
        let source_at = value
            .get("timestamp")
            .and_then(Value::as_str)
            .and_then(|ts| DateTime::parse_from_rfc3339(ts).ok())
            .map_or(now, |ts| ts.with_timezone(&Utc));
        let kind = value
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let content = value
            .get("message")
            .and_then(|m| m.get("content"))
            .and_then(Value::as_array);
        let Some(content) = content else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for block in content {
            let block_type = block
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or_default();
            match (kind, block_type) {
                ("assistant", "text") => {
                    let Some(text) = block.get("text").and_then(Value::as_str) else {
                        continue;
                    };
                    if text.trim().is_empty() {
                        continue;
                    }
                    out.push(SessionOutputRecord::new_output(
                        identity.clone(),
                        stream_id,
                        sequence,
                        source_at,
                        now,
                        text,
                    ));
                }
                ("assistant", "tool_use") => {
                    let name = block
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown");
                    if let Some(id) = block.get("id").and_then(Value::as_str) {
                        if self.tools.len() >= MAX_PENDING_TOOLS {
                            self.tools.clear();
                        }
                        self.tools.insert(id.to_string(), name.to_string());
                    }
                    // NOTE: `block["input"]` — the tool's arguments — is
                    // deliberately not read. Only the name crosses.
                    out.push(SessionOutputRecord::tool(
                        identity.clone(),
                        stream_id,
                        sequence,
                        source_at,
                        now,
                        name,
                        None,
                    ));
                }
                ("user", "tool_result") => {
                    let name = block
                        .get("tool_use_id")
                        .and_then(Value::as_str)
                        .and_then(|id| self.tools.remove(id))
                        .unwrap_or_else(|| "unknown".to_string());
                    let ok = !block
                        .get("is_error")
                        .and_then(Value::as_bool)
                        .unwrap_or(false);
                    // NOTE: `block["content"]` — the raw tool result — is
                    // deliberately not read.
                    out.push(SessionOutputRecord::tool(
                        identity.clone(),
                        stream_id,
                        sequence,
                        source_at,
                        now,
                        &name,
                        Some(ok),
                    ));
                }
                ("assistant", "thinking") => {
                    // Counted, never read: the block's content stays unread.
                    self.thinking_seen += 1;
                }
                // `image`, every user prompt block, and every
                // other record type: no record at all.
                _ => {}
            }
        }
        out
    }
}

fn read_chunk(path: &Path, offset: u64, budget: u64) -> Option<Vec<u8>> {
    use std::io::Read as _;
    let mut file = std::fs::File::open(path).ok()?;
    file.seek(SeekFrom::Start(offset)).ok()?;
    let mut buffer = vec![0_u8; usize::try_from(budget).ok()?];
    let mut filled = 0;
    while filled < buffer.len() {
        match file.read(&mut buffer[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(_) => break,
        }
    }
    buffer.truncate(filled);
    Some(buffer)
}

/// Every transcript file attributable to `issue`'s `/loom:sweep` sessions
/// under `projects_dir`, as `(stream_id, path)` pairs.
///
/// `stream_id` is the session uuid for a parent transcript and
/// `<uuid>/<subagent-file-stem>` for a subagent's, so it is stable across
/// hosts and does not embed an absolute path.
///
/// The head scan that decides "does this session belong to issue N" is the
/// same one [`crate::transcript_tokens`] uses for cost attribution, so the two
/// cannot disagree about which session is whose.
#[must_use]
pub fn discover(projects_dir: &Path, workspace_root: &Path, issue: u32) -> Vec<(String, PathBuf)> {
    use crate::transcript_tokens::{head_names_sweep_issue, project_slug, session_transcripts};

    let project = projects_dir.join(project_slug(workspace_root));
    let Ok(entries) = std::fs::read_dir(&project) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_none_or(|e| e != "jsonl") {
            continue;
        }
        let Some(head) = crate::transcript_tokens::read_head(&path) else {
            continue;
        };
        if !head_names_sweep_issue(&head, issue) {
            continue;
        }
        let Some(session) = path.file_stem().map(|s| s.to_string_lossy().into_owned()) else {
            continue;
        };
        for transcript in session_transcripts(&path) {
            let stream_id = if transcript == path {
                session.clone()
            } else {
                let stem = transcript
                    .file_stem()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "subagent".to_string());
                format!("{session}/{stem}")
            };
            out.push((stream_id, transcript));
        }
    }
    out.sort();
    out
}

/// What a transcript says about its own identity: the role it declared, and the
/// canonical join keys resolved by **#9445's own resolver**
/// ([`SessionContext::resolve`]) rather than by anything this module invents.
///
/// `repo` here comes from the workspace's `origin` remote, not from a directory
/// name and not from a `gh` subprocess — which is both cheaper and the exact
/// behaviour #9472 fixed. `kind` is what lets an unattributed session be
/// *explicitly* unscoped: `Interactive` means the missing issue is intended.
#[derive(Debug, Clone, Default)]
pub struct StreamContext {
    pub repo: Option<String>,
    pub issue: Option<u32>,
    pub role: Option<String>,
    pub session_kind: Option<crate::telemetry::SessionKind>,
}

/// Resolve [`StreamContext`] for one transcript.
///
/// Called **once per stream, on first sight** — never on the per-tick hot path.
/// [`parse_transcript`](crate::activity::transcript_parse::parse_transcript)
/// walks the whole file to build its usage/tool aggregates, which is far too
/// expensive to repeat every 2 s, and none of these fields change over a
/// session's life.
#[must_use]
pub fn context_of(path: &Path, now: DateTime<Utc>) -> StreamContext {
    use crate::activity::session_context::SessionContext;

    let parsed = crate::activity::transcript_parse::parse_transcript(path, now);
    let resolved = SessionContext::resolve(&parsed);
    StreamContext {
        repo: resolved.repo,
        issue: resolved.issue,
        // `slash_role` is the session's own `/loom:<role>` declaration; `role`
        // is the broader attribution. Neither is inferred from output text.
        role: parsed.slash_role.clone().or(parsed.role.clone()),
        session_kind: Some(resolved.kind),
    }
}

#[cfg(test)]
#[path = "claude_tests.rs"]
mod tests;
