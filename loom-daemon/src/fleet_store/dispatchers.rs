//! The fleet's dispatching hosts and the tailnet node each one calls from,
//! read from the compiled `fleet.json` (#12449).
//!
//! Two sections, each in the shape its other readers already expect:
//!
//! - `dispatch.dispatchers` is a list of **host-name strings**, unchanged.
//!   The store's schema, its render step, the dashboard's fleet-manifest
//!   reader and the shell tooling all read it that way, so it never carries
//!   objects.
//! - Each `hosts[]` record (which already has `name` and `host_id`) may carry
//!   an optional [`TAILNET_NODE_KEY`]: the host's MagicDNS name or its first
//!   label.
//!
//! ```json
//! {"dispatch": {"dispatchers": ["worker-a"]},
//!  "hosts": [{"name": "worker-a", "host_id": "worker-a", "tailnet_node": "node-17"}]}
//! ```
//!
//! A dispatcher is the name joined to the record with that `name`: its
//! `host_id` and `tailnet_node` come from the record. The fleet-dispatch
//! captain maps a caller's tailnet node to its `host_id` through that field
//! only; it never assumes the node name is the host id (they diverge for
//! operator machines and re-imaged hosts; spec §5.1, §11). A dispatcher with
//! no record, or a record with no `host_id` or no `tailnet_node`, is
//! [`Dispatcher::unresolved`]: it is listed, but no node maps to it.
//!
//! The reader fails on anything ambiguous rather than guessing: a dispatcher
//! named twice, two records with one name, two dispatchers with one
//! `host_id`, or two dispatchers whose nodes could match the same `whois`
//! name (equal after normalising, or a short name that is the other's first
//! label). It fails on a malformed entry too (a dispatcher that is not a
//! non-empty string, a `tailnet_node` that is not a non-empty string).

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{anyhow, bail, Result};
use serde_json::{Map, Value};

use super::compiled::Compiled;

/// The host-record key naming the host's tailnet node.
pub const TAILNET_NODE_KEY: &str = "tailnet_node";

/// One dispatching host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dispatcher {
    /// The `dispatch.dispatchers` entry: the host's `name`.
    pub name: String,
    /// The host record's `host_id`; `None` with no record or no `host_id`.
    pub host_id: Option<String>,
    /// The host record's tailnet node, lower-cased with any trailing dot
    /// trimmed; `None` with no record or no node.
    pub tailnet_node: Option<String>,
}

impl Dispatcher {
    /// The id the captain keys the host by: the record's `host_id`, or the
    /// name when there is none (an unresolved dispatcher).
    #[must_use]
    pub fn id(&self) -> &str {
        self.host_id.as_deref().unwrap_or(&self.name)
    }

    /// Whether no tailnet node can map to this dispatcher: it has no host
    /// record, or its record lacks `host_id` or `tailnet_node`.
    #[must_use]
    pub fn unresolved(&self) -> bool {
        self.host_id.is_none() || self.tailnet_node.is_none()
    }
}

/// Normalise a tailnet node name for comparison: trimmed, trailing dot
/// dropped, ASCII lower-case.
#[must_use]
pub fn normalise_node(name: &str) -> String {
    name.trim().trim_end_matches('.').to_ascii_lowercase()
}

/// Whether two normalised node names could match the same `whois` name: equal,
/// or one is a single label equal to the other's first label.
#[must_use]
pub fn nodes_overlap(a: &str, b: &str) -> bool {
    let first = |n: &str| n.split('.').next().unwrap_or_default().to_string();
    a == b || (!a.contains('.') && a == first(b)) || (!b.contains('.') && b == first(a))
}

/// Read the dispatchers from `doc`.
pub fn from_compiled(doc: &Compiled) -> Result<Vec<Dispatcher>> {
    let top = doc.roster();
    let list = top
        .get("dispatch")
        .and_then(|d| d.get("dispatchers"))
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("fleet.json has no `dispatch.dispatchers` list"))?;
    let records = host_records(top)?;
    let mut out: Vec<Dispatcher> = Vec::with_capacity(list.len());
    let mut names = BTreeSet::new();
    let mut ids = BTreeSet::new();
    for (i, entry) in list.iter().enumerate() {
        let Value::String(name) = entry else {
            bail!("`dispatch.dispatchers[{i}]` must be a host-name string");
        };
        if name.trim().is_empty() {
            bail!("`dispatch.dispatchers[{i}]` is an empty host name");
        }
        if !names.insert(name.clone()) {
            bail!("host `{name}` is listed twice in `dispatch.dispatchers`");
        }
        let (host_id, tailnet_node) = match records.get(name.as_str()) {
            Some((j, record)) => read_record(*j, record)?,
            None => (None, None),
        };
        let d = Dispatcher {
            name: name.clone(),
            host_id,
            tailnet_node,
        };
        if !ids.insert(d.id().to_string()) {
            bail!("host id `{}` is named by two `dispatch.dispatchers` entries", d.id());
        }
        if let Some(n) = &d.tailnet_node {
            if let Some(other) = out.iter().find(|o| {
                o.tailnet_node
                    .as_deref()
                    .is_some_and(|m| nodes_overlap(m, n))
            }) {
                bail!(
                    "dispatchers `{}` and `{name}` name the same tailnet node (`{}` / `{n}`)",
                    other.name,
                    other.tailnet_node.as_deref().unwrap_or_default()
                );
            }
        }
        out.push(d);
    }
    Ok(out)
}

/// A `hosts[]` record and its index.
type HostRecord<'a> = (usize, &'a Map<String, Value>);

/// `hosts[]` by `name`, with each record's index. Absent `hosts` is an empty
/// map (every dispatcher is then unresolved); records with no string `name`
/// are skipped, since no dispatcher can name them.
fn host_records(top: &Map<String, Value>) -> Result<BTreeMap<&str, HostRecord<'_>>> {
    let mut out = BTreeMap::new();
    let list = match top.get("hosts") {
        None | Some(Value::Null) => return Ok(out),
        Some(Value::Array(list)) => list,
        Some(_) => bail!("fleet.json `hosts` must be a list"),
    };
    for (j, record) in list.iter().enumerate() {
        let Some(record) = record.as_object() else {
            bail!("`hosts[{j}]` must be an object");
        };
        let Some(name) = record.get("name").and_then(Value::as_str) else {
            continue;
        };
        if out.insert(name, (j, record)).is_some() {
            bail!("two `hosts[]` records are named `{name}`");
        }
    }
    Ok(out)
}

/// A host record's `host_id` and normalised tailnet node.
fn read_record(j: usize, record: &Map<String, Value>) -> Result<(Option<String>, Option<String>)> {
    let host_id = match record.get("host_id") {
        None | Some(Value::Null) => None,
        Some(Value::String(id)) if !id.trim().is_empty() => Some(id.clone()),
        Some(_) => bail!("`hosts[{j}].host_id` must be a non-empty string"),
    };
    let node = match record.get(TAILNET_NODE_KEY) {
        None | Some(Value::Null) => None,
        Some(Value::String(n)) if !n.trim().is_empty() => Some(normalise_node(n)),
        Some(_) => bail!("`hosts[{j}].{TAILNET_NODE_KEY}` must be a non-empty string"),
    };
    Ok((host_id, node))
}
