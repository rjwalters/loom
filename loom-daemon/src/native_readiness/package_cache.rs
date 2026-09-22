//! A keyed, user-home cache for **package artifacts only** (#8581).
//!
//! # Why a cache is even a candidate
//!
//! `native_tools::provision::state::create` gives every guarded launch a fresh
//! `uuid`-named state directory and points `XDG_CACHE_HOME` inside it, so the
//! pinned `@opencode-ai/plugin` set is re-resolved on every launch. Package
//! resolution is the one boundary in [`super::Stage`] that is (a) identical
//! across launches for a fixed CLI/plugin/platform triple and (b) made of
//! content-addressed, immutable artifacts. It is therefore the only boundary
//! whose work is safe to share.
//!
//! # What may be shared, and what may never be
//!
//! Sharing is **allowlist-driven and enforced twice**:
//!
//! 1. [`PackageCache::publish`] copies only the relative paths the caller names
//!    explicitly. Nothing else in the built tree is even looked at.
//! 2. Every file that survives (1) is checked against [`FORBIDDEN_NAMES`] /
//!    [`FORBIDDEN_EXTENSIONS`] / [`FORBIDDEN_COMPONENTS`] and the publish
//!    **fails** if any of them matches. A silent skip would let a future
//!    allowlist widening leak state without anyone noticing; a hard failure
//!    cannot.
//!
//! Authentication files, session stores, databases, transcripts, prompts and
//! mutable per-worker configuration are not "excluded by convention" here —
//! they are unrepresentable in a published entry, and the tests assert that a
//! tree containing them fails to publish rather than publishing a subset.
//!
//! # Identity
//!
//! [`CacheIdentity`] keys on the exact CLI version string, the exact plugin
//! pin, the platform and architecture, and a SHA-256 of the manifest bytes
//! actually provisioned. Changing any one of the five changes the key, so a
//! CLI upgrade, a pin bump, a cross-architecture home directory (a shared NFS
//! `$HOME`, a container with a different libc target) and an edited manifest
//! each land on a different entry instead of silently reusing an incompatible
//! one.
//!
//! # Concurrency
//!
//! Two mechanisms, with the cheap one being advisory and the strict one being
//! the actual correctness guarantee:
//!
//! * **Atomic publication.** An entry is built in a private staging directory
//!   and moved into place with a single `rename`. A reader therefore sees
//!   either no entry or a complete one — never a half-copied `node_modules`.
//!   If a peer wins the race, the loser discards its staging tree and adopts
//!   the winner's entry.
//! * **Advisory lock.** [`crate::tokens_pool::locking::MkdirLock`] (mkdir-based:
//!   `flock` is unavailable on stock macOS, which is why this repo has no
//!   `flock` locks) is taken non-blockingly to avoid N workers doing the same
//!   install. Failing to get it is never fatal — the work is merely repeated,
//!   and atomic publication still keeps the result correct.
//!
//! # Invalidation
//!
//! [`PackageCache::lookup`] re-verifies the stored manifest against the
//! requested identity *and* re-digests every recorded file. A stale, truncated,
//! partially-deleted or tampered entry is moved aside into a quarantine
//! directory — never repaired in place, never deleted outright — and reported
//! as [`super::CacheOutcome::Invalidated`] so the caller rebuilds in isolation.

use super::CacheOutcome;
use crate::tokens_pool::locking::MkdirLock;
use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    path::{Component, Path, PathBuf},
    time::Duration,
};

/// Exact file names that may never appear inside a shared entry.
pub const FORBIDDEN_NAMES: &[&str] = &[
    "auth.json",
    "authorization.json",
    "credentials.json",
    "credential.json",
    "config.json",
    "session.json",
    "sessions.json",
    ".env",
    ".netrc",
    "cookies.txt",
];

/// File extensions that may never appear inside a shared entry: durable state
/// and key material.
pub const FORBIDDEN_EXTENSIONS: &[&str] = &[
    "db",
    "db-wal",
    "db-shm",
    "sqlite",
    "sqlite3",
    "sqlite-wal",
    "sqlite-shm",
    "pem",
    "key",
    "p12",
    "pfx",
];

/// Path components that may never appear inside a shared entry: per-worker
/// mutable state trees.
pub const FORBIDDEN_COMPONENTS: &[&str] = &[
    "session",
    "sessions",
    "transcript",
    "transcripts",
    "storage",
    "auth",
    "prompt",
    "prompts",
];

/// Stale threshold for the advisory publication lock.
const LOCK_STALE: Duration = Duration::from_secs(600);

/// Cap on a single published entry, so a runaway install cannot fill `$HOME`.
const MAX_ENTRY_BYTES: u64 = 512 * 1024 * 1024;

