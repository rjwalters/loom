//! The `Shell-Budget-Callout:` declaration (#9297) — the one narrow way a
//! change may add lines to the PORTABLE pool.
//!
//! # Why the portable ratchet needed a carve-out at all
//!
//! Epic #7810 moves logic out of `contract` shell and into `loom-daemon`
//! subcommands. The script's NAME survives (role prompts, CI workflows, hooks
//! and consumer repos all consume it), so what is left behind is a call-site:
//! find the binary, build the argument list, read the result back. Those lines
//! are still shell, still in a `contract` file, and the ratchet in
//! `super::check_against_rev` could not tell them apart from new shell logic —
//! so it refused the migration it exists to encourage. PR #8314 hit exactly
//! that: porting its deny-spec logic into `loom-daemon role-tool-policy` cut
//! its growth from +110 to +38 and the gate still said no, with no override to
//! reach for.
//!
//! # Why this is not simply "an override for portable growth"
//!
//! A bare override would hand back the whole invariant. Four mechanical checks
//! keep it narrow, and none of them is a judgement call:
//!
//!  1. **The named subcommand must exist** in the running binary's own clap
//!     registry. A trailer naming `role-tool-policy` before that subcommand
//!     ships buys nothing.
//!  2. **The declared lines must actually be a call-site.** Credit is measured
//!     from the diff — only added code lines in a hunk that names the
//!     subcommand AND references the daemon binary count.
//!  3. **Capped per subcommand** at [`CALLOUT_CAP`]. A call-site larger than
//!     the `stub` a fully ported file leaves behind is not a call-site.
//!  4. **The grant is `min(declared, measured)`.** Over-declaring buys nothing,
//!     so the trailer cannot be padded "just in case".
//!
//! Everything the parent already enforces still holds: undeclared portable
//! growth is refused, `settled` may not grow, and `check_against_rev`'s
//! per-file protection against recategorising a script to dodge the limit is
//! untouched.

use std::collections::BTreeMap;

use super::declaration::{scan_trailers, MalformedDeclaration};
use super::{code_lines, Budget, PORTABLE};

/// The commit-message trailer that declares call-site lines for logic that
/// moved into a `loom-daemon` subcommand.
pub const CALLOUT_TRAILER: &str = "Shell-Budget-Callout:";

/// The per-subcommand ceiling on declared call-site lines.
///
/// Deliberately the same figure as [`super::STUB_CAP`], for the same reason:
/// under 40 code lines a file cannot be carrying logic. A call-site is
/// strictly less than a fully ported file's leftover glue — find the binary,
/// pass the arguments, read the result — so anything larger is new shell logic
/// wearing a call-site's trailer, and must be argued as such rather than
/// declared.
pub const CALLOUT_CAP: u64 = super::STUB_CAP as u64;

/// A parsed `Shell-Budget-Callout:` trailer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CalloutDeclaration {
    /// The subcommand path as written, e.g. `role-tool-policy` or
    /// `forge check-open-pr`.
    pub subcommand: String,
    /// Call-site lines the author is declaring for it.
    pub lines: u64,
}

impl CalloutDeclaration {
    /// The top-level subcommand — the token clap registers, and the token a
    /// call-site line must name.
    #[must_use]
    pub fn top(&self) -> &str {
        self.subcommand
            .split_whitespace()
            .next()
            .unwrap_or(&self.subcommand)
    }
}

/// How many added code lines the diff actually shows calling a subcommand.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CalloutEvidence {
    /// The declared subcommand path this was attributed to, verbatim.
    pub subcommand: String,
    /// Added code lines in PORTABLE files belonging to a call-site hunk for it.
    pub lines: u64,
}

/// Parse every `Shell-Budget-Callout:` trailer out of a block of commit
/// messages.
///
/// Positional rules are shared with the `Shell-Budget-Growth:` trailer — column
/// 0, outside any fence, never the subject — via
/// [`super::declaration::scan_trailers`]. Accepted shape:
///
/// ```text
/// Shell-Budget-Callout: role-tool-policy +38
/// ```
///
/// The `+` is optional, a trailing `lines` is tolerated, and anything after the
/// count is free prose.
#[must_use]
pub fn parse_callout_declarations(
    text: &str,
) -> (Vec<CalloutDeclaration>, Vec<MalformedDeclaration>) {
    scan_trailers(text, CALLOUT_TRAILER, parse_callout_value)
}

