//! Provenance stamps (#9027, harness-ops D33 "provenance policy", lean form).
//!
//! Two surfaces, both rendered here so no shell script owns the format:
//!
//! - **Commit trailers** on agent-created commits — exactly three:
//!   `Loom-Story`, `Loom-Trace-Id` (D32 v1, [`crate::telemetry::trace::story_context`])
//!   and `Loom-Build`. Injected deterministically by [`hooks`] (a git
//!   `commit-msg` shim), never by asking the model.
//! - **One hidden marker line** on PR bodies ([`marker`]):
//!   `<!-- loom:provenance v1 build=… prompts=… sweep=… story=… trace=… host=… base=… -->`.
//!
//! Every field that cannot be determined is the literal [`UNKNOWN`] — never
//! omitted, never guessed (D33 "unknown is explicit").
use std::path::Path;

pub mod hooks;
pub mod marker;

/// The explicit placeholder for a field that exists but could not be determined.
pub const UNKNOWN: &str = "unknown";

/// A field that does not exist by design (no story behind the action, not a
/// sweep, not an Actions run). D33 keeps it distinct from [`UNKNOWN`].
pub const NONE: &str = "none";

/// The three trailer keys, in the order they are written.
pub const TRAILER_KEYS: [&str; 3] = ["Loom-Story", "Loom-Trace-Id", "Loom-Build"];

/// A commit's provenance trailers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Trailers {
    /// `owner/repo#n`, or [`NONE`] — never `unknown`.
    pub story: String,
    /// The D32 v1 trace id (32 hex), or [`UNKNOWN`] when `repo_id` is unresolvable.
    pub trace_id: String,
    /// `<version> <40-hex> clean|dirty|unknown` ([`crate::self_update::BUILD_STAMP`]).
    pub build: String,
}

impl Trailers {
    /// The trailers for `issue` in the checkout at `root`, built by this binary.
    #[must_use]
    pub fn for_issue(root: &Path, issue: u32) -> Self {
        let (story, trace_id) = story_and_trace(root, issue);
        Self {
            story,
            trace_id,
            build: crate::self_update::BUILD_STAMP.to_string(),
        }
    }

    /// `Key: value` lines in [`TRAILER_KEYS`] order.
    #[must_use]
    pub fn lines(&self) -> Vec<String> {
        [&self.story, &self.trace_id, &self.build]
            .iter()
            .zip(TRAILER_KEYS)
            .map(|(value, key)| format!("{key}: {}", sanitize(value)))
            .collect()
    }
}

/// `(owner/repo#n | none, trace_id)` for `issue`. D33: there is no `unknown`
/// story. A checkout with no GitHub `origin` has no story key at all
/// (`none`, trace `unknown`). A GitHub story whose `repo_id` is unresolvable
/// keeps the origin's spelling with trace `unknown` — the only case a real
/// story carries an unknown trace; a name-derived id would be wrong (#9068).
#[must_use]
pub fn story_and_trace(root: &Path, issue: u32) -> (String, String) {
    if let Some(story) = crate::observability::tracing::resolve_story(root, issue) {
        return (story.story, story.context.trace_id.as_str().to_string());
    }
    match crate::release_resolve::host::repo_slug(root) {
        Some(slug) => (format!("{slug}#{issue}"), UNKNOWN.to_string()),
        None => (NONE.to_string(), UNKNOWN.to_string()),
    }
}

/// Reduce a field value to marker/trailer-safe tokens: whitespace-separated
/// runs of `[A-Za-z0-9._:/#+@-]`. Anything else (`=`, `>`, newlines, …)
/// could forge a field or close the HTML comment, so a value containing it
/// becomes [`UNKNOWN`]. An empty value is [`UNKNOWN`] too.
#[must_use]
pub fn sanitize(value: &str) -> String {
    let tokens: Vec<&str> = value.split_whitespace().collect();
    let safe = |t: &&str| {
        t.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b".:/#+@_-".contains(&b))
    };
    if tokens.is_empty() || !tokens.iter().all(safe) {
        return UNKNOWN.to_string();
    }
    tokens.join(" ")
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_keeps_ids_and_refuses_marker_breakers() {
        assert_eq!(sanitize(" 0.19.409  abc  clean "), "0.19.409 abc clean");
        assert_eq!(sanitize("rjwalters/loom#9027"), "rjwalters/loom#9027");
        assert_eq!(sanitize("sweep-issue-307-1790400000"), "sweep-issue-307-1790400000");
        for bad in ["", "   ", "a=b", "x -->", "a\u{0}b", "<!--", "é"] {
            assert_eq!(sanitize(bad), UNKNOWN, "{bad:?}");
        }
    }

    #[test]
    fn trailer_lines_are_exactly_the_three_keys() {
        let trailers = Trailers {
            story: "rjwalters/loom#9027".into(),
            trace_id: "4e99cc89953fb6bd604a06896b83dc81".into(),
            build: "0.19.409 unknown unknown".into(),
        };
        assert_eq!(
            trailers.lines(),
            [
                "Loom-Story: rjwalters/loom#9027",
                "Loom-Trace-Id: 4e99cc89953fb6bd604a06896b83dc81",
                "Loom-Build: 0.19.409 unknown unknown",
            ]
        );
    }
}
