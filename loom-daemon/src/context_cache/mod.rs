//! `loom-daemon context` (#9783) — the durable, content-addressed retrieval
//! cache that the collision experiment (#9785's pilot) and footprint stage
//! (#9784) consume.
//!
//! # The contract
//!
//! A cache key is the SHA-256 over the *canonical input snapshot*: forge/repo
//! identity, issue identity, canonical title/body, the explicitly supplied
//! requirement-comment snapshot, the pinned source revision, the index
//! identity, the query-policy version and the adapter+schema versions. An
//! issue's `updatedAt` is deliberately NOT part of the key — it triggers
//! reconsideration (recompute the key, compare), never cache invalidation by
//! timestamp.
//!
//! Retrieval runs through a bounded adapter session: exact queries and
//! options are recorded, calls/time/bytes/retries are capped, and every
//! outcome is explicit — success, empty-success, partial, unavailable,
//! malformed. Errors are never persisted as empty-success entries.
//!
//! Artifacts are checksummed, written atomically, single-flighted per key,
//! and stored in an owner-only durable host directory outside every
//! worktree. The initial reuse guarantee is per host; export/import moves
//! bundles between hosts. Offline replay needs no provider credentials.
//!
//! # CLI contract
//!
//! | verb | purpose | exit |
//! |---|---|---|
//! | `fetch` | key → cache hit (byte-identical reuse) or bounded retrieval + persist | 0 served, 1 provider/argument failure, 2 could not answer |
//! | `status` | report an artifact's presence/integrity without replaying | 0 known, 1 absent/corrupt |
//! | `export` / `import` | move checksummed bundles between hosts | 0 ok, 1 failure |
//! | `replay` | print saved responses from the store (no provider) | 0 ok, 1 absent/corrupt |

pub mod adapter;
pub mod export;
pub mod flight;
pub mod key;
pub mod session;
pub mod store;

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

/// A complete, persisted retrieval artifact (#9783 step 3): raw responses and
/// normalized retrieval locations, checksummed, replayable offline.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct ContextArtifact {
    /// Schema version. Only 1 is understood.
    pub schema_version: u32,
    /// The content key this artifact is stored under (hex SHA-256).
    pub key: String,
    /// The exact input snapshot the key was derived from (provenance).
    pub input: key::InputSnapshot,
    /// The bounded retrieval session outcome.
    pub session: session::SessionRecord,
    /// RFC3339 completion time.
    pub completed_at: String,
}

impl ContextArtifact {
    pub const SCHEMA_VERSION: u32 = 1;

    /// The artifact's integrity checksum: SHA-256 over the canonical JSON of
    /// everything except the checksum itself (the session's stored checksum
    /// is zeroed before hashing).
    pub fn compute_checksum(&self) -> String {
        use sha2::{Digest, Sha256};
        let mut session = self.session.clone();
        session.checksum = String::new();
        let bare = ContextArtifactChecksum {
            schema_version: self.schema_version,
            key: &self.key,
            input: &self.input,
            session: &session,
            completed_at: &self.completed_at,
        };
        let canonical = serde_json::to_vec(&bare).unwrap_or_default();
        let mut h = Sha256::new();
        h.update(&canonical);
        hex::encode(h.finalize())
    }
}

/// Checksum projection (excludes the stored checksum itself).
#[derive(serde::Serialize)]
struct ContextArtifactChecksum<'a> {
    schema_version: u32,
    key: &'a str,
    input: &'a key::InputSnapshot,
    session: &'a session::SessionRecord,
    completed_at: &'a str,
}

/// The default durable store root: owner-only, outside every worktree,
/// shared by this host's worktrees (#9783 step 3).
pub fn default_store_root() -> Result<PathBuf> {
    let base = dirs::home_dir().ok_or_else(|| anyhow::anyhow!("cannot resolve home directory"))?;
    let root = base.join(".loom").join("context-cache");
    std::fs::create_dir_all(&root)
        .with_context(|| format!("creating store root {}", root.display()))?;
    // Owner-only: the artifact may embed proprietary source snippets.
    restrict_owner_only(&root)?;
    Ok(root)
}

fn restrict_owner_only(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let meta = std::fs::metadata(path)?;
        let mut perms = meta.permissions();
        if perms.mode() & 0o077 != 0 {
            perms.set_mode(0o700);
            std::fs::set_permissions(path, perms)?;
        }
    }
    Ok(())
}

