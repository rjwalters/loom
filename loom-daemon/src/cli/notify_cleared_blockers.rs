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
//! Population: [`super::stale_blocked::list_blocked`], the fleet-wide
//! advisory's own enumeration. Filter: [`cited_among`], which runs
//! `dep_recheck`'s existing prose extractor and `## Dependencies` parser over
//! one body+comments read per artifact. Classification: the advisory's own
//! [`super::stale_blocked::gather`] + [`classify`], only for the artifacts
//! that cite a closed number. No second reference parser exists here
//! (`.loom/docs/shell-language-policy.md`).
//!
//! # This command WRITES, unlike `check-stale-blocked`
//!
//! It posts one comment per newly-cleared artifact; it never edits a label —
//! removing `loom:blocked` stays a Curator/human decision, exactly as with the
//! advisory. `--dry-run` restores a read-only preview.
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

use loom_daemon::cmd_out::CmdOutcome;
use loom_daemon::dep_recheck::{extract, forge};
use loom_daemon::script_helpers::run_gh;
use loom_daemon::stale_blocked::{cited_among, classify, Artifact, Verdict};

use super::stale_blocked::{gather, list_blocked, DEFAULT_LIMIT};

/// The idempotency marker's prefix. Rendered as `{MARKER_PREFIX}<N> -->`.
const MARKER_PREFIX: &str = "<!-- loom:blocker-cleared:#";

#[derive(clap::Args)]
pub(crate) struct NotifyClearedBlockersArgs {
    /// Issue/PR number(s) that just closed or merged. Repeatable, and also
    /// accepts several values after one flag, so one merge is one scan.
    #[arg(long, value_name = "N", num_args = 1.., required_unless_present = "pr")]
    pub closed: Vec<i64>,

    /// A just-merged PR: adds the PR itself plus every issue it closed
    /// (`closingIssuesReferences`, the same source as `merge-pr.sh`'s
    /// `forge_pr_close_targets`) to the closed set. What `merge-pr.sh` passes.
    #[arg(long, value_name = "N")]
    pub pr: Option<i64>,

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

        let mut kinds = vec![Artifact::Issue];
        if !self.no_prs {
            kinds.push(Artifact::Pr);
        }

        let mut candidates: Vec<(Artifact, i64)> = Vec::new();
        for kind in kinds {
            let (rows, err) = list_blocked(kind, &root, repo, self.limit);
            if let Some(why) = err {
                unread.push(format!("{} population: {why}", kind.label()));
            }
            candidates.extend(rows.into_iter().map(|r| (kind, r.number)));
        }

        // Bounded-parallel reads: this runs inside merge-pr.sh's post-merge
        // path, where a serial scan of a ~20-artifact population measured
        // ~70s. Order is preserved (chunks are joined in order).
        let dry_run = self.dry_run;
        let mut notified: Vec<Notified> = Vec::new();
        for chunk in candidates.chunks(READ_CONCURRENCY) {
            let outcomes: Vec<Outcome> = std::thread::scope(|s| {
                let handles: Vec<_> = chunk
                    .iter()
                    .map(|&(kind, number)| {
                        let (closed, root) = (&closed, &root);
                        s.spawn(move || evaluate(kind, number, closed, repo, root, dry_run))
                    })
                    .collect();
                handles
                    .into_iter()
                    .map(|h| h.join().unwrap_or(Outcome::Skip))
                    .collect()
            });
            for o in outcomes {
                match o {
                    Outcome::Skip => {}
                    Outcome::Unread(why) => unread.push(why),
                    Outcome::Notified(n) => notified.push(n),
                }
            }
        }

        report(&notified, &closed, &unread, self.dry_run, self.quiet);
        Ok(())
    }
}

/// How many artifacts are read concurrently.
const READ_CONCURRENCY: usize = 8;

enum Outcome {
    Skip,
    Unread(String),
    Notified(Notified),
}

