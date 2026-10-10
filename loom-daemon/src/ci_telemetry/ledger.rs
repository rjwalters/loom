//! The durable dedup ledger — `.loom/state/ci-telemetry/seen.jsonl`.
//!
//! Append-only JSONL. Six line types:
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
//! - `log_wanted` (#8825) — one completed job whose log should be captured,
//!   carrying everything needed to build its `ci.job.log` records without
//!   re-listing the run (the run is already "seen", so it is never listed
//!   again). A job-log unit's key is the *same* `(repo, run_id, job_id)`
//!   with `logs: true`, so log capture is a **separate** commit from the
//!   `ci.job` record's: a failed download retries on the next cycle and the
//!   already-emitted `ci.job` record is never redone.
//! - `log_failure` (#8825) — cumulative failed download attempts for one
//!   `(repo, job_id)` and the last named reason. At
//!   [`logs::MAX_ATTEMPTS`](super::logs::MAX_ATTEMPTS) the poller stops
//!   retrying and `status` reports the job's logs as failed — silence would
//!   otherwise read as "captured".
//!
//! **Torn tail.** A crash mid-append can leave a trailing partial line. On
//! open it is detected (bytes after the final `\n`), the file is truncated
//! back to the last complete line, and the partial entry is never counted —
//! the unit it would have committed was therefore never emitted either (emit
//! strictly follows commit), and the next poll simply commits it again.
//!
//! **Bounded working set (#11159).** The ledger is opened on every poll
//! cycle, so its in-memory size must follow the working set, not all history.
//! Opening *streams* the file line by line ([`scan_lines`]) — the whole file
//! is never materialised — and a key line whose `committed_at` lies more than
//! [`SEEN_RETENTION_DAYS`] before its repo's watermark is **expired**: it is
//! read past and never enters the seen set (see [`Ledger::open`] for why no
//! consumer can ask for such a key again). Keys are held compactly, with the
//! repo name interned once per repo rather than once per key.

use std::collections::{BTreeMap, HashMap};
use std::fs::{File, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use chrono::{DateTime, Duration, TimeZone, Utc};
use serde::{Deserialize, Serialize};

use super::logs::LogTarget;
use crate::telemetry::TelemetryEnvelope;

/// A unit's dedup key: `(repo, run_id, job_id)` plus the run attempt.
/// `job_id` is `None` for the run-level unit. The attempt makes a re-run of
/// an already-recorded run a *new* run-level unit (its jobs already have
/// fresh GitHub `job_id`s), so each attempt is emitted exactly once.
///
/// `logs` (#8825) splits the key space in two: `false` is the run/job
/// **record** unit, `true` the same job's captured **log** chunks. They are
/// committed independently, so a log-download failure retries without ever
/// re-emitting the `ci.job` record that already landed.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct UnitKey {
    pub repo: String,
    pub run_id: u64,
    pub job_id: Option<u64>,
    pub attempt: u32,
    pub logs: bool,
}

impl UnitKey {
    #[must_use]
    pub fn run(repo: &str, run_id: u64, attempt: u32) -> Self {
        UnitKey {
            repo: repo.to_string(),
            run_id,
            job_id: None,
            attempt,
            logs: false,
        }
    }

    #[must_use]
    pub fn job(repo: &str, run_id: u64, job_id: u64, attempt: u32) -> Self {
        UnitKey {
            repo: repo.to_string(),
            run_id,
            job_id: Some(job_id),
            attempt,
            logs: false,
        }
    }

    /// The `(repo, job_id)` job-log capture unit — "logs done" for that job.
    #[must_use]
    pub fn job_logs(repo: &str, run_id: u64, job_id: u64, attempt: u32) -> Self {
        UnitKey {
            logs: true,
            ..UnitKey::job(repo, run_id, job_id, attempt)
        }
    }
}

fn first_attempt() -> u32 {
    1
}

#[allow(clippy::trivially_copy_pass_by_ref)]
fn is_false(value: &bool) -> bool {
    !*value
}

/// Compact the ledger once it exceeds this many bytes (and nothing is
/// pending) — confirmed units drop their envelope payloads.
pub const COMPACT_THRESHOLD_BYTES: u64 = 8 * 1024 * 1024;

