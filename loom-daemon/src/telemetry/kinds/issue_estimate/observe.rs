//! Daemon-side producer of `issue.estimate` (#12118): a curated issue the
//! daemon observes carries the fact, so no Curator has to run a CLI command.
//!
//! The producer rides the intake pass's walk of open issues
//! (`intake_reconcile::singleton`, `GhIntakeForge::list_open`). An open issue that carries
//! `loom:curated`, was updated within [`WINDOW`], has a trusted author and a
//! valid estimate marker is journaled through [`super::completion::record`],
//! which the ordinary backfill pass then exports.
//!
//! # Identity
//!
//! The completion id is a UUID derived from repo, issue and the marker values,
//! so observing the same issue again finds its journal file and emits nothing.
//! A re-curation that writes the same marker is the same fact; a changed
//! marker is a new one. The Curator's model and runtime are not observable, so
//! they are recorded as `unknown` (JSON null).
//!
//! The window keeps a first pass over a repo from backfilling historical
//! issues, which the issue leaves out of scope.

use std::path::Path;

use chrono::{DateTime, Duration, Utc};
use sha2::{Digest, Sha256};

use super::completion::{self, Completion};
use super::parse;
use crate::comment_trust::{Author, TrustPolicy};
use crate::forge_listing::RestIssue;
use crate::story_points::POINTS_LABEL_PREFIX;

/// How recently an issue must have changed to be treated as freshly curated.
pub const WINDOW: Duration = Duration::hours(48);

const CURATED: &str = concat!("loom", ":curated");

/// The completion id for one observed curation: stable across observations.
fn completion_id(repo: &str, issue: u32, marker: &super::Estimate) -> String {
    let key = format!(
        "{repo}#{issue}|lines={}|files={}|doctor_risk={}",
        marker.lines, marker.files, marker.doctor_risk
    );
    let digest = Sha256::digest(key.as_bytes());
    let mut bytes = [0u8; 16];
    bytes.copy_from_slice(&digest[..16]);
    uuid::Builder::from_bytes(bytes)
        .with_variant(uuid::Variant::RFC4122)
        .with_version(uuid::Version::Sha1)
        .into_uuid()
        .to_string()
}

fn points(row: &RestIssue) -> Option<u32> {
    row.labels
        .iter()
        .find_map(|l| l.strip_prefix(POINTS_LABEL_PREFIX)?.parse().ok())
}

/// Journal the `issue.estimate` fact of each freshly curated issue in `rows`
/// (one repo's open listing) that has none yet. Returns the number newly
/// journaled. A missing or malformed marker is skipped, never an error.
pub fn observe(
    root: &Path,
    repo: &str,
    rows: &[RestIssue],
    policy: &TrustPolicy,
    now: DateTime<Utc>,
) -> usize {
    let host = crate::sweep_registry::host_identity();
    let mut journaled = 0;
    for row in rows {
        if row.is_pull_request || !row.labels.iter().any(|l| l == CURATED) {
            continue;
        }
        let updated = row
            .updated_at
            .as_deref()
            .and_then(|t| DateTime::parse_from_rfc3339(t).ok())
            .map(|t| t.with_timezone(&Utc));
        if !updated.is_some_and(|t| now - t <= WINDOW) {
            continue;
        }
        let author = Author::new(row.author.as_deref(), row.author_association.as_deref());
        if !policy.trusts(&author) {
            continue;
        }
        let body = row.body.as_deref().unwrap_or("");
        let Ok(Some(estimate)) = parse(body) else {
            continue;
        };
        let id = completion_id(repo, row.number, &estimate);
        let request = Completion {
            repo,
            issue: row.number,
            model: "unknown",
            runtime: "unknown",
            id: &id,
            host_id: &host,
            at: updated.unwrap_or(now),
            points: points(row),
        };
        match completion::existing(root, &request) {
            Ok(Some(_)) => continue,
            Ok(None) => {}
            Err(e) => {
                log::debug!("issue.estimate: {repo}#{}: {e:#}", row.number);
                continue;
            }
        }
        match completion::record(root, body, &request) {
            Ok(Some(_)) => journaled += 1,
            Ok(None) => {}
            Err(e) => log::debug!("issue.estimate: {repo}#{}: {e:#}", row.number),
        }
    }
    journaled
}

#[cfg(test)]
#[path = "observe_tests.rs"]
mod tests;