/// Evaluate one open `loom:blocked` artifact against the closed set, posting
/// the comment unless `dry_run`.
fn evaluate(
    kind: Artifact,
    number: i64,
    closed: &[i64],
    repo: Option<&str>,
    root: &Path,
    dry_run: bool,
) -> Outcome {
    let unread = |why: String| Outcome::Unread(format!("{} #{number}: {why}", kind.label()));
    let input = match fetch_input(kind, number, repo, root) {
        Ok(i) => i,
        Err(why) => return unread(why),
    };
    let cited: Vec<i64> = cited_among(kind, &input, closed)
        .into_iter()
        .filter(|n| !has_marker(&input, *n))
        .collect();
    if cited.is_empty() {
        return Outcome::Skip;
    }
    let evidence = match gather(kind, number, repo, root) {
        Ok(e) => e,
        Err(why) => return unread(why),
    };
    let reasons = match classify(&evidence) {
        Verdict::Stale(reasons) => reasons,
        Verdict::Superseded { cleared, .. } => cleared,
        // StillBlocked: the forge does not (yet) read the cited number as
        // resolved. Undocumented cannot follow a citation. Neither is a
        // cleared block worth a comment.
        Verdict::Undocumented | Verdict::StillBlocked => return Outcome::Skip,
    };
    let posted = !dry_run && post_comment(kind, number, &cited, &reasons, repo, root);
    Outcome::Notified(Notified {
        kind,
        number,
        cited,
        reasons,
        posted,
    })
}

/// The issues a merged PR closed, per the forge's own `closingIssuesReferences`.
/// Also how a Judge's checkout finds its issue (`attend_hook`, #10120).
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

fn fetch_input(
    kind: Artifact,
    number: i64,
    repo: Option<&str>,
    root: &Path,
) -> Result<extract::Input, String> {
    match kind {
        Artifact::Issue => forge::fetch_body_and_comments(number, repo, root),
        Artifact::Pr => forge::fetch_pr_body_and_comments(number, repo, root),
    }
    .map_err(|e| e.to_string())
}

fn marker_for(closed: i64) -> String {
    format!("{MARKER_PREFIX}{closed} -->")
}

/// Whether this artifact already carries `closed`'s marker (idempotency).
fn has_marker(input: &extract::Input, closed: i64) -> bool {
    let marker = marker_for(closed);
    input.body.contains(&marker) || input.comments.iter().any(|c| c.body.contains(&marker))
}

/// The notification comment. Pure, so the wording is unit-tested.
fn comment_body(kind: Artifact, cited: &[i64], reasons: &[String]) -> String {
    let refs: Vec<String> = cited.iter().map(|n| format!("#{n}")).collect();
    let mut body = format!(
        "**Cited blocker cleared**: {} just closed, and this {}'s `loom:blocked` cites it. \
         The block now reads as stale:\n",
        refs.join(", "),
        kind.label()
    );
    for r in reasons {
        body.push_str(&format!("- {r}\n"));
    }
    body.push_str(
        "\nRe-check whether the block still applies and remove `loom:blocked` if not. \
         Posted at merge time by `loom-daemon notify-cleared-blockers` (#9102), rather \
         than waiting for the next `check-stale-blocked` sweep pass (#8927).\n",
    );
    for n in cited {
        body.push('\n');
        body.push_str(&marker_for(*n));
    }
    body
}

/// Post the notification comment. Returns whether `gh` reported success.
fn post_comment(
    kind: Artifact,
    number: i64,
    cited: &[i64],
    reasons: &[String],
    repo: Option<&str>,
    root: &Path,
) -> bool {
    let entity = match kind {
        Artifact::Issue => "issue",
        Artifact::Pr => "pr",
    };
    let n = number.to_string();
    let body = comment_body(kind, cited, reasons);
    let mut args = vec![entity, "comment", n.as_str(), "--body", body.as_str()];
    if let Some(r) = repo {
        args.extend(["--repo", r]);
    }
    matches!(run_gh(&args, root, false), CmdOutcome::Ran(o) if o.status.success())
}

/// Stdout: what was notified (or nothing). Stderr: what could not be read or
/// posted — reported as unknown, never folded into "nothing to notify".
fn report(notified: &[Notified], closed: &[i64], unread: &[String], dry_run: bool, quiet: bool) {
    let refs: Vec<String> = closed.iter().map(|n| format!("#{n}")).collect();
    let refs = refs.join(", ");
    let mut out = std::io::stdout();
    let mut err = std::io::stderr();
    if notified.is_empty() && !quiet {
        let _ = writeln!(
            out,
            "[notify-cleared-blockers] no open loom:blocked artifact newly cleared by {refs}."
        );
    }
    for n in notified {
        let verb = match (dry_run, n.posted) {
            (true, _) => "would notify",
            (false, true) => "notified",
            (false, false) => "FAILED to notify",
        };
        let cited: Vec<String> = n.cited.iter().map(|c| format!("#{c}")).collect();
        let line = format!(
            "[notify-cleared-blockers] {verb} {} #{} (cites {})",
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
