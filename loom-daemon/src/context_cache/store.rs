//! The durable artifact store (#9783 steps 3 & 5): checksummed immutable
//! artifacts under an owner-only host directory outside every worktree,
//! atomic writes, corruption detection, orphan-tmp recovery and bounded
//! retention.
//!
//! Scope note (documented, deliberate): the initial reuse guarantee is **per
//! host** — there is no fleet-wide exactly-once lease here. `export` /
//! `import` moves bundles between hosts; fleet-wide reuse is deferred until
//! measured duplicate retrieval justifies a lease service (#9783 "Storage
//! decision").

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

use super::ContextArtifact;

pub const SCHEMA_VERSION: u32 = 1;
/// Artifacts retained per store before oldest-completed eviction.
const RETENTION_MAX: usize = 512;
/// Stale temporary files older than this are orphans of crashed writers.
const ORPHAN_TMP_MAX_AGE_SECS: u64 = 3600;

#[derive(Debug, Clone)]
pub struct ArtifactStore {
    root: PathBuf,
}

impl ArtifactStore {
    /// A store rooted at `root` (created owner-only on first use).
    pub fn at(root: PathBuf) -> Self {
        Self { root }
    }

    /// The store's root directory (single-flight locks live beside the
    /// sharded artifacts).
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The default host store: `~/.loom/context-cache`.
    pub fn default_open() -> Result<Self> {
        Ok(Self::at(super::default_store_root()?))
    }

    /// Sharded path: `<root>/<key[0..2]>/<key>.json`.
    fn artifact_path(&self, key: &str) -> PathBuf {
        self.root.join(&key[..2]).join(format!("{key}.json"))
    }

    fn tmp_path(&self, key: &str) -> PathBuf {
        self.root
            .join(&key[..2])
            .join(format!("{key}.tmp.{}", std::process::id()))
    }

    /// Load an artifact by key, verifying its checksum. Corruption is an
    /// explicit `Err`, never a silent miss.
    pub fn load(&self, key: &str) -> Result<Option<ContextArtifact>> {
        let path = self.artifact_path(key);
        if !path.exists() {
            return Ok(None);
        }
        let raw =
            std::fs::read(&path).with_context(|| format!("reading artifact {}", path.display()))?;
        let artifact: ContextArtifact = serde_json::from_slice(&raw)
            .with_context(|| format!("parsing artifact {}", path.display()))?;
        let expected = artifact.compute_checksum();
        let stored = artifact.session.checksum.clone();
        if stored != expected {
            anyhow::bail!(
                "artifact {} failed checksum verification (stored {stored}, computed {expected})",
                path.display()
            );
        }
        if artifact.schema_version != SCHEMA_VERSION {
            anyhow::bail!(
                "artifact {} has unsupported schema version {} (want {SCHEMA_VERSION})",
                path.display(),
                artifact.schema_version
            );
        }
        Ok(Some(artifact))
    }

