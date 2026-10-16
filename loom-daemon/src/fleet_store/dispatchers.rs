//! `dispatch.dispatchers` in the compiled `fleet.json`: the fleet's
//! dispatching hosts and the tailnet node each one calls from (#12449).
//!
//! An entry is either an object
//!
//! ```json
//! {"host_id": "worker-a", "tailnet_node": "worker-a-node"}
//! ```
//!
//! or, the older shape, a bare host-id string, which names a dispatcher with
//! no node. The fleet-dispatch captain maps a caller's tailnet node to its
//! `host_id` through [`TAILNET_NODE_KEY`] only; it never assumes the node name
//! is the host id (they diverge for operator machines and re-imaged hosts).
//!
//! The reader fails on anything ambiguous (a host or a node named twice) or
//! malformed, rather than guessing.

use std::collections::BTreeSet;

use anyhow::{anyhow, bail, Result};
use serde_json::Value;

use super::compiled::Compiled;

/// The dispatcher entry key naming the host's tailnet node.
pub const TAILNET_NODE_KEY: &str = "tailnet_node";

/// One `dispatch.dispatchers` entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dispatcher {
    pub host_id: String,
    /// The tailnet node name, lower-cased with any trailing dot trimmed;
    /// `None` for a bare-string entry.
    pub tailnet_node: Option<String>,
}

/// Normalise a tailnet node name for comparison: trimmed, trailing dot
/// dropped, ASCII lower-case.
#[must_use]
pub fn normalise_node(name: &str) -> String {
    name.trim().trim_end_matches('.').to_ascii_lowercase()
}

/// Read `dispatch.dispatchers` from `doc`.
pub fn from_compiled(doc: &Compiled) -> Result<Vec<Dispatcher>> {
    let list = doc
        .roster()
        .get("dispatch")
        .and_then(|d| d.get("dispatchers"))
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("fleet.json has no `dispatch.dispatchers` list"))?;
    let mut out = Vec::with_capacity(list.len());
    let mut hosts = BTreeSet::new();
    let mut nodes = BTreeSet::new();
    for (i, entry) in list.iter().enumerate() {
        let (host_id, tailnet_node) = match entry {
            Value::String(s) => (s.clone(), None),
            Value::Object(o) => {
                let host_id = o
                    .get("host_id")
                    .and_then(Value::as_str)
                    .ok_or_else(|| anyhow!("`dispatch.dispatchers[{i}].host_id` is missing"))?
                    .to_string();
                let node = match o.get(TAILNET_NODE_KEY) {
                    None | Some(Value::Null) => None,
                    Some(Value::String(n)) if !n.trim().is_empty() => Some(normalise_node(n)),
                    Some(_) => bail!(
                        "`dispatch.dispatchers[{i}].{TAILNET_NODE_KEY}` must be a non-empty string"
                    ),
                };
                (host_id, node)
            }
            _ => bail!("`dispatch.dispatchers[{i}]` must be a string or an object"),
        };
        if host_id.trim().is_empty() {
            bail!("`dispatch.dispatchers[{i}]` names an empty host id");
        }
        if !hosts.insert(host_id.clone()) {
            bail!("host `{host_id}` is listed twice in `dispatch.dispatchers`");
        }
        if let Some(n) = &tailnet_node {
            if !nodes.insert(n.clone()) {
                bail!("tailnet node `{n}` is named by two dispatcher entries");
            }
        }
        out.push(Dispatcher {
            host_id,
            tailnet_node,
        });
    }
    Ok(out)
}
