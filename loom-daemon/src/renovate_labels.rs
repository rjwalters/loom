//! The Renovate-side `loom:review-requested` routing contract (Issue #9418).
//!
//! # The question this answers
//!
//! `defaults/scripts/check-dependabot-labels.sh` (#7577) fails CI when a
//! `.github/dependabot.yml` entry loses `loom:review-requested` — the
//! structural tie that stops the #5455 livelock (an unlabeled dependency PR
//! parked forever in the Judge's unlabeled-PR fallback queue, which applies no
//! labels and so can never clear it). Since #8559, Renovate owns *scheduled*
//! dependency PRs, and its equivalent guarantee is one `"labels"` array in
//! `renovate.json5` that nothing checked. Worse, Renovate's `labels` is not
//! mergeable: a `packageRules` entry (or `vulnerabilityAlerts`,
//! `lockFileMaintenance`, an update-type block, …) that sets its own `labels`
//! **replaces** the top-level array for the PRs it matches, so a well-meant
//! "add a label for majors" rule silently drops the routing label.
//!
//! # What it asserts
//!
//! For every Renovate repo-config file present at a location Renovate reads:
//!
//! 1. the file parses (JSON, or the JSON5 subset this module understands —
//!    comments, trailing commas, single-quoted strings, unquoted keys);
//! 2. the top-level `labels` array exists and contains
//!    [`REQUIRED_LABEL`];
//! 3. every **other** `labels` key anywhere in the tree (a `packageRules`
//!    entry, `vulnerabilityAlerts`, `lockFileMaintenance`, `major`, …) also
//!    contains it. Checking every nested `labels` rather than a hand-kept list
//!    of override sites means a newly-used override site is covered the day it
//!    appears. `addLabels` is additive and is deliberately not checked.
//!
//! No config file at all is a clean no-op: an installed downstream repo need
//! not use Renovate.
//!
//! # Why this is Rust and not more lines in that script
//!
//! `check-dependabot-labels.sh` is `contract`-category shell. Epic #7810's
//! `shell-budget` gate ratchets that pool down, never up, and
//! `.loom/docs/shell-language-policy.md` sends new executable logic to a
//! `loom-daemon` subcommand — the same move [`crate::guard_wiring`] made for
//! #9108.

use std::fmt::Write as _;
use std::path::Path;

use serde_json::Value;

/// The label that routes a dependency PR into the Judge's review queue.
pub const REQUIRED_LABEL: &str = "loom:review-requested";

/// Repo-config locations Renovate reads (`package.json`'s `renovate` key is
/// deprecated upstream and not supported here).
pub const CONFIG_CANDIDATES: &[&str] = &[
    "renovate.json",
    "renovate.json5",
    ".github/renovate.json",
    ".github/renovate.json5",
    ".gitlab/renovate.json",
    ".gitlab/renovate.json5",
    ".renovaterc",
    ".renovaterc.json",
    ".renovaterc.json5",
];

/// One broken invariant, rendered on stderr by the CLI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Violation {
    /// Repo-relative config file path.
    pub file: String,
    /// Where in the config, e.g. `packageRules[1].labels`.
    pub location: String,
    pub detail: String,
}

impl Violation {
    pub fn render(&self) -> String {
        format!("ERROR: {}: {} — {}", self.file, self.location, self.detail)
    }
}

/// Outcome of a run: which config files were checked, and what failed.
#[derive(Debug, Default)]
pub struct Report {
    pub checked: Vec<String>,
    pub violations: Vec<Violation>,
}

/// Check every Renovate config file present under `root`.
pub fn check(root: &Path) -> Report {
    let mut report = Report::default();
    for rel in CONFIG_CANDIDATES {
        let path = root.join(rel);
        if !path.is_file() {
            continue;
        }
        report.checked.push((*rel).to_string());
        match std::fs::read_to_string(&path) {
            Ok(text) => report.violations.extend(check_text(rel, &text)),
            Err(e) => report.violations.push(Violation {
                file: (*rel).to_string(),
                location: "(file)".into(),
                detail: format!("could not be read: {e}"),
            }),
        }
    }
    report
}

/// Check one config file's contents. `file` is only used for messages.
pub fn check_text(file: &str, text: &str) -> Vec<Violation> {
    let violation = |location: &str, detail: String| Violation {
        file: file.to_string(),
        location: location.to_string(),
        detail,
    };
    let value: Value = match json5_to_json(text)
        .map_err(|e| e.to_string())
        .and_then(|json| serde_json::from_str(&json).map_err(|e| e.to_string()))
    {
        Ok(v) => v,
        Err(e) => {
            return vec![violation(
                "(file)",
                format!(
                    "could not be parsed ({e}) — fix the file or extend this check's JSON5 reader"
                ),
            )]
        }
    };
    let Value::Object(top) = &value else {
        return vec![violation("(root)", "is not a JSON object".into())];
    };

    let mut out = Vec::new();
    if !top.contains_key("labels") {
        out.push(violation(
            "labels",
            format!(
                "no top-level `labels` array; Renovate PRs would carry no `{REQUIRED_LABEL}` and land in the Judge's unlabeled fallback queue (#5455)"
            ),
        ));
    }
    walk(&value, "", &mut |location, labels| {
        if let Some(detail) = labels_problem(labels) {
            out.push(violation(location, detail));
        }
    });
    out
}

