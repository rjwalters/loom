//! The daemon's ingest of agent `gh` front rows into `loom.forge.calls`
//! (#10607 slice B).
//!
//! The front ([`crate::agent_gh`]) runs in the agent's process tree and has
//! no OTLP exporter, so its rows — served reads (caller `agent_gh_front`)
//! and W5 passthroughs (`agent.gh.<command>`), each stamped `ag`/`vi`
//! ([`super::agent`]) — only reach the host sink. On every rate-limit export
//! tick the daemon tails the sink ([`tick`]) and records each new agent row
//! with [`forge_calls::record_agent`], labelled `agent` = the row's role.
//!
//! Two sources:
//!
//! - the sink itself, written by host-side fronts (tmux sessions and
//!   uncontained `spawn-worker` workers, same uid as the daemon) beside the
//!   daemon's own rows, which are skipped (no `ag`: already exported);
//! - `<sink>/`[`CONTAINED_SUBDIR`], the only part of the sink a container
//!   can write ([`super::agent::container_mount`]).
//!
//! Every row is untrusted input, whatever its source (#10644 review):
//!
//! - **Bounded reads.** At most [`MAX_READ_PER_FILE`] bytes per file per
//!   tick, lines longer than [`MAX_LINE_BYTES`] are dropped, only the
//!   retained hours' `calls-<hour>.jsonl` are read, a file is opened without
//!   following a symlink and must be a regular file (never a FIFO), and an
//!   unterminated trailing line waits for the next tick.
//! - **Schema and closed vocabularies.** A row is kept only when its `ag` is
//!   a known role, its `caller` is `agent_gh_front` or a name the W5 ledger
//!   can write, its `vi` agrees with that caller and its time is plausible.
//!   Every label is re-derived: an owner must be a GitHub login, an account
//!   one of the shapes the accounting writes; anything else is `unknown`. A
//!   row's request count is capped at [`MAX_ROW_REQUESTS`]. Nothing read
//!   from a row becomes a path or a command.
//! - **Volume.** The agent series have their own cap in `forge_calls`, so a
//!   flood of label sets folds there and never displaces the daemon's own.
//!   The contained directory is swept each tick ([`sweep_contained`]):
//!   symlinks and special files, files not named like a sink file, hour
//!   files outside the retained window and any file over
//!   [`MAX_CONTAINED_FILE_BYTES`] are removed. This bounds what a container
//!   can leave behind between ticks; a hard disk quota needs a filesystem
//!   limit, which this does not claim to be (a contained worker's workspace
//!   mount is read-write anyway).
//! - **No gating.** Ingested rows only feed the `loom.forge.calls` export.
//!   Contained rows never enter `status`, `forge calls` or the bucket book,
//!   which read only the sink's top level, and no breaker or budget decision
//!   reads this module.
//! - **No daemon state in the container's directory.** Cursors live in
//!   memory; the first tick starts every file at its current end (no
//!   replay), a file that shrinks is never re-read, and a vanished file's
//!   cursor is forgotten.
//!
//! One daemon per sink directory is the same assumption `status` makes; two
//! would export the same agent rows twice.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use super::agent::{self, CONTAINED_SUBDIR, PASSTHROUGH_CALLER_PREFIX};
use super::{Outcome, SinkLine, RETAIN_HOURS};
use crate::observability::ops::forge_calls::{self, CallLabels, CallOutcome};

/// Most bytes read from one sink file in one tick (the rest waits).
pub const MAX_READ_PER_FILE: u64 = 2 << 20;
/// Longest sink line accepted; a real row is a few hundred bytes.
pub const MAX_LINE_BYTES: usize = 2048;
/// A contained file larger than this is removed.
pub const MAX_CONTAINED_FILE_BYTES: u64 = 32 << 20;
/// Most requests one row may stand for.
pub const MAX_ROW_REQUESTS: u32 = 1000;
/// Most contained-directory entries examined per tick.
const MAX_DIR_ENTRIES: usize = 1024;
/// How far in the future a row's time may be (clock skew).
const FUTURE_SLACK_SECS: i64 = 300;
/// The label of a value that failed validation.
const UNKNOWN: &str = "unknown";

