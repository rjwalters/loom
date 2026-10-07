//! `loom-daemon merge-pr ci-result` (#10444): refuse a merge whose head's latest
//! `CI` workflow run did not conclude `success`. Logic and contract:
//! [`loom_daemon::merge_pr::ci_result`].
//!
//! | outcome | stdout | exit |
//! |---|---|---|
//! | exact-head run concluded `success` | `LOOM-CI-RESULT-CLEAN` | 0 |
//! | repository defines no `CI` workflow | `LOOM-CI-RESULT-NO-CI-WORKFLOW …` | 0 |
//! | no run for this head / still running | `LOOM-CI-RESULT-UNVERIFIED …` | 3 (caller holds, #10567) |
//! | non-success conclusion | the refusal naming cancelled/failed jobs | 1 |
//! | forge query failed | the reason | 2 |
//!
//! `--from-stdin` reads `{"runs": <actions/runs payload>, "jobs": <jobs payload>,
//! "workflows": <actions/workflows payload>}` and assesses offline (the fixture
//! seam, and a debug facility); a missing `workflows` key reads as a failed
//! workflow-list query.

use anyhow::Result;
use loom_daemon::merge_pr::ci_result::{assess, Verdict, CLEAN, NO_CI_WORKFLOW, UNVERIFIED};
use serde_json::Value;
use std::io::Read;

#[derive(clap::Args)]
pub(crate) struct CiResultArgs {
    /// The PR number.
    #[arg(long, value_name = "N")]
    pr: String,

    /// The repository as owner/repo.
    #[arg(long, value_name = "OWNER/REPO", default_value = "")]
    repo: String,

    /// The PR head SHA being merged.
    #[arg(long, value_name = "SHA")]
    head_sha: String,

    /// Assess a JSON `{"runs":…,"jobs":…}` document from stdin instead of the forge.
    #[arg(long)]
    from_stdin: bool,
}

fn gh_api(path: &str) -> Result<Value, String> {
    let out = loom_daemon::merge_pr::ci_result::gh_get(path)?;
    serde_json::from_str(&out).map_err(|e| format!("unparseable response from {path}: {e}"))
}

impl CiResultArgs {
    pub(crate) fn run(self) -> Result<()> {
        let verdict = if self.from_stdin {
            let mut raw = String::new();
            std::io::stdin().read_to_string(&mut raw)?;
            let doc: Value = serde_json::from_str(&raw)?;
            let jobs = doc.get("jobs").cloned().unwrap_or(Value::Null);
            let workflows = doc.get("workflows").cloned();
            assess(
                &self.pr,
                &self.head_sha,
                doc.get("runs").unwrap_or(&Value::Null),
                || workflows.ok_or_else(|| "no `workflows` in the stdin document".to_string()),
                |_| Ok(jobs),
            )
        } else {
            let runs = match gh_api(&format!(
                "repos/{}/actions/runs?head_sha={}&per_page=100",
                self.repo, self.head_sha
            )) {
                Ok(v) => v,
                Err(e) => {
                    println!("ci-result could not query the forge: {e}");
                    std::process::exit(2);
                }
            };
            let repo = self.repo.clone();
            assess(
                &self.pr,
                &self.head_sha,
                &runs,
                || gh_api(&format!("repos/{repo}/actions/workflows?per_page=100")),
                |id| {
                    gh_api(&format!(
                        "repos/{repo}/actions/runs/{id}/jobs?per_page=100&filter=latest"
                    ))
                },
            )
        };
        match verdict {
            Verdict::Clean => println!("{CLEAN}"),
            Verdict::NoCiWorkflow(why) => println!("{NO_CI_WORKFLOW} {why}"),
            Verdict::Unverified(why) => {
                println!("{UNVERIFIED} {why}");
                std::process::exit(3);
            }
            Verdict::Refuse(msg) => {
                println!("{msg}");
                std::process::exit(1);
            }
            Verdict::Unreadable(why) => {
                println!("ci-result could not query the forge: {why}");
                std::process::exit(2);
            }
        }
        Ok(())
    }
}
