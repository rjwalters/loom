//! Typed inventoried operation IDs for the call-identity layer (Issue #9831).
//!
//! #9777 gave [`super::CallIdentity`] an `operation` field keyed on the forge
//! operation inventory (`defaults/forge/operations/*.toml`), but a bare string
//! lets a call site pass a typo — or nothing at all — and silently land in the
//! `unknown` row. This module is the vocabulary a migrated call site names its
//! operation with instead:
//!
//! - every inventoried ID a production site records is a constant here, and
//!   `every_named_operation_is_inventoried` (in
//!   `forge_call_stats_callsite_tests.rs`) fails the build when one of them is
//!   not an active row of the embedded inventory;
//! - a site that genuinely maps to no inventoried row says so with
//!   [`ForgeOp::uninventoried`], whose `why` argument is the required comment —
//!   a deliberate `unknown` is fine, a merely unmigrated one is debt.
//!
//! A [`crate::forge_etag_store::fetch_conditional`] caller must pass a
//! [`ForgeOp`]; there is no default, so a new conditional reader cannot stay
//! `unknown` by omission.

/// An operation as the call-identity layer records it: an inventoried ID, or
/// a deliberate, documented `unknown`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ForgeOp(Option<&'static str>);

impl ForgeOp {
    /// An inventoried operation ID. Only the constants below should call
    /// this; the inventory check covers exactly those.
    #[must_use]
    pub const fn inventoried(id: &'static str) -> Self {
        Self(Some(id))
    }

    /// A call that maps to no single inventoried operation. Records
    /// [`super::UNKNOWN_OPERATION`]. `why` is not stored — it exists so the
    /// reason sits at the call site, where a reviewer reads it.
    #[must_use]
    pub const fn uninventoried(why: &'static str) -> Self {
        let _ = why;
        Self(None)
    }

    /// The inventoried ID, or `None` for a deliberate `unknown`.
    #[must_use]
    pub const fn id(self) -> Option<&'static str> {
        self.0
    }
}

/// `GET repos/{o}/{r}/issues?labels=…` — the ETag-cached issue listings.
pub const ISSUE_LIST: ForgeOp = ForgeOp::inventoried("issue.list");
/// `GET repos/{o}/{r}/issues/{n}` — one issue's (or PR's) state and labels.
pub const ISSUE_VIEW_STATE: ForgeOp = ForgeOp::inventoried("issue.view-state");
/// The PRs that close a batch of issues (GraphQL `closedByPullRequestsReferences`).
pub const ISSUE_CLOSED_BY_PULL_REQUESTS: ForgeOp =
    ForgeOp::inventoried("issue.closed-by-pull-requests");
/// `GET search/issues` — duplicate/phrase searches.
pub const ISSUE_SEARCH: ForgeOp = ForgeOp::inventoried("issue.search");
/// `GET repos/{o}/{r}/issues/{n}/comments`.
pub const COMMENT_LIST: ForgeOp = ForgeOp::inventoried("comment.list");
/// `GET repos/{o}/{r}/pulls/{n}` — one PR's decision state (mergeability…).
pub const PR_VIEW_STATE: ForgeOp = ForgeOp::inventoried("pr.view-state");
/// `GET repos/{o}/{r}` — default branch / visibility.
pub const REPO_VIEW: ForgeOp = ForgeOp::inventoried("repo.view");
/// The closing-issue references of a batch of PRs (GraphQL).
pub const PR_CLOSING_ISSUE_REFERENCES: ForgeOp =
    ForgeOp::inventoried("pr.closing-issue-references");
