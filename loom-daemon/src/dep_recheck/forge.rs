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
use crate::script_helpers::gh_query;
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
    /// `gh pr view --json labels` returns label OBJECTS. Normalised to names
    /// here, once, so every consumer — and every `--stdin` fixture — sees the
    /// one shape.
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

    let n = issue.to_string();
    let mut args = vec!["issue", "view", &n];
    args.extend(repo_args(repo));
    args.extend(["--json", "closedByPullRequestsReferences"]);
    let q: Query<IssueView> = gh_query(&args, repo_root, false, |_: &IssueView| false);
    let Query::Populated(view) = q else {
        return Err(read_failed(&format!("gh issue view {issue}")));
    };

    let mut out = Vec::with_capacity(view.closed_by.len());
    for c in &view.closed_by {
        let pn = c.number.to_string();
        let mut args = vec!["pr", "view", &pn];
        args.extend(repo_args(repo));
        args.extend(["--json", "number,state,labels,mergeable,mergeStateStatus"]);
        let q: Query<PrView> = gh_query(&args, repo_root, false, |_: &PrView| false);
        let Query::Populated(p) = q else {
            return Err(read_failed(&format!("gh pr view {}", c.number)));
        };
        out.push(recheck::Pr {
            number: p.number,
            state: p.state,
            labels: p.labels.into_iter().map(|l| l.name).collect(),
            mergeable: p.mergeable,
            merge_state_status: p.merge_state_status,
        });
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
            gh_query(&args, repo_root, false, |s: &StateField| s.state.is_empty());
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
    let q: Query<BodyOnly> = gh_query(&args, repo_root, false, |_: &BodyOnly| false);
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
    let q: Query<extract::Input> = gh_query(&args, repo_root, false, |_: &extract::Input| false);
    match q {
        Query::Populated(v) => Ok(v),
        _ => Err(ReadError(format!(
            "gh issue view {issue} failed — cannot compute a fingerprint from a \
             failed read (fail safe: never guess 'no refs' on missing data)"
        ))),
    }
}
