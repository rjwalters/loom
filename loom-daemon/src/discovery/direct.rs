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
/// `tier` is what the snapshot is recorded under.
///
/// # Errors
///
/// The item listing's: the set is all-or-nothing. A failed open-PR listing is
/// not an error; it leaves [`RepoSnapshot::pulls`] `None`.
pub fn read_repo(target: &Target, reads: &AtomicU64, tier: Tier) -> Result<RepoSnapshot> {
    // Forge-neutral at the interface, forge-specific underneath (#12312): the
    // snapshot is the same value whichever forge the root is on.
    let forge = crate::forge_cmd::detect_forge(Some(&target.root_dir));
    list_repo(forge, target, reads, tier)
}

/// [`read_repo`] for a fleet chain (#12312): its `gh` tier runs in the
/// isolated `gh` environment ([`crate::gh_invocation::discovery_isolation`]); the gateway tier keeps
/// the read router.
///
/// # Errors
///
/// As [`read_repo`].
pub fn read_repo_fleet(target: &Target, reads: &AtomicU64, tier: Tier) -> Result<RepoSnapshot> {
    let _isolated = (tier == Tier::Gh).then(crate::gh_invocation::discovery_isolation::scope);
    read_repo(target, reads, tier)
}

/// One repo's snapshot on `forge`.
///
/// # Errors
///
/// As [`read_repo`].
pub fn list_repo(
    forge: crate::forge_cmd::ForgeType,
    target: &Target,
    reads: &AtomicU64,
    tier: Tier,
) -> Result<RepoSnapshot> {
    reads.fetch_add(1, Ordering::Relaxed);
    let rows = listing::list_all_open_on(
        forge,
        &target.gh_bin,
        Some(&target.root_dir),
        Some(&target.slug),
    )?;
    // As the gather always did, the open-PR listing is read only once the
    // item listing answered.
    reads.fetch_add(1, Ordering::Relaxed);
    let pulls = crate::forge_pull_listing::list_open_pulls_on(
        forge,
        CALLER,
        &target.gh_bin,
        Some(&target.root_dir),
        Some(&target.slug),
        PULL_PAGES,
    );
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

#[cfg(test)]
#[allow(clippy::unwrap_used)]
#[path = "direct_forge_tests.rs"]
mod forge_tests;
