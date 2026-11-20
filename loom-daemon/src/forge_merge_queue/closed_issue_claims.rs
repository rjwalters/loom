//! Remove workflow claims from issues the queue merge actually closed (#14711).

use std::time::Duration;

use serde_json::Value;

use super::forge::LifecycleForge;
use super::gh_lifecycle::GhLifecycleForge;
use crate::forge_repo_facts::full_name_from_repository_url;
use crate::gh_invocation::AccessIntent;
use crate::merge_pr::closed_building::{self, Plan};
use crate::merge_pr::closed_by_merge::{self, Closer, Evidence, Facts, Verdict};
use crate::merge_pr::partial_reset::IssueView;

// GitHub can confirm the PR before closing its issues. Give a positively
// linked, still-open issue one fresh read without leaving the merge pending.
const CLOSE_RETRY_DELAY: Duration = Duration::from_secs(5);

const CLOSE_TARGETS_QUERY: &str = "query($owner:String!,$name:String!,$pr:Int!) {
    repository(owner:$owner,name:$name) {
        nameWithOwner
        pullRequest(number:$pr) {
            number mergedAt mergeCommit { oid } commits(last:100) { nodes { commit { oid } } }
            closingIssuesReferences(first:100) {
                pageInfo { hasNextPage }
                nodes {
                    number repository { nameWithOwner } state closedAt
                    timelineItems(last:1,itemTypes:CLOSED_EVENT) {
                        nodes { ... on ClosedEvent { closer {
                            __typename
                            ... on PullRequest { number repository { nameWithOwner } }
                            ... on Commit { oid }
                        } } }
                    }
                }
            }
        }
    }
}";

#[derive(Debug, PartialEq, Eq)]
struct ClosedTarget {
    number: u32,
    closed_at: String,
}

#[derive(Debug, Default)]
struct CloseTargets {
    closed: Vec<ClosedTarget>,
    has_open: bool,
}

fn same_repo(value: Option<&Value>, nwo: &str) -> bool {
    value
        .and_then(Value::as_str)
        .is_some_and(|name| name.eq_ignore_ascii_case(nwo))
}

fn close_targets(nwo: &str, pr_number: u32, doc: &Value) -> Result<CloseTargets, String> {
    let repo = &doc["data"]["repository"];
    let pr = &repo["pullRequest"];
    let refs = &pr["closingIssuesReferences"];
    if doc.get("errors").is_some()
        || !same_repo(repo.get("nameWithOwner"), nwo)
        || pr["number"].as_u64() != Some(u64::from(pr_number))
        || pr["mergedAt"].as_str().is_none()
        || refs["pageInfo"]["hasNextPage"].as_bool() != Some(false)
    {
        return Err("unverified or incomplete closing-issue references".into());
    }
    let issues = refs["nodes"].as_array().ok_or("missing closing issues")?;
    let mut targets = CloseTargets::default();
    for issue in issues {
        let Some(number) = issue["number"].as_u64().and_then(|n| u32::try_from(n).ok()) else {
            continue;
        };
        if !same_repo(issue.pointer("/repository/nameWithOwner"), nwo) {
            continue;
        }
        if issue["state"].as_str() == Some("OPEN") {
            targets.has_open = true;
            continue;
        }

        let facts = Facts::from_graphql_pair(pr, issue);
        if matches!(&facts.closer, Closer::PullRequest(_))
            && !same_repo(
                issue
                    .pointer("/timelineItems/nodes")
                    .and_then(Value::as_array)
                    .and_then(|nodes| nodes.last())
                    .and_then(|event| event.pointer("/closer/repository/nameWithOwner")),
                nwo,
            )
        {
            continue;
        }
        if matches!(
            closed_by_merge::decide(u64::from(number), u64::from(pr_number), &facts),
            Verdict::ThisMerge(Evidence::CloserIsPr | Evidence::CloserIsPrCommit)
        ) {
            if let Some(closed_at) = facts.closed_at {
                targets.closed.push(ClosedTarget { number, closed_at });
            }
        }
    }
    Ok(targets)
}

fn after_close(
    mut read: impl FnMut() -> Result<CloseTargets, String>,
    wait: impl FnOnce(),
) -> Result<Vec<ClosedTarget>, String> {
    let first = read()?;
    if !first.has_open {
        return Ok(first.closed);
    }
    wait();
    // Use only the second snapshot after waiting: previously closed issues
    // may have reopened or acquired a different closer in the meantime.
    Ok(read()?.closed)
}

fn labels_to_strip(nwo: &str, target: &ClosedTarget, body: &str) -> &'static [&'static str] {
    let Ok(doc) = serde_json::from_str::<Value>(body) else {
        return &[];
    };
    if doc["number"].as_u64() != Some(u64::from(target.number))
        || doc["closed_at"].as_str() != Some(target.closed_at.as_str())
        || !doc["repository_url"]
            .as_str()
            .and_then(full_name_from_repository_url)
            .is_some_and(|name| name.eq_ignore_ascii_case(nwo))
    {
        return &[];
    }
    match closed_building::plan(&IssueView::from_json(body)) {
        Plan::Strip => &["loom:building"],
        Plan::StripReady => &["loom:issue"],
        Plan::StripBoth => &["loom:building", "loom:issue"],
        Plan::Skip(_) => &[],
    }
}

impl GhLifecycleForge {
    pub(super) fn strip_confirmed_issue_claims(&self, pr: u32) -> Result<(), String> {
        let nwo = self.nwo();
        let targets = after_close(
            || {
                let text = self.ok(
                    "merge_queue.closing_issues",
                    AccessIntent::Read,
                    &[
                        "api".into(),
                        "graphql".into(),
                        "-f".into(),
                        format!("query={CLOSE_TARGETS_QUERY}"),
                        "-f".into(),
                        format!("owner={}", self.owner()),
                        "-f".into(),
                        format!("name={}", self.repo_name()),
                        "-F".into(),
                        format!("pr={pr}"),
                    ],
                )?;
                let doc = serde_json::from_str(&text).map_err(|e| e.to_string())?;
                close_targets(&nwo, pr, &doc)
            },
            || std::thread::sleep(CLOSE_RETRY_DELAY),
        )?;
        for target in targets {
            let num = target.number;
            let result = self
                .ok(
                    "merge_queue.closed_issue",
                    AccessIntent::Read,
                    &["api".into(), format!("repos/{nwo}/issues/{num}")],
                )
                .and_then(|body| {
                    let labels = labels_to_strip(&nwo, &target, &body);
                    if labels.is_empty() {
                        return Ok(());
                    }
                    self.edit_labels(num, &[], labels)
                });
            if let Err(e) = result {
                log::warn!("merge-queue: PR #{pr} could not clean closed issue #{num}: {e}");
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
