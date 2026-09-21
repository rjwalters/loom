//! The advisory evidence scan: where in this tree does an assertion of intent
//! co-occur with something this issue is about?
//!
//! **This is a search aid, never the enforcement.** #7979 documents at length
//! why literal-text matching over prose is brittle — it fails on rewording,
//! fails on relocation, and red-lined CI on a *correct* fix for ten
//! consecutive commits. So nothing here can fail the gate: the exit code is
//! decided entirely by [`super::scope`] (labels and the issue's own words) and
//! [`super::record`] (the record's internal consistency). What the scan does
//! is make criterion (b) of #8396 — "search the codebase for a comment, an
//! ADR, or a test asserting it as intended, **don't infer from absence**" —
//! something the agent performs against a concrete list.
//!
//! # Shape of the search
//!
//! Two cheap `git grep` passes, intersected:
//!
//! 1. One pass for [`INTENT_MARKERS`] over the whole tree (~0.15 s here).
//! 2. One `git grep -lF` per anchor extracted from the issue.
//!
//! A file is a candidate when it contains both, and an intent line is reported
//! when it sits within [`PROXIMITY_LINES`] of an anchor occurrence in that same
//! file — or anywhere in the file, when the issue cited that file by path.
//! Whole-file co-occurrence alone was tried first and is too coarse: on #7855's
//! text it returned 20 files, most of them unrelated prose that merely uses the
//! word "deliberately" somewhere.

use crate::script_helpers::run_git;
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

/// One reported location.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub path: String,
    pub line: usize,
    pub snippet: String,
    /// Which of the issue's anchors put this file in scope.
    pub anchors: Vec<String>,
    /// True when the issue cited this path directly.
    pub cited: bool,
}

/// Case-insensitive regex alternation for "somebody wrote down that this is on
/// purpose". Recall-tuned: the cost of a spurious candidate is one line the
/// agent skims.
pub const INTENT_MARKERS: &str = "deliberate|deliberately|intentional|intentionally|on purpose|\
     by design|no automatic|never automatically|is not a bug|chosen not to|\
     explicitly rejected|was rejected|must not be|do not implement|not a defect";

/// An anchor matching more files than this is not distinctive enough to say
/// anything (`daemon`, `issue`, `config`), so it is dropped rather than
/// weighted. A count, not a stoplist, because the right list differs per repo
/// and a hardcoded one goes stale.
pub const MAX_ANCHOR_FILES: usize = 40;

/// How far an intent line may sit from an anchor occurrence in the same file.
/// One screenful either way — wide enough to span a wrapped Rust string
/// literal or a doc-comment block, narrow enough that "this file mentions both
/// somewhere" does not qualify.
pub const PROXIMITY_LINES: usize = 60;

/// Most candidates reported from any one file, so the list spans the tree
/// rather than being consumed by whichever document is largest.
pub const MAX_PER_FILE: usize = 2;

/// Anchors never worth grepping: too common in any repo, or noise from
/// markdown/log formatting.
const STOPWORDS: &[&str] = &[
    "https", "http", "true", "false", "null", "none", "todo", "note", "error", "warning", "debug",
    "info", "github", "issue", "issues", "loom", "daemon", "config", "script", "scripts", "test",
    "tests", "main", "origin", "branch", "commit", "repo", "file", "files", "line", "lines",
    "should", "would", "could", "because", "confirm", "check", "checks", "there", "their", "which",
    "where", "while", "after", "before", "about", "still", "other", "every", "under", "above",
];

