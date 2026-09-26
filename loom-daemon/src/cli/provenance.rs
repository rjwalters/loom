//! `loom-daemon provenance …` (#9027): renders the D33 provenance stamps so
//! no shell script owns their format (`.loom/docs/shell-language-policy.md`).
//! The formats themselves live in [`loom_daemon::provenance`].
use anyhow::Result;
use loom_daemon::provenance::{self, hooks, marker};
use std::path::PathBuf;

#[derive(clap::Subcommand)]
pub(crate) enum ProvenanceCommand {
    /// Print the three commit trailers (`Loom-Story`, `Loom-Trace-Id`,
    /// `Loom-Build`) for an issue of the checkout at `--repo-root`.
    Trailers {
        #[arg(long)]
        issue: u32,
        #[arg(long, default_value = ".")]
        repo_root: PathBuf,
    },
    /// `commit-msg` hook entry point: append the trailers a dispatched
    /// sweep's environment carries to the message file. A no-op outside a
    /// sweep; exits non-zero only when stamping itself failed.
    StampCommitMsg { file: PathBuf },
    /// Print the hidden `<!-- loom:provenance v1 … -->` PR-body line.
    PrMarker {
        /// The issue the PR closes (story + trace); omitted → `unknown`.
        #[arg(long)]
        issue: Option<u32>,
        /// Sweep id; defaults to `$LOOM_SWEEP_ID`, else `unknown`.
        #[arg(long)]
        sweep: Option<String>,
        /// Ref the branch forked from (default `origin/HEAD`, then `origin/main`).
        #[arg(long)]
        base_ref: Option<String>,
        #[arg(long, default_value = ".")]
        repo_root: PathBuf,
    },
}

impl ProvenanceCommand {
    pub(crate) fn run(self) -> Result<()> {
        match self {
            Self::Trailers { issue, repo_root } => {
                for line in provenance::Trailers::for_issue(&repo_root, issue).lines() {
                    println!("{line}");
                }
            }
            Self::StampCommitMsg { file } => {
                if let Some(trailers) = hooks::trailers_from_env() {
                    hooks::stamp_message_file(&file, &trailers)?;
                }
            }
            Self::PrMarker {
                issue,
                sweep,
                base_ref,
                repo_root,
            } => {
                let inputs = marker::MarkerInputs {
                    issue,
                    sweep: sweep.as_deref(),
                    base_ref: base_ref.as_deref(),
                };
                println!("{}", marker::collect(&repo_root, &inputs).render());
            }
        }
        Ok(())
    }
}
