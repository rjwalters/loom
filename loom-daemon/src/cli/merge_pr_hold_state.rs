//! `loom-daemon merge-pr hold-state` (#7419 AC #3, a slice of the merge-pr
//! port #8191).
//!
//! # This one is advisory, and its exit codes say so
//!
//! Its siblings ([`super::merge_pr_labels`], [`super::merge_pr_loom_pr_guard`])
//! are merge GATES: they exit 1 to refuse, and their shell wrappers treat any
//! unrecognized outcome as a refusal, because a gate that cannot run must not
//! pass. This check is not a gate. It only ever prints a warning beside a
//! merge that is going ahead regardless, so exit 1 would mean "refuse" to a
//! caller that has no refusal to make.
//!
//! | outcome | stdout | exit |
//! |---|---|---|
//! | no marker, or the marker names this head | [`CLEAN`] | 0 |
//! | the marker names a different head | the warning | 0 |
//! | stdin unreadable | (nothing) | 2 |
//!
//! Both normal outcomes exit 0 and are told apart by CONTENT, the same
//! discrimination `loom-pr-guard` uses for its two zero-exit outcomes. The
//! sentinel still earns its keep: without it the shell cannot distinguish "the
//! check ran and found nothing" from "the binary is older than this
//! subcommand and printed nothing", and only the second deserves a note in the
//! log.
//!
//! Comments arrive on stdin, never in argv: they are forge-controlled text,
//! routinely tens of kilobytes, and an argument vector is the wrong place for
//! either property.

use anyhow::Result;
use loom_daemon::merge_pr::hold_state::{assess, CLEAN};
use std::io::Read;

#[derive(clap::Args)]
pub(crate) struct HoldStateArgs {
    /// The PR number, for the warning text.
    #[arg(long, value_name = "N")]
    pr: String,

    /// The head SHA about to be merged — what the recorded marker is compared
    /// against.
    #[arg(long, value_name = "SHA", default_value = "")]
    head_sha: String,

    /// Comment bodies, concatenated. Omit to read them from stdin, which is
    /// what the caller does.
    #[arg(long, value_name = "TEXT")]
    comments: Option<String>,
}

impl HoldStateArgs {
    pub(crate) fn run(self) -> Result<()> {
        let comments = match self.comments {
            Some(c) => c,
            None => {
                let mut buf = String::new();
                if std::io::stdin().read_to_string(&mut buf).is_err() {
                    // Unreadable stdin is an unknown comment stream, which is
                    // not the same as an empty one — say so rather than
                    // printing the clean sentinel over a read that failed.
                    eprintln!("merge-pr hold-state: could not read comments from stdin");
                    std::process::exit(2);
                }
                buf
            }
        };

        match assess(&self.pr, &comments, &self.head_sha) {
            Some(warning) => println!("{warning}"),
            None => println!("{CLEAN}"),
        }
        Ok(())
    }
}
