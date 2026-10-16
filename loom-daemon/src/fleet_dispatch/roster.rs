//! The captain's roster: which tailnet node is which fleet `host_id`, and each
//! host's `maxConcurrent` (#12449).
//!
//! Read from the fleet store's compiled `fleet.json`
//! ([`crate::fleet_store::compiled`]), the same document every other fleet
//! fact comes from:
//!
//! - **Who may call:** `dispatch.dispatchers`, one entry per dispatching host.
//!   An entry is an object naming the host and its tailnet node:
//!
//!   ```json
//!   {"dispatch": {"dispatchers": [
//!     {"host_id": "worker-a", "tailnet_node": "worker-a-node"}
//!   ]}}
//!   ```
//!
//!   The entries are read by [`crate::fleet_store::dispatchers`]. The node
//!   is matched against the `whois` node's MagicDNS name, either
//!   whole (`worker-a-node.example.ts.net`, trailing dot ignored) or its first
//!   label, ASCII case-insensitively. The node name is **never** assumed to be
//!   the `host_id` (they diverge for operator machines and re-imaged hosts;
//!   spec §5.1, §11). A bare-string entry (`"worker-a"`, the older shape)
//!   names a dispatcher with no node, so it cannot call until its entry gains
//!   `tailnet_node`. A node no entry maps is refused `403`.
//! - **Its cap:** `autonomous.workFinder.maxConcurrent`, resolved per host as
//!   a host renders it: `config.hosts.<host>.local` over
//!   `config.hosts.<host>.defaults` over `config.defaults`.
//! - **Whether it is in fleet mode:** `fleetDispatch.enabled`, resolved the
//!   same way. The grace window ends early once every fleet-mode host has
//!   heartbeated; a host not in fleet mode never calls, so waiting for it
//!   would only ever run the window to its time bound.
//!
//! [`RosterSource`] is the seam: the server reloads through it, and tests
//! inject a fixed roster.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use serde_json::Value;

use super::identity::TailnetNode;
use crate::fleet_store::compiled::Compiled;
use crate::fleet_store::dispatchers::{self, normalise_node as normalise};

/// One dispatching host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RosterHost {
    /// The tailnet node name, lower-cased, trailing dot trimmed. `None` for a
    /// bare-string entry: the host cannot call.
    pub tailnet_node: Option<String>,
    /// `autonomous.workFinder.maxConcurrent` for the host, when configured.
    pub max_concurrent: Option<u32>,
    /// `fleetDispatch.enabled` for the host.
    pub fleet_mode: bool,
}

/// The host roster.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Roster {
    hosts: BTreeMap<String, RosterHost>,
}

impl Roster {
    /// A roster of `hosts` (tests and [`RosterSource`] implementations).
    #[must_use]
    pub fn new(hosts: BTreeMap<String, RosterHost>) -> Self {
        Self {
            hosts: hosts
                .into_iter()
                .map(|(id, mut h)| {
                    h.tailnet_node = h.tailnet_node.as_deref().map(normalise);
                    (id, h)
                })
                .collect(),
        }
    }

    /// Read the roster from a compiled `fleet.json`. Fails when
    /// `dispatch.dispatchers` is missing or malformed, or two entries claim
    /// one host or one node: an ambiguous map is never guessed at.
    pub fn from_compiled(doc: &Compiled) -> Result<Self> {
        let hosts = dispatchers::from_compiled(doc)?
            .into_iter()
            .map(|d| {
                let host = RosterHost {
                    tailnet_node: d.tailnet_node,
                    max_concurrent: tiered(
                        doc,
                        &d.host_id,
                        &["autonomous", "workFinder", "maxConcurrent"],
                    )
                    .and_then(Value::as_u64)
                    .and_then(|n| u32::try_from(n).ok()),
                    fleet_mode: tiered(doc, &d.host_id, &["fleetDispatch", "enabled"])
                        .and_then(Value::as_bool)
                        .unwrap_or(false),
                };
                (d.host_id, host)
            })
            .collect();
        Ok(Self { hosts })
    }

