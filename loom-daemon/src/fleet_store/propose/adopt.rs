//! Turn this host's `render --check` drift into the store-side edit that
//! would make it the new rendered value, for `propose adopt`.
//!
//! Reuses [`render::render`] for the same machine-tier merge and host-local
//! read `render` itself uses, so `adopt` can never disagree with `render
//! --check` about what counts as drift.
//!
//! - **Host-local tier** (`fleet/hosts/<H>/local.json`) is written verbatim
//!   from the workspace's on-disk file — the store's value for this tier
//!   *is* the file, with no merge, so "adopt" is just "copy it up". This is
//!   the one case `render()` cannot see on its own: a store with no
//!   `local.json` for this host yet gets no [`render::Tier::Local`] target
//!   at all, so `adopt` also checks the on-disk file directly, to cover
//!   *adding* a host-local override the store has never had, not just
//!   updating one that drifted from an existing one.
//! - **Machine tier** (`fleet/hosts/<H>/defaults.json`) is a patch: only the
//!   leaves where the on-disk file differs from `deep_merge(base, overlay)`
//!   are written into the host's overlay object, preserving every other key
//!   already there (and everything `defaults.json`'s base tier already
//!   supplies) — a full re-render would blow away the host's other
//!   overrides along with the drift.

use std::path::Path;

use anyhow::{Context, Result};
use serde_json::{Map, Value};

use crate::fleet_store::fetch::Snapshot;
use crate::fleet_store::render::{self, Tier};
use crate::fleet_store::{host_defaults_path, host_local_path};

/// One file [`plan`] proposes to change in the store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdoptedFile {
    /// Store-relative path.
    pub path: String,
    /// The store's current text, `None` when it does not have the file yet.
    pub before: Option<String>,
    /// The new text.
    pub after: String,
}

/// The store-file changes that would adopt `host`'s on-disk drift
/// (machine tier at `machine_path`, host-local tier at `local_path`) into
/// the store. Empty when there is no drift to adopt.
pub fn plan(
    snapshot: &Snapshot,
    host: &str,
    machine_path: &Path,
    local_path: &Path,
) -> Result<Vec<AdoptedFile>> {
    let targets = render::render(snapshot, host, machine_path, local_path)?;
    let local_store_path = host_local_path(host);
    let mut out = Vec::new();
    let mut saw_local_target = false;

    for t in &targets {
        let Some(current) = on_disk(&t.path)? else {
            continue;
        };
        if current == t.value {
            continue;
        }
        match t.tier {
            Tier::Local => {
                saw_local_target = true;
                out.push(AdoptedFile {
                    before: snapshot.text(&local_store_path)?,
                    path: local_store_path.clone(),
                    after: pretty(&current)?,
                });
            }
            Tier::Machine => {
                let overlay_path = host_defaults_path(host);
                let before = snapshot.text(&overlay_path)?;
                let mut overlay: Value = match &before {
                    Some(t) => serde_json::from_str(t)
                        .with_context(|| format!("parsing store file {overlay_path}"))?,
                    None => Value::Object(Map::new()),
                };
                let mut ops = Vec::new();
                collect(&mut Vec::new(), &current, &t.value, &mut ops);
                if !ops.is_empty() {
                    apply(&mut overlay, &ops);
                    out.push(AdoptedFile {
                        path: overlay_path,
                        before,
                        after: pretty(&overlay)?,
                    });
                }
            }
        }
    }

    // A host-local override the store has never had: `render()` emits no
    // `Local` target to compare against, so check the on-disk file directly.
    if !saw_local_target && snapshot.text(&local_store_path)?.is_none() {
        if let Some(current) = on_disk(local_path)? {
            out.push(AdoptedFile {
                path: local_store_path,
                before: None,
                after: pretty(&current)?,
            });
        }
    }

    Ok(out)
}

fn on_disk(path: &Path) -> Result<Option<Value>> {
    match std::fs::read_to_string(path) {
        Ok(t) => Ok(Some(
            serde_json::from_str(&t).with_context(|| format!("parsing {}", path.display()))?,
        )),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

fn pretty(v: &Value) -> Result<String> {
    let mut s = serde_json::to_string_pretty(v)?;
    s.push('\n');
    Ok(s)
}

/// One leaf (or whole differing subtree) to adopt from `current` (on disk).
enum Op {
    /// Set this dotted path to this value.
    Set(Vec<String>, Value),
    /// Remove this dotted path (present in the rendered/store value, absent
    /// on disk — the operator deleted it locally).
    Remove(Vec<String>),
}

/// Walk `current` (on disk) against `wanted` (rendered from the store) and
/// collect every path where they differ, taking the on-disk value as truth.
fn collect(path: &mut Vec<String>, current: &Value, wanted: &Value, out: &mut Vec<Op>) {
    match (current, wanted) {
        (Value::Object(c), Value::Object(w)) => {
            for (k, cv) in c {
                path.push(k.clone());
                match w.get(k) {
                    Some(wv) => collect(path, cv, wv, out),
                    None => out.push(Op::Set(path.clone(), cv.clone())),
                }
                path.pop();
            }
            for k in w.keys() {
                if !c.contains_key(k) {
                    path.push(k.clone());
                    out.push(Op::Remove(path.clone()));
                    path.pop();
                }
            }
        }
        (c, w) if c == w => {}
        (c, _) => out.push(Op::Set(path.clone(), c.clone())),
    }
}

fn apply(root: &mut Value, ops: &[Op]) {
    for op in ops {
        match op {
            Op::Set(path, v) => set_path(root, path, v.clone()),
            Op::Remove(path) => remove_path(root, path),
        }
    }
}

fn set_path(root: &mut Value, path: &[String], value: Value) {
    if path.is_empty() {
        *root = value;
        return;
    }
    if !root.is_object() {
        *root = Value::Object(Map::new());
    }
    let Value::Object(obj) = root else {
        unreachable!("just normalized to an object")
    };
    if path.len() == 1 {
        obj.insert(path[0].clone(), value);
    } else {
        let entry = obj
            .entry(path[0].clone())
            .or_insert_with(|| Value::Object(Map::new()));
        set_path(entry, &path[1..], value);
    }
}

fn remove_path(root: &mut Value, path: &[String]) {
    if path.is_empty() {
        return;
    }
    let Some(obj) = root.as_object_mut() else {
        return;
    };
    if path.len() == 1 {
        obj.remove(&path[0]);
        return;
    }
    if let Some(next) = obj.get_mut(&path[0]) {
        remove_path(next, &path[1..]);
    }
}

#[cfg(test)]
#[path = "tests/adopt_tests.rs"]
mod tests;
