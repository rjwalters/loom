//! Verdict-finding extraction — the Rust port of
//! `classify-dependency-block.sh`'s `extract_findings` (epic #7810, PR 3).
//!
//! Pulls the bullet list out of a Champion verdict comment, so each finding can
//! be classified as timing or merits. One finding per output line, with wrapped
//! continuation lines folded back onto their bullet.
//!
//! # The rules, as the `awk` encoded them
//!
//! - A line whose first non-space character is `-` or `*` **starts** a bullet.
//! - Once bullets have started, an **indented, non-blank** line continues the
//!   current bullet, joined with a single space.
//! - Any other line after bullets have started **ends the list** — the verdict's
//!   prose resumes, and the rest is not findings.
//! - A `**Recommended actions` heading ends the list wherever it appears, even
//!   before any bullet.
//!
//! The "ends the list" rules are the load-bearing ones. A verdict comment
//! continues past its findings into remediation prose that also contains issue
//! references; reading that as findings would classify a merits verdict as a
//! dependency and un-escalate work a human parked on purpose.

/// The findings in `comment`, one per line.
///
/// Returns an empty string when the comment has no bullet list.
#[must_use]
pub fn extract_findings(comment: &str) -> String {
    let mut out: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut started = false;

    // `flush` prints the accumulated bullet if non-empty and clears it.
    macro_rules! flush {
        () => {
            if !cur.is_empty() {
                out.push(std::mem::take(&mut cur));
            }
        };
    }

    for line in comment.lines() {
        if is_recommended_actions(line) {
            flush!();
            return joined(out);
        }

        if starts_bullet(line) {
            flush!();
            cur = line.to_string();
            started = true;
            continue;
        }

        if started {
            if is_indented_continuation(line) {
                cur.push(' ');
                cur.push_str(line);
                continue;
            }
            // Prose resumed: the findings list is over.
            flush!();
            return joined(out);
        }
    }

    flush!();
    joined(out)
}

/// `/^[[:space:]]*\*\*Recommended actions/`
fn is_recommended_actions(line: &str) -> bool {
    line.trim_start().starts_with("**Recommended actions")
}

/// `/^[[:space:]]*[-*][[:space:]]/` — a bullet marker followed by whitespace.
///
/// The trailing whitespace requirement matters: `*emphasis*` at the start of a
/// line is prose, not a bullet.
fn starts_bullet(line: &str) -> bool {
    let t = line.trim_start();
    let mut chars = t.chars();
    matches!(
        (chars.next(), chars.next()),
        (Some('-' | '*'), Some(c)) if c.is_whitespace()
    )
}

/// `/^[[:space:]]+[^[:space:]]/` — starts with whitespace, then has content.
///
/// A blank line is NOT a continuation: it has no non-space character, so it
/// falls through to the "prose resumed" branch and ends the list.
fn is_indented_continuation(line: &str) -> bool {
    let mut chars = line.chars();
    match chars.next() {
        Some(c) if c.is_whitespace() => chars.any(|c| !c.is_whitespace()),
        _ => false,
    }
}

/// Join bullets one per line, with a trailing newline when non-empty —
/// matching `awk`'s `print`.
fn joined(out: Vec<String>) -> String {
    if out.is_empty() {
        String::new()
    } else {
        let mut s = out.join("\n");
        s.push('\n');
        s
    }
}

#[cfg(test)]
mod tests;
