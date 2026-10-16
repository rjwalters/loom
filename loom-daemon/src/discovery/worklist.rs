//! The `GET /v1/mirror/worklist` answer and its conversion to
//! [`RepoSnapshot`]s.
//!
//! The shape (fleet mirror, #12304 "Design"):
//!
//! ```json
//! { "repos": { "owner/name": {
//!     "watermark": { "last_event_at": "…", "last_reconciled_at": "…",
//!                    "latest_seq": 42, "known": true, … },
//!     "items": [ {
//!       "number": 7, "type": "issue" | "pr", "state": "open",
//!       "labels": ["…"], "labels_at": { "label": "RFC-3339" },
//!       "assignees": ["login"], "author": "login", "author_association": "MEMBER",
//!       "created_at": "…", "updated_at": "…", "title": "…", "body": "…",
//!       "comments": 3, "sub_issues_total": 2, "sub_issues_completed": 1,
//!       "blocked_by_total": 1, "blocked_by_open": 1,
//!       // PRs only:
//!       "draft": false, "head_ref": "…", "head_sha": "…", "head_repo": "owner/name",
//!       "base_ref": "main", "author_is_bot": false,
//!       "checks": { "state": "pending" | "success" | "failure", "started_at": "…" }
//!     } ] } } }
//! ```
//!
//! Only the repos the caller's key may read appear. Every field but `number`
//! is optional; one that is absent reads as the value that passes the item
//! (for example no `head_repo` means the PR links by its body alone).

use std::collections::BTreeMap;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use serde::Deserialize;

use super::snapshot::{Checks, ItemExtras, RepoSnapshot, Source, WatermarkView};
use crate::forge_mirror::client::{MirrorItem, Watermark};
use crate::forge_pull_listing::RestPull;

#[derive(Debug, Default, Deserialize)]
struct Response {
    #[serde(default)]
    repos: BTreeMap<String, WireRepo>,
}

#[derive(Debug, Default, Deserialize)]
struct WireRepo {
    #[serde(default)]
    watermark: Watermark,
    #[serde(default)]
    items: Vec<WireItem>,
}

#[derive(Debug, Default, Deserialize)]
struct WireChecks {
    #[serde(default)]
    state: String,
    #[serde(default)]
    started_at: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct WireItem {
    #[serde(flatten)]
    item: MirrorItem,
    #[serde(default)]
    assignees: Vec<String>,
    #[serde(default)]
    blocked_by_total: Option<u64>,
    #[serde(default)]
    blocked_by_open: Option<u64>,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    head_ref: Option<String>,
    #[serde(default)]
    head_sha: Option<String>,
    #[serde(default)]
    head_repo: Option<String>,
    #[serde(default)]
    base_ref: Option<String>,
    #[serde(default)]
    author_is_bot: bool,
    #[serde(default)]
    checks: Option<WireChecks>,
}

/// A decoded worklist: every readable repo, lowercase slug → snapshot.
#[derive(Debug, Default)]
pub struct Worklist {
    pub repos: BTreeMap<String, Arc<RepoSnapshot>>,
}

fn time(raw: Option<&str>) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(raw?)
        .ok()
        .map(|t| t.with_timezone(&Utc))
}

fn count(n: Option<u64>) -> Option<u32> {
    n.and_then(|n| u32::try_from(n).ok())
}

impl WireRepo {
    fn into_snapshot(self, slug: &str) -> RepoSnapshot {
        let watermark = WatermarkView {
            last_event_at: time(self.watermark.last_event_at.as_deref()),
            last_reconciled_at: time(self.watermark.last_reconciled_at.as_deref()),
            latest_seq: self.watermark.latest_seq,
        };
        let mut rows = Vec::new();
        let mut pulls = Vec::new();
        let mut extras = BTreeMap::new();
        for wire in self.items {
            let open = wire
                .item
                .state
                .as_deref()
                .is_some_and(|s| s.eq_ignore_ascii_case("open"));
            let Some(row) = wire.item.to_rest().filter(|_| open) else {
                continue;
            };
            let is_pr = row.is_pull_request;
            if is_pr {
                pulls.push(RestPull {
                    number: row.number,
                    state: row.state.clone(),
                    draft: wire.draft,
                    title: row.title.clone(),
                    created_at: row.created_at.clone(),
                    updated_at: row.updated_at.clone(),
                    author: row.author.clone(),
                    labels: row.labels.clone(),
                    head_ref: wire.head_ref.clone(),
                    head_sha: wire.head_sha.clone(),
                    base_ref: wire.base_ref.clone(),
                    body: row.body.clone(),
                    author_association: row.author_association.clone(),
                    author_is_bot: wire.author_is_bot,
                    head_repo: wire.head_repo.clone(),
                });
            }
            extras.insert(
                row.number,
                ItemExtras {
                    assignees: wire.assignees,
                    blocked_by_total: count(wire.blocked_by_total),
                    blocked_by_open: count(wire.blocked_by_open),
                    checks: wire.checks.filter(|_| is_pr).map(|c| Checks {
                        state: c.state,
                        started_at: c.started_at,
                    }),
                },
            );
            rows.push(row);
        }
        RepoSnapshot {
            slug: slug.to_string(),
            source: Source::Worklist,
            rows,
            pulls: Some(pulls),
            extras,
            watermark: Some(watermark),
        }
    }
}

/// Decode a worklist body.
///
/// # Errors
///
/// [`crate::forge_mirror::client::MirrorError::Malformed`].
pub fn decode(
    body: serde_json::Value,
) -> Result<Worklist, crate::forge_mirror::client::MirrorError> {
    let response: Response = crate::forge_mirror::client::decode(body)?;
    Ok(Worklist {
        repos: response
            .repos
            .into_iter()
            .map(|(slug, repo)| {
                let slug = slug.to_ascii_lowercase();
                let snap = Arc::new(repo.into_snapshot(&slug));
                (slug, snap)
            })
            .collect(),
    })
}
