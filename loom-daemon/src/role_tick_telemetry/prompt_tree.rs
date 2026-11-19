//! Which prompt tree a role session ran (#11989).
//!
//! A scheduled role session runs from the workspace's main checkout, and its
//! `/loom:<role>` prompt is that checkout's `.claude/commands/loom/<role>.md`.
//! A checkout the fast-forward cannot advance (diverged, detached) keeps
//! serving old prompts, which then do old things: on 2026-10-15 a Champion
//! running from a checkout 121 commits behind re-added a retired label. The
//! daemon's own version says nothing about that, so each tick records the
//! tree it launched from, read immediately before the spawn.
//!
//! Every field is best-effort and absent when it cannot be read, never a
//! guessed value.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::fleet_sync::checkout_ff::git::{self, LOCAL_TIMEOUT};

/// The checkout a role session's prompts were read from, at launch.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromptTree {
    /// `HEAD` of the workspace checkout (full SHA).
    pub commit: String,
    /// `loom_version` from the working tree's `.loom/install-metadata.json`:
    /// the Loom release whose prompts are installed there.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub loom_version: Option<String>,
    /// Commits `HEAD` is missing from the clone's `origin/<default>` (the ref
    /// as the clone has it; the checkout fast-forward fetches it every pass).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commits_behind: Option<u64>,
    /// `false` when `HEAD` is detached.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on_branch: Option<bool>,
    /// GitHub's tip of the default branch, as the daemon's checkout
    /// fast-forward pass last heard it (#12509). Absent when no head was heard
    /// recently: it is never fetched for this record.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote_tip: Option<String>,
    /// Commits `HEAD` is missing from [`Self::remote_tip`]. Absent when the tip
    /// is unknown or not in the object store, never 0 on doubt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commits_behind_remote: Option<u64>,
    /// The clone's local `origin/<default>` ref disagrees with
    /// [`Self::remote_tip`] (or does not exist): the remote-tracking ref is
    /// stale or divergent, so `commits_behind` would under-report. Absent when
    /// `remote_tip` is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin_ref_stale: Option<bool>,
    /// `owner/repo` of the `origin` remote, never a full URL or userinfo.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote_repo: Option<String>,
    /// `origin` is not the expected forge repo (a local path, another host,
    /// another repo, or an unparseable URL).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote_foreign: Option<bool>,
}

/// One bounded local `git` read; `None` on a non-zero exit, a spawn failure
/// or a timeout (#12111).
fn git(root: &Path, args: &[&str]) -> Option<String> {
    git::ok(root, args, LOCAL_TIMEOUT).ok()
}

/// `root`'s `origin` URL, for classification only: it must not be exported.
pub(crate) fn git_remote_url(root: &Path) -> Option<String> {
    git(root, &["remote", "get-url", "origin"])
}

/// Read the prompt tree of `root`. `None` when `root` is not a git checkout.
/// Local reads only (three `git` calls and one file): no remote is asked.
#[must_use]
pub fn read(root: &Path) -> Option<PromptTree> {
    let commit = git(root, &["rev-parse", "--verify", "--quiet", "HEAD"])?;
    let loom_version = std::fs::read_to_string(root.join(".loom/install-metadata.json"))
        .ok()
        .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok())
        .and_then(|v| v.get("loom_version")?.as_str().map(str::to_string))
        .filter(|v| !v.is_empty());
    let commits_behind = git(root, &["symbolic-ref", "--quiet", "refs/remotes/origin/HEAD"])
        .filter(|r| r.starts_with("refs/remotes/origin/"))
        .and_then(|tracking| git(root, &["rev-list", "--count", &format!("HEAD..{tracking}")]))
        .and_then(|n| n.parse().ok());
    let on_branch = Some(git(root, &["symbolic-ref", "--quiet", "HEAD"]).is_some());
    Some(PromptTree {
        commit,
        loom_version,
        commits_behind,
        on_branch,
        ..PromptTree::default()
    })
}

