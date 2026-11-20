//! GitHub CI suite selection using the complete checked-out tree (#14823).
//! A missing proof always runs the full suites.

mod proof;
mod tree;

use std::path::PathBuf;

use anyhow::Result;
use clap::ValueEnum;
use serde_json::Value;

use self::proof::Forge;
use self::tree::Tree;

#[derive(Clone, Copy, Debug, ValueEnum)]
#[value(rename_all = "snake_case")]
enum Event {
    PullRequest,
    MergeGroup,
    Push,
}

impl Event {
    fn name(self) -> &'static str {
        match self {
            Self::PullRequest => "pull_request",
            Self::MergeGroup => "merge_group",
            Self::Push => "push",
        }
    }
}

#[derive(clap::Args)]
pub(crate) struct CiDocsOnlyArgs {
    /// Full-history, credential-free checkout of the exact tree the suites test.
    #[arg(long)]
    root: PathBuf,
    /// GitHub event name for the current workflow run.
    #[arg(long)]
    event: Event,
    /// Path to GitHub's event payload (GITHUB_EVENT_PATH).
    #[arg(long)]
    event_path: PathBuf,
    /// Full GitHub event SHA (GITHUB_SHA).
    #[arg(long)]
    head: String,
    /// GitHub repository name (GITHUB_REPOSITORY).
    #[arg(long)]
    repo: String,
    /// ID of the trusted CI workflow whose green pushes serve as the baseline.
    #[arg(long)]
    workflow_id: u64,
}

impl CiDocsOnlyArgs {
    pub(crate) fn run(self) -> Result<()> {
        let forge = Forge::github(&self.root, &self.repo);
        let result = classify(&self, &forge);
        let docs_only = result.is_ok();
        let reason = match &result {
            Ok(_) => "allowed_docs",
            Err(reason) => reason,
        };
        // Fixed fields only. Never emit paths, git stderr, or forge exceptions.
        eprintln!("CI_DOCS_ONLY event={} docs_only={docs_only} reason={reason}", self.event.name());
        println!("docs_only={docs_only}");
        Ok(())
    }
}

fn classify(args: &CiDocsOnlyArgs, forge: &Forge<'_>) -> Result<(), &'static str> {
    let payload = std::fs::read(&args.event_path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
        .ok_or("event_unavailable")?;
    if !valid_repo(&args.repo)
        || field(&payload, "/repository/full_name") != Some(&args.repo)
        || field(&payload, "/repository/default_branch") != Some("main")
        || args.workflow_id == 0
        || !tree::is_sha(&args.head)
    {
        return Err("event_mismatch");
    }
    let tree = Tree::new(&args.root, &args.head).ok_or("history_unavailable")?;
    let (base, protected) = match args.event {
        Event::PullRequest => {
            let target = sha(&payload, "/pull_request/base/sha").ok_or("event_mismatch")?;
            let branch = sha(&payload, "/pull_request/head/sha").ok_or("event_mismatch")?;
            if field(&payload, "/pull_request/base/ref") != Some("main")
                || field(&payload, "/pull_request/base/repo/full_name") != Some(&args.repo)
                || tree
                    .parents(&args.head)
                    .is_none_or(|parents| parents != [target, branch])
            {
                return Err("event_mismatch");
            }
            let base = tree.merge_base(target, branch).ok_or("base_unavailable")?;
            forge
                .green_push(args.workflow_id, Some(&base))
                .ok_or("green_base_unavailable")?;
            (base, target.to_string())
        }
        Event::Push => {
            if field(&payload, "/ref") != Some("refs/heads/main")
                || sha(&payload, "/after") != Some(&args.head)
            {
                return Err("event_mismatch");
            }
            let base = forge
                .green_push(args.workflow_id, None)
                .ok_or("green_base_unavailable")?;
            (base.clone(), base)
        }
        Event::MergeGroup => {
            let group_ref = field(&payload, "/merge_group/head_ref").ok_or("event_mismatch")?;
            let stated_base = sha(&payload, "/merge_group/base_sha").ok_or("event_mismatch")?;
            if field(&payload, "/action") != Some("checks_requested")
                || field(&payload, "/merge_group/base_ref") != Some("refs/heads/main")
                || sha(&payload, "/merge_group/head_sha") != Some(&args.head)
                || !proof::is_queue_ref(group_ref)
            {
                return Err("event_mismatch");
            }
            let parents = tree.parents(&args.head).ok_or("base_unavailable")?;
            let [first, _] = parents.as_slice() else {
                return Err("queue_base_unproven");
            };
            if !tree.first_parent_contains(stated_base, first) {
                return Err("queue_base_unproven");
            }
            let protected = forge
                .queue_basis(&tree, first, args.workflow_id)
                .ok_or("queue_policy_unproven")?;
            ((*first).to_string(), protected)
        }
    };
    if !tree.ancestor(&base, &args.head) {
        return Err("base_not_ancestor");
    }
    let allowlist = tree.allowlist(&protected).ok_or("policy_unavailable")?;
    tree.docs_only(&base, &protected, &args.head, &allowlist)
}

fn field<'a>(object: &'a Value, pointer: &str) -> Option<&'a str> {
    object.pointer(pointer)?.as_str()
}

fn sha<'a>(object: &'a Value, pointer: &str) -> Option<&'a str> {
    field(object, pointer).filter(|s| tree::is_sha(s))
}

fn valid_repo(repo: &str) -> bool {
    fn component(value: &str) -> bool {
        !value.is_empty()
            && !matches!(value, "." | "..")
            && value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"_.-".contains(&byte))
    }
    repo.split_once('/')
        .is_some_and(|(owner, name)| component(owner) && component(name))
}

#[cfg(test)]
mod tests;
