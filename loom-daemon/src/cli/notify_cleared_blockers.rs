//! `loom-daemon notify-cleared-blockers` — the close-triggered `loom:blocked`
//! re-check (issue #9102, item 2 of #8927's deferred "Suggested fix" list).
//!
//! # The gap this closes
//!
//! `loom-daemon check-stale-blocked` (#8927) re-checks every open
//! `loom:blocked` artifact once per `/loom:sweep` pre-wave, which finds a stale
//! block within days but never *the moment it goes stale*. In #8927's incident,
//! issue #178 sat blocked for eleven months on a blocker that closed the day
//! after the block was applied. This command watches the close event itself:
//! `merge-pr.sh`'s post-merge path calls it once per merge, with the merged PR
//! and every issue that merge closed as `--closed`, right beside the
//! closed-issue `loom:building` cleanup (#6199).
//!
//! # Reused, not re-parsed
//!
//! Population, text and evidence: [`batch`]'s REST + ETag gatherer, the one
//! `check-stale-blocked` runs, through [`notify::gather_cited`] (#10515). That
//! filter runs [`crate::stale_blocked::cited_among`] (`dep_recheck`'s
//! prose extractor and `## Dependencies` parser) over each artifact's text, so
//! the closing-PR and blocker-state reads run only for the artifacts that cite
//! a closed number. Classification: the advisory's own [`classify`]. No second
//! reference parser exists here (`.loom/docs/shell-language-policy.md`).
//!
//! # Cost (#10515)
//!
//! This runs once per merge. The per-artifact `gh issue view` gatherer it
//! used to run cost one GraphQL read per open `loom:blocked` artifact per
//! merge, plus three or more per citer — thousands of points a day. Now: one
//! ETag'd REST listing, a comment walk only where there are comments (mostly
//! `304`s, and warmed by the sweep's `check-stale-blocked` pass), and at most
//! one GraphQL point per 100 citers. The one remaining GraphQL read is `--pr`'s
//! closing references.
//!
//! # This command WRITES, unlike `check-stale-blocked`
//!
//! It posts one comment per newly-cleared artifact. It edits labels only in the
//! one carve-out of #13331 ([`unpark_eligible`]): an issue parked by the
//! Curator whose every blocker has resolved gets `loom:blocked` swapped for
//! `loom:curated` (never `loom:issue` — promotion stays Champion/human), and
//! the comment states the transition. Every other case stays comment-only.
//! `--dry-run` restores a read-only preview.
//!
//! # Idempotency
//!
//! Each posted comment carries one `<!-- loom:blocker-cleared:#<N> -->` marker
//! per closed number it reports. A candidate already carrying the marker for
//! every closed number it cites is skipped, so a re-run (or a retried merge)
//! never double-posts.
//!
//! # Best-effort, always exit 0
//!
//! The merge already happened; nothing here may fail it. Unreadable artifacts
//! are skipped and named on stderr, never guessed about.

use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::Result;

use crate::cmd_out::CmdOutcome;
use crate::script_helpers::run_gh;
use crate::stale_blocked::notify::{self, marker_for};
use crate::stale_blocked::{batch, budget, classify, resolved, Artifact, Evidence, Verdict};

use super::stale_blocked::DEFAULT_LIMIT;

#[derive(clap::Args)]
pub(crate) struct NotifyClearedBlockersArgs {
    /// Issue/PR number(s) that just closed or merged. Repeatable, and also
    /// accepts several values after one flag, so one merge is one scan.
    #[arg(long, value_name = "N", num_args = 1.., required_unless_present_any = ["pr", "resweep"])]
    pub closed: Vec<i64>,

    /// A just-merged PR: adds the PR itself plus every issue it closed
    /// (`closingIssuesReferences`, the same source as `merge-pr.sh`'s
    /// `forge_pr_close_targets`) to the closed set. What `merge-pr.sh` passes.
    #[arg(long, value_name = "N")]
    pub pr: Option<i64>,

    /// Re-sweep every open `loom:blocked` issue instead of reacting to a close:
    /// unpark (and comment on) each one that is `unpark_eligible`, even when it
    /// already carries a `loom:blocker-cleared` marker from an earlier
    /// comment-only notification. Non-eligible items are left untouched.
    /// Conflicts with `--closed` and `--pr`.
    #[arg(long, conflicts_with_all = ["closed", "pr"])]
    pub resweep: bool,

    /// Print nothing when nothing was newly cleared.
    #[arg(long)]
    pub quiet: bool,

    /// Report what would be posted without posting it.
    #[arg(long)]
    pub dry_run: bool,

    /// Repository to scan, as `owner/name`. Defaults to whatever `gh` resolves
    /// from `--repo-root`.
    #[arg(long, value_name = "OWNER/NAME")]
    pub repo: Option<String>,

