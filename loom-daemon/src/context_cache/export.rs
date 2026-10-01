//! Offline export/import/replay (#9783 acceptance: "Offline export/import
//! and replay return the saved responses without provider credentials;
//! checksums and schema compatibility are checked").
//!
//! A bundle is a single JSON document carrying every artifact plus a store
//! checksum; import verifies each artifact's integrity before writing and
//! refuses schema mismatches. Replay prints a saved artifact's raw responses
//! without touching any provider.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use super::store::ArtifactStore;
use super::ContextArtifact;

#[derive(Serialize, Deserialize)]
struct Bundle {
    bundle_version: u32,
    created_at: String,
    artifacts: Vec<ContextArtifact>,
    /// SHA-256 over the concatenation of each artifact's stored checksum —
    /// detects bundle-level tampering cheaply.
    bundle_checksum: String,
}

const BUNDLE_VERSION: u32 = 1;

fn bundle_checksum(artifacts: &[ContextArtifact]) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    for a in artifacts {
        h.update(a.session.checksum.as_bytes());
    }
    hex::encode(h.finalize())
}

/// Write `keys` (or the whole store when `keys` is empty) to `out` as a
/// checksummed bundle.
pub fn export(store: &ArtifactStore, keys: &[String], out: &std::path::Path) -> Result<usize> {
    let all = store.keys()?;
    let wanted: Vec<String> = if keys.is_empty() { all } else { keys.to_vec() };
    let mut artifacts = Vec::new();
    for k in &wanted {
        match store.load(k)? {
            Some(a) => artifacts.push(a),
            None => anyhow::bail!("cannot export absent key {k}"),
        }
    }
    let bundle = Bundle {
        bundle_version: BUNDLE_VERSION,
        created_at: chrono::Utc::now().to_rfc3339(),
        bundle_checksum: bundle_checksum(&artifacts),
        artifacts,
    };
    std::fs::write(out, serde_json::to_vec_pretty(&bundle)?)
        .with_context(|| format!("writing bundle {}", out.display()))?;
    Ok(bundle.artifacts.len())
}

/// Import a bundle: verify the bundle checksum and every artifact's
/// integrity + schema, then persist. Imported artifacts never overwrite a
/// *different* current artifact under the same key (checksum-identical
/// writes are idempotent no-ops).
pub fn import(store: &ArtifactStore, bundle_path: &std::path::Path) -> Result<usize> {
    let raw = std::fs::read(bundle_path)
        .with_context(|| format!("reading bundle {}", bundle_path.display()))?;
    let bundle: Bundle = serde_json::from_slice(&raw)
        .with_context(|| format!("parsing bundle {}", bundle_path.display()))?;
    if bundle.bundle_version != BUNDLE_VERSION {
        anyhow::bail!(
            "unsupported bundle version {} (want {BUNDLE_VERSION})",
            bundle.bundle_version
        );
    }
    if bundle.bundle_checksum != bundle_checksum(&bundle.artifacts) {
        anyhow::bail!("bundle checksum mismatch — bundle is tampered or truncated");
    }
    let mut imported = 0usize;
    for a in &bundle.artifacts {
        if a.schema_version != ContextArtifact::SCHEMA_VERSION {
            anyhow::bail!("artifact {} has unsupported schema {}", a.key, a.schema_version);
        }
        let expected = a.compute_checksum();
        if a.session.checksum != expected {
            anyhow::bail!("artifact {} failed integrity verification", a.key);
        }
        if let Some(existing) = store.load(&a.key)? {
            if existing.session.checksum == a.session.checksum {
                continue; // identical — idempotent import
            }
            // Same key means same input snapshot; a different body under the
            // same key is corruption in one of the two stores.
            anyhow::bail!(
                "imported artifact for key {} conflicts with the existing local artifact",
                a.key
            );
        }
        store.persist(a)?;
        imported += 1;
    }
    Ok(imported)
}

/// Replay: print the saved artifact (responses + normalized locations) as
/// JSON, no provider involved.
pub fn replay(store: &ArtifactStore, key: &str) -> Result<ContextArtifact> {
    store
        .load(key)?
        .ok_or_else(|| anyhow::anyhow!("no artifact for key {key}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context_cache::{adapter, key, session};

    fn artifact(key_str: &str) -> ContextArtifact {
        ContextArtifact {
            schema_version: 1,
            key: key_str.into(),
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
                raw_responses: vec!["payload".into()],
                budget_report: session::BudgetReport {
                    calls: 1,
                    bytes: 7,
                    retries: 0,
                    elapsed_ms: 1,
                },
                coverage_notes: vec![],
                checksum: String::new(),
            },
            completed_at: "2026-01-01T00:00:00Z".into(),
        }
    }

    fn store_at(tag: &str) -> (tempfile::TempDir, ArtifactStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = ArtifactStore::at(dir.path().join(tag));
        (dir, store)
    }

    const K: &str = "dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd";

    #[test]
    fn export_import_roundtrip_replays_offline() {
        let (_d1, src) = store_at("src");
        let a = artifact(K);
        src.persist(&a).unwrap();
        let bundle = std::env::temp_dir().join(format!("ctx-bundle-{}.json", std::process::id()));
        let n = export(&src, &[], &bundle).unwrap();
        assert_eq!(n, 1);

        let (_d2, dst) = store_at("dst");
        let n = import(&dst, &bundle).unwrap();
        assert_eq!(n, 1);
        // Import is idempotent for identical artifacts.
        assert_eq!(import(&dst, &bundle).unwrap(), 0);

        // Replay returns saved responses without any provider.
        let replayed = replay(&dst, K).unwrap();
        assert_eq!(replayed.session.raw_responses, vec!["payload".to_string()]);
        std::fs::remove_file(&bundle).ok();
    }

    #[test]
    fn tampered_bundle_is_refused() {
        let (_d1, src) = store_at("src2");
        src.persist(&artifact(K)).unwrap();
        let bundle =
            std::env::temp_dir().join(format!("ctx-bundle-bad-{}.json", std::process::id()));
        export(&src, &[], &bundle).unwrap();
        let mut raw = std::fs::read_to_string(&bundle).unwrap();
        raw = raw.replace("\"calls\": 1", "\"calls\": 2");
        std::fs::write(&bundle, raw).unwrap();
        let (_d2, dst) = store_at("dst2");
        assert!(import(&dst, &bundle).is_err(), "tampered bundle must be refused");
        std::fs::remove_file(&bundle).ok();
    }
}
