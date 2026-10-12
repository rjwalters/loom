//! REST-first reads for the quarantine reconciliation pass (#9243).
//!
//! The pass used to run `gh issue list --label loom:blocked --json
//! number,comments,updatedAt` once per workspace per pass: a GraphQL call that
//! pulled every comment of every blocked issue. GraphQL is the pool that
//! exhausts while REST sits idle, so both legs now go over REST:
//!
//! 1. The `loom:blocked` set comes from [`crate::forge_listing`]'s ETag-cached
//!    issues listing (an unchanged set is a free `304`).
//! 2. Comments (and the timeline) are read per issue only on a
//!    [`QUARANTINE_SCAN`] miss, keyed on the issue's `updatedAt` (a new comment
//!    or label event bumps it), so an unchanged set costs no comment fetch.

use super::{forge, BlockedIssue, MAX_ISSUES_PER_WORKSPACE};
use crate::claim_reconciliation::read_cache::{self, QuarantineScan, QUARANTINE_SCAN};
use anyhow::Result;
use std::path::Path;

/// The open `loom:blocked` issues of `root` (pull requests dropped, capped at
/// [`MAX_ISSUES_PER_WORKSPACE`]) as `(number, updatedAt)`.
///
/// # Errors
///
/// The REST listing failed; the message carries the `gh` stderr tail so the
/// rate-limit classifier sees it unchanged.
pub(super) fn list_blocked(gh_bin: &Path, root: &Path) -> Result<Vec<(u32, Option<String>)>> {
    let rows = crate::forge_listing::list_issues_cached_as(
        "quarantine_reconciliation",
        gh_bin,
        Some(root),
        None,
        "loom:blocked",
        "open",
    )?;
    Ok(rows
        .into_iter()
        .filter(|r| !r.is_pull_request)
        .take(MAX_ISSUES_PER_WORKSPACE as usize)
        .map(|r| (r.number, r.updated_at))
        .collect())
}

/// One uncached read of an issue's scan legs, plus whether every leg answered
/// (a failed comments read is not an answer: it must be retried, not cached).
fn scan_parts(gh_bin: &Path, root: &Path, issue: u32) -> (QuarantineScan, bool) {
    let trusted = forge::trusted_quarantine_comments(gh_bin, root, issue);
    let answered = trusted.is_some();
    let has = trusted.as_ref().is_some_and(|c| !c.is_empty());
    let last_comment =
        trusted.and_then(|c| crate::comment_trust::records::max_timestamp(&c, "created_at"));
    let labeled = has
        .then(|| forge::fetch_last_blocked_labeled_at(gh_bin, root, issue))
        .flatten();
    // A marked issue also needs its timeline leg; an unmarked one is `Keep`
    // unconditionally and needs nothing more.
    let complete = answered && (!has || labeled.is_some());
    ((has, last_comment, labeled), complete)
}

/// Fill in `number`'s scan (trusted marker comment, newest marker, newest
/// `labeled loom:blocked`), reusing the cached answer while `updated_at` holds.
pub(super) fn scan_issue(
    gh_bin: &Path,
    root: &Path,
    number: u32,
    updated_at: Option<&str>,
) -> BlockedIssue {
    let key = read_cache::key(root, number, "quarantine", updated_at);
    let fresh = std::cell::Cell::new(None);
    let cached = QUARANTINE_SCAN.get_or(key, || {
        let (scan, complete) = scan_parts(gh_bin, root, number);
        fresh.set(Some(scan));
        complete.then_some(scan)
    });
    // A miss that did not qualify for the cache still carries what it read.
    let (has, last_comment, labeled) = cached
        .or_else(|| fresh.get())
        .unwrap_or_else(|| scan_parts(gh_bin, root, number).0);
    BlockedIssue {
        number,
        has_quarantine_comment: has,
        last_quarantine_comment_at: last_comment,
        last_blocked_labeled_at: labeled,
    }
}
