//! Retired `merge-pr` verbs kept as hidden, deprecated answers (#12063 slice 1).
//!
//! The required-check freshness guard (`stale-checks`, #8248), its re-date
//! remedy (`redate-checks`, #8508) and the CI-run conclusion gate
//! (`ci-result`, #10444) are deleted. The current `merge-pr.sh` no longer
//! calls them, but a host can briefly run an OLDER `merge-pr.sh` (a repo whose
//! `.loom/scripts` has not been resynced yet) against THIS daemon. That script
//! fails closed on any answer but its clean sentinel, so an unknown subcommand
//! would refuse every merge until the resync. Each verb therefore accepts the
//! arguments the old caller passes and answers in the old contract.
//!
//! | verb | answer |
//! |---|---|
//! | `stale-checks` | `LOOM-STALE-CHECKS-CLEAN`, exit 0 (freshness is no longer loom's to enforce) |
//! | `ci-result` | the `required-checks` gate (#10688), in `ci-result`'s old contract (below) |
//! | `redate-checks` | nothing on stdout (reason on stderr), exit 1 |
//!
//! `ci-result` is NOT a fixed CLEAN. An installed `merge-pr.sh` from 0.19.763
//! up to 0.19.1077 predates the `required-checks` gate (first shipped in
//! 0.19.1078), so on its plain (non-`--auto`) path `ci-result` was its only
//! loom-side CI verification. A fixed CLEAN would leave a red or unfinished
//! head with only branch protection in front of it, which a bypass-capable
//! (admin) token walks through. The stub therefore evaluates the required
//! contexts at `--head-sha` against the PR's base branch and maps the verdict
//! onto what that caller understands:
//!
//! | required-checks verdict | stdout | exit | old caller |
//! |---|---|---|---|
//! | every required context green (or none required) | `LOOM-CI-RESULT-CLEAN` | 0 | merges |
//! | a required context concluded red | the refusal naming it | 1 | refuses (exit 1) |
//! | forge unreadable | `ci-result could not query the forge: …` | 2 | retries, then refuses |
//! | a required context missing or pending | `LOOM-CI-RESULT-UNVERIFIED …` | 3 | holds (exit 5) |
//!
//! `redate-checks` is only reached after a `stale-checks` refusal, which can
//! no longer happen; exit 1 leaves the old caller's refusal path unchanged
//! rather than pushing anything. Delete this file once every fleet host has
//! resynced past slice 1 (slice 5 replaces the verb family).

use anyhow::Result;
use loom_daemon::merge_pr::checks_failure::required::{
    assess, hold_reason, live_inputs, pr_base_ref, refusal, Verdict,
};

/// `ci-result`'s old clean sentinel.
const CI_CLEAN: &str = "LOOM-CI-RESULT-CLEAN";
/// `ci-result`'s old no-verdict prefix (exit 3; the old caller holds).
const CI_UNVERIFIED: &str = "LOOM-CI-RESULT-UNVERIFIED";
/// `ci-result`'s old forge-failure prefix (exit 2; the old caller retries).
const CI_UNREADABLE: &str = "ci-result could not query the forge:";

/// One line on stderr naming the retirement, so a caller that still asks is
/// visible in its own log.
fn warn(verb: &str) {
    eprintln!(
        "loom-daemon merge-pr {verb}: retired by #12063 slice 1 (deprecated); \
answered for an older merge-pr.sh only. Resync .loom/scripts to stop calling it."
    );
}

/// Whatever the old caller passes; none of it is read.
#[derive(clap::Args)]
pub(crate) struct RetiredArgs {
    #[arg(trailing_var_arg = true, allow_hyphen_values = true, hide = true)]
    _ignored: Vec<String>,
}

/// The argv an older `merge-pr.sh` passes to `ci-result`:
/// `--pr N --repo OWNER/REPO --head-sha SHA`.
#[derive(clap::Args)]
pub(crate) struct CiResultArgs {
    #[arg(long, value_name = "N")]
    pr: String,

    #[arg(long, value_name = "OWNER/REPO", default_value = "")]
    repo: String,

    #[arg(long, value_name = "SHA")]
    head_sha: String,

    /// The branch whose required contexts apply; read from the PR when
    /// absent (an older caller never passes it).
    #[arg(long, value_name = "REF", hide = true)]
    base_ref: Option<String>,

    /// Assess a `required-checks --from-stdin` document instead of the forge
    /// (the fixture seam).
    #[arg(long, hide = true)]
    from_stdin: bool,

    #[arg(trailing_var_arg = true, allow_hyphen_values = true, hide = true)]
    _ignored: Vec<String>,
}

