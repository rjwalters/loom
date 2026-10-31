//! The tick's snapshot, for readers that run outside the tick (#12307).
//!
//! The step driver publishes each repo its gather read; the blocked-queue
//! gauge, quarantine reconciliation and claim reconciliation read their
//! listing from it when the chain is the mirror's worklist
//! ([`BackendKind::Mirror`]), so they make no listing of their own. On any
//! other chain they list as before ([`Lookup::NotMirror`]).
//!
//! A repo's entry is replaced when a later tick reads it again and is never
//! dropped, so a repo a deadline-truncated tick skipped is served from its
//! last read, however old: the readers act on listings only after live
//! checks of their own (`scan_issue`, `mergeable_of`, PR state). A repo the
//! snapshot has never read is [`Lookup::Absent`]: the reader treats it as a
//! failed listing, never as an empty one, unless it can ask for one
//! ([`on_demand`], #12314).

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

use std::time::{Duration, Instant};

use super::{BackendKind, Chain, Discovery, RepoSnapshot, Target, TickSnapshot};

static LATEST: Mutex<BTreeMap<String, Arc<RepoSnapshot>>> = Mutex::new(BTreeMap::new());

/// What a reader finds for one workspace.
#[derive(Debug, Clone)]
pub enum Lookup {
    /// The chain is not the mirror's: list the repo as before.
    NotMirror,
    /// Mirror chain, and no tick has read this repo (or it has no
    /// `github.com` slug): the reader must not list, and must not read this
    /// as "no items".
    Absent,
    /// The repo's part of the latest snapshot.
    Repo(Arc<RepoSnapshot>),
}

fn latest() -> std::sync::MutexGuard<'static, BTreeMap<String, Arc<RepoSnapshot>>> {
    LATEST
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Record every repo `snapshot` read. A no-op unless it is a mirror tick.
pub fn publish(snapshot: &TickSnapshot) {
    if snapshot.kind() != BackendKind::Mirror {
        return;
    }
    let mut latest = latest();
    for repo in snapshot.repos() {
        latest.insert(repo.slug.to_ascii_lowercase(), repo);
    }
}

/// The latest snapshot's part for the workspace at `root`.
#[must_use]
pub fn lookup(root: &Path) -> Lookup {
    if Chain::for_root(root).backend() != BackendKind::Mirror {
        return Lookup::NotMirror;
    }
    lookup_slug(crate::forge_mirror::resolve_repo(Some(root), None).as_deref())
}

fn lookup_slug(slug: Option<&str>) -> Lookup {
    slug.and_then(|s| latest().get(&s.to_ascii_lowercase()).cloned())
        .map_or(Lookup::Absent, Lookup::Repo)
}

/// How long an on-demand tick is reused by the next root of the same startup
/// pass, so N roots cost one worklist request, not N.
const ON_DEMAND_TTL: Duration = Duration::from_secs(30);

/// The discovery state of snapshots taken outside the step tick (#12314).
struct OnDemand {
    discovery: Discovery,
    tick: Option<(Instant, Arc<TickSnapshot>)>,
}

static ON_DEMAND: Mutex<Option<OnDemand>> = Mutex::new(None);

fn on_demand_state() -> std::sync::MutexGuard<'static, Option<OnDemand>> {
    ON_DEMAND
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// The workspace's part of a snapshot taken now, for a reader that runs
/// before the first step tick (quarantine reconciliation's startup pass,
/// #12314). This is the one place a snapshot is taken outside the tick.
///
/// The repo is published like a tick's, and the mirror's worklist and ETag
/// stay behind for the step driver ([`take_mirror_seed`]), so its first tick
/// asks conditionally and gets a `304` instead of a second full worklist.
///
/// # Errors
///
/// The workspace has no `github.com` slug, or no tier answered for it.
pub fn on_demand(gh_bin: &Path, root: &Path) -> anyhow::Result<Arc<RepoSnapshot>> {
    let slug =
        crate::forge_mirror::resolve_repo(Some(root), None).ok_or_else(|| absent_error(root))?;
    let target = Target {
        slug,
        gh_bin: gh_bin.to_path_buf(),
        root_dir: root.to_path_buf(),
    };
    let mut state = on_demand_state();
    let state = state.get_or_insert_with(|| OnDemand {
        discovery: Discovery::unseeded(crate::work_finder::resolve_interval()),
        tick: None,
    });
    let tick = match &state.tick {
        Some((at, tick)) if at.elapsed() < ON_DEMAND_TTL => tick.clone(),
        _ => {
            let tick = Arc::new(
                state
                    .discovery
                    .begin_tick(Chain::for_root(root), chrono::Utc::now()),
            );
            state.tick = Some((Instant::now(), tick.clone()));
            tick
        }
    };
    let repo = tick.repo(&target);
    publish(&tick);
    repo
}

