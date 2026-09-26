//! One issue's end-to-end story trace (#9037), keyed per harness-ops D32 v1
//! (`docs/story-trace.md`, #9068). Both ids derive from GitHub's numeric
//! `repo_id` and the issue number, so every emitter on every host — sweep
//! child, CI poller, the harness-ops storyline reconciler — recomputes the
//! same context without propagation, and a repo rename or transfer cannot
//! split a story. The repo *name* is never part of the key.
use super::{SpanId, TraceContext, TraceId};
use sha2::{Digest, Sha256};
use std::fmt;

/// The derivation version stamped as `loom.story.key_version`.
pub const STORY_KEY_VERSION: &str = "v1";

/// D32's closed set of derived story-span kinds. `loom.story` is deliberately
/// absent: the root has exactly one id, [`story_context`]'s `span_id`.
pub const STORY_SPAN_KINDS: [&str; 5] = [
    "story.intake",
    "story.queue_dwell",
    "story.review_wait",
    "story.merge",
    "story.reopened",
];

/// Why a D32 id was refused. Inputs are never escaped or normalised, so every
/// implementation either derives the same id or refuses the same input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StoryIdError {
    /// The derived id is all zero (2⁻¹²⁸ / 2⁻⁶⁴). D32 forbids substituting one.
    ZeroId,
    /// `kind` is not one of [`STORY_SPAN_KINDS`].
    UnknownKind,
    /// `source_event_id` does not match `^[A-Za-z0-9._-]{1,256}$`.
    InvalidEventId,
}

impl fmt::Display for StoryIdError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::ZeroId => "story id derivation produced an all-zero id",
            Self::UnknownKind => "story span kind is not in the D32 v1 allowlist",
            Self::InvalidEventId => "story source_event_id must match ^[A-Za-z0-9._-]{1,256}$",
        })
    }
}

impl std::error::Error for StoryIdError {}

/// `loom-story/v1:github:<repo_id>:<number>` — decimal, no sign, no padding.
#[must_use]
pub fn story_input(repo_id: u64, issue: u32) -> String {
    format!("loom-story/v1:github:{repo_id}:{issue}")
}

/// `hex(sha256(preimage)[0..bytes])`, refusing an all-zero prefix.
fn digest_prefix(preimage: &str, bytes: usize) -> Result<String, StoryIdError> {
    let digest = Sha256::digest(preimage.as_bytes());
    let prefix = &digest[..bytes];
    if prefix.iter().all(|b| *b == 0) {
        return Err(StoryIdError::ZeroId);
    }
    Ok(hex::encode(prefix))
}

/// The story root for GitHub repository `repo_id` and `issue`. Its span is
/// the parent of every lifecycle span for the issue; the root span itself is
/// emitted by the storyline reconciler once the story ends.
pub fn story_context(repo_id: u64, issue: u32) -> Result<TraceContext, StoryIdError> {
    let input = story_input(repo_id, issue);
    let trace_id =
        TraceId::try_from(digest_prefix(&input, 16)?).map_err(|_| StoryIdError::ZeroId)?;
    let span_id = SpanId::try_from(digest_prefix(&format!("{input}:root"), 8)?)
        .map_err(|_| StoryIdError::ZeroId)?;
    // Sampled; the W3C level-2 random flag is NOT set (D32: hashed, not random).
    Ok(TraceContext {
        trace_id,
        span_id,
        flags: 1,
    })
}

/// A derived story-phase span id: `sha256(input:span:<kind>:<event>)[0..8]`.
pub fn story_span_id(
    repo_id: u64,
    issue: u32,
    kind: &str,
    source_event_id: &str,
) -> Result<SpanId, StoryIdError> {
    if !STORY_SPAN_KINDS.contains(&kind) {
        return Err(StoryIdError::UnknownKind);
    }
    if !valid_event_id(source_event_id) {
        return Err(StoryIdError::InvalidEventId);
    }
    let preimage = format!("{}:span:{kind}:{source_event_id}", story_input(repo_id, issue));
    SpanId::try_from(digest_prefix(&preimage, 8)?).map_err(|_| StoryIdError::ZeroId)
}

fn valid_event_id(event: &str) -> bool {
    (1..=256).contains(&event.len())
        && event
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}