/// Cap on file count, same reason.
const MAX_ENTRY_FILES: usize = 100_000;

/// Everything that must match for a published package set to be reusable.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CacheIdentity {
    /// The CLI's own reported version line, sanitized.
    pub cli_version: String,
    /// The pinned plugin dependency, e.g. `@opencode-ai/plugin@1.18.31`.
    pub plugin_pin: String,
    /// `std::env::consts::OS` of the building host.
    pub platform: String,
    /// `std::env::consts::ARCH` of the building host.
    pub arch: String,
    /// SHA-256 (hex) of the exact provisioned manifest bytes.
    pub manifest_digest: String,
}

impl CacheIdentity {
    /// Build an identity from the pieces a probe already has.
    #[must_use]
    pub fn new(cli_version: &str, plugin_pin: &str, manifest: &[u8]) -> Self {
        Self {
            cli_version: cli_version.to_owned(),
            plugin_pin: plugin_pin.to_owned(),
            platform: std::env::consts::OS.to_owned(),
            arch: std::env::consts::ARCH.to_owned(),
            manifest_digest: hex::encode(Sha256::digest(manifest)),
        }
    }

    /// The cache key: a SHA-256 over every identity component, length-prefixed
    /// so no two distinct identities can produce the same concatenation.
    #[must_use]
    pub fn key(&self) -> String {
        let mut hasher = Sha256::new();
        for part in [
            self.cli_version.as_str(),
            self.plugin_pin.as_str(),
            self.platform.as_str(),
            self.arch.as_str(),
            self.manifest_digest.as_str(),
        ] {
            hasher.update(part.len().to_le_bytes());
            hasher.update(part.as_bytes());
        }
        hex::encode(hasher.finalize())
    }
}

/// The integrity record stored inside a published entry.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct Manifest {
    schema: u32,
    identity: CacheIdentity,
    /// Every published file, as a relative path and its SHA-256.
    files: Vec<(String, String)>,
}

/// A user-home cache of package artifacts.
#[derive(Debug)]
pub struct PackageCache {
    base: PathBuf,
}

impl PackageCache {
    /// Open (creating if absent) the default cache under the user's home.
    ///
    /// # Errors
    ///
    /// Fails when there is no home directory, when the resolved base is not
    /// under it, or when it lies inside a repository checkout.
    pub fn user_home() -> Result<Self> {
        let home = dirs::home_dir().context("no home directory for the package cache")?;
        Self::open(&home.join(".local/state/loom/native-packages"))
    }

    /// Open (creating if absent) a cache at an explicit base directory.
    ///
    /// The base must be under the user's home directory and outside every
    /// repository checkout — a cache inside a checkout would be committable,
    /// and a cache outside `$HOME` is outside the ownership boundary the rest
    /// of Loom's private-state rules assume.
    ///
    /// # Errors
    ///
    /// Propagates a creation failure, a non-private directory, a base outside
    /// the home directory, or a base inside a repository.
    pub fn open(base: &Path) -> Result<Self> {
        let home = dirs::home_dir().context("no home directory")?;
        Self::open_under(base, &home)
    }

    /// [`Self::open`] with the home boundary supplied explicitly.
    ///
    /// Same shape as `telemetry_live::workspace::validate_private_paths`, and
    /// for the same reason: the boundary is a parameter so the rule itself can
    /// be exercised by a test without writing into the real `$HOME`.
    ///
    /// # Errors
    ///
    /// As [`Self::open`].
    pub(crate) fn open_under(base: &Path, home: &Path) -> Result<Self> {
        crate::native_tools::provision::private_directory(base)?;
        let base = base
            .canonicalize()
            .context("cannot resolve the package cache base")?;
        let home = home
            .canonicalize()
            .context("cannot resolve the home directory")?;
        ensure!(
            base.starts_with(&home),
            "the shared package cache must live under the user home directory"
        );
        crate::native_tools::provision::outside_every_repository(&base)?;
        Ok(Self { base })
    }

    /// The cache's base directory.
    #[must_use]
    pub fn base(&self) -> &Path {
        &self.base
    }

    fn entry(&self, identity: &CacheIdentity) -> PathBuf {
        self.base.join(identity.key())
    }

    /// Return a validated entry for `identity`, quarantining an invalid one.
    ///
    /// `Ok((None, Miss))` means nothing is published; `Ok((None, Invalidated))`
    /// means something was and has been moved aside.
    ///
    /// # Errors
    ///
    /// Propagates a filesystem failure that prevents answering at all.
    pub fn lookup(&self, identity: &CacheIdentity) -> Result<(Option<PathBuf>, CacheOutcome)> {
        let entry = self.entry(identity);
        if !entry.exists() {
            return Ok((None, CacheOutcome::Miss));
        }
        match validate(&entry, identity) {
            Ok(()) => Ok((Some(entry), CacheOutcome::Hit)),
            Err(_) => {
                self.quarantine(&entry)?;
                Ok((None, CacheOutcome::Invalidated))
            }
        }
    }

