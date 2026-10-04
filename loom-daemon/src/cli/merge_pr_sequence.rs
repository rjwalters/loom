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
        let bin = loom_daemon::gh_invocation::gh_bin();

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

/// `loom-daemon merge-pr sequence-plan` (#9686) — compute the landing-order
/// plan for a repository and PRINT it, touching nothing. The read-only
/// replay surface: run it against a real PR inventory to record what the
/// pass would do before enabling it.
#[derive(clap::Args)]
pub(crate) struct SequencePlanArgs {
    /// OWNER/REPO to plan. Omit to let `gh` resolve the repository from the
    /// working directory.
    #[arg(long, value_name = "OWNER/REPO")]
    repo: Option<String>,
}

impl SequencePlanArgs {
    pub(crate) fn run(self) -> Result<()> {
        // The planner resolves the repo from cwd (gh_pr applies LOOM_REPO),
        // so run from the requested directory when one is given.
        if let Some(nwo) = self.repo.as_deref() {
            std::env::set_var("LOOM_REPO", nwo);
        }
        let root = std::env::current_dir()?;
        let gh = std::env::var("LOOM_GH_BIN").unwrap_or_else(|_| "gh".into());
        let report = loom_daemon::claim_reconciliation::merge_sequence::plan_report(
            std::path::Path::new(&gh),
            &root,
        )?;
        println!(
            "{} open PR(s); trigger is >2; {} existing holder(s)",
            report.open_prs, report.holders
        );
        // #10077: existing holds Phase 1 would release (nothing is written).
        for (follower, after) in &report.would_release_no_overlap {
            println!("would release #{follower} (after #{after}): no shared files");
        }
        println!(
            "would release {} hold(s) with no shared files",
            report.would_release_no_overlap.len()
        );
        if report.groups.is_empty() {
            println!("no overlapping groups planned");
            return Ok(());
        }
        for g in &report.groups {
            let chain: Vec<String> = g.order.iter().map(|n| format!("#{n}")).collect();
            // `order` is the landing order; edges are the direct-overlap DAG
            // (#10060), so two members need not be ordered against each other.
            println!("group (plan {}) landing order: {}", g.plan, chain.join(" -> "));
            for e in &g.edges {
                println!(
                    "  would sequence #{follower} after #{after} at pred_head {} [{reason:?}]",
                    e.pred_head,
                    follower = e.follower,
                    after = e.after,
                    reason = e.reason
                );
            }
        }
        println!(
            "would apply {} edge(s); {} already satisfied",
            report.groups.iter().map(|g| g.edges.len()).sum::<usize>() - report.already_planned,
            report.already_planned
        );
        Ok(())
    }
}
