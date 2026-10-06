//! The repository owner behind `head=<owner>:<branch>` PR lookups, and the
//! guards that keep a stale owner from turning into a false "no PR".
//!
//! [`repo_owner`] answers from [`crate::forge_repo_facts`] (no forge call on a
//! warm record) instead of `gh api repos/{owner}/{repo} --jq .owner.login` on
//! every pass. A remembered owner can be stale after a rename or transfer, and
//! GitHub answers a `head=` filter with the wrong owner with `[]` — which
//! [`super::clean::select_pr_status`] reads as [`PrStatus::NoPr`]. So, for an
//! owner that came from a fact:
//!
//! - every row of a non-empty answer must name the canonical repo as its
//!   `base.repo.full_name`, else the answer is [`PrStatus::Unknown`]
//!   ([`pr_status_validated`]);
//! - an empty answer is re-confirmed against the forge before it may be
//!   `NoPr` ([`pr_status_confirmed`], at most one confirm per root per
//!   [`crate::forge_repo_facts::PassScope`]).
//!
//! With `LOOM_REPO_FACTS=0` (or a root pinned to legacy) every function here
//! issues exactly the pre-facts calls.

use std::path::Path;

use super::clean::{self, PrStatus};
use super::gh;
use crate::forge_repo_facts::{self as facts, GhRepoEnv, Lookup, OwnerFact};

/// One row of `GET repos/{o}/{r}/pulls…` (or `pulls/<n>`).
#[derive(Debug, serde::Deserialize)]
pub(crate) struct PrRowRest {
    pub(crate) state: String,
    #[serde(default)]
    pub(crate) merged_at: Option<String>,
    #[serde(default)]
    pub(crate) closed_at: Option<String>,
    #[serde(default)]
    pub(crate) head: Option<PrHeadRest>,
    /// `base.repo.full_name`: the repository the PR targets — the canonical
    /// (post-redirect) name, which validates the owner that built the filter.
    #[serde(default)]
    pub(crate) base: Option<PrSideRest>,
}

/// The `head` of a REST pull-request payload. `sha` is the safety criterion
/// for force-deleting a `pr-<N>` worktree's local branch (issue #5939,
/// mirroring `merge-pr.sh`'s #4100 rule); `repo.owner.login` is the fork
/// owner the `head=` filter matched.
#[derive(Debug, serde::Deserialize)]
pub(crate) struct PrHeadRest {
    #[serde(default)]
    pub(crate) sha: Option<String>,
    #[serde(default)]
    pub(crate) repo: Option<PrRepoRest>,
}

/// The `base` side of a REST pull-request payload.
#[derive(Debug, serde::Deserialize)]
pub(crate) struct PrSideRest {
    #[serde(default)]
    pub(crate) repo: Option<PrRepoRest>,
}

#[derive(Debug, serde::Deserialize)]
pub(crate) struct PrRepoRest {
    #[serde(default)]
    pub(crate) full_name: Option<String>,
    #[serde(default)]
    pub(crate) owner: Option<PrOwnerRest>,
}

#[derive(Debug, serde::Deserialize)]
pub(crate) struct PrOwnerRest {
    #[serde(default)]
    pub(crate) login: Option<String>,
}

impl PrRowRest {
    /// `base.repo.full_name`, when present.
    pub(crate) fn base_full_name(&self) -> Option<&str> {
        self.base.as_ref()?.repo.as_ref()?.full_name.as_deref()
    }

    /// `head.repo.owner.login`, when present.
    pub(crate) fn head_owner(&self) -> Option<&str> {
        self.head
            .as_ref()?
            .repo
            .as_ref()?
            .owner
            .as_ref()?
            .login
            .as_deref()
    }
}

/// Why [`fetch_pr_rows`] has no rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RowsError {
    /// `gh` gave no answer, failed, or printed something unparseable.
    Failed,
    /// The repository answered 404 / 410.
    Gone,
}

/// `repos/{owner}/{repo}/pulls?state=all&head=<owner>:<branch>&per_page=30` —
/// the one REST list call [`clean::check_pr_status_for_branch_rest`] has
/// always made, unchanged (same argv, same `clean.pr_status_rest` row).
pub(crate) fn fetch_pr_rows(
    repo_root: &Path,
    owner: &str,
    branch: &str,
) -> Result<Vec<PrRowRest>, RowsError> {
    let path =
        format!("repos/{{owner}}/{{repo}}/pulls?state=all&head={owner}:{branch}&per_page=30");
    let out = gh::bounded_counted("clean.pr_status_rest", repo_root, ["api", &path])
        .ok_or(RowsError::Failed)?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        if stderr.contains("HTTP 404") || stderr.contains("HTTP 410") {
            return Err(RowsError::Gone);
        }
        return Err(RowsError::Failed);
    }
    serde_json::from_slice::<Vec<PrRowRest>>(&out.stdout).map_err(|_| RowsError::Failed)
}