    /// Move a bad entry out of the way without destroying evidence.
    ///
    /// A single `rename` of the directory, which is atomic and works on a
    /// non-empty tree. A concurrent peer that already moved it leaves nothing
    /// to move, which is success, not an error.
    fn quarantine(&self, entry: &Path) -> Result<()> {
        let quarantine = self.base.join(".quarantine");
        crate::native_tools::provision::private_directory(&quarantine)?;
        let target = quarantine.join(uuid::Uuid::new_v4().to_string());
        match std::fs::rename(entry, &target) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error).context("cannot quarantine an invalid cache entry"),
        }
    }

    /// Publish the artifacts named by `allow` from `built` under `identity`.
    ///
    /// `allow` holds relative paths inside `built`; a directory entry is
    /// published recursively. Every resulting file must pass the forbidden-name
    /// checks or the whole publication fails without leaving a partial entry.
    ///
    /// Returns the published entry path, which may be a peer's if the peer won
    /// the `rename` race.
    ///
    /// # Errors
    ///
    /// Fails on a traversing/absolute `allow` path, a missing source, a
    /// forbidden file, a symlink anywhere in the copied set, or an entry over
    /// the size/count caps.
    pub fn publish(
        &self,
        identity: &CacheIdentity,
        built: &Path,
        allow: &[&str],
    ) -> Result<PathBuf> {
        let entry = self.entry(identity);
        let lock_path = self.base.join(format!("{}.lock", identity.key()));
        // Advisory only: a peer holding it means duplicated work, never a
        // wrong result, because `rename` below is the real serialization point.
        let _lock = MkdirLock::try_acquire(&lock_path, LOCK_STALE)
            .ok()
            .flatten();
        if let (Some(existing), CacheOutcome::Hit) = self.lookup(identity)? {
            return Ok(existing);
        }
        let staging = self.base.join(".staging");
        crate::native_tools::provision::private_directory(&staging)?;
        let staged = staging.join(uuid::Uuid::new_v4().to_string());
        crate::native_tools::provision::private_directory(&staged)?;
        let result = stage(built, &staged, allow, identity);
        match result {
            Ok(()) => {}
            Err(error) => {
                // Never leave a half-built tree where a reader could find it.
                let _ = std::fs::remove_dir_all(&staged);
                return Err(error);
            }
        }
        match std::fs::rename(&staged, &entry) {
            Ok(()) => Ok(entry),
            Err(_) => {
                // A peer published first (or the destination is otherwise
                // occupied). Its entry is authoritative; ours is discarded.
                let _ = std::fs::remove_dir_all(&staged);
                ensure!(
                    entry.exists(),
                    "cannot publish a package cache entry and none was published by a peer"
                );
                Ok(entry)
            }
        }
    }

    /// Copy a published entry's artifacts into `destination`.
    ///
    /// The integrity manifest itself is not restored: it describes the entry,
    /// and a worker's tree should not gain a file no launch writes.
    ///
    /// # Errors
    ///
    /// Propagates a filesystem failure, or refuses an entry that turns out to
    /// contain a symlink.
    pub fn restore(&self, entry: &Path, destination: &Path) -> Result<()> {
        for item in std::fs::read_dir(entry).context("cannot read a cache entry")? {
            let item = item?;
            if item.file_name() == MANIFEST {
                continue;
            }
            copy_tree(&item.path(), &destination.join(item.file_name()))?;
        }
        Ok(())
    }
}

/// Name of the integrity record inside a published entry.
const MANIFEST: &str = "loom-package-manifest.json";

/// Whether `relative` is safe and permitted inside a shared entry.
fn permitted(relative: &Path) -> Result<()> {
    ensure!(
        relative
            .components()
            .all(|c| matches!(c, Component::Normal(_))),
        "shared package artifacts must be named by plain relative paths"
    );
    for component in relative.components() {
        let Component::Normal(name) = component else {
            continue;
        };
        let lower = name.to_string_lossy().to_ascii_lowercase();
        ensure!(
            !FORBIDDEN_COMPONENTS.contains(&lower.as_str()),
            "refusing to share per-worker state: a path component names mutable worker state"
        );
    }
    let name = relative
        .file_name()
        .context("shared package artifact has no file name")?
        .to_string_lossy()
        .to_ascii_lowercase();
    ensure!(
        !FORBIDDEN_NAMES.contains(&name.as_str()),
        "refusing to share auth/session/config state through the package cache"
    );
    if let Some((_, extension)) = name.rsplit_once('.') {
        ensure!(
            !FORBIDDEN_EXTENSIONS.contains(&extension),
            "refusing to share a database or key artifact through the package cache"
        );
    }
    Ok(())
}

