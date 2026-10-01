//! `session.output` (#9764): live, incremental, issue-correlated agent-output
//! chunks emitted while a run is still active.
//!
//! The second — and with [`crate::telemetry::ci::CiJobLogRecord`], so far the
//! only other — record kind whose body is free text the daemon did not author:
//! Claude Code transcript output, tailed incrementally while the agent runs.
//! The same operator decision #8825 made for CI build logs applies: the
//! **gateway** collector is the redaction boundary
//! (`transform/session_output_redaction` in
//! `defaults/observability/collector/config.yaml`, scoped by
//! [`SESSION_OUTPUT_CHUNK_MARKER_KEY`]), so the daemon sends bounded text
//! as-is and this struct never derives an *attribute* from it — the scrub
//! stage rewrites bodies only, so a body-derived attribute would ride straight
//! past it. That is also why the kind pins its own gate (`13`) instead of
//! sharing [`crate::telemetry::NEW_KIND_SCHEMA_VERSION`]: a backend that is
//! not ready to ingest free-text agent output can refuse exactly this kind
//! without losing any other post-#8921 telemetry.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::telemetry::{RepoVisibility, SessionKind};

/// The gateway's scrub stage is scoped by this key and nothing else, so the
/// body exception cannot silently widen to a third record kind. Emitted by,
/// and only by, `session.output` chunks (see [`SessionOutputRecord`]).
pub const SESSION_OUTPUT_CHUNK_MARKER_KEY: &str = "loom.output.chunk_index";

/// Every secret class the gateway scrubs out of a `session.output` body —
/// byte-identical to the `ci.job.log` list, deliberately: both kinds carry
/// free text the daemon did not author through the same redaction boundary,
/// so one reviewed secret-class list governs both. #9764 does not widen what
/// counts as a secret; it widens which records the existing list applies to.
/// (This is a *reference*, not a copy — the two transforms cannot drift.)
pub const SESSION_OUTPUT_SCRUB_CLASSES: &[&str] = crate::telemetry::ci::CI_LOG_SCRUB_CLASSES;

/// Every attribute key a `session.output` log record can carry. The
/// collector's `transform/privacy` log `keep_keys` must list each one
/// (`defaults/observability/collector/config.yaml`, contract-tested). No key
/// here — and no value at any of these keys — is derived from the chunk's
/// text.
pub const SESSION_OUTPUT_LOG_ATTRIBUTE_KEYS: &[&str] = &[
    // Join keys, absent-never-zero exactly like `session.summary` (#9445):
    "loom.repo",
    "loom.repo.visibility",
    "loom.session_id",
    "loom.parent_session_id",
    "loom.runtime",
    "loom.role",
    "loom.issue",
    "loom.session_kind",
    // Chunk protocol:
    SESSION_OUTPUT_CHUNK_MARKER_KEY,
    "loom.output.chunk_count",
    "loom.output.truncated",
    "loom.output.bytes_total",
];

