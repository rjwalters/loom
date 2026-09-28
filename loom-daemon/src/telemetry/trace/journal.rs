//! Process-safe execution journals. Children never open the daemon export queue.
use super::{SpanName, SpanRecord, SpanStatus, TraceAttributes, TraceContext};
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs::{File, OpenOptions},
    io::{BufRead, BufReader, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
};

pub const MAX_JOURNAL_BYTES: u64 = 16 * 1024 * 1024;
const MAX_ENTRY_BYTES: usize = 32 * 1024;

// Test-only counters for the durability barriers this module issues *besides*
// the per-boundary journal write (Issue #8643). Thread-local, so tests running
// in parallel never observe one another's counts.
#[cfg(test)]
thread_local! {
    static PARENT_DIR_SYNCS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static CURSOR_COMMITS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
fn count_barrier(counter: &'static std::thread::LocalKey<std::cell::Cell<usize>>) {
    counter.with(|slot| slot.set(slot.get() + 1));
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActiveSpan {
    pub record: SpanRecord,
    pub owner_pid: u32,
    #[serde(default)]
    pub owner_observed_at: Option<DateTime<Utc>>,
    /// The process responsible for recording the authoritative terminal result.
    /// Worker ownership may change without releasing this supervisor's claim.
    #[serde(default)]
    pub supervisor_pid: Option<u32>,
    #[serde(default)]
    pub supervisor_observed_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "event", content = "span")]
enum Entry {
    Started(ActiveSpan),
    Completed(SpanRecord),
    Supervisor {
        context: TraceContext,
        pid: u32,
        observed_at: DateTime<Utc>,
    },
    Owner {
        context: TraceContext,
        pid: u32,
        #[serde(default)]
        observed_at: Option<DateTime<Utc>>,
    },
}

#[derive(Debug, Clone)]
pub struct Journal {
    path: PathBuf,
}
impl Journal {
    pub fn for_context(path: &Path) -> Self {
        Self {
            path: path.with_extension("jsonl"),
        }
    }
    pub fn from_path(path: PathBuf) -> Self {
        Self { path }
    }
    pub fn path(&self) -> &Path {
        &self.path
    }
    fn lock(&self) -> Result<File> {
        let parent = self.path.parent().context("journal has no parent")?;
        std::fs::create_dir_all(parent)?;
        let mut options = OpenOptions::new();
        options.create(true).truncate(false).read(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let file = options.open(&self.path)?;
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(1000);
        loop {
            match file.try_lock() {
                Ok(()) => break,
                Err(error) if std::time::Instant::now() >= deadline => {
                    log::warn!("observability: trace journal lock retry budget exhausted; record deferred or omitted");
                    return Err(error).context("trace journal busy");
                }
                Err(_) => std::thread::sleep(std::time::Duration::from_millis(2)),
            }
        }
        let len = file.metadata()?.len();
        // A parent-directory fsync durably links a *newly created* journal;
        // once the file holds a record that link is already stable, so
        // repeating it on every lock spends a barrier and buys nothing
        // (Issue #8643). The condition is "the journal is still empty", not
        // "this call created the file", and that difference is what keeps the
        // skip durability-neutral: creation happens before the lock is held,
        // so another process can legitimately open a file this one created and
        // has not yet fsynced. Whoever appends the FIRST record necessarily
        // holds the lock on a zero-length journal and therefore fsyncs the
        // directory before that record exists — no record is ever made durable
        // ahead of the directory entry a reader has to reach it through.
        if len == 0 {
            File::open(parent)?.sync_all()?;
            #[cfg(test)]
            count_barrier(&PARENT_DIR_SYNCS);
        }
        anyhow::ensure!(len <= MAX_JOURNAL_BYTES, "trace journal full");
        Ok(file)
    }
    fn entries(file: &mut File) -> Result<Vec<Entry>> {
        file.seek(SeekFrom::Start(0))?;
        let mut reader = BufReader::new(&mut *file);
        let mut entries = Vec::new();
        let mut complete_bytes = 0_u64;
        loop {
            let mut line = Vec::new();
            let size = reader
                .by_ref()
                .take(MAX_ENTRY_BYTES as u64 + 1)
                .read_until(b'\n', &mut line)?;
            if size == 0 {
                break;
            }
            anyhow::ensure!(size <= MAX_ENTRY_BYTES, "trace journal entry exceeds bound");
            if line.last() != Some(&b'\n') {
                // Holding the file lock proves no writer is still appending.
                // Keep every complete record; discard only a crashed write's
                // unterminated tail so subsequent starts/completions can recover.
                reader.get_mut().set_len(complete_bytes)?;
                reader.get_mut().sync_data()?;
                log::warn!("observability: recovered incomplete trace journal tail");
                break;
            }
            entries.push(serde_json::from_slice(&line)?);
            complete_bytes += size as u64;
        }
        Ok(entries)
    }
    fn append(file: &mut File, entry: &Entry) -> Result<()> {
        let mut bytes = serde_json::to_vec(entry)?;
        bytes.push(b'\n');
        anyhow::ensure!(
            bytes.len() <= MAX_ENTRY_BYTES
                && file.metadata()?.len() + bytes.len() as u64 <= MAX_JOURNAL_BYTES,
            "trace journal full"
        );
        file.seek(SeekFrom::End(0))?;
        file.write_all(&bytes)?;
        file.sync_data()?;
        Ok(())
    }
    pub fn start(
        &self,
        context: TraceContext,
        parent: Option<&TraceContext>,
        name: SpanName,
        at: DateTime<Utc>,
        attributes: TraceAttributes,
    ) -> Result<ActiveSpan> {
        self.start_linked(context, parent, name, at, attributes, Vec::new())
    }
    #[allow(clippy::too_many_arguments)]
    pub fn start_linked(
        &self,
        context: TraceContext,
        parent: Option<&TraceContext>,
        name: SpanName,
        at: DateTime<Utc>,
        attributes: TraceAttributes,
        links: Vec<super::SpanLink>,
    ) -> Result<ActiveSpan> {
        let mut file = self.lock()?;
        let entries = Self::entries(&mut file)?;
        if let Some(active) = entries.iter().find_map(|e| match e {
            Entry::Started(a) if a.record.context == context => Some(a),
            _ => None,
        }) {
            return Ok(active.clone());
        }
        let mut attributes = attributes;
        super::provenance::stamp(&mut attributes);
        if name == SpanName::RoleAttempt {
            let role = attributes.get("loom.role");
            let attempt = entries.iter().filter(|e| matches!(e, Entry::Started(s) if s.record.name == name && s.record.attributes.get("loom.role") == role)).count() + 1;
            attributes
                .entry("loom.attempt".into())
                .or_insert_with(|| attempt.to_string());
        }
        let record = SpanRecord {
            context,
            parent_span_id: parent.map(|p| p.span_id.clone()),
            name,
            started_at: at,
            ended_at: at,
            status: SpanStatus::Unset,
            attributes,
            events: Vec::new(),
            links,
        }
        .bounded();
        // Container PIDs are not host PIDs. Zero prevents daemon orphan probes
        // from guessing; the authoritative parent/reaper closes them.
        let pid = if std::env::var_os("LOOM_SPAWN_CONTAINERIZED").is_some() {
            0
        } else {
            std::process::id()
        };
        let observed_at = Some(Utc::now());
        let active = ActiveSpan {
            record,
            owner_observed_at: observed_at,
            owner_pid: pid,
            supervisor_pid: Some(pid),
            supervisor_observed_at: observed_at,
        };
        Self::append(&mut file, &Entry::Started(active.clone()))?;
        Ok(active)
    }
    pub fn finish(
        &self,
        active: &ActiveSpan,
        at: DateTime<Utc>,
        status: SpanStatus,
        attributes: TraceAttributes,
    ) -> Result<()> {
        let mut file = self.lock()?;
        if Self::entries(&mut file)?
            .iter()
            .any(|e| matches!(e, Entry::Completed(r) if r.context == active.record.context))
        {
            return Ok(());
        }
        let mut record = active.record.clone();
        record.ended_at = at.max(record.started_at);
        record.status = status;
        record.attributes.extend(attributes);
        Self::append(&mut file, &Entry::Completed(record.bounded()))
    }
    pub fn set_owner(&self, context: &TraceContext, pid: u32) -> Result<()> {
        let mut file = self.lock()?;
        Self::entries(&mut file)?;
        Self::append(
            &mut file,
            &Entry::Owner {
                context: context.clone(),
                pid,
                observed_at: Some(Utc::now()),
            },
        )
    }
    /// Adoption transfers only supervision, never the observed worker identity.
    pub fn set_supervisor(&self, context: &TraceContext, pid: u32) -> Result<()> {
        let mut file = self.lock()?;
        Self::entries(&mut file)?;
        Self::append(
            &mut file,
            &Entry::Supervisor {
                context: context.clone(),
                pid,
                observed_at: Utc::now(),
            },
        )
    }
    /// Every completed span still in the journal (drained or not), oldest
    /// first.
    pub fn completed(&self) -> Result<Vec<SpanRecord>> {
        let mut file = self.lock()?;
        Ok(Self::entries(&mut file)?
            .into_iter()
            .filter_map(|entry| match entry {
                Entry::Completed(record) => Some(record),
                _ => None,
            })
            .collect())
    }
    /// Journal an already-complete span in one write (Issue #8908: a late
    /// `loom.runtime.usage` child whose interval is already known). An
    /// invalid record is refused rather than journalled.
    pub fn append_completed(&self, record: SpanRecord) -> Result<()> {
        record.validate().map_err(anyhow::Error::msg)?;
        let mut file = self.lock()?;
        Self::append(&mut file, &Entry::Completed(record.bounded()))
    }
    pub fn has_checkpoint_observations(&self) -> Result<bool> {
        let mut file = self.lock()?;
        Ok(Self::entries(&mut file)?.iter().any(|entry| matches!(entry, Entry::Completed(span) if span.attributes.get("loom.timing_source").is_some_and(|v| matches!(v.as_str(), "checkpoint_write_observed" | "owned_start_checkpoint_completion")))))
    }
    pub fn active(&self) -> Result<Vec<ActiveSpan>> {
        let mut file = self.lock()?;
        Ok(Self::active_entries(Self::entries(&mut file)?))
    }
    fn active_entries(entries: Vec<Entry>) -> Vec<ActiveSpan> {
        let mut active = BTreeMap::new();
        for entry in entries {
            match entry {
                Entry::Started(mut a) => {
                    // Legacy journals already persisted the original creator in
                    // Started, before any Owner transfer. Preserve that identity.
                    if a.supervisor_pid.is_none() {
                        a.supervisor_pid = Some(a.owner_pid);
                        a.supervisor_observed_at = a.owner_observed_at;
                    }
                    active.insert(a.record.context.span_id.as_str().to_owned(), a);
                }
                Entry::Completed(r) => {
                    active.remove(r.context.span_id.as_str());
                }
                Entry::Supervisor {
                    context,
                    pid,
                    observed_at,
                } => {
                    if let Some(span) = active.get_mut(context.span_id.as_str()) {
                        span.supervisor_pid = Some(pid);
                        span.supervisor_observed_at = Some(observed_at);
                    }
                }
                Entry::Owner {
                    context,
                    pid,
                    observed_at,
                } => {
                    if let Some(span) = active.get_mut(context.span_id.as_str()) {
                        span.owner_pid = pid;
                        span.owner_observed_at = observed_at;
                    }
                }
            }
        }
        active.into_values().collect()
    }
    /// Recovery and supervisor adoption serialize on the same journal lock.
    /// Never complete from a stale snapshot taken before an adoption or finish.
    pub fn finish_abandoned(
        &self,
        abandoned: impl Fn(&ActiveSpan) -> bool,
        attributes: TraceAttributes,
    ) -> Result<()> {
        let mut file = self.lock()?;
        let active = Self::active_entries(Self::entries(&mut file)?);
        if active.is_empty() || !active.iter().all(abandoned) {
            return Ok(());
        }
        let now = Utc::now();
        for span in active {
            let mut record = span.record;
            record.ended_at = now.max(record.started_at);
            record.status = SpanStatus::Unset;
            record.attributes.extend(attributes.clone());
            Self::append(&mut file, &Entry::Completed(record.bounded()))?;
        }
        Ok(())
    }
    fn drain_lock(&self) -> Result<File> {
        let mut options = OpenOptions::new();
        options.create(true).truncate(false).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let lock = options.open(self.path.with_extension("drain-lock"))?;
        lock.try_lock().context("trace journal drain busy")?;
        Ok(lock)
    }

    /// Called only by the daemon after drain: retire completed executions once
    /// every completed span is durably in its queue. Active work is retained.
    pub fn retire_if_drained(&self) -> Result<bool> {
        let _drain_lock = self.drain_lock()?;
        let mut file = self.lock()?;
        let entries = Self::entries(&mut file)?;
        let mut active = std::collections::BTreeSet::new();
        let mut started = std::collections::BTreeSet::new();
        let mut completed_parents = Vec::new();
        for entry in entries {
            match entry {
                Entry::Started(span) => {
                    let id = span.record.context.span_id.as_str().to_owned();
                    started.insert(id.clone());
                    active.insert(id);
                }
                Entry::Completed(span) => {
                    active.remove(span.context.span_id.as_str());
                    completed_parents.push(span.parent_span_id);
                }
                Entry::Owner { .. } | Entry::Supervisor { .. } => {}
            }
        }
        // The execution root is the span parented outside this journal: none
        // at all, or a story root (#9037) that no execution journals itself.
        // An orphan whose journalled parent never started (the root's own
        // start failed) counts too; before #9038 such a journal never retired.
        let completed_root = completed_parents.iter().any(|parent| {
            parent
                .as_ref()
                .is_none_or(|p| !started.contains(p.as_str()))
        });
        let cursor_path = self.path.with_extension("cursor");
        let cursor = std::fs::read_to_string(&cursor_path)?.parse::<u64>()?;
        if !completed_root || !active.is_empty() || cursor != file.metadata()?.len() {
            return Ok(false);
        }
        // Queue durability precedes all deletion. A crash during cleanup can
        // replay already queued stable IDs, but cannot lose an unqueued span.
        match std::fs::remove_file(self.path.with_extension("json")) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
        std::fs::remove_file(&self.path)?;
        std::fs::remove_file(cursor_path)?;
        std::fs::remove_file(self.path.with_extension("drain-lock"))?;
        File::open(self.path.parent().context("journal has no parent")?)?.sync_all()?;
        Ok(true)
    }

    /// Atomically replace the byte cursor and durably link the replacement:
    /// two barriers (the temp file's `sync_all`, the parent directory's
    /// `fsync`) every time it is called, which is why [`Self::drain`] commits
    /// per *delivered* record rather than per journal entry.
    fn commit_cursor(cursor_path: &Path, cursor: u64) -> Result<()> {
        let parent = cursor_path.parent().context("cursor has no parent")?;
        let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
        write!(temporary, "{cursor}")?;
        temporary.as_file().sync_all()?;
        temporary.persist(cursor_path).map_err(|e| e.error)?;
        File::open(parent)?.sync_all()?;
        #[cfg(test)]
        count_barrier(&CURSOR_COMMITS);
        Ok(())
    }

    /// Byte cursor advances only after the caller durably accepts a complete span.
    /// A crash after acceptance but before cursor persistence replays stable IDs.
    ///
    /// The cursor is committed once per **delivered** record plus once for the
    /// batch's undelivered tail — not once per journal entry (Issue #8643).
    /// Entries that deliver nothing (`Started`, `Owner`, `Supervisor`) are
    /// re-read and re-skipped on replay, never re-delivered, so batching the
    /// cursor across them costs nothing on a crash. Batching it across
    /// *delivered* records would instead re-offer up to a whole batch, and the
    /// backend only deduplicates `sweep.completed`/`sweep.outcome` by
    /// `(kind, sweep_id)` — `trace.span` has no such index, so a replayed span
    /// duplicates its row. The at-most-one-replayed-record contract therefore
    /// stays exactly as it was.
    pub fn drain(&self, mut accept: impl FnMut(SpanRecord) -> Result<()>) -> Result<usize> {
        let _drain_lock = self.drain_lock()?;
        let mut file = self.lock()?;
        let cursor_path = self.path.with_extension("cursor");
        let cursor = std::fs::read_to_string(&cursor_path)
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(0);
        anyhow::ensure!(cursor <= file.metadata()?.len(), "trace cursor exceeds journal");
        file.seek(SeekFrom::Start(cursor))?;
        let mut reader = BufReader::new(&mut file);
        let mut batch = Vec::new();
        let mut end = cursor;
        for _ in 0..512 {
            let mut line = Vec::new();
            let size = reader
                .by_ref()
                .take(MAX_ENTRY_BYTES as u64 + 1)
                .read_until(b'\n', &mut line)?;
            if size == 0 || line.last() != Some(&b'\n') {
                break;
            }
            anyhow::ensure!(size <= MAX_ENTRY_BYTES, "trace entry exceeds bound");
            let entry = serde_json::from_slice::<Entry>(&line)?;
            end += size as u64;
            batch.push((end, entry));
        }
        // Queue fsync may be slow. Release the writer lock before any queue or
        // cursor I/O so live tool calls can continue appending during export.
        drop(reader);
        drop(file);
        let mut count = 0;
        let mut committed = cursor;
        let mut scanned = cursor;
        for (end, entry) in batch {
            scanned = end;
            if let Entry::Completed(record) = entry {
                record.validate().map_err(anyhow::Error::msg)?;
                accept(record.bounded())?;
                count += 1;
                Self::commit_cursor(&cursor_path, end)?;
                committed = end;
            }
        }
        // One commit for the undelivered tail, so the cursor still reaches EOF
        // and `retire_if_drained` can retire a fully drained journal.
        if scanned > committed {
            Self::commit_cursor(&cursor_path, scanned)?;
        }
        Ok(count)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
#[path = "journal_tests.rs"]
mod tests;
