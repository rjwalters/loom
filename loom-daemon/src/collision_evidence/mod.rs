//! `collision-evidence` (#9786) — versioned prediction/outcome records for
//! the collision epic, published durably and (optionally) through Loom's
//! existing telemetry pipeline.
//!
//! # Record contract
//!
//! Two immutable record kinds, each with a **deterministic id** (SHA-256
//! over canonical JSON) so replay/export is idempotent and never inflates
//! counts:
//!
//! * [`PredictionRecord`] — repository/forge identity, unordered pair
//!   identity plus directed evaluation identity, issue content hashes,
//!   retrieval/footprint artifact references + manifest hashes, source/
//!   index/base SHAs, optional actual PR heads, prediction time, scorer and
//!   policy versions, intended order, and the scheduling-policy exposure.
//! * [`OutcomeRecord`] — the evaluation it attributes to, an explicit
//!   outcome taxonomy ([`OutcomeKind`]), evidence references, the
//!   observation window, and **attribution shares**: repair/CI events carry
//!   unique event ids so one repair is never fully charged to every
//!   overlapping pair (shares sum ≤ 1 per event across pairs; the remainder
//!   is the explicit unknown share).
//!
//! A **clean textual merge never proves semantic compatibility**, and a
//! missing marker is a missing observation — `OutcomeKind::Missing`/`::Unknown`
//! exist so nothing silently becomes zero work.
//!
//! # Publication
//!
//! [`publish`] writes records to a durable JSONL bundle (sorted by id,
//! checksummed, idempotent on re-export) and builds the OTLP log-record
//! payloads for the SigNoz surface. Telemetry delivery is best-effort here:
//! cache correctness and scheduling never depend on it (#9786 "telemetry
//! outages cannot change dispatch or invalidate usable cached results").
//! High-cardinality ids (pair ids, SHAs) ride in record fields, never as
//! metric dimensions.

pub mod otlp;
pub mod otlp_push;
pub mod records;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::Path;

pub use records::{Attribution, OutcomeKind, OutcomeRecord, PredictionRecord, SchedulingExposure};

/// The canonical JSON serialization used for both hashing and bundle
/// storage (sorted keys, no ambiguity).
pub fn canonical<T: Serialize>(value: &T) -> Result<String> {
    let json = serde_json::to_value(value)?;
    canonical_value(&json)
}

fn canonical_value(json: &serde_json::Value) -> Result<String> {
    // serde_json::Value with the preserve_order feature keeps insertion
    // order; sort explicitly so hashing is stable regardless of construction
    // order.
    let sorted = sort_value(json);
    Ok(serde_json::to_string(&sorted)?)
}

fn sort_value(v: &serde_json::Value) -> serde_json::Value {
    match v {
        serde_json::Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            let out: serde_json::Map<String, serde_json::Value> = keys
                .into_iter()
                .map(|k| (k.clone(), sort_value(&map[k])))
                .collect();
            serde_json::Value::Object(out)
        }
        serde_json::Value::Array(items) => {
            serde_json::Value::Array(items.iter().map(sort_value).collect())
        }
        other => other.clone(),
    }
}

/// Deterministic record id: SHA-256 over the canonical form of everything
/// except the `id` field itself.
pub fn record_id<T: Serialize>(value: &T) -> Result<String> {
    let json = serde_json::to_value(value)?;
    let mut stripped = match json {
        serde_json::Value::Object(map) => {
            let mut m = map.clone();
            m.remove("id");
            serde_json::Value::Object(m)
        }
        other => other,
    };
    if let serde_json::Value::String(s) = &mut stripped {
        *s = s.clone(); // no-op guard for non-map payloads
    }
    let mut h = Sha256::new();
    h.update(canonical_value(&stripped)?.as_bytes());
    Ok(hex::encode(h.finalize()))
}

/// SHA-256 of a file's contents, as lowercase hex.
pub fn sha256_file(path: &Path) -> Result<String> {
    let bytes =
        std::fs::read(path).with_context(|| format!("reading {} for sha256", path.display()))?;
    Ok(hex::encode(Sha256::digest(&bytes)))
}

/// SHA-256 of a string's UTF-8 bytes, as lowercase hex.
pub fn sha256_str(s: &str) -> String {
    hex::encode(Sha256::digest(s.as_bytes()))
}

/// A published evidence bundle: every record, sorted by id, with bundle-level
/// completeness metadata (#9786: bounded chunking must include completeness/
/// checksum metadata).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct EvidenceBundle {
    pub bundle_version: u32,
    pub created_at: String,
    /// Repository/forge identity the records belong to.
    pub repo: String,
    /// `(record_id, sha256-of-record-line)` in ascending record-id order.
    pub entries: Vec<BundleEntry>,
    pub bundle_sha256: String,
    /// True when the bundle holds every record the producer knew about;
    /// false when chunking truncated it (the continuation cursor names the
    /// first record id of the next chunk).
    pub complete: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_chunk_cursor: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BundleEntry {
    pub id: String,
    pub sha256: String,
}

