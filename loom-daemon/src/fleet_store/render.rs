//! Render a host's config from a store snapshot, detect drift, and write it.
//!
//! - **Machine tier** = `deep_merge(fleet/defaults.json,
//!   fleet/hosts/<host>/defaults.json)` via [`crate::config_resolver::deep_merge`]
//!   — the same function the resolver merges tiers with, so the store's merge
//!   semantics cannot drift from the daemon's. Written to the file the machine
//!   tier is read from ([`crate::config_resolver::private_defaults_path`]).
//! - **Host-local tier** = `fleet/hosts/<host>/local.json` verbatim, written to
//!   the daemon workspace's `.loom-local/local.json`
//!   ([`crate::config_resolver::LOCAL_CONFIG_REL`]). A store without that file
//!   leaves the local tier untouched.
//!
//! Drift is **semantic**: a target whose parsed JSON equals the rendered value
//! is in sync regardless of formatting, and is not rewritten (so a no-op render
//! never churns backups). A write keeps exactly one timestamped backup of the
//! file it replaces (`<file>.fleet-store-bak-<UTC>`); backups named any other
//! way are never touched.

use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use serde_json::Value;

use super::fetch::{write_atomic, Snapshot};

/// Suffix marker of the backups a render keeps.
pub const BACKUP_MARKER: &str = ".fleet-store-bak-";

/// Which tier a [`Target`] is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    /// The machine-level defaults tier.
    Machine,
    /// The workspace's host-local tier.
    Local,
}

impl Tier {
    /// Human name.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Tier::Machine => "machine tier",
            Tier::Local => "host-local tier",
        }
    }
}

/// One rendered file and where it goes.
#[derive(Debug, Clone, PartialEq)]
pub struct Target {
    /// Which tier.
    pub tier: Tier,
    /// Destination path.
    pub path: PathBuf,
    /// The rendered content.
    pub value: Value,
}

/// The rendered config for `host`: its machine tier, and its host-local tier
/// when the store has one.
pub fn render(
    snapshot: &Snapshot,
    host: &str,
    machine_path: &Path,
    local_path: &Path,
) -> Result<Vec<Target>> {
    super::validate_host(host)?;
    let base = require_object(snapshot, super::FLEET_DEFAULTS_PATH)?.ok_or_else(|| {
        anyhow!(
            "the store has no {} (commit {})",
            super::FLEET_DEFAULTS_PATH,
            snapshot.short_commit()
        )
    })?;
    let overlay_path = super::host_defaults_path(host);
    let overlay = require_object(snapshot, &overlay_path)?.ok_or_else(|| {
        anyhow!(
            "host `{host}` is not in the store (no {overlay_path} at commit {}); set LOOM_HOST_ID \
             or pass --host",
            snapshot.short_commit()
        )
    })?;
    let mut out = vec![Target {
        tier: Tier::Machine,
        path: machine_path.to_path_buf(),
        value: crate::config_resolver::deep_merge(&base, &overlay),
    }];
    if let Some(local) = require_object(snapshot, &super::host_local_path(host))? {
        out.push(Target {
            tier: Tier::Local,
            path: local_path.to_path_buf(),
            value: local,
        });
    }
    Ok(out)
}

/// A store file parsed as a JSON object; `Ok(None)` when absent. Unlike the
/// resolver's soft-fail tier read, a malformed store file is an error — a
/// render must never silently drop a tier.
fn require_object(snapshot: &Snapshot, path: &str) -> Result<Option<Value>> {
    let Some(text) = snapshot.text(path)? else {
        return Ok(None);
    };
    match serde_json::from_str::<Value>(&text)
        .with_context(|| format!("parsing store file {path}"))?
    {
        v @ Value::Object(_) => Ok(Some(v)),
        _ => bail!("store file {path} is not a JSON object"),
    }
}

/// How a target compares with what is on disk.
#[derive(Debug, Clone, PartialEq)]
pub enum Drift {
    /// Semantically identical.
    InSync,
    /// The file does not exist.
    Missing,
    /// The file exists but is not valid JSON.
    Unparseable(String),
    /// The file differs; one line per differing path.
    Differs(Vec<String>),
}