/// The mirror cache (last worklist and ETag) an on-demand snapshot left, for
/// the step driver's [`Discovery`] to start from. Taken once.
pub(super) fn take_mirror_seed() -> Option<super::mirror::MirrorBackend> {
    on_demand_state().take().map(|s| s.discovery.mirror)
}

/// The error a reader reports for [`Lookup::Absent`], so its caller treats
/// the listing as failed.
#[must_use]
pub fn absent_error(root: &Path) -> anyhow::Error {
    anyhow::anyhow!(
        "discovery: {} is not in the tick's snapshot (mirror mode lists nothing itself)",
        root.display()
    )
}

#[cfg(test)]
pub(crate) mod test_support {
    use super::*;

    pub(crate) fn repo(slug: &str) -> RepoSnapshot {
        RepoSnapshot {
            slug: slug.to_string(),
            source: super::super::Source::Worklist,
            tier: super::super::Tier::Mirror,
            status: super::super::RepoStatus::Ok,
            rows: Vec::new(),
            pulls: Some(Vec::new()),
            extras: BTreeMap::new(),
            watermark: None,
        }
    }

    /// Forget the on-demand snapshot state.
    pub(crate) fn reset_on_demand() {
        *on_demand_state() = None;
    }

    /// Replace the published repos.
    pub(crate) fn set(repos: Vec<RepoSnapshot>) {
        let mut latest = latest();
        latest.clear();
        for repo in repos {
            latest.insert(repo.slug.to_ascii_lowercase(), Arc::new(repo));
        }
    }

    /// A workspace whose chain is the mirror's worklist and whose `origin`
    /// is `github.com/<slug>`, plus a `gh` that records every call in
    /// `calls.log` and fails: a reader that lists shows up there.
    pub(crate) struct MirrorRoot {
        pub(crate) dir: tempfile::TempDir,
        pub(crate) gh: std::path::PathBuf,
    }

    impl MirrorRoot {
        pub(crate) fn new(slug: &str) -> Self {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path();
            std::fs::create_dir_all(root.join(".loom-local")).unwrap();
            std::fs::write(
                root.join(crate::config_resolver::LOCAL_CONFIG_REL),
                r#"{"forge":{"mirror":{"origin":"https://events.example.com","keyRef":"file:/k","discovery":"worklist"}}}"#,
            )
            .unwrap();
            for args in [
                vec!["init", "-q"],
                vec![
                    "remote",
                    "add",
                    "origin",
                    &format!("https://github.com/{slug}"),
                ],
            ] {
                let ok = std::process::Command::new("git")
                    .args(&args)
                    .current_dir(root)
                    .status()
                    .unwrap()
                    .success();
                assert!(ok, "git {args:?}");
            }
            let gh = root.join("fake-gh");
            std::fs::write(
                &gh,
                format!(
                    "#!/bin/sh\necho \"$*\" >> '{}'\nexit 1\n",
                    root.join("calls.log").display()
                ),
            )
            .unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o755)).unwrap();
            }
            Self { dir, gh }
        }

        pub(crate) fn path(&self) -> &Path {
            self.dir.path()
        }

        /// The `gh` calls made so far.
        pub(crate) fn gh_calls(&self) -> String {
            std::fs::read_to_string(self.dir.path().join("calls.log")).unwrap_or_default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::{repo, set};
    use super::*;

    #[test]
    #[serial_test::serial]
    fn a_repo_is_found_by_its_lowercase_slug_and_an_unread_one_is_absent() {
        set(vec![repo("Acme/Widgets")]);
        assert!(matches!(lookup_slug(Some("acme/WIDGETS")), Lookup::Repo(_)));
        assert!(matches!(lookup_slug(Some("acme/other")), Lookup::Absent));
        assert!(matches!(lookup_slug(None), Lookup::Absent));
        set(Vec::new());
    }

    #[test]
    #[serial_test::serial]
    fn a_non_mirror_chain_is_not_served_from_the_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        set(vec![repo("acme/widgets")]);
        assert!(matches!(lookup(dir.path()), Lookup::NotMirror));
        set(Vec::new());
    }
}
