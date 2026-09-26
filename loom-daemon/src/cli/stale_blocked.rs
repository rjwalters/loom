//! `loom-daemon check-stale-blocked` — the fleet-wide `loom:blocked` re-check
//! (issue #8927), backing `defaults/scripts/check-stale-blocked.sh`'s Shape-A
//! stub.
//!
//! The fourth pre-wave advisory check, alongside `check-host-sleep.sh` (#3350),
//! `check-main-freshness.sh` (#3770) and `check-quarantine-stashes.sh` (#5185).
//! Same contract as all three: strictly read-only, **always exits 0**, and a
//! one-line stdout confirmation when clear that `--quiet` suppresses.
//!
//! Brand-new logic, so it is native from the start per
//! `.loom/docs/shell-language-policy.md` — the `generate-agent-skills.sh`
//! precedent — rather than a script to be ported later. The classification
//! itself lives in [`loom_daemon::stale_blocked`]; this module is the
//! enumeration, the live reads (all delegated to
//! [`loom_daemon::dep_recheck::forge`]) and the rendering.
//!
//! # Why every failure is still exit 0
//!
//! An advisory that can fail is an advisory that gets removed from the
//! pre-flight. A forge read that did not answer is reported as *unevaluated* —
//! its own line in the output, never folded into "clear" and never into
//! "stale" — because [`loom_daemon::dep_recheck::forge`]'s whole fail-safe
//! posture is that a conclusion drawn from a failed read is worse than no
//! conclusion. The one thing this command will not do is guess.

use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::Result;
use serde::Deserialize;

use loom_daemon::cmd_out::Query;
use loom_daemon::dep_recheck::{extract, forge};
use loom_daemon::script_helpers::gh_query;
use loom_daemon::stale_blocked::{classify, Evidence, Verdict};

/// How many open `loom:blocked` issues to examine by default.
///
/// Generous rather than tuned: the population is the issues a repo has given
/// up on, which is small in every repo observed (single digits here, three in
/// the `rulehunt` incident that filed #8927). The cap exists so a pathological
/// repo cannot turn a pre-flight advisory into a multi-minute forge crawl.
const DEFAULT_LIMIT: u32 = 100;

#[derive(clap::Args)]
pub(crate) struct StaleBlockedArgs {
    /// Suppress the one-line stdout confirmation when nothing is found.
    /// Warnings still go to stderr. Matches `check-host-sleep.sh --quiet`.
    #[arg(long, short = 'q')]
    pub quiet: bool,

    /// Emit one JSON object on stdout instead of the human report. Implies the
    /// `--quiet` suppression of the confirmation line.
    #[arg(long)]
    pub json: bool,

    /// Repository to scan, as `owner/name`. Defaults to whatever `gh` resolves
    /// from `--repo-root`.
    #[arg(long, value_name = "OWNER/NAME")]
    pub repo: Option<String>,

    /// Directory to run `gh` from. Defaults to the current directory.
    #[arg(long, value_name = "PATH")]
    pub repo_root: Option<PathBuf>,

    /// Maximum number of open `loom:blocked` issues to examine.
    #[arg(long, value_name = "N", default_value_t = DEFAULT_LIMIT)]
    pub limit: u32,
}

/// One row of the enumeration query.
#[derive(Debug, Clone, Deserialize)]
struct IssueRow {
    number: i64,
    #[serde(default)]
    title: String,
}

/// One classified issue, ready to render.
struct Finding {
    number: i64,
    title: String,
    verdict: Verdict,
}

impl StaleBlockedArgs {
    /// Always `Ok(())`. See the module doc: the exit code is contract.
    pub(crate) fn run(self) -> Result<()> {
        let root = match &self.repo_root {
            Some(r) => r.clone(),
            None => std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
        };
        let repo = self.repo.as_deref();

        let (rows, enumerate_error) = list_blocked(&root, repo, self.limit);

        let mut stale: Vec<Finding> = Vec::new();
        let mut undocumented: Vec<Finding> = Vec::new();
        let mut unevaluated: Vec<(i64, String)> = Vec::new();

        for row in rows {
            match gather(row.number, repo, &root) {
                Ok(evidence) => {
                    let verdict = classify(&evidence);
                    let finding = Finding {
                        number: row.number,
                        title: row.title.clone(),
                        verdict,
                    };
                    match finding.verdict {
                        Verdict::Stale(_) => stale.push(finding),
                        Verdict::Undocumented => undocumented.push(finding),
                        Verdict::StillBlocked => {}
                    }
                }
                Err(why) => unevaluated.push((row.number, why)),
            }
        }

        if self.json {
            print_json(&stale, &undocumented, &unevaluated, enumerate_error.as_deref());
            return Ok(());
        }

        report(&stale, &undocumented, &unevaluated, enumerate_error.as_deref(), self.quiet);
        Ok(())
    }
}

