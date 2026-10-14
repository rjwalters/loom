//! Format-preserving edit of `fleet.yml`'s `state:` section for `propose
//! state`.
//!
//! `host = None` edits the fleet default, `state.fleet`, which must already
//! exist (the store's own read path, [`super::super::state::resolve`],
//! requires it as the fallback for every host, so a store missing it is
//! already broken, and this command will not paper over that by inventing
//! one). `host = Some(h)` edits `state.hosts.<h>`, creating `hosts:` and/or
//! the host's own entry when either is missing.
//!
//! `state`/`since`/`by` are always (re)written; `reason` only when the
//! caller passes non-empty text, leaving any pre-existing `reason:` alone
//! otherwise: a routine state flip need not explain away a previous one.

use anyhow::{bail, Result};
use serde_json::Value;

use super::yaml_edit::{Doc, Seg};
use crate::fleet_store::state::RunState;

/// Edit `text` (the current `fleet.yml`) to set `host`'s (or the fleet
/// default's) desired state.
pub fn edit(
    text: &str,
    host: Option<&str>,
    state: RunState,
    reason: &str,
    by: &str,
    since: &str,
) -> Result<String> {
    let mut doc = Doc::new(text);
    let base: Vec<Seg<'_>> = match host {
        None => vec![Seg::Key("state"), Seg::Key("fleet")],
        Some(h) => vec![Seg::Key("state"), Seg::Key("hosts"), Seg::Key(h)],
    };
    if host.is_none() && !doc.has(&base)? {
        bail!(
            "fleet.yml has no `state.fleet` default block — add one by hand first, then \
             `propose state` can edit it"
        );
    }
    let mut fields = vec![("state", state.as_str()), ("since", since), ("by", by)];
    if !reason.trim().is_empty() {
        fields.push(("reason", reason));
    }
    for (key, value) in fields {
        let mut path = base.clone();
        path.push(Seg::Key(key));
        doc.set(&path, &Value::from(value))?;
    }
    Ok(doc.finish())
}

#[cfg(test)]
#[path = "tests/state_edit_tests.rs"]
mod tests;
