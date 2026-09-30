//! `fleet/state.yml` → this host's desired run state.
//!
//! ```yaml
//! fleet:                    # the default for every host
//!   state: running          # running | paused | stopped
//!   since: 2026-01-01T00:00Z
//!   by: operator
//!   reason: free text
//! hosts:
//!   build-3:
//!     state: paused         # a host entry overrides the fleet default
//! ```
//!
//! `running` dispatches normally, `paused` keeps the daemon up without new
//! dispatch, `stopped` means the daemon is meant to be down. This module only
//! reads and reports it; nothing enforces it yet.

use anyhow::{anyhow, bail, Result};
use serde::Serialize;
use serde_json::Value;

/// A desired run state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum RunState {
    /// Dispatching normally.
    Running,
    /// Up, no new dispatch.
    Paused,
    /// Down, and meant to stay down.
    Stopped,
}

impl RunState {
    /// Parse `running`, `paused` or `stopped`; `None` for anything else.
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "running" => Some(Self::Running),
            "paused" => Some(Self::Paused),
            "stopped" => Some(Self::Stopped),
            _ => None,
        }
    }

    /// Lowercase name.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Paused => "paused",
            Self::Stopped => "stopped",
        }
    }
}

/// A host's resolved desired state and where it came from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct HostState {
    /// The host.
    pub host: String,
    /// The desired state.
    pub state: RunState,
    /// `host` (its own entry) or `fleet` (the default).
    pub source: &'static str,
    /// `since`, from the entry that set the state.
    pub since: Option<String>,
    /// `by`, from the entry that set the state.
    pub by: Option<String>,
    /// `reason`, from the entry that set the state.
    pub reason: Option<String>,
}

/// Resolve `host`'s desired state from the text of `fleet/state.yml`.
pub fn resolve(text: &str, host: &str) -> Result<HostState> {
    let doc = super::yaml::parse(text).map_err(|e| anyhow!("fleet/state.yml: {e:#}"))?;
    let top = doc
        .as_object()
        .ok_or_else(|| anyhow!("fleet/state.yml: top level must be a mapping"))?;
    let host_entry = match top.get("hosts") {
        None | Some(Value::Null) => None,
        Some(Value::Object(hosts)) => hosts.get(host),
        Some(_) => bail!("fleet/state.yml: `hosts` must be a mapping"),
    };
    let (entry, source) = match host_entry.filter(|e| e.get("state").is_some()) {
        Some(e) => (e, "host"),
        None => match top.get("fleet") {
            Some(f) if f.get("state").is_some() => (f, "fleet"),
            _ => bail!("fleet/state.yml sets no state for `{host}` and no `fleet.state` default"),
        },
    };
    let raw = entry
        .get("state")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let state = RunState::parse(raw).ok_or_else(|| {
        anyhow!("fleet/state.yml: `{raw}` is not running, paused or stopped ({source} entry)")
    })?;
    let text_field = |k: &str| {
        entry
            .get(k)
            .and_then(Value::as_str)
            .map(|s| s.trim().to_string())
    };
    Ok(HostState {
        host: host.to_string(),
        state,
        source,
        since: text_field("since"),
        by: text_field("by"),
        reason: text_field("reason"),
    })
}

#[cfg(test)]
#[path = "tests/state_tests.rs"]
mod tests;