/// `ci-result`'s answer, `(stdout line, exit code)`, for a `required-checks`
/// verdict on `sha` (`Err` = the forge could not be read).
pub(crate) fn ci_result_answer(
    pr: &str,
    sha: &str,
    verdict: Result<Verdict, String>,
) -> (String, i32) {
    match verdict {
        Ok(Verdict::Clean) => (CI_CLEAN.to_string(), 0),
        // No override list is ever passed here, so a bypass cannot occur; a
        // red context is a refusal whichever list it landed in.
        Ok(Verdict::Bypassed(red) | Verdict::Refuse { red, .. }) => (refusal(pr, sha, &red), 1),
        Ok(Verdict::Hold { waiting, .. }) => {
            (format!("{CI_UNVERIFIED} {}", hold_reason(sha, &waiting)), 3)
        }
        Err(why) => (format!("{CI_UNREADABLE} {why}"), 2),
    }
}

impl CiResultArgs {
    /// `ci-result`: the `required-checks` gate in `ci-result`'s old contract.
    pub(crate) fn run(self) -> Result<()> {
        warn("ci-result");
        let inputs = if self.from_stdin {
            super::merge_pr_required_checks::read_stdin_inputs()?
        } else {
            match self.base_ref.clone() {
                Some(b) => Ok(b),
                None => pr_base_ref(&self.repo, &self.pr),
            }
            .and_then(|base| live_inputs(&self.repo, &base, &self.head_sha))
        };
        let verdict =
            inputs.map(|(required, runs, statuses)| assess(&required, &runs, &statuses, &[]));
        let (line, code) = ci_result_answer(&self.pr, &self.head_sha, verdict);
        println!("{line}");
        if code != 0 {
            std::process::exit(code);
        }
        Ok(())
    }
}

impl RetiredArgs {
    /// `stale-checks`: always fresh.
    pub(crate) fn run_stale_checks(self) -> Result<()> {
        warn("stale-checks");
        println!("LOOM-STALE-CHECKS-CLEAN");
        Ok(())
    }

    /// `redate-checks`: never pushes.
    pub(crate) fn run_redate_checks(self) -> Result<()> {
        warn("redate-checks");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::{ci_result_answer, CiResultArgs, RetiredArgs};
    use clap::Parser;
    use loom_daemon::merge_pr::checks_failure::required::Verdict;

    #[derive(clap::Parser)]
    struct Probe {
        #[command(flatten)]
        args: RetiredArgs,
    }

    #[derive(clap::Parser)]
    struct CiProbe {
        #[command(flatten)]
        args: CiResultArgs,
    }

    /// An older `merge-pr.sh`'s exact argv for each retired verb still parses,
    /// so the answer reaches it instead of a clap usage error (exit 2).
    #[test]
    fn every_old_callers_argv_parses() {
        for argv in [
            &[
                "x",
                "--pr",
                "1",
                "--repo",
                "o/r",
                "--head-sha",
                "abc",
                "--base-ref",
                "main",
            ][..],
            &[
                "x",
                "--pr",
                "1",
                "--repo",
                "o/r",
                "--branch",
                "b",
                "--expected-head-sha",
                "abc",
            ][..],
            &["x"][..],
        ] {
            assert!(Probe::try_parse_from(argv).is_ok(), "{argv:?}");
        }
        // `ci-result`'s caller (0.19.763 .. slice 1) passes exactly this.
        let ci = ["x", "--pr", "1", "--repo", "o/r", "--head-sha", "abc"];
        assert!(CiProbe::try_parse_from(ci).is_ok());
    }

    fn named(c: &str, d: &str) -> Vec<(String, String)> {
        vec![(c.to_string(), d.to_string())]
    }

    /// `ci-result` answers in its old contract, and only a green head is
    /// CLEAN: an older plain-path `merge-pr.sh` had no other loom-side CI gate.
    #[test]
    fn ci_result_maps_required_checks_onto_the_old_contract() {
        assert_eq!(
            ci_result_answer("7", "abc", Ok(Verdict::Clean)),
            ("LOOM-CI-RESULT-CLEAN".to_string(), 0)
        );
        let red = Verdict::Refuse {
            red: named("Rust Tests", "failure"),
            bypassed: vec![],
        };
        let (line, code) = ci_result_answer("7", "abc", Ok(red));
        assert_eq!(code, 1);
        assert!(line.starts_with("Merge blocked: PR #7"), "{line}");
        assert!(line.contains("`Rust Tests` (failure)"), "{line}");
        let (line, code) =
            ci_result_answer("7", "abc", Ok(Verdict::Bypassed(named("X", "failure"))));
        assert_eq!(code, 1, "{line}");
        let hold = Verdict::Hold {
            waiting: named("Rust Tests", "in_progress"),
            bypassed: vec![],
        };
        let (line, code) = ci_result_answer("7", "abc", Ok(hold));
        assert_eq!(code, 3);
        assert!(line.starts_with("LOOM-CI-RESULT-UNVERIFIED "), "{line}");
        let (line, code) = ci_result_answer("7", "abc", Err("HTTP 502".into()));
        assert_eq!(code, 2);
        assert_eq!(line, "ci-result could not query the forge: HTTP 502");
    }
}
