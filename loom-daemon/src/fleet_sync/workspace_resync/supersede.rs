//! Close the resync pull requests a newer resync supersedes (#13488).
//!
//! A host that rolls before its last resync PR landed opens the next one for
//! the new version and leaves the old one open. Champion's after-merge step
//! only closes older ones once a resync merges, so when the newest is itself
//! held, nothing does. This runs under the claim, once the pass has settled at
//! the running version, and closes every older daemon resync PR in the repo.
//!
//! Best effort throughout: a failed list, comment, close or branch delete is
//! logged and the pass outcome is unchanged; the next pass retries.

use serde_json::Value;

use super::git::{self, TRAILER_VERSION};
use crate::fleet_store::resync_claim::Claimant;
use crate::install_compat::Version;

const TITLE_PREFIX: &str = "chore(loom): resync installed Loom to v";

/// The version a daemon resync PR proposes, or `None` when `pr` is not one.
///
/// A daemon resync PR has the title [`git::resync_message`] emits, the head
/// branch [`git::resync_branch`] names for that version, and the version
/// trailer in its body. The trailer stands in for the author: the daemon's
/// login is not known here, and nobody else writes it.
fn resync_version(pr: &Value) -> Option<Version> {
    let title = pr.get("title")?.as_str()?;
    let text = title.strip_prefix(TITLE_PREFIX)?;
    let version = Version::parse(text).filter(|v| v.to_string() == text)?;
    let head = pr.pointer("/head/ref")?.as_str()?;
    if head != git::resync_branch(&version.to_string()) {
        return None;
    }
    let body = pr.get("body")?.as_str()?;
    let trailer = format!("{TRAILER_VERSION}: {version}");
    body.lines().any(|l| l.trim() == trailer).then_some(version)
}

/// Close the open daemon resync PRs in the repo that `running` has passed.
///
/// `keep` is the PR for `running` this pass opened or found: a PR for
/// `running` itself is closed only when there is none (the checkout is
/// already current, so it has nothing left to add). A PR above `running` is
/// never touched.
pub(super) fn close_superseded(claimant: &Claimant<'_>, running: Version, keep: Option<u64>) {
    let nwo = claimant.repo;
    let listed =
        match claimant
            .read
            .get(&format!("repos/{nwo}/pulls?state=open&per_page=100"), None, None)
        {
            Ok(reply) if reply.status == 200 => reply,
            Ok(reply) => {
                log::warn!(
                    "workspace_resync: {nwo}: listing resync PRs to supersede: HTTP {}",
                    reply.status
                );
                return;
            }
            Err(e) => {
                log::warn!("workspace_resync: {nwo}: listing resync PRs to supersede: {e:#}");
                return;
            }
        };
    let pulls: Value = serde_json::from_str(&listed.body).unwrap_or_default();
    for pr in pulls.as_array().into_iter().flatten() {
        let Some(number) = pr.get("number").and_then(Value::as_u64) else {
            continue;
        };
        let Some(version) = resync_version(pr) else {
            continue;
        };
        let older = version < running || (version == running && keep.is_none());
        if !older || Some(number) == keep {
            continue;
        }
        let reason = match keep {
            Some(n) => format!(
                "Superseded by #{n} (v{running}); this host now runs v{running}. Closing the \
                 resync to v{version}."
            ),
            None => format!(
                "Superseded: the installed Loom here is already v{running}. Closing the resync \
                 to v{version}."
            ),
        };
        close_one(claimant, number, version, &reason);
    }
}

fn close_one(claimant: &Claimant<'_>, number: u64, version: Version, reason: &str) {
    let nwo = claimant.repo;
    let ok = |r: anyhow::Result<crate::fleet_store::fetch::Reply>| {
        r.is_ok_and(|r| (200..300).contains(&r.status))
    };
    let commented = claimant.write.write(
        "POST",
        &format!("repos/{nwo}/issues/{number}/comments"),
        &serde_json::json!({ "body": reason }),
    );
    if !ok(commented) {
        log::warn!("workspace_resync: {nwo}: could not comment on superseded resync PR #{number}");
    }
    let closed = claimant.write.write(
        "PATCH",
        &format!("repos/{nwo}/pulls/{number}"),
        &serde_json::json!({ "state": "closed" }),
    );
    if !ok(closed) {
        // Leave the branch: deleting it under an open PR would close it
        // anyway, but a failed close should look the same next pass.
        log::warn!("workspace_resync: {nwo}: could not close superseded resync PR #{number}");
        return;
    }
    log::info!("workspace_resync: {nwo}: closed superseded resync PR #{number} (v{version})");
    let branch = git::resync_branch(&version.to_string());
    let deleted = claimant.write.write(
        "DELETE",
        &format!("repos/{nwo}/git/refs/heads/{branch}"),
        &Value::Null,
    );
    if !ok(deleted) {
        log::warn!("workspace_resync: {nwo}: could not delete branch {branch}");
    }
}
