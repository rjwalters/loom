//! Dependency-finding classification — the Rust port of
//! `classify-dependency-block.sh`'s `is_dependency_finding` and
//! `findings_are_dependency_only` (epic #7810, PR 3).
//!
//! Decides whether a recurring Champion verdict finding is a **timing** finding
//! (an open dependency) rather than a **merits** one. That distinction drives
//! whether a proposal may be un-escalated once its blocker closes: parking work
//! on a dependency is reversible, rejecting it on merits is not.
//!
//! # Why proximity, and not mere co-occurrence
//!
//! A bullet can mention a phrase-list word while discussing an unrelated
//! reference. #7756's example:
//!
//! > "#7430 (… a **prerequisite** for any meaningful soak) merged only minutes
//! > before this evaluation, so no soak observation window has started yet"
//!
//! Here "prerequisite" explains why the soak has not started; it does not cite
//! #7430 as a blocker. Requiring only "a phrase somewhere AND a reference
//! somewhere" reads that as a dependency and defers the issue forever.
//!
//! So the predicate requires the two to be *near* each other — in either
//! direction:
//!
//! - a reference within [`DEP_REF_WINDOW`] characters **after** any phrase
//!   ("blocked by #3"), or
//! - a reference within [`DEP_REF_LEAD_WINDOW`] characters **before** a bare
//!   phrase ("#3 is blocking this").
//!
//! The lead window exists because the bare forms — `blocker`, `blocking`,
//! `blocks` — are the ones that naturally follow their subject.
//!
//! # Known limitation (#7877), deliberately preserved here
//!
//! The windows are a character-distance proxy for "this phrase names this
//! reference", and 60 characters is too narrow for some real prose:
//!
//! > "Blocked by the sibling Phase 4 issue (run-job seam contract + host
//! > executor)" … #7853
//!
//! classifies as a merits finding, which is wrong. That is #7877. It is **not**
//! fixed in this port: the 252-assertion shell suite pins current behaviour, and
//! changing the predicate in the same change would make a failing assertion
//! ambiguous — port bug, or intended change? The fix lands as its own commit
//! where a diff in the suite is expected and reviewable.

use regex::Regex;
use std::sync::OnceLock;

/// How far **after** a phrase a reference may appear and still count.
///
/// See the module docs: known to be too narrow (#7877), preserved verbatim
/// here so the port is provably behaviour-identical first.
pub const DEP_REF_WINDOW: usize = 60;

/// How far **before** a bare phrase (`blocker`/`blocking`/`blocks`) a reference
/// may appear and still count.
pub const DEP_REF_LEAD_WINDOW: usize = 30;

/// Every dependency phrase, case-insensitive.
fn phrase_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"(?i)(blocked by|blocker|blocking|blocks|depends on|dependent on|dependenc(y|ies) (on|of)|requires|prerequisite|waiting on|waits on|cannot (start|proceed|begin)( work)? until|not (start|begin)able until|must wait (for|until))",
        )
        .expect("static phrase pattern")
    })
}

/// The bare phrases, which tend to FOLLOW their subject ("#3 is blocking this").
fn bare_phrase_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?i)(blocker|blocking|blocks)").expect("static bare pattern"))
}

/// An issue or PR reference. Case-sensitive, matching the shell's `grep -E`.
fn ref_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"([A-Za-z0-9._-]+/[A-Za-z0-9._-]+)?#[0-9]+|https?://[^\s),]+/(issues|pull)/[0-9]+",
        )
        .expect("static reference pattern")
    })
}

/// Whether `bullet` is a dependency (timing) finding rather than a merits one.
#[must_use]
pub fn is_dependency_finding(bullet: &str) -> bool {
    // Both must be present at all before proximity is even considered — the
    // shell's two `grep -q` gates.
    if !phrase_re().is_match(bullet) || !ref_re().is_match(bullet) {
        return false;
    }

    // Trailing window: a reference shortly AFTER a phrase.
    if windows_after(bullet, phrase_re(), DEP_REF_WINDOW)
        .iter()
        .any(|w| ref_re().is_match(w))
    {
        return true;
    }

    // Leading window: a reference shortly BEFORE a bare phrase.
    windows_before(bullet, bare_phrase_re(), DEP_REF_LEAD_WINDOW)
        .iter()
        .any(|w| ref_re().is_match(w))
}

/// Whether every non-blank line in `findings` is a dependency finding.
///
/// `false` for an entirely blank input: "no findings" is not "only dependency
/// findings", and treating it as such would un-escalate a proposal nobody has
/// actually re-evaluated. That is the shell's `saw` flag.
#[must_use]
pub fn findings_are_dependency_only(findings: &str) -> bool {
    let mut saw = false;
    for line in findings.lines() {
        if line.chars().all(char::is_whitespace) {
            continue;
        }
        saw = true;
        if !is_dependency_finding(line) {
            return false;
        }
    }
    saw
}

/// Each phrase match plus up to `n` characters following it.
///
/// Mirrors `grep -oE "${phrase_re}.{0,N}"`. Slicing is done on a char-boundary
/// basis rather than by byte, so a multi-byte character inside the window
/// cannot panic — the shell counts characters here too, since `grep`'s `.`
/// matches a character.
fn windows_after(hay: &str, re: &Regex, n: usize) -> Vec<String> {
    re.find_iter(hay)
        .map(|m| {
            let tail: String = hay[m.start()..]
                .chars()
                .take(m.as_str().chars().count() + n)
                .collect();
            tail
        })
        .collect()
}

/// Up to `n` characters preceding each match, plus the match itself.
///
/// Mirrors `grep -oE ".{0,N}${bare_phrase_re}"`.
fn windows_before(hay: &str, re: &Regex, n: usize) -> Vec<String> {
    re.find_iter(hay)
        .map(|m| {
            let head: Vec<char> = hay[..m.end()].chars().collect();
            let start = head.len().saturating_sub(m.as_str().chars().count() + n);
            head[start..].iter().collect()
        })
        .collect()
}

#[cfg(test)]
mod tests;
