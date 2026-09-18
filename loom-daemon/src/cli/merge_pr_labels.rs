//! `loom-daemon merge-pr verdict-contradiction` (#8112, slice 2 of #8191).
//!
//! # Passing requires a POSITIVE signal, and that is the whole design
//!
//! | outcome | stdout | exit |
//! |---|---|---|
//! | clean | [`loom_daemon::merge_pr::labels::CLEAN`] | 0 |
//! | contradictory | the refusal | 1 |
//! | could not answer | — | 2 |
//!
//! Porting a *sourced shell function* into a *subprocess* introduces a failure
//! mode the original could not have: the binary can be missing, unreadable, or
//! an older install that does not know this subcommand. A sourced function is
//! either defined or the script does not start.
//!
//! The obvious contract — mirror the shell's `0 = contradiction, 1 = clean` —
//! is fail-OPEN, and a test caught it. Exit 1 is also the most common generic
//! failure code in existence, so anything that fell over quietly would be read
//! as "reviewed and clean" and the merge would proceed. The inverse mapping
//! fails the same way against anything that exits 0.
//!
//! So a pass requires the caller to see the CLEAN sentinel on stdout AND a
//! zero exit.
//! Only this subcommand, answering the question that was actually asked, can
//! produce that pair; every other outcome — wrong binary, missing binary, old
//! binary, silent success, silent failure — refuses the merge. That is the
//! right default for the last check standing between a racing approval and an
//! irreversible merge.

use anyhow::Result;
use std::io::Read;

#[derive(clap::Args)]
pub(crate) struct VerdictContradictionArgs {
    /// The PR number, for the refusal message.
    #[arg(long, value_name = "N")]
    pr: String,

    /// The head SHA the labels were read against.
    #[arg(long, value_name = "SHA", default_value = "")]
    head_sha: String,

    /// Labels, newline-separated. Omit to read them from stdin, which is what
    /// the caller does: a label is forge-controlled text and belongs on a
    /// stream rather than in an argument vector.
    #[arg(long, value_name = "LABELS")]
    labels: Option<String>,
}

impl VerdictContradictionArgs {
    pub(crate) fn run(self) -> Result<()> {
        let labels = match self.labels {
            Some(l) => l,
            None => {
                let mut buf = String::new();
                if std::io::stdin().read_to_string(&mut buf).is_err() {
                    // Unreadable stdin means an unknown label set, which is
                    // not the same as an empty one.
                    eprintln!("merge-pr verdict-contradiction: could not read labels from stdin");
                    std::process::exit(2);
                }
                buf
            }
        };

        match loom_daemon::merge_pr::labels::contradiction(&labels) {
            Some(blocker) => {
                print!(
                    "{}",
                    loom_daemon::merge_pr::labels::message(
                        &self.pr,
                        blocker,
                        &labels,
                        &self.head_sha
                    )
                );
                println!();
                std::process::exit(1)
            }
            None => {
                println!("{}", loom_daemon::merge_pr::labels::CLEAN);
                std::process::exit(0)
            }
        }
    }
}
