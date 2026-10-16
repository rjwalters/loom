//! The captain's roster: which tailnet node is which fleet `host_id`, and each
//! host's `maxConcurrent` (#12449).
//!
//! Read from the fleet store's compiled `fleet.json`
//! ([`crate::fleet_store::compiled`]), the same document every other fleet
//! fact comes from:
//!
//! - **Who may call:** `dispatch.dispatchers`, a list of host-name strings,
//!   joined to each host's `hosts[]` record for its `host_id` and its optional
//!   `tailnet_node`:
//!
//!   ```json
//!   {"dispatch": {"dispatchers": ["worker-a"]},
//!    "hosts": [{"name": "worker-a", "host_id": "worker-a", "tailnet_node": "node-17"}]}
//!   ```
//!
//!   Both are read by [`crate::fleet_store::dispatchers`]; the list's element
//!   type is never changed, because the store's schema, render step,
//!   dashboard and shell tooling read it as strings. The node is matched
//!   against the `whois` node's MagicDNS name, either whole
//!   (`node-17.example.ts.net`, trailing dot ignored) or its first label,
//!   ASCII case-insensitively. The node name is **never** assumed to be the
//!   `host_id` (they diverge for operator machines and re-imaged hosts;
//!   spec §5.1, §11). A dispatcher with no host record, or whose record has
//!   no `host_id` or no `tailnet_node`, is unresolved
//!   ([`Roster::unresolved`]): nothing maps to it, so it cannot call, and the
//!   server warns at start. A node no dispatcher maps is refused `403`.
//! - **Its cap:** `autonomous.workFinder.maxConcurrent`, resolved per host as
//!   a host renders it: `config.hosts.<host>.local` over
//!   `config.hosts.<host>.defaults` over `config.defaults`.
//! - **Whether it is in fleet mode:** `fleetDispatch.enabled`, resolved the
//!   same way. The grace window ends early once every fleet-mode host that
//!   can call has heartbeated; a host not in fleet mode, or unresolved, never
//!   calls, so waiting for it would only ever run the window to its time
//!   bound.
//!
//! The config tiers (`config.hosts.<name>`) are keyed by the host's name;
//! the roster is keyed by its `host_id`, the id a caller's body carries.
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
    /// The tailnet node name, lower-cased, trailing dot trimmed. `None` for
    /// an unresolved dispatcher (no host record, `host_id` or node): the
    /// host cannot call.
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
    /// `dispatch.dispatchers` or `hosts` is missing or malformed, or the map
    /// is ambiguous (a host named twice, or two dispatchers whose nodes could
    /// match one `whois` name): an ambiguous map is never guessed at.
    pub fn from_compiled(doc: &Compiled) -> Result<Self> {
        let hosts = dispatchers::from_compiled(doc)?
            .into_iter()
            .map(|d| {
                let host = RosterHost {
                    tailnet_node: if d.unresolved() {
                        None
                    } else {
                        d.tailnet_node.clone()
                    },
                    max_concurrent: tiered(
                        doc,
                        &d.name,
                        &["autonomous", "workFinder", "maxConcurrent"],
                    )
                    .and_then(Value::as_u64)
                    .and_then(|n| u32::try_from(n).ok()),
                    fleet_mode: tiered(doc, &d.name, &["fleetDispatch", "enabled"])
                        .and_then(Value::as_bool)
                        .unwrap_or(false),
                };
                (d.id().to_string(), host)
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

    /// Roster hosts in fleet mode that can call: the hosts the grace window
    /// waits for.
    #[must_use]
    pub fn fleet_mode_hosts(&self) -> BTreeSet<String> {
        self.hosts
            .iter()
            .filter(|(_, h)| h.fleet_mode && h.tailnet_node.is_some())
            .map(|(id, _)| id.clone())
            .collect()
    }

    /// Roster hosts no tailnet node maps to (no host record, `host_id` or
    /// `tailnet_node`): every call from them is refused `403`.
    #[must_use]
    pub fn unresolved(&self) -> Vec<String> {
        self.hosts
            .iter()
            .filter(|(_, h)| h.tailnet_node.is_none())
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

    /// A `fleet.json` with `dispatchers` and the host records `hosts`.
    pub(crate) fn fleet_json(dispatchers: Value, hosts: Value) -> Compiled {
        let doc = json!({
            "_generated": {"schema_version": 1},
            "root": "~/src",
            "repos": [],
            "state": {},
            "dispatch": {"dispatchers": dispatchers},
            "hosts": hosts,
            "config": {
                "defaults": {"autonomous": {"workFinder": {"maxConcurrent": 20}}},
                "hosts": {
                    "worker-a": {
                        "defaults": {"autonomous": {"workFinder": {"maxConcurrent": 30}}},
                        "local": {"autonomous": {"workFinder": {"maxConcurrent": 40}},
                                  "fleetDispatch": {"enabled": true, "url": "http://c:1"}}
                    },
                    "worker-b": {
                        "defaults": {"autonomous": {"workFinder": {"maxConcurrent": 8}}},
                        "local": {"fleetDispatch": {"enabled": true}}
                    }
                }
            }
        });
        crate::fleet_store::compiled::parse(&doc.to_string()).unwrap()
    }

    fn record(name: &str, host_id: &str, node: Option<&str>) -> Value {
        let mut r = json!({"name": name, "host_id": host_id, "role": "worker"});
        if let Some(n) = node {
            r["tailnet_node"] = json!(n);
        }
        r
    }

    fn node(name: &str) -> TailnetNode {
        TailnetNode {
            name: name.to_string(),
            addresses: Vec::new(),
            tags: Vec::new(),
        }
    }

    #[test]
    fn nodes_map_through_the_host_record_never_by_host_id() {
        let roster = Roster::from_compiled(&fleet_json(
            json!(["worker-a", "worker-b", "worker-c", "worker-d"]),
            json!([
                record("worker-a", "worker-a", Some("node-17")),
                record("worker-b", "host-b", Some("Node-18.Example.ts.net.")),
                record("worker-c", "worker-c", None),
                record("not-a-dispatcher", "x", Some("node-99"))
            ]),
        ))
        .unwrap();
        assert_eq!(roster.host_for(&node("node-17.example.ts.net.")), Some("worker-a"));
        assert_eq!(roster.host_for(&node("NODE-17")), Some("worker-a"));
        // Keyed by the record's host_id, not the dispatcher name.
        assert_eq!(roster.host_for(&node("node-18.example.ts.net")), Some("host-b"));
        // The host id is not a node name.
        assert_eq!(roster.host_for(&node("worker-a.example.ts.net.")), None);
        // A record without a node, and a dispatcher without a record, are
        // unresolved: nothing maps to them.
        assert_eq!(roster.host_for(&node("worker-c.example.ts.net.")), None);
        assert_eq!(roster.host_for(&node("worker-d")), None);
        // A host that is not a dispatcher cannot call.
        assert_eq!(roster.host_for(&node("node-99")), None);
        assert_eq!(roster.host_for(&node("")), None);
        assert_eq!(roster.host_ids().len(), 4);
        assert_eq!(roster.callable(), 2);
        assert_eq!(roster.unresolved(), vec!["worker-c".to_string(), "worker-d".to_string()]);
    }

    #[test]
    fn max_concurrent_and_fleet_mode_follow_the_render_tiers_by_name() {
        let roster = Roster::from_compiled(&fleet_json(
            json!(["worker-a", "worker-b", "worker-c"]),
            json!([
                record("worker-a", "id-a", Some("n-a")),
                record("worker-b", "id-b", None),
                record("worker-c", "id-c", Some("n-c"))
            ]),
        ))
        .unwrap();
        assert_eq!(roster.max_concurrent("id-a"), Some(40), "local wins");
        assert_eq!(roster.max_concurrent("id-b"), Some(8), "host defaults next");
        assert_eq!(roster.max_concurrent("id-c"), Some(20), "fleet defaults last");
        assert_eq!(roster.max_concurrent("stranger"), None);
        // worker-b is in fleet mode but cannot call, so grace never waits on it.
        let fleet_mode: Vec<String> = roster.fleet_mode_hosts().into_iter().collect();
        assert_eq!(fleet_mode, vec!["id-a".to_string()]);
    }

    #[test]
    fn a_store_without_host_records_leaves_every_dispatcher_unresolved() {
        let roster =
            Roster::from_compiled(&fleet_json(json!(["worker-a", "worker-b"]), Value::Null))
                .unwrap();
        assert_eq!(roster.callable(), 0);
        assert_eq!(roster.unresolved().len(), 2);
        assert!(roster.fleet_mode_hosts().is_empty());
    }

    #[test]
    fn an_ambiguous_or_malformed_roster_is_refused() {
        let two = json!(["a", "b"]);
        for (dispatchers, hosts) in [
            // The node's short and full forms are one node.
            (
                two.clone(),
                json!([
                    record("a", "a", Some("n")),
                    record("b", "b", Some("N.example.ts.net."))
                ]),
            ),
            (
                two.clone(),
                json!([
                    record("a", "a", Some("n.example.ts.net")),
                    record("b", "b", Some("n"))
                ]),
            ),
            (two.clone(), json!([record("a", "a", Some("n")), record("b", "b", Some("N."))])),
            // Two dispatchers, one host id.
            (two.clone(), json!([record("a", "x", Some("n1")), record("b", "x", Some("n2"))])),
            // Two records with one name.
            (
                json!(["a"]),
                json!([record("a", "a", Some("n1")), record("a", "a2", Some("n2"))]),
            ),
            // Malformed records.
            (json!(["a"]), json!([{"name": "a", "host_id": "a", "tailnet_node": 3}])),
            (json!(["a"]), json!([{"name": "a", "host_id": "a", "tailnet_node": " "}])),
            (json!(["a"]), json!([{"name": "a", "host_id": 4}])),
            (json!(["a"]), json!(["a"])),
            (json!(["a"]), json!({"a": {}})),
            // The list stays strings: the object form is gone.
            (json!([{"host_id": "a", "tailnet_node": "n"}]), json!([])),
            (json!(["a", "a"]), json!([])),
            (json!([""]), json!([])),
            (json!([4]), json!([])),
            (json!("a"), json!([])),
        ] {
            let doc = fleet_json(dispatchers.clone(), hosts.clone());
            assert!(Roster::from_compiled(&doc).is_err(), "{dispatchers} / {hosts}");
        }
        // Distinct full names sharing a first label are distinct nodes: a
        // `whois` name matches at most one of them.
        let ok = fleet_json(
            two,
            json!([
                record("a", "a", Some("n.one.ts.net")),
                record("b", "b", Some("n.two.ts.net"))
            ]),
        );
        let roster = Roster::from_compiled(&ok).unwrap();
        assert_eq!(roster.host_for(&node("n.two.ts.net")), Some("b"));
    }
}