/// Where a row was read from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// The sink's top level (host-side fronts).
    Host,
    /// `<sink>/contained` (container fronts).
    Contained,
}

/// One validated agent row, ready for [`forge_calls::record_agent`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentCall {
    pub labels: CallLabels,
    /// The agent role (`ag`), from the closed vocabulary.
    pub agent: &'static str,
    /// `served` | `passthrough`, implied by the caller.
    pub via: &'static str,
    pub requests: u64,
    pub source: Source,
}

/// Per-file read positions, kept in memory only.
#[derive(Debug, Default)]
pub struct Cursors {
    primed: bool,
    offsets: BTreeMap<PathBuf, u64>,
}

/// What one tick found.
#[derive(Debug, Default)]
pub struct Drained {
    pub calls: Vec<AgentCall>,
    /// Agent-looking rows refused (bad schema, vocabulary, length or time).
    pub rejected: u64,
    /// Contained-directory entries removed by [`sweep_contained`].
    pub removed: u64,
}

/// Read every agent row appended to `sink` since the previous call with
/// these `cursors`, as of `now` (epoch seconds). The first call only marks
/// where every file ends.
pub fn drain(sink: &Path, cursors: &mut Cursors, now: i64) -> Drained {
    let mut out = Drained::default();
    let contained = sink.join(CONTAINED_SUBDIR);
    let contained_ok = std::fs::symlink_metadata(&contained).is_ok_and(|m| m.is_dir());
    if contained_ok {
        out.removed = sweep_contained(&contained, now.div_euclid(3600));
    }
    let hour = now.div_euclid(3600);
    let mut files = Vec::new();
    for h in hour - RETAIN_HOURS..=hour {
        files.push((super::sink::sink_file(sink, h), Source::Host));
        if contained_ok {
            files.push((super::sink::sink_file(&contained, h), Source::Contained));
        }
    }
    let live: BTreeSet<PathBuf> = files.iter().map(|(p, _)| p.clone()).collect();
    cursors.offsets.retain(|p, _| live.contains(p));
    if !cursors.primed {
        for (path, _) in &files {
            if let Some(len) = open_regular(path).map(|(_, len)| len) {
                cursors.offsets.insert(path.clone(), len);
            }
        }
        cursors.primed = true;
        return out;
    }
    for (path, source) in files {
        let start = cursors.offsets.get(&path).copied().unwrap_or(0);
        let (next, lines, dropped) = read_new(&path, start);
        out.rejected += dropped;
        if next > 0 || cursors.offsets.contains_key(&path) {
            cursors.offsets.insert(path, next);
        }
        for line in lines {
            match parse(&line, source, now) {
                Parsed::Agent(call) => out.calls.push(*call),
                Parsed::Rejected => out.rejected += 1,
                Parsed::NotAgent => {}
            }
        }
    }
    out
}

/// Open `path` for reading without following a symlink or blocking on a
/// FIFO; `Some` only for a regular file, with its length.
fn open_regular(path: &Path) -> Option<(std::fs::File, u64)> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::custom_flags(
        &mut options,
        libc::O_NOFOLLOW | libc::O_NONBLOCK,
    );
    let file = options.open(path).ok()?;
    let meta = file.metadata().ok()?;
    meta.is_file().then_some((file, meta.len()))
}