/// Copy the allowlisted paths into `staged` and write the integrity manifest.
fn stage(built: &Path, staged: &Path, allow: &[&str], identity: &CacheIdentity) -> Result<()> {
    let mut files = Vec::new();
    let mut bytes = 0u64;
    for name in allow {
        let relative = Path::new(name);
        permitted(relative)?;
        let source = built.join(relative);
        ensure!(source.exists(), "a named package artifact is missing from the built tree");
        collect(built, &source, staged, &mut files, &mut bytes)?;
    }
    ensure!(!files.is_empty(), "no package artifact was named for publication");
    files.sort();
    let manifest = Manifest {
        schema: 1,
        identity: identity.clone(),
        files,
    };
    std::fs::write(staged.join(MANIFEST), serde_json::to_vec(&manifest)?)
        .context("cannot write the cache entry manifest")
}

/// Recursively copy `source` (a file or directory under `built`) into `staged`,
/// recording each file's digest.
fn collect(
    built: &Path,
    source: &Path,
    staged: &Path,
    files: &mut Vec<(String, String)>,
    bytes: &mut u64,
) -> Result<()> {
    let metadata = std::fs::symlink_metadata(source)?;
    ensure!(
        !metadata.file_type().is_symlink(),
        "refusing to share a symlink through the package cache: its target is outside the entry"
    );
    let relative = source
        .strip_prefix(built)
        .context("package artifact escaped the built tree")?;
    permitted(relative)?;
    if metadata.is_dir() {
        crate::native_tools::provision::private_directory(&staged.join(relative))?;
        let mut entries: Vec<PathBuf> = std::fs::read_dir(source)?
            .map(|e| e.map(|e| e.path()))
            .collect::<std::io::Result<_>>()?;
        entries.sort();
        for child in entries {
            collect(built, &child, staged, files, bytes)?;
        }
        return Ok(());
    }
    ensure!(metadata.is_file(), "package artifacts must be regular files");
    *bytes += metadata.len();
    ensure!(*bytes <= MAX_ENTRY_BYTES, "package cache entry exceeds the size cap");
    ensure!(files.len() < MAX_ENTRY_FILES, "package cache entry exceeds the file cap");
    let content = std::fs::read(source)?;
    if let Some(parent) = staged.join(relative).parent() {
        crate::native_tools::provision::private_directory(parent)?;
    }
    std::fs::write(staged.join(relative), &content)?;
    files.push((relative.to_string_lossy().into_owned(), hex::encode(Sha256::digest(&content))));
    Ok(())
}

/// Copy a published file or directory tree to `destination`.
fn copy_tree(source: &Path, destination: &Path) -> Result<()> {
    let metadata = std::fs::symlink_metadata(source)?;
    ensure!(!metadata.file_type().is_symlink(), "a cache entry must not contain a symlink");
    if metadata.is_dir() {
        crate::native_tools::provision::private_directory(destination)?;
        for item in std::fs::read_dir(source)? {
            let item = item?;
            copy_tree(&item.path(), &destination.join(item.file_name()))?;
        }
        return Ok(());
    }
    if let Some(parent) = destination.parent() {
        crate::native_tools::provision::private_directory(parent)?;
    }
    std::fs::copy(source, destination)?;
    Ok(())
}

/// Re-verify a published entry against `identity` and its own digests.
fn validate(entry: &Path, identity: &CacheIdentity) -> Result<()> {
    let raw = std::fs::read(entry.join(MANIFEST)).context("cache entry has no manifest")?;
    let manifest: Manifest =
        serde_json::from_slice(&raw).context("cache entry manifest is unreadable")?;
    ensure!(manifest.schema == 1, "cache entry manifest schema is unknown");
    ensure!(
        &manifest.identity == identity,
        "cache entry identity does not match the requested identity"
    );
    ensure!(!manifest.files.is_empty(), "cache entry manifest records no files");
    for (relative, digest) in &manifest.files {
        let path = Path::new(relative);
        permitted(path)?;
        let content = std::fs::read(entry.join(path)).context("cache entry file is missing")?;
        ensure!(
            hex::encode(Sha256::digest(&content)) == *digest,
            "cache entry file does not match its recorded digest"
        );
    }
    Ok(())
}

#[cfg(test)]
#[path = "package_cache_tests.rs"]
mod tests;
