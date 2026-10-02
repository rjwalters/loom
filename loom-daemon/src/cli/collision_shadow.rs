//! `loom-daemon collision-shadow capture | evaluate` (#9787) — the
//! prospective shadow-study surface. Capture reads a dispatch-tick
//! candidates snapshot and emits advisory capture records (never touching
//! scheduling); evaluate joins frozen captures with #9786 outcomes and
//! reports the frozen-policy comparison with the pre-registered verdict.

use anyhow::{bail, Context as _, Result};
use std::path::PathBuf;

use loom_daemon::collision_evidence::OutcomeRecord;
use loom_daemon::collision_shadow::{
    capture, capture_id, evaluate, write_tick_records, CandidateSnapshotTick, StudyBudgets,
};

#[derive(clap::Subcommand)]
pub(crate) enum CollisionShadowCommand {
    /// Capture advisory records for one tick's candidate snapshot.
    Capture {
        /// Snapshot JSON: { tick_id, admission_policy, captured_at,
        /// candidates: [ { issue, curator_affected_files, ... } ] }.
        #[arg(long, value_name = "PATH")]
        snapshot: PathBuf,

        /// Output JSONL path (one capture record per line, id-stamped).
        #[arg(long, value_name = "PATH")]
        out: PathBuf,

        /// Capture budget overrides (max_pairs_per_tick, max_pairs_total,
        /// study_stopped).
        #[arg(long, value_name = "PATH")]
        budgets: Option<PathBuf>,
    },

    /// Run capture against the **live** ready queue: lists open `loom:issue`
    /// rows through the ETag-cached forge listing (a 304 costs nothing in
    /// steady state), builds candidate snapshots, and writes one id-stamped
    /// JSONL file per tick.
    ///
    /// v1 records the cohort skeleton — issue numbers, claim state,
    /// eligibility — without fetching issue bodies, so per-feature Curator
    /// Jaccard is recorded as missing (honest absence, per the #9787
    /// contract). Body-derived feature enrichment is a follow-up.
    CaptureLive {
        /// `OWNER/REPO` to capture (default: resolved from the cwd's remote).
        #[arg(long, value_name = "OWNER/REPO")]
        repo: Option<String>,

        /// Output directory for per-tick JSONL files.
        #[arg(long, value_name = "DIR", default_value = ".loom/shadow")]
        out_dir: PathBuf,

        /// Capture budget overrides.
        #[arg(long, value_name = "PATH")]
        budgets: Option<PathBuf>,
    },

    /// Evaluate frozen policies over captured records + recorded outcomes.
    Evaluate {
        /// JSONL of capture records (from `capture`).
        #[arg(long, value_name = "PATH")]
        captures: PathBuf,

        /// JSONL of #9786 outcome records.
        #[arg(long, value_name = "PATH")]
        outcomes: PathBuf,

        /// Frozen thresholds: {"curator": t, "retrieval": t}.
        #[arg(long, value_name = "PATH")]
        policies: PathBuf,

        /// Pre-registered budgets: {"min_positives_for_verdict": n,
        /// "max_flag_rate": f}.
        #[arg(long, value_name = "PATH")]
        budgets: PathBuf,

        /// Where to write the evaluation report JSON.
        #[arg(long, value_name = "PATH")]
        out: PathBuf,
    },
}

