//! Host facts release resolution needs: the target triple, the repo slug, and
//! a file's sha256 (epic #7810, PR 5).

use crate::script_helpers::run_git;
use sha2::{Digest, Sha256};
use std::path::Path;

/// The release target triple for this host, or `None` when the platform has no
/// published artifact.
///
/// `None` is a first-class answer, not an error: it becomes `ok:false` with
/// "unrecognized host platform", and the daemon falls back to its source path.
/// Guessing a triple would make it fetch a binary that cannot run.
#[must_use]
pub fn target_triple() -> Option<&'static str> {
    target_triple_for(std::env::consts::OS, std::env::consts::ARCH)
}

/// [`target_triple`] for an explicit os/arch, so the mapping is testable on one
/// host.
///
/// The shell read `uname -s`/`uname -m`; Rust's `consts` are the same facts
/// resolved at compile time, and the aliases below are kept because `uname`
/// reports both spellings on real hosts.
#[must_use]
pub fn target_triple_for(os: &str, arch: &str) -> Option<&'static str> {
    match (os, arch) {
        ("macos", "aarch64" | "arm64") => Some("aarch64-apple-darwin"),
        ("linux", "aarch64" | "arm64") => Some("aarch64-unknown-linux-gnu"),
        ("linux", "x86_64" | "amd64") => Some("x86_64-unknown-linux-gnu"),
        // Notably absent: x86_64-apple-darwin. The release workflow does not
        // publish it, so reporting a triple here would resolve an artifact that
        // does not exist.
        _ => None,
    }
}

/// `owner/repo` from the checkout's `origin` remote, or `None`.
///
/// Handles the `git@github.com:`, `https://github.com/`, `http://github.com/`
/// and `ssh://git@github.com/` forms the shell enumerated. A non-GitHub remote
/// yields `None`: releases are resolved through `gh`, so a slug that `gh`
/// cannot address is worse than no slug.
#[must_use]
pub fn repo_slug(repo_root: &Path) -> Option<String> {
    let url = run_git(repo_root, &["remote", "get-url", "origin"]).ok_stdout_trimmed()?;
    slug_from_remote_url(&url)
}

/// The parsing half of [`repo_slug`], separated so every URL form is testable
/// without a checkout.
#[must_use]
pub fn slug_from_remote_url(url: &str) -> Option<String> {
    let rest = url
        .strip_prefix("git@github.com:")
        .or_else(|| url.strip_prefix("https://github.com/"))
        .or_else(|| url.strip_prefix("http://github.com/"))
        .or_else(|| url.strip_prefix("ssh://git@github.com/"))?;
    let rest = rest.strip_suffix(".git").unwrap_or(rest);
    let rest = rest.trim_end_matches('/');
    // Must name both halves; "owner" alone is not addressable.
    if rest.split('/').filter(|s| !s.is_empty()).count() != 2 {
        return None;
    }
    Some(rest.to_string())
}

/// A file's sha256 as lowercase hex, or `None` when it could not be read.
#[must_use]
pub fn sha256_file(path: &Path) -> Option<String> {
    let bytes = std::fs::read(path).ok()?;
    Some(
        Sha256::digest(&bytes)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect(),
    )
}

#[cfg(test)]
mod tests;
