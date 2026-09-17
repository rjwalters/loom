//! The resolution itself (epic #7810, PR 5).

use super::{host, semver};
use crate::cmd_out::Query;
use crate::script_helpers::gh_query;
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Deadline for each `gh` call. Resolution runs on every auto-update tick, so
/// an unbounded call would wedge the loop rather than fail it.
const GH_TIMEOUT: Duration = Duration::from_secs(30);

/// Everything one successful resolution learned.
///
/// Every field but `tag`/`version` is optional, and `None` means *could not be
/// determined* — never a fabricated value. An older `gh` reports no
/// `publishedAt`; a `.sha256` asset may not download; a host may have no
/// resolvable installed binary at all.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Resolved {
    pub repo: String,
    pub target: String,
    pub tag: String,
    pub version: String,
    pub published_at: Option<String>,
    pub asset_sha256: Option<String>,
    pub installed_bin: Option<PathBuf>,
    pub installed_version: Option<String>,
    /// The literal `"unknown"` when the binary answered but named no commit —
    /// a distinct value from `None` ("no binary at all"), and `auto_update`
    /// special-cases it.
    pub installed_commit: Option<String>,
    pub installed_sha256: Option<String>,
    pub source_version: Option<String>,
    pub source_commit: Option<String>,
}

/// What one resolution attempt concluded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resolution {
    Resolved(Box<Resolved>),
    /// No artifact for this host. **Not an error** — no Releases yet, an
    /// unreachable or rate-limited API, an unbuilt platform, fetch disabled.
    /// The tick falls back to the source path, exactly as before #7609.
    Unresolved(String),
}

/// Inputs a caller can override, so resolution is drivable without env vars.
#[derive(Debug, Clone)]
pub struct Inputs<'a> {
    pub repo_root: &'a Path,
    /// `LOOM_DAEMON_UPDATE_TARGET`.
    pub target_override: Option<String>,
    /// `LOOM_DAEMON_UPDATE_GH_REPO`.
    pub repo_override: Option<String>,
    /// The installed binary to compare against, when one was resolved.
    ///
    /// Caller-supplied on purpose: "installed" means something different to
    /// each one. The shell compares against the binary its detected SUPERVISOR
    /// launches (falling back to PATH resolution) — that is what an operator
    /// asking "is my host stale" means. The daemon compares against the binary
    /// it is itself running, which is what its own roll decision is about. A
    /// single hard-coded notion here would be wrong for one of them.
    pub installed_bin: Option<PathBuf>,
    /// `--no-fetch` / `LOOM_DAEMON_UPDATE_FETCH=0`.
    pub fetch_disabled: bool,
}

#[derive(Deserialize)]
struct TagName {
    #[serde(rename = "tagName")]
    tag_name: String,
}

#[derive(Deserialize)]
struct AssetName {
    name: String,
}

#[derive(Deserialize)]
struct Assets {
    #[serde(default)]
    assets: Vec<AssetName>,
}

#[derive(Deserialize)]
struct PublishedAt {
    #[serde(default, rename = "publishedAt")]
    published_at: String,
}

/// Resolve the latest release artifact for this host.
///
/// Read-only: the only download is the release's ~65-byte `.sha256` asset.
#[must_use]
pub fn resolve(inputs: &Inputs<'_>) -> Resolution {
    // Honoured first: an operator who disabled the artifact path fleet-wide
    // gets that as the reason, which is what keeps the daemon's tick falling
    // back to source on such a host rather than reporting a forge problem.
    if inputs.fetch_disabled {
        return Resolution::Unresolved(
            "artifact-fetch is disabled on this host (--no-fetch / LOOM_DAEMON_UPDATE_FETCH=0)"
                .to_string(),
        );
    }

    let Some(target) = inputs
        .target_override
        .clone()
        .filter(|t| !t.is_empty())
        .or_else(|| host::target_triple().map(str::to_string))
    else {
        return Resolution::Unresolved(format!(
            "unrecognized host platform ({}/{}) -- no release target-triple mapping",
            std::env::consts::OS,
            std::env::consts::ARCH
        ));
    };

    let Some(repo) = inputs
        .repo_override
        .clone()
        .filter(|r| !r.is_empty())
        .or_else(|| host::repo_slug(inputs.repo_root))
    else {
        return Resolution::Unresolved(
            "could not resolve owner/repo from git remote 'origin' \
             (set LOOM_DAEMON_UPDATE_GH_REPO to override)"
                .to_string(),
        );
    };

    let root = inputs.repo_root;
    let q: Query<TagName> = gh_query(
        &["release", "view", "--json", "tagName", "-R", &repo],
        root,
        false,
        |t: &TagName| t.tag_name.is_empty(),
    );
    let Query::Populated(tag) = q else {
        return Resolution::Unresolved(format!(
            "'gh release view' found no latest release for {repo} \
             (no Releases yet, an unreachable/rate-limited API, or an auth failure)"
        ));
    };
    let tag = tag.tag_name;

    let Some(version) = semver::extract_version(&tag) else {
        return Resolution::Unresolved(format!(
            "could not parse a semver version out of release tag '{tag}'"
        ));
    };

    // BOTH assets must exist. The binary alone is not enough: without the
    // `.sha256` sibling there is nothing to verify a fetch against, so an
    // artifact that resolved on the binary alone would promise a verified
    // upgrade this host cannot actually verify.
    let bin_name = format!("loom-daemon-{target}");
    let sha_name = format!("{bin_name}.sha256");
    let q: Query<Assets> = gh_query(
        &["release", "view", "--json", "assets", "-R", &repo],
        root,
        false,
        |_: &Assets| false,
    );
    let names: Vec<String> = match q {
        Query::Populated(a) => a.assets.into_iter().map(|a| a.name).collect(),
        _ => Vec::new(),
    };
    if !names.contains(&bin_name) || !names.contains(&sha_name) {
        return Resolution::Unresolved(format!(
            "release {tag} has no artifact for target {target} \
             (checked for {bin_name} + {sha_name})"
        ));
    }

    let published_at = fetch_published_at(&tag, &repo, root);
    let asset_sha256 = fetch_asset_sha256(&tag, &repo, &sha_name, root);
    let (installed_version, installed_commit) = installed_identity(inputs.installed_bin.as_deref());

    Resolution::Resolved(Box::new(Resolved {
        repo,
        target,
        tag,
        version,
        published_at,
        asset_sha256,
        installed_sha256: inputs.installed_bin.as_deref().and_then(host::sha256_file),
        installed_bin: inputs.installed_bin.clone(),
        installed_version,
        installed_commit,
        source_version: read_source_version(root),
        source_commit: read_source_commit(root),
    }))
}

