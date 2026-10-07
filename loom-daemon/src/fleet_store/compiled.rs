//! `fleet.json` — the compiled fleet document (#10705).
//!
//! The store's one source file is `fleet.yml`; fleet-gitops compiles and
//! validates it (`scripts/render.py`, schema `schema/fleet.schema.json`) into
//! one canonical JSON document, `fleet.json`. When the store has it, the
//! roster, the run state, the machine tier and the host-local tier are all
//! taken from that one document, parsed with `serde_json`:
//!
//! | Section | Was | Read by |
//! |---|---|---|
//! | top-level `root`, `repos` | `repos.yml` | [`super::roster`] |
//! | `state` | `fleet/state.yml` | [`super::state`] |
//! | `config.defaults` | `fleet/defaults.json` | [`super::render`] |
//! | `config.hosts.<host>.defaults` | `fleet/hosts/<host>/defaults.json` | [`super::render`] |
//! | `config.hosts.<host>.local` | `fleet/hosts/<host>/local.json` | [`super::render`] |
//!
//! # Transition rules
//!
//! - **Absent** (`Ok(None)` from [`from_snapshot`]): the store predates the
//!   compiled document; callers fall back to the legacy files and the YAML
//!   reader. That fallback is dropped (and `yaml.rs` deleted) in a follow-up.
//! - **Present but invalid** — not JSON, not an object, no `_generated`
//!   header, an unknown `schema_version`, or a consumed section of the wrong
//!   shape — is an error, and **never** falls back to the legacy files: the
//!   store's own source of truth is broken, and reading the legacy files then
//!   would act on whatever they happened to say.

use anyhow::{anyhow, bail, Context, Result};
use serde_json::{Map, Value};

use super::fetch::Snapshot;
use super::FLEET_JSON_PATH;

/// The `_generated.schema_version` this reader understands. fleet-gitops bumps
/// it when the document's shape changes incompatibly for a reader, so any
/// other value is refused rather than guessed at.
pub const SCHEMA_VERSION: u64 = 1;

/// A parsed, shape-checked `fleet.json`.
#[derive(Debug, Clone, PartialEq)]
pub struct Compiled {
    doc: Map<String, Value>,
}

/// The store's `fleet.json`, `Ok(None)` when the snapshot does not have one,
/// `Err` when it has one that is not a valid compiled document.
pub fn from_snapshot(snapshot: &Snapshot) -> Result<Option<Compiled>> {
    let Some(text) = snapshot.text(FLEET_JSON_PATH)? else {
        return Ok(None);
    };
    parse(&text)
        .with_context(|| format!("{FLEET_JSON_PATH} (commit {})", snapshot.short_commit()))
        .map(Some)
}

/// Parse and shape-check the text of a `fleet.json`.
pub fn parse(text: &str) -> Result<Compiled> {
    let value: Value = serde_json::from_str(text).context("not valid JSON")?;
    let Value::Object(doc) = value else {
        bail!("top level must be a JSON object");
    };
    let generated = doc
        .get("_generated")
        .and_then(Value::as_object)
        .ok_or_else(|| anyhow!("no `_generated` header — not a compiled fleet document"))?;
    let version = generated
        .get("schema_version")
        .ok_or_else(|| anyhow!("`_generated.schema_version` is missing"))?;
    match version.as_u64() {
        Some(SCHEMA_VERSION) => {}
        _ => bail!(
            "`_generated.schema_version` is {version}; this loom-daemon reads only version \
             {SCHEMA_VERSION} — upgrade loom-daemon or fix the store"
        ),
    }
    if !doc.get("root").is_some_and(Value::is_string) {
        bail!("`root` must be a string");
    }
    if !doc.get("repos").is_some_and(Value::is_array) {
        bail!("`repos` must be a list");
    }
    if !doc.get("state").is_some_and(Value::is_object) {
        bail!("`state` must be an object");
    }
    let config = doc
        .get("config")
        .and_then(Value::as_object)
        .ok_or_else(|| anyhow!("`config` must be an object"))?;
    if !config.get("defaults").is_some_and(Value::is_object) {
        bail!("`config.defaults` must be an object");
    }
    match config.get("hosts") {
        None => {}
        Some(Value::Object(hosts)) => {
            for (host, tiers) in hosts {
                let tiers = tiers
                    .as_object()
                    .ok_or_else(|| anyhow!("`config.hosts.{host}` must be an object"))?;
                for tier in ["defaults", "local"] {
                    if tiers.get(tier).is_some_and(|t| !t.is_object()) {
                        bail!("`config.hosts.{host}.{tier}` must be an object");
                    }
                }
            }
        }
        Some(_) => bail!("`config.hosts` must be an object"),
    }
    Ok(Compiled { doc })
}

impl Compiled {
    /// The whole document: the roster reads its top-level `root` and `repos`,
    /// exactly the keys `repos.yml` had.
    #[must_use]
    pub fn roster(&self) -> &Map<String, Value> {
        &self.doc
    }

    /// `state`: the run-state document `fleet/state.yml` was.
    #[must_use]
    pub fn state(&self) -> &Value {
        // Checked present and an object by `parse`.
        &self.doc["state"]
    }

    /// `config.defaults`: the machine tier every host shares.
    #[must_use]
    pub fn fleet_defaults(&self) -> &Value {
        &self.doc["config"]["defaults"]
    }

    /// `config.hosts.<host>.defaults`: `host`'s machine-tier overlay.
    #[must_use]
    pub fn host_defaults(&self, host: &str) -> Option<&Value> {
        self.host_tier(host, "defaults")
    }

    /// `config.hosts.<host>.local`: `host`'s host-local tier.
    #[must_use]
    pub fn host_local(&self, host: &str) -> Option<&Value> {
        self.host_tier(host, "local")
    }

    fn host_tier(&self, host: &str, tier: &str) -> Option<&Value> {
        self.doc.get("config")?.get("hosts")?.get(host)?.get(tier)
    }
}

#[cfg(test)]
#[path = "tests/compiled_tests.rs"]
mod tests;
