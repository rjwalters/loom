//! Live-mode forge reads (epic #7810, PR 4).
//!
//! Every call goes through [`crate::cmd_out`] (PR 2), so each is bounded and
//! each outcome classified rather than collapsed.
//!
//! # Fail safe on a failed read
//!
//! Every function here returns `Err` rather than a default when the forge could
//! not be read. The shell carries the reason on each of its `_die` calls:
//! *"cannot compute a fingerprint from a failed read (fail safe: never guess
//! 'clear' on missing data)"*. A fingerprint computed from a failed read is
//! worse than no fingerprint — it is a confident wrong answer that gets
//! persisted into a marker and compared against on every later pass.

use super::{extract, named, premise, recheck};
use crate::cmd_out::Query;
use crate::script_helpers::gh_read_json;
use serde::Deserialize;
use std::path::Path;

/// A read that did not produce an answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadError(pub String);

impl std::fmt::Display for ReadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

fn read_failed(what: &str) -> ReadError {
    ReadError(format!(
        "{what} failed — cannot compute a fingerprint from a failed read \
         (fail safe: never guess 'clear' on missing data)"
    ))
}

fn repo_args(repo: Option<&str>) -> Vec<&str> {
    repo.map_or_else(Vec::new, |r| vec!["--repo", r])
}

/// `gh pr view --json labels` returns label OBJECTS. Normalised to names at this
/// fetch boundary, once, so every consumer — and every `--stdin` fixture — sees
/// the one shape.
#[derive(Deserialize)]
struct Label {
    name: String,
}

#[derive(Deserialize)]
struct PrView {
    number: i64,
    #[serde(default)]
    state: String,
    #[serde(default)]
    labels: Vec<Label>,
    #[serde(default)]
    mergeable: String,
    #[serde(default, rename = "mergeStateStatus")]
    merge_state_status: String,
}

/// One PR's state, labels and merge state, in [`recheck::Pr`]'s shape.
///
/// # Errors
///
/// [`ReadError`] if the PR could not be read.
pub fn fetch_pr(pr: i64, repo: Option<&str>, repo_root: &Path) -> Result<recheck::Pr, ReadError> {
    let pn = pr.to_string();
    let mut args = vec!["pr", "view", &pn];
    args.extend(repo_args(repo));
    args.extend(["--json", "number,state,labels,mergeable,mergeStateStatus"]);
    let q: Query<PrView> =
        gh_read_json("recheck.pr_view", &args, repo_root, false, |_: &PrView| false);
    let Query::Populated(p) = q else {
        return Err(read_failed(&format!("gh pr view {pr}")));
    };
    Ok(recheck::Pr {
        number: p.number,
        state: p.state,
        labels: p.labels.into_iter().map(|l| l.name).collect(),
        mergeable: p.mergeable,
        merge_state_status: p.merge_state_status,
    })
}

/// The PRs declared to close `issue`, with each one's state.
///
/// # Errors
///
/// [`ReadError`] if the issue or any of its PRs could not be read.
pub fn fetch_prs(
    issue: i64,
    repo: Option<&str>,
    repo_root: &Path,
) -> Result<Vec<recheck::Pr>, ReadError> {
    #[derive(Deserialize)]
    struct ClosedBy {
        number: i64,
    }
    #[derive(Deserialize)]
    struct IssueView {
        #[serde(default, rename = "closedByPullRequestsReferences")]
        closed_by: Vec<ClosedBy>,
    }

    let n = issue.to_string();
    let mut args = vec!["issue", "view", &n];
    args.extend(repo_args(repo));
    args.extend(["--json", "closedByPullRequestsReferences"]);
    let q: Query<IssueView> =
        gh_read_json("recheck.issue_view", &args, repo_root, false, |_: &IssueView| false);
    let Query::Populated(view) = q else {
        return Err(read_failed(&format!("gh issue view {issue}")));
    };

    let mut out = Vec::with_capacity(view.closed_by.len());
    for c in &view.closed_by {
        out.push(fetch_pr(c.number, repo, repo_root)?);
    }
    Ok(out)
}

/// One node's `state`, trying `issue view` then `pr view`.
///
/// A reference is either an issue or a PR, and `gh issue view` on a PR number
/// exits non-zero — so the *failure* of the first lookup is the expected shape
/// of "this is a PR", not an error. Only when BOTH fail is it a real read
/// failure. (The same ladder as `dep_classify::state::ref_state`, which had to
/// learn that `Failed` must fall through, not just an empty result, or every PR
/// reference reads as unknown.)
fn fetch_state(number: i64, repo: Option<&str>, repo_root: &Path) -> Option<String> {
    #[derive(Deserialize)]
    struct StateField {
        #[serde(default)]
        state: String,
    }
    let n = number.to_string();
    for entity in ["issue", "pr"] {
        let mut args = vec![entity, "view", &n];
        args.extend(repo_args(repo));
        args.extend(["--json", "state"]);
        let q: Query<StateField> =
            gh_read_json("recheck.ref_state", &args, repo_root, false, |s: &StateField| {
                s.state.is_empty()
            });
        if let Query::Populated(s) = q {
            return Some(s.state);
        }
    }
    None
}