/// How far behind its repo's watermark a key's `committed_at` may fall before
/// [`Ledger::open`] expires it (#11159).
///
/// The bound has to outlast every path that can still ask about a key:
///
/// - the repo sweep lists runs `created >= runs_floor`, the older of the
///   watermark and `now - rescan window` ([`super::poll::runs_floor`]); a
///   watermark is a GitHub `created_at`, never ahead of now, so a key
///   committed (hence created) before `watermark - RETENTION` is below both;
/// - the feed-driven single-run path ([`super::poll::targeted`]) records any
///   run GitHub reports as completed, and a run can be re-run — re-listing
///   its earlier attempts' jobs under `filter=all` — for up to 30 days after
///   it was created; a workflow run can last at most 35 days;
/// - the initial lookback and the rescan window (both far shorter, asserted
///   below).
///
/// 45 days clears all of them with a margin for host clock skew. Expiry is
/// relative to the *watermark*, not to the wall clock, so a daemon that was
/// stopped for months does not expire the keys its first sweep will re-list
/// (the floor is then the old watermark).
pub const SEEN_RETENTION_DAYS: i64 = 45;

const _: () = assert!(SEEN_RETENTION_DAYS * 24 > super::RESCAN_WINDOW_HOURS);
const _: () = assert!(SEEN_RETENTION_DAYS * 24 > super::INITIAL_LOOKBACK_HOURS);
const _: () = assert!(SEEN_RETENTION_DAYS > 35, "must outlast GitHub's re-run and run limits");

/// The in-memory stamp of a key with no `committed_at` (a line written before
/// #11159, or by an older daemon). It is never expired; the next compaction
/// stamps it with the compaction time, which is a safe upper bound on when
/// its run was created.
const UNSTAMPED: i64 = i64::MIN;

/// The in-memory form of a [`UnitKey`]: the repo is an index into
/// [`Ledger::repos`], so a key carries no heap allocation of its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct SeenKey {
    repo: u32,
    attempt: u32,
    run_id: u64,
    job_id: Option<u64>,
    logs: bool,
}

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
        /// #8825. Absent on every pre-#8825 line, which is exactly the
        /// record-unit meaning, so an existing ledger loads unchanged.
        #[serde(default, skip_serializing_if = "is_false")]
        logs: bool,
        /// #11159: the wall-clock time this unit was committed — always at or
        /// after its run's `created_at`. Optional on read (pre-#11159 lines
        /// have none), and an older daemon ignores it as an unknown field.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        committed_at: Option<DateTime<Utc>>,
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
        #[serde(default, skip_serializing_if = "is_false")]
        logs: bool,
        /// #11159: as on `unit`; a compaction carries it over, and stamps a
        /// key that had none with the compaction time.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        committed_at: Option<DateTime<Utc>>,
    },
    Watermark {
        repo: String,
        created_at: DateTime<Utc>,
    },
    Emitted {
        through_seq: u64,
    },
    /// #8825: a completed job whose log capture is wanted but not yet done.
    LogWanted {
        target: LogTarget,
    },
    /// #8825: cumulative failed download attempts for one job's log.
    LogFailure {
        repo: String,
        job_id: u64,
        attempts: u32,
        error: String,
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

/// Bytes read per step when scanning backwards for the last newline, and the
/// read buffer of every forward line scan. Memory for a repair or a scan is
/// bounded by this plus the longest single line, never by the file size
/// (#11045: a 5.4 GB journal read whole OOM-killed the fleet captain).
pub const SCAN_BLOCK_BYTES: usize = 64 * 1024;

/// Length of `file`'s prefix that ends in a newline (`0` when it holds
/// none), found by reading backwards from the end in [`SCAN_BLOCK_BYTES`]
/// blocks. A torn tail costs at most its own length plus one block.
pub fn complete_len(file: &mut File) -> io::Result<u64> {
    let len = file.metadata()?.len();
    let mut end = len;
    let mut block = vec![0_u8; SCAN_BLOCK_BYTES];
    while end > 0 {
        let start = end.saturating_sub(SCAN_BLOCK_BYTES as u64);
        let size = usize::try_from(end - start).unwrap_or(SCAN_BLOCK_BYTES);
        file.seek(SeekFrom::Start(start))?;
        file.read_exact(&mut block[..size])?;
        if let Some(i) = block[..size].iter().rposition(|b| *b == b'\n') {
            return Ok(start + i as u64 + 1);
        }
        end = start;
    }
    Ok(0)
}

/// Truncate a torn (newline-less) trailing fragment of `path` in place,
/// reading only the tail ([`complete_len`]). Returns whether a repair
/// happened; a missing file is unrepaired. **Only a writer holding the cycle
/// lock may call this** — a concurrent writer's in-flight append looks
/// exactly like a torn tail.
pub fn repair_torn_tail(path: &Path) -> io::Result<bool> {
    let mut file = match OpenOptions::new().read(true).write(true).open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e),
    };
    let len = file.metadata()?.len();
    let complete = complete_len(&mut file)?;
    if complete == len {
        return Ok(false);
    }
    log::warn!(
        "ci_telemetry: {} had a torn trailing line ({} byte(s)) — truncated to the last complete line",
        path.display(),
        len - complete
    );
    file.set_len(complete)?;
    file.sync_all()?;
    Ok(true)
}

