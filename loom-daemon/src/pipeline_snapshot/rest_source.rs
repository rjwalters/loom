//! REST + ETag backed pipeline fetch for [`GhPipelineSource`] (Issue #9253).
//!
//! The pre-#9253 fetch ran up to nine GraphQL `gh issue/pr list --limit 500`
//! queries per root on every cache miss — ~500 GraphQL requests per miss on a
//! ~58-root fleet, with no conditional-request escape hatch. This module
//! serves every *label* count from
//! [`forge_listing::list_issues_cached_persistent`] instead: one conditional
//! REST listing per label, split client-side by `is_pull_request`, which is a
//! **free `304`** when the label's row set is unchanged. The disk-persistent
//! variant is required because `serve`, `status --pipeline` and `health` are
//! separate (and, for the CLIs, short-lived) processes from the daemon — an
//! in-process ETag cache would be born empty on every invocation.
//!
//! Only two things stay on GraphQL, because the REST issues listing cannot
//! answer them:
//!
//! - **`merged_24h`** — a merged-in-window search (one call per root).
//! - **`operator_held_conflicting`** — needs each held PR's `mergeable`; the
//!   `gh pr list --label loom:operator` call runs **only when the REST
//!   `loom:operator` count is non-zero**, which is usually not the case.
//!
//! So an unchanged root costs at most ~2 *billable* (non-`304`) calls.
//!
//! **Truncation:** the REST listing is a single 100-row page. When a label's
//! page comes back full it may be truncated, and a capped `100` would be read
//! by every consumer as an exact count, so that metric (only) falls back to
//! its pre-#9253 GraphQL query.

use std::path::Path;

use anyhow::Result;

use super::{GhPipelineSource, RepoPipelineSnapshot};
use crate::forge_listing::{self, CachedListing, RestIssue};
use crate::work_finder::PARK_LABELS;

/// Whether a metric counts the issue rows or the PR rows of a REST listing
/// (REST issue listings return both — [`RestIssue::is_pull_request`]).
#[derive(Debug, Clone, Copy)]
enum Side {
    Issue,
    Pr,
}

impl Side {
    fn matches(self, row: &RestIssue) -> bool {
        match self {
            Self::Issue => !row.is_pull_request,
            Self::Pr => row.is_pull_request,
        }
    }

    /// The `gh` noun for this side's GraphQL fallback query.
    fn noun(self) -> &'static str {
        match self {
            Self::Issue => "issue",
            Self::Pr => "pr",
        }
    }
}

fn has_any(row: &RestIssue, labels: &[&str]) -> bool {
    row.labels.iter().any(|l| labels.contains(&l.as_str()))
}

/// Records the first failure (the [`RepoPipelineSnapshot::error`] contract)
/// and turns each metric's result into `Some`/`None`.
#[derive(Default)]
struct Recorder {
    first_err: Option<String>,
}

impl Recorder {
    fn take<T>(&mut self, result: Result<T>) -> Option<T> {
        match result {
            Ok(v) => Some(v),
            Err(e) => {
                if self.first_err.is_none() {
                    self.first_err = Some(e.to_string());
                }
                None
            }
        }
    }
}

/// Per-root fetch context: the source's knobs plus the root's resolved repo.
struct Ctx<'a> {
    src: &'a GhPipelineSource,
    root: &'a Path,
    /// The root's `owner/repo`, passed explicitly so an ambient `LOOM_REPO`
    /// can never point every root's listing at one repo. `None` (not a git
    /// checkout) lets `gh` resolve the `{owner}/{repo}` placeholder from cwd.
    repo: Option<String>,
}

impl Ctx<'_> {
    /// The open rows carrying `label`, via the disk-persistent ETag cache.
    fn listing(&self, label: &str) -> Result<CachedListing> {
        forge_listing::list_issues_cached_persistent(
            &self.src.gh_bin,
            Some(self.root),
            self.repo.as_deref(),
            label,
            "open",
        )
    }

    /// The pre-#9253 GraphQL count of open `side` rows labeled `label` — the
    /// fallback for a truncated REST page.
    fn graphql_label_count(&self, side: Side, label: &str) -> Result<usize> {
        self.src.count(
            self.root,
            &[
                side.noun(),
                "list",
                "--state",
                "open",
                "--label",
                label,
                "--json",
                "number",
                "--limit",
                "500",
            ],
        )
    }

    /// The pre-#9253 GraphQL `--search` count — the fallback for a truncated
    /// REST page on a metric that needs label negation.
    fn graphql_search_count(&self, side: Side, search: &str) -> Result<usize> {
        self.src.count(
            self.root,
            &[
                side.noun(),
                "list",
                "--search",
                search,
                "--json",
                "number",
                "--limit",
                "500",
            ],
        )
    }

    /// Count `side` rows of the `label` listing that pass `keep`, or run
    /// `fallback` when the page may be truncated.
    fn count(
        &self,
        label: &str,
        side: Side,
        keep: impl Fn(&RestIssue) -> bool,
        fallback: impl FnOnce() -> Result<usize>,
    ) -> Result<usize> {
        let listing = self.listing(label)?;
        if listing.truncated {
            log::info!(
                "pipeline_snapshot: {label} listing for {} is a full page; using the GraphQL \
                 count instead (#9253)",
                self.root.display()
            );
            return fallback();
        }
        Ok(listing
            .issues
            .iter()
            .filter(|r| side.matches(r) && keep(r))
            .count())
    }
}

/// Whole days since an RFC-3339 timestamp, floored at 0.
fn age_days(ts: &str, now: chrono::DateTime<chrono::Utc>) -> Option<i64> {
    let at = chrono::DateTime::parse_from_rfc3339(ts).ok()?;
    Some((now - at.with_timezone(&chrono::Utc)).num_days().max(0))
}