/// One ≤ 8 KiB chunk of an active agent session's transcript output
/// (`session.output`, #9764).
///
/// # The body is free text, on purpose
///
/// As with `ci.job.log` (#8825), the **gateway** is the redaction boundary:
/// the daemon forwards transcript text bounded but unscrubbed, and the
/// collector's `session.output`-scoped transform redacts it before any sink.
/// Nothing in this struct may ever move output text into an *attribute* —
/// see the module doc.
///
/// # Reconstruction contract
///
/// A session's output is `ORDER BY chunk_index` over the records sharing one
/// `(host_id, session_id)`; `chunk_index` is the session's stable event
/// sequence, so two records at the same `recorded_at` are still
/// distinguishable and ordered. Unlike a completed CI job's log, a live
/// session's chunk total is not known while it runs, so `chunk_count` is
/// chunks-so-far (it grows with the session) — a consumer must not treat it
/// as final until the run's terminal records arrive. `truncated` is true on
/// **every** chunk of a session that hit the per-session byte cap (not just
/// the last), so a single record read in isolation can never read as a
/// complete transcript; `output_bytes_total` counts *observed* output bytes
/// including everything the cap dropped, so the gap is explicit.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionOutputRecord {
    /// Repository the session worked, as the `owner/name` forge slug —
    /// resolved once per session from the workspace's `origin` remote
    /// (`activity::session_context::SessionContext`, the #9445 identity
    /// fix), **omitted, never a directory name**, when no remote answers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
    /// Visibility tag for `repo`. Always the fail-closed `Private` default:
    /// the emitter makes no forge round trip, exactly like the
    /// `session.summary` pass.
    #[serde(default)]
    pub visibility: RepoVisibility,
    /// The session's own stable id — the transcript's `sessionId` (the
    /// subagent file's stem composed onto the parent's for a `subagents/`
    /// transcript), the same identity `session.summary` carries.
    pub session_id: String,
    /// The enclosing parent session's id, for a `subagents/` transcript.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_session_id: Option<String>,
    /// Runtime that wrote the transcript (#8664's `loom.runtime`
    /// vocabulary). The #9764 emitter tails Claude Code transcripts only,
    /// so today it is always `"claude"`; unsupported runtimes are documented
    /// as uncovered, never reported as live.
    pub runtime: String,
    /// Attributed Loom role (`builder`, `judge`, …), when the session's
    /// first user message names one — resolved once per session alongside
    /// the other join keys.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    /// Issue number the session worked, from the #9445 attribution
    /// precedence (slash-command argument, `issue-<N>` worktree,
    /// `feature/issue-<N>` branch). Absent — never guessed from output
    /// text — for an unattributed (interactive) session; `session_kind`
    /// says which case that was.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub issue: Option<u32>,
    /// Which kind of session this was (#9445) — above all, whether its
    /// missing `issue` is deliberate (`interactive`) or a lost attribution.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_kind: Option<SessionKind>,
    /// 0-based position of this chunk in the session's output stream — the
    /// stable per-session event sequence, and the marker key the gateway's
    /// scrub stage is scoped by ([`SESSION_OUTPUT_CHUNK_MARKER_KEY`]).
    pub chunk_index: u32,
    /// Chunks emitted for this session so far, this one included. A live
    /// stream has no final count; this grows with the session.
    pub chunk_count: u32,
    /// True on every chunk of a session that hit the per-session byte cap
    /// (`autonomous.transcriptIngest.liveOutput.maxBytesPerSession`).
    pub truncated: bool,
    /// Total output bytes observed for this session so far, **including**
    /// bytes the cap dropped — so `truncated` records always admit a gap.
    pub output_bytes_total: u64,
    /// The source-event instant this chunk's newest line was written by the
    /// agent — the transcript line's own `timestamp`, not the poll instant
    /// (that rides separately as the OTLP observed-time, so a quiet run is
    /// distinguishable from a stalled export). Falls back to the poll
    /// instant only when the tail carried no parseable source timestamp.
    pub recorded_at: DateTime<Utc>,
    /// This chunk's output text. **Never** promoted to an attribute.
    pub text: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The shared scrub-class list is a reference, not a copy: it cannot
    /// drift from the `ci.job.log` list it deliberately mirrors.
    #[test]
    fn scrub_classes_are_the_ci_list_by_reference() {
        assert!(std::ptr::eq(
            SESSION_OUTPUT_SCRUB_CLASSES,
            crate::telemetry::ci::CI_LOG_SCRUB_CLASSES
        ));
    }

    /// The marker key is one of the declared attribute keys, and the chunk
    /// protocol owns exactly four `loom.output.*` keys.
    #[test]
    fn marker_key_is_in_the_declared_vocabulary() {
        assert!(SESSION_OUTPUT_LOG_ATTRIBUTE_KEYS.contains(&SESSION_OUTPUT_CHUNK_MARKER_KEY));
        let chunk_keys: Vec<_> = SESSION_OUTPUT_LOG_ATTRIBUTE_KEYS
            .iter()
            .filter(|key| key.starts_with("loom.output."))
            .copied()
            .collect();
        assert_eq!(
            chunk_keys,
            [
                "loom.output.chunk_index",
                "loom.output.chunk_count",
                "loom.output.truncated",
                "loom.output.bytes_total",
            ]
        );
    }

    /// A record round-trips, and the serde tag the registry row declares
    /// (`session.output`) is what the kind reports — covered exhaustively by
    /// the `kind_registry` tests; here the payload's own shape is pinned.
    #[test]
    fn record_round_trips_with_optional_fields_absent() {
        let record: SessionOutputRecord = serde_json::from_value(serde_json::json!({
            "session_id": "uuid-a",
            "runtime": "claude",
            "chunk_index": 0,
            "chunk_count": 1,
            "truncated": false,
            "output_bytes_total": 42,
            "recorded_at": "2026-10-01T00:00:00Z",
            "text": "hello",
        }))
        .unwrap();
        assert_eq!(record.repo, None);
        assert_eq!(record.visibility, RepoVisibility::Private);
        assert_eq!(record.issue, None);
        let json = serde_json::to_string(&record).unwrap();
        let decoded: SessionOutputRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(record, decoded);
        assert!(!json.contains("session_kind"), "absent optional stays absent: {json}");
    }
}