/// Every open `loom:blocked` issue, plus a reason string when the enumeration
/// itself did not answer.
///
/// `gh issue list --label="loom:blocked" --state=open` is `curator.md`'s own
/// Priority-2 query shape, reused rather than re-derived. An empty result is a
/// fact (`Query::Empty`), not a failure — that is the healthy repo.
fn list_blocked(root: &Path, repo: Option<&str>, limit: u32) -> (Vec<IssueRow>, Option<String>) {
    let limit = limit.to_string();
    let mut args = vec![
        "issue",
        "list",
        "--label",
        "loom:blocked",
        "--state",
        "open",
    ];
    if let Some(r) = repo {
        args.extend(["--repo", r]);
    }
    args.extend(["--json", "number,title", "--limit", &limit]);

    let q: Query<Vec<IssueRow>> = gh_query(&args, root, false, |v: &Vec<IssueRow>| v.is_empty());
    match q {
        Query::Populated(rows) => (rows, None),
        Query::Empty => (Vec::new(), None),
        Query::Malformed { error, .. } => {
            (Vec::new(), Some(format!("gh issue list returned unreadable JSON: {error}")))
        }
        Query::Failed { status, .. } => {
            (Vec::new(), Some(format!("gh issue list exited {status}")))
        }
        Query::Unavailable(u) => {
            (Vec::new(), Some(format!("gh issue list could not be run: {u:?}")))
        }
    }
}

/// Read one issue's blocker references in all three shapes.
///
/// Every read here is a `dep_recheck::forge` call. Nothing in this function
/// parses issue text: `fetch_named_deps` runs the `## Dependencies` checklist
/// matcher, `extract::extract` runs the prose dependency-phrase matcher over
/// the body plus every non-bot comment, and `fetch_prs` reads the linked
/// closing PRs. That is the whole point — #8927 asks for a trigger for the
/// existing check, not a second copy of it.
fn gather(issue: i64, repo: Option<&str>, root: &Path) -> Result<Evidence, String> {
    let named = forge::fetch_named_deps(issue, repo, root).map_err(|e| e.to_string())?;

    let input = forge::fetch_body_and_comments(issue, repo, root).map_err(|e| e.to_string())?;
    let numbers: Vec<i64> = extract::extract(&input, extract::DEFAULT_BOT_LOGIN)
        .split_whitespace()
        .filter_map(|t| t.parse().ok())
        .collect();
    let prose = if numbers.is_empty() {
        Vec::new()
    } else {
        forge::fetch_refs(&numbers, repo, root).map_err(|e| e.to_string())?
    };

    let closing = forge::fetch_prs(issue, repo, root).map_err(|e| e.to_string())?;

    Ok(Evidence {
        named,
        prose,
        closing,
    })
}

