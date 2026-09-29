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
//!
//! # Both populations, two enumerations (#8925)
//!
//! `gh issue list` never returns a pull request, so the original single
//! enumeration could not see a parked PR at all. This command now runs
//! `gh pr list --label loom:blocked --state open` as well, and reads a PR's body
//! and comments through [`forge::fetch_pr_body_and_comments`] (`gh issue view`
//! exits non-zero on a PR number). `--no-prs` restores the issues-only
//! behaviour for a caller that wants it; nothing in the fleet passes it.

use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::Result;
use serde::Deserialize;

use loom_daemon::cmd_out::Query;
use loom_daemon::dep_recheck::{extract, forge};
use loom_daemon::park_record;
use loom_daemon::script_helpers::gh_query;
use loom_daemon::stale_blocked::{
    classify, park_self_block, undeclared, Artifact, Evidence, Verdict,
};

/// How many open `loom:blocked` issues to examine by default.
///
/// Generous rather than tuned: the population is the issues a repo has given
/// up on, which is small in every repo observed (single digits here, three in
/// the `rulehunt` incident that filed #8927). The cap exists so a pathological
/// repo cannot turn a pre-flight advisory into a multi-minute forge crawl.
pub(super) const DEFAULT_LIMIT: u32 = 100;

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

    /// Maximum number of open `loom:blocked` artifacts to examine, per
    /// population (issues and PRs are capped separately).
    #[arg(long, value_name = "N", default_value_t = DEFAULT_LIMIT)]
    pub limit: u32,

    /// Skip the open `loom:blocked` **pull request** population (#8925),
    /// restoring the issues-only behaviour this check shipped with.
    #[arg(long)]
    pub no_prs: bool,
}

/// One row of the enumeration query. Shared by both populations — `gh pr list`
/// and `gh issue list` return the same `number,title` shape.
///
/// `pub(super)` (rather than private): reused by `notify_cleared_blockers`
/// (issue #9102), the close-triggered sibling of this fleet-wide advisory,
/// which enumerates the same `loom:blocked` population via [`list_blocked`]
/// rather than re-deriving it.
#[derive(Debug, Clone, Deserialize)]
pub(super) struct IssueRow {
    pub(super) number: i64,
    #[serde(default)]
    pub(super) title: String,
}

/// One classified artifact, ready to render. `Clone` because a prose-only park is
/// reported in its own section *as well as* under its verdict.
#[derive(Clone)]
struct Finding {
    kind: Artifact,
    number: i64,
    title: String,
    verdict: Verdict,
    /// The park is stated somewhere, but not in a park record (#8925).
    undeclared: bool,
}

impl Finding {
    /// `issue #123` / `PR #123` — the report never leaves the kind implicit,
    /// because the remedy differs.
    fn reference(&self) -> String {
        format!("{} #{}", self.kind.label(), self.number)
    }
}

impl StaleBlockedArgs {
    /// Always `Ok(())`. See the module doc: the exit code is contract.
    pub(crate) fn run(self) -> Result<()> {
        let root = match &self.repo_root {
            Some(r) => r.clone(),
            None => std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
        };
        let repo = self.repo.as_deref();

        let mut populations: Vec<(Artifact, Vec<IssueRow>)> = Vec::new();
        let mut enumerate_errors: Vec<String> = Vec::new();

        let (rows, err) = list_blocked(Artifact::Issue, &root, repo, self.limit);
        populations.push((Artifact::Issue, rows));
        enumerate_errors.extend(err);

        if !self.no_prs {
            let (rows, err) = list_blocked(Artifact::Pr, &root, repo, self.limit);
            populations.push((Artifact::Pr, rows));
            enumerate_errors.extend(err);
        }

        let mut stale: Vec<Finding> = Vec::new();
        let mut superseded: Vec<Finding> = Vec::new();
        let mut undocumented: Vec<Finding> = Vec::new();
        let mut prose_only: Vec<Finding> = Vec::new();
        let mut unevaluated: Vec<(String, String)> = Vec::new();

        for (kind, rows) in populations {
            for row in rows {
                let evidence = match gather(kind, row.number, repo, &root) {
                    Ok(e) => e,
                    Err(why) => {
                        unevaluated.push((format!("{} #{}", kind.label(), row.number), why));
                        continue;
                    }
                };
                let finding = Finding {
                    kind,
                    number: row.number,
                    title: row.title.clone(),
                    verdict: classify(&evidence),
                    undeclared: undeclared(&evidence),
                };
                // A prose-only park is reported REGARDLESS of its verdict: a
                // still-blocked park whose blocker is unreadable is the defect
                // in waiting, and waiting for it to go stale is what let #8852
                // sit through its blocker closing (#8925).
                if finding.undeclared {
                    prose_only.push(finding.clone());
                }
                match finding.verdict {
                    Verdict::Stale(_) => stale.push(finding),
                    Verdict::Superseded { .. } => superseded.push(finding),
                    Verdict::Undocumented => undocumented.push(finding),
                    Verdict::StillBlocked => {}
                }
            }
        }

        let enumerate_error = if enumerate_errors.is_empty() {
            None
        } else {
            Some(enumerate_errors.join("; "))
        };

        let sections = Sections {
            stale: &stale,
            superseded: &superseded,
            undocumented: &undocumented,
            prose_only: &prose_only,
            unevaluated: &unevaluated,
            enumerate_error: enumerate_error.as_deref(),
        };

        if self.json {
            print_json(&sections);
            return Ok(());
        }

        report(&sections, self.quiet);
        Ok(())
    }
}