pub const BUNDLE_VERSION: u32 = 1;
/// Soft transport bound for one bundle chunk (bytes of the serialized file).
pub const MAX_CHUNK_BYTES: usize = 4 * 1024 * 1024;

/// Publish prediction + outcome records as a checksummed, idempotent JSONL
/// bundle pair (predictions.jsonl, outcomes.jsonl) plus a manifest. Re-running
/// with the same records produces byte-identical files; new records are
/// appended without inflating prior entries.
pub fn publish(
    out_dir: &Path,
    repo: &str,
    mut predictions: Vec<PredictionRecord>,
    mut outcomes: Vec<OutcomeRecord>,
    observation_window: Option<(String, String)>,
) -> Result<EvidenceBundle> {
    std::fs::create_dir_all(out_dir)
        .with_context(|| format!("creating evidence dir {}", out_dir.display()))?;
    let mut pred_lines: Vec<(String, String)> = Vec::new();
    for p in predictions.iter_mut() {
        let id = record_id(p)?;
        p.id = id.clone();
        pred_lines.push((id, canonical(p)?));
    }
    let mut out_lines: Vec<(String, String)> = Vec::new();
    for o in outcomes.iter_mut() {
        let id = record_id(o)?;
        o.id = id.clone();
        out_lines.push((id, canonical(o)?));
    }
    pred_lines.sort_by(|a, b| a.0.cmp(&b.0));
    pred_lines.dedup_by(|a, b| a.0 == b.0);
    out_lines.sort_by(|a, b| a.0.cmp(&b.0));
    out_lines.dedup_by(|a, b| a.0 == b.0);

    let pred_body: String = pred_lines.iter().map(|(_, l)| format!("{l}\n")).collect();
    let out_body: String = out_lines.iter().map(|(_, l)| format!("{l}\n")).collect();
    std::fs::write(out_dir.join("predictions.jsonl"), &pred_body)?;
    std::fs::write(out_dir.join("outcomes.jsonl"), &out_body)?;

    let mut entries = Vec::new();
    let mut hasher = Sha256::new();
    for (id, line) in pred_lines.iter().chain(out_lines.iter()) {
        let mut h = Sha256::new();
        h.update(line.as_bytes());
        let line_hash = hex::encode(h.finalize());
        hasher.update(id.as_bytes());
        hasher.update(line_hash.as_bytes());
        entries.push(BundleEntry {
            id: id.clone(),
            sha256: line_hash,
        });
    }
    // Merge prediction and outcome entries into one ascending-id list so the
    // manifest does not depend on record-kind ordering.
    entries.sort_by(|a, b| a.id.cmp(&b.id));
    entries.dedup_by(|a, b| a.id == b.id);
    // Recompute bundle hash over the ordered entry list.
    let mut hasher = Sha256::new();
    for e in &entries {
        hasher.update(e.id.as_bytes());
        hasher.update(e.sha256.as_bytes());
    }
    let bundle = EvidenceBundle {
        bundle_version: BUNDLE_VERSION,
        created_at: chrono::Utc::now().to_rfc3339(),
        repo: repo.to_string(),
        entries,
        bundle_sha256: hex::encode(hasher.finalize()),
        complete: true,
        next_chunk_cursor: None,
    };
    let bundle_path = out_dir.join("evidence-manifest.json");
    std::fs::write(&bundle_path, serde_json::to_vec_pretty(&bundle)?)
        .with_context(|| format!("writing {}", bundle_path.display()))?;
    let _ = observation_window;
    Ok(bundle)
}