/// The human report: a bordered stderr warning when anything was found, plus
/// the suppressible one-line stdout confirmation when nothing was.
///
/// Deliberately uncoloured, unlike the three sibling shell scripts: every
/// caller of this on the sweep path captures stderr to a log file, which is the
/// same reasoning `script_helpers::emit` records for its own diagnostics.
fn report(
    stale: &[Finding],
    undocumented: &[Finding],
    unevaluated: &[(i64, String)],
    enumerate_error: Option<&str>,
    quiet: bool,
) {
    let mut w = std::io::stderr();

    if let Some(why) = enumerate_error {
        let _ = writeln!(w, "[stale-blocked] could not enumerate open loom:blocked issues: {why}");
        let _ = writeln!(
            w,
            "[stale-blocked] reporting nothing — this is UNKNOWN, not clear (advisory; exit 0)."
        );
    }

    if !stale.is_empty() || !undocumented.is_empty() {
        let n = stale.len() + undocumented.len();
        let _ = writeln!(w);
        let _ = writeln!(w, "{}", "=".repeat(72));
        let _ =
            writeln!(w, "  WARNING: {n} open loom:blocked issue(s) need a human re-check (#8927)");
        let _ = writeln!(w, "{}", "=".repeat(72));
        let _ =
            writeln!(w, "loom:blocked is applied once and never re-examined, and a blocked issue");
        let _ =
            writeln!(w, "is skipped by /loom:sweep and by Champion's promotion lane — so a label");
        let _ = writeln!(w, "that outlives its cause removes an issue from every queue.");
    }

    if !stale.is_empty() {
        let _ = writeln!(w);
        let _ = writeln!(w, "STALE BLOCK — the cited blocker has resolved ({}):", stale.len());
        for f in stale {
            let _ = writeln!(w, "  #{} {}", f.number, f.title);
            if let Verdict::Stale(reasons) = &f.verdict {
                for r in reasons {
                    let _ = writeln!(w, "      - {r}");
                }
            }
        }
    }

    if !undocumented.is_empty() {
        let _ = writeln!(w);
        let _ = writeln!(
            w,
            "UNDOCUMENTED BLOCK — no parseable blocker reference anywhere ({}):",
            undocumented.len()
        );
        for f in undocumented {
            let _ = writeln!(w, "  #{} {}", f.number, f.title);
        }
        let _ =
            writeln!(w, "  A block with no stated reason cannot be verified or cleared by anyone");
        let _ = writeln!(
            w,
            "  who was not present when it was applied. Record one as `Blocked by #N`,"
        );
        let _ = writeln!(w, "  or a `## Dependencies` checklist item, or drop the label.");
    }

    if !unevaluated.is_empty() {
        let _ = writeln!(w);
        let _ = writeln!(
            w,
            "NOT EVALUATED — a forge read did not answer ({}); neither clear nor stale:",
            unevaluated.len()
        );
        for (number, why) in unevaluated {
            let _ = writeln!(w, "  #{number}: {why}");
        }
    }

    if !stale.is_empty() || !undocumented.is_empty() {
        let _ = writeln!(w);
        let _ = writeln!(w, "To re-check one issue's cited blockers in detail:");
        let _ = writeln!(
            w,
            "      ./.loom/scripts/dep-recheck-fingerprint.sh named-dependency --number <N>"
        );
        let _ = writeln!(
            w,
            "      ./.loom/scripts/dep-recheck-fingerprint.sh extract-refs --number <N>"
        );
        let _ = writeln!(w, "This check never edits a label — deciding is a human's job.");
        let _ = writeln!(w, "{}", "=".repeat(72));
        let _ = writeln!(w);
    }

    if quiet {
        return;
    }

    if stale.is_empty() && undocumented.is_empty() {
        if enumerate_error.is_some() {
            println!("[stale-blocked] could not enumerate loom:blocked issues; see stderr.");
        } else {
            println!("[stale-blocked] no stale or undocumented loom:blocked issues.");
        }
    } else {
        println!(
            "[stale-blocked] WARNING: {} stale, {} undocumented loom:blocked issue(s). See stderr for details.",
            stale.len(),
            undocumented.len()
        );
    }
}

/// The `--json` rendering: one object, for a caller that wants to branch rather
/// than read.
fn print_json(
    stale: &[Finding],
    undocumented: &[Finding],
    unevaluated: &[(i64, String)],
    enumerate_error: Option<&str>,
) {
    let stale_json: Vec<_> = stale
        .iter()
        .map(|f| {
            let reasons: &[String] = match &f.verdict {
                Verdict::Stale(r) => r,
                _ => &[],
            };
            serde_json::json!({ "number": f.number, "title": f.title, "reasons": reasons })
        })
        .collect();
    let undoc_json: Vec<_> = undocumented
        .iter()
        .map(|f| serde_json::json!({ "number": f.number, "title": f.title }))
        .collect();
    let uneval_json: Vec<_> = unevaluated
        .iter()
        .map(|(n, why)| serde_json::json!({ "number": n, "reason": why }))
        .collect();
    println!(
        "{}",
        serde_json::json!({
            "stale": stale_json,
            "undocumented": undoc_json,
            "unevaluated": uneval_json,
            "enumerate_error": enumerate_error,
        })
    );
}