/// Parse a trailer's VALUE — everything after `Shell-Budget-Callout:`.
fn parse_callout_value(value: &str, raw: &str) -> Result<CalloutDeclaration, MalformedDeclaration> {
    let bad = |why: &'static str| MalformedDeclaration {
        line: raw.trim().to_string(),
        why,
    };

    let tokens: Vec<&str> = value.split_whitespace().collect();
    // The count is the first token that is one: everything before it is the
    // subcommand path, everything after is prose. Scanning for it rather than
    // fixing the position lets `forge check-open-pr +12` work without a second
    // grammar.
    let Some(at) = tokens.iter().position(|t| parse_count(t).is_some()) else {
        return Err(bad(
            "no line count — expected `<subcommand> +<n>`, e.g. `role-tool-policy +38`",
        ));
    };
    let Some(lines) = parse_count(tokens[at]) else {
        return Err(bad("line count does not fit in a u64"));
    };

    let path = &tokens[..at];
    if path.is_empty() {
        return Err(bad(
            "names no subcommand — the count must come AFTER the subcommand it is for",
        ));
    }
    if !path.iter().all(|t| is_subcommand_token(t)) {
        return Err(bad(
            "the subcommand must be a bare clap subcommand name (letters, digits, dashes)",
        ));
    }

    Ok(CalloutDeclaration {
        subcommand: path.join(" "),
        lines,
    })
}

/// `+38`, `38` and `38` from `38` — but never a word that merely starts with a
/// digit, which would swallow prose as the count.
fn parse_count(token: &str) -> Option<u64> {
    let digits = token.strip_prefix('+').unwrap_or(token);
    if digits.is_empty() || !digits.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
}

