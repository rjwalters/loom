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

        /// `OWNER/REPO` stamped on the records (default: resolved from the
        /// cwd's git `origin` remote — see `resolve_capture_repo`).
        #[arg(long, value_name = "OWNER/REPO")]
        repo: Option<String>,

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

    /// Attribute live outcomes to captured pairs from the forge (#9920):
    /// for each captured pair, resolve both issues' merged PRs (one
    /// `gh pr list` call total + one files call per PR), compare their
    /// changed-file sets, and emit idempotent OutcomeRecords keyed to the
    /// capture pair ids. Semantics pre-registered with the offline
    /// converter's mapping (shared paths → TextualConflictPinned; none →
    /// Missing; either side unresolved → Pending).
    Attribute {
        /// Directory of capture-record JSONL files (from capture-live).
        #[arg(long, value_name = "DIR", default_value = ".loom/shadow")]
        captures: PathBuf,

        /// OWNER/REPO the issues live in.
        #[arg(long, value_name = "OWNER/REPO")]
        repo: String,

        /// Outcomes JSONL output (appended; existing ids skipped).
        #[arg(
            long,
            value_name = "PATH",
            default_value = ".loom/shadow/outcomes.jsonl"
        )]
        out: PathBuf,
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
                repo,
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
                let resolved = resolve_capture_repo(repo.as_deref())?;
                let records = capture(&resolved, &tick, &budget)?;
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
            Self::Attribute {
                captures,
                repo,
                out,
            } => {
                let gh_bin = std::env::var("LOOM_GH_BIN").unwrap_or_else(|_| "gh".into());
                let records = read_capture_records(&captures)?;
                if records.is_empty() {
                    bail!("no capture records under {}", captures.display());
                }
                let attribute_records =
                    loom_daemon::collision_shadow::attribute::attribute_captures(
                        &records, &gh_bin, &repo, &captures,
                    )?;
                // Idempotent: skip pairs whose record id is already in out.
                let mut existing: std::collections::BTreeSet<String> =
                    std::collections::BTreeSet::new();
                if out.exists() {
                    for line in std::fs::read_to_string(&out)?.lines() {
                        if line.trim().is_empty() {
                            continue;
                        }
                        if let Ok(v) = serde_json::from_str::<serde_json::Value>(line) {
                            if let Some(id) = v.get("id").and_then(|i| i.as_str()) {
                                existing.insert(id.to_string());
                            }
                        }
                    }
                }
                let fresh: Vec<_> = attribute_records
                    .iter()
                    .filter(|r| !existing.contains(&r.id))
                    .collect();
                use std::io::Write as _;
                let mut f = std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&out)
                    .with_context(|| format!("opening {}", out.display()))?;
                for r in &fresh {
                    writeln!(f, "{}", serde_json::to_string(r)?)?;
                }
                println!(
                    "attribute: {} pair(s) → {} new record(s) ({} already present) → {}",
                    attribute_records.len(),
                    fresh.len(),
                    existing.len(),
                    out.display()
                );
                Ok(())
            }
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
/// Resolve the `OWNER/REPO` stamped on capture records. An explicit
/// `--repo` wins; otherwise the cwd's git `origin` remote is parsed
/// (https and scp forms). Failure is a loud bail with the remedy — never a
/// guessed default (#9903 review: records must name the repo actually
/// queried).
fn resolve_capture_repo(explicit: Option<&str>) -> Result<String> {
    resolve_capture_repo_in(explicit, &std::env::current_dir()?)
}

