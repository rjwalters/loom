//! Turn this host's `render --check` drift into the `fleet.yml` edit that
//! would make it the new rendered value, for `propose adopt`.
//!
//! Reuses [`render::render`] for the same machine-tier merge and host-local
//! read `render` itself uses, so `adopt` can never disagree with `render
//! --check` about what counts as drift. The store must have the compiled
//! `fleet.json`: the edit lands in its source, `fleet.yml` (#10905).
//!
//! Both tiers are patched leaf by leaf, as format-preserving edits
//! ([`super::yaml_edit`]) of `config.hosts.<H>`:
//!
//! - **Machine tier** (`config.hosts.<H>.defaults`): only the leaves where
//!   the on-disk file differs from `deep_merge(defaults, overlay)` are
//!   written into the host's overlay, preserving every other key already
//!   there (and everything `config.defaults` already supplies): a full
//!   re-render would blow away the host's other overrides along with the
//!   drift.
//! - **Host-local tier** (`config.hosts.<H>.local`): the store's value *is*
//!   the file, with no merge, so the patch makes it equal to the on-disk
//!   file. A store with no local tier for this host gets no
//!   [`render::Tier::Local`] target at all, so `adopt` also checks the
//!   on-disk file directly, to cover *adding* a host-local override the
//!   store has never had, not just updating one that drifted.

use std::path::Path;

use anyhow::{anyhow, Context, Result};
use serde_json::Value;

use super::yaml_edit::{Doc, Seg};
use crate::fleet_store::fetch::Snapshot;
use crate::fleet_store::render::{self, Tier};
use crate::fleet_store::{compiled, FLEET_JSON_PATH};

/// `source` (the store's `fleet.yml`) edited to adopt `host`'s on-disk
/// drift (machine tier at `machine_path`, host-local tier at `local_path`);
/// `None` when there is no drift to adopt.
pub fn plan(
    snapshot: &Snapshot,
    source: &str,
    host: &str,
    machine_path: &Path,
    local_path: &Path,
) -> Result<Option<String>> {
    let doc_json = compiled::from_snapshot(snapshot)?.ok_or_else(|| {
        anyhow!("the store has no {FLEET_JSON_PATH}; `propose adopt` edits fleet.yml, its source")
    })?;
    let targets = render::render(snapshot, host, machine_path, local_path)?;
    let mut doc = Doc::new(source);
    let mut saw_local_target = false;

    for t in &targets {
        let tier = match t.tier {
            Tier::Local => {
                saw_local_target = true;
                "local"
            }
            Tier::Machine => "defaults",
        };
        let Some(current) = on_disk(&t.path)? else {
            continue;
        };
        if current == t.value {
            continue;
        }
        let mut ops = Vec::new();
        collect(&mut Vec::new(), &current, &t.value, &mut ops);
        for op in &ops {
            let (leaf, value) = match op {
                Op::Set(p, v) => (p, Some(v)),
                Op::Remove(p) => (p, None),
            };
            let mut path = vec![
                Seg::Key("config"),
                Seg::Key("hosts"),
                Seg::Key(host),
                Seg::Key(tier),
            ];
            path.extend(leaf.iter().map(|k| Seg::Key(k)));
            match value {
                Some(v) => doc.set(&path, v)?,
                None => {
                    doc.remove(&path)?;
                }
            }
        }
    }

    // A host-local override the store has never had: `render()` emits no
    // `Local` target to compare against, so check the on-disk file directly.
    if !saw_local_target && doc_json.host_local(host).is_none() {
        if let Some(current) = on_disk(local_path)? {
            let path = [
                Seg::Key("config"),
                Seg::Key("hosts"),
                Seg::Key(host),
                Seg::Key("local"),
            ];
            doc.set(&path, &current)?;
        }
    }

    let after = doc.finish();
    Ok((after != source).then_some(after))
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

#[cfg(test)]
#[path = "tests/adopt_tests.rs"]
mod tests;