/// Stream the complete (newline-terminated) lines of `path` from byte `from`,
/// handing each to `visit` without its `\n`. A trailing fragment is never
/// visited. `visit` returning `false` stops the scan *before* that line is
/// consumed. Returns the offset just past the last consumed line (`from`
/// when nothing was, or when the file is missing).
///
/// A line longer than `max_line` is skipped (consumed, never buffered whole,
/// and named in a warning), so one corrupt newline-less run cannot pull the
/// whole file into memory.
pub fn scan_lines(
    path: &Path,
    from: u64,
    max_line: u64,
    mut visit: impl FnMut(&[u8]) -> bool,
) -> io::Result<u64> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(from),
        Err(e) => return Err(e),
    };
    let mut reader = BufReader::with_capacity(SCAN_BLOCK_BYTES, file);
    reader.seek(SeekFrom::Start(from))?;
    let mut offset = from;
    let mut line = Vec::new();
    loop {
        line.clear();
        let read = (&mut reader).take(max_line).read_until(b'\n', &mut line)? as u64;
        if read == 0 {
            break;
        }
        if line.last() != Some(&b'\n') {
            if read < max_line {
                break; // a trailing fragment: never consumed
            }
            let Some(rest) = skip_past_newline(&mut reader)? else {
                break; // an oversized trailing fragment
            };
            log::warn!(
                "ci_telemetry: skipping a {}-byte line at offset {offset} of {} (over the {max_line}-byte line cap)",
                read + rest,
                path.display()
            );
            offset += read + rest;
            continue;
        }
        if !visit(&line[..line.len() - 1]) {
            break;
        }
        offset += read;
    }
    Ok(offset)
}

/// Consume `reader` through its next newline without buffering the bytes.
/// Returns how many bytes that was, or `None` at end of file.
fn skip_past_newline(reader: &mut impl BufRead) -> io::Result<Option<u64>> {
    let mut skipped = 0_u64;
    loop {
        let buffer = reader.fill_buf()?;
        if buffer.is_empty() {
            return Ok(None);
        }
        if let Some(i) = buffer.iter().position(|b| *b == b'\n') {
            reader.consume(i + 1);
            return Ok(Some(skipped + i as u64 + 1));
        }
        let size = buffer.len();
        reader.consume(size);
        skipped += size as u64;
    }
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

/// One job's log-capture state, as `status` reports it (#8825).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogCounts {
    /// Jobs whose log chunks are committed.
    pub done: usize,
    /// Jobs wanted and still retriable.
    pub pending: usize,
    /// Jobs given up on after [`logs::MAX_ATTEMPTS`](super::logs::MAX_ATTEMPTS).
    pub failed: usize,
}

