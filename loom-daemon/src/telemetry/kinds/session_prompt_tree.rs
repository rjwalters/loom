//! `session.prompt_tree` (#12509): the checkout an attended session started
//! in, measured against GitHub's tip.
//!
//! Daemon role ticks already record the prompt tree they launch from
//! (`role_tick.outcome`'s `loom.prompt.*`, #11989). An attended session (a
//! person's Claude Code session on a Mac or an AWS box) reports nothing, so
//! "how often does a hand-driven session run from a stale or divergent
//! checkout" had no answer. A user-scope `SessionStart` hook calls
//! `loom-daemon prompt-tree session-start`, which emits one of these.
//!
//! **OTLP-only** (`otlp: Logs`, `native: false`). `host` rides on the Resource
//! as `host.name`/`host.id`. **No credential, token or URL can reach the
//! record**: the only remote fact is `owner/repo`, and the transcript is
//! never read.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::role_tick_telemetry::prompt_tree::PromptTree;

/// Every log attribute key `session.prompt_tree` exports beyond `loom.kind`.
/// The collector's log `keep_keys` must list each (contract-tested).
pub const SESSION_PROMPT_TREE_LOG_ATTRIBUTE_KEYS: &[&str] = &[
    "loom.session.output.launch",
    "loom.session_id",
    "loom.repo",
    "loom.prompt.commit",
    "loom.prompt.loom_version",
    "loom.prompt.commits_behind",
    "loom.prompt.on_branch",
    "loom.prompt.remote_tip",
    "loom.prompt.origin_ref_stale",
    "loom.prompt.remote_repo",
    "loom.prompt.remote_foreign",
];

/// The `session.prompt_tree` record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionPromptTreeRecord {
    /// When the tree was read; the record time.
    pub captured_at: DateTime<Utc>,
    /// The Claude Code session id (an opaque id, never content).
    pub session_id: String,
    /// What started the session: `startup`, `resume`, `clear` or `compact`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// The checkout the session started in.
    pub prompt: PromptTree,
}
