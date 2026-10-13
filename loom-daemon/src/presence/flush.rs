//! Daemon-less export of the presence journal (#11713).
//!
//! An operator host runs no `loom-daemon`, so nothing drains
//! `.loom/logs/operator-presence.jsonl`. [`flush`] is the one-shot drain: it
//! resolves an OTLP target, POSTs every journaled fact with the same
//! [`OtlpExporter`] the daemon uses, and returns. The `Stop` hook reaches it
//! through `loom-daemon presence beat` (or `presence flush` directly).
//!
//! The journal is only released after the collector acknowledged every
//! envelope ([`journal::export`]), so a failed or partial send keeps the facts
//! for the next flush.

use std::path::Path;

use anyhow::{anyhow, Result};

use super::journal::{self, ExportReport, Transport};
use crate::observability::exporter::Exporter;
use crate::observability::otlp::OtlpExporter;
use crate::observability::{self, endpoint_policy};
use crate::telemetry::TelemetryEnvelope;

/// Override for the OTLP/HTTP base URL (no signal suffix), consulted before the
/// workspace's `observability` config.
pub const ENDPOINT_ENV: &str = "LOOM_PRESENCE_OTLP_ENDPOINT";
/// Override for the extra-request-headers file paired with [`ENDPOINT_ENV`].
pub const HEADERS_FILE_ENV: &str = "LOOM_PRESENCE_OTLP_HEADERS_FILE";

/// Where a flush POSTs to. Holds a credential, so deliberately not `Debug`.
pub struct Target {
    pub endpoint: String,
    pub headers_file: Option<String>,
    pub key: Option<String>,
}

fn env_nonempty(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.trim().is_empty())
}

/// Resolve the OTLP target for `root`: [`ENDPOINT_ENV`] first, else the first
/// planned OTLP exporter of the workspace's resolved `observability` config.
/// The ingest key (`observability.ingestKeyFile`, env, or the conventional
/// default) is read only when the entry has no `headers_file` of its own.
///
/// # Errors
/// When no endpoint is configured, the endpoint is a placeholder, or a needed
/// key file cannot be read. The message never contains the key.
pub fn resolve_target(root: &Path) -> Result<Target> {
    let config = observability::read_config(root);
    let (endpoint, headers_file) = match env_nonempty(ENDPOINT_ENV) {
        Some(endpoint) => (endpoint, env_nonempty(HEADERS_FILE_ENV)),
        None => observability::planned_otlp_exporters(&config)
            .into_iter()
            .next()
            .ok_or_else(|| {
                anyhow!(
                    "no OTLP export target: set ${ENDPOINT_ENV} or an enabled otlp entry in \
                     observability.exporters"
                )
            })?,
    };
    if let Some(host) = endpoint_policy::reserved_placeholder_host(&endpoint) {
        anyhow::bail!("OTLP endpoint points at the reserved placeholder domain {host}");
    }
    let key = if headers_file.is_some() {
        None
    } else {
        let path = observability::resolve_ingest_key_file(&config)
            .ok_or_else(|| anyhow!("no ingest key file configured"))?;
        let key = std::fs::read_to_string(&path)
            .map_err(|e| anyhow!("could not read ingest key file {path}: {e}"))?;
        let key = key.trim().to_string();
        anyhow::ensure!(!key.is_empty(), "ingest key file {path} is empty");
        Some(key)
    };
    Ok(Target {
        endpoint,
        headers_file,
        key,
    })
}

/// [`Transport`] over any [`Exporter`], driven from sync code on a private
/// current-thread runtime (on its own thread, so it also works when the caller
/// is already inside a runtime). Requires full acknowledgment of the batch.
pub struct OtlpTransport<E: Exporter> {
    exporter: E,
}

impl<E: Exporter> OtlpTransport<E> {
    pub fn new(exporter: E) -> Self {
        Self { exporter }
    }
}

impl<E: Exporter> Transport for OtlpTransport<E> {
    fn send(&mut self, batch: &[TelemetryEnvelope]) -> Result<()> {
        let exporter = &self.exporter;
        let outcome = std::thread::scope(|s| {
            s.spawn(|| {
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .map(|rt| rt.block_on(exporter.emit_batch_outcome(batch)))
            })
            .join()
            .map_err(|_| anyhow!("presence flush worker panicked"))?
            .map_err(|e| anyhow!("starting presence flush runtime: {e}"))
        })?;
        if let Some(error) = outcome.error {
            return Err(anyhow!("OTLP export failed: {error}"));
        }
        anyhow::ensure!(
            outcome.acknowledged == batch.len(),
            "collector acknowledged {} of {} record(s)",
            outcome.acknowledged,
            batch.len()
        );
        Ok(())
    }
}

