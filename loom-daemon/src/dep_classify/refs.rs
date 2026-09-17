//! Dependency-reference parsing — the Rust port of
//! `detect-dependency-cycle.sh`'s `parse_dependency_refs` (epic #7810, PR 3).
//!
//! Finds the issues a body declares itself blocked by, normalised to
//! `owner/repo#N` so references from different spellings compare equal.
//!
//! # The shell's shape, preserved
//!
//! The original is a three-stage pipeline:
//!
//! ```text
//! grep -E '(Blocked by|Depends on|Requires)…'   # keep LINES that declare a dep
//!   | grep -oE '…#[0-9]+|https?://…/issues/[0-9]+'  # extract EVERY ref on them
//!   | …normalise…
//!   | sort -u
//! ```
//!
//! Two consequences of that shape are load-bearing and easy to lose in a
//! rewrite:
//!
//! **The second stage scans the whole line, not the text after the phrase.** So
//! `Blocked by #3 (see also #99)` yields *both* `#3` and `#99`. That is
//! over-capture, but it is the behaviour every existing fixture and every live
//! issue body has been read against, so the port keeps it. (Contrast
//! `classify-dependency-block.sh`'s own `is_dependency_finding`, which
//! deliberately adds a proximity window — a different function with a different
//! job, and the subject of #7877.)
//!
//! **The phrase match is case-sensitive.** `grep -E` without `-i`, so
//! `blocked by #3` in lower case does not register. Preserved deliberately.

use regex::Regex;
use std::collections::BTreeSet;
use std::sync::OnceLock;

/// Lines that declare a dependency. Case-sensitive, matching `grep -E`.
fn line_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"(Blocked by|Depends on|Requires)[*_:\s]*(([A-Za-z0-9._-]+/[A-Za-z0-9._-]+)?#[0-9]+|https?://[^\s),]+/issues/[0-9]+)",
        )
        .expect("static dependency-phrase pattern")
    })
}

/// Every reference on such a line.
fn ref_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"([A-Za-z0-9._-]+/[A-Za-z0-9._-]+)?#[0-9]+|https?://[^\s),]+/issues/[0-9]+")
            .expect("static reference pattern")
    })
}

/// Every reference anywhere in the text, with **no** phrase gate, and `/pull/`
/// URLs as well as `/issues/`.
fn bare_ref_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"([A-Za-z0-9._-]+/[A-Za-z0-9._-]+)?#[0-9]+|https?://[^\s),]+/(issues|pull)/[0-9]+",
        )
        .expect("static bare reference pattern")
    })
}

/// Dependency references in `body`, normalised to `owner/repo#N`, sorted and
/// deduplicated.
///
/// `default_repo` supplies the owner/repo for bare `#N` references. Sorting is
/// byte-wise over the normalised strings, matching `sort -u` under the `C`
/// collation the scripts run with, and `BTreeSet` gives both properties at once.
#[must_use]
pub fn parse_dependency_refs(body: &str, default_repo: &str) -> Vec<String> {
    let mut out: BTreeSet<String> = BTreeSet::new();

    for line in body.lines() {
        if !line_re().is_match(line) {
            continue;
        }
        for m in ref_re().find_iter(line) {
            if let Some(normalised) = normalise(m.as_str(), default_repo) {
                out.insert(normalised);
            }
        }
    }

    out.into_iter().collect()
}

/// Every reference in `text`, normalised, sorted and deduplicated — the Rust
/// port of `classify-dependency-block.sh`'s `_extract_refs`.
///
/// # Why this is not [`parse_dependency_refs`]
///
/// They are two parsers with two jobs, and the difference is deliberate — the
/// shell used each in exactly one place.
///
/// This one is applied to **findings text that has already been established as
/// dependency-only**. By that point every reference on it IS a blocker, so
/// there is nothing left to gate on: no `Blocked by` / `Depends on` / `Requires`
/// phrase is required, and none of the case-sensitivity that phrase match
/// implies applies. A Champion verdict that says "blocked by #9 (the harness
/// PR), still open" in lower case names a real blocker, and gating it on the
/// capitalised phrase would silently drop it — which is exactly what
/// [`parse_dependency_refs`] would do here.
///
/// It also accepts `/pull/` URLs, which [`parse_dependency_refs`] does not: a
/// verdict routinely cites the PR that will unblock the work, and a merged PR
/// resolves the block just as a closed issue does.
///
/// [`parse_dependency_refs`] keeps the phrase gate because it reads a whole
/// issue **body**, where most `#N` mentions are not dependencies at all.
#[must_use]
pub fn extract_refs(text: &str, default_repo: &str) -> Vec<String> {
    let mut out: BTreeSet<String> = BTreeSet::new();
    for m in bare_ref_re().find_iter(text) {
        if let Some(normalised) = normalise(m.as_str(), default_repo) {
            out.insert(normalised);
        }
    }
    out.into_iter().collect()
}

/// Normalise one raw reference to `owner/repo#N`.
///
/// `None` when the reference cannot be resolved to that shape — a URL whose
/// path is too short to carry both an owner and a repo. The shell expresses
/// this as `[[ "$rest" == */* ]] && printf …`, i.e. it silently emits nothing,
/// so dropping is the faithful behaviour rather than an error.
fn normalise(raw: &str, default_repo: &str) -> Option<String> {
    if raw.starts_with("http") {
        // https://<host>/<owner>/<repo>/(issues|pull)/<N> — the shell drops the
        // last two path segments (`${ref%/*/*}`) rather than naming `issues`,
        // so a `/pull/` URL normalises the same way.
        let num = raw.rsplit('/').next()?;
        let rest = raw.rsplit_once('/')?.0.rsplit_once('/')?.0; // https://<host>/<owner>/<repo>
        let rest = rest.split_once("://")?.1; //                   <host>/<owner>/<repo>
        let rest = rest.split_once('/')?.1; //                     <owner>/<repo>
        if !rest.contains('/') {
            return None;
        }
        Some(format!("{rest}#{num}"))
    } else if let Some(num) = raw.strip_prefix('#') {
        Some(format!("{default_repo}#{num}"))
    } else {
        // Already `owner/repo#N`.
        Some(raw.to_string())
    }
}

#[cfg(test)]
mod tests;
