//! The Direct backend: today's per-repo listings, behind the discovery
//! interface unchanged. The item listing is
//! [`crate::step_dispatch::listing::list_all_open`] and the PRs' facts are the
//! ETag-cached open-PR listing; an idle repo costs free `304`s.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::Result;

use super::chain::Tier;
use super::snapshot::RepoStatus;
use super::{RepoSnapshot, Source, Target};
use crate::step_dispatch::gather::PULL_PAGES;
use crate::step_dispatch::listing::{self, CALLER};

/// One repo's snapshot, by listing it. `reads` counts the listing calls.
/// `per_call_mirror`: the item listing may ask the mirror per call (today's
/// behaviour, `forge.mirror.discovery = "per_call"`); the chain's gateway and
/// `gh` tiers never do. `tier` is what the snapshot is recorded under.
///
/// # Errors
///
/// The item listing's: the set is all-or-nothing. A failed open-PR listing is
/// not an error; it leaves [`RepoSnapshot::pulls`] `None`.
pub fn read_repo(
    target: &Target,
    reads: &AtomicU64,
    per_call_mirror: bool,
    tier: Tier,
) -> Result<RepoSnapshot> {
    reads.fetch_add(1, Ordering::Relaxed);
    let list = if per_call_mirror {
        listing::list_all_open
    } else {
        listing::list_all_open_forge
    };
    let items = list(&target.gh_bin, Some(&target.root_dir), Some(&target.slug))?;
    // A per-call mirror answer is the mirror tier's.
    let tier = if items.source == listing::ListingSource::Mirror {
        Tier::Mirror
    } else {
        tier
    };
    // As the gather always did, the open-PR listing is read only once the
    // item listing answered.
    reads.fetch_add(1, Ordering::Relaxed);
    let pulls = crate::forge_pull_listing::list_open_pulls_cached_as(
        CALLER,
        &target.gh_bin,
        Some(&target.root_dir),
        Some(&target.slug),
        PULL_PAGES,
    );
    let rows = items.rows;
    Ok(RepoSnapshot {
        slug: target.slug.clone(),
        source: Source::Listing,
        tier,
        status: RepoStatus::Ok,
        rows,
        pulls: pulls.ok(),
        extras: BTreeMap::new(),
        watermark: None,
    })
}
