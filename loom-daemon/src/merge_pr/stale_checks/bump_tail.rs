//! Version-bump-only base moves are not staleness (#11330).
//!
//! # The race
//!
//! `main` takes a post-merge `chore: bump version to …` commit after nearly
//! every merge (`.github/workflows/version-bump-on-merge.yml`). A PR whose
//! required checks went green just before that bump read stale the moment it
//! landed: the #8248 time rule refuses ANY base move after `started_at`, and a
//! bump touches `Cargo.toml`/`Cargo.lock`, which every Rust check reads. The
//! merge then exited 1 — indistinguishable from a real rejection — although
//! the only thing that changed under the PR was a version string.
//!
//! # The rule
//!
//! Walk the base branch's first-parent chain back from its tip while each
//! commit is a **version bump**; the first commit that is not one is the
//! *effective* base tip ([`BumpTail`]). The freshness guard then judges the
//! evidence against that commit instead of the literal tip: the time rule
//! compares `started_at` with its commit time, and a check whose tested base
//! IS that commit is fresh outright.
//!
//! A commit is a version bump ([`is_version_bump`]) only when ALL hold — the
//! subject line is never consulted, because anyone can write one:
//!
//! - its author is the bump workflow's identity (a fleet GitHub App bot, the
//!   `APP_SLUG[bot]` the workflow commits as), checked by the caller's
//!   predicate against the forge-resolved author account, not the git name;
//! - it has exactly one parent;
//! - every changed path is in [`version_set`] — `scripts/version.sh list`'s
//!   set, read from that script at build time, plus each manifest's lockfile;
//! - `VERSION` strictly increases and every changed line of every other file
//!   carries the old/new version ([`super::evidence::strip_validated_restamps`]
//!   discounts the whole commit);
//! - its `Cargo.lock` lines are workspace crates' own `version =` lines
//!   ([`cargo_lock_is_workspace_only`]): a registry or git package (one with a
//!   `source =` line) moving is a dependency change, never a bump.
//!
//! Any read failure, a walk longer than [`MAX_WALK`], or a tip that is not a
//! bump yields `None` — the guard's verdict is exactly what it was before.

use super::evidence::{strip_validated_restamps, ChangedFile};
use chrono::{DateTime, Utc};
use std::sync::LazyLock;

/// `scripts/version.sh`, the single source of the version-bearing file list.
const VERSION_SH: &str = include_str!("../../../../scripts/version.sh");

/// How many trailing bump commits the walk follows before giving up. One
/// bump per merge is the steady state; a short pile-up of queued bump runs is
/// the most a PR can sit behind without some real merge landing too.
pub const MAX_WALK: usize = 8;

/// The version-bearing paths: `scripts/version.sh list` (its `VERSION_FILES`
/// array plus its `INSTALL_METADATA_FILE`), parsed from the script itself so
/// there is no second hard-coded copy, plus the lockfile each manifest drags
/// along (`Cargo.toml` → `Cargo.lock`, `<dir>/package.json` →
/// `<dir>/package-lock.json`).
#[must_use]
pub fn version_set() -> &'static [String] {
    static SET: LazyLock<Vec<String>> = LazyLock::new(|| derive_version_set(VERSION_SH));
    &SET
}

/// [`version_set`] over an arbitrary `version.sh` source (the testable seam).
#[must_use]
pub fn derive_version_set(src: &str) -> Vec<String> {
    fn push(set: &mut Vec<String>, p: String) {
        if !p.is_empty() && !set.contains(&p) {
            set.push(p);
        }
    }
    let mut set: Vec<String> = Vec::new();
    let mut in_array = false;
    for line in src.lines() {
        let t = line.trim();
        if t.starts_with("VERSION_FILES=(") {
            in_array = true;
            continue;
        }
        if in_array {
            if t.starts_with(')') {
                in_array = false;
                continue;
            }
            push(&mut set, t.trim_matches('"').to_string());
        } else if let Some(v) = t.strip_prefix("INSTALL_METADATA_FILE=") {
            push(
                &mut set,
                v.trim_matches('"')
                    .trim_start_matches("$REPO_ROOT/")
                    .to_string(),
            );
        }
    }
    let lockfiles: Vec<String> = set
        .iter()
        .filter_map(|p| {
            if p == "Cargo.toml" {
                Some("Cargo.lock".to_string())
            } else {
                p.strip_suffix("package.json")
                    .map(|dir| format!("{dir}package-lock.json"))
            }
        })
        .collect();
    for l in lockfiles {
        push(&mut set, l);
    }
    set
}

/// One base-branch commit, as `GET /repos/{nwo}/commits/{sha}` reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BaseCommit {
    pub sha: String,
    /// The forge-resolved author ACCOUNT login (`author.login`), not the git
    /// author name; `None` when the forge could not attribute it.
    pub author_login: Option<String>,
    /// `author.type == "Bot"`.
    pub author_is_bot: bool,
    pub parents: Vec<String>,
    /// The committer date: when the commit appeared on the branch.
    pub committed_at: Option<DateTime<Utc>>,
    pub files: Vec<ChangedFile>,
}

/// The base tip with trailing version bumps discounted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BumpTail {
    /// The newest first-parent ancestor of the tip that is NOT a bump.
    pub effective_sha: String,
    /// Its committer date.
    pub effective_at: DateTime<Utc>,
    /// How many bump commits were discounted (always ≥ 1).
    pub bumps: usize,
}