/// [`clean::select_pr_status`] over REST rows (empty ⇒ `NoPr`).
pub(crate) fn rows_status(rows: &[PrRowRest]) -> PrStatus {
    clean::select_pr_status(rows.iter().map(|row| {
        clean::classify_pr_row(
            row.state.as_str(),
            row.merged_at.as_deref(),
            row.closed_at.as_deref(),
        )
    }))
}

/// The owner `gh api repos/{owner}/{repo}` would report for `repo_root`
/// (`GH_REPO`/`LOOM_REPO` honoured, post-redirect).
///
/// From the canonical record when facts are on; the legacy
/// [`clean::repo_owner_rest`] call when they are off or the root is pinned to
/// legacy. `None` when neither can answer — callers keep their GraphQL
/// fallback.
#[must_use]
pub(crate) fn repo_owner(repo_root: &Path) -> Option<OwnerFact> {
    match facts::canonical(repo_root, GhRepoEnv::Honour) {
        Lookup::Fact(f) => Some(OwnerFact::from_fact(f)),
        Lookup::Unavailable => None,
        Lookup::Legacy => clean::repo_owner_rest(repo_root).map(OwnerFact::legacy),
    }
}

/// Every row must target the canonical repo. A row that does not (or carries
/// no `base.repo`) casts doubt on the record and makes the answer unusable.
/// A fully matching answer refreshes the record for free.
fn rows_match(fact: &facts::Fact, rows: &[PrRowRest], sent_at: i64) -> bool {
    let canonical = fact.full_name();
    for row in rows {
        match row.base_full_name() {
            Some(full) if full.eq_ignore_ascii_case(&canonical) => {}
            seen => {
                if let Some(full) = seen {
                    facts::observe(&fact.host, &fact.configured_nwo, full, sent_at);
                }
                log::warn!(
                    "clean: a pulls row names base {} (head owner {}), not {canonical}; the PR \
                     status is unknown",
                    seen.unwrap_or("<none>"),
                    row.head_owner().unwrap_or("<none>")
                );
                return false;
            }
        }
    }
    if !rows.is_empty() {
        facts::observe(&fact.host, &fact.configured_nwo, &canonical, sent_at);
    }
    true
}

fn unix_now() -> i64 {
    chrono::Utc::now().timestamp()
}

/// [`clean::check_pr_status_for_branch_rest`] for an owner from
/// [`repo_owner`]: the same call, plus row validation when the owner came
/// from a fact. An empty answer is `NoPr` here — for a decision that may act
/// on `NoPr`, use [`pr_status_confirmed`].
#[must_use]
pub(crate) fn pr_status_validated(repo_root: &Path, owner: &OwnerFact, branch: &str) -> PrStatus {
    let Some(fact) = owner.fact.as_ref() else {
        return clean::check_pr_status_for_branch_rest(repo_root, &owner.owner, branch);
    };
    let sent_at = unix_now();
    match fetch_pr_rows(repo_root, &owner.owner, branch) {
        Ok(rows) if rows_match(fact, &rows, sent_at) => rows_status(&rows),
        Ok(_) => PrStatus::Unknown,
        Err(RowsError::Gone) => {
            facts::invalidate(repo_root, "pulls listing answered 404/410");
            PrStatus::Unknown
        }
        Err(RowsError::Failed) => PrStatus::Unknown,
    }
}

/// [`pr_status_validated`], with an owner-confirmed `NoPr`: an empty answer
/// built from a remembered owner is `NoPr` only when a forced re-read says
/// that owner is still current; otherwise (a different owner, or a failed
/// read) `Unknown`, which every reaper maps to a skip. For the reapers'
/// `pr_status` probes, inside their [`facts::PassScope`].
#[must_use]
pub(crate) fn pr_status_confirmed(repo_root: &Path, owner: &OwnerFact, branch: &str) -> PrStatus {
    let status = pr_status_validated(repo_root, owner, branch);
    if owner.fact.is_none() || !matches!(status, PrStatus::NoPr) {
        return status;
    }
    if facts::confirm_owner(repo_root, GhRepoEnv::Honour, &owner.owner) {
        PrStatus::NoPr
    } else {
        PrStatus::Unknown
    }
}