/// The state of each `--refs` number.
///
/// # Errors
///
/// [`ReadError`] if any reference could not be read as either an issue or a PR.
pub fn fetch_refs(
    numbers: &[i64],
    repo: Option<&str>,
    repo_root: &Path,
) -> Result<Vec<premise::Ref>, ReadError> {
    numbers
        .iter()
        .map(|&number| {
            fetch_state(number, repo, repo_root)
                .map(|state| premise::Ref { number, state })
                .ok_or_else(|| {
                    ReadError(format!(
                        "could not read state for reference #{number} \
                         (neither gh issue view nor gh pr view succeeded)"
                    ))
                })
        })
        .collect()
}

/// The state of one node, verified to be the answer **for that node** (#14710).
///
/// Used through [`BoundReader`], which adds the rename layer: a repository
/// that was renamed answers under its *canonical* slug (`rjwalters/loom#N`
/// reads back as `2AMLogic/loom`), so an answer that names the requested
/// slug exactly binds immediately, and one that does not is bound against
/// the requested slug's resolved canonical name before being rejected. A
/// redirect to the same repository is the same dependency; the unrelated
/// same-numbered node in a different repository (`2AMLogic/2am#630` vs the
/// open draft `2AMLogic/.loom#630`) still does not bind. See
/// [`super::states::url_names_ref`] for the accepted URL shapes.
///
/// Like [`fetch_state`] this tries `issue view` then `pr view`, because a
/// reference may name either — but it asks for `number,url,state` and refuses
/// an answer that does not name the requested `repo#number`: a number that
/// differs, or a URL for any other repository (or path shape), is treated as
/// **no** answer, not as the state of something else. That is the read the
/// Guide's unblock sweep needs: on 2026-11-19 a lookup for the
/// still-OPEN cross-repo draft `2AMLogic/.loom#630` was answered "resolved"
/// and the hold was lifted (#14710), and nothing in a bare
/// `--json state --jq .state` read can distinguish the right node's state
/// from a cached, shed or mismatched answer for the same number elsewhere
/// (`2AMLogic/2am#630` is CLOSED).
///
/// Returns `None` when neither entity kind yields a bound answer — unreadable
/// is a *result* here, and the caller classifies it as unknown, fail closed.
#[must_use]
pub fn fetch_bound_state(
    number: i64,
    repo: &str,
    repo_root: &Path,
    canonical: Option<&str>,
) -> Option<String> {
    #[derive(Deserialize)]
    struct NodeView {
        #[serde(default)]
        number: i64,
        #[serde(default)]
        state: String,
        #[serde(default)]
        url: String,
    }
    let n = number.to_string();
    for entity in ["issue", "pr"] {
        let args = vec![
            entity,
            "view",
            &n,
            "--repo",
            repo,
            "--json",
            "number,state,url",
        ];
        let q: Query<NodeView> =
            gh_read_json("recheck.state_bound", &args, repo_root, false, |v: &NodeView| {
                v.state.is_empty()
            });
        if let Query::Populated(v) = q {
            if v.number == number
                && (super::states::url_names_ref(&v.url, repo, number)
                    || canonical.is_some_and(|c| super::states::url_names_ref(&v.url, c, number)))
            {
                return Some(v.state);
            }
            // A populated answer naming a DIFFERENT node is not an answer —
            // fall through and try the other entity kind before giving up.
        }
    }
    None
}

/// The canonical `owner/name` of `repo`, resolved once (#14710's rename
/// layer). REST `repos/{owner}/{repo}` follows a rename redirect and answers
/// with the repository's canonical `full_name` (`gh repo view`'s GraphQL
/// lookup does not follow renames), which is exactly the identity evidence
/// needed: binding a node's canonical URL against this name accepts a
/// verified redirect to the same repository while still rejecting the
/// unrelated same-numbered node in a different one.
///
/// `None` when the repository cannot be read — the caller fails closed on
/// that, same as every other unreadable answer here.
fn canonical_nwo(repo: &str, repo_root: &Path) -> Option<String> {
    #[derive(Deserialize)]
    struct FullName {
        #[serde(rename = "full_name")]
        full_name: String,
    }
    let path = format!("repos/{repo}");
    let q: Query<FullName> = gh_read_json(
        "recheck.repo_canonical",
        &["api", &path],
        repo_root,
        false,
        |f: &FullName| f.full_name.is_empty(),
    );
    match q {
        Query::Populated(f) if super::named::is_owner_repo(&f.full_name) => Some(f.full_name),
        _ => None,
    }
}

