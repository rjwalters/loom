//! Rendering a validated [`Decision`] into an issue body, and parsing it back.
//!
//! The managed section is fenced by two HTML comments so a re-apply finds and
//! REPLACES it rather than stacking a second decision on top. The section's
//! first fenced block is the canonical ```` ```decision ```` JSON — loom-ui
//! reads the first such block — followed by a readable ranked list for humans
//! reading the issue on the forge.

use super::Decision;

/// Opens the helper-managed decision section.
pub const SECTION_START: &str = "<!-- loom:operator-decision:start -->";
/// Closes the helper-managed decision section.
pub const SECTION_END: &str = "<!-- loom:operator-decision:end -->";
/// Heading the pre-existing body is kept under when a decision is prepended.
pub const ORIGINAL_REPORT_HEADING: &str = "## Original report";

const FENCE_OPEN: &str = "```decision";
const FENCE_CLOSE: &str = "```";

/// Collapse a field onto one line for the readable list (the JSON block keeps
/// the exact text).
fn one_line(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The fenced ```` ```decision ```` block alone, newline-terminated. Array
/// order is the ranking.
#[must_use]
pub fn render_block(d: &Decision) -> String {
    // Serializing a plain struct of strings cannot fail.
    let json = serde_json::to_string_pretty(d).unwrap_or_default();
    format!("{FENCE_OPEN}\n{json}\n{FENCE_CLOSE}\n")
}

/// The readable ranked list (`/repo:decide` text form), newline-terminated.
#[must_use]
pub fn render_list(d: &Decision) -> String {
    let mut out = format!("**Decision needed:** {}\n", one_line(&d.question));
    if !d.context.trim().is_empty() {
        out.push_str(&format!("\n{}\n", d.context.trim()));
    }
    out.push_str("\nOptions, ranked best to worst:\n\n");
    let rec = d.recommended.as_deref().map(str::trim);
    for (i, o) in d.options.iter().enumerate() {
        let tag = if rec == Some(o.id.trim()) {
            " (recommended)"
        } else {
            ""
        };
        out.push_str(&format!(
            "{}. **{}**{tag}: {}\n",
            i + 1,
            one_line(&o.label),
            one_line(o.why.as_deref().unwrap_or(""))
        ));
    }
    if let Some(deadline) = d.deadline.as_deref().filter(|s| !s.trim().is_empty()) {
        out.push_str(&format!("\nDeadline: {}\n", one_line(deadline)));
    }
    if !d.context_links.is_empty() {
        out.push_str("\nContext:\n");
        for l in &d.context_links {
            out.push_str(&format!("- {}\n", one_line(l)));
        }
    }
    out
}

/// The whole managed section: markers, block, list.
#[must_use]
pub fn render_section(d: &Decision) -> String {
    format!("{SECTION_START}\n{}\n{}{SECTION_END}\n", render_block(d), render_list(d))
}

/// The first ```` ```decision ```` block's JSON text in `body`, if any.
#[must_use]
pub fn extract_block(body: &str) -> Option<String> {
    let mut lines = body.lines();
    lines.find(|l| l.trim_end() == FENCE_OPEN)?;
    let mut json = Vec::new();
    for l in lines {
        if l.trim() == FENCE_CLOSE {
            return Some(json.join("\n"));
        }
        json.push(l);
    }
    None
}

/// The first decision block in `body`, parsed. `None` when there is no
/// block or it is not decision JSON.
#[must_use]
pub fn parse_block(body: &str) -> Option<Decision> {
    serde_json::from_str(&extract_block(body)?).ok()
}

/// `body` with the managed section removed, plus any stray (hand-written)
/// ```` ```decision ```` fenced blocks — the new section supersedes them.
fn strip_prior_decision(body: &str) -> String {
    let mut text = body.to_string();
    while let Some(start) = text.find(SECTION_START) {
        let Some(rel_end) = text[start..].find(SECTION_END) else {
            break;
        };
        let end = start + rel_end + SECTION_END.len();
        text.replace_range(start..end, "");
    }
    let mut out = Vec::new();
    let mut in_block = false;
    for l in text.lines() {
        if !in_block && l.trim_end() == FENCE_OPEN {
            in_block = true;
            continue;
        }
        if in_block {
            if l.trim() == FENCE_CLOSE {
                in_block = false;
            }
            continue;
        }
        out.push(l);
    }
    out.join("\n")
}

/// The new issue body: the decision section first, the prior body after it
/// under [`ORIGINAL_REPORT_HEADING`]. Idempotent — composing the result again
/// with the same decision yields the same body, and composing it with a NEW
/// decision replaces the section instead of stacking a second one.
#[must_use]
pub fn compose_body(existing: &str, d: &Decision) -> String {
    let section = render_section(d);
    let rest = strip_prior_decision(existing);
    let rest = rest.trim();
    if rest.is_empty() {
        return section;
    }
    if rest.starts_with(ORIGINAL_REPORT_HEADING) {
        format!("{section}\n{rest}\n")
    } else {
        format!("{section}\n{ORIGINAL_REPORT_HEADING}\n\n{rest}\n")
    }
}
