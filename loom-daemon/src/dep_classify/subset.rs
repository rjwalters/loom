//! Startable-subset extraction — the Rust port of `detect-startable-subset.sh`
//! (epic #7810, PR 3).
//!
//! An issue that is blocked by a dependency may still declare a **startable
//! subset**: work it can begin now, independent of the blocker. Champion uses
//! that to un-escalate a proposal that was parked on a blocker covering only
//! part of its scope — the mistake being the parking, not the waiting.
//!
//! The shell original located the section with an `awk` program embedded in a
//! function; the rules it encodes are preserved exactly here, because
//! `tests/test-detect-startable-subset.sh` drives them through the CLI and must
//! keep passing unchanged.

/// The heading depth range a `## Startable subset` heading may use.
///
/// Matches the shell's `/^#{2,6}[[:space:]]+/`: a top-level `#` is the issue
/// title, never a section, so it is deliberately excluded.
const MIN_DEPTH: usize = 2;
const MAX_DEPTH: usize = 6;

/// The text under an issue's "Startable subset" heading, or empty when there is
/// none.
///
/// Capture starts at a heading whose text begins (case-insensitively) with
/// `startable subset`, and ends at the next heading of the **same or shallower**
/// depth. A deeper subsection — `### Files` under `## Startable subset` — stays
/// inside the captured text, which is why depth is compared rather than simply
/// stopping at the next heading.
#[must_use]
pub fn extract_startable_subset(body: &str) -> String {
    let mut out: Vec<&str> = Vec::new();
    let mut capturing_at: Option<usize> = None;

    for line in body.lines() {
        let stripped = line.trim_start();
        let depth = heading_depth(stripped);

        match capturing_at {
            None => {
                // Only a heading can open a capture, and only if its text names
                // the section.
                if let Some(d) = depth {
                    if (MIN_DEPTH..=MAX_DEPTH).contains(&d) && is_startable_heading(stripped, d) {
                        capturing_at = Some(d);
                    }
                }
                // Everything before the heading is skipped, including the
                // heading line itself — the section's *content* is the answer.
            }
            Some(open_depth) => {
                // A heading at the same or shallower depth closes the section.
                // `<=` is the whole reason depth is tracked: `###` under `##`
                // belongs to the subset, `##` after `##` does not.
                if let Some(d) = depth {
                    if d <= open_depth {
                        capturing_at = None;
                        continue;
                    }
                }
                out.push(line);
            }
        }
    }

    if out.is_empty() {
        String::new()
    } else {
        // The shell's `print line` emits each captured line with a trailing
        // newline, so the result ends with one.
        let mut s = out.join("\n");
        s.push('\n');
        s
    }
}

/// Whether `body` declares a non-blank startable subset.
///
/// Blank-but-present is treated as absent, matching the shell's
/// `[[ -n "${text//[[:space:]]/}" ]]` — a heading with nothing under it is not a
/// subset anyone can start.
#[must_use]
pub fn has_startable_subset(body: &str) -> bool {
    !extract_startable_subset(body)
        .chars()
        .all(char::is_whitespace)
}

/// The number of leading `#` characters when `stripped` is a heading.
///
/// `None` when the line is not a heading. A run of `#` must be followed by
/// whitespace to count — `#5664` in prose is an issue reference, not a heading,
/// and treating it as one would silently truncate a captured section.
fn heading_depth(stripped: &str) -> Option<usize> {
    let hashes = stripped.chars().take_while(|c| *c == '#').count();
    if hashes == 0 {
        return None;
    }
    match stripped.chars().nth(hashes) {
        Some(c) if c.is_whitespace() => Some(hashes),
        _ => None,
    }
}

/// Whether a heading line names the startable-subset section.
///
/// Prefix match, not equality: the shell uses `~ /^startable subset/`, so
/// "Startable subset (partial)" and "Startable Subset" both qualify.
fn is_startable_heading(stripped: &str, depth: usize) -> bool {
    stripped[depth..]
        .trim_start()
        .to_ascii_lowercase()
        .starts_with("startable subset")
}

#[cfg(test)]
mod tests;
