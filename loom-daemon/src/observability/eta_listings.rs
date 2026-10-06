//! The review-listing views the ETA pass reads (`pr_views`, `pr_links`), split
//! out of [`super`] to keep it under the file-size budget.

use std::collections::BTreeSet;

use super::parse_time;
use crate::eta::tracker::PrView;
use crate::forge_listing::RestIssue;

/// The PR rows of a repo's review listings, each keyed to the issue it
/// closes. A PR that closes no issue is not tracked.
#[must_use]
pub fn pr_views(listings: &[Vec<RestIssue>]) -> Vec<PrView> {
    let mut seen = BTreeSet::new();
    let mut views = Vec::new();
    for item in listings.iter().flatten() {
        if !item.is_pull_request || !seen.insert(item.number) {
            continue;
        }
        let Some(issue) =
            super::super::ops::stage_dwell::closing_refs(item.body.as_deref().unwrap_or_default())
                .first()
                .copied()
        else {
            continue;
        };
        views.push(PrView {
            number: item.number,
            issue,
            labels: item.labels.clone(),
            created_at: parse_time(item.created_at.as_deref()),
            updated_at: parse_time(item.updated_at.as_deref()),
        });
    }
    views
}

/// Every listed PR with the issues its body links, by the work finder's rule
/// (`linkage_refs`: closing keywords and `Part of`), for the star state
/// (#10372).
#[must_use]
pub fn pr_links(listings: &[Vec<RestIssue>]) -> Vec<(u32, Vec<u32>)> {
    let mut seen = BTreeSet::new();
    listings
        .iter()
        .flatten()
        .filter(|item| item.is_pull_request && seen.insert(item.number))
        .map(|item| {
            (
                item.number,
                crate::eta::star::body_links(item.body.as_deref().unwrap_or_default()),
            )
        })
        .collect()
}
