//! Stage 2: the premise record, and the consistency rules it must satisfy.
//!
//! # The marker
//!
//! ```text
//! <!-- loom:premise-check exists=yes deliberate=yes reversal=yes verdict=operator-decision -->
//! premise-evidence: loom-daemon/src/watchdog/mod.rs:902 — "No automatic kill/restart is attempted (#4398)"
//! ```
//!
//! Anchored as the full `<!-- … -->` comment form with constrained values, the
//! same way `require-complexity-marker.sh` and `classify-ac-verification.sh`
//! anchor theirs (#4840): prose that merely *quotes* the syntax — this doc
//! comment, the role prompts, `premise-gate.md` — can never be mistaken for a
//! live record, because a placeholder like `<verdict>` is not an accepted
//! value.
//!
//! # What the rules actually buy
//!
//! Only one direction is constrained, and it is the permissive one. Routing an
//! issue to a human is always allowed; *declining* to route is what needs a
//! consistent story:
//!
//! - `deliberate=yes reversal=yes` ⇒ `verdict` MUST be `operator-decision`.
//!   This is the whole gate. #7855's Curator wrote, in prose, "it reverses the
//!   documented report-only design … the filing is the ruling" and proceeded.
//!   In this format that sentence does not parse.
//! - `deliberate=yes` ⇒ at least one `premise-evidence:` citation that
//!   **resolves against the tree**. A deliberateness claim with no locator is
//!   not evidence.
//! - `deliberate=no` ⇒ at least one `premise-searched:` citation that
//!   resolves. #8396 requires the check to "search the codebase … don't infer
//!   from absence"; this is the mechanical residue of having searched.
//! - `deliberate=yes reversal=no verdict=clear` ⇒ a non-empty
//!   `premise-extends:` line. "It is deliberate but I am not reversing it" is
//!   a real and common answer; it just has to be said.
//! - `exists=no` ⇒ `deliberate` must be `no`. A behaviour that does not exist
//!   as described cannot have been intended.
//!
//! Nothing here can stop an agent writing `deliberate=no` about a behaviour
//! that is in fact deliberate. What it does is force that claim to be written
//! down, next to the paths it says it read, where Judge and Champion can see
//! it — instead of being an unrecorded inference.

use std::collections::BTreeSet;
use std::path::Path;

/// Does the reported behaviour exist as described? (#8396 criterion (a))
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exists {
    Yes,
    No,
    Unclear,
}

/// What the record concludes the pipeline should do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Curation proceeds normally.
    Clear,
    /// `loom:operator-only` + `loom:operator-decision`, no enrichment pass.
    OperatorDecision,
}

/// The gate's answer once a record has been read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    OutOfScope,
    RecordRequired,
    Proceed,
    RouteOperator,
    PremiseFalse,
    Malformed(String),
}

impl std::fmt::Display for Outcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            Outcome::OutOfScope => "out-of-scope",
            Outcome::RecordRequired => "record-required",
            Outcome::Proceed => "proceed",
            Outcome::RouteOperator => "operator-decision",
            Outcome::PremiseFalse => "premise-false",
            Outcome::Malformed(_) => "record-malformed",
        };
        f.write_str(s)
    }
}

#[derive(Debug, Clone)]
pub struct Record {
    pub exists: Exists,
    pub deliberate: bool,
    pub reversal: bool,
    pub verdict: Verdict,
    /// `premise-evidence:` citations, first token of each line.
    pub evidence: Vec<String>,
    /// `premise-searched:` citations.
    pub searched: Vec<String>,
    /// `premise-extends:` free text.
    pub extends: Option<String>,
}

/// Keys the marker accepts. An unknown key is an error rather than ignored
/// noise: `delibrate=no` silently defaulting to "absent" would be a typo that
/// disarms the gate, which is the one failure mode a gate must not have.
const KNOWN_KEYS: &[&str] = &[
    "exists",
    "deliberate",
    "reversal",
    "verdict",
    "issue",
    "sha",
];

const MARKER_OPEN: &str = "loom:premise-check";

/// The last chunk (body or comment) that carries a well-formed marker opener.
///
/// "Carries an opener" is deliberately looser than "parses": a chunk with a
/// broken marker must be *found* so it can be reported malformed, not skipped
/// in favour of an older valid one.
#[must_use]
pub fn last_chunk_with_marker<'a>(chunks: &[&'a str]) -> Option<&'a str> {
    chunks.iter().rfind(|c| marker_span(c).is_some()).copied()
}

