//! The fleet floor as a process that is **not** the daemon can learn it
//! (Issue #11044).
//!
//! [`super::floor_knowledge`] answers for the daemon process: it knows whether
//! its own [`super::start`] found a store, and what its own passes resolved. A
//! separate process, `loom-daemon daemon-update` behind
//! `loom-daemon-update.sh`, has neither, and must not wait on a daemon that may
//! be busy, wedged or stopped. So it reads the same two inputs from disk:
//!
//! 1. **Is this a fleet host?** Either signal is enough:
//!    * the store location, resolved exactly as [`super::start`] resolves it:
//!      `LOOM_FLEET_REPO` over `fleet.repo` in the workspace's effective
//!      config (`fleet.repo` normally lives in the host-wide private-defaults
//!      tier). A store that is named but unusable is a fleet host whose floor
//!      is unknown, as for the daemon;
//!    * a fleet-sync snapshot on disk. A host often names its store only in
//!      the daemon's supervisor environment (`LOOM_FLEET_REPO` in the launchd
//!      plist or systemd unit), which an operator's shell does not have. The
//!      daemon deletes the snapshot at startup when it reads no store
//!      ([`super::clear_status`]), so a snapshot present means the daemon that
//!      wrote it reads one.
//! 2. **What is the floor?** The host-level snapshot
//!    (`<loom_dir>/fleet-sync-status.json`, [`super::status_path`]), which
//!    every completed fleet-sync pass rewrites, and whose `floor.floor` the
//!    daemon itself trusts before its first pass completes.
//!
//! # How stale the floor can be
//!
//! While the daemon runs, the snapshot is at most one `fleet.syncIntervalSecs`
//! old (300 s by default), plus however long a pass takes. When the daemon is
//! stopped it is as old as its last pass, which can be days. The answer
//! carries the snapshot's own `at` so the caller can show it; nothing here
//! fetches the store.
//!
//! The snapshot is read loosely, as JSON, for the three fields used here
//! (`repo`, `at`, `floor`). A snapshot written by an older or newer binary
//! whose other fields no longer match [`super::FleetSyncStatus`] still gives
//! its floor.

use std::path::Path;

use serde_json::Value;

use super::FloorKnowledge;
use crate::fleet_store::{self as store, StoreLocation};

/// What [`offline_floor`] found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OfflineFloor {
    /// `NoStore` (not a fleet host), `Unknown` (a fleet host whose floor is
    /// not known, and why) or `Set` (the floor in the snapshot).
    pub knowledge: FloorKnowledge,
    /// The fleet store, `OWNER/REPO`, when one is named.
    pub store: Option<String>,
    /// When the snapshot the floor came from was written (its `at`).
    pub snapshot_at: Option<String>,
}

/// The host-level snapshot, reduced to what the floor needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Snapshot {
    /// No snapshot file: no fleet-sync pass has completed on this host.
    Missing,
    /// A snapshot exists but could not be read or parsed.
    Unreadable(String),
    /// The fields the floor needs.
    Found {
        /// `repo`, the store the pass read.
        repo: Option<String>,
        /// `at`, when the pass ran.
        at: Option<String>,
        /// `floor.floor`.
        floor: Option<String>,
        /// `floor.error`, why the pass could not read a floor.
        error: Option<String>,
    },
}

/// Classify this host for the workspace at `workspace`, from its effective
/// config, the environment and the on-disk snapshot. Never contacts the daemon
/// or the forge.
#[must_use]
pub fn offline_floor(workspace: &Path) -> OfflineFloor {
    let effective = crate::config_resolver::resolve_effective_config(workspace);
    let location = store::resolve_location(&effective, &|k| std::env::var(k).ok())
        .map_err(|e| format!("{e:#}"));
    let snapshot = match super::status_path() {
        Some(path) => read_snapshot(&path),
        None => Snapshot::Missing,
    };
    classify(location, snapshot)
}

