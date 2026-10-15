//! `loom-daemon merge-pr auto-refresh` (#12014): `merge-pr.sh --auto`'s
//! in-process refresh of a stale PR. Every decision is in
//! [`loom_daemon::merge_pr::auto_refresh`]; `merge-pr.sh` only dispatches.
//!
//! # Protocol (exit = `merge-pr.sh`'s own exit for that outcome)
//!
//! | `--phase` | outcome | stdout (first line) | exit |
//! |---|---|---|---|
//! | any | auto-refresh off (`merge.autoRefresh=false`, no `--explicit`) | `LOOM-AUTO-REFRESH-OFF` (`timeout`: nothing) | 0 |
//! | `stale` | re-date pushed / base synced | `LOOM-AUTO-REFRESH next=reinvoke pr=N round=k/B head=S` | 4 |
//! | `stale` | backoff deferral / sync conflict | `LOOM-AUTO-REFRESH next=requeue …` | 4 |
//! | `stale` | rounds and sync handoffs spent (`loom:operator` kept) | `LOOM-AUTO-REFRESH-EXHAUSTED pr=N spent=k budget=B` | 6 |
//! | `stale` | branch moved before the push | `LOOM-AUTO-REFRESH-HEAD-MOVED current=S` | 3 |
//! | `stale`/`verdict` | remedy or forge read failed | `LOOM-AUTO-REFRESH-FAILED` + reason | 1 |
//! | `verdict` | not a Loom refresh head, or the approval carries (`tree`/`clean-merge`) | `LOOM-AUTO-REFRESH-CLEAR` | 0 |
//! | `verdict` | the approval does not carry | `LOOM-AUTO-REFRESH next=judge …` | 3 |
//! | `timeout` | Loom refresh head, CI pending | `LOOM-AUTO-REFRESH next=reinvoke\|requeue …` | 0 |
//! | `timeout` | any other head | nothing | 0 |
//!
//! # Fail direction
//!
//! OPEN onto the pre-#12014 behaviour, never onto a merge: `merge-pr.sh`
//! acts only on a `LOOM-AUTO-REFRESH*` first line, so a binary without this
//! verb (clap exit 2, no stdout) leaves the #8248 refusal standing (or runs
//! the legacy `redate-checks` under `--redate-stale-checks`), skips the
//! post-wait gate exactly as before, and prints no re-invoke advice on exit 5.

use anyhow::Result;
use loom_daemon::merge_pr::auto_refresh::{run_stale, run_timeout, run_verdict, Ctx};

#[derive(clap::ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    /// The #8248 guard said STALE: run the re-date remedy and map it.
    Stale,
    /// After the settle-wait: the verdict gate on a Loom-refreshed head.
    Verdict,
    /// `merge-pr.sh` is exiting 5: re-invoke now, or re-queue?
    Timeout,
}

#[derive(clap::Args)]
pub(crate) struct AutoRefreshArgs {
    /// Which decision `merge-pr.sh` is asking for.
    #[arg(long, value_enum)]
    phase: Phase,

    /// The PR number.
    #[arg(long, value_name = "N")]
    pr: String,

    /// The repository as owner/repo.
    #[arg(long, value_name = "OWNER/REPO")]
    repo: String,

    /// The PR's head branch name (the ref a re-date pushes onto).
    #[arg(long, value_name = "BRANCH")]
    branch: String,

    /// The head SHA this decision is about.
    #[arg(long, value_name = "SHA")]
    head_sha: String,

    /// `--redate-stale-checks` was passed: on regardless of the opt-out.
    #[arg(long)]
    explicit: bool,
}

impl AutoRefreshArgs {
    pub(crate) fn run(self) -> Result<()> {
        let ctx = Ctx {
            pr: self.pr,
            nwo: self.repo,
            branch: self.branch,
            head: self.head_sha,
            explicit: self.explicit,
        };
        let d = match self.phase {
            Phase::Stale => run_stale(&ctx),
            Phase::Verdict => run_verdict(&ctx),
            Phase::Timeout => run_timeout(&ctx, chrono::Utc::now()),
        };
        if !d.stdout.is_empty() {
            println!("{}", d.stdout);
        }
        std::process::exit(d.exit);
    }
}

#[cfg(test)]
mod tests {
    use super::{AutoRefreshArgs, Phase};
    use clap::Parser;

    #[derive(Parser)]
    struct Harness {
        #[command(flatten)]
        args: AutoRefreshArgs,
    }

    #[test]
    fn parses_the_operands_merge_pr_sh_passes() {
        for (phase, want) in [
            ("stale", Phase::Stale),
            ("verdict", Phase::Verdict),
            ("timeout", Phase::Timeout),
        ] {
            let h = Harness::try_parse_from([
                "x",
                "--phase",
                phase,
                "--pr",
                "1",
                "--repo",
                "o/r",
                "--branch",
                "b",
                "--head-sha",
                "abc",
                "--explicit",
            ])
            .unwrap_or_else(|e| panic!("{phase}: {e}"));
            assert_eq!(h.args.phase, want);
            assert!(h.args.explicit);
        }
        assert!(Harness::try_parse_from(["x", "--phase", "merge", "--pr", "1"]).is_err());
    }
}
