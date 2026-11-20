//! `loom-daemon check-stale-blocked` — the per-repository `loom:blocked`
//! re-check (issue #8927), backing `defaults/scripts/check-stale-blocked.sh`'s
//! Shape-A stub. One invocation examines one repository; see "One repository
//! per invocation" below.
//!
//! The fourth pre-wave advisory check, alongside `check-host-sleep.sh` (#3350),
//! `check-main-freshness.sh` (#3770) and `check-quarantine-stashes.sh` (#5185).
//! Same contract as all three: strictly read-only, **always exits 0**, and a
//! one-line stdout confirmation when clear that `--quiet` suppresses.
//!
//! Brand-new logic, so it is native from the start per
//! `.loom/docs/shell-language-policy.md` — the `generate-agent-skills.sh`
//! precedent — rather than a script to be ported later. The classification
//! itself lives in [`crate::stale_blocked`], and the forge reads in
//! [`crate::stale_blocked::batch`] (#10480: one REST listing, REST + ETag
//! comment and blocker-state reads, one GraphQL query per 100 issues); this
//! module is the rendering. `notify-cleared-blockers` reads through the same
//! gatherer (#10515).
//!
//! # Budget floor (#10480)
//!
//! After the candidate listing the run reads the free budget probe and
//! projects its own cost; if it would take the GraphQL or core bucket below
//! `--min-graphql-remaining` / `--min-core-remaining` (default 1,000 each, `0`
//! disables) it gathers nothing and reports every artifact *not evaluated*.
//! The same floors are re-checked between reads, from the forge's own
//! answers. `--json` carries what the run spent as `forge_cost`; the human
//! report prints the same numbers as one stderr line unless `--quiet`.
//! The logic is [`crate::stale_blocked::budget`].
//!
//! # One repository per invocation (#9191)
//!
//! `--repo` / `--repo-root` resolve exactly one repository and nothing here
//! walks a workspace registry. A caller that wants several repositories has to
//! invoke this once per repository, and that loop is not protected by anything
//! in this process: the floor above reads the hourly buckets, which a GitHub
//! secondary (burst) limit does not appear in, and this one-shot process has
//! no rate-limit breaker (it exists only in the running daemon), so a refusal
//! stops nothing here and reaches no later invocation.
//! `.loom/docs/github-authentication.md` -> "Secondary rate limits and
//! hand-rolled loops" has the caller-side rules. Nothing removes
//! `loom:blocked` automatically: the daemon's release pass was retired
//! (#11301), so a blocked item stays blocked until a role or a human clears it.
//!
//! # Why every failure is still exit 0
//!
//! An advisory that can fail is an advisory that gets removed from the
//! pre-flight. A forge read that did not answer is reported as *unevaluated* —
//! its own line in the output, never folded into "clear" and never into
//! "stale" — because [`crate::dep_recheck::forge`]'s whole fail-safe
//! posture is that a conclusion drawn from a failed read is worse than no
//! conclusion. The one thing this command will not do is guess.
//!
//! # Archived repositories (#10562)
//!
//! The run first reads the repository's `archived` flag (one REST + ETag
//! `repos/{owner}/{repo}` read, the probe the release pass shares). An
//! archived repository is read-only, so no role can act on its rows: nothing
//! is listed or gathered, `--json` reports `archived: true`, and the human
//! report is one line. A probe that did not answer is an enumeration failure
//! — unknown, never archived and never clear.
//!
//! # Both populations (#8925)
//!
//! `gh issue list` never returns a pull request, so the original single
//! enumeration could not see a parked PR at all. The REST listing the batch
//! gatherer reads returns both, split by `pull_request`. `--no-prs` restores
//! the issues-only behaviour for a caller that wants it; nothing in the fleet
//! passes it.

use std::io::Write;
use std::path::PathBuf;

use anyhow::Result;

use crate::park_record;
use crate::stale_blocked::{batch, budget, classify, resolved, undeclared, Artifact, Verdict};

