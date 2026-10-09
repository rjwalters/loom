//! The forge [`RawEventSource`]s for the raw fleet event cache (#10197).
//!
//! Four paged REST listings, one [`ForgeEndpoint`] each, read by the same
//! [`ForgeEventSource`]: the issue-events listing (below), the pulls listing
//! ([`super::fleet_events_pulls`]: PR open/merge/close times, closing
//! references and head commits, 100 PRs per call, op
//! `pr.closing-issue-references`), and two per-PR listings walked by
//! [`super::fleet_events_fanout`] ([`super::fleet_events_reviews`]): a PR's
//! reviews (op `review.list-formal`) and its head commit's check runs (op
//! `ci.check-runs-for-sha`). A per-PR read answering `404` / `422` (a deleted
//! PR, a garbage-collected commit) is an empty last page, so that PR settles
//! instead of stopping every later run.
//!
//! The issue-events endpoint reads the repo-wide issue-events listing,
//! `GET repos/{owner}/{repo}/issues/events?per_page=100&page=N`, through the
//! shared ETag store's conditional read
//! ([`crate::forge_etag_store::fetch_conditional`]): the same reader-App
//! routing, credential scoping and call accounting (`timeline.read`) every
//! other conditional REST read in the daemon gets. One listing covers every
//! issue and PR in the repo, newest first, 100 rows per call — so a backfill
//! costs `ceil(events / 100)` calls and a quiet refresh costs one `304`.
//!
//! Each row yields its own event (`labeled`, `unlabeled`, `closed`,
//! `reopened`, `merged`; other kinds are skipped) **plus** an `opened` row
//! synthesised from the embedded item's `created_at`, so an item's open time
//! is known without a per-item read. The synthesised row's id does not depend
//! on which event carried it, so it is stored once.
//!
//! [`ForgeEventSource::reader_only`] (#10263) is the daemon's variant: the
//! same listings (issue events and pulls, #10298) read through the
//! reader-only primitive, never the writer.
//!
//! # Known limit
//!
//! GitHub serves this listing only to a bounded depth (it answers `422` past
//! it). A `422` ends the backfill as complete; history older than that window
//! is the webhook mirror's job (a later #10197 increment). The pulls listing
//! has no such window.

use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::Deserialize;

use super::fleet_events::{EventKind, ItemKind, PageFetch, RawEvent, RawEventSource, SOURCE_FORGE};
use super::fleet_events_fanout::{PerPrKind, PerPrSource};

/// Rows per page — the REST maximum.
pub const PER_PAGE: u32 = 100;

/// The cursor key of the issue-events endpoint.
pub const ENDPOINT: &str = "issues-events";

/// The cursor key of the pulls endpoint.
pub const PULLS_ENDPOINT: &str = "pulls";

/// Which paged forge listing a [`ForgeEventSource`] reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForgeEndpoint {
    /// `GET repos/{o}/{r}/issues/events` — labels, close/reopen/merge.
    IssuesEvents,
    /// `GET repos/{o}/{r}/pulls?state=all` — PR open/merge/close times,
    /// closing references and head commits.
    Pulls,
    /// `GET repos/{o}/{r}/pulls/{n}/reviews`, one PR at a time.
    Reviews,
    /// `GET repos/{o}/{r}/commits/{sha}/check-runs`, one PR head at a time.
    CheckRuns,
}

impl ForgeEndpoint {
    /// Every endpoint, in the order a full sync reads them (check runs need
    /// the head commits the pulls listing records).
    pub const ALL: [ForgeEndpoint; 4] = [
        ForgeEndpoint::IssuesEvents,
        ForgeEndpoint::Pulls,
        ForgeEndpoint::Reviews,
        ForgeEndpoint::CheckRuns,
    ];