/// The classified populations, passed as one struct so adding a section does not
/// grow every signature.
struct Sections<'a> {
    stale: &'a [Finding],
    superseded: &'a [Finding],
    undocumented: &'a [Finding],
    prose_only: &'a [Finding],
    unevaluated: &'a [(String, String)],
    enumerate_error: Option<&'a str>,
}

impl Sections<'_> {
    /// Whether anything at all needs a human's attention.
    fn any(&self) -> bool {
        !self.stale.is_empty()
            || !self.superseded.is_empty()
            || !self.undocumented.is_empty()
            || !self.prose_only.is_empty()
    }
}

/// Every open `loom:blocked` artifact of one kind, plus a reason string when the
/// enumeration itself did not answer.
///
/// `gh issue list --label="loom:blocked" --state=open` is `curator.md`'s own
/// Priority-2 query shape, reused rather than re-derived; the PR arm is the same
/// query against `gh pr list`, which is the enumeration #8925 found missing. An
/// empty result is a fact (`Query::Empty`), not a failure — that is the healthy
/// repo.
pub(super) fn list_blocked(
    kind: Artifact,
    root: &Path,
    repo: Option<&str>,
    limit: u32,
) -> (Vec<IssueRow>, Option<String>) {
    let limit = limit.to_string();
    let entity = match kind {
        Artifact::Issue => "issue",
        Artifact::Pr => "pr",
    };
    let mut args = vec![entity, "list", "--label", "loom:blocked", "--state", "open"];
    if let Some(r) = repo {
        args.extend(["--repo", r]);
    }
    args.extend(["--json", "number,title", "--limit", &limit]);

    let q: Query<Vec<IssueRow>> = gh_query(&args, root, false, |v: &Vec<IssueRow>| v.is_empty());
    match q {
        Query::Populated(rows) => (rows, None),
        Query::Empty => (Vec::new(), None),
        Query::Malformed { error, .. } => {
            (Vec::new(), Some(format!("gh {entity} list returned unreadable JSON: {error}")))
        }
        Query::Failed { status, .. } => {
            (Vec::new(), Some(format!("gh {entity} list exited {status}")))
        }
        Query::Unavailable(u) => {
            (Vec::new(), Some(format!("gh {entity} list could not be run: {u:?}")))
        }
    }
}

