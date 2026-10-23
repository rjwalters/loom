//! Restart-required config changes since this process booted (#12424, child 2
//! of #12419).
//!
//! Nothing in `fleet_sync` used to classify what a render changed: the only
//! `Pending restart:` marker is written by the `fleet-config render` CLI, and a
//! timer-driven render leaves no drift behind to notice. The signal that
//! survives every writer is the files themselves, compared with how they looked
//! when this process booted.
//!
//! [`record_boot`] snapshots the two rendered tiers (machine tier and the
//! host-local tier) once `fleet_sync::start` has finished its startup pass.
//! Every timer pass then calls [`check`], which re-reads both, collects the
//! dotted paths that changed ([`render::diff_paths`]) and keeps those that
//! [`reload::partition`] calls restart-required. A non-empty set is the
//! process-wide [`config_restart_demand`]; an empty one clears it, so A -> B ->
//! A leaves no demand. A tier file that is missing or unparseable on either
//! side counts as restart-required as a whole, the rule `fleet-config render`
//! applies.
//!
//! Paths only: the demand, its fingerprint and every log line carry dotted
//! paths, never a config value (the fingerprint hashes values but exposes only
//! the digest).

use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

use chrono::{DateTime, Utc};
use serde_json::Value;
use sha2::{Digest, Sha256};

use super::Mode;
use crate::fleet_store::render::{self, Tier};
use crate::fleet_store::{pending_restart, reload};

/// Restart-required config changes seen since boot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigRestartDemand {
    /// The restart-required dotted paths (a whole-file change is named
    /// `<tier> (whole file)`). Sorted and de-duplicated.
    pub paths: Vec<String>,
    /// A digest of the paths and their current values: it moves when a value
    /// changes again, and says nothing about what the value is.
    pub fingerprint: String,
    /// When this demand was first seen. Kept while the demand stays non-empty.
    pub first_seen: DateTime<Utc>,
}

/// One tier file as read: `None` when missing, unreadable or unparseable.
type TierRead = Option<Value>;

/// The tiers compared, in report order.
#[derive(Debug, Clone, PartialEq)]
pub struct TierFiles {
    files: Vec<(Tier, PathBuf)>,
}

impl TierFiles {
    /// The machine tier and the host-local tier of `workspace`.
    #[must_use]
    pub fn for_workspace(workspace: &std::path::Path) -> Self {
        let mut files = Vec::new();
        if let Some(machine) = crate::config_resolver::private_defaults_path() {
            files.push((Tier::Machine, machine));
        }
        files.push((Tier::Local, workspace.join(crate::config_resolver::LOCAL_CONFIG_REL)));
        Self { files }
    }

    fn read(&self) -> Vec<TierRead> {
        self.files
            .iter()
            .map(|(_, path)| {
                let text = std::fs::read_to_string(path).ok()?;
                serde_json::from_str::<Value>(&text).ok()
            })
            .collect()
    }
}

/// The tiers as they were at boot.
#[derive(Debug, Clone, PartialEq)]
pub struct BootSnapshot {
    files: TierFiles,
    tiers: Vec<TierRead>,
}

impl BootSnapshot {
    /// Read `files` now.
    #[must_use]
    pub fn take(files: TierFiles) -> Self {
        let tiers = files.read();
        Self { files, tiers }
    }
}