    /// Persist atomically: write `*.tmp.<pid>`, fsync, rename onto the final
    /// path. A crashed writer leaves only an orphan tmp, never a partial
    /// artifact.
    pub fn persist(&self, artifact: &ContextArtifact) -> Result<()> {
        if artifact.key.len() < 2 {
            anyhow::bail!("artifact key too short to shard");
        }
        let dir = self.root.join(&artifact.key[..2]);
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("creating shard dir {}", dir.display()))?;
        super::restrict_owner_only(&dir)?;
        let mut writable = artifact.clone();
        writable.session.checksum = writable.compute_checksum();
        let payload = serde_json::to_vec_pretty(&writable)?;
        let tmp = self.tmp_path(&artifact.key);
        {
            use std::io::Write;
            let mut f = std::fs::File::create(&tmp)
                .with_context(|| format!("creating {}", tmp.display()))?;
            f.write_all(&payload)?;
            f.sync_all()?;
        }
        std::fs::rename(&tmp, self.artifact_path(&artifact.key))
            .with_context(|| format!("publishing artifact for key {}", artifact.key))?;
        self.enforce_retention()?;
        Ok(())
    }

    /// Recover crashed writers: delete orphan `*.tmp.*` files older than the
    /// orphan age bound. Safe to run at any time.
    pub fn recover_orphans(&self) -> Result<usize> {
        let mut removed = 0;
        if !self.root.exists() {
            return Ok(0);
        }
        for shard in std::fs::read_dir(&self.root)?.filter_map(|e| e.ok()) {
            let shard_path = shard.path();
            if !shard_path.is_dir() {
                continue;
            }
            for entry in std::fs::read_dir(&shard_path)?.filter_map(|e| e.ok()) {
                let p = entry.path();
                let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
                if !name.contains(".tmp.") {
                    continue;
                }
                let age = entry
                    .metadata()
                    .and_then(|m| m.modified())
                    .ok()
                    .and_then(|m| m.elapsed().ok())
                    .map(|e| e.as_secs())
                    .unwrap_or(0);
                if age > ORPHAN_TMP_MAX_AGE_SECS {
                    std::fs::remove_file(&p).ok();
                    removed += 1;
                }
            }
        }
        Ok(removed)
    }

    /// Bounded retention: keep at most [`RETENTION_MAX`] artifacts, evicting
    /// oldest-completed first. History beyond the bound is the export
    /// bundle's job, not the live store's.
    fn enforce_retention(&self) -> Result<()> {
        let mut artifacts: Vec<(std::time::SystemTime, PathBuf)> = Vec::new();
        for shard in std::fs::read_dir(&self.root)?.filter_map(|e| e.ok()) {
            let shard_path = shard.path();
            if !shard_path.is_dir() {
                continue;
            }
            for entry in std::fs::read_dir(&shard_path)?.filter_map(|e| e.ok()) {
                let p = entry.path();
                if p.extension().is_some_and(|x| x == "json") {
                    let modified = entry
                        .metadata()
                        .and_then(|m| m.modified())
                        .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
                    artifacts.push((modified, p));
                }
            }
        }
        if artifacts.len() <= RETENTION_MAX {
            return Ok(());
        }
        artifacts.sort_by_key(|(t, _)| *t);
        let excess = artifacts.len() - RETENTION_MAX;
        for (_, p) in artifacts.into_iter().take(excess) {
            std::fs::remove_file(&p).ok();
        }
        Ok(())
    }

    /// All keys currently in the store (sorted).
    pub fn keys(&self) -> Result<Vec<String>> {
        let mut keys = Vec::new();
        if !self.root.exists() {
            return Ok(keys);
        }
        for shard in std::fs::read_dir(&self.root)?.filter_map(|e| e.ok()) {
            let shard_path = shard.path();
            if !shard_path.is_dir() {
                continue;
            }
            for entry in std::fs::read_dir(&shard_path)?.filter_map(|e| e.ok()) {
                let p = entry.path();
                if p.extension().is_some_and(|x| x == "json") {
                    if let Some(stem) = p.file_stem().and_then(|s| s.to_str()) {
                        keys.push(stem.to_string());
                    }
                }
            }
        }
        keys.sort();
        Ok(keys)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context_cache::{adapter, key, session};

    fn artifact(key: &str) -> ContextArtifact {
        ContextArtifact {
            schema_version: SCHEMA_VERSION,
            key: key.into(),
            input: key::InputSnapshot {
                schema_version: 1,
                repo: "o/r".into(),
                issue: 1,
                title: "t".into(),
                body: "b".into(),
                requirement_comments: vec![],
                source_revision: "s".into(),
                index_identity: "i".into(),
                query_policy_version: "q".into(),
                adapter_version: "a".into(),
            },
            session: session::SessionRecord {
                status: session::SessionStatus::Success,
                queries: vec![],
                provider: adapter::ProviderIdentity {
                    name: "fake".into(),
                    version: "1".into(),
                },
                results: vec![],
                raw_responses: vec![],
                budget_report: session::BudgetReport {
                    calls: 0,
                    bytes: 0,
                    retries: 0,
                    elapsed_ms: 0,
                },
                coverage_notes: vec![],
                checksum: String::new(),
            },
            completed_at: "2026-01-01T00:00:00Z".into(),
        }
    }

    #[test]
    fn persist_load_roundtrip_and_checksum_binding() {
        let dir = tempfile::tempdir().unwrap();
        let store = ArtifactStore::at(dir.path().join("s"));
        let a = artifact("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
        store.persist(&a).unwrap();
        let loaded = store.load(&a.key).unwrap().expect("present");
        assert_eq!(loaded, {
            let mut expected = a.clone();
            expected.session.checksum = a.compute_checksum();
            expected
        });
        // Corruption is an explicit error, not a miss.
        let path = store.artifact_path(&a.key);
        let raw = std::fs::read_to_string(&path).unwrap();
        std::fs::write(&path, raw.replace("fake", "tampered")).unwrap();
        assert!(store.load(&a.key).is_err(), "tampered artifact must fail verification");
    }

    #[test]
    fn missing_key_is_a_clean_miss() {
        let dir = tempfile::tempdir().unwrap();
        let store = ArtifactStore::at(dir.path().join("s"));
        assert!(store
            .load("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb")
            .unwrap()
            .is_none());
    }
}
