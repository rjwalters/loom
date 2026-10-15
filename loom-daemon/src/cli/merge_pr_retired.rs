//! Retired `merge-pr` verbs kept as hidden, deprecated answers (#12063 slice 1).
//!
//! The required-check freshness guard (`stale-checks`, #8248), its re-date
//! remedy (`redate-checks`, #8508) and the CI-run conclusion gate
//! (`ci-result`, #10444) are deleted: required checks at the head are
//! GitHub's to enforce. The current `merge-pr.sh` no longer calls them, but a
//! host can briefly run an OLDER `merge-pr.sh` (a repo whose `.loom/scripts`
//! has not been resynced yet) against THIS daemon. That script fails closed on
//! any answer but its clean sentinel, so an unknown subcommand would refuse
//! every merge until the resync. Each verb therefore accepts whatever
//! arguments the old caller passes and answers the way the deleted check now
//! always would: nothing for loom to enforce.
//!
//! | verb | stdout | exit |
//! |---|---|---|
//! | `stale-checks` | `LOOM-STALE-CHECKS-CLEAN` | 0 |
//! | `ci-result` | `LOOM-CI-RESULT-CLEAN` | 0 |
//! | `redate-checks` | (nothing; reason on stderr) | 1 |
//!
//! `redate-checks` is only reached after a `stale-checks` refusal, which can
//! no longer happen; exit 1 leaves the old caller's refusal path unchanged
//! rather than pushing anything. Delete this file once every fleet host has
//! resynced past slice 1 (slice 5 replaces the verb family).

use anyhow::Result;

/// One line on stderr naming the retirement, so a caller that still asks is
/// visible in its own log.
fn warn(verb: &str) {
    eprintln!(
        "loom-daemon merge-pr {verb}: retired by #12063 slice 1 (deprecated no-op); \
required checks at the head are GitHub's to enforce. Resync .loom/scripts to stop calling it."
    );
}

/// Whatever the old caller passes; none of it is read.
#[derive(clap::Args)]
pub(crate) struct RetiredArgs {
    #[arg(trailing_var_arg = true, allow_hyphen_values = true, hide = true)]
    _ignored: Vec<String>,
}

impl RetiredArgs {
    /// `stale-checks`: always fresh.
    pub(crate) fn run_stale_checks(self) -> Result<()> {
        warn("stale-checks");
        println!("LOOM-STALE-CHECKS-CLEAN");
        Ok(())
    }

    /// `ci-result`: always clean.
    pub(crate) fn run_ci_result(self) -> Result<()> {
        warn("ci-result");
        println!("LOOM-CI-RESULT-CLEAN");
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
    use super::RetiredArgs;
    use clap::Parser;

    #[derive(clap::Parser)]
    struct Probe {
        #[command(flatten)]
        args: RetiredArgs,
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
            &["x", "--pr", "1", "--repo", "o/r", "--head-sha", "abc"][..],
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
    }
}
