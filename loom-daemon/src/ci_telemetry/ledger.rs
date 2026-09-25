//! The durable dedup ledger — `.loom/state/ci-telemetry/seen.jsonl`.
//!
//! Append-only JSONL. Four line types:
//!
//! - `unit` — one committed run or job: its `(repo, run_id, job_id)` key, a
//!   monotonically increasing `seq`, and the exact envelopes it will emit.
//!   **Appending + fsyncing this line is the commit**: from then on the unit
//!   is "seen" and no re-poll will ever build it again.
//! - `seen` — the compacted form of a unit whose emission is confirmed (key
//!   only, no envelopes); written by [`Ledger::compact_if_large`].
//! - `watermark` — a repo's `created_at` polling watermark.
//! - `emitted` — "every unit with `seq <= through_seq` has reached the
//!   journal". Units above it are *pending*: the next cycle replays them,
//!   skipping any envelope the journal already holds.
//!
//! **Torn tail.** A crash mid-append can leave a trailing partial line. On
//! open it is detected (bytes after the final `\n`), the file is truncated
//! back to the last complete line, and the partial entry is never counted —
//! the unit it would have committed was therefore never emitted either (emit
//! strictly follows commit), and the next poll simply commits it again.

use std::collections::{BTreeMap, HashSet};
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::telemetry::TelemetryEnvelope;

/// A unit's dedup key: `(repo, run_id, job_id)` plus the run attempt.
/// `job_id` is `None` for the run-level unit. The attempt makes a re-run of
/// an already-recorded run a *new* run-level unit (its jobs already have
/// fresh GitHub `job_id`s), so each attempt is emitted exactly once.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct UnitKey {
    pub repo: String,
    pub run_id: u64,
    pub job_id: Option<u64>,
    pub attempt: u32,
}

impl UnitKey {
    #[must_use]
    pub fn run(repo: &str, run_id: u64, attempt: u32) -> Self {
        UnitKey {
            repo: repo.to_string(),
            run_id,
            job_id: None,
            attempt,
        }
    }

    #[must_use]
    pub fn job(repo: &str, run_id: u64, job_id: u64, attempt: u32) -> Self {
        UnitKey {
            repo: repo.to_string(),
            run_id,
            job_id: Some(job_id),
            attempt,
        }
    }
}

fn first_attempt() -> u32 {
    1
}

/// Compact the ledger once it exceeds this many bytes (and nothing is
/// pending) — confirmed units drop their envelope payloads.
pub const COMPACT_THRESHOLD_BYTES: u64 = 8 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum LedgerLine {
    Unit {
        seq: u64,
        repo: String,
        run_id: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        job_id: Option<u64>,
        #[serde(default = "first_attempt")]
        attempt: u32,
        envelopes: Vec<TelemetryEnvelope>,
    },
    Seen {
        seq: u64,
        repo: String,
        run_id: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        job_id: Option<u64>,
        #[serde(default = "first_attempt")]
        attempt: u32,
    },
    Watermark {
        repo: String,
        created_at: DateTime<Utc>,
    },
    Emitted {
        through_seq: u64,
    },
}

/// A unit to commit.
#[derive(Debug, Clone)]
pub struct UnitDraft {
    pub key: UnitKey,
    pub envelopes: Vec<TelemetryEnvelope>,
}

/// A committed unit not yet confirmed as emitted.
#[derive(Debug, Clone)]
pub struct PendingUnit {
    pub seq: u64,
    pub key: UnitKey,
    pub envelopes: Vec<TelemetryEnvelope>,
}

/// Read `path` as complete lines, truncating a torn (newline-less) trailing
/// fragment in place. Returns the complete lines and whether a repair
/// happened. A missing file is an empty, unrepaired read. **Only a writer
/// holding the cycle lock may call this** — a concurrent writer's in-flight
/// append looks exactly like a torn tail; readers use
/// [`read_complete_lines`].
pub fn read_repaired_lines(path: &Path) -> io::Result<(Vec<String>, bool)> {
    let mut file = match OpenOptions::new().read(true).write(true).open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok((Vec::new(), false)),
        Err(e) => return Err(e),
    };
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    let complete_len = complete_prefix_len(&bytes);
    let repaired = complete_len < bytes.len();
    if repaired {
        log::warn!(
            "ci_telemetry: {} had a torn trailing line ({} byte(s)) — truncated to the last complete line",
            path.display(),
            bytes.len() - complete_len
        );
        file.set_len(complete_len as u64)?;
        file.sync_all()?;
        bytes.truncate(complete_len);
    }
    Ok((split_lines(&bytes), repaired))
}

