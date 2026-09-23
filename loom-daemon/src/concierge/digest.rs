//! The periodic room digest: "here is what the daemon has been doing"
//! (Issue #8762, Phase 4 of #4196).
//!
//! # The gap this closes
//!
//! Through Phase 3b the concierge persona was *purely reactive* — it spoke only
//! when an allowlisted human addressed it. An operator who closed their laptop
//! and came back had no room-side answer to "what happened while I was away";
//! the daemon's own narration sink (`crate::safehouse`) reports individual
//! sweep events as they happen, which is a different thing from a summary and
//! is useless if you were not watching.
//!
//! # Why the content is deterministic, not LLM-written
//!
//! A digest is a report on state the daemon already holds: the sweep journal
//! (`~/.loom/sweeps.json`) and the watch registry (`~/.loom/watches.json`).
//! Rendering it with a model would spend a session to restate two JSON files,
//! and — worse — would make the *content* of a room message a probabilistic
//! function of untrusted inputs. [`render`] is a pure function of [`Facts`],
//! so what the room sees is auditable from what the files said.
//!
//! No forge call, ever. Both sources are local files the daemon owns, which is
//! what makes a 5-minute cadence affordable (cf.
//! `crate::watch_registry`'s own "zero forge calls when no watches are
//! registered" property).
//!
//! # Why a quiet fleet stays quiet
//!
//! A cadence-driven digest with nothing new to say is the room-spam failure
//! mode. Suppression is by **fingerprint of the facts' identity**
//! ([`fingerprint`]), not of the rendered line: ages and elapsed minutes change
//! every tick, so a body-level comparison would never match and every tick
//! would post. Two ticks that see the same issues in flight and the same
//! watches registered therefore produce exactly one digest, however far apart
//! they are — and the moment a sweep starts or a watch resolves, the next tick
//! speaks.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Suppression state, relative to a workspace root. Sits beside
/// [`super::budget::LEDGER_REL`] in the same gitignored, machine-local,
/// disposable directory — deleting it costs at most one duplicate digest.
pub const STATE_REL: &str = ".loom/concierge/digest.json";

/// How many items of each kind are named in the line before it collapses to a
/// count. A room message is read by a human on a phone; an unbounded list of
/// 40 in-flight sweeps is not a summary.
const NAMED_LIMIT: usize = 5;

/// The prefix every digest line opens with.
///
/// Chosen so it cannot be read as a mention of any plausible daemon persona —
/// but note that is a readability nicety, not the safety property. The safety
/// property is that [`super::room::emit`] runs `vet_say` on the finished body
/// and refuses it if 3a would hear it as addressed, whatever this prefix says.
const PREFIX: &str = "daemon digest";

/// One in-flight sweep, as the journal recorded it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SweepFact {
    /// The issue the sweep is working.
    pub issue: u32,
    /// Short label for the workspace (the final path component), because a room
    /// line has no room for an absolute path and an operator does not need one.
    pub workspace: String,
    /// Whole minutes since the sweep was spawned, floored at 0.
    pub age_minutes: i64,
}

/// Everything one digest reports. Pure data, so [`render`] and
/// [`fingerprint`] are both testable without a daemon, a journal, or a home
/// directory.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Facts {
    /// In-flight sweeps, sorted by issue number.
    pub sweeps: Vec<SweepFact>,
    /// Registered watch target labels (`WatchSpec::target_label`), sorted.
    pub watches: Vec<String>,
}

impl Facts {
    /// Whether there is nothing at all to report.
    #[must_use]
    pub fn is_idle(&self) -> bool {
        self.sweeps.is_empty() && self.watches.is_empty()
    }
}

/// Read the sweep journal and the watch registry into [`Facts`].
///
/// Best-effort throughout, matching both sources' own philosophy: a path that
/// cannot be resolved (no home directory) or a file that cannot be read
/// contributes nothing rather than failing the digest. A digest that
/// under-reports is a worse digest; a digest that errors is no digest.
#[must_use]
pub fn collect(now: DateTime<Utc>) -> Facts {
    let sweeps = crate::sweep_journal::default_journal_path()
        .ok()
        .map(|path| crate::sweep_journal::load(&path))
        .map(|journal| {
            let mut facts: Vec<SweepFact> = journal
                .entries
                .iter()
                .map(|entry| SweepFact {
                    issue: entry.issue,
                    workspace: short_workspace(&entry.repo),
                    age_minutes: now
                        .signed_duration_since(entry.started_at)
                        .num_minutes()
                        .max(0),
                })
                .collect();
            facts.sort_by_key(|fact| (fact.issue, fact.workspace.clone()));
            facts
        })
        .unwrap_or_default();
    let watches = crate::watch_registry::default_watches_path()
        .ok()
        .map(|path| crate::watch_registry::load(&path))
        .map(|registry| {
            let mut labels: Vec<String> = registry
                .watches
                .iter()
                .map(|spec| super::room::one_line(&spec.target_label()))
                .collect();
            labels.sort();
            labels
        })
        .unwrap_or_default();
    Facts { sweeps, watches }
}

