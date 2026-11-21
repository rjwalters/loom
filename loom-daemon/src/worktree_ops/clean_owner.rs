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
//!
//! The repo is the checkout's own (W6, [`super::forge_state`]): gh's base
//! repo with `GH_REPO` / `LOOM_REPO` ignored, and a listing built from a
//! fact names that repo explicitly instead of gh's placeholder.

use std::path::Path;

use super::clean::{self, PrStatus};
use crate::forge_repo_facts::{self as facts, GhRepoEnv, Lookup, OwnerFact};
use crate::gh_invocation::{AccessIntent, GhInvocation, GhTarget, Operation, ReadClass};

/// One row of `GET repos/{o}/{r}/pulls…` (or `pulls/<n>`).
#[derive(Debug, serde::Deserialize)]
pub(crate) struct PrRowRest {
    /// The PR number (identity check of a single-PR read, W6).
    #[serde(default)]
    pub(crate) number: Option<u64>,
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
    #[serde(default, rename = "ref")]
    pub(crate) branch: Option<String>,
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
    /// The numeric repository id: rename-proof identity (W6).
    #[serde(default)]
    pub(crate) id: Option<u64>,
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

/// `repos/<repo>/pulls?state=all&head=<owner>:<branch>&per_page=30` — the
/// one REST list call [`clean::check_pr_status_for_branch_rest`] has always
/// made (same `clean.pr_status_rest` row). `<repo>` is the fact's canonical
/// repo when the owner came from one, else gh's placeholder; `GH_REPO` is
/// stripped either way (W6). Unconditional: a listing is not dropped by
/// `gh-cached --invalidate`, so a stored one could outlive a merge.
pub(crate) fn fetch_pr_rows(
    repo_root: &Path,
    owner: &str,
    branch: &str,
    fact: Option<&facts::Fact>,
) -> Result<Vec<PrRowRest>, RowsError> {
    let path = super::forge_state::pulls_by_head_path(fact, owner, branch);
    let out = super::forge_state::hygiene_get(
        "clean.pr_status_rest",
        None,
        repo_root,
        &path,
        fact,
        crate::gh_invocation::ReadClass::Hygiene,
    )
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

/// Fresh GitHub PR evidence for an independent clone. Use the clone's
/// verified origin rather than resolving a base from its Git config. One
/// complete page suffices; if it could be truncated, preserve the worktree.
pub(crate) fn finished_pr_at_head(
    repo_root: &Path,
    host: &str,
    slug: &str,
    branch: &str,
    tip: &str,
) -> bool {
    finished_pr_request(repo_root, host, slug, branch)
        .and_then(super::gh::bounded_via)
        .filter(|out| out.status.success())
        .and_then(|out| serde_json::from_slice::<Vec<PrRowRest>>(&out.stdout).ok())
        .is_some_and(|rows| finished_pr_in_rows(&rows, slug, branch, tip))
}

fn finished_pr_request(
    repo_root: &Path,
    host: &str,
    slug: &str,
    branch: &str,
) -> Option<GhInvocation> {
    let target = GhTarget::repo(slug).ok()?;
    let (owner, name) = slug.split_once('/')?;
    let mut url = reqwest::Url::parse(&format!("https://{host}/")).ok()?;
    url.path_segments_mut()
        .ok()?
        .extend(["repos", owner, name, "pulls"]);
    url.query_pairs_mut()
        .append_pair("state", "all")
        .append_pair("head", &format!("{owner}:{branch}"))
        .append_pair("per_page", "100");
    let endpoint = format!("{}?{}", url.path().trim_start_matches('/'), url.query()?);

    // Parameters are already in the URL. gh api defaults to GET with
    // this argv and would pick POST if given form-field arguments.
    Some(
        GhInvocation::new(
            Operation::new("clean.pr_status_rest"),
            AccessIntent::Read,
            target,
            super::gh::GH_PROBE_TIMEOUT,
        )
        .args(["api", "--hostname", host, &endpoint])
        .current_dir(repo_root)
        .read_class(ReadClass::Hygiene)
        .strip_env("GH_REPO"),
    )
}

fn finished_pr_in_rows(rows: &[PrRowRest], slug: &str, branch: &str, tip: &str) -> bool {
    if rows.len() >= 100 {
        return false;
    }
    let mut finished = false;
    for row in rows {
        if !row
            .base_full_name()
            .is_some_and(|base| base.eq_ignore_ascii_case(slug))
        {
            return false;
        }
        let Some(head) = row.head.as_ref() else {
            return false;
        };
        let Some(head_repository) = head
            .repo
            .as_ref()
            .and_then(|repo| repo.full_name.as_deref())
        else {
            return false;
        };
        if !head_repository.eq_ignore_ascii_case(slug) {
            continue;
        }
        if head.branch.as_deref() != Some(branch) {
            return false;
        }
        // The host reaper prefers a merge anywhere in a branch's history.
        // Forced removal of dirty private work is narrower: a newer open PR
        // on the same branch must take precedence over any historical merge.
        if !row.state.eq_ignore_ascii_case("closed") {
            return false;
        }
        let status =
            clean::classify_pr_row(&row.state, row.merged_at.as_deref(), row.closed_at.as_deref());
        if head.sha.as_deref() == Some(tip)
            && matches!(
                status,
                PrStatus::Merged { .. } | PrStatus::ClosedNoMerge { closed_at: Some(_) }
            )
        {
            finished = true;
        }
    }
    finished
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

/// The owner of `repo_root`'s own repo (gh's base repo, `GH_REPO` /
/// `LOOM_REPO` ignored — W6), post-redirect.
///
/// From the canonical record when facts are on; the legacy
/// [`clean::repo_owner_rest`] call when they are off or the root is pinned to
/// legacy. `None` when neither can answer — callers keep their GraphQL
/// fallback.
#[must_use]
pub(crate) fn repo_owner(repo_root: &Path) -> Option<OwnerFact> {
    match facts::canonical(repo_root, GhRepoEnv::Ignore) {
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
    match fetch_pr_rows(repo_root, &owner.owner, branch, Some(fact)) {
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
    if facts::confirm_owner(repo_root, GhRepoEnv::Ignore, &owner.owner) {
        PrStatus::NoPr
    } else {
        PrStatus::Unknown
    }
}

#[cfg(test)]
mod private_clone_tests {
    use super::*;
    use serde_json::{json, Value};

    #[test]
    fn finished_pr_lookup_sends_encoded_query_only_get() {
        let dir = tempfile::tempdir().unwrap();
        let gh = dir.path().join("fake-gh");
        crate::exec_test_support::write_executable(
            &gh,
            r#"#!/bin/sh
set -eu
test "$#" -eq 4
test "$1" = api
test "$2" = --hostname
test "$3" = forge.example
test "$4" = 'repos/Owner/Repo/pulls?state=all&head=Owner%3Afeature%2Fa%26b%23c%2B%25d%3De&per_page=100'
test -z "${GH_REPO+x}"
printf '%s\n' '[{"state":"closed","closed_at":"2026-11-18T21:12:00Z","base":{"repo":{"full_name":"Owner/Repo"}},"head":{"ref":"feature/a&b#c+%d=e","sha":"abc","repo":{"full_name":"Owner/Repo"}}}]'
"#,
        );
        let request =
            finished_pr_request(dir.path(), "forge.example", "Owner/Repo", "feature/a&b#c+%d=e")
                .unwrap()
                .program(&gh);
        let output = crate::worktree_ops::gh::bounded_via(request).unwrap();
        assert!(output.status.success(), "{output:?}");
        let rows: Vec<PrRowRest> = serde_json::from_slice(&output.stdout).unwrap();
        assert!(finished_pr_in_rows(&rows, "Owner/Repo", "feature/a&b#c+%d=e", "abc"));
    }

    fn row(state: &str, repository: &str, branch: &str, tip: &str) -> Value {
        json!({
            "state": state,
            "head": {"ref": branch, "sha": tip, "repo": {"full_name": repository}},
            "base": {"repo": {"full_name": "Org/Repo"}},
        })
    }

    #[test]
    fn finished_pr_proves_exact_repository_branch_and_head() {
        let qualifies = |rows| {
            let rows: Vec<PrRowRest> = serde_json::from_value(rows).unwrap();
            finished_pr_in_rows(&rows, "org/repo", "topic", "abc")
        };
        let mut closed = row("closed", "Org/Repo", "topic", "abc");
        closed["closed_at"] = json!("2026-11-18T21:12:00Z");
        assert!(qualifies(json!([closed])));
        let mut merged = closed.clone();
        merged["merged_at"] = closed["closed_at"].clone();
        assert!(qualifies(json!([merged])));

        assert!(!qualifies(json!([])));
        assert!(!qualifies(json!([merged, row("open", "Org/Repo", "topic", "new-tip")])));
        for (field, value) in [
            ("/base/repo/full_name", "Org/Other"),
            ("/head/repo/full_name", "Org/Fork"),
            ("/head/ref", "different-topic"),
            ("/head/sha", "stale-tip"),
        ] {
            let mut wrong = merged.clone();
            *wrong.pointer_mut(field).unwrap() = json!(value);
            assert!(!qualifies(json!([wrong])), "{field}");
        }
        assert!(!qualifies(json!([row("closed", "Org/Repo", "topic", "abc")])));
        assert!(!qualifies(json!(vec![merged; 100])));
    }
}
