//! The `webhook-mirror` source of the raw event cache (#10197, PR 2).
//!
//! The loom-ui webhook files one `label.transition` row per `loom:*` label
//! change (and per `opened` / `closed` / `reopened` of an item carrying one)
//! into its D1 `records` table. That table is a fleet-wide label timeline
//! reaching back months before the forge cache's window — but D1 evicts at its
//! row cap (2AMLogic/loom-ui#1464), so an export of it, or a local mirror, is
//! the durable copy. This module imports such an export into the same
//! append-only events file the forge sources write ([`super::fleet_events`]).
//!
//! # Contract
//!
//! - **Read-only.** The export is only read; nothing here talks to D1, the
//!   forge or the network.
//! - **Append-only and idempotent.** Each mapped row is a [`RawEvent`] whose
//!   id is derived from its content (`source`, `seq`, item, kind, label,
//!   event time) — never from the import time — and
//!   [`super::fleet_events::EventLog::append`] writes only ids not already
//!   held. Re-importing the same export, or a
//!   later export that overlaps it (D1 having evicted the oldest rows), adds
//!   only the rows not yet cached and never removes one.
//! - **`source` and `fetched_at` apart from `event_time`.** `source` is
//!   [`SOURCE_WEBHOOK_MIRROR`]; `fetched_at` is the import instant;
//!   `event_time` is the row's own `at` (the Worker's receipt time, falling
//!   back to the `emitted_at` column). Receipt follows the forge change by
//!   seconds, so `event_time` is never *earlier* than the change became
//!   knowable: replaying it is leak-free.
//! - **Deterministic tie-break.** `seq` is the D1 `records.id`
//!   (`AUTOINCREMENT`, so delivery order), which orders same-second rows in
//!   the canonical `(event_time, source, seq, id)` key.
//!
//! # Accepted input
//!
//! Either JSONL (one `records` row per line) or one JSON document as
//! `wrangler d1 execute --json` prints it (`[{"results": [rows…]}]`) or a
//! bare array of rows. A row needs `id` (integer) and a `payload` — the
//! `label.transition` record, as an object or as the JSON string D1 stores.
//! `kind` and `repo` are read from the row, else from the payload. Rows of
//! another kind or repo, and rows that do not parse, are counted and skipped.
//!
//! `labels_after` (the full `loom:*` set after the change) is not imported:
//! every transition is already its own row, and the forge rows carry
//! non-`loom:*` labels the webhook never sees.

use chrono::{DateTime, Utc};
use serde_json::Value;
use std::collections::BTreeMap;

use super::fleet_events::{EventKind, ItemKind, RawEvent};

/// `source` of a row imported from a webhook `label.transition` export.
pub const SOURCE_WEBHOOK_MIRROR: &str = "webhook-mirror";

/// The D1 `records.kind` this importer reads.
const TRANSITION_KIND: &str = "label.transition";

/// What one parse of an export found.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MirrorParse {
    /// Mapped rows, per `owner/repo` (lower-cased).
    pub by_repo: BTreeMap<String, Vec<RawEvent>>,
    /// Rows whose `kind` is not `label.transition`.
    pub other_kind: usize,
    /// Rows that did not parse or lacked a required field.
    pub unparseable: usize,
}

impl MirrorParse {
    /// Mapped rows, all repos.
    #[must_use]
    pub fn mapped(&self) -> usize {
        self.by_repo.values().map(Vec::len).sum()
    }
}

