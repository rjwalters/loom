//! Claude burn source: `${CLAUDE_CONFIG_DIR:-~/.claude}/projects/**.jsonl`
//! (Issue #8857, made incremental by #8930).
//!
//! Records are decoded by the shared
//! [`usage_from_record`](crate::script_helpers::transcript_usage::usage_from_record).
//! Streamed chunks of one API message repeat its `message.id` with cumulative
//! usage, so ids are remembered across polls (for [`MESSAGE_RETAIN_SECS`]):
//! a message's first record counts as one request with its usage, and a later
//! chunk contributes only what it adds over the running maximum. The memory is
//! global across files, so a message copied into a resumed session counts
//! once. A record without an id is one request on its own. `<synthetic>`
//! records (Claude Code's internal echoes) are skipped.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Duration, Utc};

use super::burn::{BurnEvent, BurnSource, Emit, ModelBurn, LATE_GRACE_SECS};
use super::tail::TailSet;
use crate::script_helpers::transcript_usage::usage_from_record;

/// `provider` label for the Claude transcript store — the same value
/// `tokens.snapshot` uses for the Claude pool.
pub const CLAUDE_PROVIDER: &str = "claude";

/// How long a message id is remembered after its last chunk. Longer than
/// [`LATE_GRACE_SECS`], so a copy young enough to be counted is always
/// recognised.
pub const MESSAGE_RETAIN_SECS: i64 = 2 * LATE_GRACE_SECS;

/// `projects/<slug>/<session>/subagents/<agent>.jsonl` is three levels deep;
/// one spare level, and no unbounded walk.
const MAX_WALK_DEPTH: usize = 4;

/// What has been counted for one message id.
#[derive(Debug, Clone)]
struct Seen {
    last_at: DateTime<Utc>,
    counted: ModelBurn,
}

/// Message-id memory: turns transcript records into once-only events.
#[derive(Debug, Default)]
pub struct ClaudeMessages {
    seen: HashMap<String, Seen>,
}

impl ClaudeMessages {
    /// Decode one transcript line, pushing the usage it adds (if any).
    pub fn add_line(&mut self, line: &str, emit: Emit, out: &mut Vec<BurnEvent>) {
        if !line.contains("\"usage\"") {
            return;
        }
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            return;
        };
        let Some(record) = usage_from_record(&value) else {
            return;
        };
        if record.synthetic {
            return;
        }
        let Some(at) = record
            .timestamp
            .as_deref()
            .and_then(|ts| DateTime::parse_from_rfc3339(ts).ok())
            .map(|dt| dt.with_timezone(&Utc))
        else {
            return;
        };
        let usage = ModelBurn {
            input: record.input,
            output: record.output,
            cache_read: record.cache_read,
            cache_write: record.cache_write_5m.saturating_add(record.cache_write_1h),
            requests: 1,
        };
        let added = match record.message_id {
            None => usage,
            Some(id) => match self.seen.get_mut(&id) {
                None => {
                    self.seen.insert(
                        id,
                        Seen {
                            last_at: at,
                            counted: usage.clone(),
                        },
                    );
                    usage
                }
                Some(seen) => {
                    seen.last_at = seen.last_at.max(at);
                    let counted = &mut seen.counted;
                    let added = ModelBurn {
                        input: (usage.input - counted.input).max(0),
                        output: (usage.output - counted.output).max(0),
                        cache_read: (usage.cache_read - counted.cache_read).max(0),
                        cache_write: (usage.cache_write - counted.cache_write).max(0),
                        requests: 0,
                    };
                    counted.input = counted.input.max(usage.input);
                    counted.output = counted.output.max(usage.output);
                    counted.cache_read = counted.cache_read.max(usage.cache_read);
                    counted.cache_write = counted.cache_write.max(usage.cache_write);
                    added
                }
            },
        };
        if emit.accepts(at) && !added.is_empty() {
            out.push(BurnEvent {
                provider: CLAUDE_PROVIDER.to_string(),
                model: record.model,
                at,
                usage: added,
            });
        }
    }

    /// Forget messages whose last chunk is older than [`MESSAGE_RETAIN_SECS`].
    pub fn prune(&mut self, now: DateTime<Utc>) {
        let horizon = now - Duration::seconds(MESSAGE_RETAIN_SECS);
        self.seen.retain(|_, seen| seen.last_at >= horizon);
    }
}

/// `*.jsonl` files under `dir` (depth-bounded, symlinks not followed).
pub fn transcripts_under(dir: &Path) -> Vec<PathBuf> {
    fn walk(dir: &Path, depth: usize, out: &mut Vec<PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            let path = entry.path();
            if file_type.is_dir() {
                if depth < MAX_WALK_DEPTH {
                    walk(&path, depth + 1, out);
                }
            } else if file_type.is_file() && path.extension().is_some_and(|ext| ext == "jsonl") {
                out.push(path);
            }
        }
    }
    let mut out = Vec::new();
    walk(dir, 1, &mut out);
    out
}

/// The Claude transcript store, tailed.
#[derive(Debug, Default)]
pub struct ClaudeSource {
    /// Overrides [`crate::transcript_tokens::claude_projects_dir`] (tests).
    pub projects_dir: Option<PathBuf>,
    files: TailSet<()>,
    messages: ClaudeMessages,
}

impl BurnSource for ClaudeSource {
    fn poll(&mut self, emit: Emit, out: &mut Vec<BurnEvent>) {
        let Some(dir) = self
            .projects_dir
            .clone()
            .or_else(crate::transcript_tokens::claude_projects_dir)
        else {
            return;
        };
        let messages = &mut self.messages;
        self.files
            .poll(transcripts_under(&dir), emit.active_since(), |(), line| {
                messages.add_line(line, emit, out);
            });
        self.messages.prune(emit.now);
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
#[path = "claude_tests.rs"]
mod tests;
