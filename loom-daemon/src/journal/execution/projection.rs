//! Execution projections rebuilt from J1 (§10.1), and the per-append
//! divergence check (§10.2 steps 1-3).
//!
//! A projection maps `(store, path, item)` to the value the legacy store
//! should hold, by replaying `set` / `patch` (RFC 7386) / `remove` records in
//! file order. Records of unknown kinds or without the shadow fields are
//! ignored (§11.4: unknown kinds pass through). A projection is a *mirror of
//! what was written*; it is never consulted for liveness.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use serde_json::{Map, Value};

use super::super::{Envelope, JournalError, Position, ReadStats, Stream};
use super::{Kind, ShadowRecord, Store, Verdict};

/// The projected state of one key.
#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    /// `None` after a `remove`.
    pub value: Option<Value>,
    /// Whether the value is anchored on a full `set` (or a removal). A patch
    /// with no journaled base — journaling was switched on while the key
    /// already existed — is unanchored, so a mismatch is explained.
    pub based: bool,
}

type Key = (Store, String, String);

/// All execution projections.
#[derive(Debug, Default, Clone)]
pub struct Projection {
    entries: HashMap<Key, Entry>,
    position: Position,
    /// Records applied to some key.
    pub applied: u64,
    /// Records skipped: unknown kind, unknown store, or no shadow fields.
    pub unapplied: u64,
    /// Durable `journal.divergence` records seen.
    pub divergence_records: u64,
}

impl Projection {
    /// Replay one record.
    pub fn apply(&mut self, envelope: &Envelope) {
        if envelope.kind == Kind::Divergence.as_str() {
            self.divergence_records += 1;
            return;
        }
        let data = &envelope.data;
        let field = |name: &str| data.get(name).and_then(Value::as_str);
        let (Some(store), Some(path), Some(item), Some(op)) =
            (field("store").and_then(Store::parse), field("path"), field("item"), field("op"))
        else {
            self.unapplied += 1;
            return;
        };
        let key = (store, path.to_owned(), item.to_owned());
        let entry = match op {
            "set" => Entry {
                value: data.get("value").filter(|v| !v.is_null()).cloned(),
                based: true,
            },
            "patch" => {
                let patch = data.get("patch").cloned().unwrap_or(Value::Null);
                match self.entries.get(&key) {
                    Some(Entry {
                        value: Some(base),
                        based,
                    }) => {
                        let mut value = base.clone();
                        merge_patch(&mut value, &patch);
                        Entry {
                            value: Some(value),
                            based: *based,
                        }
                    }
                    _ => {
                        let mut value = Value::Object(Map::new());
                        merge_patch(&mut value, &patch);
                        Entry {
                            value: Some(value),
                            based: false,
                        }
                    }
                }
            }
            "remove" => Entry {
                value: None,
                based: true,
            },
            _ => {
                self.unapplied += 1;
                return;
            }
        };
        self.applied += 1;
        self.entries.insert(key, entry);
    }

    /// The projected state of one key; `None` if the journal never named it.
    #[must_use]
    pub fn get(&self, store: Store, path: &str, item: &str) -> Option<&Entry> {
        self.entries.get(&(store, path.to_owned(), item.to_owned()))
    }

    /// Every key the journal named, with its state.
    pub fn entries(&self) -> impl Iterator<Item = (Store, &str, &str, &Entry)> {
        self.entries
            .iter()
            .map(|((s, p, i), e)| (*s, p.as_str(), i.as_str(), e))
    }

    /// Rebuild from the start of `stream`. Never writes.
    pub fn rebuild(stream: &Stream) -> Result<(Projection, ReadStats), JournalError> {
        let mut projection = Projection::default();
        let mut reader = stream.reader(Position::START);
        for record in reader.by_ref() {
            let record = record?;
            projection.apply(&record.envelope);
            projection.position = record.next;
        }
        Ok((projection, reader.stats()))
    }

    /// Apply records up to and including `until`. `true` if it was reached.
    fn advance(&mut self, stream: &Stream, until: u64) -> Result<bool, JournalError> {
        for record in stream.reader(self.position) {
            let record = record?;
            let seq = record.envelope.seq;
            if seq > until {
                break;
            }
            self.apply(&record.envelope);
            self.position = record.next;
            if seq == until {
                return Ok(true);
            }
        }
        Ok(false)
    }
}

/// A cache that consumed this far past the record being checked is reset
/// rather than skipped (a `seq` regression after a restore, §11.2).
const REBUILD_WINDOW: u64 = 4096;

