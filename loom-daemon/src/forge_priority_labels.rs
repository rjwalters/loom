//! `loom-daemon forge priority-labels` (#10518): the priority labels a new PR
//! inherits from the issues its body closes.
//!
//! A PR that closes a starred issue carries the star (`loom:operator-priority`,
//! or a higher level, #9244/#10307). That copy used to be a prompt instruction
//! only, and two thirds of the PRs closing a starred issue opened without it
//! (66% across the fleet, 2026-10-03..06; 94% on the default-model launch
//! path). `create-pr.sh` and the daemon's recovery PR now ask this module
//! instead, so the PR is correct from its first second.
//!
//! # What is copied, from which issues
//!
//! - **Issues**: every closing reference in the body ([`closing_refs`]:
//!   `Closes|Fixes|Resolves #N`, minus a negated `does not fix #N`), plus every
//!   `Loom-Issue: owner/repo#N` trailer naming *this* repo — unless the body
//!   declares that issue `Part of #N`/`Contributes to #N`. A partial increment
//!   copies nothing: the trailer is additive and rides on partial PRs too.
//! - **Labels**: each label of the level table ([`crate::operator_levels`])
//!   the issue carries — operator and inherited labels alike, ascending by
//!   level; the union over every issue, deduplicated. Nothing else.
//!
//! # Fail open
//!
//! A lookup that fails never blocks the PR: the issue's labels are skipped and
//! one warning line naming the issue goes to stderr. The other issues still
//! count. Exit 0 either way (exit 2 on bad arguments, 3 on Gitea).
//!
//! # Audit (`--audit-pr`)
//!
//! After the PR exists, `--audit-pr URL` posts one comment per issue whose
//! labels were copied, in #10012 §2's inherited-star marker shape
//! ([`crate::star_liveness::inherited_star::marker`], `inherited_from=#N`), so
//! §3's removal rule treats the copied star as inherited from that issue, not
//! as the operator's own. `requested_at` is the issue's starred-at, exactly as
//! `create-issue.sh --parent` stars a child ([`crate::star_liveness::parent_link`]).
//! Best-effort: a failed post warns and moves on.

use std::collections::BTreeSet;
use std::path::Path;

use crate::merge_pr::refs::{
    closing_refs, has_unnegated_closing_ref, loom_issue_trailer_refs, partial_increment_refs,
};
use crate::operator_levels::{self, PriorityLevel};
use crate::star_liveness::forge::{GhStarForge, StarForge as _};
use crate::work_finder::operator_priority::{GhTimelineStarredAt, StarredAtSource as _};

/// The issues a PR body inherits priority labels from, ascending.
///
/// `repo` is the PR's own `owner/repo`; a `Loom-Issue:` trailer counts only
/// when it names that repo (case-insensitively). With `repo` unknown, trailers
/// are ignored rather than guessed at.
#[must_use]
pub fn source_issues(body: &str, repo: Option<&str>) -> Vec<u32> {
    let partial: BTreeSet<u64> = partial_increment_refs(body).into_iter().collect();
    let mut out: BTreeSet<u64> = closing_refs(body)
        .into_iter()
        .filter(|n| has_unnegated_closing_ref(body, *n))
        .collect();
    if let Some(repo) = repo {
        out.extend(
            loom_issue_trailer_refs(body)
                .into_iter()
                .filter(|r| r.repo.eq_ignore_ascii_case(repo) && !partial.contains(&r.issue))
                .map(|r| r.issue),
        );
    }
    out.into_iter()
        .filter_map(|n| u32::try_from(n).ok())
        .collect()
}

/// The level-table labels among `labels`, in table order (ascending level,
/// operator label before inherited label).
#[must_use]
pub fn priority_labels_in<S: AsRef<str>>(table: &[PriorityLevel], labels: &[S]) -> Vec<String> {
    operator_levels::starred_labels(table)
        .into_iter()
        .filter(|p| labels.iter().any(|l| l.as_ref() == *p))
        .map(str::to_string)
        .collect()
}

