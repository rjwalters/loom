//! Format-preserving edit of `fleet/state.yml` for `propose state`.
//!
//! `host = None` edits the top-level `fleet:` default block, which must
//! already exist (the store's own read path, [`super::super::state::resolve`],
//! requires it as the fallback for every host, so a store missing it is
//! already broken — this command will not paper over that by inventing one).
//! `host = Some(h)` edits `hosts.<h>:`, creating the `hosts:` mapping and/or
//! the host's own entry when either is missing.
//!
//! `state`/`since`/`by` are always (re)written; `reason` only when the
//! caller passes non-empty text, leaving any pre-existing `reason:` alone
//! otherwise — a routine state flip need not explain away a previous one.

use anyhow::{anyhow, Result};

use super::block;
use crate::fleet_store::state::RunState;

/// Edit `text` (the current `fleet/state.yml`) to set `host`'s (or the fleet
/// default's) desired state.
pub fn edit(
    text: &str,
    host: Option<&str>,
    state: RunState,
    reason: &str,
    by: &str,
    since: &str,
) -> Result<String> {
    let mut lines: Vec<String> = text.lines().map(str::to_string).collect();
    let trailing_newline = text.is_empty() || text.ends_with('\n');

    let mut entry = match host {
        None => block::find_block(&lines, 0, lines.len(), 0, "fleet").ok_or_else(|| {
            anyhow!(
                "fleet/state.yml has no top-level `fleet:` default block — add one by hand first, \
                 then `propose state` can edit it"
            )
        })?,
        Some(h) => {
            let mut hosts = block::find_or_append_top_block(&mut lines, 0, "hosts");
            match block::find_block(&lines, hosts.start, hosts.end, hosts.child_indent, h) {
                Some(b) => b,
                None => block::append_mapping(&mut lines, &mut hosts, h),
            }
        }
    };

    block::set_scalar(&mut lines, &mut entry, "state", state.as_str());
    block::set_scalar_quoted(&mut lines, &mut entry, "since", since);
    block::set_scalar_quoted(&mut lines, &mut entry, "by", by);
    if !reason.trim().is_empty() {
        block::set_scalar_quoted(&mut lines, &mut entry, "reason", reason);
    }

    let mut out = lines.join("\n");
    if trailing_newline && !out.is_empty() {
        out.push('\n');
    }
    Ok(out)
}

#[cfg(test)]
#[path = "tests/state_edit_tests.rs"]
mod tests;
