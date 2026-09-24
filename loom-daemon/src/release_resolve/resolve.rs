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

/// The repo this binary was built from, compiled in via Cargo's `repository`
/// field (`repository.workspace = true` in `loom-daemon/Cargo.toml`, #8513) —
/// tier 3 of [`resolve`]'s priority order, populated into
/// [`Inputs::build_time_repo`] by every caller (`auto_update::native_probe`,
/// the `loom-daemon release resolve` CLI) so they share one implementation.
///
/// `env!("CARGO_PKG_REPOSITORY")` is always set at compile time — empty when
/// `Cargo.toml` has no `repository` field (e.g. an unconfigured fork), never
/// missing outright — so this is a `const` read, not I/O. Parsed through the
/// same [`host::slug_from_remote_url`] every other tier uses, so a
/// `https://github.com/owner/repo` value here resolves identically to the
/// workspace-`origin` tier it outranks.
#[must_use]
pub fn build_time_repo() -> Option<String> {
    host::slug_from_remote_url(env!("CARGO_PKG_REPOSITORY"))
}

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
    /// `LOOM_DAEMON_UPDATE_GH_REPO` — tier 1 (highest-priority) repo override.
    pub repo_override: Option<String>,
    /// `LOOM_MACHINE_CHECKOUT` — a path to a Loom source checkout on this
    /// host (Epic #3835 Phase 3b), consulted for its OWN `origin` remote as
    /// tier 2 of the repo-resolution order (#8513). This is what makes a
    /// machine-mode host (workspace == some consumer repo, checkout ==
    /// this field) resolve the Loom release repo instead of the
    /// workspace's: the shell-era `loom-daemon-update.sh` already consulted
    /// this env var (line ~1369), but the native resolver did not.
    pub machine_checkout: Option<PathBuf>,
    /// The repo this binary was BUILT from, compiled in via Cargo's
    /// `repository` field (tier 3, #8513) — see
    /// [`super::host::slug_from_remote_url`]'s caller in
    /// `auto_update::native_probe`. `None` on a build with no `repository`
    /// configured (e.g. an unconfigured fork). Unlike every tier above and
    /// below it, this can NEVER resolve to a consumer's own repo: it is fixed
    /// at compile time by the checkout the release was actually built from,
    /// which is why it outranks the workspace's own (tier 4) `origin`.
    pub build_time_repo: Option<String>,
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

/// Every asset name a release publishes, per the release's own metadata:
/// `tag`'s assets, or the repo's LATEST release when `tag` is `None`.
///
/// `None` means the question could not be answered (the query failed, timed
/// out, or decoded to something unusable) and is never the same as
/// `Some(vec![])`, "this release publishes nothing". Keeping those two apart is
/// the point: [`crate::release_fetch`] uses this to tell an unsigned release
/// from one whose `.sig` merely would not download (#8197), and folding
/// *unknown* into *absent* there is exactly the fail-open that issue closes.
#[must_use]
pub fn asset_names(root: &Path, repo: &str, tag: Option<&str>) -> Option<Vec<String>> {
    let mut args: Vec<&str> = vec!["release", "view"];
    if let Some(t) = tag {
        args.push(t);
    }
    args.extend_from_slice(&["--json", "assets", "-R", repo]);
    let q: Query<Assets> = gh_query(&args, root, false, |_: &Assets| false);
    match q {
        Query::Populated(a) => Some(a.assets.into_iter().map(|a| a.name).collect()),
        _ => None,
    }
}

/// Which repo's releases to ask about, by **descending** priority (#8513).
///
/// The daemon binary is released from exactly one project, so the workspace's
/// own `origin` — which is whatever repo this daemon happens to be managing —
/// is the *worst* available answer and therefore the last resort, not the
/// default it used to be:
///
/// 1. [`Inputs::repo_override`] (`LOOM_DAEMON_UPDATE_GH_REPO`) — an explicit
///    operator answer always wins.
/// 2. The `origin` of [`Inputs::machine_checkout`] (`LOOM_MACHINE_CHECKOUT`) —
///    a dedicated Loom source checkout this host already points at.
/// 3. [`Inputs::build_time_repo`] — the repo this binary was *built* from,
///    compiled in (see [`build_time_repo`]). Cannot name a consumer repo.
/// 4. The `origin` of [`Inputs::repo_root`], the workspace itself.
///
/// The incident this order exists for: a daemon whose workspace is a consumer
/// repo resolved tier 4, asked *that* project for `loom-daemon-<target>`
/// assets, found none, and reported the soft "no artifact for this platform"
/// forever — one release short of a feature it needed, with nothing
/// escalating. An empty string at any tier means "unset" (Cargo reports an
/// absent `repository` field as `""`, and an exported-but-empty env var is
/// how a shell says "not set"), never a literal empty slug.
#[must_use]
pub fn resolve_repo(inputs: &Inputs<'_>) -> Option<String> {
    inputs
        .repo_override
        .clone()
        .filter(|r| !r.is_empty())
        .or_else(|| {
            inputs
                .machine_checkout
                .as_deref()
                .filter(|p| !p.as_os_str().is_empty())
                .and_then(host::repo_slug)
        })
        .or_else(|| inputs.build_time_repo.clone().filter(|r| !r.is_empty()))
        .or_else(|| host::repo_slug(inputs.repo_root))
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

    let Some(repo) = resolve_repo(inputs) else {
        return Resolution::Unresolved(
            "could not resolve owner/repo from LOOM_DAEMON_UPDATE_GH_REPO, \
             LOOM_MACHINE_CHECKOUT's git remote 'origin', the repo this binary was built from, \
             or the workspace's own git remote 'origin' \
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
    // An unreadable asset list is `None` here and resolves to the same empty
    // list it always did: unresolved, fall back to source. (The fetch path
    // treats that `None` differently -- see `asset_names`.)
    let names: Vec<String> = asset_names(root, &repo, None).unwrap_or_default();
    if !names.contains(&bin_name) || !names.contains(&sha_name) {
        return Resolution::Unresolved(no_artifact_reason(
            &tag, &repo, &target, &bin_name, &sha_name,
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

/// The "this release publishes nothing for my platform" reason, which **names
/// the repo it asked** (#8513).
///
/// A separate function only so the wording is assertable without a forge call:
/// this is the exact line the wrong-repo incident emitted for 20+ ticks
/// (`release v0.11.0 has no artifact for target x86_64-unknown-linux-gnu …`),
/// and with no repo in it, "I asked a completely different project for Loom's
/// assets" reads identically to "Loom has not built this platform yet".
fn no_artifact_reason(
    tag: &str,
    repo: &str,
    target: &str,
    bin_name: &str,
    sha_name: &str,
) -> String {
    format!(
        "release {tag} of {repo} has no artifact for target {target} \
         (checked for {bin_name} + {sha_name})"
    )
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