fn is_subcommand_token(token: &str) -> bool {
    !token.is_empty()
        && token.starts_with(|c: char| c.is_ascii_alphanumeric())
        && token
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// Whether a line references the daemon binary at all.
///
/// Real call-sites do not always spell the binary literally — PR #8314's
/// resolves it into `$_loom_policy_bin` first — so this is deliberately a
/// hunk-level signal rather than a per-line one. It is a second, independent
/// thing a would-be gamer has to write; the cap is what actually bounds the
/// damage.
fn mentions_daemon(line: &str) -> bool {
    let lower = line.to_ascii_lowercase();
    lower.contains("loom-daemon") || lower.contains("loom_daemon")
}

/// Whether `haystack` contains `word` delimited by something that is not part
/// of a subcommand name, so `role-tool-policy` does not match inside
/// `my-role-tool-policy-helper`.
fn contains_word(haystack: &str, word: &str) -> bool {
    if word.is_empty() {
        return false;
    }
    let boundary = |c: char| !(c.is_ascii_alphanumeric() || c == '-' || c == '_');
    let bytes = haystack.as_bytes();
    let mut from = 0;
    while let Some(rel) = haystack[from..].find(word) {
        let start = from + rel;
        let end = start + word.len();
        let before_ok = start == 0 || haystack[..start].chars().next_back().is_some_and(&boundary);
        let after_ok = end == bytes.len() || haystack[end..].chars().next().is_some_and(&boundary);
        if before_ok && after_ok {
            return true;
        }
        // Advance past this occurrence's first character, not past the whole
        // match: overlapping candidates are possible for short names.
        from = start + haystack[start..].chars().next().map_or(1, char::len_utf8);
    }
    false
}

/// Measure, from a unified diff, how many added code lines each declared
/// subcommand's call-site is actually made of.
///
/// The unit of attribution is the HUNK, not the line. A call-site is not one
/// line — it is the invocation plus the lines that build its arguments and read
/// its result, and those arrive as one contiguous block. `--unified=0` makes a
/// hunk exactly that block: it carries no context lines, so every added line in
/// it is adjacent in the new file.
///
/// A hunk is credited to a declared subcommand when BOTH hold:
///
///  - one of its added CODE lines names the subcommand (a comment does not
///    count — prose about a port is not a call to it), and
///  - one of its added lines references the daemon binary.
///
/// Only files that are PORTABLE in `now` are considered; a call-site added to a
/// `bootstrap` script is floor growth, which is the `Shell-Budget-Growth:`
/// trailer's business rather than this one's. A hunk is credited at most once,
/// to the first declaration it matches, so two trailers cannot both bill the
/// same lines.
#[must_use]
pub fn measure_evidence(
    diff: &str,
    now: &Budget,
    declared: &[CalloutDeclaration],
) -> Vec<CalloutEvidence> {
    let mut credit: BTreeMap<String, u64> =
        declared.iter().map(|d| (d.subcommand.clone(), 0)).collect();

    let mut file: Option<String> = None;
    let mut in_hunk = false;
    let mut hunk: Vec<&str> = Vec::new();

    for line in diff.lines() {
        if line.starts_with("diff --git ") {
            flush(&mut credit, now, declared, file.as_deref(), &mut hunk);
            file = None;
            in_hunk = false;
        } else if line.starts_with("@@") {
            flush(&mut credit, now, declared, file.as_deref(), &mut hunk);
            in_hunk = true;
        } else if !in_hunk && line.starts_with("+++ ") {
            // Only a header OUTSIDE a hunk: an added line whose content begins
            // `++ ` arrives here as `+++ ` and must stay content.
            flush(&mut credit, now, declared, file.as_deref(), &mut hunk);
            file = parse_new_path(line);
        } else if in_hunk {
            if let Some(added) = line.strip_prefix('+') {
                hunk.push(added);
            }
        }
    }
    flush(&mut credit, now, declared, file.as_deref(), &mut hunk);

    declared
        .iter()
        .map(|d| CalloutEvidence {
            subcommand: d.subcommand.clone(),
            lines: credit.get(&d.subcommand).copied().unwrap_or(0),
        })
        .collect()
}

/// Attribute one finished hunk and reset the accumulator.
fn flush(
    credit: &mut BTreeMap<String, u64>,
    now: &Budget,
    declared: &[CalloutDeclaration],
    file: Option<&str>,
    hunk: &mut Vec<&str>,
) {
    let added = std::mem::take(hunk);
    let Some(path) = file else { return };
    if added.is_empty() || !is_portable(now, path) {
        return;
    }
    if let Some(d) = attribute(&added, declared) {
        let body = added.join("\n");
        *credit.entry(d.subcommand.clone()).or_default() += code_lines(&body) as u64;
    }
}

fn parse_new_path(header: &str) -> Option<String> {
    let path = header.strip_prefix("+++ ")?.trim();
    if path == "/dev/null" {
        return None;
    }
    // git writes `b/<path>`; `--no-prefix` and `/dev/null` are the only other
    // shapes this ever sees.
    Some(path.strip_prefix("b/").unwrap_or(path).to_string())
}

fn is_portable(now: &Budget, path: &str) -> bool {
    now.by_file
        .get(path)
        .is_some_and(|(cat, _)| PORTABLE.contains(&cat.as_str()))
}

/// The first declaration this hunk is a call-site for, if any.
fn attribute<'a>(
    added: &[&str],
    declared: &'a [CalloutDeclaration],
) -> Option<&'a CalloutDeclaration> {
    if !added.iter().any(|l| mentions_daemon(l)) {
        return None;
    }
    declared.iter().find(|d| {
        added.iter().any(|l| {
            let t = l.trim_start();
            !t.is_empty() && !t.starts_with('#') && contains_word(l, d.top())
        })
    })
}

