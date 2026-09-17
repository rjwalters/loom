//! Forge reads and writes for dependency classification (epic #7810, PR 3).
//!
//! The I/O half, kept apart from every decision so the decisions stay testable
//! without a forge. Everything here goes through [`crate::cmd_out`] (PR 2), so
//! each call is bounded and each outcome classified rather than collapsed.

use crate::cmd_out::Query;
use crate::script_helpers::{gh_query, run_gh};
use serde::Deserialize;
use std::path::Path;

/// `gh issue view --json body,labels,comments`.
#[derive(Debug, Default, Deserialize)]
pub struct IssueView {
    #[serde(default)]
    pub body: String,
    #[serde(default)]
    pub labels: Vec<Label>,
    #[serde(default)]
    pub comments: Vec<Comment>,
}

#[derive(Debug, Deserialize)]
pub struct Label {
    pub name: String,
}

#[derive(Debug, Deserialize)]
pub struct Comment {
    #[serde(default)]
    pub body: String,
}

impl IssueView {
    /// Label names, in forge order.
    #[must_use]
    pub fn label_names(&self) -> Vec<String> {
        self.labels.iter().map(|l| l.name.clone()).collect()
    }

    /// All comment bodies newline-joined, as the shell built the haystack it
    /// searched for markers.
    #[must_use]
    pub fn comments_joined(&self) -> String {
        self.comments
            .iter()
            .map(|c| c.body.as_str())
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// The **last** comment containing `needle`, or empty.
    ///
    /// Last, not first: an issue accumulates verdicts, and the current decision
    /// is about the most recent one. Taking the first would re-litigate a
    /// finding that a later pass already superseded.
    #[must_use]
    pub fn last_comment_containing(&self, needle: &str) -> String {
        // `rfind`, i.e. searching from the end: LAST, not first.
        self.comments
            .iter()
            .rfind(|c| c.body.contains(needle))
            .map(|c| c.body.clone())
            .unwrap_or_default()
    }
}

/// Read an issue, requesting exactly `fields`.
///
/// `None` when it could not be read at all — the caller exits 2, matching the
/// shell's `err … exit 2`. A missing issue and an unreachable forge are the
/// same to this caller: neither yields a body to reason about.
///
/// `fields` is per-mode rather than fixed because the shell's cost note is
/// per-mode: `--check-defer` asks for `body,comments` and the un-escalate modes
/// add `labels`. One call either way, but the response is what it needs to be.
#[must_use]
pub fn read_issue(
    issue: i64,
    repo: &str,
    repo_root: &Path,
    use_cache: bool,
    fields: &str,
) -> Option<IssueView> {
    read_issue_in(&issue.to_string(), repo, repo_root, use_cache, fields)
}

/// [`read_issue`] for a number already in string form — the shape the cycle
/// walk and the cycle report have, where nodes are `owner/repo#N` text.
#[must_use]
pub fn read_issue_in(
    num: &str,
    repo: &str,
    repo_root: &Path,
    use_cache: bool,
    fields: &str,
) -> Option<IssueView> {
    let q: Query<IssueView> = gh_query(
        &["issue", "view", num, "--repo", repo, "--json", fields],
        repo_root,
        use_cache,
        // Never "empty": an issue with no body, no labels and no comments is
        // still a real issue, and treating it as absent would exit 2 on a
        // perfectly readable one.
        |_: &IssueView| false,
    );
    match q {
        Query::Populated(v) => Some(v),
        _ => None,
    }
}

/// Fetch one node for the cycle walk: its state and body.
#[must_use]
pub fn fetch_node(node: &str, repo_root: &Path, use_cache: bool) -> Option<super::cycle::Node> {
    #[derive(Deserialize)]
    struct StateBody {
        #[serde(default)]
        state: String,
        #[serde(default)]
        body: String,
    }
    let (repo, num) = node.rsplit_once('#')?;
    let q: Query<StateBody> = gh_query(
        &["issue", "view", num, "--repo", repo, "--json", "state,body"],
        repo_root,
        use_cache,
        |_: &StateBody| false,
    );
    match q {
        Query::Populated(v) => Some(super::cycle::Node {
            state: v.state,
            body: v.body,
        }),
        _ => None,
    }
}

/// The forge writes an apply performs, bound to one issue.
pub struct GhWriter<'a> {
    pub issue: i64,
    pub repo: &'a str,
    pub repo_root: &'a Path,
}

impl GhWriter<'_> {
    /// Add labels — the cycle report's own write, which no un-escalation makes,
    /// so it lives here rather than on the [`super::apply::Writer`] trait.
    pub fn add_labels(&mut self, labels: &str) -> crate::cmd_out::CmdOutcome {
        let n = self.issue.to_string();
        run_gh(
            &[
                "issue",
                "edit",
                &n,
                "--repo",
                self.repo,
                "--add-label",
                labels,
            ],
            self.repo_root,
            false,
        )
    }
}

impl super::apply::Writer for GhWriter<'_> {
    fn remove_label(&mut self, label: &str) -> crate::cmd_out::CmdOutcome {
        let n = self.issue.to_string();
        // Writes never use the read cache: a cached response to a mutation is
        // meaningless, and `gh-cached` is a read-side wrapper.
        run_gh(
            &[
                "issue",
                "edit",
                &n,
                "--repo",
                self.repo,
                "--remove-label",
                label,
            ],
            self.repo_root,
            false,
        )
    }

    fn post_comment(&mut self, body: &str) -> crate::cmd_out::CmdOutcome {
        let n = self.issue.to_string();
        run_gh(
            &["issue", "comment", &n, "--repo", self.repo, "--body", body],
            self.repo_root,
            false,
        )
    }

    fn edit_body(&mut self, body: &str) -> crate::cmd_out::CmdOutcome {
        let n = self.issue.to_string();
        run_gh(
            &["issue", "edit", &n, "--repo", self.repo, "--body", body],
            self.repo_root,
            false,
        )
    }
}

#[cfg(test)]
mod tests;