/// How many open `loom:blocked` issues to examine by default.
///
/// Generous rather than tuned: the population is the issues a repo has given
/// up on, which is small in every repo observed (single digits here, three in
/// the `rulehunt` incident that filed #8927). The cap exists so a pathological
/// repo cannot turn a pre-flight advisory into a multi-minute forge crawl.
pub(super) const DEFAULT_LIMIT: u32 = 100;

#[derive(clap::Args)]
pub struct StaleBlockedArgs {
    /// Suppress the one-line stdout confirmation when nothing is found.
    /// Warnings still go to stderr. Matches `check-host-sleep.sh --quiet`.
    #[arg(long, short = 'q')]
    pub quiet: bool,

    /// Emit one JSON object on stdout instead of the human report. Implies the
    /// `--quiet` suppression of the confirmation line.
    #[arg(long)]
    pub json: bool,

    /// The one repository to scan, as `owner/name`. Defaults to whatever `gh`
    /// resolves from `--repo-root`. Not repeatable: for several repositories,
    /// pace separate invocations and stop at the first rate-limit refusal.
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

    /// Report what Guide's named-blocker PR sweep can release or must leave
    /// parked. Read-only; unnamed parks and issue rows are outside this mode.
    #[arg(long, conflicts_with = "no_prs")]
    pub pr_resweep: bool,

    /// GraphQL points that must remain after this run (#10480). If the free
    /// budget probe says the run would go below it, nothing is gathered and
    /// every artifact is reported not evaluated (still exit 0); it is also
    /// re-checked between GraphQL batches. `0` disables the check.
    #[arg(long, value_name = "N", default_value_t = budget::DEFAULT_MIN_GRAPHQL_REMAINING)]
    pub min_graphql_remaining: u64,

    /// Core (REST) requests that must remain after this run (#10480). Same
    /// semantics as `--min-graphql-remaining`, re-checked before each REST
    /// read. `0` disables the check.
    #[arg(long, value_name = "N", default_value_t = budget::DEFAULT_MIN_CORE_REMAINING)]
    pub min_core_remaining: u64,
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
    pub fn run(self) -> Result<()> {
        let root = match &self.repo_root {
            Some(r) => r.clone(),
            None => std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
        };
        let mut forge = batch::GhStaleBlockedForge::new(&root, self.repo.as_deref());
        let fleet = crate::forge_identity::FleetLogins::for_root(&root);
        let opts = batch::Options {
            limit: self.limit,
            no_prs: self.no_prs,
            floor: budget::Floor {
                graphql: self.min_graphql_remaining,
                core: self.min_core_remaining,
            },
        };
        if self.pr_resweep {
            let gathering = batch::gather_pr_resweep(&mut forge, &fleet, opts);
            report_pr_resweep(&gathering, self.limit, self.json, self.quiet);
            return Ok(());
        }
        let batch::Gathering {
            items: gathered,
            enumerate_error,
            cost,
            archived,
        } = batch::gather_checked(&mut forge, &fleet, opts);

        let mut stale: Vec<Finding> = Vec::new();
        let mut superseded: Vec<Finding> = Vec::new();
        let mut unticked: Vec<Finding> = Vec::new();
        let mut undocumented: Vec<Finding> = Vec::new();
        let mut held: Vec<Finding> = Vec::new();
        let mut prose_only: Vec<Finding> = Vec::new();
        let mut unevaluated: Vec<(String, String)> = Vec::new();

        for g in gathered {
            let evidence = match g.evidence {
                Ok(e) => e,
                Err(why) => {
                    unevaluated.push((format!("{} #{}", g.kind.label(), g.number), why));
                    continue;
                }
            };
            let finding = Finding {
                kind: g.kind,
                number: g.number,
                title: g.title,
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
                Verdict::Unticked { .. } => unticked.push(finding),
                Verdict::Undocumented => undocumented.push(finding),
                Verdict::HeldWithReason { .. } => held.push(finding),
                Verdict::StillBlocked => {}
            }
        }

        let sections = Sections {
            stale: &stale,
            superseded: &superseded,
            unticked: &unticked,
            undocumented: &undocumented,
            held: &held,
            prose_only: &prose_only,
            unevaluated: &unevaluated,
            enumerate_error: enumerate_error.as_deref(),
            cost: &cost,
            archived,
        };

        if self.json {
            print_json(&sections);
            return Ok(());
        }

        report(&sections, self.quiet);
        Ok(())
    }
}

