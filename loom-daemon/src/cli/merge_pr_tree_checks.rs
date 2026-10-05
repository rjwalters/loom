//! `loom-daemon merge-pr tree-checks` (#10026): the repo-configured pre-merge
//! merge-tree gate. Logic and contract: [`loom_daemon::merge_pr::tree_checks`].
//!
//! | outcome | stdout | exit |
//! |---|---|---|
//! | no checks declared / all passed | `LOOM-TREE-CHECKS-CLEAN` | 0 |
//! | a check failed, `--allow-red-tree` | `LOOM-TREE-CHECKS-BYPASSED …` (audit comment posted) | 0 |
//! | a check failed | the refusal with the check's real output (comment posted) | 1 |
//! | could not build the tree / run a check | the reason | 2 |
//!
//! `--dry-run` never posts a comment. Exit 2 must refuse the merge: a guard
//! that cannot run is not a pass. Callers only invoke this when the config
//! declares checks, so an older binary never affects repos that do not opt in.

use anyhow::Result;
use loom_daemon::merge_pr::tree_checks::{
    bypass_comment, evaluate, failure_comment, refusal, Outcome, BYPASSED, CLEAN,
};

#[derive(clap::Args)]
pub(crate) struct TreeChecksArgs {
    /// The PR number.
    #[arg(long, value_name = "N")]
    pr: String,

    /// The repository as owner/repo (comment target).
    #[arg(long, value_name = "OWNER/REPO")]
    repo: String,

    /// The PR head SHA being merged.
    #[arg(long, value_name = "SHA")]
    head_sha: String,

    /// The branch the merge lands on.
    #[arg(long, value_name = "REF")]
    base_ref: String,

    /// Git remote to fetch from.
    #[arg(long, value_name = "NAME", default_value = "origin")]
    remote: String,

    /// Config file (default: `<repo root>/.loom/config.json`).
    #[arg(long, value_name = "PATH")]
    config: Option<std::path::PathBuf>,

    /// Warn and record an audit comment instead of refusing a red tree.
    #[arg(long)]
    allow_red_tree: bool,

    /// Report only; post no comments.
    #[arg(long)]
    dry_run: bool,
}

fn post_comment(repo: &str, pr: &str, body: &str) {
    use loom_daemon::gh_invocation::{
        AccessIntent, GhCompletion, GhInvocation, GhTarget, Operation,
    };
    use loom_daemon::proc_exec::Completion;
    // `-f` sends `body` as a string field (never read as a file, unlike `-F`),
    // so the comment travels as an argument rather than on stdin.
    let ok = GhTarget::repo(repo).ok().and_then(|target| {
        GhInvocation::new(
            Operation::new("pr.tree_check_comment"),
            AccessIntent::Write,
            target,
            std::time::Duration::from_secs(60),
        )
        .args([
            "api",
            &format!("repos/{repo}/issues/{pr}/comments"),
            "-f",
            &format!("body={body}"),
        ])
        .execute()
        .ok()
    });
    let success = matches!(
        ok,
        Some(GhCompletion::Captured(Completion::Exited(ref o))) if o.status.success()
    );
    if !success {
        eprintln!("Warning: could not post the tree-check comment on PR #{pr}");
    }
}

impl TreeChecksArgs {
    pub(crate) fn run(self) -> Result<()> {
        let root = loom_daemon::repo_root::find_repo_root_from_cwd()
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_default();
        let cfg_path = self
            .config
            .clone()
            .unwrap_or_else(|| root.join(".loom/config.json"));
        let json = std::fs::read_to_string(&cfg_path).ok();
        match evaluate(
            &root,
            json.as_deref(),
            &self.remote,
            &self.pr,
            &self.base_ref,
            &self.head_sha,
        ) {
            Outcome::Clean => println!("{CLEAN}"),
            Outcome::Failed { check, output } if self.allow_red_tree => {
                println!("{BYPASSED} PR #{}: tree check `{check}` failed; proceeding under --allow-red-tree.\n{output}", self.pr);
                if !self.dry_run {
                    post_comment(&self.repo, &self.pr, &bypass_comment(&self.head_sha, &check));
                }
            }
            Outcome::Failed { check, output } => {
                println!("{}", refusal(&self.pr, &check, &output));
                if !self.dry_run {
                    post_comment(&self.repo, &self.pr, &failure_comment(&check, &output));
                }
                std::process::exit(1);
            }
            Outcome::Unknown(why) => {
                println!("tree-checks could not run: {why}");
                std::process::exit(2);
            }
        }
        Ok(())
    }
}
