//! `repos.yml` → the desired workspace set, and its diff against the daemon's
//! workspace registry.
//!
//! # Contract
//!
//! ```yaml
//! root: ~/GitHub          # where every repo is cloned (`~` = home)
//! repos:
//!   - name: app           # unique
//!     dir: app            # clone directory under root (default: name)
//!     remote: git@github.com:acme/app.git
//!     fleet: true         # the daemon manages it (default false)
//!     fleet_priority: 10  # dispatch tier, lower first (default 100)
//!     firewall: true      # must never be an unattended-agent target (default false)
//! ```
//!
//! Other keys are ignored. The desired set is every record with `fleet: true`
//! and not `firewall: true`. A record with **both** is a hard error for the
//! whole roster, never a silent exclusion: the manifest is then in exactly the
//! state `firewall` exists to prevent, and quietly dropping the repo would hide
//! the drift a human needs to see. A non-boolean flag, a non-integer
//! priority, a duplicate name or dir, or an unsafe `dir` are hard errors too.
//!
//! # Plan
//!
//! Against the registry: **adds** (desired, not registered — only applicable
//! when already cloned under `root`; a missing clone is reported, never
//! cloned), **removes** (registered, and the store has a record for that path
//! that is not desired), and **priority changes**. A registered workspace the
//! store has no record for is *unmanaged* and left alone — the store only
//! governs the repos it names.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Result};
use serde::Serialize;
use serde_json::Value;

use crate::workspace_registry::DEFAULT_WORKSPACE_PRIORITY;

/// One `repos[]` record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Record {
    /// `name`.
    pub name: String,
    /// `dir` (defaults to `name`).
    pub dir: String,
    /// `remote`, when a string.
    pub remote: Option<String>,
    /// `fleet`.
    pub fleet: bool,
    /// `firewall`.
    pub firewall: bool,
    /// `fleet_priority`, when set.
    pub fleet_priority: Option<u32>,
}

/// A parsed, validated roster.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Roster {
    /// `root`, with `~` expanded.
    pub root: PathBuf,
    /// Every record, in file order.
    pub records: Vec<Record>,
}

/// A workspace the store wants registered.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Desired {
    /// Record name.
    pub name: String,
    /// `root/dir`.
    pub path: PathBuf,
    /// Priority tier.
    pub priority: u32,
}

impl Roster {
    /// The desired workspace set: `fleet: true` and not `firewall: true`, in
    /// file order. (Both flags together were refused at parse time.)
    #[must_use]
    pub fn desired(&self) -> Vec<Desired> {
        self.records
            .iter()
            .filter(|r| r.fleet && !r.firewall)
            .map(|r| Desired {
                name: r.name.clone(),
                path: self.root.join(&r.dir),
                priority: r.fleet_priority.unwrap_or(DEFAULT_WORKSPACE_PRIORITY),
            })
            .collect()
    }
}

/// Parse and validate `repos.yml`. `home` expands a leading `~` in `root`.
pub fn parse(text: &str, home: &Path) -> Result<Roster> {
    let doc = super::yaml::parse(text).map_err(|e| anyhow!("repos.yml: {e:#}"))?;
    let top = doc
        .as_object()
        .ok_or_else(|| anyhow!("repos.yml: top level must be a mapping"))?;
    let root = top
        .get("root")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("repos.yml: `root` must be a string"))?;
    let root = expand_root(root, home)?;
    let repos = top
        .get("repos")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("repos.yml: `repos` must be a list"))?;

    let mut records = Vec::with_capacity(repos.len());
    let mut errors = Vec::new();
    for (i, item) in repos.iter().enumerate() {
        match record(item) {
            Ok(r) => records.push(r),
            Err(e) => errors.push(format!("repos[{i}]: {e}")),
        }
    }
    let mut names = BTreeSet::new();
    let mut dirs = BTreeSet::new();
    for r in &records {
        if !names.insert(r.name.as_str()) {
            errors.push(format!("duplicate name `{}`", r.name));
        }
        if !dirs.insert(r.dir.as_str()) {
            errors.push(format!("duplicate dir `{}`", r.dir));
        }
        if r.fleet && r.firewall {
            errors.push(format!(
                "record `{}` (dir={}) is fleet: true AND firewall: true — refusing the whole roster \
                 rather than silently excluding it; fix the manifest",
                r.name, r.dir
            ));
        }
    }
    if !errors.is_empty() {
        bail!("repos.yml is not a valid roster:\n  {}", errors.join("\n  "));
    }
    Ok(Roster { root, records })
}

fn expand_root(root: &str, home: &Path) -> Result<PathBuf> {
    let p = if root == "~" {
        home.to_path_buf()
    } else if let Some(rest) = root.strip_prefix("~/") {
        home.join(rest)
    } else {
        PathBuf::from(root)
    };
    if !p.is_absolute() {
        bail!("repos.yml: `root` must be absolute or start with `~/` (got `{root}`)");
    }
    Ok(p)
}

