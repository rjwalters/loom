//! Conditional fetch of a fleet store into a local cache.
//!
//! # Wire shape (GitHub REST)
//!
//! 1. `GET repos/{repo}/commits/{ref}` with `Accept: application/vnd.github.sha`
//!    and `If-None-Match` — the steady state is a single `304`, which GitHub
//!    does not charge against the rate limit.
//! 2. On a new commit: `GET repos/{repo}/git/trees/{sha}?recursive=1`, then
//!    `GET repos/{repo}/git/blobs/{blob}` for each contract file whose blob is
//!    not already cached. Blobs are content-addressed, so an unchanged file is
//!    never fetched twice.
//!
//! # Cache layout
//!
//! `<cache>/manifest.json` records the repo, ref, commit SHA, ETag, when the
//! forge last confirmed it, and each contract path's blob SHA; blob bodies live
//! in `<cache>/blobs/<sha>`. Blobs are written before the manifest, and the
//! manifest is replaced atomically, so the cache is always a complete snapshot
//! of *some* commit.
//!
//! # Freshness policy
//!
//! [`load`] takes a [`Policy`]. Config and run state may be served from the
//! last good snapshot when the forge is unreachable ([`Policy::AllowStale`]),
//! with a staleness warning. The roster is [`Policy::FailClosed`]: it must be
//! confirmed current by the forge in this invocation (a `304` counts), and any
//! fetch failure is an error.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::StoreLocation;

/// One HTTP reply from the forge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reply {
    /// HTTP status.
    pub status: u16,
    /// The `ETag` header, if any.
    pub etag: Option<String>,
    /// The body.
    pub body: String,
}

/// The network seam: one `GET` of a forge REST path. `Err` means the request
/// could not be made at all; any HTTP status comes back as a [`Reply`].
/// Production uses [`super::gh::GhTransport`]; tests inject a fake.
pub trait Transport {
    /// `GET api_path` (e.g. `repos/o/r/git/trees/…`), optionally with an
    /// `Accept` media type and an `If-None-Match` validator.
    fn get(&self, api_path: &str, accept: Option<&str>, etag: Option<&str>) -> Result<Reply>;
}

const MANIFEST: &str = "manifest.json";
const BLOBS: &str = "blobs";
const MANIFEST_VERSION: u32 = 1;

/// What the cache records about the snapshot it holds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Manifest {
    /// Cache format version.
    pub version: u32,
    /// `OWNER/REPO`.
    pub repo: String,
    /// The ref that was resolved.
    pub reference: String,
    /// The commit the snapshot is of.
    pub commit: String,
    /// The validator for the ref-resolution request.
    pub etag: Option<String>,
    /// When the forge last confirmed `commit` is current for `reference`.
    pub confirmed_at: DateTime<Utc>,
    /// Contract path → blob SHA.
    pub files: BTreeMap<String, String>,
}

/// A complete snapshot of the store's contract files at one commit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    /// The manifest describing it.
    pub manifest: Manifest,
    /// Contract path → file content.
    pub files: BTreeMap<String, Vec<u8>>,
}

impl Snapshot {
    /// A file as UTF-8, or `None` when the store does not have it.
    pub fn text(&self, path: &str) -> Result<Option<String>> {
        self.files
            .get(path)
            .map(|b| String::from_utf8(b.clone()).with_context(|| format!("{path} is not UTF-8")))
            .transpose()
    }

    /// Short form of the commit for messages.
    #[must_use]
    pub fn short_commit(&self) -> &str {
        let c = &self.manifest.commit;
        &c[..c.len().min(12)]
    }
}

/// How [`load`] may treat an unreachable forge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Policy {
    /// The snapshot must be confirmed current now; any failure is an error.
    FailClosed,
    /// Fetch, but fall back to the last good cache (with a warning) on failure.
    AllowStale,
    /// Do not contact the forge; read the cache.
    Offline,
}

/// Where a loaded snapshot came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Freshness {
    /// Confirmed current by the forge in this invocation.
    Live {
        /// Whether this fetch moved the cache to a new commit.
        changed: bool,
    },
    /// Served from the cache without confirmation.
    Cached {
        /// Why the forge was not consulted or did not answer.
        why: String,
    },
}

/// A snapshot and how fresh it is.
#[derive(Debug, Clone)]
pub struct Loaded {
    /// The snapshot.
    pub snapshot: Snapshot,
    /// Its freshness.
    pub freshness: Freshness,
}

impl Loaded {
    /// A one-line warning for a cached snapshot, naming its age; `None` when
    /// live.
    #[must_use]
    pub fn staleness_warning(&self, now: DateTime<Utc>) -> Option<String> {
        let Freshness::Cached { why } = &self.freshness else {
            return None;
        };
        let m = &self.snapshot.manifest;
        Some(format!(
            "warning: using the CACHED fleet store snapshot of {} @ {} (commit {}), last confirmed \
             current {} ago — {why}",
            m.repo,
            m.reference,
            self.snapshot.short_commit(),
            human_age(now - m.confirmed_at),
        ))
    }
}