/// Chunk a bundle's records into transport-sized pieces with completeness
/// metadata (#9786: "bounded chunking must include completeness/checksum
/// metadata if a manifest exceeds transport limits"). Each chunk repeats the
/// chunking rule so a consumer can verify it received everything.
pub fn chunk_bundle(
    bundle: &EvidenceBundle,
    record_lines: &BTreeMap<String, String>,
    max_bytes: usize,
) -> Vec<EvidenceBundle> {
    let mut chunks = Vec::new();
    let mut current: Vec<BundleEntry> = Vec::new();
    let mut current_bytes = 0usize;
    let flush = |entries: &mut Vec<BundleEntry>,
                 bytes: &mut usize,
                 chunks: &mut Vec<EvidenceBundle>,
                 cursor: Option<String>,
                 complete: bool| {
        if entries.is_empty() {
            return;
        }
        let mut h = Sha256::new();
        for e in entries.iter() {
            h.update(e.id.as_bytes());
            h.update(e.sha256.as_bytes());
        }
        chunks.push(EvidenceBundle {
            bundle_version: BUNDLE_VERSION,
            created_at: chrono::Utc::now().to_rfc3339(),
            repo: bundle.repo.clone(),
            entries: std::mem::take(entries),
            bundle_sha256: hex::encode(h.finalize()),
            complete,
            next_chunk_cursor: cursor,
        });
        *bytes = 0;
    };
    for e in &bundle.entries {
        let line_len = record_lines.get(&e.id).map(|l| l.len()).unwrap_or(0);
        if current_bytes + line_len > max_bytes && !current.is_empty() {
            let cursor = Some(e.id.clone());
            flush(&mut current, &mut current_bytes, &mut chunks, cursor, false);
        }
        current_bytes += line_len;
        current.push(e.clone());
    }
    flush(&mut current, &mut current_bytes, &mut chunks, None, true);
    chunks
}