/// What the lookups over a set of issues produced.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Outcome {
    /// The union of every issue's priority labels, in table order.
    pub labels: Vec<String>,
    /// Each issue that contributed at least one label, with its labels.
    pub sources: Vec<(u32, Vec<String>)>,
    /// Each issue whose labels could not be read, with the reason.
    pub failed: Vec<(u32, String)>,
}

/// Read each issue's labels through `fetch` and fold them into an [`Outcome`].
pub fn collect(
    table: &[PriorityLevel],
    issues: &[u32],
    mut fetch: impl FnMut(u32) -> Result<Vec<String>, String>,
) -> Outcome {
    let mut out = Outcome::default();
    let mut seen: BTreeSet<String> = BTreeSet::new();
    for &issue in issues {
        match fetch(issue) {
            Ok(labels) => {
                let found = priority_labels_in(table, &labels);
                if !found.is_empty() {
                    seen.extend(found.iter().cloned());
                    out.sources.push((issue, found));
                }
            }
            Err(e) => out.failed.push((issue, e)),
        }
    }
    out.labels = operator_levels::starred_labels(table)
        .into_iter()
        .filter(|l| seen.contains(*l))
        .map(str::to_string)
        .collect();
    out
}

/// The one-line warning for an issue whose labels could not be read.
#[must_use]
pub fn warning(issue: u32, reason: &str) -> String {
    let reason = reason
        .lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("no detail");
    let reason: String = reason.trim().chars().take(200).collect();
    format!(
        "WARNING: could not read issue #{issue}'s labels ({reason}) -- its priority labels \
         (the operator star) were NOT checked or copied onto this PR; if #{issue} is starred, \
         add the star to the PR by hand (#10518)"
    )
}

/// The audit comment for labels `labels` copied from `issue` onto `pr`;
/// `starred_at` is the issue's starred-at (the marker's `requested_at`).
#[must_use]
pub fn audit_comment(issue: u32, pr: u32, labels: &[String], starred_at: Option<&str>) -> String {
    format!(
        "Priority labels copied from #{issue} at PR creation: `{}` (#10518). They are \
         inherited from #{issue}, not set on this PR by the operator.\n\n{}",
        labels.join("`, `"),
        crate::star_liveness::inherited_star::marker(issue, pr, starred_at)
    )
}

/// One issue's label names over REST (`repos/{slug}/issues/N`), so a PR filed
/// on the GraphQL-exhausted fallback path can still read them.
///
/// # Errors
///
/// When the call does not complete, exits non-zero, or answers something that
/// is not a JSON array of strings.
pub fn fetch_labels(root: &Path, slug: &str, issue: u32) -> Result<Vec<String>, String> {
    let path = format!("repos/{slug}/issues/{issue}");
    let out = crate::worktree_ops::gh::bounded_counted(
        "forge.priority_labels",
        root,
        ["api", path.as_str(), "--jq", "[.labels[].name]"],
    )
    .ok_or_else(|| "gh did not answer".to_string())?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).trim().to_string());
    }
    serde_json::from_slice(&out.stdout).map_err(|e| format!("unparseable label list: {e}"))
}

/// The `owner/repo` of the checkout at `root`, when it resolves.
fn ambient_repo(root: &Path) -> Option<String> {
    crate::worktree_ops::gh::resolve_owner_repo(root).map(|(o, r)| format!("{o}/{r}"))
}

/// Read `issues`' labels in `repo` (unresolvable: every lookup fails open).
fn collect_live(root: &Path, repo: Option<&str>, issues: &[u32]) -> Outcome {
    let Some(slug) = repo else {
        return collect(operator_levels::table(), issues, |_| {
            Err("cannot resolve owner/repo".to_string())
        });
    };
    collect(operator_levels::table(), issues, |n| fetch_labels(root, slug, n))
}

