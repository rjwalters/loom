//! The three dependency-classification subcommands (epic #7810, PR 3).
//!
//! These back `classify-dependency-block.sh`, `detect-dependency-cycle.sh` and
//! `detect-startable-subset.sh`, which are now thin stubs that `exec` into
//! them. Their flags, stdout markers and exit codes are contract: role prompts
//! invoke the stubs by path and parse their output line-wise. What each answers,
//! and why each field exists, is documented on
//! [`loom_daemon::dep_classify::cli`].
//!
//! The args live here rather than in `main.rs`'s `Commands` enum for the same
//! reason `RestartArgs` does (#6969): `main.rs` is over
//! `.loom/docs/file-size-policy.md`'s threshold and frozen, so it keeps only a
//! flattened variant and a one-line dispatch arm.

use anyhow::Result;
use loom_daemon::dep_classify::cli;
use std::path::PathBuf;

#[derive(clap::Subcommand)]
pub(crate) enum DepClassifyCommand {
    /// Champion's dependency-timing gate: should this proposal be parked on an
    /// open dependency, and may a parked one be released? (#5664, #7650)
    ///
    /// Exit codes: `0` defer / un-escalate, `1` do not (a verdict, not an
    /// error), `2` invalid arguments or an unreadable issue, `3` re-evaluate —
    /// every recorded blocker has closed, `4` promote a startable subset. `3`
    /// and `4` are `--check-defer` only.
    ClassifyDependencyBlock {
        #[arg(long, value_name = "N")]
        issue: i64,

        /// Defaults to the checkout's origin remote.
        #[arg(long, value_name = "OWNER/REPO")]
        repo: Option<String>,

        /// Should Champion park this proposal? (the default mode)
        #[arg(long = "check-defer")]
        check_defer: bool,

        /// May a parked proposal be released because its blockers closed?
        #[arg(long = "check-unescalate", conflicts_with = "check_defer")]
        check_unescalate: bool,

        /// May a parked proposal be released because a commit resolved every
        /// cited finding? (#7650)
        #[arg(
            long = "check-fact-unescalate",
            conflicts_with_all = ["check_defer", "check_unescalate"]
        )]
        check_fact_unescalate: bool,

        /// Perform the release, not just report it. Only meaningful with one of
        /// the two un-escalate modes; rejected otherwise rather than silently
        /// doing nothing.
        #[arg(long)]
        apply: bool,

        /// Per-finding `RESOLVED:` / `UNRESOLVED:` lines
        /// (`--check-fact-unescalate` only).
        #[arg(long = "resolutions-file", value_name = "PATH")]
        resolutions_file: Option<String>,

        /// The commit that resolved them (`--check-fact-unescalate` only).
        #[arg(long, value_name = "SHA")]
        commit: Option<String>,

        /// Read the findings from here instead of the issue's comments.
        #[arg(long = "findings-file", value_name = "PATH")]
        findings_file: Option<String>,

        /// Skip the bounded dependency-cycle walk.
        #[arg(long = "skip-cycle-check")]
        skip_cycle_check: bool,

        /// Bypass the `gh` read cache.
        #[arg(long = "no-cache")]
        no_cache: bool,
    },

    /// Walk the declared dependency graph for a closed loop (#5671).
    ///
    /// Exit codes: `0` no cycle found, `1` **cycle found** — data, not an
    /// error, `2` invalid arguments or an unreadable root issue.
    DetectDependencyCycle {
        #[arg(long, value_name = "N")]
        issue: i64,

        /// Defaults to the checkout's origin remote.
        #[arg(long, value_name = "OWNER/REPO")]
        repo: Option<String>,

        #[arg(long = "max-depth", value_name = "N")]
        max_depth: Option<usize>,

        #[arg(long = "max-nodes", value_name = "N")]
        max_nodes: Option<usize>,

        #[arg(long = "max-steps", value_name = "N")]
        max_steps: Option<usize>,

        /// Post the cycle report and park the issue for an operator
        /// (idempotent on the cycle's fingerprint).
        #[arg(long)]
        report: bool,

        /// Bypass the `gh` read cache.
        #[arg(long = "no-cache")]
        no_cache: bool,
    },

    /// Extract the part of an issue that does not depend on its blockers
    /// (#5664).
    ///
    /// Exit codes: `0` a subset is declared and printed, `1` none is — data,
    /// not an error, `2` invalid arguments or an unreadable issue.
    DetectStartableSubset {
        #[arg(long, value_name = "N")]
        issue: i64,

        /// Defaults to the checkout's origin remote.
        #[arg(long, value_name = "OWNER/REPO")]
        repo: Option<String>,

        /// Read the body from here instead of the forge.
        #[arg(long = "body-file", value_name = "PATH")]
        body_file: Option<String>,

        /// Bypass the `gh` read cache.
        #[arg(long = "no-cache")]
        no_cache: bool,
    },
}

impl DepClassifyCommand {
    /// Never returns: each arm exits with the subcommand's own code, which
    /// callers branch on.
    pub(crate) fn run(self) -> Result<()> {
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        match self {
            DepClassifyCommand::ClassifyDependencyBlock {
                issue,
                repo,
                // `--check-defer` is the default, so the flag only ever confirms
                // it; clap's `conflicts_with` keeps the three exclusive.
                check_defer: _,
                check_unescalate,
                check_fact_unescalate,
                apply,
                resolutions_file,
                commit,
                findings_file,
                skip_cycle_check,
                no_cache,
            } => {
                let mode = if check_fact_unescalate {
                    cli::Mode::FactUnescalate
                } else if check_unescalate {
                    cli::Mode::Unescalate
                } else {
                    cli::Mode::Defer
                };
                let opts = cli::ClassifyOpts {
                    issue,
                    repo,
                    mode,
                    apply,
                    resolutions_file,
                    commit,
                    findings_file,
                    skip_cycle_check,
                    no_cache,
                };
                std::process::exit(cli::run_classify(&cwd, &opts));
            }
            DepClassifyCommand::DetectDependencyCycle {
                issue,
                repo,
                max_depth,
                max_nodes,
                max_steps,
                report,
                no_cache,
            } => {
                let opts = cli::CycleOpts {
                    issue,
                    repo,
                    max_depth,
                    max_nodes,
                    max_steps,
                    report,
                    no_cache,
                };
                std::process::exit(cli::run_cycle(&cwd, &opts));
            }
            DepClassifyCommand::DetectStartableSubset {
                issue,
                repo,
                body_file,
                no_cache,
            } => {
                let opts = cli::SubsetOpts {
                    issue,
                    repo,
                    body_file,
                    no_cache,
                };
                std::process::exit(cli::run_subset(&cwd, &opts));
            }
        }
    }
}