/// Read the snapshot at `path` loosely (see the module doc).
#[must_use]
pub fn read_snapshot(path: &Path) -> Snapshot {
    let raw = match std::fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Snapshot::Missing,
        Err(e) => return Snapshot::Unreadable(format!("could not read {}: {e}", path.display())),
    };
    let value: Value = match serde_json::from_str(&raw) {
        Ok(v) => v,
        Err(e) => {
            return Snapshot::Unreadable(format!("could not parse {}: {e}", path.display()));
        }
    };
    let text = |v: Option<&Value>| v.and_then(Value::as_str).map(str::to_string);
    Snapshot::Found {
        repo: text(value.get("repo")),
        at: text(value.get("at")),
        floor: text(value.pointer("/floor/floor")),
        error: text(value.pointer("/floor/error")),
    }
}

/// [`offline_floor`] over plain values.
#[must_use]
pub fn classify(
    location: Result<Option<StoreLocation>, String>,
    snapshot: Snapshot,
) -> OfflineFloor {
    let location = match location {
        // No store in this process's config: the snapshot decides.
        Ok(None) => return from_snapshot_alone(snapshot),
        Err(why) => {
            return OfflineFloor {
                knowledge: FloorKnowledge::Unknown(format!(
                    "a fleet store is named but unusable: {why}"
                )),
                store: None,
                snapshot_at: None,
            }
        }
        Ok(Some(location)) => location,
    };
    let unknown = |why: String, at: Option<String>| OfflineFloor {
        knowledge: FloorKnowledge::Unknown(why),
        store: Some(location.repo.clone()),
        snapshot_at: at,
    };
    match snapshot {
        Snapshot::Missing => unknown(
            "no fleet-sync snapshot records a floor: the daemon has not completed a fleet-sync \
             pass on this host"
                .to_string(),
            None,
        ),
        Snapshot::Unreadable(why) => unknown(why, None),
        Snapshot::Found {
            repo: Some(repo),
            at,
            ..
        } if repo != location.repo => unknown(
            format!(
                "the fleet-sync snapshot is for store {repo}, not the configured {}",
                location.repo
            ),
            at,
        ),
        Snapshot::Found {
            floor: Some(floor),
            at,
            ..
        } => OfflineFloor {
            knowledge: FloorKnowledge::Set(floor),
            store: Some(location.repo.clone()),
            snapshot_at: at,
        },
        Snapshot::Found { error, at, .. } => unknown(
            error.unwrap_or_else(|| {
                format!("the last fleet-sync pass recorded no `{}`", crate::fleet_store::floor::KEY)
            }),
            at,
        ),
    }
}