/// Read one artifact's blocker references in every shape that applies to it.
///
/// Every forge read here is a `dep_recheck::forge` call, and the only text this
/// function parses itself is the park record ([`park_record::blockers`], which
/// reads the *same* `Blocked by: #N` vocabulary rather than adding a second one).
/// `fetch_named_deps` runs the `## Dependencies` checklist matcher,
/// `extract::extract` runs the prose dependency-phrase matcher over the body plus
/// every non-bot comment, and `fetch_prs` reads the linked closing PRs. That is
/// the whole point — #8927 asks for a trigger for the existing check, not a
/// second copy of it.
///
/// # What differs for a PR (#8925)
///
/// - The body/comments read goes through `gh pr view`, not `gh issue view`.
/// - There is no `## Dependencies` checklist arm and no linked-closing-PR arm: a
///   PR body carries `Closes #N`, which is the *opposite* relation, and a PR has
///   no closing PR of its own. Reading either would answer a question nobody
///   asked.
/// - The PR's own state supplies [`Evidence::self_block`], the superseding-block
///   gate the issue arm gets from `closing` instead.
pub(super) fn gather(
    kind: Artifact,
    number: i64,
    repo: Option<&str>,
    root: &Path,
) -> Result<Evidence, String> {
    let input = match kind {
        Artifact::Issue => forge::fetch_body_and_comments(number, repo, root),
        Artifact::Pr => forge::fetch_pr_body_and_comments(number, repo, root),
    }
    .map_err(|e| e.to_string())?;

    let numbers: Vec<i64> = extract::extract(&input, extract::DEFAULT_BOT_LOGIN)
        .split_whitespace()
        .filter_map(|t| t.parse().ok())
        .collect();
    let prose = if numbers.is_empty() {
        Vec::new()
    } else {
        forge::fetch_refs(&numbers, repo, root).map_err(|e| e.to_string())?
    };

    // The park record is read from the BODY only. A record in a comment would be
    // the very thing #8925 is closing: a park nobody can attribute to the
    // artifact's own declared state.
    let declared = park_record::blockers(&input.body);

    let (named, closing, self_block) = match kind {
        Artifact::Issue => (
            forge::fetch_named_deps(number, repo, root).map_err(|e| e.to_string())?,
            forge::fetch_prs(number, repo, root).map_err(|e| e.to_string())?,
            None,
        ),
        Artifact::Pr => {
            let this = forge::fetch_pr(number, repo, root).map_err(|e| e.to_string())?;
            (Vec::new(), Vec::new(), park_self_block(&this))
        }
    };

    Ok(Evidence {
        named,
        prose,
        closing,
        declared,
        self_block,
    })
}

