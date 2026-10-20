//! `loom-daemon orphan-replace` — the git half of Doctor's orphan-stack
//! replacement recipe (#12767). The rules live in [`loom_daemon::orphan_replace`];
//! this file only parses arguments and prints.
//!
//! Exit codes: 0 done (JSON on stdout), 1 refused (`REFUSED <TOKEN>: <why>` on
//! stdout, nothing pushed — post the reason and leave the flag), 2 transient
//! (nothing pushed, a retry may work).

use std::io::Read;
use std::path::PathBuf;

use anyhow::Result;
use clap::{Args, Subcommand};
use loom_daemon::orphan_replace::{self as replace, CloseDecision, Failure, Orphan};

#[derive(Args, Debug)]
pub(crate) struct OrphanArgs {
    /// The closed-unmerged parent PR.
    #[arg(long, value_name = "N")]
    parent_pr: u32,
    /// The parent PR's last head (full sha), from its PR record.
    #[arg(long, value_name = "SHA")]
    parent_head: String,
    /// The flagged child PR.
    #[arg(long, value_name = "N")]
    child_pr: u32,
    /// The child PR's current head (full sha).
    #[arg(long, value_name = "SHA")]
    child_head: String,
    /// Any working tree of the repository. Defaults to the current directory.
    #[arg(long, value_name = "DIR")]
    repo_dir: Option<PathBuf>,
}

impl OrphanArgs {
    fn orphan(&self) -> Orphan<'_> {
        Orphan {
            parent_pr: self.parent_pr,
            parent_head: &self.parent_head,
            child_pr: self.child_pr,
            child_head: &self.child_head,
        }
    }

    fn repo(&self) -> Result<PathBuf> {
        Ok(match &self.repo_dir {
            Some(d) => d.clone(),
            None => std::env::current_dir()?,
        })
    }
}

#[derive(Subcommand, Debug)]
pub(crate) enum OrphanReplaceCommand {
    /// Establish the child-only commit range (`<parent head>..<child head>`),
    /// or refuse. Changes nothing but fetches the two PR heads.
    Verify(OrphanArgs),
    /// `verify`, then replay the range onto the fetched default branch in a
    /// scratch worktree and push it as the NEW branch `orphan-replace/pr-<child>`
    /// (never forced; the original branch is not touched). A retry reuses the
    /// branch when it already holds exactly the range.
    Create {
        #[command(flatten)]
        orphan: OrphanArgs,
        /// The default branch NAME.
        #[arg(long, value_name = "BRANCH", default_value = "main")]
        default_branch: String,
    },
    /// May the original PR be closed? Prints `CLOSE` (exit 0) or the reason it
    /// may not (exit 1). `--replacement` is the replacement PR number (omit if
    /// none exists), `--links-posted` that both link comments exist, and each
    /// `--descendant` an open PR whose base is the original's head branch.
    CloseCheck {
        #[arg(long, value_name = "N")]
        replacement: Option<u32>,
        #[arg(long)]
        links_posted: bool,
        #[arg(long, value_name = "N")]
        descendant: Vec<u32>,
    },
    /// Read trusted comment bodies on stdin (separated by a line of `----`) and
    /// print the replacement PR number named by the newest
    /// `<!-- loom:orphan-stack replaced by=N -->` marker, or nothing (exit 1).
    ReplacedBy,
}

fn report(f: &Failure) -> i32 {
    match f {
        Failure::Refused(r) => {
            println!("REFUSED {}: {}", r.token(), r.message());
            1
        }
        Failure::Transient(t) => {
            eprintln!("TRANSIENT: {}", t.0);
            2
        }
    }
}

impl OrphanReplaceCommand {
    pub(crate) fn run(self) -> Result<()> {
        match self {
            Self::Verify(a) => match replace::verify(&a.repo()?, &a.orphan()) {
                Ok(b) => {
                    println!("{}", serde_json::json!({ "commits": b.commits }));
                    Ok(())
                }
                Err(f) => std::process::exit(report(&f)),
            },
            Self::Create {
                orphan,
                default_branch,
            } => match replace::create(&orphan.repo()?, &default_branch, &orphan.orphan()) {
                Ok(r) => {
                    println!(
                        "{}",
                        serde_json::json!({
                            "branch": r.branch,
                            "head": r.head,
                            "onto": r.onto,
                            "commits": r.commits,
                            "reused": r.reused,
                            "marker": "see `replaced-by`; post `<!-- loom:orphan-stack replaced by=<PR> -->` once the PR exists",
                        })
                    );
                    Ok(())
                }
                Err(f) => std::process::exit(report(&f)),
            },
            Self::CloseCheck {
                replacement,
                links_posted,
                descendant,
            } => match replace::close_decision(replacement, links_posted, &descendant) {
                CloseDecision::Close => {
                    println!("CLOSE");
                    Ok(())
                }
                CloseDecision::NoReplacement => {
                    println!("NO_REPLACEMENT: no replacement PR exists yet");
                    std::process::exit(1)
                }
                CloseDecision::LinksMissing => {
                    println!("LINKS_MISSING: both link comments must be posted first");
                    std::process::exit(1)
                }
                CloseDecision::BlockedByDescendants(d) => {
                    let list: Vec<String> = d.iter().map(|n| format!("#{n}")).collect();
                    println!(
                        "BLOCKED_BY_DESCENDANTS: open PRs are stacked on this one ({}); leave it open and flagged",
                        list.join(", ")
                    );
                    std::process::exit(1)
                }
            },
            Self::ReplacedBy => {
                let mut input = String::new();
                std::io::stdin().read_to_string(&mut input)?;
                let bodies: Vec<String> = input.split("\n----\n").map(str::to_string).collect();
                match replace::replaced_by(&bodies) {
                    Some(n) => {
                        println!("{n}");
                        Ok(())
                    }
                    None => std::process::exit(1),
                }
            }
        }
    }
}
