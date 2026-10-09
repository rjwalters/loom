//! Read-side and validation helpers for an artifact published to a dedicated
//! branch of the fleet store (`fleet.repo`): the ETA fit (#10395) and the
//! captain gauges heartbeat (W12).
//!
//! They lived in `eta::fit::publish` and were borrowed from there by
//! `observability::captain_gauges::store`. They moved here (#11098, Stage 2)
//! so the captain gauges no longer depend on the ETA subsystem; behaviour is
//! unchanged. Nothing in this module writes to the forge: each artifact's
//! own publish path keeps its write calls, so
//! `write_scope::tests::daemon_write_paths_are_scoped` still reviews them
//! where they are.

use anyhow::{bail, Context, Result};
use serde_json::Value;

use super::fetch::{Reply, Transport};
use super::StoreLocation;

/// The contents-API path of `path` on `loc`'s publication branch.
pub(crate) fn contents_path(loc: &StoreLocation, path: &str) -> String {
    format!("repos/{}/contents/{path}?ref={}", loc.repo, loc.reference)
}

/// The raw-content media type for a contents read.
pub(crate) const RAW: &str = "application/vnd.github.raw+json";

/// `Ok` for a 2xx `reply`, else an error naming `what` and `repo`.
///
/// # Errors
///
/// When `reply` is not a 2xx.
pub(crate) fn ensure_ok(reply: &Reply, what: &str, repo: &str) -> Result<()> {
    if (200..300).contains(&reply.status) {
        return Ok(());
    }
    let hint = if reply.status == 403 || reply.status == 404 || reply.status == 422 {
        " — check the writer App has contents:write on the store and the eta-fit branch is \
         exempt from the main ruleset"
    } else {
        ""
    };
    bail!("{what} in {repo}: HTTP {}{hint}", reply.status)
}

/// The blob sha of `path` on the branch, or `None` when absent.
///
/// # Errors
///
/// A transport failure, a malformed reply, or a status other than 200/404.
pub(crate) fn blob_sha(
    t: &dyn Transport,
    loc: &StoreLocation,
    path: &str,
) -> Result<Option<String>> {
    let r = t.get(&contents_path(loc, path), None, None)?;
    match r.status {
        200 => {
            let v: Value = serde_json::from_str(&r.body).context("malformed contents response")?;
            Ok(v.get("sha").and_then(Value::as_str).map(str::to_string))
        }
        404 => Ok(None),
        s => bail!("HTTP {s} reading {path} in {}", loc.repo),
    }
}

/// The publication branch configured under `key` is spliced into store
/// request paths, a query string and the PUT `branch` field, so it must pass
/// the same check as `fleet.ref` ([`super::validate_ref`]) and be a bare
/// branch name: no `refs/` or `heads/` prefix and no empty or dot-led segment
/// (so no trailing `/`). A non-canonical spelling would make the written
/// branch ambiguous.
///
/// # Errors
///
/// When `reference` is not a plain, canonical branch name.
pub(crate) fn validate_branch_for(key: &str, reference: &str) -> Result<()> {
    super::validate_ref(reference).with_context(|| format!("`{key}` `{reference}` is invalid"))?;
    if reference.starts_with("refs/")
        || reference.starts_with("heads/")
        || reference
            .split('/')
            .any(|seg| seg.is_empty() || seg.starts_with('.'))
    {
        bail!(
            "`{key}` `{reference}` must be a bare branch name \
             (no `refs/` or `heads/` prefix, no empty or dot-led segment)"
        );
    }
    Ok(())
}

/// The branch `reference` names, for comparison only: trimmed, without a
/// leading `refs/heads/` or `heads/`, without trailing `/`.
fn branch_name(reference: &str) -> &str {
    let r = reference.trim();
    r.strip_prefix("refs/heads/")
        .or_else(|| r.strip_prefix("heads/"))
        .unwrap_or(r)
        .trim_end_matches('/')
}

/// An autonomous write of artifact `what`, whose branch is configured under
/// `key`, may land only on its dedicated publication branch, never on the
/// store's reviewed branch (`fleet.ref`) or `main`. The branch must be valid
/// and canonical ([`validate_branch_for`]); both sides are normalized and
/// compared case-insensitively, so `refs/heads/main`, `Main` or
/// `fleet.ref = refs/heads/stable` against `stable` are refused.
///
/// # Errors
///
/// When `loc.reference` is invalid or names the reviewed branch or `main`.
pub(crate) fn refuse_reviewed_branch_for(
    key: &str,
    what: &str,
    loc: &StoreLocation,
    base_ref: &str,
) -> Result<()> {
    validate_branch_for(key, &loc.reference)?;
    let r = branch_name(&loc.reference);
    if r.eq_ignore_ascii_case(branch_name(base_ref)) || r.eq_ignore_ascii_case("main") {
        bail!(
            "refusing to publish {what} to `{r}` in {}: `{key}` must name a dedicated \
             branch, not the store's reviewed branch",
            loc.repo
        );
    }
    Ok(())
}
