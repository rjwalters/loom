//! Concern verification for J1 (§10.2 step 4): rebuild every execution
//! projection from scratch and diff it against the legacy stores it shadows.
//!
//! **Read-only.** It reads the journal and the legacy files, and probes PIDs
//! with signal 0. It never writes a file, edits a label or claim, or signals
//! a process. Only the stores the journal names are compared, so a store
//! that was never journaled is not reported.
//!
//! Explained differences (reported, not failing):
//!
//! - `dead_pid` — E1/E2 entries whose recorded PID is dead on both sides or
//!   pruned from the legacy side (the dead-PID prune E1 applies on read);
//! - `unjournaled` — a legacy key the journal never named (written before
//!   shadowing was enabled, or by a producer outside 2b);
//! - `no_base` — an E2 owner whose journal history starts with a patch;
//! - `retention` — a key the bounded legacy store has since dropped
//!   (E8 rotation, E9 24 h horizon, E10 1000-event cap, E16 log rotation);
//! - `legacy_corrupt` — an unparseable legacy file (E2's known torn write).

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use chrono::Utc;
use serde::Serialize;
use serde_json::Value;

use super::projection::{diff_fields, normalize, Entry, Projection};
use super::{scrub, Journal, Store, LAUNCH_ITEM, STREAM};

/// E9's floor window (`prless_retry::durable::NO_PHASE_STREAK_WINDOW_SECS`).
const PRLESS_HORIZON_SECS: i64 = 86_400;
/// E10's cap (`validate_phase::MAX_RECOVERY_EVENTS`).
const RECOVERY_CAP: usize = 1000;

/// One unexplained journal-vs-legacy difference.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct UnexplainedDivergence {
    pub store: String,
    pub path: String,
    pub item: String,
    pub fields: Vec<String>,
}

/// Per legacy store file.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct StoreSummary {
    pub store: String,
    pub path: String,
    pub compared: u64,
    pub matched: u64,
    pub unexplained: u64,
    /// Explained differences by reason.
    pub explained: BTreeMap<String, u64>,
}

/// The concern report. `ok` only if the journal was readable and no
/// unexplained divergence was found.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct ConcernReport {
    pub concern: String,
    pub root: PathBuf,
    pub ok: bool,
    pub records: u64,
    pub corrupt_lines: u64,
    pub unknown_major: u64,
    pub torn_tail: bool,
    /// Durable `journal.divergence` records found (write-time divergences).
    pub recorded_divergences: u64,
    pub stores: Vec<StoreSummary>,
    pub unexplained: Vec<UnexplainedDivergence>,
    pub errors: Vec<String>,
}

/// A legacy store read into `item -> value`.
struct Legacy {
    items: HashMap<String, Value>,
    /// The file could not be parsed at all.
    corrupt: bool,
    /// The store dropped entries for capacity (E10 at its cap).
    at_capacity: bool,
}

impl Legacy {
    fn empty() -> Self {
        Legacy {
            items: HashMap::new(),
            corrupt: false,
            at_capacity: false,
        }
    }
}

/// Verify the execution concern under journal root `root`. Never writes.
#[must_use]
pub fn verify_concern(root: &Path) -> ConcernReport {
    let mut report = ConcernReport {
        concern: STREAM.to_owned(),
        root: root.to_path_buf(),
        ..ConcernReport::default()
    };
    let stream = match Journal::open(root).stream(STREAM) {
        Ok(stream) => stream,
        Err(e) => {
            report.errors.push(e.to_string());
            return report;
        }
    };
    if !stream.dir().is_dir() {
        report
            .errors
            .push(format!("no execution journal at {}", stream.dir().display()));
        return report;
    }
    let (projection, stats) = match Projection::rebuild(&stream) {
        Ok(rebuilt) => rebuilt,
        Err(e) => {
            report.errors.push(format!("cannot read journal: {e}"));
            return report;
        }
    };
    report.records = stats.records;
    report.corrupt_lines = stats.corrupt_lines;
    report.unknown_major = stats.unknown_major;
    report.torn_tail = stats.torn_tail;
    report.recorded_divergences = projection.divergence_records;

    let mut by_file: BTreeMap<(Store, String), Vec<(&str, &Entry)>> = BTreeMap::new();
    for (store, path, item, entry) in projection.entries() {
        by_file
            .entry((store, path.to_owned()))
            .or_default()
            .push((item, entry));
    }
    for ((store, path), items) in by_file {
        let legacy = load_legacy(store, Path::new(&path));
        let summary = compare_store(store, &path, &items, &legacy, &mut report.unexplained);
        report.stores.push(summary);
    }
    report.ok = report.errors.is_empty() && report.unexplained.is_empty();
    report
}