/// The complete lines of `path` past `start`: `(next start, lines, dropped)`.
fn read_new(path: &Path, start: u64) -> (u64, Vec<String>, u64) {
    let Some((mut file, len)) = open_regular(path) else {
        return (start, Vec::new(), 0);
    };
    if len <= start {
        // Unchanged — or shrunk, which is never re-read.
        return (len, Vec::new(), 0);
    }
    let want = (len - start).min(MAX_READ_PER_FILE);
    let mut buf = Vec::new();
    if file.seek(SeekFrom::Start(start)).is_err() || file.take(want).read_to_end(&mut buf).is_err()
    {
        return (start, Vec::new(), 0);
    }
    let Some(end) = buf.iter().rposition(|b| *b == b'\n') else {
        // One line longer than a whole read is never a row: skip it.
        return if want == MAX_READ_PER_FILE {
            (start + want, Vec::new(), 1)
        } else {
            (start, Vec::new(), 0)
        };
    };
    let mut dropped = 0;
    let lines = buf[..end]
        .split(|b| *b == b'\n')
        .filter(|l| !l.is_empty())
        .filter_map(|l| {
            let kept = (l.len() <= MAX_LINE_BYTES)
                .then(|| std::str::from_utf8(l).ok())
                .flatten();
            dropped += u64::from(kept.is_none());
            kept.map(str::to_string)
        })
        .collect();
    (start + end as u64 + 1, lines, dropped)
}

enum Parsed {
    Agent(Box<AgentCall>),
    Rejected,
    NotAgent,
}

fn parse(line: &str, source: Source, now: i64) -> Parsed {
    let Ok(row) = serde_json::from_str::<SinkLine>(line) else {
        // Not a row at all: in the contained dir that is junk; at the top
        // level the daemon's own lines always parse, so it is junk too.
        return Parsed::Rejected;
    };
    if row.ag.ag.is_none() && row.ag.vi.is_none() {
        return Parsed::NotAgent;
    }
    validate(&row, source, now).map_or(Parsed::Rejected, |c| Parsed::Agent(Box::new(c)))
}

/// The ingest's whole trust decision for one stamped row.
fn validate(row: &SinkLine, source: Source, now: i64) -> Option<AgentCall> {
    let agent = role(row.ag.ag.as_deref()?)?;
    let (caller, via) = caller(&row.c)?;
    if row.ag.vi.as_deref() != Some(via) {
        return None;
    }
    let oldest = now - (RETAIN_HOURS + 1) * 3600;
    if row.t < oldest || row.t > now + FUTURE_SLACK_SECS {
        return None;
    }
    let owner = |o: Option<&str>| {
        o.filter(|o| crate::forge_bucket_book::valid_owner(o))
            .map_or_else(|| UNKNOWN.to_string(), str::to_ascii_lowercase)
    };
    let labels = CallLabels {
        caller: caller.to_string(),
        op: token(row.op.as_deref(), 64),
        role: identity_role(row.ir.as_deref()),
        account: account(row.at.ca.as_deref()),
        cred_owner: owner(row.at.co.as_deref()),
        target_owner: owner(
            row.rp
                .as_deref()
                .map(crate::credential_preflight::owner_of_nwo),
        ),
        resource: row
            .at
            .rr
            .as_deref()
            .filter(|r| is_token(r, 32))
            .unwrap_or(row.p.as_str())
            .to_string(),
        outcome: match row.o {
            Outcome::Ok => CallOutcome::Ok,
            Outcome::NotModified => CallOutcome::NotModified,
            Outcome::RateLimited => CallOutcome::RateLimited,
            Outcome::Error => CallOutcome::Error,
            Outcome::Shed => CallOutcome::Shed,
        },
    };
    Some(AgentCall {
        labels,
        agent,
        via,
        requests: u64::from(row.at.pg.unwrap_or(1).clamp(1, MAX_ROW_REQUESTS)),
        source,
    })
}

/// `ag`, back to its `&'static` vocabulary entry.
fn role(ag: &str) -> Option<&'static str> {
    agent::ROLES
        .iter()
        .chain(&["other", "none"])
        .find(|r| **r == ag)
        .copied()
}

/// `(caller, via)` for a caller the front can write.
fn caller(c: &str) -> Option<(&'static str, &'static str)> {
    if c == crate::agent_gh::STATS_CALLER {
        return Some((crate::agent_gh::STATS_CALLER, "served"));
    }
    let command = c.strip_prefix(PASSTHROUGH_CALLER_PREFIX)?;
    crate::agent_gh::ledger::caller_of(command)
        .filter(|known| *known == c)
        .map(|known| (known, "passthrough"))
}

