//! `loom-daemon merge-pr sequence-eval` (#9378) — evaluate one durable
//! "approved, but not yet" sequencing hold against live forge state.
//!
//! # Contract
//!
//! | outcome | stdout | exit |
//! |---|---|---|
//! | any answer ([`CLEAR`]/[`DISSOLVED`]/[`KEEP`]/[`REPLAN`]/[`NONE`]) | the sentinel-led line | 0 |
//! | could not read trusted comments, or could not read the predecessor | (nothing on stdout) | 2 |
//!
//! Exit 0 on every *answered* outcome, including KEEP: this verb is the
//! evaluation half of the gate, not a refusal — the merge path itself refuses
//! via the label (`loom:sequenced` in `labels::BLOCKING`), and this verb is
//! what decides whether the label may move. What makes it fail-closed is the
//! release rule: a caller clears the hold ONLY on a positive `CLEAR` (or
//! `DISSOLVED`, per its own policy) from a zero exit — exactly the
//! positive-signal asymmetry `verdict-contradiction` established. A missing
//! binary, an old install, a forge error, or any exit other than 0 must
//! never release a hold, so failure prints nothing a caller could mistake
//! for an answer and exits 2.
//!
//! The follower's `--head-sha` comes from the caller (the value it read when
//! deciding to act), not from this verb's own read: the point of the
//! `follower_head` pin is to detect that the tree the caller is about to act
//! on is the tree the plan judged.

use anyhow::Result;
use loom_daemon::merge_pr::sequence::{self, verdict_line, NONE};

#[derive(clap::Args)]
pub(crate) struct SequenceEvalArgs {
    /// The PR whose sequencing hold is being evaluated (the follower).
    #[arg(long, value_name = "N")]
    pr: u32,

    /// The follower's current head SHA, as the caller read it. A mismatch
    /// with the marker's `follower_head` pin is a REPLAN, never a release.
    #[arg(long, value_name = "SHA")]
    head_sha: String,

    /// OWNER/REPO to evaluate against. Omit to let `gh` resolve the
    /// repository from the working directory.
    #[arg(long, value_name = "OWNER/REPO")]
    repo: Option<String>,
}

impl SequenceEvalArgs {
    pub(crate) fn run(self) -> Result<()> {
        let root = std::env::current_dir()?;
        // The `{owner}/{repo}` template lets `gh` resolve the repo from cwd,
        // matching the reconciliation scanner's convention.
        let nwo = self.repo.as_deref().unwrap_or("{owner}/{repo}");
        let bin = sequence::gh_bin();

        let Some(bodies) = sequence::fetch_trusted_bodies(&bin, &root, nwo, self.pr) else {
            eprintln!(
                "merge-pr sequence-eval: could not read trusted comments on PR #{} — \
                 failing closed (never release on a failed read)",
                self.pr
            );
            std::process::exit(2);
        };
        let Some(marker) = sequence::parse(&bodies) else {
            println!("{NONE}");
            return Ok(());
        };

        let Some(pred) = sequence::fetch_predecessor(&bin, &root, nwo, marker.after) else {
            eprintln!(
                "merge-pr sequence-eval: could not read predecessor PR #{}'s state — \
                 failing closed (never release on a failed read)",
                marker.after
            );
            std::process::exit(2);
        };

        let verdict = sequence::evaluate(&marker, &pred, &self.head_sha);
        println!("{}", verdict_line(verdict, &marker));
        Ok(())
    }
}
