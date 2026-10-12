//! Single completion producer for attended and daemon-mediated Curators.
//! Write once before label completion; the ordinary daemon backfill exports it.
use std::io::Write;
use std::path::Path;

use chrono::{DateTime, Utc};

use super::{parse, IssueEstimateRecord};
use crate::observability::queue::QueueSink;
use crate::telemetry::{TelemetryEnvelope, TelemetryRecord};

fn directory(root: &Path) -> std::path::PathBuf {
    root.join(".loom/logs/issue-estimates")
}

/// Actual curation provenance, supplied by the completing Curator, not a later tick.
pub struct Completion<'a> {
    pub repo: &'a str,
    pub issue: u32,
    pub model: &'a str,
    pub runtime: &'a str,
    pub id: &'a str,
    pub host_id: &'a str,
    pub at: DateTime<Utc>,
}

fn known(value: &str) -> Option<String> {
    (value != "unknown").then(|| value.to_string())
}

/// Resolve an already recorded completion without another forge read.
pub fn existing(root: &Path, request: &Completion<'_>) -> anyhow::Result<Option<TelemetryEnvelope>> {
    let id = uuid::Uuid::parse_str(request.id)?.to_string();
    let path = directory(root).join(format!("{id}.json"));
    if !path.exists() { return Ok(None) }
    let saved: TelemetryEnvelope = serde_json::from_reader(std::fs::File::open(path)?)?;
    let TelemetryRecord::IssueEstimate(ref r) = saved.record else {
        anyhow::bail!("completion journal has wrong kind");
    };
    anyhow::ensure!(r.repo == request.repo && r.issue == request.issue
        && r.model == known(request.model) && r.runtime == known(request.runtime),
        "completion id already belongs to another curation");
    Ok(Some(saved))
}

/// Persist one completion atomically. Retrying an id returns its original envelope.
/// An absent marker produces no event; callers may require it for new curation.
pub fn record(root: &Path, body: &str, request: &Completion<'_>)
    -> anyhow::Result<Option<TelemetryEnvelope>> {
    let id = uuid::Uuid::parse_str(request.id)?.to_string();
    anyhow::ensure!(request.issue > 0 && request.repo.split('/').count() == 2,
        "invalid issue identity");
    anyhow::ensure!(!request.model.trim().is_empty() && !request.runtime.trim().is_empty(),
        "use unknown explicitly for unavailable provenance");
    let dir = directory(root);
    let path = dir.join(format!("{id}.json"));
    if let Some(saved) = existing(root, request)? { return Ok(Some(saved)) }
    let Some(estimate) = parse(body)? else { return Ok(None) };
    let r = IssueEstimateRecord {
        repo: request.repo.to_string(), issue: request.issue, estimate,
        model: known(request.model), runtime: known(request.runtime),
        estimated_at: request.at, completion_id: id,
        loom: crate::telemetry::provenance::Provenance::current(),
    };
    let mut envelope = TelemetryEnvelope::new(request.host_id, TelemetryRecord::IssueEstimate(r));
    envelope.emitted_at = request.at;
    std::fs::create_dir_all(&dir)?;
    let mut temporary = tempfile::NamedTempFile::new_in(&dir)?;
    serde_json::to_writer(&mut temporary, &envelope)?;
    temporary.write_all(b"\n")?;
    temporary.as_file().sync_all()?;
    match temporary.persist_noclobber(&path) {
        Ok(_) => {
            std::fs::File::open(dir)?.sync_all()?;
            Ok(Some(envelope))
        }
        Err(e) if e.error.kind() == std::io::ErrorKind::AlreadyExists => record(root, body, request),
        Err(e) => Err(e.error.into()),
    }
}

/// Retry durable queue delivery with the SAME envelope. No historical issue reads.
pub fn backfill(root: &Path, queue: &dyn QueueSink) -> usize {
    let Ok(entries) = std::fs::read_dir(directory(root)) else { return 0 };
    let mut offered = 0;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json")
            || path.with_extension("offered").exists() { continue }
        let envelope = std::fs::File::open(&path).ok()
            .and_then(|f| serde_json::from_reader::<_, TelemetryEnvelope>(f).ok());
        let Some(envelope) = envelope else { continue };
        if !matches!(envelope.record, TelemetryRecord::IssueEstimate(_)) { continue }
        if queue.offer_durable(envelope).is_err() { continue }
        if std::fs::write(path.with_extension("offered"), b"").is_ok() { offered += 1 }
    }
    offered
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    const BODY: &str = "<!-- loom:estimate difficulty=2 lines=50 files=2 doctor_risk=0.1 -->";
    #[test]
    fn retry_and_recuration_have_distinct_semantics() {
        let dir = tempfile::tempdir().unwrap();
        let id = uuid::Uuid::new_v4().to_string();
        let mut r = Completion { repo: "o/r", issue: 1, model: "actual-model",
            runtime: "codex", id: &id, host_id: "volt", at: Utc::now() };
        let first = record(dir.path(), BODY, &r).unwrap().unwrap();
        r.at += chrono::Duration::seconds(1);
        assert_eq!(record(dir.path(), BODY, &r).unwrap().unwrap(), first);
        let second_id = uuid::Uuid::new_v4().to_string();
        r.id = &second_id;
        let second = record(dir.path(), BODY, &r).unwrap().unwrap();
        assert!(second.emitted_at > first.emitted_at);
        assert_eq!(std::fs::read_dir(directory(dir.path())).unwrap().count(), 2);
    }
    #[test]
    fn historical_absence_is_not_backfilled() {
        let dir = tempfile::tempdir().unwrap();
        let id = uuid::Uuid::new_v4().to_string();
        let r = Completion { repo: "o/r", issue: 1, model: "m", runtime: "claude",
            id: &id, host_id: "h", at: Utc::now() };
        assert_eq!(record(dir.path(), "old issue", &r).unwrap(), None);
        assert!(!directory(dir.path()).exists());
    }
}

#[cfg(test)]
#[path = "completion_tests.rs"]
mod delivery_tests;
