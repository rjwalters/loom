//! Reader-only forge reads for the daemon's fleet snapshot refresh (#10263).
//!
//! # What this is
//!
//! The forge half of [`super::fleet_refresh`]: a [`ForgeRead`] seam (tests
//! inject a fake), its one production implementation [`ReaderForge`], and the
//! pure REST → [`PrHistory`] mapping. Two endpoints, both REST, never GraphQL:
//!
//! - **Listing** — `repos/{o}/{r}/issues?state=all&sort=updated&direction=desc
//!   &per_page=100&page=N` ([`listing_url`]). One fixed URL per page, with no
//!   `since=`, so page 1's ETag stays reusable from one refresh to the next and
//!   a quiet repo costs one `304`. Issues and PRs share the listing; a row with
//!   a `pull_request` object is a PR ([`parse_listing`]).
//! - **Timeline** — `repos/{o}/{r}/issues/{n}/timeline?per_page=100&page=N`
//!   ([`timeline_url`]), paged **explicitly** by the caller so every page is
//!   one budgeted call, and parsed with the CLI's own
//!   [`crate::pr_latency::timeline::parse_timeline_page`].
//!
//! # Reader Apps only
//!
//! Every request runs under a repo's reader App through
//! [`crate::forge_etag_store::fetch_with_reader`]: the reader's `GH_CONFIG_DIR`
//! with every token env var removed, and **no writer fallback**. A failed read
//! is classified ([`crate::forge_identity::classify_failure`]) and the reader
//! withdrawn — App-wide for a rate limit, for this repo only for a coverage
//! gap — and the failure is returned to the caller, which stops. Nothing here
//! can spend the operator's credential; that is the point of this module.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use chrono::{DateTime, Utc};
use serde::Deserialize;

use crate::forge_call_stats::ForgeOp;
use crate::pr_latency::{PrEvent, PrHistory, PrState};

/// Rows per page — the REST maximum, for both endpoints.
pub const PER_PAGE: usize = 100;

/// The facade operation name every read here is recorded under.
pub const CALLER: &str = "eta_fleet_refresh";

/// A repo's reader App, as resolved for one cycle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reader {
    /// The App id — not a secret; reported on the cycle record.
    pub app_id: String,
    /// The reader's `GH_CONFIG_DIR`.
    pub dir: PathBuf,
}

/// Why a repo has no usable reader this cycle. Either way it costs zero calls.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoReader {
    /// A github.com repo with no fresh, un-withdrawn reader App.
    NoReader,
    /// A forge other than github.com (Gitea, GHE): reader Apps do not exist
    /// there.
    UnsupportedForge,
}

/// One registered repo, as a cycle sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoTarget {
    /// `owner/repo`.
    pub repo: String,
    /// The remote's forge host, when known.
    pub host: Option<String>,
    /// Where `gh` runs: the repo's provisioned root, else the daemon's.
    pub cwd: PathBuf,
    /// The reader the repo's reads run under, or why there is none.
    pub reader: Result<Reader, NoReader>,
}

/// What a failed read says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadFailure {
    /// The reader App as a whole cannot serve (rate limit, bad credentials).
    RateLimited,
    /// The repo is outside the reader's installation (403/404).
    Coverage,
    /// Anything else: a 5xx, a transport failure, a timeout.
    Other,
}

/// One GET's answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Read {
    /// A `200` or a `304`.
    Ok {
        status: u16,
        etag: Option<String>,
        body: String,
        /// `x-ratelimit-remaining`, when sent.
        remaining: Option<u64>,
    },
    /// Anything else. The reader has already been withdrawn when the failure
    /// was the credential's.
    Failed {
        failure: ReadFailure,
        remaining: Option<u64>,
        /// `x-ratelimit-reset`, epoch seconds, when sent.
        reset_epoch: Option<i64>,
        detail: String,
    },
}

/// The forge seam [`super::fleet_refresh::run_cycle`] reads through.
pub trait ForgeRead {
    /// The host's rate-limit breaker is suppressing forge calls.
    fn breaker_open(&self) -> bool;
    /// The daemon is shutting down: stop at the next call boundary.
    fn shutting_down(&self) -> bool {
        false
    }
    /// One GET of `url` (a `repos/…` path) under `reader`, conditional on
    /// `etag` when given, accounted under `op`.
    fn get(
        &mut self,
        target: &RepoTarget,
        reader: &Reader,
        url: &str,
        etag: Option<&str>,
        op: ForgeOp,
    ) -> Read;
}

