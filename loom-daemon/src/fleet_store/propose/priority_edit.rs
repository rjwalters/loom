//! Format-preserving edit of `fleet.yml` for `propose priority`.
//!
//! Finds the `repos[]` record whose `name:` is the requested repo and
//! sets/inserts its `fleet_priority:` line only. Every other line in the
//! record (including its `fleet`/`firewall` flags, which this command must
//! never touch) and every other record are left byte-for-byte alone.

use anyhow::Result;
use serde_json::Value;

use super::yaml_edit::{Doc, Seg};

/// Edit `text` (the current `fleet.yml`) to set `repo`'s `fleet_priority`.
pub fn edit(text: &str, repo: &str, priority: u32) -> Result<String> {
    let mut doc = Doc::new(text);
    let path = [
        Seg::Key("repos"),
        Seg::Item {
            field: "name",
            value: repo,
        },
        Seg::Key("fleet_priority"),
    ];
    doc.set(&path, &Value::from(priority))?;
    Ok(doc.finish())
}

#[cfg(test)]
#[path = "tests/priority_edit_tests.rs"]
mod tests;
