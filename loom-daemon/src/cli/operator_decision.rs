//! `loom-daemon operator-decision` (#9344): clap shim over
//! [`loom_daemon::operator_decision`], which documents the contract and the
//! exit codes. Lives here, not in `main.rs`, for the frozen-`main.rs` reason
//! `PremiseCheckArgs` gives.

use anyhow::Result;
use loom_daemon::operator_decision::cli::{
    self, apply, default_repo_root, read_input, validate_cmd, ApplyRequest, GhForge, Target,
};
use std::path::PathBuf;

#[derive(clap::Subcommand, Debug)]
pub(crate) enum OperatorDecisionCommand {
    /// Check decision JSON against the ranked-options contract. Exit 0 valid,
    /// 1 refused (every reason on stderr), 2 unreadable input.
    Validate {
        /// Decision JSON file, or `-` for stdin.
        #[arg(long, value_name = "FILE|-")]
        input: PathBuf,
    },
    /// Write a validated decision onto an issue (or file a new one) and label
    /// it loom:operator-decision. Refuses, touching nothing, on any contract
    /// failure.
    Apply {
        /// Existing issue to rewrite and relabel (relabel mode).
        #[arg(value_name = "ISSUE", required_unless_present = "new")]
        issue: Option<u64>,

        /// File a new issue instead (filing mode); requires --title.
        #[arg(long, requires = "title", conflicts_with_all = ["issue", "remove_label"])]
        new: bool,

        /// Title for --new.
        #[arg(long, value_name = "TEXT", requires = "new")]
        title: Option<String>,

        /// Decision JSON file, or `-` for stdin.
        #[arg(long, value_name = "FILE|-")]
        input: PathBuf,

        /// Extra label to add with loom:operator-decision (repeatable), e.g.
        /// loom:operator-only.
        #[arg(long = "also-label", value_name = "LABEL")]
        also_label: Vec<String>,

        /// Label to remove after the body and labels are written
        /// (repeatable), e.g. loom:building. Relabel mode only.
        #[arg(long = "remove-label", value_name = "LABEL")]
        remove_label: Vec<String>,

        /// Print the planned body and label changes; mutate nothing.
        #[arg(long = "dry-run")]
        dry_run: bool,

        /// Target repository; defaults to the checkout's remote.
        #[arg(long, value_name = "OWNER/REPO")]
        repo: Option<String>,
    },
}

impl OperatorDecisionCommand {
    /// Never returns: exits with the command's own code.
    pub(crate) fn run(self) -> Result<()> {
        let mut out = std::io::stdout();
        let mut err = std::io::stderr();
        let code = match self {
            OperatorDecisionCommand::Validate { input } => match read_input(&input) {
                Ok(text) => validate_cmd(&text, &mut out, &mut err),
                Err(e) => {
                    eprintln!("ERROR: {e}");
                    cli::exit::USAGE
                }
            },
            OperatorDecisionCommand::Apply {
                issue,
                new,
                title,
                input,
                also_label,
                remove_label,
                dry_run,
                repo,
            } => {
                let target = match (new, issue, title) {
                    (true, _, Some(title)) => Target::New { title },
                    (false, Some(n), _) => Target::Existing(n),
                    _ => {
                        eprintln!("ERROR: pass an ISSUE number, or --new --title TEXT");
                        std::process::exit(cli::exit::USAGE);
                    }
                };
                match read_input(&input) {
                    Ok(text) => {
                        let mut forge = GhForge::new(default_repo_root(), repo);
                        let req = ApplyRequest {
                            target,
                            also_labels: also_label,
                            remove_labels: remove_label,
                            dry_run,
                        };
                        apply(&mut forge, &text, &req, &mut out, &mut err)
                    }
                    Err(e) => {
                        eprintln!("ERROR: {e}");
                        cli::exit::USAGE
                    }
                }
            }
        };
        std::process::exit(code)
    }
}
