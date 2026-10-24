//! The commit facts an external join needs on a sweep's root span (#11982,
//! stage 1 of #11776): `loom.base_sha` and `loom.head_sha`.
//!
//! Both are full 40-hex OIDs read from the checkout's local refs — no network,
//! no forge call. An unresolvable ref omits the attribute rather than
//! inventing a value.

use std::path::Path;
use std::process::Command;

/// The tip of the default branch a sweep's work is based on.
pub const BASE_SHA: &str = "loom.base_sha";
/// The tip of the issue's `feature/issue-N` branch.
pub const HEAD_SHA: &str = "loom.head_sha";

fn rev_parse(root: &Path, rev: &str) -> Option<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["rev-parse", "--verify", "--quiet"])
        .arg(format!("{rev}^{{commit}}"))
        .output()
        .ok()
        .filter(|out| out.status.success())?;
    let oid = String::from_utf8(out.stdout).ok()?.trim().to_owned();
    (oid.len() == 40 && oid.chars().all(|c| c.is_ascii_hexdigit())).then_some(oid)
}

/// The default branch tip at dispatch: `origin/HEAD`, else `origin/main`.
#[must_use]
pub fn base_sha(root: &Path) -> Option<String> {
    ["refs/remotes/origin/HEAD", "refs/remotes/origin/main"]
        .iter()
        .find_map(|rev| rev_parse(root, rev))
}

/// The issue branch tip: the local `feature/issue-N`, else its remote copy.
/// `None` until the builder has created the branch.
#[must_use]
pub fn head_sha(root: &Path, issue: &str) -> Option<String> {
    if issue.is_empty() || !issue.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    [
        format!("refs/heads/feature/issue-{issue}"),
        format!("refs/remotes/origin/feature/issue-{issue}"),
    ]
    .iter()
    .find_map(|rev| rev_parse(root, rev))
}

#[cfg(test)]
#[path = "git_facts/tests.rs"]
pub(in crate::observability) mod tests;
