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
//! When the store has the compiled `fleet.json` (#10705), the same document is
//! its `state` section ([`resolve_snapshot`]); `fleet/state.yml` is read only
//! when `fleet.json` is absent.
//!
//! `running` dispatches normally, `paused` keeps the daemon up without new
//! dispatch, `stopped` means the daemon is meant to be down. This module reads
//! and reports it; [`crate::fleet_state`] is what *enforces* it on a live host
//! (#9598).

use anyhow::{anyhow, bail, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::fetch::Snapshot;

/// A desired run state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
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
///
/// `Serialize`-only by design (`source` is a `&'static str`): the persisted
/// forms live in [`crate::fleet_state::StatePass`], which owns its strings.
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
    let doc = super::yaml::parse(text).map_err(|e| anyhow!("{}: {e:#}", super::STATE_PATH))?;
    resolve_value(&doc, host, super::STATE_PATH)
}

/// Resolve `host`'s desired state from a store snapshot: `fleet.json`'s
/// `state` when the store has it, else `fleet/state.yml`. `Ok(None)` when it
/// has neither. A present but invalid `fleet.json` is an error and never falls
/// back to `fleet/state.yml`.
pub fn resolve_snapshot(snapshot: &Snapshot, host: &str) -> Result<Option<HostState>> {
    if let Some(doc) = super::compiled::from_snapshot(snapshot)? {
        let source = format!("{} `state`", super::FLEET_JSON_PATH);
        return resolve_value(doc.state(), host, &source).map(Some);
    }
    let Some(text) = snapshot.text(super::STATE_PATH)? else {
        return Ok(None);
    };
    resolve(&text, host).map(Some)
}

/// The message for a snapshot with no run state at all
/// ([`resolve_snapshot`] returned `Ok(None)`).
#[must_use]
pub fn missing_message(snapshot: &Snapshot) -> String {
    format!(
        "the store has neither {} nor {} (commit {})",
        super::FLEET_JSON_PATH,
        super::STATE_PATH,
        snapshot.short_commit()
    )
}

/// Resolve `host`'s desired state from a parsed run-state document.
/// `source_name` names the document in messages.
fn resolve_value(doc: &Value, host: &str, source_name: &str) -> Result<HostState> {
    let top = doc
        .as_object()
        .ok_or_else(|| anyhow!("{source_name}: top level must be a mapping"))?;
    let host_entry = match top.get("hosts") {
        None | Some(Value::Null) => None,
        Some(Value::Object(hosts)) => hosts.get(host),
        Some(_) => bail!("{source_name}: `hosts` must be a mapping"),
    };
    let (entry, source) = match host_entry.filter(|e| e.get("state").is_some()) {
        Some(e) => (e, "host"),
        None => match top.get("fleet") {
            Some(f) if f.get("state").is_some() => (f, "fleet"),
            _ => bail!("{source_name} sets no state for `{host}` and no `fleet.state` default"),
        },
    };
    let raw = entry
        .get("state")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let state = RunState::parse(raw).ok_or_else(|| {
        anyhow!("{source_name}: `{raw}` is not running, paused or stopped ({source} entry)")
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
