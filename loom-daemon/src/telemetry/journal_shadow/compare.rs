//! Normalized shadow comparison: one J2 stream against its legacy store(s)
//! (`docs/design/persistence-inventory.md` §10.2).
//!
//! Both sides are reduced to a **fingerprint** of the normalized record:
//! object keys sorted, `null`-valued keys dropped (absent == `null`), and any
//! RFC 3339 string compared at whole seconds in UTC. Journal-added fields
//! (`seq`, `ts`, `writer`, segment metadata) live in the journal envelope,
//! not in `data`, so only `data` is compared.
//!
//! Matching is a multiset match. A record on one side only is
//! - *explained* when it precedes the first record of its side that matched
//!   at all: a legacy record from before shadowing was enabled, or a journal
//!   record whose legacy copy was already rotated/pruned away;
//! - *unexplained* otherwise — a divergence.
//!
//! When both sides are non-empty and **nothing** matches, every one-sided
//! record is unexplained: there is no false green from two disjoint sets.
//! Reports carry counts, kinds, natural keys and fingerprints — never record
//! content, so a credential that leaked into a legacy line cannot be copied
//! into a report.

use std::collections::HashMap;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use serde::Serialize;
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

use super::TelemetryStream;
use crate::journal::{Journal, Position};

/// Divergence samples kept per stream report.
pub const MAX_SAMPLES: usize = 5;

/// One one-sided record, identified without its content.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DivergenceSample {
    /// `legacy_only` or `journal_only`.
    pub side: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    pub fingerprint: String,
}

/// The comparison of one stream.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct StreamComparison {
    pub stream: String,
    /// Legacy file names read (rotated `.1` backups included when present).
    pub legacy_files: Vec<String>,
    pub legacy_records: u64,
    /// Legacy lines that are not JSON. Legacy readers skip them too, so they
    /// are reported but are not a divergence.
    pub legacy_unparseable: u64,
    pub journal_records: u64,
    pub matched: u64,
    pub legacy_only_predating_shadow: u64,
    pub legacy_only_unexplained: u64,
    pub journal_only_predating_legacy: u64,
    pub journal_only_unexplained: u64,
    pub samples: Vec<DivergenceSample>,
    pub errors: Vec<String>,
}

impl StreamComparison {
    #[must_use]
    pub fn is_clean(&self) -> bool {
        self.errors.is_empty()
            && self.legacy_only_unexplained == 0
            && self.journal_only_unexplained == 0
    }
}

/// The legacy files backing `stream` in `workspace`, oldest first: each
/// file's rotated `.1` backup precedes it.
#[must_use]
pub fn legacy_sources(stream: TelemetryStream, workspace: &Path) -> Vec<Vec<PathBuf>> {
    let logs = workspace.join(".loom").join("logs");
    let env_or = |env: &str, default: PathBuf| {
        std::env::var(env)
            .ok()
            .filter(|v| !v.is_empty())
            .map_or(default, PathBuf::from)
    };
    let files = match stream {
        TelemetryStream::Sweep => {
            vec![crate::sweep_outcomes::default_outcome_telemetry_path(
                workspace,
            )]
        }
        TelemetryStream::RoleTick => {
            vec![crate::sweep_outcomes::default_role_tick_telemetry_path(
                workspace,
            )]
        }
        TelemetryStream::Rework => vec![crate::rework_events::path(workspace)],
        TelemetryStream::MergeQueue => {
            vec![workspace.join(crate::forge_merge_queue::events::LOG_RELATIVE_PATH)]
        }
        TelemetryStream::Shell => vec![
            env_or("LOOM_GUIDE_DOCS_TELEMETRY_LOG", logs.join("guide-docs-telemetry.jsonl")),
            env_or(
                "LOOM_MERGE_ADMISSION_TELEMETRY_LOG",
                logs.join("merge-admission-telemetry.jsonl"),
            ),
        ],
        TelemetryStream::Experiment => {
            vec![workspace.join(crate::script_helpers::sweep_experiment::DEFAULT_STATS_FILE)]
        }
    };
    files
        .into_iter()
        .map(|file| {
            let name = file
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            vec![file.with_file_name(format!("{name}.1")), file]
        })
        .collect()
}

/// Normalize a record for comparison (see the module docs).
#[must_use]
pub fn normalize(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut keys: Vec<&String> = map
                .iter()
                .filter(|(_, v)| !v.is_null())
                .map(|(k, _)| k)
                .collect();
            keys.sort();
            let mut out = Map::new();
            for key in keys {
                out.insert(key.clone(), normalize(&map[key]));
            }
            Value::Object(out)
        }
        Value::Array(items) => Value::Array(items.iter().map(normalize).collect()),
        Value::String(s) => match chrono::DateTime::parse_from_rfc3339(s) {
            Ok(ts) => Value::String(
                ts.with_timezone(&chrono::Utc)
                    .format("%Y-%m-%dT%H:%M:%SZ")
                    .to_string(),
            ),
            Err(_) => value.clone(),
        },
        other => other.clone(),
    }
}

/// The fingerprint of a normalized record: 16 hex chars of its SHA-256.
#[must_use]
pub fn fingerprint(value: &Value) -> String {
    let bytes = serde_json::to_vec(&normalize(value)).unwrap_or_default();
    let digest = Sha256::digest(&bytes);
    hex::encode(digest)[..16].to_owned()
}

