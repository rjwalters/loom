//! The installed-Loom / daemon compatibility contract (#10716, tracker #10698
//! decisions D3/D4).
//!
//! Each side declares what it needs from the other, so a version mismatch on
//! its own never decides anything:
//!
//! * the **installed Loom** (`.loom/install-metadata.json` on a repo's default
//!   branch) records `loom_version`, the release whose installed files these
//!   are, and `requires_daemon`, the oldest daemon those files work with;
//! * the **daemon** knows its running version (`CARGO_PKG_VERSION`) and
//!   [`SUPPORTS_INSTALLED`], the oldest installed Loom it still works with.
//!
//! [`classify`] turns one pair into a [`Compat`], the input to the workspace
//! states (W0-W4) the tracker defines. Nothing here acts on the result: the
//! resync (#10717, #10718) and the dispatch hold (#10719) do. The claims are
//! proven in CI by [`harness`] across adjacent releases.
//!
//! Both constants move only when a release breaks compatibility. Where and how
//! they are bumped is in `defaults/docs/release-cadence.md`
//! ("Compatibility contract").

use serde::Deserialize;
use std::fmt;
use std::path::Path;
use std::process::Command;

pub mod harness;

/// The oldest installed Loom (`install-metadata.json` `loom_version`) this
/// daemon works with.
///
/// 0.19.0 is the first release whose installed tree carries every file in
/// [`DAEMON_INVOKED_INSTALLED_FILES`]. No break has been declared since.
pub const SUPPORTS_INSTALLED: &str = "0.19.0";

/// The oldest daemon the installed files this release ships work with. The
/// install-metadata writers record it as `requires_daemon`.
///
/// It must be at least every hard `# requires-daemon: <sub> >= <version>`
/// floor in the shipped shell (`defaults/`); the CI harness enforces that.
/// 0.19.772 is the highest such floor today (`gh-shim`, #10516).
///
/// Keep this on one line in exactly this shape: the installer
/// (`scripts/install/loom-source-path.sh`) reads it from this file with
/// `sed`, because the tree it installs can be newer than any binary it has.
pub const REQUIRES_DAEMON: &str = "0.19.772";

// compat:invoked-files:begin
/// The installed files this daemon executes in a workspace. An installed tree
/// missing one of them cannot be dispatched into, whatever its version says.
///
/// The CI harness reads the list for an OLD daemon out of that release's copy
/// of this file, between the `compat:invoked-files` markers, so keep one
/// quoted path per line.
pub const DAEMON_INVOKED_INSTALLED_FILES: &[&str] = &[
    ".loom/scripts/spawn-worker.sh",
    ".loom/scripts/claude-wrapper.sh",
    ".loom/scripts/sweep-lease-publish.sh",
    ".loom/scripts/sweep-lease-renew.sh",
];
// compat:invoked-files:end

/// Where the installed Loom's metadata lives, relative to the repo root.
pub const INSTALL_METADATA_PATH: &str = ".loom/install-metadata.json";

/// A `MAJOR.MINOR.PATCH` release version. Strict on purpose: `"unknown"`, an
/// empty string or a pre-release suffix is not a version, and the classifier
/// treats it as "not recorded" rather than guessing an order for it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Version {
    /// Major component.
    pub major: u64,
    /// Minor component.
    pub minor: u64,
    /// Patch component.
    pub patch: u64,
}

impl Version {
    /// Parse `MAJOR.MINOR.PATCH`, with an optional leading `v` and surrounding
    /// whitespace. Anything else is `None`.
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        let s = s.trim();
        let s = s.strip_prefix('v').unwrap_or(s);
        let mut parts = s.split('.');
        let mut next = || -> Option<u64> {
            let p = parts.next()?;
            if p.is_empty() || !p.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            p.parse().ok()
        };
        let v = Self {
            major: next()?,
            minor: next()?,
            patch: next()?,
        };
        if parts.next().is_some() {
            return None;
        }
        Some(v)
    }

    /// The next patch release: what the post-merge bump of `VERSION` makes.
    #[must_use]
    pub fn next_patch(self) -> Self {
        Self {
            patch: self.patch + 1,
            ..self
        }
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

/// The contract fields of `.loom/install-metadata.json`. Every other key in
/// the file is ignored here. Both fields default to `None`, so metadata
/// written before #10716 (no `requires_daemon`) still parses.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct InstallMeta {
    /// The release whose installed files these are: the repo's active version.
    #[serde(default)]
    pub loom_version: Option<String>,
    /// The oldest daemon these installed files work with.
    #[serde(default)]
    pub requires_daemon: Option<String>,
}