    /// Directory to run `gh` from. Defaults to the current directory.
    #[arg(long, value_name = "PATH")]
    pub repo_root: Option<PathBuf>,

    /// Maximum number of open `loom:blocked` artifacts to examine, per
    /// population (issues and PRs are capped separately). Same default as
    /// `check-stale-blocked`.
    #[arg(long, value_name = "N", default_value_t = DEFAULT_LIMIT)]
    pub limit: u32,

    /// Skip the open `loom:blocked` pull-request population.
    #[arg(long)]
    pub no_prs: bool,
}

/// One artifact this close event newly cleared (or would, under `--dry-run`).
struct Notified {
    kind: Artifact,
    number: i64,
    cited: Vec<i64>,
    reasons: Vec<String>,
    posted: bool,
    /// `loom:blocked` was swapped for `loom:curated` (or would be, `--dry-run`).
    unparked: bool,
}

impl NotifyClearedBlockersArgs {
    /// Always `Ok(())` — see the module doc's "Best-effort" section.
    pub(crate) fn run(self) -> Result<()> {
        let root = match &self.repo_root {
            Some(r) => r.clone(),
            None => std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
        };
        let repo = self.repo.as_deref();
        let mut closed = self.closed.clone();
        let mut unread: Vec<String> = Vec::new();
        if let Some(pr) = self.pr {
            closed.push(pr);
            match pr_close_targets(pr, repo, &root) {
                Ok(targets) => closed.extend(targets),
                Err(why) => unread.push(format!("PR #{pr} closing references: {why}")),
            }
        }
        closed.sort_unstable();
        closed.dedup();

        let mut forge = batch::GhStaleBlockedForge::new(&root, repo);
        let fleet = crate::forge_identity::FleetLogins::for_root(&root);
        let opts = batch::Options {
            limit: self.limit,
            no_prs: self.no_prs,
            floor: budget::Floor::default(),
        };
        // `--resweep` skips `gather_cited`: its `has_marker` short-circuit is
        // exactly what hides the already-notified parked issues.
        let (gathering, mut cited) = if self.resweep {
            (batch::gather_all(&mut forge, &fleet, opts), Default::default())
        } else {
            let c = notify::gather_cited(&mut forge, &fleet, opts, &closed);
            (c.gathering, c.cited)
        };
        if let Some(why) = gathering.enumerate_error {
            unread.push(format!("population: {why}"));
        }

        let mut notified: Vec<Notified> = Vec::new();
        for g in gathering.items {
            let evidence = match g.evidence {
                Ok(e) => e,
                Err(why) => {
                    unread.push(format!("{} #{}: {why}", g.kind.label(), g.number));
                    continue;
                }
            };
            let cited = if self.resweep {
                resweep_cited(&evidence)
            } else {
                let Some(c) = cited.remove(&(g.kind, g.number)) else {
                    continue;
                };
                c
            };
            let reasons = match classify(&evidence) {
                Verdict::Stale(reasons) => reasons,
                Verdict::Superseded { cleared, .. } => cleared,
                // Unticked (#9274): every checklist ref resolved but a box is
                // still unticked. Not stale (an unticked box is unmet until a
                // human confirms it), yet "tick the box or drop the block" is
                // exactly the human signal this advisory exists to send.
                Verdict::Unticked {
                    resolved_refs,
                    unparsed,
                } => vec![unticked_reason(&resolved_refs, unparsed)],
                // StillBlocked: the forge does not (yet) read the cited number
                // as resolved. Undocumented cannot follow a citation. Neither
                // is a cleared block worth a comment.
                Verdict::Undocumented | Verdict::HeldWithReason { .. } | Verdict::StillBlocked => {
                    continue
                }
            };
            let eligible = unpark_eligible(g.kind, &g.labels, &g.body, &evidence);
            // A resweep acts only on what it can unpark: everything else
            // already got its advisory comment.
            if self.resweep && !eligible {
                continue;
            }
            // Label first, comment second: the comment is the audit trail and
            // must only claim a transition that actually happened.
            let unparked = eligible && (self.dry_run || unpark(g.number, repo, &root));
            // Resweep has no marker short-circuit, so a comment posted after a
            // failed unpark would repeat on every run. Report it instead.
            if resweep_skips_comment(self.resweep, self.dry_run, unparked) {
                unread.push(format!(
                    "{} #{}: unpark failed; no comment posted",
                    g.kind.label(),
                    g.number
                ));
                continue;
            }
            let posted = !self.dry_run && {
                let body = comment_body_for(g.kind, &cited, &reasons, unparked, self.resweep);
                post_comment(g.kind, g.number, &body, repo, &root)
            };
            notified.push(Notified {
                kind: g.kind,
                number: g.number,
                cited,
                reasons,
                posted,
                unparked,
            });
        }

        report(&notified, &closed, &unread, self.dry_run, self.quiet, self.resweep);
        Ok(())
    }
}

