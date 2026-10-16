//! The remote half of the prompt tree (#12509): how far a checkout is behind
//! *GitHub's* tip, not just behind its own `origin/<default>` ref, and whether
//! `origin` is even the expected repo.
//!
//! Measures, never fixes: the only write is an object-only fetch that moves no
//! ref, writes no `FETCH_HEAD` and touches no working tree. Every network
//! fact is absent when it cannot be read; `commits_behind` is never 0 on doubt.
//! The raw remote URL never leaves [`classify_origin`].

use std::path::Path;

use super::{git, PromptTree, LOCAL_TIMEOUT};
use crate::fleet_sync::checkout_ff::git as ff;

/// The two network reads the comparison needs, injectable for tests.
pub trait Remote {
    /// `(default branch, tip)` as the remote advertises them.
    ///
    /// # Errors
    /// When the remote could not be asked (offline, timeout, no HEAD).
    fn probe(&self, root: &Path) -> Result<(String, String), String>;
    /// Download `branch`'s objects, moving no ref.
    ///
    /// # Errors
    /// When the objects could not be fetched.
    fn fetch_objects(&self, root: &Path, branch: &str) -> Result<(), String>;
}

/// The real remote: `git ls-remote --symref` and an object-only fetch.
pub struct GitRemote;

impl Remote for GitRemote {
    fn probe(&self, root: &Path) -> Result<(String, String), String> {
        ff::remote_default_head(root, ff::PROBE_TIMEOUT)
    }

    fn fetch_objects(&self, root: &Path, branch: &str) -> Result<(), String> {
        ff::fetch_objects(root, branch, ff::FETCH_TIMEOUT)
    }
}

/// What `origin` should be. The repo is `$LOOM_REPO` when set (the convention
/// `presence` and the lease renewer use); there is no other local source of
/// truth for an attended checkout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Expected {
    pub host: String,
    pub repo: Option<String>,
}

impl Expected {
    #[must_use]
    pub fn from_env() -> Self {
        let non_empty = |k: &str| std::env::var(k).ok().filter(|v| !v.trim().is_empty());
        Self {
            host: non_empty("GH_HOST").unwrap_or_else(|| "github.com".to_string()),
            repo: non_empty("LOOM_REPO"),
        }
    }
}

/// What `origin` is, as a pure function of its URL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Origin {
    /// `owner/repo` when the URL parses as exactly two path segments.
    pub repo: Option<String>,
    /// Not the expected forge repo.
    pub foreign: bool,
}

/// Classify an `origin` URL (post-`insteadOf`). Only `owner/repo` survives:
/// userinfo, scheme and host are dropped here.
#[must_use]
pub fn classify_origin(url: &str, expected: &Expected) -> Origin {
    let parsed = crate::forge_etag_store::parse_remote_url(url)
        .filter(|(_, repo)| repo.split('/').count() == 2);
    let Some((host, repo)) = parsed else {
        return Origin {
            repo: None,
            foreign: true,
        };
    };
    let wrong_repo = expected
        .repo
        .as_deref()
        .is_some_and(|want| !want.eq_ignore_ascii_case(&repo));
    let foreign = !host.eq_ignore_ascii_case(&expected.host) || wrong_repo;
    Origin {
        repo: Some(repo),
        foreign,
    }
}

/// Fill `tree`'s remote facts from `remote`. `commits_behind` is reset first:
/// on this record it only ever means "behind the remote's tip".
/// `fetch_allowed` is false under low disk.
pub fn compare(root: &Path, tree: &mut PromptTree, remote: &dyn Remote, fetch_allowed: bool) {
    tree.commits_behind = None;
    let Ok((branch, tip)) = remote.probe(root) else {
        return;
    };
    tree.remote_tip = Some(tip.clone());
    tree.origin_ref_stale =
        Some(ff::tracking_head(root, &branch, LOCAL_TIMEOUT).as_deref() != Some(tip.as_str()));
    let spec = format!("{tip}^{{commit}}");
    let have = |root: &Path| git(root, &["cat-file", "-e", &spec]).is_some();
    if !have(root)
        && (!fetch_allowed || remote.fetch_objects(root, &branch).is_err() || !have(root))
    {
        return;
    }
    tree.commits_behind =
        git(root, &["rev-list", "--count", &format!("HEAD..{tip}")]).and_then(|n| n.parse().ok());
}

/// [`super::read`] plus the remote comparison, for an attended session.
#[must_use]
pub fn read_with_remote(
    root: &Path,
    expected: &Expected,
    remote: &dyn Remote,
) -> Option<PromptTree> {
    let mut tree = super::read(root)?;
    tree.commits_behind = None;
    if matches!(crate::forge_cmd::detect_forge(Some(root)), crate::forge_cmd::ForgeType::Gitea) {
        return Some(tree);
    }
    let url = git(root, &["remote", "get-url", "origin"]).unwrap_or_default();
    let origin = classify_origin(&url, expected);
    tree.remote_repo = origin.repo;
    tree.remote_foreign = Some(origin.foreign);
    if !origin.foreign {
        let fetch_allowed = crate::fetch_headroom::skip_reason(root).is_none();
        compare(root, &mut tree, remote, fetch_allowed);
    }
    Some(tree)
}
