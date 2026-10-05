//! The per-pass feature reads of the ETA collector (#10232), split out of
//! [`super`] to keep it under the file-size budget.

use std::path::{Path, PathBuf};

use chrono::Utc;

use super::{lock, ListedPr, PrView};
use crate::eta::pr_features::FEATURE_READ_BUDGET;

/// The feature reads (#10232): at most [`FEATURE_READ_BUDGET`] conditional
/// GETs through the ETag store, a budget separate from the outcome reads',
/// and the host's stall snapshot (no forge call). Both run before the pass's
/// `now`, so its estimates may use them. While the rate-limit breaker
/// suppresses polling the budget is zero. Returns how many forge calls the
/// planned reads make (a check or required-context read is two).
pub(super) async fn run(
    repos: &[(PathBuf, String, Vec<PrView>, Vec<ListedPr>)],
    workspace_root: &Path,
) -> usize {
    let roots: Vec<(String, PathBuf)> = repos
        .iter()
        .map(|(root, slug, ..)| (slug.to_ascii_lowercase(), root.clone()))
        .collect();
    let readable: Vec<String> = roots.iter().map(|(slug, _)| slug.clone()).collect();
    let budget = if crate::rate_limit_breaker::global_is_suppressed() {
        0
    } else {
        FEATURE_READ_BUDGET
    };
    let reads = lock()
        .as_mut()
        .map(|state| {
            state
                .tracker
                .plan_feature_reads(&readable, Utc::now(), budget)
        })
        .unwrap_or_default();
    let count = crate::eta::pr_features::total_cost(&reads);
    let root = workspace_root.to_path_buf();
    let Ok((answers, stall)) = tokio::task::spawn_blocking(move || {
        let answers = crate::eta::pr_features_forge::run(reads, &roots);
        (answers, crate::eta::stall_features::collect(&root, Utc::now()))
    })
    .await
    else {
        return count;
    };
    if let Some(state) = lock().as_mut() {
        state.tracker.on_feature_reads(&answers);
        state.tracker.on_stall_snapshot(stall);
    }
    count
}