    /// The endpoint's cursor-key suffix (and CLI name).
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            ForgeEndpoint::IssuesEvents => ENDPOINT,
            ForgeEndpoint::Pulls => PULLS_ENDPOINT,
            ForgeEndpoint::Reviews => PerPrKind::Reviews.name(),
            ForgeEndpoint::CheckRuns => PerPrKind::CheckRuns.name(),
        }
    }

    /// The per-PR walk this endpoint is, if it is one.
    #[must_use]
    pub fn per_pr(self) -> Option<PerPrKind> {
        match self {
            ForgeEndpoint::Reviews => Some(PerPrKind::Reviews),
            ForgeEndpoint::CheckRuns => Some(PerPrKind::CheckRuns),
            ForgeEndpoint::IssuesEvents | ForgeEndpoint::Pulls => None,
        }
    }

    /// The endpoint named `name`.
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|e| e.name() == name)
    }

    fn url(self, repo: &str, subject: &Subject, page: u32) -> String {
        let pr = subject.pr;
        let sha = subject.sha.as_deref().unwrap_or("");
        match self {
            ForgeEndpoint::IssuesEvents => {
                format!("repos/{repo}/issues/events?per_page={PER_PAGE}&page={page}")
            }
            ForgeEndpoint::Pulls => format!(
                "repos/{repo}/pulls?state=all&sort=created&direction=desc&per_page={PER_PAGE}&page={page}"
            ),
            ForgeEndpoint::Reviews => {
                format!("repos/{repo}/pulls/{pr}/reviews?per_page={PER_PAGE}&page={page}")
            }
            ForgeEndpoint::CheckRuns => {
                format!("repos/{repo}/commits/{sha}/check-runs?per_page={PER_PAGE}&page={page}")
            }
        }
    }

    fn op(self) -> crate::forge_call_stats::ForgeOp {
        use crate::forge_call_stats::ops;
        match self {
            ForgeEndpoint::IssuesEvents => ops::TIMELINE_READ,
            ForgeEndpoint::Pulls => ops::PR_CLOSING_ISSUE_REFERENCES,
            ForgeEndpoint::Reviews => ops::REVIEW_LIST_FORMAL,
            ForgeEndpoint::CheckRuns => ops::CI_CHECK_RUNS_FOR_SHA,
        }
    }

    fn parse(
        self,
        repo: &str,
        pr: u32,
        body: &str,
        fetched_at: DateTime<Utc>,
    ) -> anyhow::Result<(Vec<RawEvent>, usize)> {
        use super::fleet_events_reviews::{parse_check_runs, parse_reviews};
        match self {
            ForgeEndpoint::IssuesEvents => parse_issue_events(repo, body, fetched_at),
            ForgeEndpoint::Pulls => super::fleet_events_pulls::parse_pulls(repo, body, fetched_at),
            ForgeEndpoint::Reviews => parse_reviews(repo, pr, body, fetched_at),
            ForgeEndpoint::CheckRuns => parse_check_runs(repo, pr, body, fetched_at),
        }
    }
}

/// A repo-wide listing's request: no PR.
impl From<ForgeEndpoint> for (ForgeEndpoint, u32) {
    fn from(endpoint: ForgeEndpoint) -> Self {
        (endpoint, 0)
    }
}

/// The PR (and head commit) a per-PR endpoint is pointed at. Unused by the
/// repo-wide listings.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Subject {
    pub pr: u32,
    pub sha: Option<String>,
}

/// The facade operation name every call here is recorded under.
const CALLER: &str = "eta_fleet_events";

#[derive(Deserialize)]
struct RestEvent {
    id: u64,
    event: String,
    created_at: DateTime<Utc>,
    #[serde(default)]
    label: Option<RestLabel>,
    #[serde(default)]
    issue: Option<RestItem>,
}

#[derive(Deserialize)]
struct RestLabel {
    name: String,
}

#[derive(Deserialize)]
struct RestItem {
    number: u32,
    created_at: DateTime<Utc>,
    #[serde(default)]
    pull_request: Option<serde_json::Value>,
}