impl CollisionShadowCommand {
    pub(crate) fn run(self) -> Result<()> {
        match self {
            Self::Capture {
                snapshot,
                out,
                budgets,
            } => {
                let raw = std::fs::read_to_string(&snapshot)
                    .with_context(|| format!("reading {}", snapshot.display()))?;
                let tick: CandidateSnapshotTick = serde_json::from_str(&raw)
                    .with_context(|| format!("parsing {}", snapshot.display()))?;
                let budget: StudyBudgets = match budgets {
                    Some(p) => serde_json::from_str(&std::fs::read_to_string(p)?)?,
                    None => StudyBudgets::default(),
                };
                let records = capture("rjwalters/loom", &tick, &budget)?;
                let mut body = String::new();
                for mut r in records {
                    r.id = capture_id(&r)?;
                    body.push_str(&serde_json::to_string(&r)?);
                    body.push('\n');
                }
                if body.is_empty() {
                    body.push_str("{\"captured\":0}\n");
                }
                std::fs::write(&out, body).with_context(|| format!("writing {}", out.display()))?;
                println!("captured → {} (study_stopped={})", out.display(), budget.study_stopped);
                Ok(())
            }
            Self::CaptureLive {
                repo,
                out_dir,
                budgets,
            } => run_capture_live(repo.as_deref(), &out_dir, budgets.as_deref()),
            Self::Evaluate {
                captures,
                outcomes,
                policies,
                budgets,
                out,
            } => {
                let read_jsonl = |p: &PathBuf, what: &str| -> Result<Vec<String>> {
                    let raw = std::fs::read_to_string(p)
                        .with_context(|| format!("reading {} {}", what, p.display()))?;
                    Ok(raw
                        .lines()
                        .filter(|l| !l.trim().is_empty())
                        .map(str::to_string)
                        .collect())
                };
                let capture_lines = read_jsonl(&captures, "captures")?;
                let mut capture_records = Vec::new();
                for (n, l) in capture_lines.iter().enumerate() {
                    if l.trim() == "{\"captured\":0}" {
                        continue;
                    }
                    capture_records.push(
                        serde_json::from_str(l)
                            .with_context(|| format!("parsing capture row {}", n + 1))?,
                    );
                }
                let mut outcome_records = Vec::new();
                for (n, l) in read_jsonl(&outcomes, "outcomes")?.iter().enumerate() {
                    let o: OutcomeRecord = serde_json::from_str(l)
                        .with_context(|| format!("parsing outcome row {}", n + 1))?;
                    outcome_records.push(o);
                }
                let policies: loom_daemon::collision_shadow::PolicyThresholds =
                    serde_json::from_str(&std::fs::read_to_string(&policies)?)?;
                let budget: StudyBudgets =
                    serde_json::from_str(&std::fs::read_to_string(&budgets)?)?;
                if capture_records.is_empty() {
                    bail!("no capture records to evaluate");
                }
                let thresholds = policies;
                let report = evaluate(&capture_records, &outcome_records, &budget, &thresholds);
                std::fs::write(&out, serde_json::to_vec_pretty(&report)?)
                    .with_context(|| format!("writing {}", out.display()))?;
                println!(
                    "verdict: {} ({} captured, {} resolved positives) → {}",
                    report.verdict,
                    report.captured,
                    report.resolved_positives,
                    out.display()
                );
                Ok(())
            }
        }
    }
}
/// `capture-live`: list the ready queue through the ETag-cached forge
/// listing, snapshot every concurrent candidate pair, write one JSONL file.
///
/// v1 deliberately does **not** fetch issue bodies: the cohort skeleton
/// (issue numbers, claim state, eligibility) is what the denominators need,
/// and the ETag-cached listing costs nothing when unchanged (#5057's
/// API-pressure contract). Feature enrichment rides a follow-up.
fn run_capture_live(
    repo: Option<&str>,
    out_dir: &std::path::Path,
    budgets_path: Option<&std::path::Path>,
) -> Result<()> {
    use loom_daemon::collision_shadow::CandidateSnapshot;
    let now = chrono::Utc::now();
    let tick_id = format!("tick-{}", now.to_rfc3339().replace(':', "-"));
    let budget: StudyBudgets = match budgets_path {
        Some(p) => serde_json::from_str(&std::fs::read_to_string(p)?)?,
        None => StudyBudgets::default(),
    };
    if budget.study_stopped {
        println!("capture-live: study stopped — zero records, zero forge calls");
        return Ok(());
    }
    let gh_bin = std::env::var("LOOM_GH_BIN").unwrap_or_else(|_| "gh".into());
    let cwd = std::env::current_dir()?;
    let issues = loom_daemon::forge_listing::list_issues_cached_as(
        "collision-shadow-capture",
        std::path::Path::new(&gh_bin),
        Some(&cwd),
        repo,
        "loom:issue",
        "open",
    )?;
    let candidates: Vec<CandidateSnapshot> = issues
        .iter()
        .map(|i| CandidateSnapshot {
            issue: i.number,
            tick_id: tick_id.clone(),
            admission_policy: "work-finder-ready-queue".into(),
            claim_state: "ready".into(),
            dispatch_eligible: true,
            has_implementation_evidence: false,
            curator_affected_files: Vec::new(),
            retrieval_files: None,
            captured_at: now.to_rfc3339(),
        })
        .collect();
    let tick = CandidateSnapshotTick {
        tick_id: tick_id.clone(),
        admission_policy: "work-finder-ready-queue".into(),
        captured_at: now.to_rfc3339(),
        candidates,
    };
    let prior = loom_daemon::collision_shadow::count_captured_records(out_dir)?;
    let records =
        loom_daemon::collision_shadow::capture_with_prior("rjwalters/loom", &tick, &budget, prior)?;
    let path = write_tick_records(out_dir, &tick_id, &mut records.clone())?;
    println!(
        "capture-live: {} candidate(s), {} pair record(s) → {}",
        tick.candidates.len(),
        records.len(),
        path.display()
    );
    Ok(())
}