/// The `key=value …` interior of the first `<!-- loom:premise-check … -->` in
/// `chunk`.
fn marker_span(chunk: &str) -> Option<&str> {
    let start = chunk.find(MARKER_OPEN)?;
    // The opener must really be an HTML comment, not a prose mention.
    let before = chunk[..start].trim_end();
    if !before.ends_with("<!--") {
        return None;
    }
    let rest = &chunk[start + MARKER_OPEN.len()..];
    let end = rest.find("-->")?;
    Some(&rest[..end])
}

/// Parse the marker and its companion lines out of one chunk.
///
/// # Errors
///
/// A human-readable reason, printed verbatim as the caller's `REASON=` line.
pub fn parse(chunk: &str) -> Result<Record, String> {
    let span =
        marker_span(chunk).ok_or_else(|| "no loom:premise-check marker found".to_string())?;

    let mut exists = None;
    let mut deliberate = None;
    let mut reversal = None;
    let mut verdict = None;

    for field in span.split_whitespace() {
        let Some((key, value)) = field.split_once('=') else {
            return Err(format!("marker field `{field}` is not key=value"));
        };
        if !KNOWN_KEYS.contains(&key) {
            return Err(format!("unknown marker key `{key}` (known: {})", KNOWN_KEYS.join(", ")));
        }
        match key {
            "exists" => {
                exists = Some(match value {
                    "yes" => Exists::Yes,
                    "no" => Exists::No,
                    "unclear" => Exists::Unclear,
                    _ => return Err(format!("exists={value} (want yes|no|unclear)")),
                });
            }
            "deliberate" => deliberate = Some(parse_bool("deliberate", value)?),
            "reversal" => reversal = Some(parse_bool("reversal", value)?),
            "verdict" => {
                verdict = Some(match value {
                    "clear" => Verdict::Clear,
                    "operator-decision" => Verdict::OperatorDecision,
                    _ => return Err(format!("verdict={value} (want clear|operator-decision)")),
                });
            }
            _ => {}
        }
    }

    Ok(Record {
        exists: exists.ok_or("marker is missing exists=")?,
        deliberate: deliberate.ok_or("marker is missing deliberate=")?,
        reversal: reversal.ok_or("marker is missing reversal=")?,
        verdict: verdict.ok_or("marker is missing verdict=")?,
        evidence: companion_citations(chunk, "premise-evidence:"),
        searched: companion_citations(chunk, "premise-searched:"),
        extends: companion_text(chunk, "premise-extends:"),
    })
}

fn parse_bool(key: &str, value: &str) -> Result<bool, String> {
    match value {
        "yes" => Ok(true),
        "no" => Ok(false),
        _ => Err(format!("{key}={value} (want yes|no)")),
    }
}

/// First whitespace-delimited token after each `prefix:` line — the locator.
/// Everything after it is free-text explanation this never parses.
fn companion_citations(chunk: &str, prefix: &str) -> Vec<String> {
    chunk
        .lines()
        .filter_map(|l| {
            let t = l.trim_start().trim_start_matches(['-', '*', ' ']);
            let rest = t.strip_prefix(prefix)?;
            rest.split_whitespace().next().map(|s| {
                s.trim_matches(|c: char| c == '`' || c == '"' || c == ',')
                    .to_string()
            })
        })
        .filter(|s| !s.is_empty())
        .collect()
}

fn companion_text(chunk: &str, prefix: &str) -> Option<String> {
    chunk.lines().find_map(|l| {
        let t = l.trim_start().trim_start_matches(['-', '*', ' ']);
        let rest = t.strip_prefix(prefix)?.trim();
        (!rest.is_empty()).then(|| rest.to_string())
    })
}