/// The loaded ledger.
#[derive(Debug)]
pub struct Ledger {
    path: PathBuf,
    /// Committed keys → their `committed_at` (unix seconds, or
    /// [`UNSTAMPED`]). Expired keys are never inserted (#11159).
    seen: HashMap<SeenKey, i64>,
    /// Interned repo names; [`SeenKey::repo`] indexes this.
    repos: Vec<String>,
    repo_index: HashMap<String, u32>,
    watermarks: BTreeMap<String, DateTime<Utc>>,
    next_seq: u64,
    emitted_through: u64,
    pending: Vec<PendingUnit>,
    repaired: bool,
    /// `(repo, job_id)` → the job whose log is wanted.
    log_wanted: BTreeMap<(String, u64), LogTarget>,
    /// `(repo, job_id)` → (cumulative attempts, last named reason).
    log_failures: BTreeMap<(String, u64), (u32, String)>,
    /// True while the file holds `unit` lines (envelope payloads) a
    /// compaction would fold into key-only `seen` lines: set when such a
    /// line is loaded or committed, cleared by a compaction (#11160).
    has_uncompacted_units: bool,
    /// On-disk size right after the last compaction this process ran
    /// (`0` = none yet). Compaction keeps every `seen` key, so a ledger
    /// that stays above the threshold afterwards must not be rewritten
    /// again until it has really grown.
    compacted_size: u64,
    /// Key lines read past as expired by this open.
    expired: usize,
}

impl Ledger {
    /// Load the ledger at `path` **as its writer** (under the cycle lock),
    /// repairing a torn tail. A missing file is an empty ledger; an
    /// unparseable complete line is skipped with a warning.
    ///
    /// The file is streamed, never held whole, and expired keys are dropped
    /// as they are read (#11159). A `unit`/`seen` key of repo R is
    /// **expired** when it carries a `committed_at` and that is more than
    /// [`SEEN_RETENTION_DAYS`] before R's watermark *as read so far*.
    /// Nothing can ask about such a key again:
    ///
    /// - run listing, per-run and per-job dedupe, and the artifact-span gate
    ///   all key on runs the sweep lists (`created >= runs_floor`, the older
    ///   of the watermark and the rescan window) or the feed path records
    ///   (re-runnable for 30 days) — see [`SEEN_RETENTION_DAYS`];
    /// - a job-log key is the "logs done" marker for its `log_wanted` line.
    ///   That line is always written *before* the log unit (a log is only
    ///   captured once wanted, and a captured job is never wanted again), so
    ///   when the marker expires its wanted line is already loaded and is
    ///   dropped with it — exactly what compaction does for captured jobs;
    /// - a committed-but-unconfirmed unit is pending whatever its age, and
    ///   its key is restored once the file is read.
    ///
    /// Using the watermark seen *so far* errs towards keeping: watermarks
    /// only advance, so an earlier value can only expire less. Compaction
    /// writes the watermarks first so the compacted form expires fully.
    /// A key with no `committed_at` (pre-#11159) is never expired here; the
    /// next compaction stamps it, so it ages out
    /// [`SEEN_RETENTION_DAYS`] after that.
    ///
    /// [`unit_count`](Self::unit_count) and the log `done` counts therefore
    /// report the **retained** keys, not all history.
    pub fn open(path: PathBuf) -> io::Result<Self> {
        let repaired = repair_torn_tail(&path)?;
        let mut ledger = Self::load(path, repaired)?;
        // A loaded file with no `unit` lines and no expired key lines is
        // already compact: skip the redundant rewrite a fresh process would
        // otherwise do (#11160).
        if !ledger.has_uncompacted_units && ledger.expired == 0 {
            ledger.compacted_size = std::fs::metadata(&ledger.path).map_or(0, |m| m.len());
        }
        Ok(ledger)
    }

    /// Load the ledger read-only (for `status`): never repairs, so it is
    /// safe beside a running cycle. Same streaming and expiry as
    /// [`open`](Self::open).
    pub fn open_read_only(path: PathBuf) -> io::Result<Self> {
        Self::load(path, false)
    }

