//! Unified-diff splitting for the bot-PR classifier (#4765).
//!
//! Only the workflow version-pin carve-out needs per-file *content*; the
//! allowlist check needs only paths. But the authoritative path list must come
//! from the paginated `gh api .../pulls/N/files --paginate` call, never from
//! `gh pr view --json files`, which silently truncates at 100 entries with no
//! error — the confirmed false-negative mechanism behind #4613. So the caller
//! passes the path list separately and this module only answers "what patch,
//! if any, did the diff carry for path P?".
//!
//! A path present in the file list but missing from the diff is therefore
//! *information*, not a parse failure: the classifier fails such a file closed
//! when it needs its patch, rather than assuming the diff was complete.

use std::collections::HashMap;

/// Split a `git`/`gh pr diff` unified diff into `path -> patch` (the patch
/// being everything after the file's `+++`/`---` header pair).
///
/// Paths are taken from the `+++ b/<path>` header, falling back to `--- a/<path>`
/// for a deletion (`+++ /dev/null`), falling back to the `diff --git` line for
/// a mode-only change that carries neither. Trailing tab-separated metadata
/// (`\t2026-09-18 ...`) is stripped.
#[must_use]
pub fn split_by_file(diff: &str) -> HashMap<String, String> {
    let mut out: HashMap<String, String> = HashMap::new();
    let mut cur_git_path: Option<String> = None;
    let mut cur_minus: Option<String> = None;
    let mut cur_plus: Option<String> = None;
    let mut cur_body = String::new();

    // Close the file currently being accumulated, if any.
    fn flush(
        out: &mut HashMap<String, String>,
        git_path: &Option<String>,
        minus: &Option<String>,
        plus: &Option<String>,
        body: &mut String,
    ) {
        let path = plus
            .clone()
            .or_else(|| minus.clone())
            .or_else(|| git_path.clone());
        if let Some(p) = path {
            out.entry(p).or_default().push_str(body);
        }
        body.clear();
    }

    for line in diff.lines() {
        if let Some(rest) = line.strip_prefix("diff --git ") {
            flush(&mut out, &cur_git_path, &cur_minus, &cur_plus, &mut cur_body);
            cur_git_path = git_header_path(rest);
            cur_minus = None;
            cur_plus = None;
            continue;
        }
        if let Some(rest) = line.strip_prefix("--- ") {
            if cur_minus.is_none() && cur_body.is_empty() {
                cur_minus = header_path(rest);
                continue;
            }
        }
        if let Some(rest) = line.strip_prefix("+++ ") {
            if cur_plus.is_none() && cur_body.is_empty() {
                cur_plus = header_path(rest);
                continue;
            }
        }
        // Everything else (hunk headers, context, +/- content, "index ...",
        // "new file mode ...") is body. Keeping the non-content lines costs
        // nothing: the pin-only check skips anything without a +/- marker.
        cur_body.push_str(line);
        cur_body.push('\n');
    }
    flush(&mut out, &cur_git_path, &cur_minus, &cur_plus, &mut cur_body);
    out
}

/// `a/path` / `b/path` / `/dev/null`, with any trailing `\t<meta>` stripped.
fn header_path(rest: &str) -> Option<String> {
    let rest = rest.split('\t').next().unwrap_or(rest).trim_end();
    if rest == "/dev/null" || rest.is_empty() {
        return None;
    }
    let stripped = rest
        .strip_prefix("a/")
        .or_else(|| rest.strip_prefix("b/"))
        .unwrap_or(rest);
    Some(stripped.to_string())
}

/// `a/x/y b/x/y` — ambiguous when a path contains a space, which is why this
/// is only the last-resort fallback. Split on the first ` b/` that is preceded
/// by something starting `a/`.
fn git_header_path(rest: &str) -> Option<String> {
    let rest = rest.trim();
    if let Some(idx) = rest.find(" b/") {
        let a = &rest[..idx];
        return Some(a.strip_prefix("a/").unwrap_or(a).to_string());
    }
    None
}

/// Every path the diff itself mentions — the fallback file list when the
/// caller did not supply the authoritative one.
#[must_use]
pub fn paths(diff: &str) -> Vec<String> {
    let mut v: Vec<String> = split_by_file(diff).into_keys().collect();
    v.sort();
    v
}

#[cfg(test)]
mod tests;