pub mod remote;

#[cfg(test)]
#[path = "prompt_tree_tests.rs"]
mod remote_tests;

#[cfg(test)]
mod tests {
    use std::process::Command;

    use super::*;

    fn run(dir: &Path, args: &[&str]) {
        let ok = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_INDEX_FILE")
            .output()
            .unwrap();
        assert!(ok.status.success(), "{args:?}: {}", String::from_utf8_lossy(&ok.stderr));
    }

    fn commit(dir: &Path, msg: &str) {
        run(
            dir,
            &[
                "-c",
                "user.name=T",
                "-c",
                "user.email=t@example.invalid",
                "commit",
                "--quiet",
                "--allow-empty",
                "-m",
                msg,
            ],
        );
    }

    #[test]
    fn a_checkout_behind_and_detached_reports_both() {
        let tmp = tempfile::tempdir().unwrap();
        let origin = tmp.path().join("origin.git");
        let host = tmp.path().join("host");
        run(
            tmp.path(),
            &[
                "init",
                "--quiet",
                "--bare",
                "-b",
                "main",
                origin.to_str().unwrap(),
            ],
        );
        run(
            tmp.path(),
            &[
                "clone",
                "--quiet",
                origin.to_str().unwrap(),
                host.to_str().unwrap(),
            ],
        );
        std::fs::create_dir_all(host.join(".loom")).unwrap();
        std::fs::write(
            host.join(".loom/install-metadata.json"),
            r#"{"loom_version": "0.19.1100"}"#,
        )
        .unwrap();
        commit(&host, "one");
        run(&host, &["push", "--quiet", "origin", "HEAD:main"]);
        commit(&host, "two");
        run(&host, &["push", "--quiet", "origin", "HEAD:main"]);
        run(&host, &["fetch", "--quiet", "origin"]);
        run(&host, &["remote", "set-head", "origin", "main"]);
        run(&host, &["checkout", "--quiet", "--detach", "HEAD~1"]);

        let tree = read(&host).unwrap();
        assert_eq!(tree.commit.len(), 40);
        assert_eq!(tree.loom_version.as_deref(), Some("0.19.1100"));
        assert_eq!(tree.commits_behind, Some(1));
        assert_eq!(tree.on_branch, Some(false));
    }

    #[test]
    fn a_clean_up_to_date_checkout_is_zero_behind_on_branch() {
        let tmp = tempfile::tempdir().unwrap();
        let origin = tmp.path().join("origin.git");
        let host = tmp.path().join("host");
        run(
            tmp.path(),
            &[
                "init",
                "--quiet",
                "--bare",
                "-b",
                "main",
                origin.to_str().unwrap(),
            ],
        );
        run(
            tmp.path(),
            &[
                "clone",
                "--quiet",
                origin.to_str().unwrap(),
                host.to_str().unwrap(),
            ],
        );
        commit(&host, "one");
        run(&host, &["push", "--quiet", "origin", "HEAD:main"]);
        run(&host, &["fetch", "--quiet", "origin"]);
        run(&host, &["remote", "set-head", "origin", "main"]);

        let tree = read(&host).unwrap();
        assert_eq!(tree.commits_behind, Some(0));
        assert_eq!(tree.on_branch, Some(true));
    }

    #[test]
    fn not_a_checkout_is_none_and_unknown_fields_are_absent() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(read(&tmp.path().join("missing")).is_none());

        run(tmp.path(), &["init", "--quiet", "-b", "main"]);
        commit(tmp.path(), "one");
        let tree = read(tmp.path()).unwrap();
        assert_eq!(tree.loom_version, None, "no install metadata");
        assert_eq!(tree.commits_behind, None, "no origin/HEAD");
        assert_eq!(tree.on_branch, Some(true));
        let json = serde_json::to_string(&tree).unwrap();
        assert!(!json.contains("loom_version") && !json.contains("commits_behind"), "{json}");
    }
}