/// Compare `target` with its file on disk.
#[must_use]
pub fn drift(target: &Target) -> Drift {
    let text = match std::fs::read_to_string(&target.path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Drift::Missing,
        Err(e) => return Drift::Unparseable(e.to_string()),
    };
    match serde_json::from_str::<Value>(&text) {
        Ok(current) if current == target.value => Drift::InSync,
        Ok(current) => {
            let mut lines = Vec::new();
            diff_values("", &current, &target.value, &mut lines);
            Drift::Differs(lines)
        }
        Err(e) => Drift::Unparseable(e.to_string()),
    }
}

/// `render --check`'s exit code for these drift results: `0` when every
/// target is in sync, `1` when any drifts. (Errors exit `2` in the CLI.)
#[must_use]
pub fn check_exit_code(drifts: &[Drift]) -> i32 {
    i32::from(drifts.iter().any(|d| *d != Drift::InSync))
}

/// A path-by-path diff from `current` (on disk) to `wanted` (rendered):
/// `- path: old`, `+ path: new`, `~ path: old -> new`.
pub fn diff_values(path: &str, current: &Value, wanted: &Value, out: &mut Vec<String>) {
    match (current, wanted) {
        (Value::Object(c), Value::Object(w)) => {
            for (k, cv) in c {
                let p = join(path, k);
                match w.get(k) {
                    Some(wv) => diff_values(&p, cv, wv, out),
                    None => out.push(format!("- {p}: {}", brief(cv))),
                }
            }
            for (k, wv) in w {
                if !c.contains_key(k) {
                    out.push(format!("+ {}: {}", join(path, k), brief(wv)));
                }
            }
        }
        (c, w) if c == w => {}
        (c, w) => {
            let p = if path.is_empty() { "." } else { path };
            out.push(format!("~ {p}: {} -> {}", brief(c), brief(w)));
        }
    }
}

fn join(path: &str, key: &str) -> String {
    if path.is_empty() {
        key.to_string()
    } else {
        format!("{path}.{key}")
    }
}

fn brief(v: &Value) -> String {
    let s = v.to_string();
    if s.chars().count() > 120 {
        format!("{}…", s.chars().take(117).collect::<String>())
    } else {
        s
    }
}

/// Serialized form a target is written in: pretty JSON with a final newline.
pub fn serialize(value: &Value) -> Result<Vec<u8>> {
    let mut body = serde_json::to_vec_pretty(value)?;
    body.push(b'\n');
    Ok(body)
}

/// Write `target` if it drifts, keeping one backup of the replaced file.
/// Returns the backup path when one was made, and whether anything was
/// written.
pub fn write(target: &Target, stamp: &str) -> Result<(bool, Option<PathBuf>)> {
    if drift(target) == Drift::InSync {
        return Ok((false, None));
    }
    let backup = if target.path.exists() {
        let name = target
            .path
            .file_name()
            .ok_or_else(|| anyhow!("{} has no file name", target.path.display()))?
            .to_string_lossy()
            .to_string();
        let backup = target
            .path
            .with_file_name(format!("{name}{BACKUP_MARKER}{stamp}"));
        std::fs::copy(&target.path, &backup).with_context(|| {
            format!("backing up {} to {}", target.path.display(), backup.display())
        })?;
        prune_backups(&target.path, &name, &backup);
        Some(backup)
    } else {
        None
    };
    write_atomic(&target.path, &serialize(&target.value)?)?;
    Ok((true, backup))
}

/// Remove every `<name>.fleet-store-bak-*` beside `path` except `keep`.
fn prune_backups(path: &Path, name: &str, keep: &Path) {
    let Some(dir) = path.parent() else {
        return;
    };
    let prefix = format!("{name}{BACKUP_MARKER}");
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let p = entry.path();
        if p != keep && entry.file_name().to_string_lossy().starts_with(&prefix) {
            let _ = std::fs::remove_file(p);
        }
    }
}

#[cfg(test)]
#[path = "tests/render_tests.rs"]
mod tests;