/// Fetch one context key end-to-end: single-flight → cache hit (byte-stable
/// reuse) → bounded session → persist. Returns (key, artifact, reused).
///
/// The key is computed against the adapter that *actually runs*: `fetch`
/// overwrites `input.adapter_version` from `adapter.identity().version`
/// before hashing, so a provider behavior/schema change re-keys the cache
/// even if a caller supplied a stale or generic value (#9848 review finding).
pub fn fetch(
    mut input: key::InputSnapshot,
    store: &store::ArtifactStore,
    adapter: &dyn adapter::RetrievalAdapter,
    budget: adapter::Budget,
    provenance: session::ProvenanceSource<'_>,
) -> Result<(String, ContextArtifact, bool)> {
    input.adapter_version = adapter.identity().version;
    let key = input.content_key();
    // Single-flight: concurrent same-key requests coalesce on one lock.
    let _guard = flight::SingleFlight::acquire(store, &key)?;
    if let Some(existing) = store.load(&key)? {
        // Recheck the generation before publishing current state: the
        // caller's input must still hash to the key we loaded under.
        if existing.input.content_key() == key {
            return Ok((key, existing, true));
        }
        // Key collision under the same prefix is corruption — never serve.
        anyhow::bail!("cached artifact {} does not match its key (corruption)", key);
    }
    let session = session::run(&input, adapter, budget, provenance);
    let artifact = ContextArtifact {
        schema_version: ContextArtifact::SCHEMA_VERSION,
        key: key.clone(),
        input,
        session,
        completed_at: chrono::Utc::now().to_rfc3339(),
    };
    store.persist(&artifact)?;
    Ok((key, artifact, false))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot(title: &str) -> key::InputSnapshot {
        key::InputSnapshot {
            schema_version: 1,
            repo: "o/r".into(),
            issue: 1,
            title: title.into(),
            body: "body".into(),
            requirement_comments: Vec::new(),
            source_revision: "0123456789abcdef0123456789abcdef01234567".into(),
            index_identity: "idx-1".into(),
            query_policy_version: "qp-v1".into(),
            adapter_version: "fake-1".into(),
        }
    }

    #[test]
    fn fetch_reuses_byte_identically_and_keys_on_content() {
        let dir = tempfile::tempdir().unwrap();
        let store = store::ArtifactStore::at(dir.path().join("store"));
        let adapter = adapter::FakeAdapter::seeded(vec![(
            "q1".into(),
            adapter::RawSnippet {
                path: "src/a.rs".into(),
                ranges: vec![(1, 3)],
                text: "fn a() {}".into(),
                source_ref: "idx:1".into(),
            },
        )]);
        let (k1, a1, reused1) = fetch(
            snapshot("t"),
            &store,
            &adapter,
            adapter::Budget::default(),
            session::ProvenanceSource::Unavailable("test".into()),
        )
        .unwrap();
        let (k2, a2, reused2) = fetch(
            snapshot("t"),
            &store,
            &adapter,
            adapter::Budget::default(),
            session::ProvenanceSource::Unavailable("test".into()),
        )
        .unwrap();
        assert_eq!(k1, k2);
        assert!(!reused1);
        assert!(reused2);
        // Replay is byte-identical: the second fetch returns the stored
        // artifact, whose only difference is the filled-in integrity
        // checksum that persist computed from the first.
        let mut expected = a1.clone();
        expected.session.checksum = a1.compute_checksum();
        assert_eq!(a2, expected);
        // Zero provider calls served the reuse.
        assert_eq!(adapter.call_count(), 4); // one 4-query session, once
                                             // The recorded provider identity carries the adapter version that
                                             // actually ran.
        assert_eq!(a1.session.provider.version, "fake-1");
        // Content change → new key.
        let (k3, _, _) = fetch(
            snapshot("t2"),
            &store,
            &adapter,
            adapter::Budget::default(),
            session::ProvenanceSource::Unavailable("test".into()),
        )
        .unwrap();
        assert_ne!(k1, k3);
    }

    /// Integration guard for the #9848 review finding: changing the
    /// adapter's reported identity version must re-key the cache even when
    /// every other input is identical, so a provider behavior change can
    /// never keep serving a stale pre-change artifact. Uses the
    /// `LOOM_FAKE_ADAPTER_VERSION` seam — the same identity path the CLI's
    /// `run_fetch` threads into the key via `adapter.identity().version`.
    #[test]
    fn adapter_version_change_rekeys_cache() {
        let dir = tempfile::tempdir().unwrap();
        let store = store::ArtifactStore::at(dir.path().join("store"));
        let make = |version: &str| {
            adapter::FakeAdapter::seeded(vec![(
                "q1".into(),
                adapter::RawSnippet {
                    path: "src/a.rs".into(),
                    ranges: vec![(1, 3)],
                    text: "fn a() {}".into(),
                    source_ref: "idx:1".into(),
                },
            )])
            .with_version(version)
        };
        let (k1, _, reused1) = fetch(
            snapshot("t"),
            &store,
            &make("fake-1"),
            adapter::Budget::default(),
            session::ProvenanceSource::Unavailable("test".into()),
        )
        .unwrap();
        assert!(!reused1);
        // The snapshot's stale/generic adapter_version is irrelevant: fetch
        // derives the key from the adapter identity that actually ran.
        let mut stale_input = snapshot("t");
        stale_input.adapter_version = "fake".into();
        let (k2, a2, reused2) = fetch(
            stale_input,
            &store,
            &make("fake-2"),
            adapter::Budget::default(),
            session::ProvenanceSource::Unavailable("test".into()),
        )
        .unwrap();
        assert_ne!(k1, k2, "adapter schema version is a key dimension");
        assert!(!reused2);
        // The new artifact records the new identity, and the old artifact is
        // still independently loadable (history, not overwrite).
        assert_eq!(a2.session.provider.version, "fake-2");
        assert_eq!(store.load(&k1).unwrap().unwrap().session.provider.version, "fake-1");
    }
}
