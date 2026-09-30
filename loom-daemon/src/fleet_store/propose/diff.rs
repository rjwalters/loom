//! A minimal, unified-diff-flavored rendering of one file's `before` ->
//! `after`, for `propose --dry-run` and the PR body.
//!
//! This is deliberately not a general-purpose diff algorithm (no LCS, no
//! dependency): it finds the longest common prefix and suffix of lines and
//! shows only the differing middle, with a little context. That is enough
//! for the small, mostly-untouched fleet-store files this module edits —
//! the whole point of a format-preserving edit ([`super::block`]) is that
//! only one contiguous region changes, so a prefix/suffix diff is already
//! minimal in practice.

/// Lines of context kept on each side of a changed region.
const CONTEXT: usize = 2;

/// Render `before` -> `after` for `path` as a small unified-diff-style hunk.
/// `"{path}: no change\n"` when they are identical.
#[must_use]
pub fn unified(path: &str, before: &str, after: &str) -> String {
    if before == after {
        return format!("{path}: no change\n");
    }
    let b: Vec<&str> = before.lines().collect();
    let a: Vec<&str> = after.lines().collect();

    let mut prefix = 0;
    while prefix < b.len() && prefix < a.len() && b[prefix] == a[prefix] {
        prefix += 1;
    }
    let mut suffix = 0;
    while suffix < b.len() - prefix
        && suffix < a.len() - prefix
        && b[b.len() - 1 - suffix] == a[a.len() - 1 - suffix]
    {
        suffix += 1;
    }

    let ctx_start = prefix.saturating_sub(CONTEXT);
    let b_end = b.len() - suffix;
    let a_end = a.len() - suffix;
    let ctx_end_b = (b_end + CONTEXT).min(b.len());
    let ctx_end_a = (a_end + CONTEXT).min(a.len());

    let mut out = format!("--- a/{path}\n+++ b/{path}\n");
    out.push_str(&format!(
        "@@ -{},{} +{},{} @@\n",
        ctx_start + 1,
        ctx_end_b - ctx_start,
        ctx_start + 1,
        ctx_end_a - ctx_start,
    ));
    for l in &b[ctx_start..prefix] {
        out.push_str(&format!(" {l}\n"));
    }
    for l in &b[prefix..b_end] {
        out.push_str(&format!("-{l}\n"));
    }
    for l in &a[prefix..a_end] {
        out.push_str(&format!("+{l}\n"));
    }
    for l in &b[b_end..ctx_end_b] {
        out.push_str(&format!(" {l}\n"));
    }
    out
}

#[cfg(test)]
#[path = "tests/diff_tests.rs"]
mod tests;