/// No store in this process's config. A snapshot on disk still makes this a
/// fleet host (see the module doc); none means it is not one.
fn from_snapshot_alone(snapshot: Snapshot) -> OfflineFloor {
    match snapshot {
        Snapshot::Missing => OfflineFloor {
            knowledge: FloorKnowledge::NoStore,
            store: None,
            snapshot_at: None,
        },
        Snapshot::Unreadable(why) => OfflineFloor {
            knowledge: FloorKnowledge::Unknown(why),
            store: None,
            snapshot_at: None,
        },
        Snapshot::Found {
            repo,
            at,
            floor,
            error,
        } => OfflineFloor {
            knowledge: match floor {
                Some(floor) => FloorKnowledge::Set(floor),
                None => FloorKnowledge::Unknown(error.unwrap_or_else(|| {
                    format!(
                        "the last fleet-sync pass recorded no `{}`",
                        crate::fleet_store::floor::KEY
                    )
                })),
            },
            store: repo,
            snapshot_at: at,
        },
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn loc(repo: &str) -> Result<Option<StoreLocation>, String> {
        Ok(Some(StoreLocation {
            repo: repo.to_string(),
            reference: "main".to_string(),
        }))
    }

    fn found(repo: &str, floor: Option<&str>, error: Option<&str>) -> Snapshot {
        Snapshot::Found {
            repo: Some(repo.to_string()),
            at: Some("2026-10-08T00:00:00Z".to_string()),
            floor: floor.map(str::to_string),
            error: error.map(str::to_string),
        }
    }

    #[test]
    fn no_store_and_no_snapshot_is_not_a_fleet_host() {
        let got = classify(Ok(None), Snapshot::Missing);
        assert_eq!(got.knowledge, FloorKnowledge::NoStore);
        assert_eq!(got.store, None);
    }

    /// The store named only in the daemon's supervisor environment: the
    /// operator's shell resolves no store, but the snapshot the daemon wrote
    /// says this is a fleet host and gives its floor.
    #[test]
    fn a_snapshot_alone_makes_a_fleet_host() {
        let got = classify(Ok(None), found("o/fleet", Some("0.19.950"), None));
        assert_eq!(got.knowledge, FloorKnowledge::Set("0.19.950".to_string()));
        assert_eq!(got.store.as_deref(), Some("o/fleet"));
        let got = classify(Ok(None), found("o/fleet", None, None));
        assert!(matches!(got.knowledge, FloorKnowledge::Unknown(_)));
        let got = classify(Ok(None), Snapshot::Unreadable("could not parse".to_string()));
        assert!(matches!(got.knowledge, FloorKnowledge::Unknown(_)), "fail closed");
    }

    #[test]
    fn a_named_but_unusable_store_is_a_fleet_host_with_an_unknown_floor() {
        let got = classify(Err("fleet store `x` is not OWNER/REPO".to_string()), Snapshot::Missing);
        let FloorKnowledge::Unknown(why) = got.knowledge else {
            panic!("an unusable store is unknown, never `NoStore`");
        };
        assert!(why.contains("not OWNER/REPO"), "{why}");
    }

    #[test]
    fn the_snapshot_floor_is_the_floor_for_its_own_store() {
        let got = classify(loc("o/fleet"), found("o/fleet", Some("0.19.950"), None));
        assert_eq!(got.knowledge, FloorKnowledge::Set("0.19.950".to_string()));
        assert_eq!(got.store.as_deref(), Some("o/fleet"));
        assert_eq!(got.snapshot_at.as_deref(), Some("2026-10-08T00:00:00Z"));
    }

    #[test]
    fn a_fleet_host_with_no_usable_snapshot_floor_is_unknown_and_says_why() {
        let why = |s: Snapshot| match classify(loc("o/fleet"), s).knowledge {
            FloorKnowledge::Unknown(why) => why,
            other => panic!("expected unknown, got {other:?}"),
        };
        assert!(why(Snapshot::Missing).contains("no fleet-sync snapshot"));
        assert!(why(Snapshot::Unreadable("could not parse x".to_string())).contains("parse"));
        assert!(why(found("o/other", Some("0.19.950"), None)).contains("o/other"));
        assert!(why(found("o/fleet", None, None)).contains("loom_min_version"));
        assert_eq!(
            why(found("o/fleet", None, Some("fleet.json: malformed"))),
            "fleet.json: malformed"
        );
    }

    #[test]
    fn the_snapshot_is_read_loosely_for_the_floor_fields_only() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fleet-sync-status.json");
        assert_eq!(read_snapshot(&path), Snapshot::Missing);
        // Not a full `FleetSyncStatus`: a strict parse would reject it.
        std::fs::write(
            &path,
            r#"{"repo":"o/fleet","at":"2026-10-08T01:02:03Z","floor":{"floor":"0.19.950"}}"#,
        )
        .unwrap();
        assert_eq!(read_snapshot(&path), found_at("0.19.950", "2026-10-08T01:02:03Z"));
        std::fs::write(&path, "{not json").unwrap();
        assert!(matches!(read_snapshot(&path), Snapshot::Unreadable(_)));
    }

    fn found_at(floor: &str, at: &str) -> Snapshot {
        Snapshot::Found {
            repo: Some("o/fleet".to_string()),
            at: Some(at.to_string()),
            floor: Some(floor.to_string()),
            error: None,
        }
    }
}