/// Parse an export (JSONL, or a wrangler `--json` / bare-array document),
/// stamping every row with `fetched_at`.
#[must_use]
pub fn parse_export(text: &str, fetched_at: DateTime<Utc>) -> MirrorParse {
    let mut out = MirrorParse::default();
    let trimmed = text.trim_start();
    let rows: Vec<Value> = if trimmed.starts_with('[') {
        match serde_json::from_str::<Value>(trimmed) {
            Ok(Value::Array(items)) => items
                .into_iter()
                .flat_map(|item| match item {
                    Value::Object(mut obj) if obj.contains_key("results") => {
                        match obj.remove("results") {
                            Some(Value::Array(rows)) => rows,
                            _ => Vec::new(),
                        }
                    }
                    other => vec![other],
                })
                .collect(),
            _ => {
                out.unparseable += 1;
                Vec::new()
            }
        }
    } else {
        text.lines()
            .filter(|l| !l.trim().is_empty())
            .filter_map(|l| match serde_json::from_str::<Value>(l) {
                Ok(v) => Some(v),
                Err(_) => {
                    out.unparseable += 1;
                    None
                }
            })
            .collect()
    };
    for row in &rows {
        match map_row(row, fetched_at) {
            Mapped::Event(event) => out
                .by_repo
                .entry(event.repo.to_ascii_lowercase())
                .or_default()
                .push(*event),
            Mapped::OtherKind => out.other_kind += 1,
            Mapped::Invalid => out.unparseable += 1,
        }
    }
    out
}

enum Mapped {
    Event(Box<RawEvent>),
    OtherKind,
    Invalid,
}

/// One D1 `records` row as a [`RawEvent`].
fn map_row(row: &Value, fetched_at: DateTime<Utc>) -> Mapped {
    let Some(obj) = row.as_object() else {
        return Mapped::Invalid;
    };
    let payload = match obj.get("payload") {
        Some(Value::String(s)) => match serde_json::from_str::<Value>(s) {
            Ok(v) => v,
            Err(_) => return Mapped::Invalid,
        },
        Some(v @ Value::Object(_)) => v.clone(),
        _ => return Mapped::Invalid,
    };
    let field = |name: &str| {
        obj.get(name)
            .or_else(|| payload.get(name))
            .and_then(Value::as_str)
    };
    if field("kind") != Some(TRANSITION_KIND) {
        return Mapped::OtherKind;
    }
    let Some(seq) = obj.get("id").and_then(Value::as_u64) else {
        return Mapped::Invalid;
    };
    let Some(repo) = field("repo").filter(|r| r.contains('/')) else {
        return Mapped::Invalid;
    };
    let at = payload
        .get("at")
        .and_then(Value::as_str)
        .or_else(|| obj.get("emitted_at").and_then(Value::as_str));
    let Some(event_time) = at
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map(|d| d.with_timezone(&Utc))
    else {
        return Mapped::Invalid;
    };
    let item_kind = match payload.get("target").and_then(Value::as_str) {
        Some("issue") => ItemKind::Issue,
        Some("pr") => ItemKind::Pr,
        _ => return Mapped::Invalid,
    };
    let Some(item) = payload
        .get("number")
        .and_then(Value::as_u64)
        .and_then(|n| u32::try_from(n).ok())
        .filter(|n| *n > 0)
    else {
        return Mapped::Invalid;
    };
    let label = payload
        .get("label")
        .and_then(Value::as_str)
        .map(str::to_string);
    let merged = payload.get("merged").and_then(Value::as_bool) == Some(true);
    let (kind, label) = match payload.get("action").and_then(Value::as_str) {
        Some("labeled") if label.is_some() => (EventKind::LabelAdded, label),
        Some("unlabeled") if label.is_some() => (EventKind::LabelRemoved, label),
        Some("opened") => (EventKind::Opened, None),
        Some("reopened") => (EventKind::Reopened, None),
        Some("closed") if merged => (EventKind::Merged, None),
        Some("closed") => (EventKind::Closed, None),
        _ => return Mapped::Invalid,
    };
    Mapped::Event(Box::new(RawEvent::new(
        repo,
        item,
        item_kind,
        kind,
        label,
        event_time,
        SOURCE_WEBHOOK_MIRROR,
        seq,
        fetched_at,
    )))
}

#[cfg(test)]
#[path = "fleet_events_webhook_tests.rs"]
mod tests;
