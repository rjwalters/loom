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
//! failed listing, never as an empty one.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

use super::{BackendKind, Chain, RepoSnapshot, TickSnapshot};

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
