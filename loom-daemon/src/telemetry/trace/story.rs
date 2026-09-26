//! One issue's end-to-end story trace (#9037). Both ids derive from
//! `(repo, issue)`, so every process on every host — sweep child, CI poller,
//! merge path — recomputes the same context without any propagation.
use super::{SpanId, TraceContext, TraceId};

/// The story root for `repo` (`owner/name`, compared case-insensitively as
/// the forge does) and `issue`. Its span is the parent of every lifecycle
/// span for the issue; the span itself is emitted once the story ends.
#[must_use]
pub fn story_context(repo: &str, issue: u32) -> TraceContext {
    let repo = repo.trim().to_ascii_lowercase();
    let issue = issue.to_string();
    TraceContext {
        trace_id: TraceId::derived(&["loom.story.trace", &repo, &issue]),
        span_id: SpanId::derived(&["loom.story.root", &repo, &issue]),
        flags: 1,
    }
}
