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
//!
//! # The input-scoped predicate (#8919)
//!
//! The verdict is decided by
//! [`loom_daemon::merge_pr::stale_checks::assess_scoped`]: when the base a
//! check actually tested (`B`), the base-move diff (`D`) and the PR delta (`P`)
//! are all available, staleness is a question about *which files* the move
//! touched, not about *when* the run happened. Whenever any of the three is
//! missing for a context, that context falls back to #8248's `started_at` rule
//! and a `Warning:` naming it goes to **stderr** — stdout still carries nothing
//! but the sentinel or the refusal, so the exit-code contract above is
//! unchanged.
//!
//! The `--from-stdin` payload accepts the new evidence as OPTIONAL fields
//! (`pr_files`, `base_moves`); a payload without them behaves exactly as before.

use anyhow::Result;
use loom_daemon::merge_pr::stale_checks::evidence::{
    strip_validated_restamps, to_file_set, ChangedFile,
};
use loom_daemon::merge_pr::stale_checks::inputs::{BaseMove, ScopedEvidence};
use loom_daemon::merge_pr::stale_checks::{
    assess_scoped, stale_inputs_message, unknown_message, Verdict, CLEAN,
};
use std::collections::BTreeMap;
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
                    let (verdict, warnings) = assess_scoped(
                        inputs.base_tip,
                        &inputs.required,
                        &inputs.runs,
                        inputs.scoped.as_ref(),
                    );
                    warn_all(&warnings);
                    (inputs.tip_sha, verdict)
                }
                Err(why) => (String::new(), Verdict::Unknown(why)),
            }
        } else {
            match loom_daemon::merge_pr::stale_checks::fetch::live_inputs(
                &self.repo,
                &self.pr,
                &self.base_ref,
                &self.head_sha,
            ) {
                Ok(inputs) => {
                    // On STDERR, always: stdout carries the CLEAN sentinel and
                    // nothing else (callers compare it for exact equality), and
                    // a degradation the operator cannot see is how a fail-open
                    // ships unnoticed (#8844).
                    warn_all(&inputs.notices);
                    let (verdict, warnings) = assess_scoped(
                        inputs.base_tip,
                        &inputs.required,
                        &inputs.runs,
                        inputs.scoped.as_ref(),
                    );
                    warn_all(&warnings);
                    (inputs.tip_sha.clone(), verdict)
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
            Verdict::StaleInputs {
                check,
                tested_base,
                reason,
            } => {
                println!(
                    "{}",
                    stale_inputs_message(&self.pr, &check, &tested_base, &reason, &tip_sha)
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
                    actions_run_id: None,
                    actions_job_id: None,
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
            scoped: scoped_from_json(&v),
        })
    }
}

/// The offline twin of [`fetch::LiveInputs`] — what `--from-stdin` parses.
struct StdinInputs {
    tip_sha: String,
    base_tip: chrono::DateTime<chrono::Utc>,
    required: Vec<String>,
    runs: Vec<loom_daemon::merge_pr::stale_checks::CheckRun>,
    scoped: Option<ScopedEvidence>,
}

/// Print every degradation on stderr, so stdout keeps carrying only the
/// sentinel or the refusal.
fn warn_all(notices: &[String]) {
    for notice in notices {
        eprintln!("Warning: {notice}");
    }
}

/// The OPTIONAL input-scoped evidence (#8919) of a `--from-stdin` payload:
///
/// ```json
/// {
///   "pr_files":  [{"filename": "a.rs", "status": "modified"}],
///   "base_moves": {
///     "File Size Ratchet": {
///       "tested_base": "803f0c7d",
///       "files": [{"filename": "scripts/file-size-baseline.txt",
///                  "status": "modified", "patch": "@@ …"}]
///     }
///   },
///   "fallbacks": {"Some Check": "why it has no evidence"}
/// }
/// ```
///
/// Absent `pr_files` AND `base_moves` ⇒ `None`, i.e. an old payload assesses by
/// the time rule exactly as before. The same
/// [`strip_validated_restamps`] the live path applies runs here, so a suite can
/// drive the restamp validation end-to-end.
fn scoped_from_json(v: &serde_json::Value) -> Option<ScopedEvidence> {
    if v.get("pr_files").is_none() && v.get("base_moves").is_none() {
        return None;
    }
    let pr_files = changed_files(v.get("pr_files"));
    let mut base_moves: BTreeMap<String, BaseMove> = BTreeMap::new();
    if let Some(obj) = v.get("base_moves").and_then(|m| m.as_object()) {
        for (ctx, mv) in obj {
            let files = changed_files(mv.get("files"));
            base_moves.insert(
                ctx.clone(),
                BaseMove {
                    tested_base: mv
                        .get("tested_base")
                        .and_then(|s| s.as_str())
                        .unwrap_or("<unknown>")
                        .to_string(),
                    files: to_file_set(&strip_validated_restamps(&files)),
                },
            );
        }
    }
    let mut fallbacks: BTreeMap<String, String> = BTreeMap::new();
    if let Some(obj) = v.get("fallbacks").and_then(|m| m.as_object()) {
        for (ctx, why) in obj {
            fallbacks.insert(ctx.clone(), why.as_str().unwrap_or("no reason given").to_string());
        }
    }
    Some(ScopedEvidence {
        pr_delta: to_file_set(&pr_files),
        base_moves,
        fallbacks,
    })
}

fn changed_files(v: Option<&serde_json::Value>) -> Vec<ChangedFile> {
    v.and_then(|f| f.as_array())
        .map(|arr| {
            arr.iter()
                .map(|f| ChangedFile {
                    path: f
                        .get("filename")
                        .and_then(|s| s.as_str())
                        .unwrap_or_default()
                        .to_string(),
                    status: f
                        .get("status")
                        .and_then(|s| s.as_str())
                        .unwrap_or("modified")
                        .to_string(),
                    previous_filename: f
                        .get("previous_filename")
                        .and_then(|s| s.as_str())
                        .map(String::from),
                    patch: f.get("patch").and_then(|s| s.as_str()).map(String::from),
                })
                .collect()
        })
        .unwrap_or_default()
}