fn human_age(d: chrono::Duration) -> String {
    let s = d.num_seconds().max(0);
    match s {
        0..=119 => format!("{s}s"),
        120..=7199 => format!("{}m", s / 60),
        7200..=172_799 => format!("{}h", s / 3600),
        _ => format!("{}d", s / 86400),
    }
}

/// Load a snapshot of `location` under `policy`. See the module docs.
pub fn load(
    transport: &dyn Transport,
    cache_dir: &Path,
    location: &StoreLocation,
    policy: Policy,
    now: DateTime<Utc>,
) -> Result<Loaded> {
    let cached = || -> Result<Snapshot> {
        read_cache(cache_dir, location)?.ok_or_else(|| {
            anyhow!(
                "no cached snapshot of {} @ {} in {} — run `loom-daemon fleet-config fetch`",
                location.repo,
                location.reference,
                cache_dir.display()
            )
        })
    };
    match policy {
        Policy::Offline => Ok(Loaded {
            snapshot: cached()?,
            freshness: Freshness::Cached {
                why: "--offline".to_string(),
            },
        }),
        Policy::FailClosed => {
            let (snapshot, changed) = sync(transport, cache_dir, location, now)?;
            Ok(Loaded {
                snapshot,
                freshness: Freshness::Live { changed },
            })
        }
        Policy::AllowStale => match sync(transport, cache_dir, location, now) {
            Ok((snapshot, changed)) => Ok(Loaded {
                snapshot,
                freshness: Freshness::Live { changed },
            }),
            Err(e) => {
                let snapshot = cached().map_err(|c| anyhow!("{e:#}; and {c:#}"))?;
                Ok(Loaded {
                    snapshot,
                    freshness: Freshness::Cached {
                        why: format!("fetch failed: {e:#}"),
                    },
                })
            }
        },
    }
}

/// Bring the cache up to date with the forge and return the snapshot, with
/// whether it moved to a new commit. Any failure is an error; the previous
/// cache is left intact.
pub fn sync(
    transport: &dyn Transport,
    cache_dir: &Path,
    location: &StoreLocation,
    now: DateTime<Utc>,
) -> Result<(Snapshot, bool)> {
    let previous = read_cache(cache_dir, location).unwrap_or_else(|e| {
        log::warn!("fleet_store: ignoring unreadable cache in {}: {e:#}", cache_dir.display());
        None
    });
    let etag = previous.as_ref().and_then(|s| s.manifest.etag.clone());
    let commit_path = format!("repos/{}/commits/{}", location.repo, location.reference);
    let reply = transport
        .get(&commit_path, Some("application/vnd.github.sha"), etag.as_deref())
        .with_context(|| format!("could not reach the forge for {}", location.repo))?;

    let (commit, new_etag) = match reply.status {
        304 => {
            let Some(mut snap) = previous else {
                bail!("forge answered 304 Not Modified but there is no cached snapshot");
            };
            snap.manifest.confirmed_at = now;
            write_manifest(cache_dir, &snap.manifest)?;
            return Ok((snap, false));
        }
        200 => (reply.body.trim().to_string(), reply.etag),
        s => bail!(
            "forge answered HTTP {s} resolving {} @ {}{}{}",
            location.repo,
            location.reference,
            error_detail(&reply.body),
            if s == 404 {
                " — a 404 also means this credential cannot see the repo: check the ref, and that \
                 the GitHub App installation includes the store with contents: read"
            } else {
                ""
            }
        ),
    };
    if commit.len() != 40 || !commit.bytes().all(|b| b.is_ascii_hexdigit()) {
        bail!("forge returned `{}` for a commit SHA", truncate(&commit, 60));
    }
    if let Some(mut snap) = previous.filter(|s| s.manifest.commit == commit) {
        snap.manifest.confirmed_at = now;
        snap.manifest.etag = new_etag;
        write_manifest(cache_dir, &snap.manifest)?;
        return Ok((snap, false));
    }

    let tree_path = format!("repos/{}/git/trees/{commit}?recursive=1", location.repo);
    let tree = transport.get(&tree_path, None, None)?;
    if tree.status != 200 {
        bail!(
            "forge answered HTTP {} listing commit {commit}{}",
            tree.status,
            error_detail(&tree.body)
        );
    }
    let tree: TreeReply = serde_json::from_str(&tree.body).context("malformed tree response")?;
    if tree.truncated {
        bail!("the store's tree listing was truncated — the store is too large to read");
    }

    let blob_dir = cache_dir.join(BLOBS);
    std::fs::create_dir_all(&blob_dir)
        .with_context(|| format!("could not create {}", blob_dir.display()))?;
    let mut files = BTreeMap::new();
    let mut bodies = BTreeMap::new();
    for entry in tree
        .tree
        .iter()
        .filter(|e| e.kind == "blob" && super::is_contract_path(&e.path))
    {
        if !entry.sha.bytes().all(|b| b.is_ascii_hexdigit()) || entry.sha.is_empty() {
            bail!("malformed blob SHA for {}", entry.path);
        }
        let blob_path = blob_dir.join(&entry.sha);
        let body = match std::fs::read(&blob_path) {
            Ok(b) => b,
            Err(_) => {
                let b = fetch_blob(transport, &location.repo, &entry.sha)
                    .with_context(|| format!("fetching {}", entry.path))?;
                write_atomic(&blob_path, &b)?;
                b
            }
        };
        files.insert(entry.path.clone(), entry.sha.clone());
        bodies.insert(entry.path.clone(), body);
    }
    let manifest = Manifest {
        version: MANIFEST_VERSION,
        repo: location.repo.clone(),
        reference: location.reference.clone(),
        commit,
        etag: new_etag,
        confirmed_at: now,
        files,
    };
    write_manifest(cache_dir, &manifest)?;
    prune_blobs(&blob_dir, &manifest);
    Ok((
        Snapshot {
            manifest,
            files: bodies,
        },
        true,
    ))
}