/// Apply the consistency rules.
#[must_use]
pub fn check(rec: &Record, repo_root: &Path) -> Outcome {
    if rec.exists == Exists::No && rec.deliberate {
        return Outcome::Malformed(
            "exists=no with deliberate=yes: a behaviour that does not exist as described \
             cannot have been intended — re-read the cited location"
                .to_string(),
        );
    }

    if rec.deliberate {
        if let Some(bad) = first_unresolvable(&rec.evidence, repo_root) {
            return Outcome::Malformed(format!(
                "premise-evidence: `{bad}` does not resolve in this checkout \
                 (want a repo-relative path, path:line, or ADR-NNNN)"
            ));
        }
        if rec.evidence.is_empty() {
            return Outcome::Malformed(
                "deliberate=yes needs at least one `premise-evidence: <path>[:line] — <what it \
                 asserts>` line citing the comment, ADR, or test that asserts the behaviour as \
                 intended"
                    .to_string(),
            );
        }
        if rec.reversal && rec.verdict != Verdict::OperatorDecision {
            return Outcome::Malformed(
                "deliberate=yes reversal=yes requires verdict=operator-decision. Reversing a \
                 documented decision is an operator ruling; an issue's own filing is never that \
                 approval (#7855, #8309). Route it: \
                 --add-label \"loom:operator-only,loom:operator-decision\""
                    .to_string(),
            );
        }
        if !rec.reversal && rec.verdict == Verdict::Clear && rec.extends.is_none() {
            return Outcome::Malformed(
                "deliberate=yes reversal=no verdict=clear needs a non-empty `premise-extends: \
                 <why this extends rather than reverses the documented decision>` line"
                    .to_string(),
            );
        }
    } else {
        if let Some(bad) = first_unresolvable(&rec.searched, repo_root) {
            return Outcome::Malformed(format!(
                "premise-searched: `{bad}` does not resolve in this checkout \
                 (want a repo-relative path, path:line, or ADR-NNNN)"
            ));
        }
        if rec.searched.is_empty() {
            return Outcome::Malformed(
                "deliberate=no needs at least one `premise-searched: <path>` line naming what was \
                 read. Deliberateness is established by finding an assertion of intent, never \
                 inferred from its absence (#8396)"
                    .to_string(),
            );
        }
    }

    match (rec.verdict, rec.exists) {
        (Verdict::OperatorDecision, _) => Outcome::RouteOperator,
        (Verdict::Clear, Exists::No) => Outcome::PremiseFalse,
        (Verdict::Clear, _) => Outcome::Proceed,
    }
}

/// The first citation that does not resolve, if any.
fn first_unresolvable(citations: &[String], repo_root: &Path) -> Option<String> {
    citations.iter().find(|c| !resolves(c, repo_root)).cloned()
}

/// Does one citation name something that exists in this checkout?
///
/// Accepted: a repo-relative path, the same path with a `:<line>` (or
/// `:<line>-<line>`) suffix, or an `ADR-NNNN` identifier resolving to a file
/// under `docs/adr/`.
#[must_use]
pub fn resolves(citation: &str, repo_root: &Path) -> bool {
    let c = citation.trim_matches(|ch: char| ch == '`' || ch == '"');
    if let Some(n) = adr_number(c) {
        return adr_exists(n, repo_root);
    }
    let path = c
        .split_once(':')
        .map_or(c, |(p, suffix)| {
            if suffix
                .chars()
                .all(|ch| ch.is_ascii_digit() || ch == '-' || ch == 'L')
            {
                p
            } else {
                c
            }
        })
        .trim_start_matches("./");
    if path.is_empty() || path.starts_with('/') || path.contains("..") {
        return false;
    }
    repo_root.join(path).exists()
}

fn adr_number(c: &str) -> Option<u32> {
    let rest = c
        .strip_prefix("ADR-")
        .or_else(|| c.strip_prefix("adr-"))
        .or_else(|| c.strip_prefix("Adr-"))?;
    rest.trim_end_matches(|ch: char| !ch.is_ascii_digit())
        .parse()
        .ok()
}

fn adr_exists(n: u32, repo_root: &Path) -> bool {
    let prefix = format!("{n:04}");
    let Ok(entries) = std::fs::read_dir(repo_root.join("docs/adr")) else {
        return false;
    };
    entries
        .flatten()
        .any(|e| e.file_name().to_string_lossy().starts_with(prefix.as_str()))
}

/// Every distinct path named anywhere in a record — what the caller compares
/// against the evidence scan when reporting an undisposed candidate.
#[must_use]
pub fn cited_paths(rec: &Record) -> BTreeSet<String> {
    rec.evidence
        .iter()
        .chain(rec.searched.iter())
        .map(|c| {
            c.split_once(':')
                .map_or(c.as_str(), |(p, _)| p)
                .trim_start_matches("./")
                .to_string()
        })
        .collect()
}

#[cfg(test)]
mod tests;