/// In `--resweep`, a comment is posted only where the unpark happened (or on a
/// dry run). Pure, so tested.
fn resweep_skips_comment(resweep: bool, dry_run: bool, unparked: bool) -> bool {
    resweep && !dry_run && !unparked
}

/// The blocker numbers an artifact's evidence cites (`--resweep` has no closed
/// set to intersect with). Local numbers only (a cross-repo number would
/// collide with a local one in the number-keyed idempotency markers). Sorted, deduplicated. Pure, so tested.
fn resweep_cited(ev: &Evidence) -> Vec<i64> {
    let mut v: Vec<i64> = ev
        .prose
        .iter()
        .map(|r| r.number)
        .chain(
            ev.named
                .iter()
                .filter(|d| d.repo.is_none())
                .map(|d| d.number),
        )
        .collect();
    v.sort_unstable();
    v.dedup();
    v
}

/// The issues a merged PR closed, per the forge's own `closingIssuesReferences`.
pub(super) fn pr_close_targets(
    pr: i64,
    repo: Option<&str>,
    root: &Path,
) -> Result<Vec<i64>, String> {
    let n = pr.to_string();
    let mut args = vec![
        "pr",
        "view",
        n.as_str(),
        "--json",
        "closingIssuesReferences",
    ];
    if let Some(r) = repo {
        args.extend(["--repo", r]);
    }
    match run_gh(&args, root, false) {
        CmdOutcome::Ran(o) if o.status.success() => parse_close_targets(&o.stdout),
        CmdOutcome::Ran(o) => Err(format!("gh pr view exited {}", o.status)),
        CmdOutcome::Unavailable(u) => Err(format!("gh pr view could not be run: {u:?}")),
    }
}

/// Parse `gh pr view --json closingIssuesReferences` output. Pure, so tested.
fn parse_close_targets(stdout: &[u8]) -> Result<Vec<i64>, String> {
    #[derive(serde::Deserialize)]
    struct Ref {
        number: i64,
    }
    #[derive(serde::Deserialize)]
    struct View {
        #[serde(default, rename = "closingIssuesReferences")]
        refs: Vec<Ref>,
    }
    serde_json::from_slice::<View>(stdout)
        .map(|v| v.refs.into_iter().map(|r| r.number).collect())
        .map_err(|e| format!("unreadable closingIssuesReferences JSON: {e}"))
}

/// The reason line for a [`Verdict::Unticked`] citer (#9274). Pure, so the
/// wording is unit-tested.
fn unticked_reason(resolved_refs: &[String], unparsed: usize) -> String {
    let mut r = "the cited `## Dependencies` ref closed, but its checklist box is still unticked"
        .to_string();
    if !resolved_refs.is_empty() {
        r.push_str(&format!(": {}", resolved_refs.join(", ")));
    }
    if unparsed > 0 {
        r.push_str(&format!(" (plus {unparsed} unchecked line(s) with no readable ref)"));
    }
    r.push_str(". Tick the box once its whole condition is confirmed, or drop the block");
    r
}

/// The close-triggered notification comment. Pure, so the wording is
/// unit-tested; production code goes through [`comment_body_for`].
#[cfg(test)]
fn comment_body(kind: Artifact, cited: &[i64], reasons: &[String], unparked: bool) -> String {
    comment_body_for(kind, cited, reasons, unparked, false)
}

/// [`comment_body`] with the `--resweep` wording: there is no close event, so
/// the cited blockers are described as closed, not "just closed".
fn comment_body_for(
    kind: Artifact,
    cited: &[i64],
    reasons: &[String],
    unparked: bool,
    resweep: bool,
) -> String {
    let refs: Vec<String> = cited.iter().map(|n| format!("#{n}")).collect();
    let when = if resweep { "closed" } else { "just closed" };
    let mut body = format!(
        "**Cited blocker cleared**: {} {when}, and this {}'s `loom:blocked` cites it. \
         The block now reads as stale:\n",
        refs.join(", "),
        kind.label()
    );
    for r in reasons {
        body.push_str(&format!("- {r}\n"));
    }
    if unparked {
        body.push_str(
            "\n**Unparked**: every blocker in this issue's Curator park record has resolved, \
             so `loom:blocked` was removed and `loom:curated` restored (#13331). Promotion to \
             `loom:issue` is unchanged: Champion or a human decides.\n",
        );
    } else {
        body.push_str(
            "\nRe-check whether the block still applies and remove `loom:blocked` if not.\n",
        );
    }
    body.push_str(
        "Posted at merge time by `loom-daemon notify-cleared-blockers` (#9102), rather \
         than waiting for the next `check-stale-blocked` sweep pass (#8927).\n",
    );
    for n in cited {
        body.push('\n');
        body.push_str(&marker_for(*n));
    }
    body
}