    fn load(path: PathBuf, repaired: bool) -> io::Result<Self> {
        let mut ledger = Ledger {
            path,
            seen: HashMap::new(),
            repos: Vec::new(),
            repo_index: HashMap::new(),
            watermarks: BTreeMap::new(),
            next_seq: 1,
            emitted_through: 0,
            pending: Vec::new(),
            repaired,
            log_wanted: BTreeMap::new(),
            log_failures: BTreeMap::new(),
            has_uncompacted_units: false,
            compacted_size: 0,
            expired: 0,
        };
        // Units read so far that no `emitted` line read so far covers. An
        // `emitted` line prunes it, so it stays bounded by what is pending
        // between confirmations, not by every unit line in the file.
        let mut units: Vec<PendingUnit> = Vec::new();
        let path = ledger.path.clone();
        scan_lines(&path, 0, u64::MAX, |line| {
            let text = String::from_utf8_lossy(line);
            let text = text.trim_end_matches('\r');
            if !text.trim().is_empty() {
                ledger.load_line(text, &mut units);
            }
            true
        })?;
        let through = ledger.emitted_through;
        units.retain(|u| u.seq > through);
        for unit in &units {
            // A pending unit replays whatever its age, so its key must stay
            // seen. Unstamped is the conservative choice: never expired.
            let key = ledger.intern_key(&unit.key);
            ledger.seen.entry(key).or_insert(UNSTAMPED);
        }
        ledger.pending = units;
        if ledger.expired > 0 {
            log::debug!(
                "ci_telemetry: {} expired ledger key line(s) not loaded from {} (#11159)",
                ledger.expired,
                ledger.path.display()
            );
        }
        Ok(ledger)
    }

    /// Fold one ledger line into the loaded state.
    fn load_line(&mut self, text: &str, units: &mut Vec<PendingUnit>) {
        match serde_json::from_str::<LedgerLine>(text) {
            Ok(LedgerLine::Unit {
                seq,
                repo,
                run_id,
                job_id,
                attempt,
                logs,
                committed_at,
                envelopes,
            }) => {
                self.next_seq = self.next_seq.max(seq + 1);
                let key = UnitKey {
                    repo,
                    run_id,
                    job_id,
                    attempt,
                    logs,
                };
                self.has_uncompacted_units = true;
                self.load_key(&key, committed_at);
                if seq > self.emitted_through {
                    units.push(PendingUnit {
                        seq,
                        key,
                        envelopes,
                    });
                }
            }
            Ok(LedgerLine::Seen {
                seq,
                repo,
                run_id,
                job_id,
                attempt,
                logs,
                committed_at,
            }) => {
                self.next_seq = self.next_seq.max(seq + 1);
                let key = UnitKey {
                    repo,
                    run_id,
                    job_id,
                    attempt,
                    logs,
                };
                self.load_key(&key, committed_at);
            }
            Ok(LedgerLine::Watermark { repo, created_at }) => {
                self.watermarks.insert(repo, created_at);
            }
            Ok(LedgerLine::Emitted { through_seq }) => {
                if through_seq > self.emitted_through {
                    self.emitted_through = through_seq;
                    units.retain(|u| u.seq > through_seq);
                }
            }
            Ok(LedgerLine::LogWanted { target }) => {
                self.log_wanted
                    .insert((target.repo.clone(), target.job_id), target);
            }
            Ok(LedgerLine::LogFailure {
                repo,
                job_id,
                attempts,
                error,
            }) => {
                let entry = self
                    .log_failures
                    .entry((repo, job_id))
                    .or_insert((0, String::new()));
                if attempts >= entry.0 {
                    *entry = (attempts, error);
                }
            }
            Err(error) => log::warn!(
                "ci_telemetry: skipping unparseable ledger line in {}: {error}",
                self.path.display()
            ),
        }
    }

    /// Insert `key` as seen unless it is expired (see [`open`](Self::open)).
    fn load_key(&mut self, key: &UnitKey, committed_at: Option<DateTime<Utc>>) {
        let stamp = committed_at.map_or(UNSTAMPED, |at| at.timestamp());
        if let Some(at) = committed_at {
            let expired = self
                .watermarks
                .get(&key.repo)
                .is_some_and(|watermark| at < *watermark - Duration::days(SEEN_RETENTION_DAYS));
            if expired {
                self.expired += 1;
                if key.logs {
                    // Captured: its wanted line, already read, is done too.
                    if let Some(job_id) = key.job_id {
                        self.log_wanted.remove(&(key.repo.clone(), job_id));
                    }
                }
                return;
            }
        }
        let interned = self.intern_key(key);
        let entry = self.seen.entry(interned).or_insert(stamp);
        // A key seen twice (a `unit` and later its compacted `seen`) keeps
        // the later stamp — later is the safe direction.
        *entry = (*entry).max(stamp);
    }