/// Everything a PR body inherits, read live. `repo` is the PR's `owner/repo`;
/// `None` resolves it from `root`. Failed lookups are in [`Outcome::failed`];
/// nothing here prints.
#[must_use]
pub fn for_body(root: &Path, body: &str, repo: Option<&str>) -> Outcome {
    let repo = repo.map(str::to_string).or_else(|| ambient_repo(root));
    let issues = source_issues(body, repo.as_deref());
    collect_live(root, repo.as_deref(), &issues)
}

/// [`for_body`]'s labels, with a [`warning`] on stderr per failed lookup —
/// the daemon's recovery-PR path (`validate_phase`), which has no shell
/// caller to print them.
#[must_use]
pub fn labels_for_body(root: &Path, body: &str) -> Vec<String> {
    let outcome = for_body(root, body, None);
    for (issue, reason) in &outcome.failed {
        eprintln!("loom-daemon: {}", warning(*issue, reason));
    }
    outcome.labels
}

/// Post the audit comment on `pr` for every issue in `outcome.sources`.
fn post_audits(root: &Path, slug: &str, pr: u32, outcome: &Outcome) {
    let mut forge = GhStarForge::new(root, slug);
    for (issue, labels) in &outcome.sources {
        let at = GhTimelineStarredAt {
            gh_bin: forge.gh_bin.clone(),
            cwd: Some(root.to_path_buf()),
            repo: Some(slug.to_string()),
        }
        .starred_at(*issue)
        .ok()
        .flatten();
        let body = audit_comment(*issue, pr, labels, at.as_deref());
        if let Err(e) = forge.post_comment(pr, &body) {
            eprintln!(
                "loom-daemon forge priority-labels: note: could not post the inherited-star \
                 audit comment on {slug}#{pr} for #{issue}: {e:#} (best-effort)"
            );
        }
    }
}

/// Arguments for the `forge priority-labels` verb ([`cli_entrypoint`]).
pub struct PriorityLabelsArgs {
    /// Explicit issue numbers (added to whatever the body names).
    pub issues: Vec<u32>,
    /// Read the PR body from this path (`-` = stdin).
    pub body_file: Option<std::path::PathBuf>,
    /// The PR's `owner/repo`; `None` resolves from the working directory.
    pub repo: Option<String>,
    /// Post the inherited-star audit comment on this PR (URL, `owner/repo#N`,
    /// or a number).
    pub audit_pr: Option<String>,
}

