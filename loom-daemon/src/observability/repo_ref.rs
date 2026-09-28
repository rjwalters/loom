//! Shared repo-root -> forge-slug resolution for the periodic collector
//! samples that name repositories (Issue #8852's `queue_snapshot` and #9222's
//! `ops::disposition`), plus a synchronous, read-only cache of the result
//! (Issue #9222). `ops::stage_dwell` resolves its own roots directly through
//! `collector::resolve_repo_slug_cached` / `resolve_visibility` (it also needs
//! the root `PathBuf`, not just its display string) and is unaffected by this
//! module.
//!
//! [`resolve_repo_refs`] is the one place that turns a set of workspace-root
//! strings into their [`QueueRepoRef`]s (forge slug + visibility), so its two
//! callers above share one resolution algorithm instead of two copies. As a
//! side effect it also populates [`cached_repo_ref`]'s
//! synchronous cache, keyed the same way [`crate::work_finder::ready_queue`]
//! names a row's repo (the root's `Path::display()` string, or `workspace
//! #N` for an unnamed one — which never resolves and is never cached).
//!
//! The synchronous cache exists for exactly one reason: the work-finder tick
//! loop is **not async** and must never shell out to `gh` itself (that is the
//! whole point of resolving slugs on the collector's own cadence instead of
//! in the tick — see `queue_snapshot`'s module doc). `ops::dispatch`'s
//! `loom.dispatch.admission` spans still want a `loom.repo` attribute, so
//! they read this cache **without ever triggering a fetch**: a miss (nothing
//! resolved yet this process, or an unresolvable root) just omits the
//! attribute, exactly as the design requires.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::{Mutex, OnceLock};

use crate::telemetry::queue_snapshot::QueueRepoRef;

/// Resolve every root in `roots` to its [`QueueRepoRef`], skipping roots that
/// are not absolute paths (the single-workspace loop's `workspace #N`
/// placeholders) exactly like the work-finder's own listing does. A root that
/// fails to resolve (no `gh` remote, a timeout, an empty answer) is simply
/// absent from the returned map — the caller decides what an unresolved row
/// means for its own record.
///
/// Every resolved pair is also written to the synchronous cache
/// [`cached_repo_ref`] reads, so a later synchronous caller in the same
/// process (the work-finder tick loop, through `ops::dispatch`) can find it
/// without shelling out itself.
pub(super) async fn resolve_repo_refs<'a>(
    roots: impl IntoIterator<Item = &'a str>,
    slug_cache: &mut HashMap<String, String>,
) -> HashMap<String, QueueRepoRef> {
    let roots: HashSet<&str> = roots.into_iter().collect();
    let mut repos = HashMap::new();
    for root in roots {
        if !Path::new(root).is_absolute() {
            continue;
        }
        let Some(slug) = super::collector::resolve_repo_slug_cached(slug_cache, root).await else {
            continue;
        };
        let visibility = super::collector::resolve_visibility(&slug).await;
        let repo_ref = QueueRepoRef {
            repo: slug,
            visibility,
        };
        cache_repo_ref(root, repo_ref.clone());
        repos.insert(root.to_string(), repo_ref);
    }
    repos
}

static REPO_REF_CACHE: OnceLock<Mutex<HashMap<String, QueueRepoRef>>> = OnceLock::new();

fn repo_ref_cache() -> &'static Mutex<HashMap<String, QueueRepoRef>> {
    REPO_REF_CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn cache_repo_ref(root: &str, repo_ref: QueueRepoRef) {
    repo_ref_cache()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(root.to_string(), repo_ref);
}

/// The cached identity for `root` (its `Path::display()` string), or `None`
/// when it has not been resolved by [`resolve_repo_refs`] yet this process.
/// Never triggers a fetch — a synchronous caller (the work-finder tick loop)
/// must be able to read this without blocking on `gh`.
#[must_use]
pub(super) fn cached_repo_ref(root: &str) -> Option<QueueRepoRef> {
    repo_ref_cache()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(root)
        .cloned()
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn cache_starts_empty_and_returns_what_was_stored() {
        // A key unique to this test, so parallel tests sharing the one
        // process-global cache cannot observe each other's writes.
        let key = "/repo-ref-cache-test/unique-root-9222";
        assert_eq!(cached_repo_ref(key), None);
        let repo_ref = QueueRepoRef {
            repo: "owner/repo".to_string(),
            visibility: crate::telemetry::RepoVisibility::Public,
        };
        cache_repo_ref(key, repo_ref.clone());
        assert_eq!(cached_repo_ref(key), Some(repo_ref));
    }

    #[tokio::test]
    async fn a_relative_root_never_resolves_or_caches() {
        let mut slug_cache = HashMap::new();
        let repos = resolve_repo_refs(["workspace #0"], &mut slug_cache).await;
        assert!(repos.is_empty());
        assert_eq!(cached_repo_ref("workspace #0"), None);
    }
}