/// The release's publish timestamp, verbatim. `None` on an older `gh` that does
/// not report the field.
fn fetch_published_at(tag: &str, repo: &str, root: &Path) -> Option<String> {
    let q: Query<PublishedAt> = gh_query(
        &["release", "view", tag, "--json", "publishedAt", "-R", repo],
        root,
        false,
        |p: &PublishedAt| p.published_at.is_empty(),
    );
    match q {
        Query::Populated(p) => Some(p.published_at),
        _ => None,
    }
}

/// The published sha256 for this platform's binary, read from the release's own
/// `.sha256` asset.
///
/// This is the one download, and it is the checksum asset only — never the
/// binary. The daemon compares it against the installed binary's own digest to
/// detect "same version, different bytes": a host that built this version from
/// source before the release existed.
fn fetch_asset_sha256(tag: &str, repo: &str, sha_name: &str, root: &Path) -> Option<String> {
    // A scratch directory of our own, removed before returning on every path —
    // the shell's `mktemp -d` plus its `_cleanup_fetch_tmpdirs` trap. Resolution
    // runs on every tick, so a leaked directory per tick is a real leak.
    let dir = std::env::temp_dir().join(format!(
        "loom-daemon-resolve-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&dir).ok()?;
    let digest = (|| {
        let dir_str = dir.to_str()?;
        let out = crate::script_helpers::run_gh(
            &[
                "release",
                "download",
                tag,
                "-R",
                repo,
                "-p",
                sha_name,
                "-D",
                dir_str,
                "--clobber",
            ],
            root,
            false,
        );
        if !out.succeeded() {
            return None;
        }
        let text = std::fs::read_to_string(dir.join(sha_name)).ok()?;
        // `awk 'NR==1{print $1}'` — first field of the first line.
        text.lines()
            .next()?
            .split_whitespace()
            .next()
            .map(str::to_string)
    })();
    let _ = std::fs::remove_dir_all(&dir);
    digest
}

/// The installed binary's version and commit, from its own `--version`.
///
/// Returns `(None, None)` when there is no binary or it would not answer.
/// `installed_commit` is `Some("unknown")` when the binary answered but named
/// no commit — the shell's literal, and a distinct state from "no binary".
fn installed_identity(bin: Option<&Path>) -> (Option<String>, Option<String>) {
    let Some(bin) = bin else {
        return (None, None);
    };
    let mut cmd = std::process::Command::new(bin);
    cmd.arg("--version").stdin(std::process::Stdio::null());
    let outcome = crate::cmd_out::run_command(cmd, GH_TIMEOUT);
    let Some(out) = outcome.ok_output() else {
        return (None, None);
    };
    let text = String::from_utf8_lossy(&out.stdout);
    (
        semver::extract_version(&text),
        Some(semver::extract_commit(&text).unwrap_or_else(|| "unknown".to_string())),
    )
}

fn read_source_version(root: &Path) -> Option<String> {
    let raw = std::fs::read_to_string(root.join("VERSION")).ok()?;
    let trimmed: String = raw.chars().filter(|c| !c.is_whitespace()).collect();
    (!trimmed.is_empty()).then_some(trimmed)
}

fn read_source_commit(root: &Path) -> Option<String> {
    crate::script_helpers::run_git(root, &["rev-parse", "--short", "HEAD"])
        .ok_stdout_trimmed()
        .or_else(|| Some("unknown".to_string()))
}

#[cfg(test)]
mod tests;
