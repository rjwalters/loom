//! `loom-daemon bot-pr` — Champion's trusted-bot dependency-PR classifier
//! (#4765), invoked from `champion-bot-pr.md` via the
//! `.loom/scripts/champion-bot-pr.sh` stub.
//!
//! # Why the PR body and diff arrive on a file / stdin
//!
//! Both are **untrusted external content** (`defaults/docs/
//! untrusted-external-content.md`) and a lockfile diff routinely runs to tens
//! of thousands of lines. Passing either as an argv element risks `E2BIG` and
//! puts attacker-controlled bytes through another layer of shell quoting.
//! `merge-pr-refs` takes its body on stdin for exactly this reason; here stdin
//! carries the diff, because the diff is the larger of the two.
//!
//! # Output contract
//!
//! `KEY='value'` lines on stdout, POSIX-single-quoted so `eval` is safe on any
//! title. Exit `0` when the PR qualifies for the waivers, `1` when it does not
//! — a verdict, not an error, matching `classify-dependency-block`'s
//! convention — and `2` for a usage or I/O failure, so "the classifier could
//! not run" is never mistaken for "this PR does not qualify".

use std::collections::HashMap;
use std::io::Read;
use std::path::PathBuf;

use anyhow::{Context, Result};

use loom_daemon::bot_pr::{
    classify::{self, ClassifyInput},
    config, diff, render,
};

#[derive(clap::Subcommand)]
pub(crate) enum BotPrCommand {
    /// Print the resolved `champion` bot-PR config block, `eval`-ready. Exit 0
    /// when the feature is enabled, 1 when it is not, so a caller can gate the
    /// whole pass on one cheap call with no forge reads.
    Config {
        /// Checkout to resolve config for. Defaults to the current directory's
        /// repository root.
        #[arg(long, value_name = "PATH")]
        repo_root: Option<PathBuf>,
    },

    /// Classify one PR. The unified diff (`gh pr diff <N>`) arrives on stdin.
    ///
    /// Exit 0 qualifies, 1 does not (a verdict), 2 usage/I-O error.
    Classify {
        /// `author.login` exactly as `gh pr view --json author` reported it.
        /// Never a branch name or title — those are forgeable by a human push.
        #[arg(long, value_name = "LOGIN")]
        author: String,

        /// The PR title, for the `dependabotMaxSemver` guard.
        #[arg(long, default_value = "", value_name = "TEXT")]
        title: String,

        /// File holding the PR body, for a grouped PR whose per-member bumps
        /// are listed there rather than in the title.
        #[arg(long, value_name = "PATH")]
        body_file: Option<PathBuf>,

        /// File holding the authoritative changed-file list, one path per
        /// line, from `gh api repos/O/R/pulls/N/files --paginate --jq
        /// '.[].filename'`. Falls back to the paths the diff itself names —
        /// but pass it: `gh pr view --json files` truncates at 100 with no
        /// error (#4613), and so can a very large diff.
        #[arg(long, value_name = "PATH")]
        files_from: Option<PathBuf>,

        /// Checkout to resolve config for.
        #[arg(long, value_name = "PATH")]
        repo_root: Option<PathBuf>,
    },
}

/// Exit code for "the classifier could not run" — distinct from the 0/1
/// verdict codes on purpose.
const EX_USAGE: i32 = 2;

impl BotPrCommand {
    /// Never returns: every path exits with one of the three contract codes.
    ///
    /// Errors are turned into [`EX_USAGE`] **here** rather than propagated,
    /// because the generic top-level error path exits with the daemon's
    /// startup-failure code — and any code that is not `2` risks being read as
    /// a verdict by a caller branching on `0`/`1`.
    pub(crate) fn run(self) -> Result<()> {
        if let Err(e) = self.run_inner() {
            eprintln!("bot-pr: {e:#}");
            std::process::exit(EX_USAGE);
        }
        unreachable!("run_inner always exits with a verdict code")
    }

    fn run_inner(self) -> Result<()> {
        match self {
            BotPrCommand::Config { repo_root } => {
                let cfg = config::resolve(&resolve_root(repo_root)?);
                print!("{}", render::render_config(&cfg));
                std::process::exit(i32::from(!cfg.enabled));
            }
            BotPrCommand::Classify {
                author,
                title,
                body_file,
                files_from,
                repo_root,
            } => {
                let cfg = config::resolve(&resolve_root(repo_root)?);

                let mut raw = Vec::new();
                std::io::stdin()
                    .read_to_end(&mut raw)
                    .context("reading the unified diff from stdin")?;
                // Lossy rather than fatal, for the same reason
                // `merge-pr-refs` is: a diff is whatever the forge returned,
                // and refusing to classify one because a dependency's README
                // snippet carries invalid UTF-8 would fail the gate for a
                // reason unrelated to the gate.
                let diff_text = String::from_utf8_lossy(&raw).into_owned();

                let body = match &body_file {
                    Some(p) => std::fs::read_to_string(p)
                        .with_context(|| format!("reading --body-file {}", p.display()))?,
                    None => String::new(),
                };

                let files = match &files_from {
                    Some(p) => read_lines(p)?,
                    None => diff::paths(&diff_text),
                };

                let patches: HashMap<String, String> = diff::split_by_file(&diff_text);

                let verdict = classify::classify(
                    &cfg,
                    &ClassifyInput {
                        author,
                        title,
                        body,
                        files,
                        patches,
                    },
                );
                print!("{}", render::render_verdict(&verdict));
                std::process::exit(i32::from(verdict.is_err()));
            }
        }
    }
}

fn resolve_root(explicit: Option<PathBuf>) -> Result<PathBuf> {
    match explicit {
        Some(p) => Ok(p),
        None => loom_daemon::repo_root::find_repo_root(std::path::Path::new(".")).map_or_else(
            || {
                eprintln!("bot-pr: not inside a git repository; pass --repo-root");
                std::process::exit(EX_USAGE)
            },
            Ok,
        ),
    }
}

fn read_lines(path: &PathBuf) -> Result<Vec<String>> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("reading --files-from {}", path.display()))?;
    Ok(text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(ToString::to_string)
        .collect())
}