/// The human report: a bordered stderr warning when anything was found, plus
/// the suppressible one-line stdout confirmation when nothing was.
///
/// Deliberately uncoloured, unlike the three sibling shell scripts: every
/// caller of this on the sweep path captures stderr to a log file, which is the
/// same reasoning `script_helpers::emit` records for its own diagnostics.
fn report(s: &Sections<'_>, quiet: bool) {
    let mut w = std::io::stderr();

    if let Some(why) = s.enumerate_error {
        let _ =
            writeln!(w, "[stale-blocked] could not enumerate open loom:blocked artifacts: {why}");
        let _ = writeln!(
            w,
            "[stale-blocked] reporting nothing for that population — this is UNKNOWN, not clear \
             (advisory; exit 0)."
        );
    }

    if s.any() {
        let n = s.stale.len() + s.superseded.len() + s.undocumented.len() + s.prose_only.len();
        let _ = writeln!(w);
        let _ = writeln!(w, "{}", "=".repeat(72));
        let _ = writeln!(
            w,
            "  WARNING: {n} open loom:blocked artifact(s) need a human re-check (#8927, #8925)"
        );
        let _ = writeln!(w, "{}", "=".repeat(72));
        let _ = writeln!(
            w,
            "loom:blocked is applied once and never re-examined, and a blocked artifact is"
        );
        let _ = writeln!(
            w,
            "skipped by /loom:sweep and by Champion's promotion lane — so a label that"
        );
        let _ = writeln!(w, "outlives its cause removes it from every queue.");
    }

    if !s.stale.is_empty() {
        let _ = writeln!(w);
        let _ = writeln!(w, "STALE BLOCK — the cited blocker has resolved ({}):", s.stale.len());
        for f in s.stale {
            let _ = writeln!(w, "  {} {}", f.reference(), f.title);
            if let Verdict::Stale(reasons) = &f.verdict {
                for r in reasons {
                    let _ = writeln!(w, "      - {r}");
                }
            }
        }
    }

    if !s.superseded.is_empty() {
        let _ = writeln!(w);
        let _ = writeln!(
            w,
            "SUPERSEDED BLOCK — the cited blocker resolved, but the artifact still cannot \
             proceed ({}):",
            s.superseded.len()
        );
        for f in s.superseded {
            let _ = writeln!(w, "  {} {}", f.reference(), f.title);
            if let Verdict::Superseded { cleared, block } = &f.verdict {
                for r in cleared {
                    let _ = writeln!(w, "      - cleared: {r}");
                }
                let _ = writeln!(w, "      - superseded: {block}");
            }
        }
        let _ =
            writeln!(w, "  Do NOT unpark these on the cleared dependency alone (#4634, #7267).");
    }

    if !s.undocumented.is_empty() {
        let _ = writeln!(w);
        let _ = writeln!(
            w,
            "UNDOCUMENTED BLOCK — no parseable blocker reference anywhere ({}):",
            s.undocumented.len()
        );
        for f in s.undocumented {
            let _ = writeln!(w, "  {} {}", f.reference(), f.title);
        }
        let _ =
            writeln!(w, "  A block with no stated reason cannot be verified or cleared by anyone");
        let _ = writeln!(
            w,
            "  who was not present when it was applied. Record a park record — its rendered"
        );
        // Named inline rather than deferred to the PROSE-ONLY section's remedy:
        // that section is only printed when something is prose-only, so an
        // undocumented-only report used to end on a "(below)" with nothing below.
        let _ = writeln!(
            w,
            "  `{} #N` line is what every existing fleet parser reads:",
            park_record::RENDERED_PHRASE
        );
        let _ = writeln!(w, "      loom-daemon park-record render --blocked-by <N> --by <role>");
        let _ = writeln!(w, "  or a `## Dependencies` checklist item, or drop the label.");
    }

    if !s.prose_only.is_empty() {
        let _ = writeln!(w);
        let _ = writeln!(
            w,
            "PROSE-ONLY PARK — a blocker is cited, but no `{}…{}` record declares it ({}):",
            park_record::MARKER_OPEN,
            park_record::MARKER_CLOSE,
            s.prose_only.len()
        );
        for f in s.prose_only {
            let _ = writeln!(w, "  {} {}", f.reference(), f.title);
        }
        let _ = writeln!(
            w,
            "  Prose in a comment is not a declaration — it is why PR #8314 sat parked 155h"
        );
        let _ = writeln!(
            w,
            "  and why issue #8852 stayed parked after its blocker closed (#8925). Add one:"
        );
        let _ = writeln!(w, "      loom-daemon park-record render --blocked-by <N> --by <role>");
        let _ = writeln!(w, "  then paste it into the artifact BODY (see park-record.md).");
    }

    if !s.unevaluated.is_empty() {
        let _ = writeln!(w);
        let _ = writeln!(
            w,
            "NOT EVALUATED — a forge read did not answer ({}); neither clear nor stale:",
            s.unevaluated.len()
        );
        for (reference, why) in s.unevaluated {
            let _ = writeln!(w, "  {reference}: {why}");
        }
    }

    if s.any() {
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

    if s.any() {
        println!(
            "[stale-blocked] WARNING: {} stale, {} superseded, {} undocumented, {} prose-only \
             loom:blocked artifact(s). See stderr for details.",
            s.stale.len(),
            s.superseded.len(),
            s.undocumented.len(),
            s.prose_only.len()
        );
    } else if s.enumerate_error.is_some() {
        println!("[stale-blocked] could not enumerate loom:blocked artifacts; see stderr.");
    } else {
        println!("[stale-blocked] no stale, superseded, undocumented or prose-only loom:blocked artifacts.");
    }
}

/// The `--json` rendering: one object, for a caller that wants to branch rather
/// than read.
///
/// Every row carries `kind` (`"issue"` / `"PR"`), added with the PR population
/// (#8925) — a bare `number` is ambiguous across the two, and a consumer that
/// guessed would relabel the wrong artifact.
fn print_json(s: &Sections<'_>) {
    fn row(f: &Finding) -> serde_json::Value {
        serde_json::json!({
            "kind": f.kind.label(),
            "number": f.number,
            "title": f.title,
            "undeclared": f.undeclared,
        })
    }

    let stale_json: Vec<_> = s
        .stale
        .iter()
        .map(|f| {
            let reasons: &[String] = match &f.verdict {
                Verdict::Stale(r) => r,
                _ => &[],
            };
            let mut v = row(f);
            v["reasons"] = serde_json::json!(reasons);
            v
        })
        .collect();
    let superseded_json: Vec<_> = s
        .superseded
        .iter()
        .map(|f| {
            let mut v = row(f);
            if let Verdict::Superseded { cleared, block } = &f.verdict {
                v["cleared"] = serde_json::json!(cleared);
                v["superseded_by"] = serde_json::json!(block);
            }
            v
        })
        .collect();
    let undoc_json: Vec<_> = s.undocumented.iter().map(row).collect();
    let prose_only_json: Vec<_> = s.prose_only.iter().map(row).collect();
    let uneval_json: Vec<_> = s
        .unevaluated
        .iter()
        .map(|(reference, why)| serde_json::json!({ "artifact": reference, "reason": why }))
        .collect();
    println!(
        "{}",
        serde_json::json!({
            "stale": stale_json,
            "superseded": superseded_json,
            "undocumented": undoc_json,
            "prose_only": prose_only_json,
            "unevaluated": uneval_json,
            "enumerate_error": s.enumerate_error,
        })
    );
}