/// The `loom-daemon forge priority-labels` verb: print the labels to copy, one
/// per line; warn (stderr) per failed lookup; with `--audit-pr`, post the
/// audit comment for each contributing issue. See the module docs.
///
/// # Errors
///
/// When the working directory or the body file cannot be read.
pub fn cli_entrypoint(args: PriorityLabelsArgs) -> anyhow::Result<()> {
    use anyhow::Context as _;
    let root = std::env::current_dir().context("forge priority-labels: no working directory")?;
    if crate::forge_cmd::detect_forge(Some(&root)) == crate::forge_cmd::ForgeType::Gitea {
        eprintln!("loom-daemon forge priority-labels: GitHub-only; copy priority labels by hand");
        std::process::exit(crate::forge_cmd::EX_FORGE_DECLINED);
    }
    let body = match &args.body_file {
        None => String::new(),
        Some(p) if p.as_os_str() == "-" => std::io::read_to_string(std::io::stdin())?,
        Some(p) => {
            std::fs::read_to_string(p).with_context(|| format!("reading {}", p.display()))?
        }
    };
    let (audit, repo) = match args
        .audit_pr
        .as_deref()
        .map(crate::forge_comment::parse_issue_ref)
    {
        Some(None) => anyhow::bail!("--audit-pr: not a PR reference"),
        Some(Some((slug, n))) => (u32::try_from(n).ok(), args.repo.clone().or(slug)),
        None => (None, args.repo.clone()),
    };
    let repo = repo.or_else(|| ambient_repo(&root));
    let mut issues = source_issues(&body, repo.as_deref());
    issues.extend(&args.issues);
    issues.sort_unstable();
    issues.dedup();
    let outcome = collect_live(&root, repo.as_deref(), &issues);
    // The audit pass re-reads what the first call already warned about.
    for (issue, reason) in outcome.failed.iter().filter(|_| audit.is_none()) {
        eprintln!("loom-daemon forge priority-labels: {}", warning(*issue, reason));
    }
    for label in &outcome.labels {
        println!("{label}");
    }
    if let (Some(pr), Some(slug)) = (audit, repo.as_deref()) {
        post_audits(&root, slug, pr, &outcome);
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::operator_levels::LEVELS;

    const STAR: &str = "loom:operator-priority";
    const HIGH: &str = "loom:operator-high-priority";
    const INH: &str = "loom:high-priority-inherited";

    fn labels(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn every_closing_keyword_counts_and_part_of_does_not() {
        let body = "Closes #3\nFixes #1 and resolves #2\nPart of #9\nContributes to #8\n";
        assert_eq!(source_issues(body, Some("o/r")), vec![1, 2, 3]);
        assert!(source_issues("Part of #9\n", Some("o/r")).is_empty());
        assert!(source_issues("does not fix #4\n", Some("o/r")).is_empty(), "negated close");
    }

    #[test]
    fn a_trailer_counts_for_this_repo_only_and_never_on_a_partial_increment() {
        let body = "Loom-Issue: o/r#5\nLoom-Issue: other/repo#6\n";
        assert_eq!(source_issues(body, Some("O/R")), vec![5]);
        assert!(source_issues(body, None).is_empty(), "unknown repo: trailers ignored");
        let partial = "Part of #5\n\nLoom-Issue: o/r#5\n";
        assert!(source_issues(partial, Some("o/r")).is_empty());
        let both = "Closes #7\n\nLoom-Issue: o/r#7\n";
        assert_eq!(source_issues(both, Some("o/r")), vec![7]);
    }

    #[test]
    fn only_level_labels_are_copied_in_table_order() {
        let got = priority_labels_in(LEVELS, &["loom:issue", INH, STAR, "loom:operator-only"]);
        assert_eq!(got, labels(&[STAR, INH]));
        assert!(priority_labels_in(LEVELS, &["loom:building"]).is_empty());
    }

    #[test]
    fn the_union_over_issues_is_deduplicated_and_unstarred_issues_add_nothing() {
        let o = collect(LEVELS, &[1, 2, 3], |n| {
            Ok(match n {
                1 => labels(&[STAR]),
                2 => labels(&["loom:issue"]),
                _ => labels(&[HIGH, STAR]),
            })
        });
        assert_eq!(o.labels, labels(&[STAR, HIGH]));
        assert_eq!(o.sources, vec![(1, labels(&[STAR])), (3, labels(&[STAR, HIGH]))]);
        assert!(o.failed.is_empty());
    }

    #[test]
    fn a_failed_lookup_fails_open_and_names_the_issue() {
        let o = collect(LEVELS, &[4, 5], |n| {
            if n == 4 {
                Err("HTTP 502: Bad Gateway\nmore".into())
            } else {
                Ok(labels(&[STAR]))
            }
        });
        assert_eq!(o.labels, labels(&[STAR]), "the other issue still counts");
        assert_eq!(o.failed.len(), 1);
        let w = warning(o.failed[0].0, &o.failed[0].1);
        assert!(
            w.contains("#4") && w.contains("HTTP 502: Bad Gateway") && !w.contains('\n'),
            "{w}"
        );
    }

    #[test]
    fn the_audit_comment_carries_the_inherited_star_marker() {
        let c = audit_comment(10, 20, &labels(&[STAR]), Some("2026-10-01T00:00:00Z"));
        let m = crate::star_liveness::inherited_star::parse_marker(&c).unwrap();
        assert_eq!(m.root, 10);
        assert_eq!(m.requested_at.as_deref(), Some("2026-10-01T00:00:00Z"));
        assert!(c.contains("inherited_from=#10"));
    }
}