/// The listing URL for `page` (1-based).
#[must_use]
pub fn listing_url(repo: &str, page: u32) -> String {
    format!(
        "repos/{repo}/issues?state=all&sort=updated&direction=desc&per_page={PER_PAGE}&page={page}"
    )
}

/// The timeline URL for PR `number`, `page` (1-based).
#[must_use]
pub fn timeline_url(repo: &str, number: u32, page: u32) -> String {
    format!("repos/{repo}/issues/{number}/timeline?per_page={PER_PAGE}&page={page}")
}

#[derive(Deserialize)]
struct RestRow {
    number: u32,
    state: String,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
    #[serde(default)]
    labels: Vec<RestLabel>,
    #[serde(default)]
    pull_request: Option<RestPullRef>,
}

#[derive(Deserialize)]
struct RestLabel {
    name: String,
}

#[derive(Deserialize)]
struct RestPullRef {
    #[serde(default)]
    merged_at: Option<DateTime<Utc>>,
}

/// One listing row: an issue or a PR, with what ordering and the
/// [`PrHistory`] need.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListedRow {
    pub number: u32,
    /// The listing's sort key.
    pub updated_at: DateTime<Utc>,
    /// `Some` for a PR.
    pub pr: Option<ListedPr>,
}

/// A PR row's own facts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListedPr {
    pub created_at: DateTime<Utc>,
    pub state: PrState,
    pub merged_at: Option<DateTime<Utc>>,
    pub labels: Vec<String>,
}

/// Parse one listing page, in listing order.
///
/// # Errors
///
/// The body is not a JSON array of issue rows.
pub fn parse_listing(body: &str) -> anyhow::Result<Vec<ListedRow>> {
    let rows: Vec<RestRow> = serde_json::from_str(body)?;
    Ok(rows
        .into_iter()
        .map(|row| {
            let pr = row.pull_request.map(|pull| ListedPr {
                created_at: row.created_at,
                // The `gh pr list` vocabulary the CLI maps through
                // `PrState::parse`: merged wins, then closed, else open.
                state: if pull.merged_at.is_some() {
                    PrState::Merged
                } else if row.state.eq_ignore_ascii_case("closed") {
                    PrState::Closed
                } else {
                    PrState::Open
                },
                merged_at: pull.merged_at,
                labels: row.labels.into_iter().map(|l| l.name).collect(),
            });
            ListedRow {
                number: row.number,
                updated_at: row.updated_at,
                pr,
            }
        })
        .collect())
}

/// The [`PrHistory`] for a listed PR and its timeline events — the same value
/// the CLI's `gh pr list` + `--paginate` path builds.
#[must_use]
pub fn history(number: u32, pr: &ListedPr, events: Vec<PrEvent>, complete: bool) -> PrHistory {
    PrHistory::new(
        number,
        pr.created_at,
        pr.state,
        pr.merged_at,
        pr.labels.clone(),
        events,
        complete,
    )
}

/// Classify a failed read. The breaker's own rate-limit vocabulary plus #9537's
/// credential classifier; a `403` that reports zero remaining is a rate limit
/// even when `gh`'s message is terse.
#[must_use]
pub fn classify(stderr: &str, http: Option<u16>, remaining: Option<u64>) -> ReadFailure {
    if http == Some(403) && remaining == Some(0) {
        return ReadFailure::RateLimited;
    }
    match crate::forge_identity::classify_failure(stderr, http) {
        Some(crate::forge_identity::Failure::App) => ReadFailure::RateLimited,
        Some(crate::forge_identity::Failure::Coverage) => ReadFailure::Coverage,
        None => ReadFailure::Other,
    }
}

/// The production [`ForgeRead`]: `gh api --include` under the repo's reader,
/// never the writer.
pub struct ReaderForge {
    gh_bin: PathBuf,
}