    fn intern_key(&mut self, key: &UnitKey) -> SeenKey {
        let repo = match self.repo_index.get(key.repo.as_str()) {
            Some(index) => *index,
            None => {
                let index = u32::try_from(self.repos.len()).unwrap_or(u32::MAX);
                self.repos.push(key.repo.clone());
                self.repo_index.insert(key.repo.clone(), index);
                index
            }
        };
        SeenKey {
            repo,
            attempt: key.attempt,
            run_id: key.run_id,
            job_id: key.job_id,
            logs: key.logs,
        }
    }

    /// `key`'s compact form, if its repo has any key at all.
    fn lookup(&self, key: &UnitKey) -> Option<SeenKey> {
        self.repo_index.get(key.repo.as_str()).map(|repo| SeenKey {
            repo: *repo,
            attempt: key.attempt,
            run_id: key.run_id,
            job_id: key.job_id,
            logs: key.logs,
        })
    }

    /// Key lines this open read past as expired (#11159).
    #[must_use]
    pub fn expired_on_open(&self) -> usize {
        self.expired
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

    /// Whether a run created at `created_at` is older than `repo`'s retention
    /// boundary (watermark − [`SEEN_RETENTION_DAYS`]). Such a run's keys may
    /// have been dropped by [`open`](Self::open), so [`is_seen`](Self::is_seen)
    /// can no longer vouch for it and a consumer fed an arbitrary run key (the
    /// feed-driven path) must refuse it rather than record it again (#11159).
    /// `false` when the repo has no watermark (nothing is ever expired there).
    #[must_use]
    pub fn run_is_past_retention(&self, repo: &str, created_at: DateTime<Utc>) -> bool {
        self.watermarks
            .get(repo)
            .is_some_and(|watermark| created_at < *watermark - Duration::days(SEEN_RETENTION_DAYS))
    }

    #[must_use]
    pub fn is_seen(&self, key: &UnitKey) -> bool {
        self.lookup(key)
            .is_some_and(|key| self.seen.contains_key(&key))
    }

    /// Number of committed units (runs + jobs) currently retained — expired
    /// keys (#11159) are not counted.
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
        // Second precision is all expiry needs, and keeps the in-memory and
        // on-disk stamps identical.
        let now = Utc::now();
        let now = Utc
            .timestamp_opt(now.timestamp(), 0)
            .single()
            .unwrap_or(now);
        for unit in units {
            if self.is_seen(&unit.key) {
                continue;
            }
            let line = LedgerLine::Unit {
                seq,
                repo: unit.key.repo.clone(),
                run_id: unit.key.run_id,
                job_id: unit.key.job_id,
                attempt: unit.key.attempt,
                logs: unit.key.logs,
                committed_at: Some(now),
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
        self.has_uncompacted_units = true;
        for unit in &committed {
            let key = self.intern_key(&unit.key);
            self.seen.insert(key, now.timestamp());
        }
        self.pending.extend(committed.iter().cloned());
        Ok(committed)
    }

    /// Record that each of `targets`' logs should be captured (#8825).
    /// Already-wanted, already-captured and already-failed jobs are skipped,
    /// so this is idempotent across re-polls and restarts.
    pub fn want_logs(&mut self, targets: &[LogTarget]) -> io::Result<()> {
        let mut buffer = String::new();
        let mut added = Vec::new();
        for target in targets {
            let key = (target.repo.clone(), target.job_id);
            if self.log_wanted.contains_key(&key)
                || self.log_failures.contains_key(&key)
                || self.is_seen(&UnitKey::job_logs(
                    &target.repo,
                    target.run_id,
                    target.job_id,
                    target.attempt,
                ))
            {
                continue;
            }
            let line = LedgerLine::LogWanted {
                target: target.clone(),
            };
            buffer.push_str(&serde_json::to_string(&line).map_err(io::Error::other)?);
            buffer.push('\n');
            added.push((key, target.clone()));
        }
        if added.is_empty() {
            return Ok(());
        }
        append_durable(&self.path, buffer.as_bytes())?;
        self.log_wanted.extend(added);
        Ok(())
    }

    /// Wanted job logs that are neither captured nor given up on, oldest
    /// completion first so a backlog drains in CI order.
    #[must_use]
    pub fn pending_logs(&self) -> Vec<LogTarget> {
        let mut pending: Vec<LogTarget> = self
            .log_wanted
            .values()
            .filter(|target| {
                !self.is_seen(&UnitKey::job_logs(
                    &target.repo,
                    target.run_id,
                    target.job_id,
                    target.attempt,
                )) && self
                    .log_failures
                    .get(&(target.repo.clone(), target.job_id))
                    .is_none_or(|(attempts, _)| *attempts < super::logs::MAX_ATTEMPTS)
            })
            .cloned()
            .collect();
        pending.sort_by_key(|target| (target.completed_at, target.job_id));
        pending
    }

    /// Durably record one failed log download. At
    /// [`logs::MAX_ATTEMPTS`](super::logs::MAX_ATTEMPTS) the job stops being
    /// retried and is reported as failed rather than silently dropped.
    pub fn record_log_failure(&mut self, repo: &str, job_id: u64, error: &str) -> io::Result<()> {
        let key = (repo.to_string(), job_id);
        let attempts = self.log_failures.get(&key).map_or(0, |(n, _)| *n) + 1;
        let line = LedgerLine::LogFailure {
            repo: repo.to_string(),
            job_id,
            attempts,
            error: error.chars().take(300).collect(),
        };
        let mut text = serde_json::to_string(&line).map_err(io::Error::other)?;
        text.push('\n');
        append_durable(&self.path, text.as_bytes())?;
        let error = error.chars().take(300).collect();
        self.log_failures.insert(key, (attempts, error));
        Ok(())
    }

    /// Log-capture totals across every repo.
    #[must_use]
    pub fn log_counts(&self) -> LogCounts {
        let mut counts = LogCounts {
            done: self.seen.keys().filter(|key| key.logs).count(),
            ..LogCounts::default()
        };
        for target in self.log_wanted.values() {
            if self.is_seen(&UnitKey::job_logs(
                &target.repo,
                target.run_id,
                target.job_id,
                target.attempt,
            )) {
                continue;
            }
            let failed = self
                .log_failures
                .get(&(target.repo.clone(), target.job_id))
                .is_some_and(|(attempts, _)| *attempts >= super::logs::MAX_ATTEMPTS);
            if failed {
                counts.failed += 1;
            } else {
                counts.pending += 1;
            }
        }
        counts
    }

    /// Per-repo log-capture totals, for `status`.
    #[must_use]
    pub fn log_counts_by_repo(&self) -> BTreeMap<String, LogCounts> {
        let mut by_repo: BTreeMap<String, LogCounts> = BTreeMap::new();
        for key in self.seen.keys().filter(|key| key.logs) {
            by_repo
                .entry(self.repos[key.repo as usize].clone())
                .or_default()
                .done += 1;
        }
        for target in self.log_wanted.values() {
            if self.is_seen(&UnitKey::job_logs(
                &target.repo,
                target.run_id,
                target.job_id,
                target.attempt,
            )) {
                continue;
            }
            let entry = by_repo.entry(target.repo.clone()).or_default();
            if self
                .log_failures
                .get(&(target.repo.clone(), target.job_id))
                .is_some_and(|(attempts, _)| *attempts >= super::logs::MAX_ATTEMPTS)
            {
                entry.failed += 1;
            } else {
                entry.pending += 1;
            }
        }
        by_repo
    }

    /// The most recent named log-download failure, if any.
    #[must_use]
    pub fn last_log_failure(&self) -> Option<(String, u64, u32, String)> {
        self.log_failures
            .iter()
            .max_by_key(|(_, (attempts, _))| *attempts)
            .map(|((repo, job_id), (attempts, error))| {
                (repo.clone(), *job_id, *attempts, error.clone())
            })
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
    ///
    /// A no-op (`Ok(false)`) unless a rewrite could actually shrink the
    /// file (#11160): compaction keeps every retained `seen` key, so a
    /// compacted ledger can itself exceed `threshold`; it is rewritten again only
    /// when `unit` lines were committed since, or the file has grown past
    /// twice its last compacted size.
    ///
    /// #11159: only the retained (unexpired) keys are written, each with its
    /// `committed_at`; a key that had none is stamped with the compaction
    /// time (a safe upper bound on its run's creation), so it can expire
    /// later. Watermarks are written **first**, so the next open expires
    /// against them from the first key line on.
    pub fn compact_if_large(&mut self, threshold: u64) -> io::Result<bool> {
        let size = std::fs::metadata(&self.path).map_or(0, |m| m.len());
        if size <= threshold || !self.pending.is_empty() {
            return Ok(false);
        }
        let grown = self.compacted_size > 0 && size > self.compacted_size.saturating_mul(2);
        if self.compacted_size > 0 && !self.has_uncompacted_units && !grown {
            return Ok(false);
        }
        let mut buffer = String::new();
        for (repo, created_at) in &self.watermarks {
            let line = LedgerLine::Watermark {
                repo: repo.clone(),
                created_at: *created_at,
            };
            buffer.push_str(&serde_json::to_string(&line).map_err(io::Error::other)?);
            buffer.push('\n');
        }
        let now = Utc::now().timestamp();
        let mut keys: Vec<(&str, SeenKey, i64)> = self
            .seen
            .iter()
            .map(|(key, stamp)| (self.repos[key.repo as usize].as_str(), *key, *stamp))
            .collect();
        // The `UnitKey` order: repo name, run, job, attempt, logs.
        keys.sort_by(|a, b| {
            (a.0, a.1.run_id, a.1.job_id, a.1.attempt, a.1.logs).cmp(&(
                b.0,
                b.1.run_id,
                b.1.job_id,
                b.1.attempt,
                b.1.logs,
            ))
        });
        let seq = self.next_seq.saturating_sub(1);
        for (repo, key, stamp) in &keys {
            let stamp = if *stamp == UNSTAMPED { now } else { *stamp };
            let line = LedgerLine::Seen {
                seq,
                repo: (*repo).to_string(),
                run_id: key.run_id,
                job_id: key.job_id,
                attempt: key.attempt,
                logs: key.logs,
                committed_at: Utc.timestamp_opt(stamp, 0).single(),
            };
            buffer.push_str(&serde_json::to_string(&line).map_err(io::Error::other)?);
            buffer.push('\n');
        }
        drop(keys);
        // #8825: a job whose log capture is still outstanding (pending, or
        // given up on) must survive compaction — dropping a `log_wanted`
        // would silently abandon the capture, and dropping a `log_failure`
        // would restart an unbounded retry loop against a log GitHub has
        // already expired. Captured jobs need neither line: their `seen`
        // entry above is the done marker.
        for target in self.log_wanted.values() {
            if self.is_seen(&UnitKey::job_logs(
                &target.repo,
                target.run_id,
                target.job_id,
                target.attempt,
            )) {
                continue;
            }
            let line = LedgerLine::LogWanted {
                target: target.clone(),
            };
            buffer.push_str(&serde_json::to_string(&line).map_err(io::Error::other)?);
            buffer.push('\n');
            if let Some((attempts, error)) =
                self.log_failures.get(&(target.repo.clone(), target.job_id))
            {
                let line = LedgerLine::LogFailure {
                    repo: target.repo.clone(),
                    job_id: target.job_id,
                    attempts: *attempts,
                    error: error.clone(),
                };
                buffer.push_str(&serde_json::to_string(&line).map_err(io::Error::other)?);
                buffer.push('\n');
            }
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
        self.has_uncompacted_units = false;
        self.compacted_size = std::fs::metadata(&self.path).map_or(0, |m| m.len());
        // The file now carries the compaction-time stamps; match it.
        for stamp in self.seen.values_mut() {
            if *stamp == UNSTAMPED {
                *stamp = now;
            }
        }
        Ok(true)
    }
}
