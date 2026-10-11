//! Restart-only fleet-config changes arm a restart (Issue #10720).
//!
//! A fleet-store commit that changes a restart-required key
//! ([`crate::fleet_store::reload`]) used to land on disk and then do nothing
//! until something else restarted the daemon. This module turns it into a
//! process-wide **demand** ([`config_restart_demand`]) that the self-update
//! loop arms through the one roll path (`trigger_pause_roll`, source
//! `config_restart`).
//!
//! # Detection: against what this process booted with
//!
//! Not "this tier drifted" (after a write the file equals the render, so drift
//! vanishes) and not "this pass wrote" (a hand-run `fleet-config render`, or a
//! startup pass that overran its cap and finished in the background, writes
//! too). Instead:
//!
//! - **Boot snapshot** ([`snapshot_boot`]): when `fleet_sync::start` has run
//!   its startup pass, the two tiers fleet-sync renders (the machine tier and
//!   `<workspace>/.loom-local/local.json`) are read and kept for the life of
//!   the process. That is what the config-dependent loops go on to read.
//! - **Each timer pass** ([`after_pass`]): both files are read again, the
//!   changed dotted paths collected with [`render::diff_paths`] and split with
//!   [`reload::partition`]. A file missing or unparseable on either side (and
//!   not identically so on both) is restart-required as a whole, named
//!   `<tier> (whole file)`, the same rule `fleet-config render` uses.
//! - **Demand**: a non-empty restart-required set. It holds the paths, a
//!   fingerprint of the paths and their current values, and when that
//!   fingerprint was first seen. An empty set clears it.
//!
//! So every change since boot, over any number of passes, is one demand; a
//! change reverted before the restart clears it; and the restarted process's
//! snapshot equals the disk, so it has no demand (no loop by construction).
//!
//! # What is never published
//!
//! **Values.** These tiers can name key files and endpoints, so only paths are
//! logged, recorded in the pending-restart marker, or handed to the
//! self-update loop. The fingerprint is a sha256 and never leaves the process.
//!
//! # Limits
//!
//! The demand is armable only on a `fleet.autoApply` host ([`Mode::Write`]).
//! On a [`Mode::Check`] host it is recorded (the `Pending restart:` status
//! line) and nothing is armed.

use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use chrono::{DateTime, Utc};
use serde_json::Value;
use sha2::{Digest, Sha256};

use super::Mode;
use crate::fleet_store::render::{self, Tier};
use crate::fleet_store::{pending_restart, reload};

/// One config tier as read from disk.
#[derive(Debug, Clone, PartialEq)]
pub enum TierFile {
    /// No file.
    Missing,
    /// A file that does not parse as JSON (or cannot be read), identified by
    /// a digest of its bytes (or of the read error kind) so two reads of the
    /// same broken file compare equal.
    Unparseable(String),
    /// The parsed file.
    Parsed(Value),
}

impl TierFile {
    /// Read `path`. Never fails: see the variants.
    #[must_use]
    pub fn read(path: &Path) -> Self {
        match std::fs::read(path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Self::Missing,
            Err(e) => Self::Unparseable(format!("unreadable:{:?}", e.kind())),
            Ok(bytes) => match serde_json::from_slice(&bytes) {
                Ok(value) => Self::Parsed(value),
                Err(_) => Self::Unparseable(hex::encode(Sha256::digest(&bytes))),
            },
        }
    }

    /// A digest of this state, for the fingerprint (never published).
    fn digest(&self) -> String {
        match self {
            Self::Missing => "missing".to_string(),
            Self::Unparseable(id) => format!("unparseable:{id}"),
            Self::Parsed(v) => hex::encode(Sha256::digest(v.to_string().as_bytes())),
        }
    }
}

/// One tier: its name (`machine tier` / `host-local tier`), path and content.
#[derive(Debug, Clone, PartialEq)]
pub struct TierSnapshot {
    /// [`Tier::name`].
    pub tier: &'static str,
    /// Where it was read from.
    pub path: PathBuf,
    /// What was there.
    pub file: TierFile,
}

/// Read the tiers fleet-sync renders: the machine tier when one resolves, and
/// the host-local tier at `local`.
#[must_use]
pub fn read_tiers(machine: Option<&Path>, local: &Path) -> Vec<TierSnapshot> {
    let mut tiers = Vec::with_capacity(2);
    if let Some(machine) = machine {
        tiers.push(TierSnapshot {
            tier: Tier::Machine.name(),
            path: machine.to_path_buf(),
            file: TierFile::read(machine),
        });
    }
    tiers.push(TierSnapshot {
        tier: Tier::Local.name(),
        path: local.to_path_buf(),
        file: TierFile::read(local),
    });
    tiers
}