/// [`fetch_bound_state`] plus the per-run canonical-name cache, shared by
/// every dependency of one `dep-states` call (#14710).
///
/// Park records routinely name several dependencies in one repository; the
/// canonical lookup is only needed for a renamed one (an exact-slug bind
/// short-circuits it), and memoising it per distinct repository keeps the
/// extra reads at one per renamed repo per run, never one per dependency.
pub struct BoundReader<'a> {
    repo_root: &'a Path,
    canonical: std::collections::HashMap<String, Option<String>>,
}

impl<'a> BoundReader<'a> {
    /// A reader bound to `repo_root` (the checkout the subcommand runs in).
    #[must_use]
    pub fn new(repo_root: &'a Path) -> Self {
        Self {
            repo_root,
            canonical: std::collections::HashMap::new(),
        }
    }

    /// The bound state of `number` in `repo`: `Some(state)` only when an
    /// answer provably names that `repo#number` (directly, or through a
    /// verified rename redirect); `None` otherwise — unreadable, mismatched
    /// and wrong-node answers are all "no answer".
    pub fn state(&mut self, number: i64, repo: &str) -> Option<String> {
        // Exact-slug binding first, canonical second: the rename resolution
        // is one extra REST read, paid only when the answer's URL did not
        // name the slug exactly as asked.
        fetch_bound_state(number, repo, self.repo_root, None).or_else(|| {
            let canon = self
                .canonical
                .entry(repo.to_string())
                .or_insert_with(|| canonical_nwo(repo, self.repo_root))
                .clone();
            fetch_bound_state(number, repo, self.repo_root, canon.as_deref())
        })
    }
}

/// An issue's body.
///
/// # Errors
///
/// [`ReadError`] if the issue could not be read.
pub fn fetch_body(issue: i64, repo: Option<&str>, repo_root: &Path) -> Result<String, ReadError> {
    #[derive(Deserialize)]
    struct BodyOnly {
        #[serde(default)]
        body: String,
    }
    let n = issue.to_string();
    let mut args = vec!["issue", "view", &n];
    args.extend(repo_args(repo));
    args.extend(["--json", "body"]);
    let q: Query<BodyOnly> =
        gh_read_json("recheck.issue_body", &args, repo_root, false, |_: &BodyOnly| false);
    match q {
        Query::Populated(b) => Ok(b.body),
        _ => Err(read_failed(&format!("gh issue view {issue}"))),
    }
}

/// The `## Dependencies` checklist entries, with each unchecked one's live
/// state.
///
/// `repo` is the *invoking* repo — where the issue whose checklist this is
/// lives. A checklist item written `owner/repo#N` (#8502) names a different
/// repo, and its state must be read **there**: looking it up in the invoking
/// repo would answer a question nobody asked (a same-numbered issue that
/// happens to exist locally) or fail outright, and a failed read is a hard
/// error here, not a guess. So each entry's own `repo` takes precedence, with
/// the invoking one as the fallback for a bare `#N`.
///
/// # Errors
///
/// [`ReadError`] if the issue, or any unchecked reference, could not be read.
pub fn fetch_named_deps(
    issue: i64,
    repo: Option<&str>,
    repo_root: &Path,
) -> Result<Vec<named::Dep>, ReadError> {
    let body = fetch_body(issue, repo, repo_root)?;
    named::parse_entries(&body)
        .into_iter()
        .map(|entry| {
            if entry.checked {
                // No live lookup: whoever ticked the box said so, and the
                // state is never consulted for a checked item.
                return Ok(entry);
            }
            let target = entry.repo.as_deref().or(repo);
            fetch_state(entry.number, target, repo_root)
                .map(|state| named::Dep {
                    state: Some(state),
                    ..entry.clone()
                })
                .ok_or_else(|| {
                    ReadError(format!(
                        "could not read state for named dependency {} \
                         (neither gh issue view nor gh pr view succeeded)",
                        entry.reference()
                    ))
                })
        })
        .collect()
}

/// An issue's body and comments, in `extract-refs`'s shape.
///
/// # Errors
///
/// [`ReadError`] if the issue could not be read.
pub fn fetch_body_and_comments(
    issue: i64,
    repo: Option<&str>,
    repo_root: &Path,
) -> Result<extract::Input, ReadError> {
    let n = issue.to_string();
    let mut args = vec!["issue", "view", &n];
    args.extend(repo_args(repo));
    args.extend(["--json", "body,comments"]);
    let q: Query<extract::Input> =
        gh_read_json("recheck.body_comments", &args, repo_root, false, |_: &extract::Input| false);
    match q {
        Query::Populated(v) => Ok(v),
        _ => Err(ReadError(format!(
            "gh issue view {issue} failed — cannot compute a fingerprint from a \
             failed read (fail safe: never guess 'no refs' on missing data)"
        ))),
    }
}