/// The identity role: the facade's `reader` / `writer` / `writer-fallback`,
/// or W5's `agent-<role>` (an unknown role is `agent-other`).
fn identity_role(ir: Option<&str>) -> String {
    match ir {
        Some(r @ ("reader" | "writer" | "writer-fallback")) => r.to_string(),
        Some(r) => match r.strip_prefix("agent-") {
            Some(rest) if rest == "session" || agent::ROLES.contains(&rest) => r.to_string(),
            Some(_) => "agent-other".to_string(),
            None => UNKNOWN.to_string(),
        },
        None => UNKNOWN.to_string(),
    }
}

/// The account shapes the accounting writes: `app-<digits>`, `app-unknown`,
/// `env-token`, `ambient`.
fn account(ca: Option<&str>) -> String {
    let ok = ca.is_some_and(|a| {
        matches!(a, "app-unknown" | "env-token" | "ambient")
            || a.strip_prefix("app-").is_some_and(|id| {
                (1..=20).contains(&id.len()) && id.bytes().all(|b| b.is_ascii_digit())
            })
    });
    ca.filter(|_| ok).unwrap_or(UNKNOWN).to_string()
}

/// A lowercase `[a-z0-9._-]` token of at most `max` bytes.
fn is_token(v: &str, max: usize) -> bool {
    (1..=max).contains(&v.len())
        && v.bytes().all(|b| {
            b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'.' | b'_' | b'-')
        })
}

fn token(v: Option<&str>, max: usize) -> String {
    v.filter(|v| is_token(v, max))
        .unwrap_or(UNKNOWN)
        .to_string()
}

/// Bound what a container leaves in `dir` (the contained sink): remove
/// symlinks and special files, files not named like a sink file, hour files
/// outside the retained window, and any file over
/// [`MAX_CONTAINED_FILE_BYTES`]. Never follows a link and never recurses (a
/// subdirectory is left alone); returns how many entries it removed.
pub fn sweep_contained(dir: &Path, hour: i64) -> u64 {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    let mut removed = 0;
    for entry in entries.flatten().take(MAX_DIR_ENTRIES) {
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        if kind.is_dir() {
            continue;
        }
        let name = entry.file_name();
        let foreign = !name.to_str().is_some_and(agent::is_sink_entry);
        let stale = name
            .to_str()
            .and_then(|n| n.strip_prefix("calls-")?.strip_suffix(".jsonl"))
            .and_then(|h| h.parse::<i64>().ok())
            .is_some_and(|h| h < hour - RETAIN_HOURS || h > hour + 1);
        let oversize = kind.is_file()
            && entry
                .metadata()
                .is_ok_and(|m| m.len() > MAX_CONTAINED_FILE_BYTES);
        let doomed = !kind.is_file() || foreign || stale || oversize;
        if doomed && std::fs::remove_file(entry.path()).is_ok() {
            removed += 1;
        }
    }
    removed
}

/// One export tick: drain this host's sink and record every new agent row
/// into `loom.forge.calls`. Returns how many rows were recorded. Local file
/// I/O only — never a forge call; a missing or disabled sink does nothing.
pub fn tick(now: i64) -> usize {
    static CURSORS: Mutex<Cursors> = Mutex::new(Cursors {
        primed: false,
        offsets: BTreeMap::new(),
    });
    let Some(sink) = super::host_sink_dir() else {
        return 0;
    };
    let mut cursors = CURSORS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    record(drain(&sink, &mut cursors, now))
}

/// Record a [`Drained`] batch (and its refusal counters).
fn record(drained: Drained) -> usize {
    super::counters::add("agent_ingest.rejected", drained.rejected);
    super::counters::add("agent_ingest.contained_removed", drained.removed);
    let n = drained.calls.len();
    for call in drained.calls {
        forge_calls::record_agent(call.labels, call.agent, call.requests);
    }
    n
}

#[cfg(test)]
#[path = "forge_call_stats_ingest_tests.rs"]
mod tests;
