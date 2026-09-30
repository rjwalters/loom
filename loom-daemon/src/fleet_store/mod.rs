//! Fleet state store reader (`loom-daemon fleet-config`).
//!
//! An operator may keep a whole fleet's desired state in one private forge
//! repository — the **fleet store**: the repo roster and priorities
//! (`repos.yml`), fleet-wide and per-host daemon config (`fleet/defaults.json`,
//! `fleet/hosts/<host>/{defaults,local}.json`) and per-host run state
//! (`fleet/state.yml`). Daemons read it straight from the forge; the forge is
//! the state store, and a change to the fleet lands as a reviewed commit.
//!
//! Nothing here names a particular store. Its location is config key
//! `fleet.repo` (`OWNER/REPO`, env `LOOM_FLEET_REPO`) and ref `fleet.ref`
//! (env `LOOM_FLEET_REF`, default `main`). With neither set the feature is off
//! and nothing in the daemon changes. This module only *reads* the store and
//! reports or applies what it says, on explicit operator command; nothing runs
//! it automatically yet.
//!
//! - [`fetch`] — conditional fetch into a local cache, through the daemon's
//!   own `gh` forge path and GitHub App credentials ([`gh`]).
//! - [`render`] — the host's machine tier and host-local tier, and drift.
//! - [`roster`] — `repos.yml` → desired workspace set, diffed against the
//!   daemon's workspace registry. Fails closed.
//! - [`state`] — this host's desired run state from `fleet/state.yml`.
//! - [`propose`] — the one *write* path: open a PR against the store instead
//!   of hand-editing it (#9599).
//!
//! The file contract is documented in `defaults/docs/daemon-reference.md`
//! §"Fleet store".

pub mod fetch;
pub mod gh;
pub mod propose;
pub mod render;
pub mod roster;
pub mod state;
pub mod yaml;

use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Result};
use serde_json::Value;

/// Env override for the store location (`OWNER/REPO`).
pub const FLEET_REPO_ENV: &str = "LOOM_FLEET_REPO";
/// Env override for the store ref.
pub const FLEET_REF_ENV: &str = "LOOM_FLEET_REF";
/// Config key for the store location.
pub const FLEET_REPO_KEY: &str = "fleet.repo";
/// Config key for the store ref.
pub const FLEET_REF_KEY: &str = "fleet.ref";
/// Ref read when none is configured.
pub const DEFAULT_REF: &str = "main";

/// Store-relative path of the roster.
pub const ROSTER_PATH: &str = "repos.yml";
/// Store-relative path of the run-state file.
pub const STATE_PATH: &str = "fleet/state.yml";
/// Store-relative path of the fleet-wide machine-tier config.
pub const FLEET_DEFAULTS_PATH: &str = "fleet/defaults.json";

/// Store-relative path of `host`'s machine-tier overlay.
#[must_use]
pub fn host_defaults_path(host: &str) -> String {
    format!("fleet/hosts/{host}/defaults.json")
}

/// Store-relative path of `host`'s host-local tier.
#[must_use]
pub fn host_local_path(host: &str) -> String {
    format!("fleet/hosts/{host}/local.json")
}

/// Whether a store path is one this reader fetches. Everything else in the
/// store (docs, `hosts.yml`, …) is ignored.
#[must_use]
pub fn is_contract_path(path: &str) -> bool {
    if matches!(path, ROSTER_PATH | STATE_PATH | FLEET_DEFAULTS_PATH) {
        return true;
    }
    let Some(rest) = path.strip_prefix("fleet/hosts/") else {
        return false;
    };
    matches!(
        rest.split_once('/'),
        Some((host, "defaults.json" | "local.json")) if !host.is_empty()
    )
}

/// Where the store is: `OWNER/REPO` and the ref to read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoreLocation {
    /// `OWNER/REPO`.
    pub repo: String,
    /// Branch, tag or commit.
    pub reference: String,
}

impl StoreLocation {
    /// The `OWNER` segment.
    #[must_use]
    pub fn owner(&self) -> &str {
        self.repo.split('/').next().unwrap_or_default()
    }
}

/// Resolve the store location from `env` (a lookup, so tests need not touch
/// the process environment) over the effective config. `Ok(None)` means the
/// feature is off.
pub fn resolve_location(
    effective_config: &Value,
    env: &dyn Fn(&str) -> Option<String>,
) -> Result<Option<StoreLocation>> {
    let pick = |env_key: &str, cfg_key: &str| -> Option<String> {
        env(env_key)
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .or_else(|| {
                crate::config_resolver::get_path(effective_config, cfg_key)
                    .and_then(Value::as_str)
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
            })
    };
    let Some(repo) = pick(FLEET_REPO_ENV, FLEET_REPO_KEY) else {
        return Ok(None);
    };
    validate_repo(&repo)?;
    let reference = pick(FLEET_REF_ENV, FLEET_REF_KEY).unwrap_or_else(|| DEFAULT_REF.to_string());
    validate_ref(&reference)?;
    Ok(Some(StoreLocation { repo, reference }))
}

/// Resolve the location for the workspace at `root` from the real
/// environment and its effective config, or explain how to turn it on.
pub fn require_location(root: &Path) -> Result<StoreLocation> {
    let effective = crate::config_resolver::resolve_effective_config(root);
    resolve_location(&effective, &|k| std::env::var(k).ok())?.ok_or_else(|| {
        anyhow!(
            "no fleet store configured for {} — set config key `{FLEET_REPO_KEY}` (OWNER/REPO) or \
             env {FLEET_REPO_ENV}",
            root.display()
        )
    })
}

fn is_name_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.')
}

fn validate_repo(repo: &str) -> Result<()> {
    let ok = repo.split_once('/').is_some_and(|(o, r)| {
        !o.is_empty()
            && !r.is_empty()
            && o.chars().all(is_name_char)
            && r.chars().all(is_name_char)
            && r != "."
            && r != ".."
    });
    if !ok {
        bail!("fleet store `{repo}` is not OWNER/REPO");
    }
    Ok(())
}

fn validate_ref(reference: &str) -> Result<()> {
    let ok = !reference.is_empty()
        && !reference.contains("..")
        && !reference.starts_with('/')
        && reference.chars().all(|c| is_name_char(c) || c == '/');
    if !ok {
        bail!("fleet store ref `{reference}` is not a plain branch, tag or commit name");
    }
    Ok(())
}

/// Whether `host` is safe to splice into a store path.
pub fn validate_host(host: &str) -> Result<()> {
    if host.is_empty() || host.starts_with('.') || !host.chars().all(is_name_char) {
        bail!("host id `{host}` is not a plain host name");
    }
    Ok(())
}

/// Default cache directory for `location`: `~/.loom/fleet-store/<owner>/<repo>`,
/// beside the machine-level workspace registry (`~/.loom/workspaces.json`).
pub fn default_cache_dir(location: &StoreLocation) -> Result<PathBuf> {
    let home = dirs::home_dir().ok_or_else(|| anyhow!("no home directory"))?;
    let (owner, repo) = location
        .repo
        .split_once('/')
        .unwrap_or((&location.repo, ""));
    Ok(home
        .join(".loom")
        .join("fleet-store")
        .join(owner)
        .join(repo))
}

#[cfg(test)]
#[path = "tests/support.rs"]
pub(crate) mod test_support;

#[cfg(test)]
#[path = "tests/mod_tests.rs"]
mod tests;