impl InstallMeta {
    /// Parse the contents of an `install-metadata.json`.
    ///
    /// # Errors
    /// The text is not a JSON object, or a contract field is not a string.
    pub fn parse(json: &str) -> Result<Self, serde_json::Error> {
        // Via `Value` so a top-level array is refused: serde would otherwise
        // read `[]` as a struct with every field defaulted.
        let value: serde_json::Value = serde_json::from_str(json)?;
        if !value.is_object() {
            return Err(serde::de::Error::custom("install metadata is not a JSON object"));
        }
        serde_json::from_value(value)
    }
}

/// What the daemon side brings to a classification.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DaemonCompat {
    /// The RUNNING daemon version, never the on-disk binary's.
    pub running: Version,
    /// The oldest installed Loom this daemon works with.
    pub supports_installed: Version,
    /// The fleet floor (`loom_min_version`), when one is set. An installed
    /// version below it is treated like one below `supports_installed`.
    pub floor: Option<Version>,
}

impl DaemonCompat {
    /// This binary's own side of the contract.
    ///
    /// `None` only if this crate's version or [`SUPPORTS_INSTALLED`] is not
    /// `MAJOR.MINOR.PATCH`, which a unit test rules out.
    #[must_use]
    pub fn this_binary(floor: Option<Version>) -> Option<Self> {
        Some(Self {
            running: Version::parse(env!("CARGO_PKG_VERSION"))?,
            supports_installed: Version::parse(SUPPORTS_INSTALLED)?,
            floor,
        })
    }
}

/// The classification of one installed Loom against one daemon. Each variant
/// is the input to one workspace state of tracker #10698:
///
/// | Variant | Workspace state | Dispatch |
/// |---|---|---|
/// | `Compatible` | W0 Current or W1 Stale (an installed-file diff decides) | yes |
/// | `ResyncOwed` | W1 Stale | yes |
/// | `InstalledTooOld` | W3 Too old | held |
/// | `NeedsNewerDaemon` | W4 Needs newer daemon | held |
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Compat {
    /// Both claims are recorded and both hold.
    Compatible,
    /// No usable claim recorded: `loom_version` or `requires_daemon` is
    /// absent or not a version. Every install before #10716 lands here. It
    /// keeps dispatching as today, and its first resync writes the fields.
    ResyncOwed,
    /// The installed `loom_version` is below this daemon's
    /// `supports_installed`, or below the fleet floor.
    InstalledTooOld,
    /// The installed files' `requires_daemon` is above the running daemon.
    NeedsNewerDaemon,
}

/// Classify an installed Loom against a daemon. Pure.
///
/// Order matters, and each step is the conservative reading:
///
/// 1. `requires_daemon` above the running daemon wins over everything. The
///    files are ahead of this host; resyncing them from this daemon would be
///    a downgrade, which the fix-forward principle rules out.
/// 2. A recorded `loom_version` below `supports_installed` or the floor is
///    too old, whether or not `requires_daemon` was recorded. The migration
///    rule ("no `requires_daemon` means compatible but owed a resync") only
///    holds while the daemon still supports that version.
/// 3. Any claim missing or unparseable means a resync is owed.
///
/// `Compatible` and `ResyncOwed` say nothing about the resync's direction.
/// A present but unparseable `requires_daemon` lands in `ResyncOwed`, and a
/// recorded `loom_version` above the running daemon is not checked here at
/// all. So the resync itself (#10717, #10718) must refuse whenever the
/// installed `loom_version` is above the running daemon, whatever
/// `requires_daemon` parses to: that resync would be the downgrade step 1
/// rules out.
#[must_use]
pub fn classify(installed: &InstallMeta, daemon: &DaemonCompat) -> Compat {
    let requires = installed
        .requires_daemon
        .as_deref()
        .and_then(Version::parse);
    let version = installed.loom_version.as_deref().and_then(Version::parse);

    if requires.is_some_and(|r| r > daemon.running) {
        return Compat::NeedsNewerDaemon;
    }
    if let Some(v) = version {
        let oldest = daemon
            .floor
            .map_or(daemon.supports_installed, |f| f.max(daemon.supports_installed));
        if v < oldest {
            return Compat::InstalledTooOld;
        }
    }
    if requires.is_none() || version.is_none() {
        return Compat::ResyncOwed;
    }
    Compat::Compatible
}