/// Distinctive tokens and paths the issue is about.
///
/// Two sources, both deliberately narrow: backticked code spans (an author
/// backticks what matters) and SHOUTY tokens in prose (`CONFIRMED`,
/// `DIVERGENCE` — log levels and state names, which is how an incident report
/// names the code it is about). Free prose words are **not** anchors; they
/// match everything.
#[must_use]
pub fn anchors(title: &str, body: &str, repo_root: &Path) -> Vec<String> {
    let text = format!("{title}\n{body}");
    let mut ordered: Vec<String> = Vec::new();
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let push = |s: String, ordered: &mut Vec<String>, seen: &mut BTreeSet<String>| {
        let key = s.to_lowercase();
        if s.len() >= 5 && !STOPWORDS.contains(&key.as_str()) && seen.insert(key) {
            ordered.push(s);
        }
    };

    for span in backticked(&text) {
        let cleaned = span.trim_matches(|c: char| !c.is_alphanumeric() && c != '/' && c != '_');
        if cleaned.is_empty() {
            continue;
        }
        if looks_like_path(cleaned) {
            let p = cleaned.trim_start_matches("./");
            let p = p.split(':').next().unwrap_or(p);
            if repo_root.join(p).exists() {
                push(p.to_string(), &mut ordered, &mut seen);
                continue;
            }
            // A path that does not exist here is still useful as its basename:
            // #8310's whole defect was a citation to a file that had moved.
            if let Some(base) = p.rsplit('/').next() {
                push(base.to_string(), &mut ordered, &mut seen);
            }
            continue;
        }
        for tok in cleaned.split(|c: char| !c.is_alphanumeric() && c != '_' && c != '-') {
            if tok.len() >= 5 {
                push(tok.to_string(), &mut ordered, &mut seen);
            }
        }
    }

    for tok in text.split(|c: char| !c.is_alphanumeric() && c != '_') {
        if tok.len() >= 5 && tok.chars().all(|c| c.is_ascii_uppercase() || c == '_') {
            push(tok.to_string(), &mut ordered, &mut seen);
        }
    }

    ordered
}

fn backticked(text: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(open) = rest.find('`') {
        rest = &rest[open + 1..];
        let Some(close) = rest.find('`') else { break };
        let span = &rest[..close];
        if !span.is_empty() && span.len() <= 200 && !span.contains('\n') {
            out.push(span);
        }
        rest = &rest[close + 1..];
    }
    out
}

fn looks_like_path(s: &str) -> bool {
    s.contains('/') && s.rsplit('/').next().is_some_and(|b| b.contains('.'))
}

/// Pathspecs excluded from both passes: build output, and the `.loom/` install
/// mirror (a symlink to `defaults/` here, so including it double-reports every
/// hit). Same exemption, same reason, as the file-size and shell-allowlist
/// gates.
const EXCLUDES: &[&str] = &[
    ":!target",
    ":!node_modules",
    ":!dist",
    ":!.loom",
    ":!*.lock",
];