/// The tiers for the daemon workspace at `workspace`.
fn read_workspace_tiers(workspace: &Path) -> Vec<TierSnapshot> {
    let machine = crate::config_resolver::private_defaults_path();
    read_tiers(machine.as_deref(), &workspace.join(crate::config_resolver::LOCAL_CONFIG_REL))
}

/// What differs between the boot snapshot and now.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Changes {
    /// Changed paths a running daemon picks up by itself.
    pub live: Vec<String>,
    /// Changed paths that need a restart, in tier order.
    pub restart_required: Vec<String>,
    /// sha256 over the restart-required paths and their current values. Empty
    /// when there are none.
    pub fingerprint: String,
}

/// Compare `now` against `boot`. A tier present now but absent from `boot`
/// (a machine tier that did not resolve at boot) compares against
/// [`TierFile::Missing`].
#[must_use]
pub fn compare(boot: &[TierSnapshot], now: &[TierSnapshot]) -> Changes {
    let mut changes = Changes::default();
    let mut hasher = Sha256::new();
    for tier in now {
        let before = boot
            .iter()
            .find(|b| b.tier == tier.tier && b.path == tier.path)
            .map_or(&TierFile::Missing, |b| &b.file);
        match (before, &tier.file) {
            (TierFile::Parsed(b), TierFile::Parsed(n)) => {
                let mut paths = Vec::new();
                render::diff_paths("", b, n, &mut paths);
                let paths = expand_added_or_removed(paths, b, n);
                let (live, restart_required) = reload::partition(&paths);
                changes.live.extend(live);
                for path in restart_required {
                    let current = if path == "." {
                        Some(n)
                    } else {
                        crate::config_resolver::get_path(n, &path)
                    };
                    let value = current.map_or_else(|| "absent".to_string(), Value::to_string);
                    hasher.update(format!("{}\0{path}\0{value}\n", tier.tier).as_bytes());
                    changes.restart_required.push(path);
                }
            }
            (b, n) if b == n => {}
            (_, n) => {
                let path = format!("{} (whole file)", tier.tier);
                hasher.update(format!("{}\0{path}\0{}\n", tier.tier, n.digest()).as_bytes());
                changes.restart_required.push(path);
            }
        }
    }
    if !changes.restart_required.is_empty() {
        changes.fingerprint = hex::encode(hasher.finalize());
    }
    changes
}

/// [`render::diff_paths`] names an added or removed key at the key itself, so
/// a block added with only live knobs in it (`autonomous.workFinder:
/// {maxConcurrent}`) would be classified by the block's entry. Expand such a
/// path into the leaf paths of the object that was added or removed, so each
/// knob is classified on its own.
fn expand_added_or_removed(paths: Vec<String>, before: &Value, now: &Value) -> Vec<String> {
    let get = |v: &'_ Value, p: &str| crate::config_resolver::get_path(v, p).cloned();
    let mut out = Vec::with_capacity(paths.len());
    for path in paths {
        let object = match (get(before, &path), get(now, &path)) {
            (None, Some(v @ Value::Object(_))) | (Some(v @ Value::Object(_)), None) => v,
            _ => {
                out.push(path);
                continue;
            }
        };
        let start = out.len();
        leaves(&path, &object, &mut out);
        if out.len() == start {
            out.push(path); // an empty object: the key itself is the change.
        }
    }
    out
}

/// Every leaf path under `value`, prefixed with `path`.
fn leaves(path: &str, value: &Value, out: &mut Vec<String>) {
    match value {
        Value::Object(map) => {
            for (k, v) in map {
                leaves(&format!("{path}.{k}"), v, out);
            }
        }
        _ => out.push(path.to_string()),
    }
}

/// A standing restart-required change since this process booted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigRestartDemand {
    /// The restart-required dotted paths (paths only, never values).
    pub paths: Vec<String>,
    /// sha256 of the paths and their current values (never published).
    pub fingerprint: String,
    /// When this fingerprint was first seen.
    pub first_seen: DateTime<Utc>,
    /// Whether this host may act on it: `fleet.autoApply` is on
    /// ([`Mode::Write`]). Otherwise it is reported and nothing is armed.
    pub armable: bool,
}

/// The process's boot snapshot and current demand. Pure of I/O so tests drive
/// it directly; [`after_pass`] wraps the process-wide one.
#[derive(Debug, Default)]
pub struct Tracker {
    boot: Option<Vec<TierSnapshot>>,
    demand: Option<ConfigRestartDemand>,
}

