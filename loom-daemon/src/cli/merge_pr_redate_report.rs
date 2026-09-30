//! `loom-daemon merge-pr redate-report` (#9746): which required checks and
//! which paths forced the #8508 re-dates, from the trailers `redate-checks`
//! writes into each re-date commit's body.
//!
//! Read-only: one local `git log`, no fetch, no forge call, no write. See
//! `loom_daemon::merge_pr::redate::report` for what is counted and why `main`
//! is a sufficient source on a merge-commit repo.
//!
//! | outcome | stdout | exit |
//! |---|---|---|
//! | report produced (possibly empty) | the table, or JSON with `--json` | 0 |
//! | bad `--since`, or `git log` failed | reason on stderr | 1 |

use anyhow::{anyhow, Result};
use loom_daemon::merge_pr::redate::report::{
    aggregate, count_other_redates, parse_log, render_text, run_git_log,
};

#[derive(clap::Args)]
pub(crate) struct RedateReportArgs {
    /// How far back to look: a bare number of seconds or `<n>[smhd]`.
    #[arg(long, value_name = "DURATION", default_value = "24h")]
    since: String,

    /// The ref whose history is read. Not fetched — run `git fetch` first if
    /// it may be behind.
    #[arg(long = "ref", value_name = "REF", default_value = "origin/main")]
    git_ref: String,

    /// Emit the report as JSON instead of the table.
    #[arg(long)]
    json: bool,
}

impl RedateReportArgs {
    pub(crate) fn run(self) -> Result<()> {
        let window = loom_daemon::health::parse_since(&self.since).map_err(|e| anyhow!(e))?;
        let since = chrono::Utc::now()
            - chrono::TimeDelta::from_std(window).map_err(|e| anyhow!("--since: {e}"))?;
        let since_str = since.to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
        let dir = loom_daemon::repo_root::find_repo_root_from_cwd()
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_default();
        let raw = run_git_log(&dir, &self.git_ref, since).map_err(|e| anyhow!(e))?;
        let report =
            aggregate(&parse_log(&raw), count_other_redates(&raw), &self.git_ref, &since_str);
        if self.json {
            println!("{}", serde_json::to_string_pretty(&report)?);
        } else {
            print!("{}", render_text(&report));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::RedateReportArgs;
    use clap::Parser;

    #[derive(Parser)]
    struct Harness {
        #[command(flatten)]
        args: RedateReportArgs,
    }

    #[test]
    fn defaults_are_24h_on_origin_main_as_text() {
        let h = Harness::try_parse_from(["x"]).expect("parses with no flags");
        assert_eq!(h.args.since, "24h");
        assert_eq!(h.args.git_ref, "origin/main");
        assert!(!h.args.json);
        let h = Harness::try_parse_from(["x", "--since", "7d", "--ref", "main", "--json"])
            .expect("parses");
        assert_eq!((h.args.since.as_str(), h.args.git_ref.as_str()), ("7d", "main"));
        assert!(h.args.json);
    }
}