/// What [`check`] found for the touched key.
#[derive(Debug, Clone, PartialEq)]
pub(super) enum Projected {
    Entry(Option<Entry>),
    Skipped,
}

/// Project the touched key as of `seq` (the record just appended), using a
/// per-root incremental cache that also absorbs other writers' records.
pub(super) fn check(
    root: &Path,
    stream: &Stream,
    seq: u64,
    record: &ShadowRecord,
) -> Result<Projected, JournalError> {
    static CACHE: OnceLock<Mutex<HashMap<PathBuf, Projection>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    let mut guard = cache
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let projection = guard.entry(root.to_path_buf()).or_default();
    if projection.position.seq >= seq {
        if projection.position.seq - seq <= REBUILD_WINDOW {
            return Ok(Projected::Skipped);
        }
        *projection = Projection::default();
    }
    if !projection.advance(stream, seq)? {
        // Truncated, rotated or restored under us: replay from the start.
        *projection = Projection::default();
        projection.advance(stream, seq)?;
    }
    let path = record.path.display().to_string();
    Ok(Projected::Entry(projection.get(record.store, &path, &record.item).cloned()))
}

/// Compare a projected key with the legacy value just written.
pub(super) fn compare(projected: Projected, expected: Option<&Value>) -> Verdict {
    let entry = match projected {
        Projected::Skipped => return Verdict::Skipped,
        Projected::Entry(entry) => entry,
    };
    let (value, based) = match entry {
        Some(Entry { value, based }) => (value, based),
        None => (None, true),
    };
    let legacy = expected.map(normalize).filter(|v| !v.is_null());
    let journal = value.as_ref().map(normalize).filter(|v| !v.is_null());
    if legacy == journal {
        Verdict::Match
    } else if !based {
        Verdict::NoBase
    } else {
        Verdict::Diverged(diff_fields(legacy.as_ref(), journal.as_ref()))
    }
}

/// The top-level fields on which two normalised values differ.
#[must_use]
pub fn diff_fields(legacy: Option<&Value>, journal: Option<&Value>) -> Vec<String> {
    match (legacy, journal) {
        (Some(Value::Object(a)), Some(Value::Object(b))) => {
            let mut keys: Vec<String> = a
                .keys()
                .chain(b.keys())
                .filter(|k| a.get(*k) != b.get(*k))
                .cloned()
                .collect();
            keys.sort();
            keys.dedup();
            keys
        }
        (None, _) => vec!["(absent in legacy)".to_owned()],
        (_, None) => vec!["(absent in journal)".to_owned()],
        _ => vec!["(value)".to_owned()],
    }
}

/// §10.2 normalisation: object fields that are `null` are dropped (absent and
/// `null` are equal), RFC 3339 timestamps compare at whole seconds in UTC.
#[must_use]
pub fn normalize(value: &Value) -> Value {
    match value {
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(k, v)| (k.clone(), normalize(v)))
                .filter(|(_, v)| !v.is_null())
                .collect(),
        ),
        Value::Array(items) => Value::Array(items.iter().map(normalize).collect()),
        Value::String(s) => match chrono::DateTime::parse_from_rfc3339(s) {
            Ok(t) => Value::from(
                t.with_timezone(&chrono::Utc)
                    .format("%Y-%m-%dT%H:%M:%SZ")
                    .to_string(),
            ),
            Err(_) => value.clone(),
        },
        _ => value.clone(),
    }
}

/// RFC 7386 JSON merge patch.
pub fn merge_patch(target: &mut Value, patch: &Value) {
    let Value::Object(patch) = patch else {
        *target = patch.clone();
        return;
    };
    if !target.is_object() {
        *target = Value::Object(Map::new());
    }
    if let Value::Object(map) = target {
        for (k, v) in patch {
            if v.is_null() {
                map.remove(k);
            } else {
                merge_patch(map.entry(k.clone()).or_insert(Value::Null), v);
            }
        }
    }
}

/// The merge patch that turns `before` into `after`.
#[must_use]
pub fn diff_patch(before: &Value, after: &Value) -> Value {
    match (before, after) {
        (Value::Object(a), Value::Object(b)) => {
            let mut patch = Map::new();
            for (k, v) in b {
                match a.get(k) {
                    Some(old) if old == v => {}
                    Some(old) if old.is_object() && v.is_object() => {
                        patch.insert(k.clone(), diff_patch(old, v));
                    }
                    _ => {
                        patch.insert(k.clone(), v.clone());
                    }
                }
            }
            for k in a.keys() {
                if !b.contains_key(k) {
                    patch.insert(k.clone(), Value::Null);
                }
            }
            Value::Object(patch)
        }
        _ => after.clone(),
    }
}
