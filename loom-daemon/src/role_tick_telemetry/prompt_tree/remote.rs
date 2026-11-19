//! The remote half of the prompt tree (#12509): how far a checkout is behind
//! GitHub's tip, not just behind its own `origin/<default>` ref, and whether
//! `origin` is even the workspace's repo.
//!
//! **Local reads only.** The tip is the one the daemon's checkout fast-forward
//! already learned ([`crate::fleet_sync::checkout_ff::heard_within`]); nothing
//! here asks a remote, fetches, or writes to `.git`. The only git children are
//! `remote get-url`, `rev-parse`, `cat-file -e` and `rev-list --count`. Every
//! remote fact is absent when it cannot be read, and `commits_behind_remote`
//! is never 0 on doubt. The raw remote URL never leaves [`classify_origin`].

use std::path::Path;
use std::time::Duration;

use super::{git, PromptTree};
use crate::fleet_sync::checkout_ff::git as ff;

/// How old a heard head may be and still describe GitHub's tip. The resync
/// pass that learns it runs every few minutes.
pub const HEARD_MAX_AGE: Duration = Duration::from_secs(15 * 60);

/// What `origin` should be: the forge host, and the workspace's registered
/// repo when the collector has resolved it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Expected {
    pub host: String,
    pub repo: Option<String>,
}

impl Expected {
    /// The expectation for workspace `root`.
    #[must_use]
    pub fn for_root(root: &Path) -> Self {
        let host = std::env::var("GH_HOST")
            .ok()
            .filter(|v| !v.trim().is_empty())
            .unwrap_or_else(|| "github.com".to_string());
        Self {
            host,
            repo: crate::observability::repo_ref::registered_repo(root),
        }
    }
}

/// What `origin` is, as a pure function of its URL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Origin {
    /// `owner/repo` when the URL parses as exactly two path segments.
    pub repo: Option<String>,
    /// Not the workspace's repo: a local path, `file://`, another host or
    /// another repo.
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

/// A full hex object id: the only shape of tip that reaches a `git` argument.
fn is_object_id(s: &str) -> bool {
    matches!(s.len(), 40 | 64) && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Fill `tree`'s remote facts. `heard` is `(default branch, tip)` as the
/// daemon last heard them, if it did. Nothing is guessed: with no head, only
/// the origin facts are set.
pub fn fill(root: &Path, tree: &mut PromptTree, expected: &Expected, heard: Option<(&str, &str)>) {
    if matches!(crate::forge_cmd::detect_forge(Some(root)), crate::forge_cmd::ForgeType::Gitea) {
        return;
    }
    let url = super::git_remote_url(root).unwrap_or_default();
    let origin = classify_origin(&url, expected);
    tree.remote_repo = origin.repo;
    tree.remote_foreign = Some(origin.foreign);
    let Some((branch, tip)) = heard.filter(|(b, t)| {
        !origin.foreign && is_object_id(t) && crate::refname::check_refname(b).is_ok()
    }) else {
        return;
    };
    tree.remote_tip = Some(tip.to_string());
    tree.origin_ref_stale =
        Some(ff::tracking_head(root, branch, ff::LOCAL_TIMEOUT).as_deref() != Some(tip));
    let spec = format!("{tip}^{{commit}}");
    if git(root, &["cat-file", "-e", &spec]).is_some() {
        tree.commits_behind_remote = git(root, &["rev-list", "--count", &format!("HEAD..{tip}")])
            .and_then(|n| n.parse().ok());
    }
}

/// [`super::read`] plus the remote facts, from what the daemon last heard.
#[must_use]
pub fn read_with_heard(root: &Path) -> Option<PromptTree> {
    let mut tree = super::read(root)?;
    let heard = crate::fleet_sync::checkout_ff::heard_within(root, HEARD_MAX_AGE);
    fill(
        root,
        &mut tree,
        &Expected::for_root(root),
        heard.as_ref().map(|(b, t)| (b.as_str(), t.as_str())),
    );
    Some(tree)
}
