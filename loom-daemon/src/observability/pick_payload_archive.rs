//! The `pick.decision` payload archive (#12974, parent #12973).
//!
//! The exported log record is a bounded summary
//! ([`PickDecisionRecord::summarize`]); the **full** record (every candidate
//! and draw, exactly what the chunk union was before) is written here, keyed by
//! `tick_id` (the summary's `payload_ref`), through a [`PickPayloadSink`]. The
//! only sink in this change is [`LocalDirSink`]:
//! `<dir>/<yyyy-mm-dd>/<tick_id>.json.zst`, bounded by age and total size the
//! way `activity/transcript_archive.rs` bounds its archives. An S3 sink is a
//! separate child.
//!
//! An archive failure never fails the tick: the summary still exports, the
//! failure is counted ([`failure_count`]) and logged.

use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};

use chrono::{DateTime, NaiveDate, Utc};

use crate::telemetry::kinds::pick_decision::PickDecisionRecord;

/// Directory override (tests, unusual hosts).
pub const DIR_ENV: &str = "LOOM_PICK_PAYLOAD_DIR";
/// Payloads older than this many days are pruned.
pub const DEFAULT_MAX_AGE_DAYS: i64 = 14;
/// The archive is pruned oldest-first past this many bytes.
pub const DEFAULT_MAX_BYTES: u64 = 512 * 1024 * 1024;
const ZSTD_LEVEL: i32 = 3;
/// Pruning walks the tree, so it runs at most this often.
const PRUNE_INTERVAL_SECS: i64 = 3600;

static FAILURES: AtomicU64 = AtomicU64::new(0);
static LAST_PRUNE: AtomicI64 = AtomicI64::new(0);

/// How many payload writes have failed since the daemon started.
#[must_use]
pub fn failure_count() -> u64 {
    FAILURES.load(Ordering::Relaxed)
}

/// Where an archived payload goes.
pub trait PickPayloadSink {
    /// Store `json` (the full record) under `payload_ref`, filed by `day`.
    ///
    /// # Errors
    /// Whatever the backing store reports.
    fn put(&self, payload_ref: &str, day: NaiveDate, json: &[u8]) -> std::io::Result<()>;
}

/// `<dir>/<yyyy-mm-dd>/<tick_id>.json.zst`, size- and age-bounded.
#[derive(Debug, Clone)]
pub struct LocalDirSink {
    pub dir: PathBuf,
    pub max_age_days: i64,
    pub max_bytes: u64,
}

impl LocalDirSink {
    /// `$LOOM_PICK_PAYLOAD_DIR`, else `~/.loom/pick-payloads`, with the default bounds.
    #[must_use]
    pub fn from_env() -> Self {
        let dir = std::env::var_os(DIR_ENV)
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                dirs::home_dir()
                    .unwrap_or_else(|| PathBuf::from("."))
                    .join(".loom")
                    .join("pick-payloads")
            });
        Self {
            dir,
            max_age_days: DEFAULT_MAX_AGE_DAYS,
            max_bytes: DEFAULT_MAX_BYTES,
        }
    }

    /// The file `payload_ref` is (or would be) stored in on `day`.
    #[must_use]
    pub fn path_for(&self, payload_ref: &str, day: NaiveDate) -> PathBuf {
        self.dir
            .join(day.format("%Y-%m-%d").to_string())
            .join(format!("{}.json.zst", file_stem(payload_ref)))
    }

    /// Delete day directories past `max_age_days` (relative to `today`), then
    /// the oldest payloads until the rest fit `max_bytes`.
    pub fn prune(&self, today: NaiveDate) {
        let Ok(days) = fs::read_dir(&self.dir) else {
            return;
        };
        let mut files: Vec<(NaiveDate, PathBuf, u64)> = Vec::new();
        for day in days.flatten() {
            let Some(date) = day
                .file_name()
                .to_str()
                .and_then(|n| NaiveDate::parse_from_str(n, "%Y-%m-%d").ok())
            else {
                continue; // not ours
            };
            if (today - date).num_days() > self.max_age_days {
                let _ = fs::remove_dir_all(day.path());
                continue;
            }
            for f in fs::read_dir(day.path()).into_iter().flatten().flatten() {
                let size = f.metadata().map_or(0, |m| m.len());
                files.push((date, f.path(), size));
            }
        }
        files.sort();
        let mut total: u64 = files.iter().map(|f| f.2).sum();
        for (_, path, size) in files {
            if total <= self.max_bytes {
                break;
            }
            if fs::remove_file(&path).is_ok() {
                total = total.saturating_sub(size);
            }
        }
    }
}

impl PickPayloadSink for LocalDirSink {
    fn put(&self, payload_ref: &str, day: NaiveDate, json: &[u8]) -> std::io::Result<()> {
        let path = self.path_for(payload_ref, day);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let compressed = zstd::stream::encode_all(json, ZSTD_LEVEL)?;
        // Write-then-rename, so a reader never sees a partial payload.
        let tmp = path.with_extension("zst.tmp");
        let mut f = fs::File::create(&tmp)?;
        f.write_all(&compressed)?;
        f.sync_all()?;
        fs::rename(&tmp, &path)?;
        let now = Utc::now().timestamp();
        if now - LAST_PRUNE.load(Ordering::Relaxed) >= PRUNE_INTERVAL_SECS {
            LAST_PRUNE.store(now, Ordering::Relaxed);
            self.prune(day);
        }
        Ok(())
    }
}

/// `tick_id` made safe as a file name (it is daemon-built, but never trust it
/// into a path).
fn file_stem(payload_ref: &str) -> String {
    let stem: String = payload_ref
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '_'
            }
        })
        .collect();
    match stem.as_str() {
        "" | "." | ".." => "_".to_string(),
        _ => stem,
    }
}

/// Archive the full `record` under its `tick_id`, filed by its end time.
/// Returns the `payload_ref` to put on the summary. A failure is counted and
/// logged, never propagated: the tick must not fail on it.
pub fn archive_record(sink: &dyn PickPayloadSink, record: &PickDecisionRecord) -> String {
    let payload_ref = record.tick_id.clone();
    let day = record.ended_at.date_naive();
    let result = serde_json::to_vec(record)
        .map_err(std::io::Error::other)
        .and_then(|json| sink.put(&payload_ref, day, &json));
    if let Err(e) = result {
        FAILURES.fetch_add(1, Ordering::Relaxed);
        log::warn!("pick.decision payload archive failed for {payload_ref}: {e}");
    }
    payload_ref
}

/// Read an archived payload back (the exact record [`archive_record`] wrote).
///
/// # Errors
/// The file is missing, not zstd, or not a `pick.decision` record.
pub fn read_payload(path: &Path) -> std::io::Result<PickDecisionRecord> {
    let mut json = Vec::new();
    zstd::stream::read::Decoder::new(fs::File::open(path)?)?.read_to_end(&mut json)?;
    serde_json::from_slice(&json).map_err(std::io::Error::other)
}

/// `read_payload` for `payload_ref` filed on `day` in `sink`'s directory.
///
/// # Errors
/// As [`read_payload`].
pub fn read_ref(
    sink: &LocalDirSink,
    payload_ref: &str,
    ended_at: DateTime<Utc>,
) -> std::io::Result<PickDecisionRecord> {
    read_payload(&sink.path_for(payload_ref, ended_at.date_naive()))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
#[path = "pick_payload_archive_tests.rs"]
mod tests;