/// Compare `boot` with the tiers on disk now.
///
/// `previous` is the demand standing from an earlier pass: an unchanged
/// restart-required set keeps its `first_seen`.
#[must_use]
pub fn compare(
    boot: &BootSnapshot,
    previous: Option<&ConfigRestartDemand>,
    now: DateTime<Utc>,
) -> Option<ConfigRestartDemand> {
    let current = boot.files.read();
    let mut paths = Vec::new();
    let mut hasher = Sha256::new();
    for (((tier, _), before), after) in boot.files.files.iter().zip(&boot.tiers).zip(&current) {
        match (before, after) {
            (Some(b), Some(a)) => {
                let mut changed = Vec::new();
                render::diff_paths("", b, a, &mut changed);
                let (_live, restart) = reload::partition(&changed);
                for path in restart {
                    hasher.update(path.as_bytes());
                    hasher.update(value_at(a, &path).to_string().as_bytes());
                    paths.push(path);
                }
            }
            (None, None) => {}
            (_, after) => {
                let path = format!("{} (whole file)", tier.name());
                hasher.update(path.as_bytes());
                hasher.update(
                    after
                        .as_ref()
                        .map_or(String::new(), Value::to_string)
                        .as_bytes(),
                );
                paths.push(path);
            }
        }
    }
    if paths.is_empty() {
        return None;
    }
    paths.sort();
    paths.dedup();
    let fingerprint = hex::encode(hasher.finalize());
    let first_seen = previous.map_or(now, |p| p.first_seen);
    Some(ConfigRestartDemand {
        paths,
        fingerprint,
        first_seen,
    })
}

/// The value at the dotted `path` of `root`, `Null` when it is gone (a key
/// containing a dot is not found; the path still moves the fingerprint).
fn value_at(root: &Value, path: &str) -> Value {
    if path == "." {
        return root.clone();
    }
    path.split('.')
        .try_fold(root, |v, key| v.get(key))
        .cloned()
        .unwrap_or(Value::Null)
}

fn boot_cell() -> &'static Mutex<Option<BootSnapshot>> {
    static CELL: OnceLock<Mutex<Option<BootSnapshot>>> = OnceLock::new();
    CELL.get_or_init(|| Mutex::new(None))
}

fn demand_cell() -> &'static Mutex<Option<ConfigRestartDemand>> {
    static CELL: OnceLock<Mutex<Option<ConfigRestartDemand>>> = OnceLock::new();
    CELL.get_or_init(|| Mutex::new(None))
}

/// Snapshot the rendered tiers of `workspace` for the rest of the process
/// lifetime. Called once, when `fleet_sync::start` returns.
pub fn record_boot(workspace: &std::path::Path) {
    let snapshot = BootSnapshot::take(TierFiles::for_workspace(workspace));
    if let Ok(mut guard) = boot_cell().lock() {
        *guard = Some(snapshot);
    }
    set_demand(None);
}

/// The restart-required config changes since this process booted, `None` when
/// there are none (or no boot snapshot exists). Beside
/// [`super::loom_min_version`]: updated every timer pass, read by whatever acts
/// on it.
#[must_use]
pub fn config_restart_demand() -> Option<ConfigRestartDemand> {
    demand_cell().lock().ok().and_then(|g| g.clone())
}

fn set_demand(demand: Option<ConfigRestartDemand>) {
    if let Ok(mut guard) = demand_cell().lock() {
        *guard = demand;
    }
}

/// One timer pass: refresh the demand from the tiers on disk. On a
/// [`Mode::Write`] host a demand that is new or has moved is also recorded as
/// the `Pending restart:` marker; [`Mode::Check`] only reports it.
pub fn check(mode: Mode, now: DateTime<Utc>) {
    let Some(boot) = boot_cell().lock().ok().and_then(|g| g.clone()) else {
        return;
    };
    let previous = config_restart_demand();
    let demand = compare(&boot, previous.as_ref(), now);
    let changed =
        demand.as_ref().map(|d| &d.fingerprint) != previous.as_ref().map(|d| &d.fingerprint);
    if changed {
        match &demand {
            Some(d) => {
                log::warn!(
                    "fleet_sync: config changed since boot needs a daemon restart: {}",
                    d.paths.join(", ")
                );
                if mode == Mode::Write {
                    if let Err(e) =
                        pending_restart::record(d.paths.clone(), std::process::id(), now)
                    {
                        log::warn!("fleet_sync: could not record the pending restart: {e:#}");
                    }
                }
            }
            None => log::info!("fleet_sync: config is back to what this process booted with"),
        }
    }
    set_demand(demand);
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
#[path = "tests/config_restart.rs"]
mod tests;