    /// The `host_id` `node` maps to, `None` when no entry names it.
    #[must_use]
    pub fn host_for(&self, node: &TailnetNode) -> Option<&str> {
        let full = normalise(&node.name);
        if full.is_empty() {
            return None;
        }
        let short = full.split('.').next().unwrap_or_default();
        self.hosts
            .iter()
            .find(|(_, h)| {
                h.tailnet_node
                    .as_deref()
                    .is_some_and(|n| n == full || n == short)
            })
            .map(|(id, _)| id.as_str())
    }

    /// `host`'s `maxConcurrent`.
    #[must_use]
    pub fn max_concurrent(&self, host: &str) -> Option<u32> {
        self.hosts.get(host).and_then(|h| h.max_concurrent)
    }

    /// Every roster host id.
    #[must_use]
    pub fn host_ids(&self) -> BTreeSet<String> {
        self.hosts.keys().cloned().collect()
    }

    /// Roster hosts in fleet mode: the hosts the grace window waits for.
    #[must_use]
    pub fn fleet_mode_hosts(&self) -> BTreeSet<String> {
        self.hosts
            .iter()
            .filter(|(_, h)| h.fleet_mode)
            .map(|(id, _)| id.clone())
            .collect()
    }

    /// Roster hosts that have a tailnet node and so can call.
    #[must_use]
    pub fn callable(&self) -> usize {
        self.hosts
            .values()
            .filter(|h| h.tailnet_node.is_some())
            .count()
    }
}

/// The config value at `path` for `host`, by the render tiers' precedence
/// (host-local over host defaults over fleet defaults). A `null` falls
/// through to the next tier.
fn tiered<'a>(doc: &'a Compiled, host: &str, path: &[&str]) -> Option<&'a Value> {
    let read = |v: Option<&'a Value>| {
        path.iter()
            .try_fold(v?, |v, k| v.get(k))
            .filter(|v| !v.is_null())
    };
    read(doc.host_local(host))
        .or_else(|| read(doc.host_defaults(host)))
        .or_else(|| read(Some(doc.fleet_defaults())))
}

/// Where the server gets its roster. Called at start and on each reload; a
/// failed reload keeps the last good roster.
pub trait RosterSource: Send + Sync + 'static {
    /// Load the current roster. Blocking (the store reads the forge).
    fn load(&self) -> Result<Roster>;
}

/// A fixed roster (tests, and a manual roster file in future).
#[derive(Debug, Clone)]
pub struct StaticRoster(pub Roster);

impl RosterSource for StaticRoster {
    fn load(&self) -> Result<Roster> {
        Ok(self.0.clone())
    }
}

/// The fleet store at the workspace `root` (`fleet.repo` / `LOOM_FLEET_REPO`):
/// fetched, falling back to the last good cache with a warning.
#[derive(Debug, Clone)]
pub struct FleetStoreRoster {
    root: PathBuf,
}

impl FleetStoreRoster {
    /// The store configured for the workspace at `root`.
    #[must_use]
    pub fn new(root: &Path) -> Self {
        Self {
            root: root.to_path_buf(),
        }
    }
}

