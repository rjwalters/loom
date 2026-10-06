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
//! Best-effort: a failed post warns and moves on. The target repo is resolved
//! once ([`audit_target`]: `--repo`, else the reference's own `owner/repo`; a
//! qualified reference disagreeing with `--repo` is refused) and that exact
//! repo passes [`crate::write_scope::may_write_from`] before any POST.

use std::collections::BTreeSet;
use std::path::Path;

use crate::merge_pr::refs::{
    closing_refs, has_unnegated_closing_ref, loom_issue_trailer_refs, partial_increment_refs,
};
use crate::operator_levels::{self, PriorityLevel};
use crate::star_liveness::forge::{GhStarForge, StarForge};
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

/// The audit PR number and the repository the whole call targets, resolved
/// once from `--audit-pr` and `--repo`: `--repo` when given, else the audit
/// reference's own `owner/repo` (`None`: the checkout's). A qualified audit
/// reference naming a different repo than `--repo` is refused.
///
/// # Errors
///
/// `--audit-pr` is not a PR reference, or disagrees with `--repo`.
pub fn audit_target(
    audit_pr: Option<&str>,
    repo: Option<&str>,
) -> Result<(Option<u32>, Option<String>), String> {
    let repo = repo.map(str::trim).filter(|r| !r.is_empty());
    let Some(reference) = audit_pr else {
        return Ok((None, repo.map(str::to_string)));
    };
    let (slug, n) = crate::forge_comment::parse_issue_ref(reference)
        .ok_or_else(|| format!("--audit-pr: `{reference}` is not a PR reference"))?;
    let pr = u32::try_from(n).map_err(|_| format!("--audit-pr: #{n} is out of range"))?;
    match (repo, slug) {
        (Some(r), Some(s)) if !r.eq_ignore_ascii_case(&s) => Err(format!(
            "--audit-pr names {s} but --repo is {r}; refusing to post across repositories"
        )),
        (Some(r), _) => Ok((Some(pr), Some(r.to_string()))),
        (None, s) => Ok((Some(pr), s)),
    }
}

/// Vet `slug` with `may_write` (production: [`crate::write_scope::may_write_from`]
/// on exactly that repo), then post the audit comment on `pr` for every issue
/// in `outcome.sources` through the forge `forge_for` builds for the allowed
/// repo. A denial posts nothing. Returns the number of comments posted.
///
/// # Errors
///
/// The write-scope gate denied `slug`.
fn post_vetted(
    slug: &str,
    pr: u32,
    outcome: &Outcome,
    may_write: impl FnOnce(&str) -> crate::write_scope::Verdict,
    forge_for: impl FnOnce(&str) -> Box<dyn StarForge + '_>,
    mut starred_at: impl FnMut(&str, u32) -> Option<String>,
) -> anyhow::Result<usize> {
    let slug = match may_write(slug) {
        crate::write_scope::Verdict::Allow(nwo) => nwo,
        crate::write_scope::Verdict::Deny(why) => {
            anyhow::bail!(
                "forge priority-labels: refusing the audit comment on {slug}#{pr} (#9548): {why}"
            )
        }
    };
    let mut forge = forge_for(&slug);
    let mut posted = 0;
    for (issue, labels) in &outcome.sources {
        let at = starred_at(&slug, *issue);
        let body = audit_comment(*issue, pr, labels, at.as_deref());
        match forge.post_comment(pr, &body) {
            Ok(()) => posted += 1,
            Err(e) => eprintln!(
                "loom-daemon forge priority-labels: note: could not post the inherited-star \
                 audit comment on {slug}#{pr} for #{issue}: {e:#} (best-effort)"
            ),
        }
    }
    Ok(posted)
}

