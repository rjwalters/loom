//! `loom-daemon merge-pr stale-checks` (#8248, slice 3 of the merge-pr port).
//!
//! # Passing requires a POSITIVE signal
//!
//! Same contract as [`super::merge_pr_labels`], for the same reason: porting a
//! sourced shell function into a subprocess introduces "the binary can be
//! missing, unreadable, or an older install that does not know this
//! subcommand", and the obvious exit-code mapping is fail-open.
//!
//! | outcome | stdout | exit |
//! |---|---|---|
//! | every required check fresh | [`loom_daemon::merge_pr::stale_checks::CLEAN`] | 0 |
//! | a stale green required check | the refusal (check + both timestamps) | 1 |
//! | could not determine | the refusal (fail-closed reason) | 2 |
//!
//! Exit 2 must refuse the merge, never skip: a freshness guard that cannot
//! run is a guard reporting nothing, and "could not check" reading as
//! "checked, fine" is the exact failure mode #8248 documents.
//!
//! The one lookup failure that is NOT an unknown is the plan gate of
//! [`loom_daemon::merge_pr::stale_checks::fetch::is_plan_gated`] — GitHub
//! refusing to serve rulesets/branch protection on a private repository whose
//! plan does not include them. There, "no required checks exist" is a fact,
//! not a guess, and the run reports it as a `Warning:` on **stderr** while
//! stdout keeps carrying nothing but the sentinel (#8844).
//!
//! `--from-stdin` reads the same evidence the live path gathers, as a JSON
//! object, and assesses it offline — the deterministic seam the retained suite
//! drives (and a debug facility: paste a real PR's inputs, see the verdict).

use anyhow::Result;
use loom_daemon::merge_pr::stale_checks::{assess, unknown_message, Verdict, CLEAN};
use std::io::Read;

#[derive(clap::Args)]
pub(crate) struct StaleChecksArgs {
    /// The PR number, for the refusal message.
    #[arg(long, value_name = "N")]
    pr: String,

    /// The repository as owner/repo.
    #[arg(long, value_name = "OWNER/REPO")]
    repo: String,

    /// The PR head SHA whose check runs are the evidence.
    #[arg(long, value_name = "SHA")]
    head_sha: String,

    /// The PR's base branch — the branch this merge will land ON. Its tip's
    /// commit time is what green results must not predate.
    #[arg(long, value_name = "REF")]
    base_ref: String,

    /// Read `{"tip_sha", "base_tip", "required": [...], "check_runs": [...]}`
    /// from stdin instead of querying the forge (suites/debug).
    #[arg(long)]
    from_stdin: bool,
}

impl StaleChecksArgs {
    pub(crate) fn run(self) -> Result<()> {
        // Display-only: named in the refusal so the operator can correlate it
        // with the merge that actually matters.
        let (tip_sha, verdict) = if self.from_stdin {
            match self.stdin_inputs() {
                Ok(inputs) => {
                    (inputs.tip_sha, assess(inputs.base_tip, &inputs.required, &inputs.runs))
                }
                Err(why) => (String::new(), Verdict::Unknown(why)),
            }
        } else {
            match loom_daemon::merge_pr::stale_checks::fetch::live_inputs(
                &self.repo,
                &self.base_ref,
                &self.head_sha,
            ) {
                Ok(inputs) => {
                    // On STDERR, always: stdout carries the CLEAN sentinel and
                    // nothing else (callers compare it for exact equality), and
                    // a degradation the operator cannot see is how a fail-open
                    // ships unnoticed (#8844).
                    for notice in &inputs.notices {
                        eprintln!("Warning: {notice}");
                    }
                    (
                        inputs.tip_sha.clone(),
                        assess(inputs.base_tip, &inputs.required, &inputs.runs),
                    )
                }
                Err(why) => (String::new(), Verdict::Unknown(why)),
            }
        };
        let tip_sha = if tip_sha.is_empty() {
            "<unknown>".to_string()
        } else {
            tip_sha
        };

        match verdict {
            Verdict::Fresh => {
                println!("{CLEAN}");
                std::process::exit(0);
            }
            Verdict::Stale {
                check,
                started_at,
                base_tip,
            } => {
                println!(
                    "{}",
                    loom_daemon::merge_pr::stale_checks::stale_message(
                        &self.pr, &check, started_at, base_tip, &tip_sha
                    )
                );
                std::process::exit(1);
            }
            Verdict::Unknown(why) => {
                println!("{}", unknown_message(&self.pr, &why));
                std::process::exit(2);
            }
        }
    }

    /// The `--from-stdin` payload: the same evidence [`fetch::live_inputs`]
    /// gathers, supplied offline (suites/debug).
    fn stdin_inputs(&self) -> std::result::Result<StdinInputs, String> {
        use chrono::{DateTime, Utc};
        use loom_daemon::merge_pr::stale_checks::CheckRun;

        let mut buf = String::new();
        std::io::stdin()
            .read_to_string(&mut buf)
            .map_err(|e| format!("could not read stdin: {e}"))?;
        let v: serde_json::Value =
            serde_json::from_str(&buf).map_err(|e| format!("stdin is not valid JSON: {e}"))?;

        let base_tip = v
            .get("base_tip")
            .and_then(|s| s.as_str())
            .ok_or("stdin payload has no base_tip")?;
        let base_tip = DateTime::parse_from_rfc3339(base_tip)
            .map_err(|e| format!("base_tip is not RFC3339: {e}"))?
            .with_timezone(&Utc);
        let required: Vec<String> = v
            .get("required")
            .and_then(|r| r.as_array())
            .ok_or("stdin payload has no required array")?
            .iter()
            .filter_map(|c| c.as_str().map(String::from))
            .collect();
        let runs: Vec<CheckRun> = v
            .get("check_runs")
            .and_then(|r| r.as_array())
            .ok_or("stdin payload has no check_runs array")?
            .iter()
            .map(|r| {
                let started_at = r
                    .get("started_at")
                    .and_then(|s| s.as_str())
                    .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
                    .map(|d| d.with_timezone(&Utc));
                CheckRun {
                    name: r
                        .get("name")
                        .and_then(|s| s.as_str())
                        .unwrap_or_default()
                        .to_string(),
                    status: r
                        .get("status")
                        .and_then(|s| s.as_str())
                        .unwrap_or_default()
                        .to_string(),
                    conclusion: r
                        .get("conclusion")
                        .and_then(|s| s.as_str())
                        .map(String::from),
                    started_at,
                }
            })
            .collect();
        let tip_sha = v
            .get("tip_sha")
            .and_then(|s| s.as_str())
            .unwrap_or("<unknown>")
            .to_string();
        Ok(StdinInputs {
            tip_sha,
            base_tip,
            required,
            runs,
        })
    }
}

/// The offline twin of [`fetch::LiveInputs`] — what `--from-stdin` parses.
struct StdinInputs {
    tip_sha: String,
    base_tip: chrono::DateTime<chrono::Utc>,
    required: Vec<String>,
    runs: Vec<loom_daemon::merge_pr::stale_checks::CheckRun>,
}
