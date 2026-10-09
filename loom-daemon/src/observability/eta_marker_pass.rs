//! The per-pass Champion marker reads of the ETA collector (#10958, Slice 1),
//! split out of [`super`] to keep it under the file-size budget.
//!
//! At most [`MARKER_READ_BUDGET`] ETag'd comment-listing calls per pass,
//! across every repo the pass lists, appended to the marker log beside the
//! fleet snapshots ([`crate::eta::hold_marker_log`]). Logging only: no
//! estimate reads the log yet. Zero calls while the rate-limit breaker
//! suppresses polling.

use std::path::{Path, PathBuf};

use chrono::Utc;

use crate::comment_trust::TrustPolicy;
use crate::eta::hold_marker_log::{self, MarkerCursor, MARKER_READ_BUDGET};
use crate::eta::tracker::{ListedPr, PrView};

/// Run the marker reads for the pass's `repos`. Returns the forge calls made.
pub(super) async fn run(
    repos: &[(PathBuf, String, Vec<PrView>, Vec<ListedPr>)],
    workspace_root: &Path,
) -> usize {
    if crate::rate_limit_breaker::global_is_suppressed() || repos.is_empty() {
        return 0;
    }
    let roots: Vec<(String, PathBuf)> = repos
        .iter()
        .map(|(root, slug, ..)| (slug.to_ascii_lowercase(), root.clone()))
        .collect();
    let root = workspace_root.to_path_buf();
    let done = tokio::task::spawn_blocking(move || {
        let policies: Vec<(String, TrustPolicy)> = roots
            .iter()
            .map(|(slug, r)| (slug.clone(), TrustPolicy::for_root(r)))
            .collect();
        let slugs: Vec<String> = roots.iter().map(|(s, _)| s.clone()).collect();
        let existing = hold_marker_log::load(&root);
        let mut cursor = MarkerCursor::read(&root);
        let calls = std::cell::Cell::new(0_usize);
        let fresh = hold_marker_log::refresh(
            &slugs,
            &existing,
            &mut cursor,
            MARKER_READ_BUDGET,
            Utc::now,
            |repo, url| {
                let repo_root = roots
                    .iter()
                    .find(|(slug, _)| slug.eq_ignore_ascii_case(repo))
                    .map(|(_, r)| r)?;
                calls.set(calls.get() + 1);
                crate::eta::pr_features_forge::fetch_comments_page(repo_root, repo, url)
            },
            |repo, comment| {
                policies
                    .iter()
                    .find(|(slug, _)| slug.eq_ignore_ascii_case(repo))
                    .is_some_and(|(_, p)| p.trusts_json(comment))
            },
        );
        // The rows go first: a lost cursor re-reads and the dedupe appends
        // nothing, but a cursor past rows that were never written loses them.
        if hold_marker_log::append(&root, &fresh).is_ok() {
            if let Err(e) = cursor.write(&root) {
                log::debug!("eta: hold-marker cursor not written: {e}");
            }
        }
        let _ = hold_marker_log::compact(&root, Utc::now());
        calls.get()
    })
    .await;
    done.unwrap_or(0)
}