/// Read-only counterpart of [`read_repaired_lines`]: the complete lines of
/// `path`, ignoring (never touching) any trailing fragment.
pub fn read_complete_lines(path: &Path) -> io::Result<Vec<String>> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    Ok(split_lines(&bytes[..complete_prefix_len(&bytes)]))
}

/// Length of the prefix of `bytes` that ends in a newline.
#[must_use]
pub fn complete_prefix_len(bytes: &[u8]) -> usize {
    bytes.iter().rposition(|b| *b == b'\n').map_or(0, |i| i + 1)
}

fn split_lines(bytes: &[u8]) -> Vec<String> {
    String::from_utf8_lossy(bytes)
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(str::to_string)
        .collect()
}

/// Append `bytes` (one or more complete lines) to `path` and fsync it —
/// plus the parent directory when the file is new, so the directory entry
/// is durable too.
pub fn append_durable(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let parent = path.parent().map(Path::to_path_buf);
    if let Some(dir) = &parent {
        std::fs::create_dir_all(dir)?;
    }
    let existed = path.exists();
    let mut file = OpenOptions::new().create(true).append(true).open(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    if !existed {
        if let Some(dir) = parent {
            if let Ok(dir) = File::open(dir) {
                let _ = dir.sync_all();
            }
        }
    }
    Ok(())
}

/// The loaded ledger.
#[derive(Debug)]
pub struct Ledger {
    path: PathBuf,
    seen: HashSet<UnitKey>,
    watermarks: BTreeMap<String, DateTime<Utc>>,
    next_seq: u64,
    emitted_through: u64,
    pending: Vec<PendingUnit>,
    repaired: bool,
}

impl Ledger {
    /// Load the ledger at `path` **as its writer** (under the cycle lock),
    /// repairing a torn tail. A missing file is an empty ledger; an
    /// unparseable complete line is skipped with a warning.
    pub fn open(path: PathBuf) -> io::Result<Self> {
        let (lines, repaired) = read_repaired_lines(&path)?;
        Ok(Self::from_lines(path, lines, repaired))
    }

    /// Load the ledger read-only (for `status`): never repairs, so it is
    /// safe beside a running cycle.
    pub fn open_read_only(path: PathBuf) -> io::Result<Self> {
        let lines = read_complete_lines(&path)?;
        Ok(Self::from_lines(path, lines, false))
    }

    fn from_lines(path: PathBuf, lines: Vec<String>, repaired: bool) -> Self {
        let mut ledger = Ledger {
            path,
            seen: HashSet::new(),
            watermarks: BTreeMap::new(),
            next_seq: 1,
            emitted_through: 0,
            pending: Vec::new(),
            repaired,
        };
        let mut units: Vec<PendingUnit> = Vec::new();
        for line in lines {
            match serde_json::from_str::<LedgerLine>(&line) {
                Ok(LedgerLine::Unit {
                    seq,
                    repo,
                    run_id,
                    job_id,
                    attempt,
                    envelopes,
                }) => {
                    let key = UnitKey {
                        repo,
                        run_id,
                        job_id,
                        attempt,
                    };
                    ledger.seen.insert(key.clone());
                    ledger.next_seq = ledger.next_seq.max(seq + 1);
                    units.push(PendingUnit {
                        seq,
                        key,
                        envelopes,
                    });
                }
                Ok(LedgerLine::Seen {
                    seq,
                    repo,
                    run_id,
                    job_id,
                    attempt,
                }) => {
                    ledger.seen.insert(UnitKey {
                        repo,
                        run_id,
                        job_id,
                        attempt,
                    });
                    ledger.next_seq = ledger.next_seq.max(seq + 1);
                }
                Ok(LedgerLine::Watermark { repo, created_at }) => {
                    ledger.watermarks.insert(repo, created_at);
                }
                Ok(LedgerLine::Emitted { through_seq }) => {
                    ledger.emitted_through = ledger.emitted_through.max(through_seq);
                }
                Err(error) => log::warn!(
                    "ci_telemetry: skipping unparseable ledger line in {}: {error}",
                    ledger.path.display()
                ),
            }
        }
        let through = ledger.emitted_through;
        ledger.pending = units.into_iter().filter(|u| u.seq > through).collect();
        ledger
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Whether a torn tail was repaired on open.
    #[must_use]
    pub fn repaired(&self) -> bool {
        self.repaired
    }

    #[must_use]
    pub fn is_seen(&self, key: &UnitKey) -> bool {
        self.seen.contains(key)
    }

    /// Number of committed units (runs + jobs).
    #[must_use]
    pub fn unit_count(&self) -> usize {
        self.seen.len()
    }

    #[must_use]
    pub fn watermark(&self, repo: &str) -> Option<DateTime<Utc>> {
        self.watermarks.get(repo).copied()
    }

    #[must_use]
    pub fn watermarks(&self) -> &BTreeMap<String, DateTime<Utc>> {
        &self.watermarks
    }

    /// Committed units whose emission is not yet confirmed.
    #[must_use]
    pub fn pending(&self) -> &[PendingUnit] {
        &self.pending
    }

    /// Commit `units` in one durable append (**the commit point**). Returns
    /// the committed units with their assigned `seq`s, now pending. Units
    /// already seen are skipped.
    pub fn commit(&mut self, units: Vec<UnitDraft>) -> io::Result<Vec<PendingUnit>> {
        let mut committed = Vec::new();
        let mut buffer = String::new();
        let mut seq = self.next_seq;
        for unit in units {
            if self.seen.contains(&unit.key) {
                continue;
            }
            let line = LedgerLine::Unit {
                seq,
                repo: unit.key.repo.clone(),
                run_id: unit.key.run_id,
                job_id: unit.key.job_id,
                attempt: unit.key.attempt,
                envelopes: unit.envelopes.clone(),
            };
            buffer.push_str(&serde_json::to_string(&line).map_err(io::Error::other)?);
            buffer.push('\n');
            committed.push(PendingUnit {
                seq,
                key: unit.key,
                envelopes: unit.envelopes,
            });
            seq += 1;
        }
        if committed.is_empty() {
            return Ok(committed);
        }
        append_durable(&self.path, buffer.as_bytes())?;
        self.next_seq = seq;
        for unit in &committed {
            self.seen.insert(unit.key.clone());
        }
        self.pending.extend(committed.iter().cloned());
        Ok(committed)
    }

    /// Durably record `repo`'s new watermark.
    pub fn set_watermark(&mut self, repo: &str, created_at: DateTime<Utc>) -> io::Result<()> {
        if self.watermarks.get(repo) == Some(&created_at) {
            return Ok(());
        }
        let line = LedgerLine::Watermark {
            repo: repo.to_string(),
            created_at,
        };
        let mut text = serde_json::to_string(&line).map_err(io::Error::other)?;
        text.push('\n');
        append_durable(&self.path, text.as_bytes())?;
        self.watermarks.insert(repo.to_string(), created_at);
        Ok(())
    }

    /// Confirm every unit with `seq <= through_seq` reached the journal.
    pub fn mark_emitted(&mut self, through_seq: u64) -> io::Result<()> {
        if through_seq <= self.emitted_through {
            return Ok(());
        }
        let mut text = serde_json::to_string(&LedgerLine::Emitted { through_seq })
            .map_err(io::Error::other)?;
        text.push('\n');
        append_durable(&self.path, text.as_bytes())?;
        self.emitted_through = through_seq;
        self.pending.retain(|u| u.seq > through_seq);
        Ok(())
    }

    /// Rewrite the ledger compactly (key-only `seen` lines) once it exceeds
    /// `threshold` bytes and nothing is pending. Atomic: temp file + fsync +
    /// rename + directory fsync, so a crash leaves either the old or the new
    /// ledger, never a mix.
    pub fn compact_if_large(&mut self, threshold: u64) -> io::Result<bool> {
        let size = std::fs::metadata(&self.path).map_or(0, |m| m.len());
        if size <= threshold || !self.pending.is_empty() {
            return Ok(false);
        }
        let mut buffer = String::new();
        let mut keys: Vec<&UnitKey> = self.seen.iter().collect();
        keys.sort();
        let seq = self.next_seq.saturating_sub(1);
        for key in keys {
            let line = LedgerLine::Seen {
                seq,
                repo: key.repo.clone(),
                run_id: key.run_id,
                job_id: key.job_id,
                attempt: key.attempt,
            };
            buffer.push_str(&serde_json::to_string(&line).map_err(io::Error::other)?);
            buffer.push('\n');
        }
        for (repo, created_at) in &self.watermarks {
            let line = LedgerLine::Watermark {
                repo: repo.clone(),
                created_at: *created_at,
            };
            buffer.push_str(&serde_json::to_string(&line).map_err(io::Error::other)?);
            buffer.push('\n');
        }
        let emitted = LedgerLine::Emitted {
            through_seq: self.emitted_through.max(seq),
        };
        buffer.push_str(&serde_json::to_string(&emitted).map_err(io::Error::other)?);
        buffer.push('\n');
        let temporary = self.path.with_extension("jsonl.compact");
        {
            let mut file = File::create(&temporary)?;
            file.write_all(buffer.as_bytes())?;
            file.sync_all()?;
        }
        std::fs::rename(&temporary, &self.path)?;
        if let Some(dir) = self.path.parent() {
            if let Ok(dir) = File::open(dir) {
                let _ = dir.sync_all();
            }
        }
        Ok(true)
    }
}