/// Parse one page of the issue-events listing into raw rows read at
/// `fetched_at`. Returns the rows and how many listing rows the page held (the
/// page is the last one when that is under [`PER_PAGE`]).
///
/// # Errors
///
/// The body is not a JSON array of issue events.
pub fn parse_issue_events(
    repo: &str,
    body: &str,
    fetched_at: DateTime<Utc>,
) -> anyhow::Result<(Vec<RawEvent>, usize)> {
    let rows: Vec<RestEvent> = serde_json::from_str(body)?;
    let mut events = Vec::new();
    for row in &rows {
        let Some(item) = &row.issue else { continue };
        let item_kind = if item.pull_request.is_some() {
            ItemKind::Pr
        } else {
            ItemKind::Issue
        };
        let (kind, label) = match row.event.as_str() {
            "labeled" => (EventKind::LabelAdded, row.label.as_ref().map(|l| l.name.clone())),
            "unlabeled" => (EventKind::LabelRemoved, row.label.as_ref().map(|l| l.name.clone())),
            "closed" => (EventKind::Closed, None),
            "reopened" => (EventKind::Reopened, None),
            "merged" => (EventKind::Merged, None),
            _ => continue,
        };
        if matches!(kind, EventKind::LabelAdded | EventKind::LabelRemoved) && label.is_none() {
            continue;
        }
        events.push(RawEvent::new(
            repo,
            item.number,
            item_kind,
            EventKind::Opened,
            None,
            item.created_at,
            SOURCE_FORGE,
            0,
            fetched_at,
        ));
        events.push(RawEvent::new(
            repo,
            item.number,
            item_kind,
            kind,
            label,
            row.created_at,
            SOURCE_FORGE,
            row.id,
            fetched_at,
        ));
    }
    Ok((events, rows.len()))
}

/// A forge source: one repo's [`ForgeEndpoint`] listing, read with `gh`.
pub struct ForgeEventSource {
    endpoint: ForgeEndpoint,
    subject: Subject,
    repo: String,
    root: PathBuf,
    gh_bin: PathBuf,
    /// Stop (cleanly, resumable) when the response says fewer than this many
    /// core calls remain, so a backfill never drains the pool the fleet's
    /// daemons share.
    reserve: u64,
    /// The last response reported fewer than `reserve` calls remaining.
    below_reserve: bool,
    /// `Some` = reader-only ([`ForgeEventSource::reader_only`], #10263).
    reader: Option<super::fleet_fetch::Reader>,
    /// Requests issued (every attempt, failures included).
    calls: u64,
    /// Why the last [`PageFetch::Stopped`] stopped.
    last_stop: Option<super::fleet_refresh::StopReason>,
}

impl ForgeEventSource {
    /// A source reading `endpoint` for `repo`, running `gh` from `root` under
    /// that root's credential.
    #[must_use]
    pub fn new(endpoint: ForgeEndpoint, repo: &str, root: &Path, reserve: u64) -> Self {
        ForgeEventSource {
            endpoint,
            subject: Subject::default(),
            repo: repo.to_string(),
            root: root.to_path_buf(),
            gh_bin: PathBuf::from(crate::gh_invocation::gh_bin()),
            reserve,
            below_reserve: false,
            reader: None,
            calls: 0,
            last_stop: None,
        }
    }

    /// A **reader-only** source (#10263): every page is read under `reader`
    /// through [`crate::forge_etag_store::fetch_with_reader`] — no writer
    /// fallback, no env token — and a credential failure withdraws the reader
    /// and stops rather than retrying anywhere else. What the daemon's fleet
    /// refresh task syncs the raw cache with; the CLI keeps [`Self::new`].
    #[must_use]
    pub fn reader_only(
        endpoint: ForgeEndpoint,
        repo: &str,
        root: &Path,
        reserve: u64,
        reader: super::fleet_fetch::Reader,
    ) -> Self {
        let mut source = Self::new(endpoint, repo, root, reserve);
        source.reader = Some(reader);
        source
    }

    /// Requests issued so far, failures included.
    #[must_use]
    pub fn calls(&self) -> u64 {
        self.calls
    }

