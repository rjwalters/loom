//! The `<!-- loom:remainder-hold reason="..." -->` issue-body marker (#9264).
//!
//! A `Part of #N` slice that leaves a remainder only a human can finish
//! declares it in the issue body. The post-merge partial-increment reset then
//! swaps `loom:building` -> `loom:operator` instead of `loom:issue`, so the
//! work finder never dispatches a builder at scope no builder can finish.
//!
//! Trust: the body is forge-controlled text, but the marker can only keep an
//! issue OUT of the ready queue, so it is honoured from any author (the same
//! reasoning as `PERMANENT_BLOCK_MARKER`). The reason is one line by
//! construction and is only ever quoted, never evaluated. A marker inside a
//! fenced code block is documentation, not a declaration.

use crate::merge_pr::refs::strip_fenced_code_blocks;
use regex::Regex;
use serde_json::Value;
use std::sync::OnceLock;

/// The label a held remainder is parked under.
pub const HELD_LABEL: &str = "loom:operator";

fn marker_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"<!--[ \t]*loom:remainder-hold\b([^\n]*?)-->").expect("static regex")
    })
}

fn reason_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r#"reason[ \t]*=[ \t]*"([^"\n]*)""#).expect("static regex"))
}

/// The hold declared by an issue body: `None` when there is no marker,
/// `Some(reason)` when there is one (`""` when the reason is absent or empty).
#[must_use]
pub fn parse_hold(body: &str) -> Option<String> {
    let visible = strip_fenced_code_blocks(body);
    let caps = marker_re().captures(&visible)?;
    let attrs = caps.get(1).map_or("", |m| m.as_str());
    let reason = reason_re()
        .captures(attrs)
        .and_then(|c| c.get(1))
        .map_or("", |m| m.as_str().trim());
    Some(reason.replace('\t', " "))
}

/// The hold declared by a (possibly multi-document) `gh api` issue read, from
/// the string `.body` fields of its documents.
#[must_use]
pub fn hold_from_json(json: &str) -> Option<String> {
    let body = serde_json::Deserializer::from_str(json)
        .into_iter::<Value>()
        .map_while(Result::ok)
        .filter_map(|d| d.get("body").and_then(Value::as_str).map(str::to_owned))
        .collect::<Vec<_>>()
        .join("\n");
    parse_hold(&body)
}

/// The reason as shown in log text and comments.
#[must_use]
pub fn reason_text(reason: &str) -> String {
    if reason.is_empty() {
        "reason unstated".to_string()
    } else {
        format!("reason: {reason}")
    }
}