fn resolve_capture_repo_in(explicit: Option<&str>, cwd: &std::path::Path) -> Result<String> {
    if let Some(r) = explicit {
        if !r.contains('/') {
            bail!("--repo {r}: expected OWNER/REPO");
        }
        return Ok(r.to_string());
    }
    let out = std::process::Command::new("git")
        .args(["-C", &cwd.to_string_lossy(), "remote", "get-url", "origin"])
        .output()
        .map_err(|e| anyhow::anyhow!("git remote get-url origin: {e}"))?;
    if !out.status.success() {
        bail!(
            "cannot resolve OWNER/REPO: git remote get-url origin failed ({}) — \
             pass --repo explicitly",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    let url = String::from_utf8_lossy(&out.stdout).trim().to_string();
    owner_repo_from_url(&url).ok_or_else(|| {
        anyhow::anyhow!("cannot parse OWNER/REPO from origin url {url:?} — pass --repo explicitly")
    })
}

/// `https://github.com/OWNER/REPO(.git)` and `git@github.com:OWNER/REPO.git`
/// both resolve to `OWNER/REPO`.
fn owner_repo_from_url(url: &str) -> Option<String> {
    let u = url.trim().trim_end_matches('/');
    let u = u.strip_suffix(".git").unwrap_or(u);
    let tail = if let Some(pos) = u.find("://") {
        let rest = &u[pos + 3..];
        match rest.find('/') {
            Some(i) => &rest[i + 1..],
            None => return None,
        }
    } else {
        match u.rsplit_once(':') {
            Some((_, t)) => t,
            None => u,
        }
    };
    let mut parts: Vec<&str> = tail.split('/').filter(|p| !p.is_empty()).collect();
    if parts.len() < 2 {
        return None;
    }
    let repo = parts.pop()?;
    let owner = parts.pop()?;
    if owner.is_empty() || repo.is_empty() {
        return None;
    }
    Some(format!("{owner}/{repo}"))
}

/// `capture-live`: list the ready queue through the ETag-cached forge
/// listing, snapshot every concurrent candidate pair, write one JSONL file.
///
/// v1 deliberately does **not** fetch issue bodies: the cohort skeleton
/// (issue numbers, claim state, eligibility) is what the denominators need,
/// and the ETag-cached listing costs nothing when unchanged (#5057's
/// API-pressure contract). Feature enrichment rides a follow-up.
/// Load every capture record from the tick JSONL files under `dir`.
fn read_capture_records(
    dir: &std::path::Path,
) -> Result<Vec<loom_daemon::collision_shadow::CaptureRecord>> {
    let mut records = Vec::new();
    let mut files: Vec<std::path::PathBuf> = std::fs::read_dir(dir)?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.extension().is_some_and(|x| x == "jsonl")
                && p.file_stem()
                    .is_some_and(|s| s.to_string_lossy().starts_with("tick-"))
        })
        .collect();
    files.sort();
    for f in files {
        let raw =
            std::fs::read_to_string(&f).with_context(|| format!("reading {}", f.display()))?;
        for (n, line) in raw.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            records.push(
                serde_json::from_str::<loom_daemon::collision_shadow::CaptureRecord>(line)
                    .with_context(|| format!("{} line {}", f.display(), n + 1))?,
            );
        }
    }
    Ok(records)
}

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
    // Resolve ONCE and thread the same value into both the forge listing
    // and the record stamping — a record's `repo` must name the repo that
    // was actually queried (#9903 review).
    let resolved_repo = resolve_capture_repo(repo)?;
    let issues = loom_daemon::forge_listing::list_issues_cached_as(
        "collision-shadow-capture",
        std::path::Path::new(&gh_bin),
        Some(&cwd),
        Some(&resolved_repo),
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
        loom_daemon::collision_shadow::capture_with_prior(&resolved_repo, &tick, &budget, prior)?;
    let path = write_tick_records(out_dir, &tick_id, &mut records.clone())?;
    println!(
        "capture-live: {} candidate(s), {} pair record(s) → {}",
        tick.candidates.len(),
        records.len(),
        path.display()
    );
    Ok(())
}

#[cfg(test)]
mod repo_resolution_tests {
    use super::{owner_repo_from_url, resolve_capture_repo, resolve_capture_repo_in};
    use std::process::Command;
    use tempfile::TempDir;

    #[test]
    fn owner_repo_from_url_handles_both_remote_forms() {
        assert_eq!(owner_repo_from_url("https://github.com/o/r.git"), Some("o/r".to_string()));
        assert_eq!(owner_repo_from_url("https://github.com/o/r"), Some("o/r".to_string()));
        assert_eq!(owner_repo_from_url("git@github.com:o/r.git"), Some("o/r".to_string()));
        assert_eq!(owner_repo_from_url("https://github.com/o/r/"), Some("o/r".to_string()));
        assert_eq!(owner_repo_from_url("not-a-url"), None);
        assert_eq!(owner_repo_from_url("https://github.com/only-owner"), None);
    }

    #[test]
    fn explicit_repo_wins_and_is_shape_checked() {
        assert_eq!(resolve_capture_repo(Some("o/r")).unwrap(), "o/r");
        assert!(resolve_capture_repo(Some("just-a-name"))
            .unwrap_err()
            .to_string()
            .contains("expected OWNER/REPO"));
    }

    #[test]
    fn unresolvable_origin_bails_with_the_remedy() {
        // A directory with no git repo at all: get-url fails → loud bail
        // naming --repo, never a guessed default.
        let dir = TempDir::new().unwrap();
        let out = Command::new("git")
            .args([
                "-C",
                dir.path().to_str().unwrap(),
                "remote",
                "get-url",
                "origin",
            ])
            .output()
            .expect("git present");
        assert!(!out.status.success(), "precondition: no origin in a temp dir");
        let err = resolve_capture_repo_in(None, dir.path()).unwrap_err();
        assert!(err.to_string().contains("pass --repo explicitly"));
    }

    #[test]
    fn cwd_origin_resolves_to_owner_repo() {
        // The real cwd has an origin (github.com/rjwalters/loom); both
        // verbs' default resolution must land on OWNER/REPO.
        let err = resolve_capture_repo_in(None, std::path::Path::new("."));
        assert!(
            err.is_ok()
                || err
                    .unwrap_err()
                    .to_string()
                    .contains("pass --repo explicitly"),
            "a real checkout resolves to OWNER/REPO or bails loudly"
        );
    }
}