    /// Why the last stop happened, when the source stopped.
    #[must_use]
    pub fn last_stop(&self) -> Option<super::fleet_refresh::StopReason> {
        self.last_stop
    }

    /// Use `gh_bin` instead of the resolved `gh` (tests).
    #[must_use]
    pub fn with_gh_bin(mut self, gh_bin: &Path) -> Self {
        self.gh_bin = gh_bin.to_path_buf();
        self
    }
}

impl PerPrSource for ForgeEventSource {
    fn select(&mut self, pr: u32, sha: Option<&str>) {
        self.subject = Subject {
            pr,
            sha: sha.map(str::to_string),
        };
    }
}

impl RawEventSource for ForgeEventSource {
    fn cursor_key(&self) -> String {
        match self.endpoint.per_pr() {
            Some(kind) => kind.item_key(self.subject.pr, self.subject.sha.as_deref()),
            None => format!("{SOURCE_FORGE}:{}", self.endpoint.name()),
        }
    }

    fn newest_first(&self) -> bool {
        self.endpoint.per_pr().is_none()
    }

    fn fetch_page(&mut self, page: u32, etag: Option<&str>) -> PageFetch {
        use super::fleet_refresh::StopReason;
        if self.below_reserve {
            self.last_stop = Some(StopReason::Reserve);
            return PageFetch::Stopped(format!(
                "fewer than {} core calls remain (reserve floor); re-run after the reset",
                self.reserve
            ));
        }
        if self.reader.is_some() && crate::rate_limit_breaker::global_is_suppressed() {
            self.last_stop = Some(StopReason::BreakerOpen);
            return PageFetch::Stopped("rate-limit breaker open".to_string());
        }
        let target = crate::forge_etag_store::resolve_target(Some(&self.root), Some(&self.repo));
        let url = self.endpoint.url(&self.repo, &self.subject, page);
        let site = crate::forge_etag_store::ConditionalRead::new(CALLER, self.endpoint.op());
        let fetched_at = Utc::now();
        self.calls += 1;
        let answer = match &self.reader {
            Some(reader) => crate::forge_etag_store::fetch_with_reader(
                site,
                &self.gh_bin,
                Some(&self.root),
                &target,
                &url,
                etag,
                &reader.dir,
            ),
            None => crate::forge_etag_store::fetch_conditional(
                site,
                &self.gh_bin,
                Some(&self.root),
                &target,
                &url,
                etag,
            ),
        };
        let (status, response, stderr) = match answer {
            Ok(answer) => answer,
            Err(e) => {
                self.last_stop = Some(StopReason::ForgeError);
                return PageFetch::Stopped(format!("gh failed: {e:#}"));
            }
        };
        let Some(response) = response else {
            self.last_stop = Some(StopReason::ForgeError);
            return PageFetch::Stopped(format!("no HTTP response (exit {status}): {stderr}"));
        };
        // A page already paid for is always kept; the floor stops the *next*
        // request, so a run below the reserve costs at most one call.
        self.below_reserve = response
            .ratelimit
            .remaining
            .is_some_and(|r| r < self.reserve);
        if let Some(reader) = &self.reader {
            if withdraws_reader(self.endpoint, response.status) {
                self.last_stop =
                    Some(withdraw_reader(reader, &self.repo, &url, &response, &stderr));
            }
        }
        let request = (self.endpoint, self.subject.pr);
        classify(request, &self.repo, page, response, &stderr, fetched_at)
    }
}

/// Whether a reader read answered with `status` is the reader's failure.
/// 200/304 are answers and 422 is past the listing's window; a per-PR 404 is
/// a PR or commit that is gone, which [`classify`] settles as an empty last
/// page (#10197) -- withdrawing the reader for it would stop every later run.
fn withdraws_reader(endpoint: ForgeEndpoint, status: u16) -> bool {
    match status {
        200 | 304 | 422 => false,
        404 => endpoint.per_pr().is_none(),
        _ => true,
    }
}