/// [`GhPipelineSource`]'s per-root fetch. Honours the [`super::PipelineMetrics`]
/// mask exactly (a masked metric stays `None`) and the partial-failure
/// contract (a failed metric is `None`, the first error is kept).
pub(super) fn fetch(src: &GhPipelineSource, root: &Path) -> RepoPipelineSnapshot {
    let ctx = Ctx {
        src,
        root,
        repo: crate::credential_preflight::nwo_from_git_remote(root),
    };
    let m = src.metrics;
    let mut rec = Recorder::default();
    let mut snap = RepoPipelineSnapshot {
        root: root.to_path_buf(),
        ..Default::default()
    };

    if m.queued {
        snap.queued = rec.take(ctx.count(
            "loom:issue",
            Side::Issue,
            |r| !has_any(r, &PARK_LABELS),
            || ctx.graphql_search_count(Side::Issue, &GhPipelineSource::queued_search_query()),
        ));
    }
    if m.building {
        snap.building = rec.take(ctx.count(
            "loom:building",
            Side::Issue,
            |_| true,
            || ctx.graphql_label_count(Side::Issue, "loom:building"),
        ));
    }
    if m.review_requested {
        snap.review_requested = rec.take(ctx.count(
            "loom:review-requested",
            Side::Pr,
            |_| true,
            || ctx.graphql_label_count(Side::Pr, "loom:review-requested"),
        ));
    }
    if m.changes_requested || m.changes_requested_unclaimed {
        // One listing serves both metrics.
        match ctx.listing("loom:changes-requested") {
            Ok(listing) if !listing.truncated => {
                let prs: Vec<&RestIssue> = listing
                    .issues
                    .iter()
                    .filter(|r| Side::Pr.matches(r))
                    .collect();
                if m.changes_requested {
                    snap.changes_requested = Some(prs.len());
                }
                if m.changes_requested_unclaimed {
                    snap.changes_requested_unclaimed = Some(
                        prs.iter()
                            .filter(|r| {
                                !has_any(r, &["loom:treating"]) && !has_any(r, &PARK_LABELS)
                            })
                            .count(),
                    );
                }
            }
            Ok(_) => {
                if m.changes_requested {
                    snap.changes_requested =
                        rec.take(ctx.graphql_label_count(Side::Pr, "loom:changes-requested"));
                }
                if m.changes_requested_unclaimed {
                    let search = GhPipelineSource::changes_requested_unclaimed_search_query();
                    snap.changes_requested_unclaimed =
                        rec.take(ctx.graphql_search_count(Side::Pr, &search));
                }
            }
            Err(e) => {
                rec.take::<()>(Err(e));
            }
        }
    }
    if m.approved {
        snap.approved = rec.take(ctx.count(
            "loom:pr",
            Side::Pr,
            |_| true,
            || ctx.graphql_label_count(Side::Pr, "loom:pr"),
        ));
    }
    if m.merged {
        // REST cannot serve a merged-in-window count; one GraphQL search.
        let search = GhPipelineSource::merged_since_query(chrono::Utc::now(), src.merge_window);
        snap.merged_24h = rec.take(src.count(
            root,
            &[
                "pr", "list", "--state", "merged", "--search", &search, "--json", "number",
                "--limit", "500",
            ],
        ));
    }
    if m.operator_held {
        fetch_operator_held(&ctx, &mut rec, &mut snap);
    }
    if m.operator_only_issues {
        snap.operator_only_issues = rec.take(ctx.count(
            "loom:operator-only",
            Side::Issue,
            |_| true,
            || ctx.graphql_label_count(Side::Issue, "loom:operator-only"),
        ));
    }

    snap.error = rec.first_err;
    snap
}

/// The three `loom:operator` metrics (Issue #8091). Count and age come from
/// the REST listing; `mergeable` needs the GraphQL row query, which runs only
/// when there is at least one held PR (or the REST page may be truncated). A
/// failed REST read leaves all three `None` — never a fabricated 0 held.
fn fetch_operator_held(ctx: &Ctx<'_>, rec: &mut Recorder, snap: &mut RepoPipelineSnapshot) {
    let now = chrono::Utc::now();
    let listing = match ctx.listing("loom:operator") {
        Ok(l) => l,
        Err(e) => {
            rec.take::<()>(Err(e));
            return;
        }
    };
    if listing.truncated {
        // Pre-#9253 path: the GraphQL rows carry all three fields.
        if let Some(rows) = rec.take(ctx.src.operator_held_rows(ctx.root)) {
            snap.operator_held = Some(rows.len());
            snap.operator_held_conflicting = Some(
                rows.iter()
                    .filter(|r| r.mergeable.as_deref() == Some("CONFLICTING"))
                    .count(),
            );
            snap.operator_held_oldest_days = rows
                .iter()
                .map(|r| r.created_at)
                .min()
                .map(|oldest| (now - oldest).num_days().max(0));
        }
        return;
    }
    let held: Vec<&RestIssue> = listing
        .issues
        .iter()
        .filter(|r| Side::Pr.matches(r))
        .collect();
    snap.operator_held = Some(held.len());
    snap.operator_held_oldest_days = held
        .iter()
        .filter_map(|r| r.created_at.as_deref())
        .filter_map(|ts| age_days(ts, now))
        .max();
    snap.operator_held_conflicting = if held.is_empty() {
        Some(0)
    } else {
        rec.take(ctx.src.operator_held_rows(ctx.root)).map(|rows| {
            rows.iter()
                .filter(|r| r.mergeable.as_deref() == Some("CONFLICTING"))
                .count()
        })
    };
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
#[path = "rest_source_tests.rs"]
mod tests;