#[derive(serde::Serialize)]
struct PrResweepRow {
    number: i64,
    disposition: &'static str,
    reason: String,
    blockers: Vec<String>,
}

fn pr_resweep_row(pr: &batch::Gathered) -> PrResweepRow {
    let blockers = park_record::blockers(&pr.body);
    let row = |disposition, reason: String| PrResweepRow {
        number: pr.number,
        disposition,
        reason,
        blockers: blockers.iter().map(ToString::to_string).collect(),
    };
    if blockers.is_empty() {
        return row("outside-scope", "no named park record".into());
    }
    let e = match &pr.evidence {
        Ok(e) => e,
        Err(why) => return row("unevaluated", why.clone()),
    };
    for blocker in &blockers {
        let number = i64::try_from(blocker.number).unwrap_or(-1);
        let state = match &blocker.repo {
            Some(repo) => e
                .remote
                .iter()
                .find(|r| r.number == number && r.repo.eq_ignore_ascii_case(repo))
                .map(|r| r.state.as_str()),
            None => e
                .prose
                .iter()
                .find(|r| r.number == number)
                .map(|r| r.state.as_str()),
        };
        match state {
            Some(state) if resolved(state) => {}
            Some(state @ ("OPEN" | "CLOSED_UNMERGED")) => {
                return row("still-blocked", format!("{blocker} is {state}"))
            }
            _ => return row("unevaluated", format!("{blocker} has no known state")),
        }
    }
    if let Some(hold) = e.champion_hold {
        return row("held", hold.reason().into());
    }
    if crate::work_finder::is_operator_hold(&pr.labels)
        || pr.labels.iter().any(|l| l == "loom:frozen")
    {
        return row("held", "operator or frozen hold".into());
    }
    if let Some(held) = &e.held {
        return row("held", held.reason.clone());
    }
    if park_record::parse(&pr.body)
        .iter()
        .any(|record| record.blocker.is_none())
    {
        return row("held", "additional park record without a named blocker".into());
    }
    // The standard advisory already checks permanent and trusted newer holds.
    // Remove the live merge block so those reasons can take priority here too.
    let mut without_merge_block = e.clone();
    without_merge_block.self_block = None;
    if let Verdict::Superseded { block, .. } = classify(&without_merge_block) {
        return row("held", block);
    }
    if let Some(conflict) = &e.self_block {
        return row(
            "routable-to-doctor",
            format!("{conflict}; separate conflict recovery required"),
        );
    }
    if e.mergeability_confirmed {
        row("releasable", "all named blockers resolved".into())
    } else {
        row("unevaluated", "the PR has no confirmed mergeability".into())
    }
}

fn report_pr_resweep(gathering: &batch::Gathering, limit: u32, json: bool, quiet: bool) {
    let prs: Vec<_> = gathering
        .items
        .iter()
        .filter(|g| g.kind == Artifact::Pr)
        .map(pr_resweep_row)
        .collect();
    let reached_limit = prs.len() == limit as usize;
    if json {
        println!(
            "{}",
            serde_json::json!({
                "pr_resweep": prs,
                "reached_limit": reached_limit,
                "enumerate_error": gathering.enumerate_error,
                "archived": gathering.archived,
                "forge_cost": gathering.cost,
            })
        );
        return;
    }
    if let Some(error) = &gathering.enumerate_error {
        eprintln!("Parked PR listing could not be evaluated: {error}");
    } else if prs.is_empty() && !quiet {
        println!("No parked PRs to resweep.");
    }
    if reached_limit {
        eprintln!("Parked PR resweep reached its {limit} limit; additional PRs may be unexamined.");
    }
    for pr in prs {
        println!("PR #{} {} — {}", pr.number, pr.disposition, pr.reason);
    }
}

