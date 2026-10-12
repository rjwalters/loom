//! The durable local journal for operator-session facts (#11563).
//!
//! Operator workstations (volt, ...) run no daemon, so their facts have no
//! exporter. Until the transport of #11525 exists, every fact is appended here
//! as one [`TelemetryEnvelope`] JSON line, and a later [`export`] hands them
//! all to a [`Transport`].
//!
//! **Never drops silently** (#11527):
//! - There is no rotation: a bounded journal that rotated would discard facts
//!   a transport never saw.
//! - [`export`] stages the journal by rename, so a fact appended mid-export
//!   lands in a fresh file and is neither lost nor sent twice. Staged files are
//!   deleted only after the transport accepted every record in them; a failed
//!   send leaves them in place for the next export.
//! - A line that no longer parses is moved to `<journal>.rejected` and counted
//!   in [`ExportReport::rejected`], not deleted.

use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::telemetry::TelemetryEnvelope;

/// Environment override for the journal path (test seam and operator knob),
/// like `sweep_outcomes::OUTCOMES_JOURNAL_PATH_ENV`.
pub const PRESENCE_JOURNAL_PATH_ENV: &str = "LOOM_OPERATOR_PRESENCE_JOURNAL_PATH";

/// Default filename under `<workspace_root>/.loom/logs/`.
pub const PRESENCE_JOURNAL_FILENAME: &str = "operator-presence.jsonl";

/// Where the journal for `workspace_root` lives: the env override when set and
/// non-empty, else `<workspace_root>/.loom/logs/operator-presence.jsonl`.
#[must_use]
pub fn default_journal_path(workspace_root: &Path) -> PathBuf {
    match std::env::var(PRESENCE_JOURNAL_PATH_ENV) {
        Ok(p) if !p.trim().is_empty() => PathBuf::from(p),
        _ => workspace_root
            .join(".loom/logs")
            .join(PRESENCE_JOURNAL_FILENAME),
    }
}

/// Append `envelope` as one JSON line, creating parent directories. One
/// `write_all` of the whole line, so concurrent hook processes interleave
/// whole lines.
///
/// # Errors
/// When the directory, file or serialization fails. Callers on a hook path
/// report it on stderr; a failure here is never silent.
pub fn append(path: &Path, envelope: &TelemetryEnvelope) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating presence journal dir {}", parent.display()))?;
    }
    let mut line = serde_json::to_string(envelope).context("serializing telemetry envelope")?;
    line.push('\n');
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("opening presence journal {}", path.display()))?;
    file.write_all(line.as_bytes())
        .with_context(|| format!("appending to presence journal {}", path.display()))
}

/// Where a staged (renamed-for-export) copy of `path` goes. `nanos` keeps two
/// exports from colliding.
fn staged_name(path: &Path, nanos: u128) -> PathBuf {
    let name = path
        .file_name()
        .map_or_else(String::new, |n| n.to_string_lossy().into_owned());
    path.with_file_name(format!("{name}.exporting.{nanos}"))
}

/// Every staged file left beside `path` by an export that did not finish.
fn staged_files(path: &Path) -> Vec<PathBuf> {
    let (Some(dir), Some(name)) = (path.parent(), path.file_name()) else {
        return Vec::new();
    };
    let prefix = format!("{}.exporting.", name.to_string_lossy());
    let mut found: Vec<PathBuf> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .filter_map(std::result::Result::ok)
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .is_some_and(|n| n.to_string_lossy().starts_with(&prefix))
        })
        .collect();
    found.sort();
    found
}

fn non_empty_lines(path: &Path) -> Vec<String> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(str::to_string)
        .collect()
}

/// Every parseable envelope pending export (staged files first, then the live
/// journal), in order. Malformed lines are skipped here and counted by
/// [`export`].
#[must_use]
pub fn read_pending(path: &Path) -> Vec<TelemetryEnvelope> {
    let mut files = staged_files(path);
    files.push(path.to_path_buf());
    files
        .iter()
        .flat_map(|f| non_empty_lines(f))
        .filter_map(|l| serde_json::from_str(&l).ok())
        .collect()
}

/// Number of facts waiting to be exported (parseable lines only).
#[must_use]
pub fn pending_count(path: &Path) -> usize {
    read_pending(path).len()
}

/// Where records go once a transport exists (#11525). Implemented by the
/// real exporter; tests use a fake.
pub trait Transport {
    /// Send the whole batch. `Ok` means the backend accepted every record;
    /// `Err` leaves the journal untouched.
    ///
    /// # Errors
    /// Any delivery failure.
    fn send(&mut self, batch: &[TelemetryEnvelope]) -> Result<()>;
}

/// The transport used while none is configured: always declines, so nothing is
/// ever deleted.
pub struct NoTransport;

impl Transport for NoTransport {
    fn send(&mut self, _batch: &[TelemetryEnvelope]) -> Result<()> {
        anyhow::bail!("no transport configured (waiting on #11525); facts stay in the journal")
    }
}

/// What an [`export`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ExportReport {
    /// Records the transport accepted (and the journal then released).
    pub exported: usize,
    /// Malformed lines moved to `<journal>.rejected`.
    pub rejected: usize,
}

/// Send every pending fact through `transport`, then release them.
///
/// # Errors
/// The transport's error, or an I/O error staging the journal. On any error
/// every record is still in the journal (staged files are retried by the next
/// call).
pub fn export(path: &Path, transport: &mut dyn Transport) -> Result<ExportReport> {
    if path.exists() {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        std::fs::rename(path, staged_name(path, nanos))
            .with_context(|| format!("staging presence journal {}", path.display()))?;
    }
    let staged = staged_files(path);
    let mut batch = Vec::new();
    let mut rejected_lines = Vec::new();
    for file in &staged {
        for line in non_empty_lines(file) {
            match serde_json::from_str::<TelemetryEnvelope>(&line) {
                Ok(envelope) => batch.push(envelope),
                Err(_) => rejected_lines.push(line),
            }
        }
    }
    if !batch.is_empty() {
        transport.send(&batch)?;
    }
    if !rejected_lines.is_empty() {
        let mut rejected_path = path.as_os_str().to_owned();
        rejected_path.push(".rejected");
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(PathBuf::from(rejected_path))
            .context("opening presence rejected file")?;
        for line in &rejected_lines {
            writeln!(file, "{line}").context("writing presence rejected file")?;
        }
    }
    for file in &staged {
        std::fs::remove_file(file)
            .with_context(|| format!("releasing exported {}", file.display()))?;
    }
    Ok(ExportReport {
        exported: batch.len(),
        rejected: rejected_lines.len(),
    })
}