/// [`post_vetted`] against the live forge from `root`.
fn post_audits(root: &Path, slug: &str, pr: u32, outcome: &Outcome) -> anyhow::Result<usize> {
    post_vetted(
        slug,
        pr,
        outcome,
        |s| crate::write_scope::may_write_from(root, Some(s)),
        |s| Box::new(GhStarForge::new(root, s)),
        |s, issue| {
            GhTimelineStarredAt {
                gh_bin: std::path::PathBuf::from(crate::gh_invocation::gh_bin()),
                cwd: Some(root.to_path_buf()),
                repo: Some(s.to_string()),
            }
            .starred_at(issue)
            .ok()
            .flatten()
        },
    )
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
    // Resolved once: the lookups, the write-scope gate and the POST all use
    // this one repo (#10518 review).
    let (audit, repo) =
        audit_target(args.audit_pr.as_deref(), args.repo.as_deref()).map_err(anyhow::Error::msg)?;
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
        post_audits(&root, slug, pr, &outcome)?;
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

    // #10518 review: the repo `--audit-pr` posts to is the repo that is vetted.
    use crate::star_liveness::tests::fake::World;
    use crate::write_scope::Verdict;

    /// One starred issue (#1) contributing to the audit.
    fn starred() -> Outcome {
        collect(LEVELS, &[1], |_| Ok(labels(&[STAR])))
    }

    /// A gate that allows only `o/r`, recording every repo it was asked about.
    fn gate(asked: &std::cell::RefCell<Vec<String>>) -> impl FnOnce(&str) -> Verdict + '_ {
        move |slug: &str| {
            asked.borrow_mut().push(slug.to_string());
            if slug.eq_ignore_ascii_case("o/r") {
                Verdict::Allow("o/r".into())
            } else {
                Verdict::Deny(format!("{slug} is not a managed repository"))
            }
        }
    }

    #[test]
    fn an_unmanaged_audit_url_is_vetted_as_itself_and_denied_with_zero_posts() {
        let (pr, repo) = audit_target(Some("https://github.com/other/repo/pull/2"), None).unwrap();
        let (pr, slug) = (pr.unwrap(), repo.unwrap());
        assert_eq!((pr, slug.as_str()), (2, "other/repo"), "the URL's repo is the target");
        let (world, asked) = (World::default(), std::cell::RefCell::new(Vec::new()));
        let r = post_vetted(&slug, pr, &starred(), gate(&asked), |s| world.forge(s), |_, _| None);
        assert!(r.unwrap_err().to_string().contains("other/repo"));
        assert_eq!(*asked.borrow(), vec!["other/repo".to_string()], "vets the posted repo");
        assert!(world.posted("other/repo").is_empty() && world.posted("o/r").is_empty());
    }

    #[test]
    fn an_audit_reference_disagreeing_with_repo_is_refused() {
        for r in ["https://github.com/other/repo/pull/2", "other/repo#2"] {
            let e = audit_target(Some(r), Some("o/r")).unwrap_err();
            assert!(e.contains("other/repo") && e.contains("o/r"), "{e}");
        }
        assert!(audit_target(Some("not a ref"), Some("o/r")).is_err());
        // Same repo (any case), or an unqualified number, is not a mismatch.
        let same = audit_target(Some("https://github.com/O/R/pull/2"), Some("o/r")).unwrap();
        assert_eq!(same, (Some(2), Some("o/r".to_string())));
        assert_eq!(audit_target(Some("7"), Some("o/r")).unwrap(), (Some(7), Some("o/r".into())));
        assert_eq!(audit_target(None, Some("o/r")).unwrap(), (None, Some("o/r".into())));
    }

    #[test]
    fn a_permitted_matching_target_posts_one_audit_per_issue() {
        let (pr, repo) = audit_target(Some("https://github.com/o/r/pull/2"), Some("o/r")).unwrap();
        let (world, asked) = (World::default(), std::cell::RefCell::new(Vec::new()));
        let n = post_vetted(
            repo.as_deref().unwrap(),
            pr.unwrap(),
            &starred(),
            gate(&asked),
            |s| world.forge(s),
            |_, _| Some("2026-10-01T00:00:00Z".into()),
        )
        .unwrap();
        assert_eq!(n, 1);
        assert_eq!(*asked.borrow(), vec!["o/r".to_string()]);
        let posted = world.posted("o/r");
        assert_eq!(posted.len(), 1);
        assert_eq!(posted[0].0, 2);
        assert!(posted[0].1.contains("inherited_from=#1"));
    }
}
