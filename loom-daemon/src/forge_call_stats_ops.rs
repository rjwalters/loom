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
/// `GET search/issues` — duplicate/phrase searches.
pub const ISSUE_SEARCH: ForgeOp = ForgeOp::inventoried("issue.search");
/// `GET repos/{o}/{r}/issues/{n}/comments`.
pub const COMMENT_LIST: ForgeOp = ForgeOp::inventoried("comment.list");
/// `GET repos/{o}/{r}/pulls/{n}` — one PR's decision state (mergeability…).
pub const PR_VIEW_STATE: ForgeOp = ForgeOp::inventoried("pr.view-state");
/// The closing-issue references of a batch of PRs (GraphQL).
pub const PR_CLOSING_ISSUE_REFERENCES: ForgeOp =
    ForgeOp::inventoried("pr.closing-issue-references");
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

/// Every inventoried constant above — the set the inventory test checks.
pub const ALL_INVENTORIED: &[ForgeOp] = &[
    ISSUE_LIST,
    ISSUE_SEARCH,
    COMMENT_LIST,
    PR_VIEW_STATE,
    PR_CLOSING_ISSUE_REFERENCES,
    CI_WORKFLOW_RUNS_FOR_SHA,
    CI_RUN_LOGS_AND_ARTIFACTS,
    REPO_LIST_FOR_OWNER,
    GIT_READ_OBJECTS,
    GIT_WRITE_REFS_AND_CONTENTS,
    TIMELINE_READ,
    QUOTA_RATE_LIMIT_READING,
];
