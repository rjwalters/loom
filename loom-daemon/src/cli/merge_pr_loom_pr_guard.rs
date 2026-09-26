//! `loom-daemon merge-pr loom-pr-guard` (#7419, slice N of the merge-pr port
//! #8191).
//!
//! # Passing requires a POSITIVE signal
//!
//! Same contract shape as [`super::merge_pr_labels`], for the same reason:
//! porting a sourced shell function into a subprocess introduces "the binary
//! can be missing, unreadable, or an older install that does not know this
//! subcommand", and any exit-code mapping that treats an unrecognized outcome
//! as "approved" is fail-open — the last thing this guard, standing between a
//! racing approval and an irreversible merge, may ever be.
//!
//! | outcome | stdout | exit |
//! |---|---|---|
//! | `loom:pr` present | [`loom_daemon::merge_pr::loom_pr_guard::CLEAN`] | 0 |
//! | `loom:pr` absent, `--allow-unapproved` | the override warning | 0 |
//! | `loom:pr` absent, no override | the refusal | 1 |
//!
//! The two exit-0 outcomes are told apart by content, not by exit code alone
//! — `merge-pr.sh`'s wrapper only treats the SENTINEL as "proceed silently";
//! any other zero-exit text (including a real override message) is printed as
//! a warning first. That is deliberate: a caller comparing only the exit code
//! could not tell "nothing to warn about" from "proceeded, but say why".
//!
//! No `--dry-run` flag here, matching [`super::merge_pr_labels`]: dry-run is
//! purely a DISPLAY concern (would this block, without blocking), owned
//! entirely by the shell wrapper around this call, exactly as it already
//! wraps `_check_verdict_label_contradiction`.

use anyhow::Result;
use loom_daemon::merge_pr::loom_pr_guard::{assess, Verdict, CLEAN};
use std::io::Read;

#[derive(clap::Args)]
pub(crate) struct LoomPrGuardArgs {
    /// The PR number, for the refusal message.
    #[arg(long, value_name = "N")]
    pr: String,

    /// The head SHA the labels were read against.
    #[arg(long, value_name = "SHA", default_value = "")]
    head_sha: String,

    /// The operator asserts responsibility for merging without a `loom:pr`
    /// review signal — overrides the block (never bypasses a present
    /// contradicting label; see `merge-pr verdict-contradiction` for that,
    /// which has no override).
    #[arg(long)]
    allow_unapproved: bool,

    /// Labels, newline-separated. Omit to read them from stdin, which is
    /// what the caller does: a label is forge-controlled text and belongs on
    /// a stream rather than in an argument vector.
    #[arg(long, value_name = "LABELS")]
    labels: Option<String>,
}

impl LoomPrGuardArgs {
    pub(crate) fn run(self) -> Result<()> {
        let labels = match self.labels {
            Some(l) => l,
            None => {
                let mut buf = String::new();
                if std::io::stdin().read_to_string(&mut buf).is_err() {
                    // Unreadable stdin means an unknown label set, which is
                    // not the same as an empty one.
                    eprintln!("merge-pr loom-pr-guard: could not read labels from stdin");
                    std::process::exit(2);
                }
                buf
            }
        };

        match assess(&self.pr, &labels, &self.head_sha, self.allow_unapproved) {
            Verdict::Approved => {
                println!("{CLEAN}");
                std::process::exit(0);
            }
            Verdict::Overridden(msg) => {
                println!("{msg}");
                std::process::exit(0);
            }
            Verdict::Blocked(msg) => {
                println!("{msg}");
                std::process::exit(1);
            }
        }
    }
}
