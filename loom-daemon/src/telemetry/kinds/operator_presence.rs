//! `operator.presence` (#11563): whether a person is at the keyboard of an
//! operator session, and which issues and PRs that session is driving.
//!
//! ETA could not tell operator-driven work that stalls (the operator stepped
//! away) from work that is merely slow. A Claude Code hook in the operator's
//! session calls `loom-daemon presence beat`, which emits one of these on each
//! `UserPromptSubmit` and at most once per throttle window otherwise.
//!
//! **No prompt or transcript content, by construction.** The struct has no
//! text field besides ids and a forge login, and the producer reads only
//! `session_id` and `hook_event_name` out of the hook payload. A prompt cannot
//! reach the wire because there is nowhere to put it.
//!
//! **Exported over OTLP** (`otlp: Logs`, `native: false`) now that #11525
//! settled the transport — "the same path as every other fact". The scalars
//! ride as the [`OPERATOR_PRESENCE_LOG_ATTRIBUTE_KEYS`] `loom.presence.*`
//! attributes (which the collector's log `keep_keys` must list; contract-tested)
//! and the body is the record's JSON, which carries the `driving[]` entries. The
//! local durable journal (`crate::presence`) stays as the buffer the exporter
//! drains; nothing drops one.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Every log attribute key `operator.presence` exports. `host` is not here: it
/// rides on the OTLP Resource as `host.id`, like every other kind. No prompt or
/// transcript content can appear — the record has no such field.
pub const OPERATOR_PRESENCE_LOG_ATTRIBUTE_KEYS: &[&str] = &[
    "loom.presence.session_id",
    "loom.presence.operator",
    "loom.presence.last_input_at",
    "loom.presence.last_activity_at",
    "loom.presence.driving_count",
];

/// One issue or PR the session holds an in-session lease on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DrivenItem {
    /// `issue` or `pr`.
    pub kind: String,
    /// The issue or PR number.
    pub number: u32,
    /// The in-session lease's sweep id (the one in the lease comment).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sweep_id: Option<String>,
}

/// The `operator.presence` record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OperatorPresenceRecord {
    /// When the beat was taken; the record time.
    pub captured_at: DateTime<Utc>,
    /// The host the session runs on.
    pub host: String,
    /// The Claude Code session id (an opaque id, never content).
    pub session_id: String,
    /// The operator's forge login, when the session knows it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operator: Option<String>,
    /// The last `UserPromptSubmit`: a human typed. Absent when none has been
    /// seen in this session: unknown, never a fabricated time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_input_at: Option<DateTime<Utc>>,
    /// The last tool call or Stop.
    pub last_activity_at: DateTime<Utc>,
    /// Issues and PRs this session holds an in-session lease on.
    #[serde(default)]
    pub driving: Vec<DrivenItem>,
}
