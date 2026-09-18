//! The `Shell-Budget-Growth:` declaration (#8154) — its shape, and how a
//! commit message is read for one.
//!
//! Split out of `shell_budget.rs` so the over-threshold parent shrinks rather
//! than grows (`.loom/docs/file-size-policy.md`). The parent owns measurement
//! and the ratchet decision; this owns only what an author writes and how it
//! is parsed.

/// The commit-message trailer that declares deliberate growth in the permanent
/// floor (`bootstrap` / `vendored`).
///
/// The gate's failure message used to end "if that is right, say why in the
/// commit" while `check_against_rev` returned `Err` unconditionally — it
/// promised an escape hatch that did not exist, and three Judge-approved
/// safety PRs sat red against it with no in-repo remedy (#8154). This is that
/// hatch, made real and made narrow.
pub const GROWTH_TRAILER: &str = "Shell-Budget-Growth:";

/// A parsed `Shell-Budget-Growth:` trailer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrowthDeclaration {
    /// Lines of floor growth the author is declaring.
    pub lines: u64,
    /// The stated reason, verbatim after the line count.
    pub reason: String,
    /// The issue the reason references. Required — an override that cites no
    /// issue is a bare escape hatch, which is the thing this must not become.
    pub issue: u64,
}

/// Why a `Shell-Budget-Growth:` line was not accepted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MalformedDeclaration {
    /// The offending line, trimmed.
    pub line: String,
    /// What was wrong with it.
    pub why: &'static str,
}

/// Parse every `Shell-Budget-Growth:` trailer out of a block of commit
/// messages.
///
/// Returns the accepted declarations and, separately, the lines that look like
/// an attempt but are not usable. Malformed lines are reported rather than
/// ignored: a typo'd override that silently degrades to "no override" fails the
/// build with a message about growth, never about the typo, and the author
/// re-reads the wrong thing.
///
/// Accepted shape, liberal about the separator:
///
/// ```text
/// Shell-Budget-Growth: 59 lines — guard against silent revert of a local fix (#7870)
/// ```
#[must_use]
pub fn parse_growth_declarations(
    text: &str,
) -> (Vec<GrowthDeclaration>, Vec<MalformedDeclaration>) {
    let mut ok = Vec::new();
    let mut bad = Vec::new();
    // The marker that opened the current fence, so it is closed only by its
    // own kind. CommonMark does not let ``` close a ~~~ fence, and treating
    // any marker as a toggle let an alternating pair un-fence a quoted example.
    let mut fence: Option<&str> = None;
    let lines: Vec<&str> = text.lines().collect();

    // git's subject is the first NON-BLANK line, not line 0: a
    // `--cleanup=verbatim` message can begin with a blank, which would put a
    // declaration at index 1 while git still calls it the subject.
    let subject_idx = lines.iter().position(|l| !l.trim().is_empty());

    for (i, raw) in lines.iter().enumerate() {
        let trimmed = raw.trim();

        // A fence marker is indented at most 3 spaces; 4 or more makes it
        // literal code in Markdown, and treating it as a fence silently
        // refused a real declaration that followed it.
        let indent = raw.len() - raw.trim_start().len();
        if indent <= 3 {
            if let Some(marker) = ["```", "~~~"].iter().find(|m| trimmed.starts_with(**m)) {
                match fence {
                    None => fence = Some(marker),
                    Some(open) if open == *marker => fence = None,
                    Some(_) => {}
                }
                continue;
            }
        }
        let fenced = fence.is_some();

        let looks_like = strip_trailer_prefix(trimmed).is_some();
        if !looks_like {
            continue;
        }

        // The subject line is never a declaration. git would not treat it as a
        // trailer, and a squash rewrites it to `* Shell-Budget-Growth: …`,
        // where it would silently stop counting.
        if Some(i) == subject_idx {
            bad.push(MalformedDeclaration {
                line: trimmed.to_string(),
                why: "a declaration cannot be the commit SUBJECT — put it in the body",
            });
            continue;
        }

        // Indented or fenced: prose showing the format, not a declaration
        // using it. Reported rather than ignored — a near-miss that silently
        // means "no override" fails the build with a message about growth
        // while the real problem is placement, and the author then re-reads
        // the wrong thing. That is the defect this whole change exists to fix,
        // so it must not be reproduced one level down.
        if fenced || raw.starts_with([' ', '\t']) {
            bad.push(MalformedDeclaration {
                line: trimmed.to_string(),
                why: if fenced {
                    "inside a fenced block, so it reads as an example — move it to column 0 \
                     outside the fence"
                } else {
                    "indented, so it reads as an example — move it to column 0"
                },
            });
            continue;
        }

        let line = raw.trim_end();
        let Some(rest) = strip_trailer_prefix(line) else {
            continue;
        };

        // Fold a continuation: git unfolds a trailer value wrapped onto an
        // indented following line, and an author who wraps a long reason
        // should not silently lose half of it.
        let mut value = rest.to_string();
        let mut j = i + 1;
        while let Some(next) = lines.get(j) {
            if !next.starts_with([' ', '\t']) || next.trim().is_empty() {
                break;
            }
            let nt = next.trim();
            // Stop at anything that is itself a field rather than a
            // continuation. Folding swallowed an indented `Closes #1` into a
            // reason that cited no issue, manufacturing the citation the rule
            // requires — and pulled `Co-Authored-By:` in with it.
            if strip_trailer_prefix(nt).is_some()
                || nt.split_once(':').is_some_and(|(k, _)| {
                    !k.is_empty() && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
                })
                || ["Closes", "Fixes", "Resolves"]
                    .iter()
                    .any(|kw| nt.starts_with(kw))
            {
                break;
            }
            value.push(' ');
            value.push_str(next.trim());
            j += 1;
        }

        match parse_declaration_value(&value, line) {
            Ok(d) => ok.push(d),
            Err(m) => bad.push(m),
        }
    }

    (ok, bad)
}