/// What one observation changed, for the log line and the status marker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Transition {
    /// Nothing new: no demand, or the same one as last pass.
    Unchanged,
    /// A new demand, or one whose paths, values or armability changed.
    Raised(ConfigRestartDemand),
    /// The standing demand cleared (the change was reverted).
    Cleared,
}

impl Tracker {
    /// A tracker whose boot snapshot is `boot`.
    #[must_use]
    pub fn booted(boot: Vec<TierSnapshot>) -> Self {
        Self {
            boot: Some(boot),
            demand: None,
        }
    }

    /// The standing demand.
    #[must_use]
    pub fn demand(&self) -> Option<&ConfigRestartDemand> {
        self.demand.as_ref()
    }

    /// Compare `now` against the boot snapshot and update the demand. With no
    /// boot snapshot (not a fleet host, or `start` has not finished) there is
    /// never a demand.
    pub fn observe(&mut self, now: &[TierSnapshot], mode: Mode, at: DateTime<Utc>) -> Transition {
        let Some(boot) = self.boot.as_deref() else {
            return Transition::Unchanged;
        };
        let changes = compare(boot, now);
        let armable = mode == Mode::Write;
        let previous = self.demand.take();
        if changes.restart_required.is_empty() {
            return if previous.is_some() {
                Transition::Cleared
            } else {
                Transition::Unchanged
            };
        }
        let same = previous
            .as_ref()
            .filter(|p| p.fingerprint == changes.fingerprint);
        let next = ConfigRestartDemand {
            paths: changes.restart_required,
            first_seen: same.map_or(at, |p| p.first_seen),
            fingerprint: changes.fingerprint,
            armable,
        };
        let transition = if same.is_some_and(|p| p.armable == armable) {
            Transition::Unchanged
        } else {
            Transition::Raised(next.clone())
        };
        self.demand = Some(next);
        transition
    }
}

fn tracker() -> &'static Mutex<Tracker> {
    static CELL: OnceLock<Mutex<Tracker>> = OnceLock::new();
    CELL.get_or_init(|| Mutex::new(Tracker::default()))
}

/// Take the boot snapshot (see the module doc). Called once, when
/// `fleet_sync::start` has finished its startup pass.
pub(super) fn snapshot_boot(workspace: &Path) {
    let boot = read_workspace_tiers(workspace);
    if let Ok(mut t) = tracker().lock() {
        *t = Tracker::booted(boot);
    }
}

/// The restart-required config change standing since this process booted, as
/// of the last fleet-sync timer pass; `None` when there is none, on a host
/// with no fleet store, or before the first pass. The self-update loop reads
/// it every tick (`auto_update::config_restart`).
#[must_use]
pub fn config_restart_demand() -> Option<ConfigRestartDemand> {
    tracker().lock().ok().and_then(|t| t.demand().cloned())
}

/// One timer pass's comparison (see the module doc): re-read the tiers,
/// update the process-wide demand, and log and record a change of it. Paths
/// only, never values.
pub(super) fn after_pass(workspace: &Path, mode: Mode) {
    let now = read_workspace_tiers(workspace);
    let transition = match tracker().lock() {
        Ok(mut t) => t.observe(&now, mode, Utc::now()),
        Err(_) => return,
    };
    announce(&transition);
}

/// Log and record a [`Transition`]. Paths only.
fn announce(transition: &Transition) {
    match transition {
        Transition::Unchanged => {}
        Transition::Raised(demand) => {
            log::warn!("fleet_sync: {}", describe(demand));
            if let Err(e) = pending_restart::record_by_daemon(
                demand.paths.clone(),
                std::process::id(),
                demand.first_seen,
                demand.armable,
            ) {
                log::warn!("fleet_sync: could not record the pending restart: {e:#}");
            }
        }
        Transition::Cleared => {
            log::info!(
                "fleet_sync: the restart-required config change since this daemon started was \
                 reverted; no restart is needed"
            );
            // Only a marker this process wrote (or one pending against it):
            // the disk is back to what this process booted with.
            if pending_restart::probe().is_some_and(|m| m.observed_pid == std::process::id()) {
                pending_restart::clear();
            }
        }
    }
}

/// The log line for a raised demand: the paths and what happens next.
#[must_use]
pub fn describe(demand: &ConfigRestartDemand) -> String {
    let next = if demand.armable {
        "the daemon restarts itself (pause-and-roll, same binary) on its next self-update tick"
    } else {
        "fleet.autoApply is off on this host, so nothing is armed; restart the daemon to apply"
    };
    format!(
        "restart-required config changed since this daemon started ({} path(s): {}) — {next}",
        demand.paths.len(),
        demand.paths.join(", ")
    )
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
#[path = "tests/config_restart.rs"]
mod tests;