/// `GET repos/{o}/{r}/pulls/{n}/reviews` — one PR's formal reviews.
pub const REVIEW_LIST_FORMAL: ForgeOp = ForgeOp::inventoried("review.list-formal");
/// `GET repos/{o}/{r}/commits/{sha}/check-runs` — one commit's check runs.
pub const CI_CHECK_RUNS_FOR_SHA: ForgeOp = ForgeOp::inventoried("ci.check-runs-for-sha");
/// `GET repos/{o}/{r}/actions/runs…` (and the jobs of one run).
pub const CI_WORKFLOW_RUNS_FOR_SHA: ForgeOp = ForgeOp::inventoried("ci.workflow-runs-for-sha");
/// Job logs, run artifacts and artifact downloads.
pub const CI_RUN_LOGS_AND_ARTIFACTS: ForgeOp = ForgeOp::inventoried("ci.run-logs-and-artifacts");
/// `GET orgs/{o}/repos` / `GET users/{u}/repos`.
pub const REPO_LIST_FOR_OWNER: ForgeOp = ForgeOp::inventoried("repo.list-for-owner");
/// Git-database / contents reads of the fleet store.
pub const GIT_READ_OBJECTS: ForgeOp = ForgeOp::inventoried("git.read-objects");
/// Git-database / contents / ref writes of the fleet store.
pub const GIT_WRITE_REFS_AND_CONTENTS: ForgeOp =
    ForgeOp::inventoried("git.write-refs-and-contents");
/// `GET repos/{o}/{r}/issues/events` (and per-item events / timelines).
pub const TIMELINE_READ: ForgeOp = ForgeOp::inventoried("timeline.read");
/// `GET rate_limit` — the free budget probe.
pub const QUOTA_RATE_LIMIT_READING: ForgeOp = ForgeOp::inventoried("quota.rate-limit-reading");
/// The open PR whose head is a given branch.
pub const PR_LIST_BY_HEAD: ForgeOp = ForgeOp::inventoried("pr.list-by-head");
/// `GET repos/{o}/{r}/pulls?state=open` — every open PR (#10382).
pub const PR_LIST_OPEN: ForgeOp = ForgeOp::inventoried("pr.list-open");
/// Add / remove labels on one issue or PR (a PR's labels are issue labels).
pub const ISSUE_EDIT_LABELS: ForgeOp = ForgeOp::inventoried("issue.edit-labels");
/// `PATCH repos/{o}/{r}/issues/{n}` — replace an issue's (or PR's) body.
pub const ISSUE_EDIT_BODY: ForgeOp = ForgeOp::inventoried("issue.edit-body");
/// Post a comment on an issue or PR.
pub const COMMENT_CREATE: ForgeOp = ForgeOp::inventoried("comment.create");
/// Edit or delete an existing comment by id.
pub const COMMENT_EDIT_DELETE: ForgeOp = ForgeOp::inventoried("comment.edit-delete");
/// A PR's changed-file list.
pub const PR_DIFF_AND_FILES: ForgeOp = ForgeOp::inventoried("pr.diff-and-files");
/// Resolve a release and download its artifact (`gh release view|download`).
pub const RELEASE_RESOLVE_AND_FETCH: ForgeOp = ForgeOp::inventoried("release.resolve-and-fetch");

/// Re-run a workflow run or job in place (`forge rerun`, #10633).
pub const CI_RERUN: ForgeOp = ForgeOp::inventoried("ci.rerun");

/// Every inventoried constant above — the set the inventory test checks.
pub const ALL_INVENTORIED: &[ForgeOp] = &[
    ISSUE_LIST,
    ISSUE_VIEW_STATE,
    ISSUE_CLOSED_BY_PULL_REQUESTS,
    ISSUE_SEARCH,
    COMMENT_LIST,
    PR_VIEW_STATE,
    REPO_VIEW,
    PR_CLOSING_ISSUE_REFERENCES,
    REVIEW_LIST_FORMAL,
    CI_CHECK_RUNS_FOR_SHA,
    CI_WORKFLOW_RUNS_FOR_SHA,
    CI_RUN_LOGS_AND_ARTIFACTS,
    CI_RERUN,
    REPO_LIST_FOR_OWNER,
    GIT_READ_OBJECTS,
    GIT_WRITE_REFS_AND_CONTENTS,
    TIMELINE_READ,
    QUOTA_RATE_LIMIT_READING,
    PR_LIST_BY_HEAD,
    PR_LIST_OPEN,
    ISSUE_EDIT_LABELS,
    ISSUE_EDIT_BODY,
    COMMENT_CREATE,
    COMMENT_EDIT_DELETE,
    PR_DIFF_AND_FILES,
    RELEASE_RESOLVE_AND_FETCH,
];
