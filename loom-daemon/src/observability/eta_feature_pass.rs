//! The per-pass feature reads of the ETA collector (#10232), split out of
//! [`super`] to keep it under the file-size budget.

use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use chrono::Utc;

use super::{lock, ListedPr, PrView};
use crate::eta::pr_features::FEATURE_READ_BUDGET;
use crate::eta::pr_file_log::{self, Candidate, ReadClock, FILE_READ_BUDGET};

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
    let count =
        crate::eta::pr_features::total_cost(&reads) + file_reads(repos, workspace_root).await;
    let root = workspace_root.to_path_buf();
    let slugs = readable.clone();
    let Ok((answers, stall)) = tokio::task::spawn_blocking(move || {
        let answers = crate::eta::pr_features_forge::run(reads, &roots);
        (answers, crate::eta::stall_features::collect(&root, &slugs, Utc::now()))
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

/// When this process last read each PR's file list ([`ReadClock`]).
fn clock() -> &'static Mutex<ReadClock> {
    static CLOCK: OnceLock<Mutex<ReadClock>> = OnceLock::new();
    CLOCK.get_or_init(Mutex::default)
}

/// The file-list reads (#10550): at most [`FILE_READ_BUDGET`] ETag'd calls
/// for the open PRs the listings show, appended to the log beside the fleet
/// snapshots only when a list changed. Forge reads only; zero budget while
/// the breaker suppresses polling. Returns the forge calls made.
async fn file_reads(
    repos: &[(PathBuf, String, Vec<PrView>, Vec<ListedPr>)],
    workspace_root: &Path,
) -> usize {
    if crate::rate_limit_breaker::global_is_suppressed() {
        return 0;
    }
    let roots: Vec<(String, PathBuf)> = repos
        .iter()
        .map(|(root, slug, ..)| (slug.to_ascii_lowercase(), root.clone()))
        .collect();
    let mut candidates: Vec<Candidate> = Vec::new();
    for (_, slug, views, listed) in repos {
        let slug = slug.to_ascii_lowercase();
        let mut seen = std::collections::BTreeSet::new();
        for (pr, updated_at) in views
            .iter()
            .map(|v| (v.number, v.updated_at))
            .chain(listed.iter().map(|l| (l.number, l.updated_at)))
        {
            if seen.insert(pr) {
                candidates.push(Candidate {
                    repo: slug.clone(),
                    pr,
                    updated_at,
                });
            }
        }
    }
    let root = workspace_root.to_path_buf();
    let done = tokio::task::spawn_blocking(move || {
        let log = pr_file_log::load(&root);
        let mut clock = clock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let calls = std::cell::Cell::new(0_usize);
        let fresh =
            pr_file_log::refresh(&candidates, &log, &mut clock, FILE_READ_BUDGET, Utc::now, |c| {
                let repo_root = roots
                    .iter()
                    .find(|(slug, _)| slug.eq_ignore_ascii_case(&c.repo))
                    .map(|(_, r)| r)?;
                calls.set(calls.get() + 1);
                crate::eta::pr_features_forge::fetch_pr_files(repo_root, &c.repo, c.pr)
            });
        // A failed append is a lost observation, not a pass failure: forget
        // those reads so the next pass reads the lists again.
        if pr_file_log::append(&root, &fresh).is_err() {
            for s in &fresh {
                clock.forget(&s.repo, s.pr);
            }
        }
        let _ = pr_file_log::compact(&root, Utc::now());
        calls.get()
    })
    .await;
    done.unwrap_or(0)
}