impl RosterSource for FleetStoreRoster {
    fn load(&self) -> Result<Roster> {
        use crate::fleet_store::{self as store, compiled, fetch, gh::GhTransport};
        let location = store::require_location(&self.root)?;
        let cache = store::default_cache_dir(&location)?;
        let transport = GhTransport::new(&self.root, &location.repo);
        let now = chrono::Utc::now();
        let loaded = fetch::load(&transport, &cache, &location, fetch::Policy::AllowStale, now)?;
        if let Some(w) = loaded.staleness_warning(now) {
            log::warn!("fleet-dispatch: {w}");
        }
        let doc = compiled::from_snapshot(&loaded.snapshot)?
            .ok_or_else(|| anyhow!("the fleet store has no compiled fleet.json"))?;
        Roster::from_compiled(&doc).context("reading the dispatcher roster")
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use serde_json::json;

    pub(crate) fn fleet_json(dispatchers: Value) -> Compiled {
        let doc = json!({
            "_generated": {"schema_version": 1},
            "root": "~/src",
            "repos": [],
            "state": {},
            "dispatch": {"dispatchers": dispatchers},
            "config": {
                "defaults": {"autonomous": {"workFinder": {"maxConcurrent": 20}}},
                "hosts": {
                    "worker-a": {
                        "defaults": {"autonomous": {"workFinder": {"maxConcurrent": 30}}},
                        "local": {"autonomous": {"workFinder": {"maxConcurrent": 40}},
                                  "fleetDispatch": {"enabled": true, "url": "http://c:1"}}
                    },
                    "worker-b": {
                        "defaults": {"autonomous": {"workFinder": {"maxConcurrent": 8}}}
                    }
                }
            }
        });
        crate::fleet_store::compiled::parse(&doc.to_string()).unwrap()
    }

    fn node(name: &str) -> TailnetNode {
        TailnetNode {
            name: name.to_string(),
            addresses: Vec::new(),
            tags: Vec::new(),
        }
    }

    #[test]
    fn nodes_map_through_the_explicit_field_never_by_host_id() {
        let roster = Roster::from_compiled(&fleet_json(json!([
            {"host_id": "worker-a", "tailnet_node": "node-17"},
            {"host_id": "worker-b", "tailnet_node": "Node-18.Example.ts.net."},
            "worker-c"
        ])))
        .unwrap();
        assert_eq!(roster.host_for(&node("node-17.example.ts.net.")), Some("worker-a"));
        assert_eq!(roster.host_for(&node("node-17")), Some("worker-a"));
        assert_eq!(roster.host_for(&node("node-18.example.ts.net")), Some("worker-b"));
        // The host id is not a node name.
        assert_eq!(roster.host_for(&node("worker-a.example.ts.net.")), None);
        // A bare-string entry has no node, so nothing maps to it.
        assert_eq!(roster.host_for(&node("worker-c.example.ts.net.")), None);
        assert_eq!(roster.host_for(&node("")), None);
        assert_eq!(roster.host_ids().len(), 3);
        assert_eq!(roster.callable(), 2);
    }

    #[test]
    fn max_concurrent_follows_the_render_tiers() {
        let roster = Roster::from_compiled(&fleet_json(json!([
            {"host_id": "worker-a", "tailnet_node": "n-a"},
            {"host_id": "worker-b", "tailnet_node": "n-b"},
            {"host_id": "worker-c", "tailnet_node": "n-c"}
        ])))
        .unwrap();
        assert_eq!(roster.max_concurrent("worker-a"), Some(40), "local wins");
        assert_eq!(roster.max_concurrent("worker-b"), Some(8), "host defaults next");
        assert_eq!(roster.max_concurrent("worker-c"), Some(20), "fleet defaults last");
        assert_eq!(roster.max_concurrent("stranger"), None);
        let fleet_mode: Vec<String> = roster.fleet_mode_hosts().into_iter().collect();
        assert_eq!(fleet_mode, vec!["worker-a".to_string()]);
    }

    #[test]
    fn an_ambiguous_or_malformed_roster_is_refused() {
        for bad in [
            json!([{"host_id": "a", "tailnet_node": "n"}, {"host_id": "b", "tailnet_node": "N"}]),
            json!(["a", {"host_id": "a", "tailnet_node": "n"}]),
            json!([{"tailnet_node": "n"}]),
            json!([{"host_id": "a", "tailnet_node": 3}]),
            json!([""]),
            json!([4]),
            json!("a"),
        ] {
            assert!(Roster::from_compiled(&fleet_json(bad.clone())).is_err(), "{bad}");
        }
    }
}