/// Post the notification comment. Returns whether `gh` reported success.
fn post_comment(kind: Artifact, number: i64, body: &str, repo: Option<&str>, root: &Path) -> bool {
    let entity = match kind {
        Artifact::Issue => "issue",
        Artifact::Pr => "pr",
    };
    let n = number.to_string();
    let mut args = vec![entity, "comment", n.as_str(), "--body", body];
    if let Some(r) = repo {
        args.extend(["--repo", r]);
    }
    matches!(run_gh(&args, root, false), CmdOutcome::Ran(o) if o.status.success())
}

/// Whether a newly-cleared artifact may be unparked mechanically (#13331).
///
/// All must hold: an issue carrying `loom:blocked` and none of `loom:operator`,
/// `loom:frozen` or a hard-exclusion label; a body whose park records are all
/// `by=curator` and all name a blocker (a reason-only record is a deliberate
/// hold); no unticked `## Dependencies` line; and every blocker the artifact
/// cites, local or cross-repo, resolved (a PR closed unmerged does not count,
/// see [`resolved`]). `by=` is a self-declared attribute, accepted because the
/// action only restores `loom:curated` and approves nothing. Pure, so tested.
fn unpark_eligible(kind: Artifact, labels: &[String], body: &str, ev: &Evidence) -> bool {
    let has = |l: &str| labels.iter().any(|x| x == l);
    if kind != Artifact::Issue
        || !has("loom:blocked")
        || has("loom:operator")
        || has("loom:frozen")
        || crate::hard_exclusion::is_hard_excluded(labels)
    {
        return false;
    }
    let records = crate::park_record::parse(body);
    if records.is_empty()
        || !records.iter().all(|r| {
            r.blocker.is_some()
                && r.by
                    .as_deref()
                    .is_some_and(|b| b.eq_ignore_ascii_case("curator"))
        })
    {
        return false;
    }
    ev.unparsed_unchecked == 0
        && ev.named.iter().all(|d| d.checked)
        && !ev.prose.is_empty()
        && ev.prose.iter().all(|r| resolved(&r.state))
        && ev.remote.iter().all(|r| resolved(&r.state))
}

/// Swap `loom:blocked` for `loom:curated`. Returns whether `gh` reported success.
fn unpark(number: i64, repo: Option<&str>, root: &Path) -> bool {
    let n = number.to_string();
    let mut args = vec![
        "issue",
        "edit",
        n.as_str(),
        "--remove-label",
        "loom:blocked",
        "--add-label",
        "loom:curated",
    ];
    if let Some(r) = repo {
        args.extend(["--repo", r]);
    }
    matches!(run_gh(&args, root, false), CmdOutcome::Ran(o) if o.status.success())
}

/// Stdout: what was notified (or nothing). Stderr: what could not be read or
/// posted — reported as unknown, never folded into "nothing to notify".
fn report(
    notified: &[Notified],
    closed: &[i64],
    unread: &[String],
    dry_run: bool,
    quiet: bool,
    resweep: bool,
) {
    let refs: Vec<String> = closed.iter().map(|n| format!("#{n}")).collect();
    let refs = refs.join(", ");
    let mut out = std::io::stdout();
    let mut err = std::io::stderr();
    if notified.is_empty() && !quiet {
        let _ = if resweep {
            writeln!(
                out,
                "[notify-cleared-blockers] resweep: no open loom:blocked issue is eligible to unpark."
            )
        } else {
            writeln!(
                out,
                "[notify-cleared-blockers] no open loom:blocked artifact newly cleared by {refs}."
            )
        };
    }
    for n in notified {
        let verb = match (dry_run, n.posted) {
            (true, _) if resweep && n.unparked => "would unpark",
            (true, _) => "would notify",
            (false, true) => "notified",
            (false, false) => "FAILED to notify",
        };
        let cited: Vec<String> = n.cited.iter().map(|c| format!("#{c}")).collect();
        let unpark = match (n.unparked, dry_run) {
            (false, _) => "",
            (true, true) => ", would unpark to loom:curated",
            (true, false) => ", unparked to loom:curated",
        };
        let line = format!(
            "[notify-cleared-blockers] {verb} {} #{} (cites {}{unpark})",
            n.kind.label(),
            n.number,
            cited.join(", ")
        );
        if !dry_run && !n.posted {
            let _ = writeln!(err, "{line}");
        } else {
            let _ = writeln!(out, "{line}");
            for r in &n.reasons {
                let _ = writeln!(out, "    - {r}");
            }
        }
    }
    for u in unread {
        let _ = writeln!(err, "[notify-cleared-blockers] not evaluated (unknown, not clear): {u}");
    }
}

#[cfg(test)]
mod tests;