/// The final path component of a workspace root, or the whole string when it
/// has none. Sweep journal entries key on
/// `workspace_root.display().to_string()`, which is an absolute path.
fn short_workspace(repo: &str) -> String {
    let trimmed = repo.trim_end_matches('/');
    let name = Path::new(trimmed)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(trimmed);
    super::room::one_line(name)
}

/// Render one digest line. Pure, single-line, and bounded.
#[must_use]
pub fn render(facts: &Facts) -> String {
    if facts.is_idle() {
        return format!("{PREFIX} — nothing in flight, no watches registered");
    }
    let mut line = String::from(PREFIX);
    line.push_str(" — ");
    let mut parts: Vec<String> = Vec::new();
    if facts.sweeps.is_empty() {
        parts.push("nothing in flight".to_owned());
    } else {
        let mut part = format!("{} in flight: ", plural(facts.sweeps.len(), "sweep"));
        let named: Vec<String> = facts
            .sweeps
            .iter()
            .take(NAMED_LIMIT)
            .map(|fact| format!("#{} in {} ({}m)", fact.issue, fact.workspace, fact.age_minutes))
            .collect();
        part.push_str(&named.join(", "));
        append_overflow(&mut part, facts.sweeps.len());
        parts.push(part);
    }
    if facts.watches.is_empty() {
        parts.push("no watches registered".to_owned());
    } else {
        let mut part = format!("{} registered: ", plural(facts.watches.len(), "watch"));
        part.push_str(
            &facts
                .watches
                .iter()
                .take(NAMED_LIMIT)
                .cloned()
                .collect::<Vec<_>>()
                .join(", "),
        );
        append_overflow(&mut part, facts.watches.len());
        parts.push(part);
    }
    line.push_str(&parts.join("; "));
    super::room::one_line(&line)
}

fn append_overflow(part: &mut String, total: usize) {
    if total > NAMED_LIMIT {
        let _ = write!(part, " (+{} more)", total - NAMED_LIMIT);
    }
}

/// `1 sweep` / `2 sweeps`, `1 watch` / `2 watches`.
fn plural(n: usize, noun: &str) -> String {
    if n == 1 {
        format!("{n} {noun}")
    } else if noun.ends_with("ch") {
        format!("{n} {noun}es")
    } else {
        format!("{n} {noun}s")
    }
}

/// A stable fingerprint of *what* the digest is about, excluding anything that
/// changes on its own (ages, counts derived from ages).
///
/// This is the suppression key. Deliberately **not** a hash of [`render`]'s
/// output: that string embeds elapsed minutes, so every tick would differ and
/// the digest would post on every tick forever — the exact room-spam failure
/// this exists to prevent.
///
/// FNV-1a rather than `DefaultHasher` because the value is persisted: a hash
/// whose algorithm may change between Rust releases would silently re-post one
/// digest after every toolchain bump. FNV-1a is fixed forever, and the file is
/// machine-local and disposable in any case.
#[must_use]
pub fn fingerprint(facts: &Facts) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    let mut feed = |bytes: &[u8]| {
        for byte in bytes {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
    };
    for fact in &facts.sweeps {
        feed(b"s\x00");
        feed(fact.issue.to_string().as_bytes());
        feed(b"\x00");
        feed(fact.workspace.as_bytes());
        feed(b"\x00");
    }
    for label in &facts.watches {
        feed(b"w\x00");
        feed(label.as_bytes());
        feed(b"\x00");
    }
    format!("{hash:016x}")
}

/// Persisted suppression state.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct State {
    /// [`fingerprint`] of the last digest actually sent.
    pub last_fingerprint: String,
    /// When it was sent (operator-facing only; nothing branches on it).
    pub last_sent_at: Option<DateTime<Utc>>,
}

/// State for a workspace root.
#[must_use]
pub fn state_path(repo_root: &Path) -> PathBuf {
    repo_root.join(STATE_REL)
}

/// Read the state. A missing, unreadable or malformed file is "no digest has
/// been sent", which costs at most one duplicate line.
#[must_use]
pub fn read_state(path: &Path) -> State {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default()
}

/// Record a sent digest.
///
/// # Errors
///
/// Any filesystem error. Unlike the budget ledger this is **not** a
/// refusal-worthy condition for the caller to abort on — the digest has already
/// been sent by the time this runs, and the only consequence of a failed write
/// is a repeated digest next tick. Callers log it.
pub fn write_state(path: &Path, state: &State) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let body = serde_json::to_string_pretty(state)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, body)?;
    std::fs::rename(&tmp, path)
}