/// Read a repo's install metadata as it stands on its default branch, from
/// the local clone's remote-tracking ref (`origin/HEAD`, else `origin/main`).
/// The caller fetches first if it needs it current.
///
/// `Ok(None)` when the default branch has no `.loom/install-metadata.json`
/// (Loom is not installed there).
///
/// # Errors
/// No default-branch ref resolves, `git` cannot run, or the file is not
/// valid metadata JSON.
pub fn read_default_branch(repo_root: &Path) -> anyhow::Result<Option<InstallMeta>> {
    let branch = default_branch_ref(repo_root).ok_or_else(|| {
        anyhow::anyhow!(
            "no default branch ref (origin/HEAD or origin/main) in {}",
            repo_root.display()
        )
    })?;
    read_at_ref(repo_root, &branch)
}

/// Read the install metadata at an arbitrary git ref. `Ok(None)` when the
/// ref has no `.loom/install-metadata.json`.
///
/// # Errors
/// `git` cannot run, the ref does not resolve, or the file does not parse.
pub fn read_at_ref(repo_root: &Path, git_ref: &str) -> anyhow::Result<Option<InstallMeta>> {
    let spec = format!("{git_ref}:{INSTALL_METADATA_PATH}");
    let exists = git(repo_root, &["cat-file", "-e", &spec])?;
    if !exists.status.success() {
        let commit = git(
            repo_root,
            &[
                "rev-parse",
                "--verify",
                "--quiet",
                &format!("{git_ref}^{{commit}}"),
            ],
        )?;
        anyhow::ensure!(
            commit.status.success(),
            "ref {git_ref} does not resolve in {}",
            repo_root.display()
        );
        return Ok(None);
    }
    let shown = git(repo_root, &["show", &spec])?;
    anyhow::ensure!(
        shown.status.success(),
        "git show {spec} failed: {}",
        String::from_utf8_lossy(&shown.stderr).trim()
    );
    let meta = InstallMeta::parse(&String::from_utf8_lossy(&shown.stdout))
        .map_err(|e| anyhow::anyhow!("{spec} is not valid install metadata: {e}"))?;
    Ok(Some(meta))
}

fn default_branch_ref(repo_root: &Path) -> Option<String> {
    let head = git(
        repo_root,
        &[
            "symbolic-ref",
            "--quiet",
            "--short",
            "refs/remotes/origin/HEAD",
        ],
    )
    .ok()?;
    if head.status.success() {
        let name = String::from_utf8_lossy(&head.stdout).trim().to_string();
        if !name.is_empty() {
            return Some(name);
        }
    }
    let main = git(
        repo_root,
        &[
            "rev-parse",
            "--verify",
            "--quiet",
            "refs/remotes/origin/main",
        ],
    )
    .ok()?;
    main.status.success().then(|| "origin/main".to_string())
}

fn git(repo_root: &Path, args: &[&str]) -> anyhow::Result<std::process::Output> {
    Command::new("git")
        .arg("-C")
        .arg(repo_root)
        .args(args)
        .output()
        .map_err(|e| anyhow::anyhow!("could not run git: {e}"))
}

#[cfg(test)]
mod tests;