impl BumpTail {
    /// Whether `tested_base` (possibly abbreviated) names the effective tip.
    #[must_use]
    pub fn is_effective(&self, tested_base: &str) -> bool {
        let (a, b) = (self.effective_sha.to_ascii_lowercase(), tested_base.to_ascii_lowercase());
        let n = a.len().min(b.len());
        n >= 7 && a[..n] == b[..n]
    }
}

/// Is `c` a post-merge version bump? See the module docs for every clause.
pub fn is_version_bump(c: &BaseCommit, is_bump_identity: &dyn Fn(&str) -> bool) -> bool {
    let identity = c.author_is_bot && c.author_login.as_deref().is_some_and(is_bump_identity);
    let set = version_set();
    identity
        && c.parents.len() == 1
        && !c.files.is_empty()
        && c.files.iter().all(|f| set.contains(&f.path))
        && c.files.iter().any(|f| f.path == "VERSION")
        && strip_validated_restamps(&c.files).is_empty()
        && c.files
            .iter()
            .filter(|f| f.path == "Cargo.lock")
            .all(|f| f.patch.as_deref().is_some_and(cargo_lock_is_workspace_only))
}

/// Every changed line of a `Cargo.lock` patch is the `version =` line of a
/// package with no `source =` — a workspace (path) crate. The line before a
/// changed run must be that package's `name =`, and the line after it must
/// not be a `source =`/`checksum =` line (and must exist: an unreadable
/// neighbour is an unknown, which is never a bump).
#[must_use]
pub fn cargo_lock_is_workspace_only(patch: &str) -> bool {
    let lines: Vec<&str> = patch.lines().collect();
    let changed = |l: &str| {
        (l.starts_with('-') || l.starts_with('+')) && !l.starts_with("---") && !l.starts_with("+++")
    };
    let mut i = 0;
    while i < lines.len() {
        if !changed(lines[i]) {
            i += 1;
            continue;
        }
        let start = i;
        while i < lines.len() && changed(lines[i]) {
            if !lines[i][1..].trim_start().starts_with("version = \"") {
                return false;
            }
            i += 1;
        }
        let before = start.checked_sub(1).map(|j| lines[j].trim());
        if !before.is_some_and(|b| b.starts_with("name = \"")) {
            return false;
        }
        match lines.get(i).map(|l| l.trim()) {
            Some(after) if !after.starts_with("source =") && !after.starts_with("checksum =") => {}
            _ => return false,
        }
    }
    true
}

/// Walk back from `tip_sha` over trailing version bumps. `fetch` reads one
/// commit; any failure, a non-bump tip, or more than [`MAX_WALK`] bumps
/// answers `None` (no discount — the fail-closed direction).
pub fn walk(
    tip_sha: &str,
    fetch: &mut dyn FnMut(&str) -> Result<BaseCommit, String>,
    is_bump_identity: &dyn Fn(&str) -> bool,
) -> Option<BumpTail> {
    let mut sha = tip_sha.to_string();
    let mut bumps = 0;
    loop {
        let c = fetch(&sha).ok()?;
        if !is_version_bump(&c, is_bump_identity) {
            if bumps == 0 {
                return None;
            }
            return Some(BumpTail {
                effective_sha: c.sha,
                effective_at: c.committed_at?,
                bumps,
            });
        }
        bumps += 1;
        if bumps > MAX_WALK {
            return None;
        }
        sha = c.parents.first()?.clone();
    }
}

/// Parse a `GET /repos/{nwo}/commits/{sha}` body. A listing at/over the
/// 300-file cap may be truncated, so it is refused.
pub fn commit_from_json(body: &str) -> Result<BaseCommit, String> {
    let v: serde_json::Value =
        serde_json::from_str(body).map_err(|e| format!("commit response was not JSON: {e}"))?;
    let s = |p: &str| v.pointer(p).and_then(|x| x.as_str()).map(str::to_string);
    let files = v
        .get("files")
        .and_then(|f| f.as_array())
        .ok_or("commit response had no files array")?;
    if files.len() >= super::evidence::COMPARE_FILE_CAP {
        return Err("commit file list may be truncated".to_string());
    }
    Ok(BaseCommit {
        sha: s("/sha").ok_or("commit response had no sha")?,
        author_login: s("/author/login"),
        author_is_bot: s("/author/type").as_deref() == Some("Bot"),
        parents: v
            .get("parents")
            .and_then(|p| p.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|p| p.get("sha")?.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default(),
        committed_at: s("/commit/committer/date")
            .and_then(|d| DateTime::parse_from_rfc3339(&d).ok())
            .map(|d| d.with_timezone(&Utc)),
        files: files
            .iter()
            .map(|f| ChangedFile {
                path: f
                    .get("filename")
                    .and_then(|x| x.as_str())
                    .unwrap_or_default()
                    .to_string(),
                status: f
                    .get("status")
                    .and_then(|x| x.as_str())
                    .unwrap_or_default()
                    .to_string(),
                previous_filename: f
                    .get("previous_filename")
                    .and_then(|x| x.as_str())
                    .map(String::from),
                patch: f.get("patch").and_then(|x| x.as_str()).map(String::from),
            })
            .collect(),
    })
}

#[cfg(test)]
mod tests;