/// The classified populations, passed as one struct so adding a section does not
/// grow every signature.
struct Sections<'a> {
    stale: &'a [Finding],
    superseded: &'a [Finding],
    unticked: &'a [Finding],
    undocumented: &'a [Finding],
    /// Parked with a stated reason and no numbered blocker (#10558). Reported,
    /// but not a warning: the record is the documentation.
    held: &'a [Finding],
    prose_only: &'a [Finding],
    unevaluated: &'a [(String, String)],
    enumerate_error: Option<&'a str>,
    cost: &'a budget::ForgeCost,
    /// The archived probe's answer (#10562); `None` when it did not answer.
    archived: Option<bool>,
}

impl Sections<'_> {
    /// Whether anything at all needs a human's attention.
    fn any(&self) -> bool {
        !self.stale.is_empty()
            || !self.superseded.is_empty()
            || !self.unticked.is_empty()
            || !self.undocumented.is_empty()
            || !self.prose_only.is_empty()
    }
}

/// The human report: a bordered stderr warning when anything was found, plus
/// the suppressible one-line stdout confirmation when nothing was.
///
/// Deliberately uncoloured, unlike the three sibling shell scripts: every
/// caller of this on the sweep path captures stderr to a log file, which is the
/// same reasoning `script_helpers::emit` records for its own diagnostics.
fn report(s: &Sections<'_>, quiet: bool) {
    render(&mut std::io::stderr(), &mut std::io::stdout(), s, quiet);
}

