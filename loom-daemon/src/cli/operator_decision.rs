//! `loom-daemon operator-decision` (#9344): clap shim over
//! [`crate::operator_decision`], which documents the contract and the
//! exit codes. Lives here, not in `main.rs`, for the frozen-`main.rs` reason
//! `PremiseCheckArgs` gives.

use crate::operator_decision::cli::{
    self, apply, default_repo_root, read_input, validate_cmd, ApplyRequest, GhForge, Target,
};
use anyhow::Result;
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
    /// it loom:operator (#11795). Refuses, touching nothing, on any contract
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

        /// Extra label to add with loom:operator (repeatable), e.g.
        /// loom:important. A retired name is added as its replacement.
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
    /// Record the operator's choice of one ranked option on an issue's
    /// published decision (#14399), bound to the exact decision and proposal
    /// reviewed. Attended only: needs a terminal and a typed confirmation, and
    /// refuses under a dispatched sweep. `premise-check` then lets a deliberate
    /// reversal proceed for an `--effect proceed` resolution it can verify.
    Resolve {
        /// Issue whose published decision is being resolved.
        #[arg(value_name = "ISSUE")]
        issue: u64,

        /// The approved option's `id` from the decision block.
        #[arg(long, value_name = "ID")]
        option: String,

        /// What the choice means: `proceed` approves the proposal as filed;
        /// `reject`, `defer` and `rescope` are recorded but never open the gate.
        #[arg(long, value_name = "EFFECT", default_value = "proceed")]
        effect: String,

        /// After the resolution verifies, remove the loom:operator hold
        /// (`--effect proceed` only).
        #[arg(long)]
        release: bool,

        /// Show the review (option, scope digests) and record nothing.
        #[arg(long = "dry-run")]
        dry_run: bool,

        /// Target repository; defaults to the checkout's remote.
        #[arg(long, value_name = "OWNER/REPO")]
        repo: Option<String>,
    },
}

/// `loom-tools operator-decision <action>`: the [`OperatorDecisionCommand`]
/// tree, unchanged (#14464, epic #12708 Phase 3b); see
/// [`ParkRecordCli`](super::park_record::ParkRecordCli).
#[derive(clap::Args)]
pub struct OperatorDecisionCli {
    #[command(subcommand)]
    command: OperatorDecisionCommand,
}

impl OperatorDecisionCli {
    /// Whether this invocation can reach the forge, and so must pass the mode
    /// gate (design §10.3) before it runs. See
    /// [`OperatorDecisionCommand::reaches_forge`].
    #[must_use]
    pub fn reaches_forge(&self) -> bool {
        self.command.reaches_forge()
    }

    /// Whether this invocation files a new issue, which runs
    /// `create-issue.sh` (duplicate check, filing lock) and a nested
    /// `forge create-issue`.
    #[must_use]
    pub fn files_new_issue(&self) -> bool {
        matches!(
            self.command,
            OperatorDecisionCommand::Apply {
                new: true,
                dry_run: false,
                ..
            }
        )
    }
}

/// Run the operator-decision action through the one shared handler.
///
/// # Errors
/// Whatever the handler returns; it exits the process with its own code.
pub fn handle_operator_decision_command(cli: OperatorDecisionCli) -> Result<()> {
    cli.command.run()
}

impl OperatorDecisionCommand {
    /// Whether this invocation can reach the forge. `validate` and
    /// `apply --new --dry-run` render offline; an existing-issue `apply`
    /// (including its preview) reads the body and labels; `apply --new` files.
    /// `resolve` reads the decision even as a preview, but a recording run that
    /// is not attended, or names an unknown effect, refuses before any forge
    /// call, so it must not be turned into a mode error first (exit 5/2 stay).
    pub(crate) fn reaches_forge(&self) -> bool {
        match self {
            OperatorDecisionCommand::Validate { .. } => false,
            OperatorDecisionCommand::Apply { new, dry_run, .. } => !(*new && *dry_run),
            OperatorDecisionCommand::Resolve {
                effect, dry_run, ..
            } => {
                crate::operator_decision::resolution::Effect::parse(effect).is_ok()
                    && (*dry_run || crate::operator_decision::resolve::require_attended().is_ok())
            }
        }
    }

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
                        let code = apply(&mut forge, &text, &req, &mut out, &mut err);
                        // #12027: best-effort, never the exit code.
                        crate::operator_decision::cli::journal_applied(&req, code);
                        code
                    }
                    Err(e) => {
                        eprintln!("ERROR: {e}");
                        cli::exit::USAGE
                    }
                }
            }
            OperatorDecisionCommand::Resolve {
                issue,
                option,
                effect,
                release,
                dry_run,
                repo,
            } => run_resolve(issue, option, &effect, release, dry_run, repo, &mut out, &mut err),
        };
        std::process::exit(code)
    }
}

/// The `resolve` shim: parse the effect, insist on an attended session unless
/// this is a read-only `--dry-run`, and confirm by having the operator type the
/// option id.
#[allow(clippy::too_many_arguments)]
fn run_resolve(
    issue: u64,
    option: String,
    effect: &str,
    release: bool,
    dry_run: bool,
    repo: Option<String>,
    out: &mut dyn std::io::Write,
    err: &mut dyn std::io::Write,
) -> i32 {
    use crate::operator_decision::{resolution::Effect, resolve};
    let effect = match Effect::parse(effect) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("ERROR: {e}");
            return cli::exit::USAGE;
        }
    };
    if !dry_run {
        if let Err(why) = resolve::require_attended() {
            eprintln!(
                "operator-decision: REFUSED - an operator resolution is an attended action: {why}"
            );
            return resolve::UNATTENDED;
        }
    }
    let root = default_repo_root();
    let policy = crate::comment_trust::TrustPolicy::for_root(&root);
    let mut forge = GhForge::new(root, repo).with_call_site("operator_decision.resolve");
    let req = resolve::ResolveRequest {
        issue,
        option,
        effect,
        release,
        dry_run,
    };
    let mut confirm = |expected: &str| {
        eprint!("Type the option id `{expected}` to record this resolution: ");
        let mut line = String::new();
        std::io::stdin().read_line(&mut line).is_ok() && line.trim() == expected
    };
    resolve::resolve(&mut forge, &policy, &req, &mut confirm, out, err)
}