/// Visit every `labels` value in the tree with its rendered location.
fn walk(value: &Value, at: &str, visit: &mut dyn FnMut(&str, &Value)) {
    match value {
        Value::Object(map) => {
            for (key, child) in map {
                let here = if at.is_empty() {
                    key.clone()
                } else {
                    format!("{at}.{key}")
                };
                if key == "labels" {
                    visit(&here, child);
                } else {
                    walk(child, &here, visit);
                }
            }
        }
        Value::Array(items) => {
            for (i, child) in items.iter().enumerate() {
                walk(child, &format!("{at}[{i}]"), visit);
            }
        }
        _ => {}
    }
}

fn labels_problem(labels: &Value) -> Option<String> {
    let Value::Array(items) = labels else {
        return Some("`labels` is not an array".into());
    };
    if items.iter().any(|l| l.as_str() == Some(REQUIRED_LABEL)) {
        return None;
    }
    Some(format!(
        "does not contain `{REQUIRED_LABEL}`. Renovate's `labels` REPLACES (does not merge with) the top-level array, so the PRs this matches would land in the Judge's unlabeled fallback queue (#5455)"
    ))
}

/// Convert the JSON5 subset Renovate configs use into strict JSON: strips
/// `//` and `/* */` comments, drops trailing commas, re-quotes single-quoted
/// strings, and quotes bare identifier keys. Anything outside that subset is
/// passed through for `serde_json` to reject loudly.
pub fn json5_to_json(text: &str) -> Result<String, &'static str> {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        match c {
            '"' | '\'' => {
                i = copy_string(&chars, i, &mut out)?;
            }
            '/' if chars.get(i + 1) == Some(&'/') => {
                while i < chars.len() && chars[i] != '\n' {
                    i += 1;
                }
            }
            '/' if chars.get(i + 1) == Some(&'*') => {
                i += 2;
                loop {
                    if i + 1 >= chars.len() {
                        return Err("unterminated /* comment");
                    }
                    if chars[i] == '*' && chars[i + 1] == '/' {
                        i += 2;
                        break;
                    }
                    i += 1;
                }
            }
            ',' if next_significant(&chars, i + 1).is_some_and(|n| n == '}' || n == ']') => {
                i += 1;
            }
            c if c.is_ascii_alphabetic() || c == '_' || c == '$' => {
                let start = i;
                while i < chars.len()
                    && (chars[i].is_ascii_alphanumeric() || chars[i] == '_' || chars[i] == '$')
                {
                    i += 1;
                }
                let ident: String = chars[start..i].iter().collect();
                if next_significant(&chars, i) == Some(':') {
                    let _ = write!(out, "\"{ident}\"");
                } else {
                    out.push_str(&ident);
                }
            }
            _ => {
                out.push(c);
                i += 1;
            }
        }
    }
    Ok(out)
}

/// Copy a quoted string starting at `start` as a double-quoted JSON string;
/// returns the index just past its closing quote.
fn copy_string(chars: &[char], start: usize, out: &mut String) -> Result<usize, &'static str> {
    let quote = chars[start];
    out.push('"');
    let mut i = start + 1;
    while i < chars.len() {
        let c = chars[i];
        if c == '\\' {
            let next = *chars.get(i + 1).ok_or("unterminated string")?;
            if next == '\'' {
                out.push('\'');
            } else {
                out.push('\\');
                out.push(next);
            }
            i += 2;
            continue;
        }
        if c == quote {
            out.push('"');
            return Ok(i + 1);
        }
        if c == '"' {
            out.push_str("\\\"");
        } else {
            out.push(c);
        }
        i += 1;
    }
    Err("unterminated string")
}

/// The next character at or after `from` that is not whitespace or inside a
/// comment.
fn next_significant(chars: &[char], from: usize) -> Option<char> {
    let mut i = from;
    while i < chars.len() {
        match chars[i] {
            c if c.is_whitespace() => i += 1,
            '/' if chars.get(i + 1) == Some(&'/') => {
                while i < chars.len() && chars[i] != '\n' {
                    i += 1;
                }
            }
            '/' if chars.get(i + 1) == Some(&'*') => {
                i += 2;
                while i + 1 < chars.len() && !(chars[i] == '*' && chars[i + 1] == '/') {
                    i += 1;
                }
                i += 2;
            }
            c => return Some(c),
        }
    }
    None
}

/// Printed on success.
pub fn ok_message(checked: &[String]) -> String {
    if checked.is_empty() {
        "check-renovate-labels: no Renovate config file — nothing to check (ok).".into()
    } else {
        format!(
            "check-renovate-labels: every `labels` array in {} carries `{REQUIRED_LABEL}` (ok).",
            checked.join(", ")
        )
    }
}

/// Printed after the violations.
pub fn failure_trailer(count: usize) -> String {
    format!(
        "\ncheck-renovate-labels: {count} problem(s). Every Renovate `labels` array must include `{REQUIRED_LABEL}` (#9418; Dependabot-side equivalent: check-dependabot-labels.sh, #7577)."
    )
}

#[cfg(test)]
mod tests;