/// [`report`] over arbitrary writers, so a test can assert section placement.
fn render(w: &mut impl Write, out: &mut impl Write, s: &Sections<'_>, quiet: bool) {
    // Nothing was listed, so the count is unknown and the line says so. It
    // goes to stdout like the clear-confirmation, so `--quiet` suppresses it.
    if s.archived == Some(true) {
        if !quiet {
            let _ = writeln!(
                out,
                "[stale-blocked] repository is archived; its open loom:blocked artifacts were \
                 not evaluated (read-only: no role can act on them)."
            );
        }
        return;
    }
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
        let n = s.stale.len()
            + s.superseded.len()
            + s.unticked.len()
            + s.undocumented.len()
            + s.prose_only.len();
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

    if !s.unticked.is_empty() {
        let _ = writeln!(w);
        let _ = writeln!(
            w,
            "CHECKLIST REFS RESOLVED, BOXES UNTICKED: confirm each condition, tick it, or \
             unpark ({}):",
            s.unticked.len()
        );
        for f in s.unticked {
            let _ = writeln!(w, "  {} {}", f.reference(), f.title);
            if let Verdict::Unticked {
                resolved_refs,
                unparsed,
            } = &f.verdict
            {
                if !resolved_refs.is_empty() {
                    let _ = writeln!(w, "      - refs resolved: {}", resolved_refs.join(", "));
                }
                if *unparsed > 0 {
                    let _ =
                        writeln!(w, "      - {unparsed} unchecked line(s) carry no readable ref");
                }
            }
        }
        let _ = writeln!(
            w,
            "  An unticked box is unmet: a merge or close does not prove its whole condition."
        );
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

    // Informational, not a warning: a held-with-reason issue needs no action,
    // so `--quiet` (the sweep pre-flight) never prints it (#10602).
    if !s.held.is_empty() {
        let _ = writeln!(
            w,
            "HELD WITH A STATED REASON — a park record states why, no numbered blocker ({}):",
            s.held.len()
        );
        for f in s.held {
            let (by, reason) = match &f.verdict {
                Verdict::HeldWithReason { by, reason } => (by.as_deref().unwrap_or("?"), reason),
                _ => ("?", &String::new()),
            };
            let _ = writeln!(w, "  {} {} (by {by}: {reason})", f.reference(), f.title);
        }
    }

    // Only when something was examined: an empty population stays silent.
    if s.cost.projected != budget::Projection::default() {
        let _ = writeln!(w, "{}", s.cost.summary());
        if let Some(why) = s
            .cost
            .budget_refused
            .as_deref()
            .or(s.cost.budget_stopped.as_deref())
        {
            let _ = writeln!(w, "[stale-blocked] {why}");
        }
    }

    if s.any() {
        let _ = writeln!(
            out,
            "[stale-blocked] WARNING: {} stale, {} superseded, {} unticked, {} undocumented, \
             {} prose-only loom:blocked artifact(s). See stderr for details.",
            s.stale.len(),
            s.superseded.len(),
            s.unticked.len(),
            s.undocumented.len(),
            s.prose_only.len()
        );
    } else if s.enumerate_error.is_some() {
        let _ = writeln!(
            out,
            "[stale-blocked] could not enumerate loom:blocked artifacts; see stderr."
        );
    } else {
        let _ = writeln!(out, "[stale-blocked] no stale, superseded, undocumented or prose-only loom:blocked artifacts.");
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
    let unticked_json: Vec<_> = s
        .unticked
        .iter()
        .map(|f| {
            let mut v = row(f);
            if let Verdict::Unticked {
                resolved_refs,
                unparsed,
            } = &f.verdict
            {
                v["resolved_refs"] = serde_json::json!(resolved_refs);
                v["unparsed"] = serde_json::json!(unparsed);
            }
            v
        })
        .collect();
    let undoc_json: Vec<_> = s.undocumented.iter().map(row).collect();
    let held_json: Vec<_> = s
        .held
        .iter()
        .map(|f| {
            let mut v = row(f);
            if let Verdict::HeldWithReason { by, reason } = &f.verdict {
                v["by"] = serde_json::json!(by);
                v["reason"] = serde_json::json!(reason);
            }
            v
        })
        .collect();
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
            "unticked": unticked_json,
            "undocumented": undoc_json,
            "held_with_reason": held_json,
            "prose_only": prose_only_json,
            "unevaluated": uneval_json,
            "enumerate_error": s.enumerate_error,
            "forge_cost": s.cost,
            "archived": s.archived,
        })
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn named_pr_resweep_distinguishes_release_conflict_holds_and_unreadable() {
        use crate::champion_hold::HoldKind;
        use crate::dep_recheck::premise::Ref;
        use crate::stale_blocked::Evidence;

        let mut pr = batch::Gathered {
            kind: Artifact::Pr,
            number: 5991,
            title: String::new(),
            evidence: Ok(Evidence {
                prose: [(6343, "CLOSED"), (6348, "MERGED"), (6354, "MERGED")]
                    .into_iter()
                    .map(|(number, state)| Ref {
                        number,
                        state: state.into(),
                    })
                    .collect(),
                mergeability_confirmed: true,
                ..Evidence::default()
            }),
            cause: None,
            labels: vec![
                "loom:blocked".into(),
                "loom:changes-requested".into(),
                "loom:ci-failure".into(),
            ],
            body: "<!-- loom:park Blocked by: #6343 by=judge -->\n\
                   <!-- loom:park Blocked by: #6348 by=judge -->\n\
                   <!-- loom:park Blocked by: #6354 by=judge -->"
                .into(),
        };
        assert_eq!(pr_resweep_row(&pr).disposition, "releasable");
        let eligible = pr.evidence.clone();

        for (state, disposition) in [
            ("OPEN", "still-blocked"),
            ("CLOSED_UNMERGED", "still-blocked"),
            ("UNKNOWN", "unevaluated"),
        ] {
            pr.evidence.as_mut().unwrap().prose[0].state = state.into();
            assert_eq!(pr_resweep_row(&pr).disposition, disposition);
        }
        pr.evidence = eligible;
        pr.evidence.as_mut().unwrap().mergeability_confirmed = false;
        assert_eq!(pr_resweep_row(&pr).disposition, "unevaluated");
        pr.evidence.as_mut().unwrap().self_block = Some("merge conflict".into());
        assert_eq!(pr_resweep_row(&pr).disposition, "routable-to-doctor");
        for hold in [
            HoldKind::Ratification,
            HoldKind::MergeRisk,
            HoldKind::CriticalFile,
        ] {
            pr.evidence.as_mut().unwrap().champion_hold = Some(hold);
            assert_eq!(pr_resweep_row(&pr).reason, hold.reason());
        }
        pr.evidence.as_mut().unwrap().champion_hold = None;
        pr.labels.push("loom:operator-blocked".into());
        assert_eq!(pr_resweep_row(&pr).disposition, "held");
        pr.labels.pop();
        let named = pr.body.clone();
        let own_diff =
            "<!-- loom:park Blocked by: (unstated) by=judge reason=\"defect in this diff\" -->";
        pr.body.push('\n');
        pr.body.push_str(own_diff);
        assert_eq!(pr_resweep_row(&pr).disposition, "held");
        pr.body = own_diff.into();
        assert_eq!(pr_resweep_row(&pr).disposition, "outside-scope");

        pr.body = named;
        pr.evidence = Err("forge read failed".into());
        assert_eq!(pr_resweep_row(&pr).disposition, "unevaluated");
    }

    /// #9191: the command examines one repository per invocation, and its
    /// help must say so. "Fleet-wide" invited a hand-rolled loop over many
    /// repositories, which is the one caller shape that is unsafe to write.
    /// Both renderings are pinned: the one-line summary `loom-daemon --help`
    /// lists, and the subcommand's own `--help`.
    #[test]
    fn help_says_per_repository_and_never_fleet_wide() {
        use crate::cli::whole_cli_parse::{on_stack, MAIN_THREAD_STACK};
        let (summary, long) = on_stack("stale-blocked-help", MAIN_THREAD_STACK, || {
            use clap::CommandFactory;
            let mut cli = <crate::cli::Cli as CommandFactory>::command();
            cli.build();
            let sub = cli
                .find_subcommand_mut("check-stale-blocked")
                .expect("check-stale-blocked is registered");
            let summary = sub.get_about().map(ToString::to_string).unwrap_or_default();
            (summary, sub.render_long_help().to_string())
        });
        // Help is re-wrapped to the terminal width, so compare on single spaces.
        let flat = |s: &str| s.split_whitespace().collect::<Vec<_>>().join(" ");
        let (summary, long) = (flat(&summary), flat(&long));

        for (name, text) in [("summary", &summary), ("--help", &long)] {
            assert!(
                !text.to_lowercase().contains("fleet-wide"),
                "{name} must not call the command fleet-wide: {text}"
            );
            assert!(
                text.contains("per-repository"),
                "{name} must say the command is per-repository: {text}"
            );
            assert!(
                text.contains("one invocation examines one repository"),
                "{name} must state the one-repository scope: {text}"
            );
        }
        assert!(
            long.contains("Do not loop it across many repositories by hand"),
            "--help must warn against the hand-rolled loop: {long}"
        );
        assert!(
            long.contains("github-authentication.md"),
            "--help must point at the doc section: {long}"
        );
    }

    fn unticked() -> Finding {
        Finding {
            kind: Artifact::Issue,
            number: 42,
            title: "parked".into(),
            verdict: Verdict::Unticked {
                resolved_refs: vec!["#187".into()],
                unparsed: 0,
            },
            undeclared: false,
        }
    }

    fn rendered(quiet: bool) -> String {
        let f = [unticked()];
        let uneval = [("issue #7".to_string(), "timeout".to_string())];
        let cost = budget::ForgeCost::default();
        let s = Sections {
            stale: &[],
            superseded: &[],
            unticked: &f,
            undocumented: &[],
            held: &[],
            prose_only: &[],
            unevaluated: &uneval,
            enumerate_error: None,
            cost: &cost,
            archived: Some(false),
        };
        let (mut err, mut out) = (Vec::new(), Vec::new());
        render(&mut err, &mut out, &s, quiet);
        String::from_utf8(err).unwrap()
    }

    /// An archived repository is one stdout line and nothing on stderr
    /// (#10562); `--quiet` silences it.
    #[test]
    fn archived_repo_is_one_line() {
        let cost = budget::ForgeCost::default();
        let s = Sections {
            stale: &[],
            superseded: &[],
            unticked: &[],
            undocumented: &[],
            held: &[],
            prose_only: &[],
            unevaluated: &[],
            enumerate_error: None,
            cost: &cost,
            archived: Some(true),
        };
        for quiet in [false, true] {
            let (mut err, mut out) = (Vec::new(), Vec::new());
            render(&mut err, &mut out, &s, quiet);
            assert!(err.is_empty());
            let out = String::from_utf8(out).unwrap();
            if quiet {
                assert!(out.is_empty(), "{out}");
            } else {
                assert_eq!(out.lines().count(), 1, "{out}");
                assert!(out.contains("repository is archived"), "{out}");
                assert!(!out.contains("no stale"), "never reported clear: {out}");
            }
        }
    }

    /// The unticked section sits inside the bordered report body (#9274): listed
    /// under `--quiet`, and before the closing remedy footer.
    #[test]
    fn unticked_section_is_inside_report_body_even_when_quiet() {
        for quiet in [false, true] {
            let err = rendered(quiet);
            let section = err
                .find("CHECKLIST REFS RESOLVED, BOXES UNTICKED")
                .unwrap_or_else(|| panic!("unticked section missing (quiet={quiet}):\n{err}"));
            let uneval = err.find("NOT EVALUATED").expect("unevaluated section");
            let footer = err.find("To re-check one issue").expect("footer");
            assert!(section < uneval, "unticked must precede NOT EVALUATED:\n{err}");
            assert!(section < footer, "unticked must precede the footer:\n{err}");
            assert!(err.contains("issue #42 parked"));
        }
    }

    /// Judge (#10602): a held-with-reason issue needs no action, so a run
    /// where every issue is held prints nothing at all under `--quiet`, and
    /// without it lists them with no WARNING banner.
    #[test]
    fn quiet_prints_nothing_when_every_issue_is_held_with_a_reason() {
        let held = [Finding {
            kind: Artifact::Issue,
            number: 7,
            title: "waiting".into(),
            verdict: Verdict::HeldWithReason {
                by: Some("curator".into()),
                reason: "vendor answer".into(),
            },
            undeclared: false,
        }];
        let cost = budget::ForgeCost::default();
        let s = Sections {
            stale: &[],
            superseded: &[],
            unticked: &[],
            undocumented: &[],
            held: &held,
            prose_only: &[],
            unevaluated: &[],
            enumerate_error: None,
            cost: &cost,
            archived: Some(false),
        };
        let (mut err, mut out) = (Vec::new(), Vec::new());
        render(&mut err, &mut out, &s, true);
        assert!(err.is_empty(), "{}", String::from_utf8_lossy(&err));
        assert!(out.is_empty(), "{}", String::from_utf8_lossy(&out));

        let (mut err, mut out) = (Vec::new(), Vec::new());
        render(&mut err, &mut out, &s, false);
        let err = String::from_utf8(err).unwrap();
        assert!(err.contains("HELD WITH A STATED REASON"), "{err}");
        assert!(err.contains("issue #7 waiting (by curator: vendor answer)"), "{err}");
        assert!(!err.contains("WARNING"), "{err}");
    }
}
