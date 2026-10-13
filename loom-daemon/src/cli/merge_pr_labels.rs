//! `loom-daemon merge-pr verdict-contradiction` (#8112, slice 2 of #8191).
//!
//! # Passing requires a POSITIVE signal, and that is the whole design
//!
//! | outcome | stdout | exit |
//! |---|---|---|
//! | clean | [`loom_daemon::merge_pr::labels::CLEAN`] | 0 |
//! | contradictory | the refusal | 1 |
//! | sequencing hold only, under `--sequenced-hold-repo` | `LOOM-SEQUENCED-HOLD pr=<N> after=<M>`, then the refusal | 7 |
//! | could not answer | — | 2 |
//!
//! Exit 7 (#11330) is opt-in: `merge-pr.sh` sets `LOOM_SEQUENCED_HOLD_REPO`
//! (an env var, so an older binary ignores it and keeps exit 1, and an older
//! script never sees 7). `after` is read best-effort from the PR's trusted
//! `loom:sequence` marker; the hold decision itself is still the label alone.
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

    /// OWNER/REPO. When set, a contradiction that is ONLY the `loom:sequenced`
    /// ordering hold exits 7 with a `LOOM-SEQUENCED-HOLD` line (#11330).
    #[arg(long, value_name = "OWNER/REPO", env = "LOOM_SEQUENCED_HOLD_REPO")]
    sequenced_hold_repo: Option<String>,
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
                let hold = self
                    .sequenced_hold_repo
                    .as_deref()
                    .filter(|r| !r.is_empty())
                    .filter(|_| loom_daemon::merge_pr::labels::sequenced_hold_only(&labels));
                if let Some(nwo) = hold {
                    println!(
                        "{}",
                        loom_daemon::merge_pr::labels::sequenced_hold_line(
                            &self.pr,
                            sequenced_after(nwo, &self.pr)
                        )
                    );
                }
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
                std::process::exit(if hold.is_some() { 7 } else { 1 })
            }
            None => {
                println!("{}", loom_daemon::merge_pr::labels::CLEAN);
                std::process::exit(0)
            }
        }
    }
}

/// The predecessor named by the PR's live trusted `loom:sequence` marker, or
/// `None` on any read/parse failure (display only — never changes the hold).
fn sequenced_after(nwo: &str, pr: &str) -> Option<u32> {
    use loom_daemon::merge_pr::sequence;
    let pr: u32 = pr.parse().ok()?;
    let root = std::env::current_dir().ok()?;
    let bodies =
        sequence::fetch_trusted_bodies(&loom_daemon::gh_invocation::gh_bin(), &root, nwo, pr)?;
    sequence::parse_live(&bodies).map(|m| m.after)
}
