//! `loom-daemon merge-pr revalidate` (#8410, an #8191 slice): `--auto`'s
//! post-wait re-validation read — see [`loom_daemon::merge_pr::revalidate`].
//!
//! # Protocol
//!
//! Reads the uncached PR re-read (`forge_get_pr_nocache`'s output) on stdin.
//!
//! | verdict | stdout | exit |
//! |---|---|---|
//! | merged underneath the wait | `LOOM-REVALIDATE MERGED` | 0 |
//! | head moved (#5579 re-queue) | `LOOM-REVALIDATE HEAD-MOVED <fresh-sha>` | 3 |
//! | head unchanged | `LOOM-REVALIDATE LABELS`, then one label per line | 0 |
//! | unreadable re-read (#8896) | the `Merge blocked: could not re-read …` refusal | 1 |
//!
//! A pass requires the exit code AND the sentinel together, as for every
//! fail-closed verb in this family: the caller (`_revalidate_merge_guards`)
//! refuses the merge on any other pair — a missing binary, one that predates
//! this verb (clap's exit 2), a crash, silence — because "re-validated" and
//! "never looked" must not be confusable at the last check before an
//! irreversible merge. Exit 3 matches the code the caller then exits with via
//! `error_head_moved`, so the two cannot drift into meaning different things.

use anyhow::Result;
use loom_daemon::merge_pr::revalidate::{decide, unreadable_message, Verdict};
use std::io::Read;

#[derive(clap::Args)]
pub(crate) struct RevalidateArgs {
    /// The PR number, for the refusal message.
    #[arg(long, value_name = "N")]
    pr: String,

    /// The head SHA the merge is gated on (`$MERGE_PRECONDITION_SHA`). Empty
    /// skips the head comparison, as the retired `-n` test did.
    #[arg(long, value_name = "SHA", default_value = "")]
    precondition_sha: String,
}

impl RevalidateArgs {
    pub(crate) fn run(self) -> Result<()> {
        let mut raw = Vec::new();
        let payload = match std::io::stdin().read_to_end(&mut raw) {
            // Strict UTF-8: JSON text is UTF-8 by definition, and a lossy
            // replacement would invent label names the forge never sent.
            Ok(_) => String::from_utf8(raw).ok(),
            Err(_) => None,
        };
        let verdict = match payload {
            Some(p) => decide(&p, &self.precondition_sha),
            None => Verdict::Unreadable("stdin is unreadable or not UTF-8".into()),
        };
        match verdict {
            Verdict::Merged => println!("LOOM-REVALIDATE MERGED"),
            Verdict::HeadMoved(sha) => {
                println!("LOOM-REVALIDATE HEAD-MOVED {sha}");
                std::process::exit(3);
            }
            Verdict::Labels(labels) if labels.is_empty() => println!("LOOM-REVALIDATE LABELS"),
            Verdict::Labels(labels) => println!("LOOM-REVALIDATE LABELS\n{labels}"),
            Verdict::Unreadable(why) => {
                eprintln!("merge-pr revalidate: {why}");
                println!("{}", unreadable_message(&self.pr));
                std::process::exit(1);
            }
        }
        Ok(())
    }
}
