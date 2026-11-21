//! Bounded, read-only inventory of PRs affected by the operator-link rule.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use anyhow::{ensure, Context, Result};
use serde::Serialize;

use crate::comment_trust::TrustPolicy;
use crate::fleet_store::{compiled, roster};
use crate::forge_cmd::{parse_host, parse_nwo_from_remote_url};
use crate::forge_listing::list_issues_cached_all_as;
use crate::forge_pull_listing::list_open_pulls_cached_as;
use crate::work_finder::step_item::{
    branch_metadata_unverified, is_review_pr, linked_operator_hold, LinkedOperatorHold,
};
use crate::work_finder::{OPERATOR_HOLD_NAMES, PARK_LABELS};
use crate::worktree_ops::linked_pr_listing::{link_facts, linked_issues};

const CALLER: &str = "forge.operator_holds";
const MAX_REPOS: usize = 128;
const MAX_PR_PAGES: usize = 4;

#[derive(clap::Args)]
pub(crate) struct Args {
    /// Path to the compiled fleet roster whose positive-weight GitHub repos
    /// to read. Maintained-only and firewall repos are outside this inventory.
    #[arg(long)]
    fleet_json: PathBuf,
}

#[derive(Debug, Default, Serialize)]
struct RepoResult {
    repo: String,
    prs_examined: usize,
    non_closing_only: Vec<(u32, u32)>,
    held_via_join: Vec<(u32, u32)>,
    unverified_prs: Vec<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    incomplete: Option<&'static str>,
}

pub(crate) fn run(args: Args) -> Result<()> {
    let text = std::fs::read_to_string(&args.fleet_json)
        .with_context(|| format!("read {}", args.fleet_json.display()))?;
    let doc = compiled::parse(&text)?;
    let home = dirs::home_dir().context("cannot resolve fleet checkout root")?;
    let roster = roster::from_compiled(doc.roster(), &home)?;
    let targets: Vec<_> = roster
        .records
        .iter()
        .filter(|r| r.fleet && !r.firewall && !r.maintain_only && r.weight.unwrap_or(0) > 0)
        .collect();
    ensure!(targets.len() <= MAX_REPOS, "roster exceeds the {MAX_REPOS}-repo audit bound");

    let cwd = std::env::current_dir()?;
    let gh = PathBuf::from(crate::gh_invocation::resolver::gh_bin());
    let _discovery = crate::gh_invocation::purpose::discovery();
    let results: Vec<_> = targets
        .into_iter()
        .map(|record| {
            let slug = record.remote.as_deref().and_then(|remote| {
                (parse_host(remote).as_deref() == Some("github.com"))
                    .then(|| parse_nwo_from_remote_url(remote))
                    .flatten()
            });
            let Some(slug) = slug else {
                return RepoResult {
                    repo: record.name.clone(),
                    incomplete: Some("unsupported_remote"),
                    ..RepoResult::default()
                };
            };
            read_repo(&slug, &roster.root.join(&record.dir), &cwd, &gh)
        })
        .collect();
    let complete = results
        .iter()
        .all(|r| r.incomplete.is_none() && r.unverified_prs.is_empty());
    println!(
        "{}",
        serde_json::to_string_pretty(&serde_json::json!({
            "complete": complete,
            "repos_examined": results.len(),
            "non_closing_only": results.iter().map(|r| r.non_closing_only.len()).sum::<usize>(),
            "held_via_join": results.iter().map(|r| r.held_via_join.len()).sum::<usize>(),
            "repos": results,
        }))?
    );
    Ok(())
}

fn read_repo(repo: &str, workspace: &Path, cwd: &Path, gh: &Path) -> RepoResult {
    let mut result = RepoResult {
        repo: repo.to_string(),
        ..RepoResult::default()
    };
    let Ok(pulls) = list_open_pulls_cached_as(CALLER, gh, Some(cwd), Some(repo), MAX_PR_PAGES)
    else {
        result.incomplete = Some("open_prs_unavailable");
        return result;
    };
    result.prs_examined = pulls.len();
    let policy = TrustPolicy::for_root(if workspace.is_dir() { workspace } else { cwd });
    let candidates: Vec<_> = pulls
        .iter()
        .filter_map(|pull| {
            if !is_review_pr(&pull.labels)
                || pull
                    .labels
                    .iter()
                    .any(|l| PARK_LABELS.contains(&l.as_str()))
            {
                return None;
            }
            let (same_repo, untrusted) = link_facts(pull, repo, &policy);
            if branch_metadata_unverified(pull) {
                result.unverified_prs.push(pull.number);
            }
            let links =
                linked_issues(pull.body.as_deref(), pull.head_ref.as_deref(), same_repo, untrusted);
            (!links.is_empty()).then_some((pull, links))
        })
        .collect();
    if candidates.is_empty() {
        return result;
    }
    let mut held_issues = BTreeSet::new();
    for label in OPERATOR_HOLD_NAMES.iter() {
        let Ok(issues) =
            list_issues_cached_all_as(CALLER, gh, Some(cwd), Some(repo), label, "open")
        else {
            result.incomplete = Some("operator_issues_unavailable");
            return result;
        };
        held_issues.extend(
            issues
                .into_iter()
                .filter(|i| !i.is_pull_request)
                .map(|i| i.number),
        );
    }
    for (pull, links) in candidates {
        match linked_operator_hold(&links, branch_metadata_unverified(pull), |n| {
            held_issues.contains(&n)
        }) {
            Some(LinkedOperatorHold::JoinedIssue(issue)) => {
                result.held_via_join.push((pull.number, issue))
            }
            Some(LinkedOperatorHold::Mention(issue)) => {
                result.non_closing_only.push((pull.number, issue))
            }
            Some(LinkedOperatorHold::Unverified) | None => {}
        }
    }
    result
}