#[derive(Deserialize)]
struct TreeReply {
    tree: Vec<TreeEntry>,
    #[serde(default)]
    truncated: bool,
}

#[derive(Deserialize)]
struct TreeEntry {
    path: String,
    #[serde(rename = "type")]
    kind: String,
    sha: String,
}

#[derive(Deserialize)]
struct BlobReply {
    content: String,
    encoding: String,
}

fn fetch_blob(transport: &dyn Transport, repo: &str, sha: &str) -> Result<Vec<u8>> {
    let reply = transport.get(&format!("repos/{repo}/git/blobs/{sha}"), None, None)?;
    if reply.status != 200 {
        bail!(
            "forge answered HTTP {} for blob {sha}{}",
            reply.status,
            error_detail(&reply.body)
        );
    }
    let blob: BlobReply = serde_json::from_str(&reply.body).context("malformed blob response")?;
    if blob.encoding != "base64" {
        bail!("unexpected blob encoding `{}`", blob.encoding);
    }
    use base64::{engine::general_purpose, Engine as _};
    let compact: String = blob
        .content
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect();
    general_purpose::STANDARD
        .decode(compact)
        .context("blob content is not valid base64")
}

/// The cached snapshot for `location`, `Ok(None)` when there is none (or it is
/// of a different repo/ref), `Err` when it is present but inconsistent.
pub fn read_cache(cache_dir: &Path, location: &StoreLocation) -> Result<Option<Snapshot>> {
    let path = cache_dir.join(MANIFEST);
    let raw = match std::fs::read_to_string(&path) {
        Ok(r) => r,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    let manifest: Manifest =
        serde_json::from_str(&raw).with_context(|| format!("parsing {}", path.display()))?;
    if manifest.version != MANIFEST_VERSION
        || manifest.repo != location.repo
        || manifest.reference != location.reference
    {
        return Ok(None);
    }
    let mut files = BTreeMap::new();
    for (p, sha) in &manifest.files {
        let blob = cache_dir.join(BLOBS).join(sha);
        let body =
            std::fs::read(&blob).with_context(|| format!("cache is missing blob {sha} for {p}"))?;
        files.insert(p.clone(), body);
    }
    Ok(Some(Snapshot { manifest, files }))
}

fn write_manifest(cache_dir: &Path, manifest: &Manifest) -> Result<()> {
    let mut body = serde_json::to_vec_pretty(manifest)?;
    body.push(b'\n');
    write_atomic(&cache_dir.join(MANIFEST), &body)
}

fn prune_blobs(blob_dir: &Path, manifest: &Manifest) {
    let Ok(entries) = std::fs::read_dir(blob_dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if !manifest.files.values().any(|s| *s == name) {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// Write `body` to `path` atomically (temp file in the same directory, then
/// rename), creating the parent directory.
pub fn write_atomic(path: &Path, body: &[u8]) -> Result<()> {
    use std::io::Write as _;
    let parent: PathBuf = path
        .parent()
        .map_or_else(|| PathBuf::from("."), Path::to_path_buf);
    std::fs::create_dir_all(&parent)
        .with_context(|| format!("could not create {}", parent.display()))?;
    let mut tmp = tempfile::NamedTempFile::new_in(&parent)
        .with_context(|| format!("could not create a temp file in {}", parent.display()))?;
    tmp.write_all(body)?;
    tmp.as_file().sync_all()?;
    tmp.persist(path)
        .map_err(|e| anyhow!("could not replace {}: {}", path.display(), e.error))?;
    Ok(())
}

fn error_detail(body: &str) -> String {
    serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|v| {
            v.get("message")
                .and_then(|m| m.as_str())
                .map(str::to_string)
        })
        .map(|m| format!(": {m}"))
        .unwrap_or_default()
}

fn truncate(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

#[cfg(test)]
#[path = "tests/fetch_tests.rs"]
mod tests;