fn record_kind(value: &Value) -> Option<String> {
    value
        .pointer("/record/kind")
        .or_else(|| value.get("kind"))
        .and_then(Value::as_str)
        .map(str::to_owned)
}

struct Item {
    fingerprint: String,
    kind: Option<String>,
    key: Option<String>,
}

/// Compare `stream` of the journal at `root` with its legacy store in
/// `workspace`. Also returns the position just past the last journal record
/// compared (for an acknowledging cursor). Never writes.
#[must_use]
pub fn compare_stream(
    root: &Path,
    workspace: &Path,
    stream: TelemetryStream,
) -> (StreamComparison, Option<Position>) {
    let mut report = StreamComparison {
        stream: stream.name().to_owned(),
        ..StreamComparison::default()
    };
    let (journal, end) = read_journal(root, stream, &mut report);
    let mut sources = Vec::new();
    for group in legacy_sources(stream, workspace) {
        let mut items = Vec::new();
        for file in group {
            read_legacy(&file, &mut items, &mut report);
        }
        sources.push(items);
    }
    report.journal_records = journal.len() as u64;
    report.legacy_records = sources.iter().map(|s| s.len() as u64).sum();
    classify(&journal, &sources, &mut report);
    (report, end)
}

fn read_journal(
    root: &Path,
    stream: TelemetryStream,
    report: &mut StreamComparison,
) -> (Vec<Item>, Option<Position>) {
    let mut items = Vec::new();
    let mut end = None;
    let handle = match Journal::open(root).stream(stream.name()) {
        Ok(handle) => handle,
        Err(e) => {
            report.errors.push(e.to_string());
            return (items, end);
        }
    };
    for record in handle.reader(Position::START) {
        match record {
            Ok(record) => {
                items.push(Item {
                    fingerprint: fingerprint(&record.envelope.data),
                    kind: Some(record.envelope.kind.clone()),
                    key: record.envelope.key.clone(),
                });
                end = Some(record.next);
            }
            Err(e) => {
                report.errors.push(format!("reading journal stream: {e}"));
                break;
            }
        }
    }
    (items, end)
}

fn read_legacy(file: &Path, items: &mut Vec<Item>, report: &mut StreamComparison) {
    let handle = match std::fs::File::open(file) {
        Ok(handle) => handle,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return,
        Err(e) => {
            report
                .errors
                .push(format!("cannot read legacy {}: {e}", file.display()));
            return;
        }
    };
    if let Some(name) = file.file_name() {
        report
            .legacy_files
            .push(name.to_string_lossy().into_owned());
    }
    for line in BufReader::new(handle).lines() {
        let line = match line {
            Ok(line) => line,
            Err(e) => {
                report
                    .errors
                    .push(format!("cannot read legacy {}: {e}", file.display()));
                return;
            }
        };
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<Value>(&line) {
            Ok(value) => items.push(Item {
                fingerprint: fingerprint(&value),
                kind: record_kind(&value),
                key: None,
            }),
            Err(_) => report.legacy_unparseable += 1,
        }
    }
}

fn counts<'a>(items: impl Iterator<Item = &'a Item>) -> HashMap<&'a str, u64> {
    let mut map = HashMap::new();
    for item in items {
        *map.entry(item.fingerprint.as_str()).or_insert(0) += 1;
    }
    map
}

fn sample(report: &mut StreamComparison, side: &'static str, item: &Item) {
    if report.samples.len() < MAX_SAMPLES {
        report.samples.push(DivergenceSample {
            side,
            kind: item.kind.clone(),
            key: item.key.clone(),
            fingerprint: item.fingerprint.clone(),
        });
    }
}

fn classify(journal: &[Item], sources: &[Vec<Item>], report: &mut StreamComparison) {
    let journal_counts = counts(journal.iter());
    let legacy_counts = counts(sources.iter().flatten());
    let matched: u64 = journal_counts
        .iter()
        .map(|(fp, n)| (*n).min(legacy_counts.get(fp).copied().unwrap_or(0)))
        .sum();
    report.matched = matched;
    // Two non-empty, disjoint sides explain nothing.
    let disjoint = matched == 0;

    let mut remaining = journal_counts.clone();
    for source in sources {
        let mut seen_match = false;
        for item in source {
            match remaining.get_mut(item.fingerprint.as_str()) {
                Some(n) if *n > 0 => {
                    *n -= 1;
                    seen_match = true;
                }
                _ if !seen_match && (!disjoint || journal.is_empty()) => {
                    report.legacy_only_predating_shadow += 1;
                }
                _ => {
                    report.legacy_only_unexplained += 1;
                    sample(report, "legacy_only", item);
                }
            }
        }
    }

    let mut remaining = legacy_counts;
    let mut seen_match = false;
    for item in journal {
        match remaining.get_mut(item.fingerprint.as_str()) {
            Some(n) if *n > 0 => {
                *n -= 1;
                seen_match = true;
            }
            _ if !seen_match && !disjoint => report.journal_only_predating_legacy += 1,
            _ => {
                report.journal_only_unexplained += 1;
                sample(report, "journal_only", item);
            }
        }
    }
}