/// Drain the journal at `journal_path` to the target resolved for `root`.
/// A journal with nothing pending is a no-op that touches no config or network.
///
/// # Errors
/// Target resolution or delivery failures; the journal keeps every fact.
pub fn flush(root: &Path, journal_path: &Path) -> Result<ExportReport> {
    if journal::pending_count(journal_path) == 0 && !journal_path.exists() {
        return Ok(ExportReport::default());
    }
    let target = resolve_target(root)?;
    let exporter = OtlpExporter::with_headers_file(
        target.endpoint,
        target.key,
        target.headers_file.as_deref(),
    )
    .map_err(|e| anyhow!("building OTLP exporter: {e}"))?;
    journal::export(journal_path, &mut OtlpTransport::new(exporter))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::observability::exporter::{BatchOutcome, ExportError};

    struct Fake(Result<usize, String>);

    impl Exporter for Fake {
        async fn emit_batch(&self, _: &[TelemetryEnvelope]) -> Result<(), ExportError> {
            Ok(())
        }
        async fn emit_batch_outcome(&self, e: &[TelemetryEnvelope]) -> BatchOutcome {
            match &self.0 {
                Ok(n) => BatchOutcome::accepted(*n.min(&e.len())),
                Err(m) => BatchOutcome {
                    error: Some(ExportError::Transport(m.clone())),
                    ..BatchOutcome::default()
                },
            }
        }
    }

    fn journal_with_one(dir: &Path) -> std::path::PathBuf {
        let path = dir.join("operator-presence.jsonl");
        let ctx = super::super::beat::Ctx {
            journal: path.clone(),
            root: dir.to_path_buf(),
            session_id: "s".into(),
            host: "h".into(),
            operator: None,
            now: chrono::Utc::now(),
        };
        super::super::beat::beat(&ctx, "UserPromptSubmit").unwrap();
        assert_eq!(journal::pending_count(&path), 1);
        path
    }

    #[test]
    fn an_acknowledged_batch_empties_the_journal() {
        let dir = tempfile::tempdir().unwrap();
        let path = journal_with_one(dir.path());
        let r = journal::export(&path, &mut OtlpTransport::new(Fake(Ok(1)))).unwrap();
        assert_eq!(r.exported, 1);
        assert_eq!(journal::pending_count(&path), 0);
    }

    #[test]
    fn a_transport_error_or_partial_ack_keeps_the_journal() {
        let dir = tempfile::tempdir().unwrap();
        let path = journal_with_one(dir.path());
        assert!(journal::export(&path, &mut OtlpTransport::new(Fake(Err("down".into())))).is_err());
        assert!(journal::export(&path, &mut OtlpTransport::new(Fake(Ok(0)))).is_err());
        assert_eq!(journal::pending_count(&path), 1);
    }

    #[test]
    fn an_absent_journal_flushes_without_resolving_a_target() {
        let dir = tempfile::tempdir().unwrap();
        let r = flush(dir.path(), &dir.path().join("none.jsonl")).unwrap();
        assert_eq!(r, ExportReport::default());
    }

    /// End to end with no daemon: `flush` resolves the target from env, POSTs
    /// the journaled `operator.presence` log to a local collector stub, sees
    /// the 200, and releases the journal.
    #[test]
    #[serial_test::serial]
    fn flush_posts_to_the_resolved_collector_and_releases_the_journal() {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = std::thread::spawn(move || {
            let (mut conn, _) = listener.accept().unwrap();
            let mut got = Vec::new();
            let mut buf = [0u8; 8192];
            // Read until the headers and the Content-Length body are in.
            loop {
                let n = conn.read(&mut buf).unwrap();
                got.extend_from_slice(&buf[..n]);
                let text = String::from_utf8_lossy(&got).to_string();
                if let Some((head, body)) = text.split_once("\r\n\r\n") {
                    let len = head
                        .lines()
                        .find_map(|l| {
                            l.to_ascii_lowercase()
                                .strip_prefix("content-length:")
                                .map(|v| v.trim().parse::<usize>().unwrap())
                        })
                        .unwrap_or(0);
                    if body.len() >= len {
                        break;
                    }
                }
                if n == 0 {
                    break;
                }
            }
            conn.write_all(
                b"HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: 2\r\nconnection: close\r\n\r\n{}",
            )
            .unwrap();
            String::from_utf8_lossy(&got).to_string()
        });
        let dir = tempfile::tempdir().unwrap();
        let key = dir.path().join("ingest.key");
        std::fs::write(&key, "test-key\n").unwrap();
        std::env::set_var(ENDPOINT_ENV, format!("http://127.0.0.1:{port}"));
        std::env::set_var("LOOM_OBSERVABILITY_INGEST_KEY_FILE", &key);
        let path = journal_with_one(dir.path());
        let result = flush(dir.path(), &path);
        std::env::remove_var(ENDPOINT_ENV);
        std::env::remove_var("LOOM_OBSERVABILITY_INGEST_KEY_FILE");
        let report = result.unwrap();
        let request = server.join().unwrap();
        assert!(request.starts_with("POST /v1/logs"), "{request}");
        assert!(request.contains("operator.presence"));
        assert!(request.to_ascii_lowercase().contains("bearer test-key"));
        assert_eq!(report.exported, 1);
        assert_eq!(journal::pending_count(&path), 0);
    }
}