impl ReaderForge {
    #[must_use]
    pub fn new() -> Self {
        ReaderForge {
            gh_bin: PathBuf::from(crate::gh_invocation::gh_bin()),
        }
    }

    /// Use `gh_bin` instead of the resolved `gh` (tests).
    #[must_use]
    pub fn with_gh_bin(gh_bin: &Path) -> Self {
        ReaderForge {
            gh_bin: gh_bin.to_path_buf(),
        }
    }
}

impl Default for ReaderForge {
    fn default() -> Self {
        Self::new()
    }
}

impl ForgeRead for ReaderForge {
    fn breaker_open(&self) -> bool {
        crate::rate_limit_breaker::global_is_suppressed()
    }

    fn get(
        &mut self,
        target: &RepoTarget,
        reader: &Reader,
        url: &str,
        etag: Option<&str>,
        op: ForgeOp,
    ) -> Read {
        let site = crate::forge_etag_store::ConditionalRead::new(CALLER, op);
        let resolved = crate::forge_etag_store::Target {
            repo: Some(target.repo.clone()),
            host: target.host.clone(),
        };
        let answer = crate::forge_etag_store::fetch_with_reader(
            site,
            &self.gh_bin,
            Some(&target.cwd),
            &resolved,
            url,
            etag,
            &reader.dir,
        );
        let (status, response, stderr) = match answer {
            Ok(answer) => answer,
            Err(e) => {
                return Read::Failed {
                    failure: ReadFailure::Other,
                    remaining: None,
                    reset_epoch: None,
                    detail: format!("{e:#}"),
                }
            }
        };
        let http = response.as_ref().map(|r| r.status);
        let remaining = response.as_ref().and_then(|r| r.ratelimit.remaining);
        let reset_epoch = response.as_ref().and_then(|r| r.ratelimit.reset_epoch);
        if let Some(r) = response.filter(|r| matches!(r.status, 200 | 304)) {
            return Read::Ok {
                status: r.status,
                etag: r.etag,
                body: r.body,
                remaining,
            };
        }
        let failure = classify(&stderr, http, remaining);
        let why = format!("{CALLER} {url}");
        match failure {
            ReadFailure::RateLimited => {
                let until = reset_epoch
                    .and_then(|s| u64::try_from(s).ok())
                    .map(|s| SystemTime::UNIX_EPOCH + Duration::from_secs(s));
                crate::forge_identity::withdraw_after(
                    &reader.app_id,
                    &target.repo,
                    crate::forge_identity::Failure::App,
                    until,
                    &why,
                );
            }
            ReadFailure::Coverage => crate::forge_identity::withdraw_after(
                &reader.app_id,
                &target.repo,
                crate::forge_identity::Failure::Coverage,
                None,
                &why,
            ),
            ReadFailure::Other => {}
        }
        let http = http.map_or_else(|| "no HTTP response".to_string(), |s| format!("HTTP {s}"));
        Read::Failed {
            failure,
            remaining,
            reset_epoch,
            detail: format!("{http} (exit {status}): {stderr}"),
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn urls_are_fixed_per_page_and_never_carry_since() {
        assert_eq!(
            listing_url("o/r", 1),
            "repos/o/r/issues?state=all&sort=updated&direction=desc&per_page=100&page=1"
        );
        assert!(!listing_url("o/r", 7).contains("since"));
        assert_eq!(timeline_url("o/r", 42, 2), "repos/o/r/issues/42/timeline?per_page=100&page=2");
    }

    #[test]
    fn failures_classify_by_status_and_message() {
        assert_eq!(
            classify("API rate limit exceeded (HTTP 403)", Some(403), Some(12)),
            ReadFailure::RateLimited
        );
        assert_eq!(classify("", Some(403), Some(0)), ReadFailure::RateLimited);
        assert_eq!(classify("", Some(429), None), ReadFailure::RateLimited);
        assert_eq!(classify("Bad credentials", Some(401), None), ReadFailure::RateLimited);
        assert_eq!(classify("Not Found (HTTP 404)", Some(404), Some(4000)), ReadFailure::Coverage);
        assert_eq!(classify("", Some(502), Some(4000)), ReadFailure::Other);
        assert_eq!(classify("connection reset", None, None), ReadFailure::Other);
    }
}