fn record(item: &Value) -> Result<Record, String> {
    let m = item.as_object().ok_or("not a mapping")?;
    let name = m
        .get("name")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or("`name` must be a non-empty string")?
        .to_string();
    let dir = match m.get("dir") {
        None | Some(Value::Null) => name.clone(),
        Some(Value::String(d)) => d.clone(),
        Some(_) => return Err(format!("`{name}`: `dir` must be a string")),
    };
    if dir.is_empty() || dir == "." || dir == ".." || dir.contains('/') || dir.contains('\\') {
        return Err(format!("`{name}`: `dir` must be a single directory name (got `{dir}`)"));
    }
    let flag = |key: &str| -> Result<bool, String> {
        match m.get(key) {
            None | Some(Value::Null) => Ok(false),
            Some(Value::Bool(b)) => Ok(*b),
            Some(other) => Err(format!("`{name}`: `{key}` must be true or false (got {other})")),
        }
    };
    let fleet = flag("fleet")?;
    let firewall = flag("firewall")?;
    let fleet_priority = match m.get("fleet_priority") {
        None | Some(Value::Null) => None,
        Some(v) => Some(
            v.as_u64()
                .and_then(|n| u32::try_from(n).ok())
                .ok_or_else(|| {
                    format!("`{name}`: `fleet_priority` must be a non-negative integer (got {v})")
                })?,
        ),
    };
    let remote = m.get("remote").and_then(Value::as_str).map(str::to_string);
    Ok(Record {
        name,
        dir,
        remote,
        fleet,
        firewall,
        fleet_priority,
    })
}

/// A registered workspace, as the plan sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Registered {
    /// Normalized root.
    pub root: PathBuf,
    /// Priority tier.
    pub priority: u32,
}

/// One planned change.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "action", rename_all = "kebab-case")]
pub enum Change {
    /// Register a desired workspace.
    Add {
        /// Record name.
        name: String,
        /// Workspace root.
        path: PathBuf,
        /// Priority tier.
        priority: u32,
    },
    /// A desired workspace that cannot be added: not cloned under `root`.
    MissingClone {
        /// Record name.
        name: String,
        /// Expected clone path.
        path: PathBuf,
    },
    /// Deregister a workspace the store says must not be managed.
    Remove {
        /// Record name.
        name: String,
        /// Workspace root.
        path: PathBuf,
        /// `firewall` or `fleet: false`.
        reason: String,
    },
    /// Change a registered workspace's priority.
    SetPriority {
        /// Record name.
        name: String,
        /// Workspace root.
        path: PathBuf,
        /// Current tier.
        from: u32,
        /// Desired tier.
        to: u32,
    },
}

/// The roster diffed against the registry.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Plan {
    /// Changes, in apply order: removes, then adds, then priority changes.
    pub changes: Vec<Change>,
    /// Registered workspaces the store has no record for (left alone).
    pub unmanaged: Vec<PathBuf>,
    /// Desired workspaces already registered at the right priority.
    pub in_sync: usize,
}

impl Plan {
    /// Whether the registry already matches the store.
    #[must_use]
    pub fn is_in_sync(&self) -> bool {
        self.changes.is_empty()
    }

    /// `roster --check`'s exit code: `0` in sync, `1` drift.
    #[must_use]
    pub fn check_exit_code(&self) -> i32 {
        i32::from(!self.is_in_sync())
    }
}

/// Diff `roster` against `registered`. `normalize` maps a path to the form the
/// registry stores (`workspace_registry::normalize_path` in production);
/// `is_cloned` says whether a desired path is an existing clone.
pub fn plan(
    roster: &Roster,
    registered: &[Registered],
    normalize: &dyn Fn(&Path) -> PathBuf,
    is_cloned: &dyn Fn(&Path) -> bool,
) -> Plan {
    let mut removes = Vec::new();
    let mut adds = Vec::new();
    let mut reprioritize = Vec::new();
    let mut in_sync = 0;
    let known: Vec<(&Record, PathBuf)> = roster
        .records
        .iter()
        .map(|r| (r, normalize(&roster.root.join(&r.dir))))
        .collect();

    for reg in registered {
        if let Some((rec, _)) = known.iter().find(|(_, p)| *p == reg.root) {
            if !(rec.fleet && !rec.firewall) {
                removes.push(Change::Remove {
                    name: rec.name.clone(),
                    path: reg.root.clone(),
                    reason: if rec.firewall {
                        "firewall"
                    } else {
                        "fleet: false"
                    }
                    .to_string(),
                });
            }
        }
    }
    for want in roster.desired() {
        let path = normalize(&want.path);
        match registered.iter().find(|r| r.root == path) {
            None if is_cloned(&path) => adds.push(Change::Add {
                name: want.name,
                path,
                priority: want.priority,
            }),
            None => adds.push(Change::MissingClone {
                name: want.name,
                path,
            }),
            Some(r) if r.priority != want.priority => reprioritize.push(Change::SetPriority {
                name: want.name,
                path,
                from: r.priority,
                to: want.priority,
            }),
            Some(_) => in_sync += 1,
        }
    }
    let unmanaged = registered
        .iter()
        .filter(|r| !known.iter().any(|(_, p)| *p == r.root))
        .map(|r| r.root.clone())
        .collect();
    let mut changes = removes;
    changes.extend(adds);
    changes.extend(reprioritize);
    Plan {
        changes,
        unmanaged,
        in_sync,
    }
}

/// One line per change, for `--check` / `--apply` output.
#[must_use]
pub fn describe(change: &Change) -> String {
    match change {
        Change::Add { name, path, priority } => {
            format!("+ add      {name:<24} {} (priority {priority})", path.display())
        }
        Change::MissingClone { name, path } => format!(
            "! missing  {name:<24} {} — not cloned; clone it, then re-run (this command never clones)",
            path.display()
        ),
        Change::Remove { name, path, reason } => {
            format!("- remove   {name:<24} {} ({reason})", path.display())
        }
        Change::SetPriority { name, path, from, to } => {
            format!("~ priority {name:<24} {} ({from} -> {to})", path.display())
        }
    }
}

#[cfg(test)]
#[path = "tests/roster_tests.rs"]
mod tests;
