//! `loom-daemon merge-group-ci …` — combined-tree CI qualification for the
//! merge queue (#10257). Library: `loom_daemon::merge_group_ci`.
//!
//! | Verb | Exit 0 | Exit 1 | Exit 2 |
//! |---|---|---|---|
//! | `audit` | qualified | a finding | could not run |
//! | `eligibility` | eligible | a prerequisite failed | could not run |
//!
//! Both verbs are read-only: `audit` reads local files; `eligibility` adds
//! `GET`-only forge reads (or `--facts`). Neither changes anything.

use std::path::PathBuf;

use anyhow::Result;
use loom_daemon::merge_group_ci::{self, eligibility, RepoFacts};

#[derive(clap::Subcommand)]
pub(crate) enum MergeGroupCiCommand {
    /// Report which relied-on suites do NOT validate the merge-group commit
    /// (missing trigger, PR-only `if:`, path-filter skip, non-merge-group
    /// checkout, cancelling/shared concurrency, missing required suite).
    Audit(AuditArgs),
    /// Decide whether a repository may pilot queue mode: organization owner,
    /// push permission, an active `merge_queue` rule, required checks, and a
    /// clean workflow audit against those checks. Read-only.
    Eligibility(EligibilityArgs),
}

impl MergeGroupCiCommand {
    pub(crate) fn run(self) -> Result<()> {
        match self {
            MergeGroupCiCommand::Audit(a) => a.run(),
            MergeGroupCiCommand::Eligibility(a) => a.run(),
        }
    }
}

#[derive(clap::Args)]
pub(crate) struct AuditArgs {
    /// Repository checkout to audit. Defaults to the current directory.
    #[arg(long, value_name = "PATH")]
    root: Option<PathBuf>,

    /// Audit only this workflow (file name or repo-relative path). Repeatable.
    #[arg(long, value_name = "FILE")]
    workflow: Vec<String>,

    /// A required status-check context to verify. Repeatable. Defaults to
    /// `.loom/config.json` `branchProtection.requiredStatusChecks`.
    #[arg(long, value_name = "CONTEXT")]
    required: Vec<String>,

    /// Also list workflows and jobs that are not relied on.
    #[arg(long, short)]
    verbose: bool,

    /// Emit the report as JSON.
    #[arg(long)]
    json: bool,
}

fn root_or_cwd(root: Option<PathBuf>) -> PathBuf {
    root.unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
}

fn could_not_run(why: &str) -> ! {
    eprintln!("{} could not run: {why} (nothing was changed)", merge_group_ci::PREFIX);
    std::process::exit(2);
}

impl AuditArgs {
    fn run(self) -> Result<()> {
        let root = root_or_cwd(self.root);
        let (workflows, bad) =
            merge_group_ci::load(&root, &self.workflow).unwrap_or_else(|e| could_not_run(&e));
        let required = if self.required.is_empty() {
            merge_group_ci::configured_required(&root)
        } else {
            self.required
        };
        let report = merge_group_ci::audit::audit(&workflows, &bad, &required);
        if self.json {
            println!("{}", serde_json::to_string_pretty(&report)?);
        } else {
            print!("{}", merge_group_ci::render_audit(&report, self.verbose));
        }
        std::process::exit(i32::from(!report.qualified()));
    }
}

#[derive(clap::Args)]
pub(crate) struct EligibilityArgs {
    /// Checkout of the candidate repository (its workflows are audited
    /// against the branch's required checks). Defaults to the current
    /// directory.
    #[arg(long, value_name = "PATH")]
    root: Option<PathBuf>,

    /// The candidate repository. Defaults to the checkout's GitHub remote.
    #[arg(long, value_name = "OWNER/NAME", conflicts_with = "facts")]
    repo: Option<String>,

    /// The branch whose rules are read. Defaults to the default branch.
    #[arg(long, value_name = "BRANCH", conflicts_with = "facts")]
    branch: Option<String>,

    /// Read the repository facts from a JSON file instead of the forge
    /// (offline / test use). Shape: `{"repository","owner_type","branch",
    /// "can_push","rules":[{"type",...,"parameters"}]}`; a missing or null
    /// field means "unknown" and fails closed.
    #[arg(long, value_name = "FILE")]
    facts: Option<PathBuf>,

    /// Emit the verdict as JSON.
    #[arg(long)]
    json: bool,
}

impl EligibilityArgs {
    fn run(self) -> Result<()> {
        let root = root_or_cwd(self.root);
        let facts: RepoFacts = if let Some(path) = &self.facts {
            let text = std::fs::read_to_string(path)
                .unwrap_or_else(|e| could_not_run(&format!("reading {}: {e}", path.display())));
            serde_json::from_str(&text)
                .unwrap_or_else(|e| could_not_run(&format!("parsing {}: {e}", path.display())))
        } else {
            eligibility::probe_ambient(self.repo.as_deref(), self.branch.as_deref())
                .unwrap_or_else(|e| could_not_run(&e))
        };
        let (workflows, bad) =
            merge_group_ci::load(&root, &[]).unwrap_or_else(|e| could_not_run(&e));
        let verdict = eligibility::evaluate(&facts, &workflows, &bad);
        if self.json {
            println!("{}", serde_json::to_string_pretty(&verdict)?);
        } else {
            print!("{}", merge_group_ci::render_eligibility(&verdict));
        }
        std::process::exit(i32::from(!verdict.eligible));
    }
}