/// Run the scan. Returns at most `limit` candidates, best first.
///
/// Never fails: an unavailable `git`, a non-repo root, or a grep that matches
/// nothing all yield an empty list. The scan is advisory, so "could not scan"
/// and "nothing found" are the same non-event to the caller — the exit code
/// does not depend on either.
#[must_use]
pub fn scan(repo_root: &Path, anchors: &[String], limit: usize) -> Vec<Candidate> {
    if anchors.is_empty() || limit == 0 {
        return Vec::new();
    }

    let intent = grep_lines(repo_root, &["-n", "-i", "-E", INTENT_MARKERS]);
    if intent.is_empty() {
        return Vec::new();
    }

    // path -> anchor -> line numbers where that anchor occurs
    let mut anchor_hits: BTreeMap<String, BTreeMap<String, Vec<usize>>> = BTreeMap::new();
    let mut cited_paths: BTreeSet<String> = BTreeSet::new();
    for anchor in anchors {
        if repo_root.join(anchor).is_file() {
            cited_paths.insert(anchor.clone());
            // A path anchor puts its own file in scope even though the literal
            // path string never appears *inside* that file. Without this the
            // one case the issue was most explicit about — "the behaviour is
            // in this file, here is the path" — is the one case that scans to
            // nothing.
            anchor_hits
                .entry(anchor.clone())
                .or_default()
                .entry(anchor.clone())
                .or_default();
        }
        let hits = grep_lines(repo_root, &["-n", "-F", "--", anchor]);
        if hits.len() > MAX_ANCHOR_FILES {
            continue;
        }
        for (path, lines) in hits {
            anchor_hits
                .entry(path)
                .or_default()
                .entry(anchor.clone())
                .or_default()
                .extend(lines.iter().map(|(n, _)| *n));
        }
    }

    let mut out: Vec<(usize, Candidate)> = Vec::new();
    for (path, lines) in &intent {
        let cited = cited_paths.contains(path);
        let Some(by_anchor) = anchor_hits.get(path) else {
            continue;
        };
        let all_anchor_lines: Vec<usize> = by_anchor.values().flatten().copied().collect();
        let names: Vec<String> = by_anchor.keys().cloned().collect();
        for (n, text) in lines {
            let distance = all_anchor_lines
                .iter()
                .map(|a| a.abs_diff(*n))
                .min()
                .unwrap_or(usize::MAX);
            if !cited && distance > PROXIMITY_LINES {
                continue;
            }
            // Lower sorts first: cited paths, then *proximity*, then more
            // distinct anchors as the tiebreak. Proximity has to dominate:
            // ranking on anchor count first put all eight slots inside one
            // 8000-line reference document that happens to mention every
            // anchor somewhere, and buried the one line that actually stated
            // the design ("Report-only, deliberately").
            let rank = (if cited { 0 } else { 1_000_000 })
                + distance.min(999) * 1000
                + (100 - names.len().min(99));
            out.push((
                rank,
                Candidate {
                    path: path.clone(),
                    line: *n,
                    snippet: squeeze(text),
                    anchors: names.clone(),
                    cited,
                },
            ));
        }
    }

    out.sort_by(|a, b| {
        a.0.cmp(&b.0)
            .then_with(|| a.1.path.cmp(&b.1.path))
            .then_with(|| a.1.line.cmp(&b.1.line))
    });

    // At most MAX_PER_FILE lines from any one file. A list of eight hits from
    // a single document tells the reader one thing; eight files tell them
    // where to look, which is the only job this list has.
    let mut per_file: BTreeMap<String, usize> = BTreeMap::new();
    out.into_iter()
        .filter(|(_, c)| {
            let n = per_file.entry(c.path.clone()).or_insert(0);
            *n += 1;
            *n <= MAX_PER_FILE
        })
        .take(limit)
        .map(|(_, c)| c)
        .collect()
}

/// `git grep <args>` over the tracked tree, as `path -> [(line, text)]`.
fn grep_lines(repo_root: &Path, args: &[&str]) -> BTreeMap<String, Vec<(usize, String)>> {
    let mut argv: Vec<&str> = vec!["grep"];
    argv.extend_from_slice(args);
    argv.push("--");
    argv.extend_from_slice(EXCLUDES);
    // `git grep` exits 1 for "no match", which is data rather than failure, so
    // stdout is read regardless of status — it is empty in exactly that case.
    let stdout = run_git(repo_root, &argv).stdout_lossy();
    let mut map: BTreeMap<String, Vec<(usize, String)>> = BTreeMap::new();
    for line in stdout.lines() {
        let Some((path, rest)) = line.split_once(':') else {
            continue;
        };
        let Some((num, text)) = rest.split_once(':') else {
            continue;
        };
        let Ok(n) = num.parse::<usize>() else {
            continue;
        };
        map.entry(path.to_string())
            .or_default()
            .push((n, text.to_string()));
    }
    map
}

/// Collapse whitespace and cap length, so one candidate is one readable line.
fn squeeze(s: &str) -> String {
    let mut out = String::new();
    let mut last_space = true;
    for c in s.chars() {
        if c.is_whitespace() {
            if !last_space {
                out.push(' ');
                last_space = true;
            }
        } else {
            out.push(c);
            last_space = false;
        }
    }
    let trimmed = out.trim();
    if trimmed.chars().count() > 160 {
        trimmed.chars().take(157).collect::<String>() + "..."
    } else {
        trimmed.to_string()
    }
}

#[cfg(test)]
mod tests;