/// Parse a trailer's VALUE — everything after `Shell-Budget-Growth:`.
///
/// `raw` is the original line, carried only so a malformed report can quote
/// what the author actually wrote.
fn parse_declaration_value(
    value: &str,
    raw: &str,
) -> Result<GrowthDeclaration, MalformedDeclaration> {
    let bad = |why: &'static str| MalformedDeclaration {
        line: raw.trim().to_string(),
        why,
    };
    let rest = value.trim();

    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    if digits.is_empty() {
        return Err(bad("no leading line count — expected `<n> lines — <reason> (#issue)`"));
    }
    let Ok(lines) = digits.parse::<u64>() else {
        return Err(bad("line count does not fit in a u64"));
    };

    let reason = rest[digits.len()..].trim_start();
    let reason = reason
        .strip_prefix("lines")
        .or_else(|| reason.strip_prefix("line"))
        .unwrap_or(reason);
    let reason = reason
        .trim_start()
        .trim_start_matches(['-', '\u{2014}', '\u{2013}', ':'])
        .trim();

    if reason.is_empty() {
        return Err(bad("no reason given after the line count"));
    }
    let Some(issue) = first_issue_reference(reason) else {
        return Err(bad("reason cites no issue — an override must reference `#<issue>`"));
    };

    Ok(GrowthDeclaration {
        lines,
        reason: reason.to_string(),
        issue,
    })
}

/// Case-insensitive match on the trailer key, so `shell-budget-growth:` works.
fn strip_trailer_prefix(line: &str) -> Option<&str> {
    let key = GROWTH_TRAILER;
    // `get` rather than a slice: real commit messages are not ASCII, and
    // `line[..20]` panics outright when byte 20 lands inside a multi-byte
    // character. Scanning the epic's own history hit exactly that on an
    // em-dash — the unit tests were all ASCII and never saw it.
    let head = line.get(..key.len())?;
    if head.eq_ignore_ascii_case(key) {
        Some(&line[key.len()..])
    } else {
        None
    }
}

/// The first `#<digits>` in the text.
fn first_issue_reference(text: &str) -> Option<u64> {
    let bytes = text.as_bytes();
    for (i, b) in bytes.iter().enumerate() {
        if *b != b'#' {
            continue;
        }
        let digits: String = text[i + 1..]
            .chars()
            .take_while(char::is_ascii_digit)
            .collect();
        if !digits.is_empty() {
            return digits.parse().ok();
        }
    }
    None
}
