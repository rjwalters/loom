//! `loom-daemon merge-pr required-checks` (#10688): the synchronous merge
//! path's required-context conclusion gate. Logic and contract:
//! [`loom_daemon::merge_pr::checks_failure::required`].
//!
//! | outcome | stdout | exit |
//! |---|---|---|
//! | every required context green | `LOOM-REQUIRED-CHECKS-CLEAN` | 0 |
//! | red only on `--allow-red-required` contexts | `LOOM-REQUIRED-CHECKS-BYPASSED …` (audit comment posted) | 0 |
//! | an unallowed required context concluded red | the refusal naming it | 1 |
//! | the forge could not be read | `LOOM-REQUIRED-CHECKS-UNREADABLE …` | 2 |
//! | a required context missing or pending | `LOOM-REQUIRED-CHECKS-HOLD …` | 3 (caller holds, exit 5) |
//!
//! `--dry-run` and `--from-stdin` never post the audit comment. `--from-stdin`
//! reads `{"required": [...], "check_runs": [{name, status, conclusion,
//! started_at}], "statuses": <commits/{sha}/status payload>}` and assesses
//! offline (the fixture seam); a missing `statuses` key reads as none.

use anyhow::Result;
use loom_daemon::merge_pr::checks_failure::required::{
    assess, bypass_comment, hold_reason, list, live_inputs, parse_check_runs, parse_statuses,
    refusal, Verdict, BYPASSED, CLEAN, HOLD, UNREADABLE,
};
use serde_json::Value;
use std::io::Read;

#[derive(clap::Args)]
pub(crate) struct RequiredChecksArgs {
    /// The PR number.
    #[arg(long, value_name = "N")]
    pr: String,

    /// The repository as owner/repo.
    #[arg(long, value_name = "OWNER/REPO", default_value = "")]
    repo: String,

    /// The PR head SHA being merged.
    #[arg(long, value_name = "SHA")]
    head_sha: String,

    /// The branch the merge lands on (whose required contexts apply).
    #[arg(long, value_name = "REF", default_value = "main")]
    base_ref: String,

    /// Override a red conclusion of this required context (repeatable).
    #[arg(long, value_name = "CONTEXT")]
    allow_red_required: Vec<String>,

    /// Report only; post no audit comment.
    #[arg(long)]
    dry_run: bool,

    /// Assess a JSON document from stdin instead of the forge.
    #[arg(long)]
    from_stdin: bool,
}

fn unreadable(why: &str) -> ! {
    println!("{UNREADABLE} {why}");
    std::process::exit(2);
}

impl RequiredChecksArgs {
    pub(crate) fn run(self) -> Result<()> {
        let (required, runs, statuses) = if self.from_stdin {
            let mut raw = String::new();
            std::io::stdin().read_to_string(&mut raw)?;
            let doc: Value = serde_json::from_str(&raw)?;
            let required: Vec<String> = doc
                .get("required")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(Value::as_str)
                        .map(String::from)
                        .collect()
                })
                .unwrap_or_default();
            let runs = parse_check_runs(doc.get("check_runs").unwrap_or(&Value::Array(vec![])))
                .unwrap_or_else(|e| unreadable(&e));
            let statuses = match doc.get("statuses") {
                Some(s) => parse_statuses(s).unwrap_or_else(|e| unreadable(&e)),
                None => Vec::new(),
            };
            (required, runs, statuses)
        } else {
            live_inputs(&self.repo, &self.base_ref, &self.head_sha)
                .unwrap_or_else(|e| unreadable(&e))
        };
        let post = !self.dry_run && !self.from_stdin;
        match assess(&required, &runs, &statuses, &self.allow_red_required) {
            Verdict::Clean => println!("{CLEAN}"),
            Verdict::Bypassed(bypassed) => {
                println!(
                    "{BYPASSED} PR #{}: red required context(s) {} overridden by --allow-red-required (#10688).",
                    self.pr,
                    list(&bypassed)
                );
                if post {
                    let body = bypass_comment(&self.pr, &self.head_sha, &bypassed);
                    if let Err(e) = loom_daemon::forge_comment::post_comment(
                        "gh", None, &self.repo, &self.pr, true, &body,
                    ) {
                        eprintln!("Warning: could not post the --allow-red-required audit comment on PR #{}: {e}", self.pr);
                    }
                }
            }
            Verdict::Refuse { red, .. } => {
                println!("{}", refusal(&self.pr, &self.head_sha, &red));
                std::process::exit(1);
            }
            Verdict::Hold { waiting, .. } => {
                println!("{HOLD} {}", hold_reason(&self.head_sha, &waiting));
                std::process::exit(3);
            }
        }
        Ok(())
    }
}