/// How many lines of portable growth the declared call-sites buy.
///
/// # Errors
/// Returns the operator-facing explanation when a declaration is not usable:
/// an unknown subcommand, one over [`CALLOUT_CAP`], or one the diff shows no
/// call-site for. These REFUSE rather than degrade to zero credit, because a
/// declaration that silently buys nothing fails the build with a message about
/// portable growth while the real problem is the trailer — the exact
/// "a typo degrades to no override" defect #8154 was filed for.
pub(super) fn allowance(
    declared: &[CalloutDeclaration],
    subcommands: &[String],
    evidence: &[CalloutEvidence],
) -> Result<u64, String> {
    if declared.is_empty() {
        return Ok(0);
    }
    // Fail closed. An empty registry means the caller could not enumerate the
    // binary's subcommands, not that none exist, and "could not check" must
    // never read as "checked and fine".
    if subcommands.is_empty() {
        return Err(format!(
            "a `{CALLOUT_TRAILER}` trailer was declared but this run could not enumerate \
             `loom-daemon`'s subcommands, so the `the named subcommand must exist` check could \
             not be made. Refusing rather than accepting an unverifiable declaration."
        ));
    }

    // Fold by subcommand FIRST. The cap is per subcommand, not per trailer, so
    // two `+40` trailers naming the same one must not buy 80 lines against the
    // same 40 measured — the cheapest possible way around a cap.
    let mut by_subcommand: BTreeMap<&str, u64> = BTreeMap::new();
    for d in declared {
        let slot = by_subcommand.entry(d.subcommand.as_str()).or_default();
        *slot = slot.saturating_add(d.lines);
    }

    let mut total: u64 = 0;
    for (subcommand, lines) in by_subcommand {
        let d = CalloutDeclaration {
            subcommand: subcommand.to_string(),
            lines,
        };
        let d = &d;
        if !subcommands.iter().any(|s| s == d.top()) {
            return Err(format!(
                "`{CALLOUT_TRAILER} {} +{}` names `{}`, which is not a `loom-daemon` \
                 subcommand.\n\n\
                 The trailer exists for lines that CALL a subcommand that already exists — it \
                 cannot pre-approve a call-site for one that has not shipped yet. Check \
                 `loom-daemon --help` for the spelling, or land the subcommand first and the \
                 call-site after.",
                d.subcommand,
                d.lines,
                d.top()
            ));
        }
        if d.lines > CALLOUT_CAP {
            return Err(format!(
                "`{CALLOUT_TRAILER} {} +{}` exceeds the per-subcommand cap of {CALLOUT_CAP} \
                 line(s).\n\n\
                 A call-site is the invocation, the arguments and the result — smaller than the \
                 `stub` a fully ported file leaves behind. {} lines of shell is logic, and logic \
                 belongs in the subcommand rather than in front of it. Move the rest into \
                 `loom-daemon {}`, or argue the growth on its own terms.",
                d.subcommand,
                d.lines,
                d.lines,
                d.top()
            ));
        }
        let measured = evidence
            .iter()
            .find(|e| e.subcommand == d.subcommand)
            .map_or(0, |e| e.lines);
        if measured == 0 {
            return Err(format!(
                "`{CALLOUT_TRAILER} {} +{}` is declared, but no added line of PORTABLE shell in \
                 this change is a call-site for it.\n\n\
                 A call-site hunk is one where an added CODE line names `{}` and an added line \
                 references the `loom-daemon` binary. A comment mentioning the subcommand is \
                 prose about a port, not a call to it; a call-site in a `bootstrap` or `vendored` \
                 script is floor growth and wants `Shell-Budget-Growth:` instead.\n\n\
                 If the call-site is there, make the invocation and its glue one contiguous block \
                 and name the binary in it. If it is not, drop the trailer.",
                d.subcommand,
                d.lines,
                d.top()
            ));
        }
        // min, not the declared figure: over-declaring must buy nothing, or the
        // trailer becomes a number an author picks rather than a fact about the
        // diff.
        total = total.saturating_add(d.lines.min(measured));
    }
    Ok(total)
}

/// The human-readable summary the gate prints when a callout was honoured.
#[must_use]
pub fn render_granted(declared: &[CalloutDeclaration], evidence: &[CalloutEvidence]) -> String {
    let mut s = String::new();
    for d in declared {
        let measured = evidence
            .iter()
            .find(|e| e.subcommand == d.subcommand)
            .map_or(0, |e| e.lines);
        s.push_str(&format!(
            "  {:>4} line(s) for `loom-daemon {}` ({} declared, {} measured in the diff)\n",
            d.lines.min(measured),
            d.subcommand,
            d.lines,
            measured
        ));
    }
    s
}

#[cfg(test)]
mod tests;