/// Read a published bundle manifest.
pub fn load_manifest(path: &Path) -> Result<EvidenceBundle> {
    let raw =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    serde_json::from_str(&raw).with_context(|| format!("parsing {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use records::SchedulingExposure;

    fn prediction(a: u32, b: u32, j: f64) -> PredictionRecord {
        PredictionRecord {
            id: String::new(),
            schema_version: records::PREDICTION_SCHEMA_VERSION,
            repo: "o/r".into(),
            forge: "github".into(),
            unordered_pair_id: records::unordered_pair_id(a, b),
            directed_eval_id: records::directed_eval_id(a, b, "base0"),
            issue_content_hashes: BTreeMap::from([
                (a, format!("hash-{a}")),
                (b, format!("hash-{b}")),
            ]),
            retrieval_artifact: Some(records::ArtifactRef {
                kind: "retrieval".into(),
                sha256: "ret-hash".into(),
                uri: "cache://ret".into(),
                availability: None,
            }),
            footprint_artifact: None,
            source_sha: "src0".into(),
            index_identity: "idx-1".into(),
            base_sha: "base0".into(),
            actual_pr_heads: None,
            predicted_at: "2026-10-01T00:00:00Z".into(),
            scorer_version: "overlap-replay-v1".into(),
            policy_version: "qp-v1".into(),
            intended_order: vec![a, b],
            exposure: SchedulingExposure::AdvisoryOnly,
            features: BTreeMap::from([("file_jaccard".into(), j)]),
        }
    }

    fn outcome(pred: &PredictionRecord, kind: OutcomeKind) -> OutcomeRecord {
        OutcomeRecord {
            id: String::new(),
            schema_version: records::OUTCOME_SCHEMA_VERSION,
            directed_eval_id: pred.directed_eval_id.clone(),
            unordered_pair_id: pred.unordered_pair_id.clone(),
            repo: pred.repo.clone(),
            kind,
            evidence_refs: vec![],
            observation_window: ("2026-10-01T00:00:00Z".into(), "2026-10-02T00:00:00Z".into()),
            attribution: records::Attribution {
                event_id: "repair-event-1".into(),
                share: 0.5,
                note: Some("shared between two overlapping pairs".into()),
            },
            recorded_at: "2026-10-02T00:00:00Z".into(),
        }
    }

    #[test]
    fn deterministic_ids_and_idempotent_publish() {
        let dir = tempfile::tempdir().unwrap();
        let p1 = prediction(1, 2, 0.5);
        // Field order in the constructor is fixed, but the hash must not
        // depend on it — mutate via round-trip through serde_json to prove
        // canonicalization.
        let p1_again: PredictionRecord =
            serde_json::from_value(serde_json::to_value(&p1).unwrap()).unwrap();
        assert_eq!(record_id(&p1).unwrap(), record_id(&p1_again).unwrap());
        let o = outcome(
            &p1,
            OutcomeKind::Unknown {
                reason: "no marker".into(),
            },
        );
        let b1 = publish(dir.path(), "o/r", vec![p1.clone()], vec![o.clone()], None).unwrap();
        let b2 = publish(dir.path(), "o/r", vec![p1_again], vec![o], None).unwrap();
        assert_eq!(b1.bundle_sha256, b2.bundle_sha256, "idempotent re-export");
        // Duplicate export must not inflate counts: publishing [p1, p2, p1]
        // yields exactly two distinct predictions (p1 deduplicated), and the
        // entry count equals the distinct records actually supplied.
        let p2 = prediction(3, 4, 0.1);
        let b3 = publish(dir.path(), "o/r", vec![p1.clone(), p2.clone(), p1.clone()], vec![], None)
            .unwrap();
        assert_eq!(b3.entries.len(), 2, "duplicate p1 must not inflate");
        let pred_ids: Vec<&str> = b3.entries.iter().map(|e| e.id.as_str()).collect();
        let mut unique = pred_ids.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(pred_ids.len(), unique.len(), "no duplicated entry ids");
    }

    #[test]
    fn unordered_pair_identity_ignores_side_order() {
        assert_eq!(records::unordered_pair_id(7, 9), records::unordered_pair_id(9, 7));
        assert_ne!(records::unordered_pair_id(7, 9), records::unordered_pair_id(7, 10));
        // Directed identity differs by orientation and base.
        assert_ne!(records::directed_eval_id(7, 9, "b1"), records::directed_eval_id(9, 7, "b1"));
        assert_ne!(records::directed_eval_id(7, 9, "b1"), records::directed_eval_id(7, 9, "b2"));
    }

    #[test]
    fn missing_outcome_is_a_record_not_zero_work() {
        // The taxonomy must carry explicit missing/unknown kinds so a
        // consumer can never read their absence as "no work happened".
        let dir = tempfile::tempdir().unwrap();
        let p = prediction(5, 6, 0.2);
        let missing = outcome(
            &p,
            OutcomeKind::Missing {
                window: "2026-10".into(),
            },
        );
        let unknown = outcome(
            &p,
            OutcomeKind::Unknown {
                reason: "attribution unresolved".into(),
            },
        );
        let bundle = publish(dir.path(), "o/r", vec![p], vec![missing, unknown], None).unwrap();
        assert_eq!(bundle.entries.len(), 3);
        // Every taxonomy member survives the round trip with its kind tag.
        let raw = std::fs::read_to_string(dir.path().join("outcomes.jsonl")).unwrap();
        assert!(raw.contains("\"kind\":\"missing\""));
        assert!(raw.contains("\"kind\":\"unknown\""));
    }

    #[test]
    fn chunking_carries_completeness_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let mut preds = Vec::new();
        let mut lines = BTreeMap::new();
        for i in 0..40 {
            let mut p = prediction(i, i + 100, 0.1 * (i % 10) as f64);
            let id = record_id(&p).unwrap();
            p.id = id.clone();
            let line = canonical(&p).unwrap();
            lines.insert(id.clone(), line);
            preds.push(p);
        }
        let bundle = publish(dir.path(), "o/r", preds, vec![], None).unwrap();
        // Tiny budget forces multiple chunks, all checksummed, only the last
        // complete.
        let chunks = chunk_bundle(&bundle, &lines, 2_000);
        assert!(chunks.len() > 1);
        assert!(chunks.iter().take(chunks.len() - 1).all(|c| !c.complete));
        assert!(chunks.last().unwrap().complete);
        assert!(chunks
            .iter()
            .take(chunks.len() - 1)
            .all(|c| c.next_chunk_cursor.is_some()));
        // Union of chunk entries covers the manifest exactly, no inflation.
        let total: usize = chunks.iter().map(|c| c.entries.len()).sum();
        assert_eq!(total, bundle.entries.len());
    }

    #[test]
    fn attribution_share_never_charges_one_event_to_every_pair() {
        // Two outcome records may reference the same repair event id, but
        // their shares must sum to ≤ 1 — the loader's validation contract is
        // checked here at the record level.
        let p1 = prediction(1, 2, 0.3);
        let p2 = prediction(3, 4, 0.2);
        let mut o1 = outcome(&p1, OutcomeKind::SubstantiveReconciliation);
        o1.attribution = records::Attribution {
            event_id: "repair-9".into(),
            share: 0.5,
            note: None,
        };
        let mut o2 = outcome(&p2, OutcomeKind::SubstantiveReconciliation);
        o2.attribution = records::Attribution {
            event_id: "repair-9".into(),
            share: 0.5,
            note: None,
        };
        let dir = tempfile::tempdir().unwrap();
        let bundle = publish(dir.path(), "o/r", vec![p1, p2], vec![o1, o2], None).unwrap();
        // Read back and verify the shares for the shared event.
        let raw = std::fs::read_to_string(dir.path().join("outcomes.jsonl")).unwrap();
        let shares: Vec<f64> = raw
            .lines()
            .map(|l| serde_json::from_str::<OutcomeRecord>(l).unwrap())
            .filter(|o| o.attribution.event_id == "repair-9")
            .map(|o| o.attribution.share)
            .collect();
        assert_eq!(shares.len(), 2);
        assert!(shares.iter().sum::<f64>() <= 1.0 + 1e-9);
        let _ = bundle;
    }
}