fn compare_store(
    store: Store,
    path: &str,
    items: &[(&str, &Entry)],
    legacy: &Legacy,
    unexplained: &mut Vec<UnexplainedDivergence>,
) -> StoreSummary {
    let mut summary = StoreSummary {
        store: store.as_str().to_owned(),
        path: path.to_owned(),
        ..StoreSummary::default()
    };
    let mut explain = |reason: &str| *summary.explained.entry(reason.to_owned()).or_default() += 1;
    let mut journaled = std::collections::HashSet::new();
    let mut found = Vec::new();
    for (item, entry) in items {
        journaled.insert(*item);
        let journal = entry.value.as_ref().map(normalize).filter(|v| !v.is_null());
        let legacy_value = legacy.items.get(*item).map(normalize);
        if journal == legacy_value {
            found.push(true);
            continue;
        }
        found.push(false);
        if legacy.corrupt {
            explain("legacy_corrupt");
        } else if !entry.based {
            explain("no_base");
        } else if let Some(reason) =
            explained(store, journal.as_ref(), legacy_value.as_ref(), legacy)
        {
            explain(reason);
        } else {
            unexplained.push(UnexplainedDivergence {
                store: store.as_str().to_owned(),
                path: path.to_owned(),
                item: (*item).to_owned(),
                fields: diff_fields(legacy_value.as_ref(), journal.as_ref()),
            });
        }
    }
    for key in legacy.items.keys() {
        if !journaled.contains(key.as_str()) {
            explain("unjournaled");
        }
    }
    summary.compared = found.len() as u64;
    summary.matched = found.iter().filter(|m| **m).count() as u64;
    summary.unexplained = summary.compared - summary.matched - summary.explained_compared();
    summary
}

impl StoreSummary {
    fn explained_compared(&self) -> u64 {
        self.explained
            .iter()
            .filter(|(reason, _)| reason.as_str() != "unjournaled")
            .map(|(_, n)| *n)
            .sum()
    }
}

/// The documented reason a journal/legacy difference is expected, if any.
fn explained(
    store: Store,
    journal: Option<&Value>,
    legacy: Option<&Value>,
    legacy_store: &Legacy,
) -> Option<&'static str> {
    match store {
        Store::Sweeps | Store::Owner => {
            let pid_field = if store == Store::Sweeps {
                "pid"
            } else {
                "owner_pid"
            };
            let dead = |v: Option<&Value>| {
                v.and_then(|v| v.get(pid_field))
                    .and_then(Value::as_u64)
                    .and_then(|p| u32::try_from(p).ok())
                    .is_none_or(|p| !crate::sweep_registry::is_pid_alive(p))
            };
            // The §10.2 prune: a side whose PID is dead counts as absent.
            (dead(journal) && dead(legacy)).then_some("dead_pid")
        }
        Store::Outcomes | Store::Launch => (legacy.is_none()).then_some("retention"),
        Store::Recovery => (legacy.is_none() && legacy_store.at_capacity).then_some("retention"),
        Store::PrlessClears | Store::PrlessCounted => {
            let expired = journal
                .and_then(Value::as_str)
                .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
                .is_some_and(|t| {
                    (Utc::now() - t.with_timezone(&Utc)).num_seconds() > PRLESS_HORIZON_SECS
                });
            (legacy.is_none() && expired).then_some("retention")
        }
    }
}

fn read_json(path: &Path) -> Result<Option<Value>, ()> {
    match std::fs::read_to_string(path) {
        Ok(text) if text.trim().is_empty() => Ok(None),
        Ok(text) => serde_json::from_str(&text).map(Some).map_err(|_| ()),
        Err(_) => Ok(None),
    }
}

fn load_legacy(store: Store, path: &Path) -> Legacy {
    let mut legacy = Legacy::empty();
    match store {
        Store::Sweeps => {
            for e in crate::sweep_journal::load(path).entries {
                legacy.items.insert(
                    super::sweeps_item(&e.repo, e.issue),
                    serde_json::json!({"pid": e.pid, "started_at": e.started_at}),
                );
            }
        }
        Store::Owner => match read_json(path) {
            Ok(Some(owner)) => {
                legacy.items.insert("owner".to_owned(), owner);
            }
            Ok(None) => {}
            Err(()) => legacy.corrupt = true,
        },
        Store::Outcomes => {
            let rotated = PathBuf::from(format!("{}.1", path.display()));
            let records = crate::sweep_outcomes::read_all(&rotated)
                .into_iter()
                .chain(crate::sweep_outcomes::read_all(path));
            for record in records {
                if let Ok(value) = serde_json::to_value(&record) {
                    legacy.items.insert(record.sweep_id.clone(), value);
                }
            }
        }
        Store::PrlessClears | Store::PrlessCounted => match read_json(path) {
            Ok(Some(Value::Object(map))) => legacy.items.extend(map),
            Ok(_) => {}
            Err(()) => legacy.corrupt = true,
        },
        Store::Recovery => match read_json(path) {
            Ok(Some(Value::Array(events))) => {
                legacy.at_capacity = events.len() >= RECOVERY_CAP;
                for event in events {
                    legacy.items.insert(super::recovery_item(&event), event);
                }
            }
            Ok(_) => {}
            Err(()) => legacy.corrupt = true,
        },
        Store::Launch => {
            if let Ok(text) = std::fs::read_to_string(path) {
                let last = text
                    .lines()
                    .rev()
                    .find_map(|l| l.strip_prefix(crate::launch_record::LAUNCH_RECORD_MARKER));
                if let Some(json) = last {
                    let credential = crate::launch_record::parse_launch_credential(json)
                        .and_then(|c| serde_json::to_value(c).ok());
                    if let Some(credential) = credential {
                        legacy.items.insert(LAUNCH_ITEM.to_owned(), credential);
                    }
                }
            }
        }
    }
    for value in legacy.items.values_mut() {
        scrub(value);
    }
    legacy
}