/// A reader-only read failed: classify it, withdraw the reader when the
/// failure is the credential's (#9537), and name the stop.
fn withdraw_reader(
    reader: &super::fleet_fetch::Reader,
    repo: &str,
    url: &str,
    response: &crate::forge_listing::HttpResponse,
    stderr: &str,
) -> super::fleet_refresh::StopReason {
    use super::fleet_fetch::ReadFailure;
    use super::fleet_refresh::StopReason;
    let why = format!("{CALLER} {url}");
    let failure = super::fleet_fetch::reader_failure(
        stderr,
        Some(response.status),
        Some(&response.ratelimit),
    );
    if let Some(f) = failure {
        crate::forge_identity::withdraw_after(&reader.app_id, repo, f, &why);
    }
    match ReadFailure::of(failure) {
        ReadFailure::RateLimited => StopReason::RateLimited,
        ReadFailure::Coverage => StopReason::Coverage,
        ReadFailure::Other => StopReason::ForgeError,
    }
}

/// Turn one HTTP answer into a [`PageFetch`]. Pure, so every branch is tested
/// without a `gh`.
///
/// `request` is the endpoint and, for a per-PR one, the PR the rows belong to
/// (ignored otherwise).
pub(crate) fn classify(
    request: impl Into<(ForgeEndpoint, u32)>,
    repo: &str,
    page: u32,
    response: crate::forge_listing::HttpResponse,
    stderr: &str,
    fetched_at: DateTime<Utc>,
) -> PageFetch {
    let (endpoint, pr) = request.into();
    match response.status {
        304 => PageFetch::NotModified,
        200 => match endpoint.parse(repo, pr, &response.body, fetched_at) {
            Ok((events, rows)) => PageFetch::Page {
                events,
                etag: response.etag,
                last: rows < PER_PAGE as usize,
            },
            Err(e) => PageFetch::Stopped(format!("page {page}: unparseable body: {e}")),
        },
        // Past the listing's pagination window: nothing older is served. For
        // a per-PR read, a PR or commit that is gone: nothing to read.
        404 if endpoint.per_pr().is_some() => PageFetch::Page {
            events: Vec::new(),
            etag: None,
            last: true,
        },
        422 => PageFetch::Page {
            events: Vec::new(),
            etag: None,
            last: true,
        },
        status => {
            let why = if crate::rate_limit_breaker::indicates_rate_limit(stderr)
                || status == 429
                || (status == 403 && response.ratelimit.remaining == Some(0))
            {
                "rate limited"
            } else {
                "forge error"
            };
            PageFetch::Stopped(format!("page {page}: HTTP {status} ({why}): {stderr}"))
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::forge_listing::HttpResponse;

    const BODY: &str = r#"[
      {"id": 30, "event": "labeled", "created_at": "2026-10-04T09:28:20Z",
       "label": {"name": "loom:building", "color": "x"},
       "issue": {"number": 9970, "created_at": "2026-10-02T18:45:51Z", "state": "open"}},
      {"id": 29, "event": "merged", "created_at": "2026-10-04T09:00:00Z",
       "issue": {"number": 77, "created_at": "2026-10-03T00:00:00Z",
                 "pull_request": {"url": "u"}, "state": "closed"}},
      {"id": 28, "event": "subscribed", "created_at": "2026-10-04T08:00:00Z",
       "issue": {"number": 9970, "created_at": "2026-10-02T18:45:51Z"}},
      {"id": 27, "event": "unlabeled", "created_at": "2026-10-04T07:00:00Z",
       "label": {"name": "loom:issue"},
       "issue": {"number": 9970, "created_at": "2026-10-02T18:45:51Z"}}
    ]"#;

    #[test]
    fn a_per_pr_404_settles_without_withdrawing_the_reader() {
        // A deleted PR / GC'd commit is an empty last page (#10197), not a
        // reader coverage failure (#9537).
        assert!(!withdraws_reader(ForgeEndpoint::Reviews, 404));
        assert!(!withdraws_reader(ForgeEndpoint::CheckRuns, 404));
        // A repo-wide 404 is still the credential's coverage gap.
        assert!(withdraws_reader(ForgeEndpoint::IssuesEvents, 404));
        assert!(withdraws_reader(ForgeEndpoint::Pulls, 404));
        for endpoint in ForgeEndpoint::ALL {
            for status in [200, 304, 422] {
                assert!(!withdraws_reader(endpoint, status));
            }
            for status in [401, 403, 429, 502] {
                assert!(withdraws_reader(endpoint, status));
            }
        }
    }

    fn now() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-10-04T10:00:00Z")
            .unwrap()
            .with_timezone(&Utc)
    }

    fn response(status: u16, body: &str) -> HttpResponse {
        HttpResponse {
            status,
            etag: Some("W/\"e1\"".to_string()),
            body: body.to_string(),
            ratelimit: crate::forge_call_stats::RateLimitHeaders::default(),
            next_page: false,
        }
    }

    #[test]
    fn parses_known_kinds_and_synthesises_one_opened_row_per_item() {
        let (events, rows) = parse_issue_events("o/r", BODY, now()).unwrap();
        assert_eq!(rows, 4);
        let mut log: Vec<RawEvent> = events;
        super::super::fleet_events::canonicalize(&mut log);
        let kinds: Vec<(u32, EventKind, ItemKind)> =
            log.iter().map(|e| (e.item, e.kind, e.item_kind)).collect();
        assert_eq!(
            kinds,
            vec![
                (9970, EventKind::Opened, ItemKind::Issue),
                (77, EventKind::Opened, ItemKind::Pr),
                (9970, EventKind::LabelRemoved, ItemKind::Issue),
                (77, EventKind::Merged, ItemKind::Pr),
                (9970, EventKind::LabelAdded, ItemKind::Issue),
            ]
        );
        assert!(log
            .iter()
            .all(|e| e.fetched_at == now() && e.source == SOURCE_FORGE));
        let added = log
            .iter()
            .find(|e| e.kind == EventKind::LabelAdded)
            .unwrap();
        assert_eq!(added.label.as_deref(), Some("loom:building"));
        assert_eq!(added.seq, 30);
    }

    #[test]
    fn a_short_page_is_the_last_and_a_full_one_is_not() {
        match classify(ForgeEndpoint::IssuesEvents, "o/r", 1, response(200, BODY), "", now()) {
            PageFetch::Page { last, etag, .. } => {
                assert!(last);
                assert_eq!(etag.as_deref(), Some("W/\"e1\""));
            }
            other => panic!("{other:?}"),
        }
        let row = r#"{"id": 1, "event": "closed", "created_at": "2026-10-04T07:00:00Z",
                     "issue": {"number": 1, "created_at": "2026-10-01T00:00:00Z"}}"#;
        let full = format!("[{}]", vec![row; PER_PAGE as usize].join(","));
        assert!(matches!(
            classify(ForgeEndpoint::IssuesEvents, "o/r", 1, response(200, &full), "", now()),
            PageFetch::Page { last: false, .. }
        ));
    }

    #[test]
    fn not_modified_window_end_and_rate_limits_classify() {
        assert_eq!(
            classify(ForgeEndpoint::IssuesEvents, "o/r", 1, response(304, ""), "", now()),
            PageFetch::NotModified
        );
        assert!(matches!(
            classify(ForgeEndpoint::IssuesEvents, "o/r", 400, response(422, "{}"), "", now()),
            PageFetch::Page { last: true, ref events, .. } if events.is_empty()
        ));
        match classify(
            ForgeEndpoint::IssuesEvents,
            "o/r",
            3,
            response(403, ""),
            "API rate limit exceeded",
            now(),
        ) {
            PageFetch::Stopped(why) => assert!(why.contains("rate limited"), "{why}"),
            other => panic!("{other:?}"),
        }
        assert!(matches!(
            classify(ForgeEndpoint::IssuesEvents, "o/r", 3, response(200, "not json"), "", now()),
            PageFetch::Stopped(_)
        ));
    }
    #[test]
    fn the_pulls_endpoint_parses_through_the_same_classifier() {
        let body = r#"[{"id": 5, "number": 3, "created_at": "2026-10-01T00:00:00Z",
                        "closed_at": null, "merged_at": null, "body": "Closes #2"}]"#;
        match classify(ForgeEndpoint::Pulls, "o/r", 1, response(200, body), "", now()) {
            PageFetch::Page { events, last, .. } => {
                assert!(last);
                assert!(events
                    .iter()
                    .any(|e| e.kind == EventKind::ClosingRef && e.target == Some(2)));
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(ForgeEndpoint::from_name("pulls"), Some(ForgeEndpoint::Pulls));
        assert_eq!(ForgeEndpoint::from_name("nope"), None);
        assert!(ForgeEndpoint::Pulls
            .url("o/r", &Subject::default(), 4)
            .starts_with("repos/o/r/pulls?state=all&sort=created&direction=desc"));
        let source = ForgeEventSource::new(ForgeEndpoint::Pulls, "o/r", Path::new("."), 1);
        assert_eq!(source.cursor_key(), "forge:pulls");
        assert!(source.newest_first());
    }

    #[test]
    fn per_pr_endpoints_name_their_pr_in_the_url_the_key_and_the_rows() {
        let subject = Subject {
            pr: 12,
            sha: Some("abc".to_string()),
        };
        assert_eq!(
            ForgeEndpoint::Reviews.url("o/r", &subject, 2),
            "repos/o/r/pulls/12/reviews?per_page=100&page=2"
        );
        assert_eq!(
            ForgeEndpoint::CheckRuns.url("o/r", &subject, 1),
            "repos/o/r/commits/abc/check-runs?per_page=100&page=1"
        );
        let mut source = ForgeEventSource::new(ForgeEndpoint::CheckRuns, "o/r", Path::new("."), 1);
        source.select(12, Some("abc"));
        assert_eq!(source.cursor_key(), "forge:check-runs#12@abc");
        assert!(!source.newest_first());
        let mut source = ForgeEventSource::new(ForgeEndpoint::Reviews, "o/r", Path::new("."), 1);
        source.select(12, None);
        assert_eq!(source.cursor_key(), "forge:reviews#12");

        let reviews = r#"[{"id": 1, "state": "APPROVED", "submitted_at": "2026-10-04T09:00:00Z"}]"#;
        match classify((ForgeEndpoint::Reviews, 12), "o/r", 1, response(200, reviews), "", now()) {
            PageFetch::Page { events, last, .. } => {
                assert!(last);
                assert_eq!(events.len(), 1);
                assert_eq!((events[0].item, events[0].kind), (12, EventKind::Review));
            }
            other => panic!("{other:?}"),
        }
        let checks = r#"{"total_count": 0, "check_runs": []}"#;
        assert!(matches!(
            classify((ForgeEndpoint::CheckRuns, 12), "o/r", 1, response(200, checks), "", now()),
            PageFetch::Page { last: true, ref events, .. } if events.is_empty()
        ));
    }

    #[test]
    fn a_gone_pr_or_commit_settles_but_a_gone_listing_still_stops() {
        for endpoint in [ForgeEndpoint::Reviews, ForgeEndpoint::CheckRuns] {
            assert!(matches!(
                classify((endpoint, 3), "o/r", 1, response(404, "{}"), "", now()),
                PageFetch::Page { last: true, ref events, .. } if events.is_empty()
            ));
            match classify((endpoint, 3), "o/r", 1, response(429, ""), "", now()) {
                PageFetch::Stopped(why) => assert!(why.contains("rate limited"), "{why}"),
                other => panic!("{other:?}"),
            }
        }
        assert!(matches!(
            classify(ForgeEndpoint::Pulls, "o/r", 1, response(404, ""), "", now()),
            PageFetch::Stopped(_)
        ));
        assert_eq!(
            ForgeEndpoint::ALL.map(ForgeEndpoint::name),
            ["issues-events", "pulls", "reviews", "check-runs"]
        );
    }
}
