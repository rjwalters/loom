//! `loom-daemon stack-parent`: pick the open PR a Builder's finished work
//! should be stacked on (#12759, sub-issue B of epic #12757).
//!
//! Run from the Builder worktree after the Builder commits and before the
//! pre-PR gate. It answers one question: does this branch really conflict with
//! an open Loom PR it could sit on top of? The decision is [`Decision`], printed
//! as one `STACK-PARENT …` line by [`Decision::line`].
//!
//! **Fail flat.** Anything unknown (a config read, a listing, a conflict test
//! that returns `None`) never stacks: a wrong stack costs more than a flat PR,
//! the opposite of the merge sequencer's fail-closed reading of the same test.
//!
//! [`decide`] is pure over a PR listing and a conflict predicate; [`run`] is the
//! IO shell around it.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Duration;

use crate::claim_reconciliation::merge_sequence::conflict::{
    ensure_pr_head, merge_tree_conflicts, Deadline,
};
use crate::forge_pull_listing::{list_open_pulls_cached_as, RestPull};
use crate::merge_pr::stacked_children::is_stackable_parent_branch;
use crate::stacking_config::{stacking_on_conflict, StackingOnConflict};

/// At most this many candidates are conflict-tested per run.
pub const MAX_CANDIDATES: usize = 10;

/// Wall-clock budget for the head fetches plus merge-trees of one run.
const BUDGET: Duration = Duration::from_secs(90);

/// Open-PR listing page budget (10 x 100, as the claim-reconciliation pass).
const MAX_PAGES: usize = 10;

/// Hold label on a PR that is not a hold in the registry's hold set.
const CHANGES_REQUESTED: &str = "loom:changes-requested";

/// Why no PR was chosen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoneReason {
    Disabled,
    MaxDepth0,
    ExplicitBase,
    AlreadyPushed,
    ListingFailed,
    NoConflict,
    NoneEligible,
}

impl NoneReason {
    #[must_use]
    pub fn token(self) -> &'static str {
        match self {
            Self::Disabled => "disabled",
            Self::MaxDepth0 => "max-depth-0",
            Self::ExplicitBase => "explicit-base",
            Self::AlreadyPushed => "already-pushed",
            Self::ListingFailed => "listing-failed",
            Self::NoConflict => "no-conflict",
            Self::NoneEligible => "none-eligible",
        }
    }
}

/// The verb's answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// Open the PR against the default branch, as today.
    None(NoneReason),
    /// Replay the Builder's commits onto `head` and open against `branch`.
    Stack {
        pr: u32,
        branch: String,
        head: String,
        /// Depth the new child would have (`depth(P) + 1`).
        depth: u32,
    },
}

impl Decision {
    /// The one stdout line. `fork` is the merge-base with the default branch,
    /// the `<upstream>` of the Builder's `git rebase --onto`.
    #[must_use]
    pub fn line(&self, fork: Option<&str>) -> String {
        match self {
            Self::None(r) => format!("STACK-PARENT none reason={}", r.token()),
            Self::Stack {
                pr,
                branch,
                head,
                depth,
            } => format!(
                "STACK-PARENT pr={pr} branch={branch} head={head} fork={} depth={depth}",
                fork.unwrap_or_default()
            ),
        }
    }
}

fn is_open(p: &RestPull) -> bool {
    p.state == "open"
}

fn has_hold(p: &RestPull) -> bool {
    p.labels
        .iter()
        .any(|l| l == CHANGES_REQUESTED || crate::work_finder::HOLD_NAMES.contains(&l.as_str()))
}

/// AC3: may a child be stacked on `p`?
fn eligible(p: &RestPull, own_branch: &str) -> bool {
    let Some(head_ref) = p.head_ref.as_deref() else {
        return false;
    };
    is_open(p)
        && !p.draft
        && is_stackable_parent_branch(head_ref)
        && head_ref != own_branch
        && p.head_sha.as_deref().is_some_and(|s| !s.is_empty())
        && p.base_ref.is_some()
        && !has_hold(p)
}

/// Open PRs from `p` (inclusive) down its base chain to the default branch,
/// nearest first. `None` on a cycle or an unresolvable hop.
fn chain(p: &RestPull, pulls: &[RestPull], default_branch: &str) -> Option<Vec<u32>> {
    let mut out = vec![p.number];
    let mut seen: HashSet<u32> = HashSet::from([p.number]);
    let mut base = p.base_ref.as_deref()?;
    while base != default_branch {
        let next = pulls
            .iter()
            .find(|q| is_open(q) && q.head_ref.as_deref() == Some(base))?;
        if !seen.insert(next.number) {
            return None;
        }
        out.push(next.number);
        base = next.base_ref.as_deref()?;
    }
    Some(out)
}

/// `(created_at, number)`, the sequencer's tiebreak.
fn age_key(p: &RestPull) -> (String, u32) {
    (p.created_at.clone().unwrap_or_default(), p.number)
}

/// The pure decision over a listing. `conflicts(p)` is the conflict test of the
/// Builder's work against `p`'s head; `None` means unknown (skip `p`).
#[must_use]
pub fn decide(
    cfg: StackingOnConflict,
    own_branch: &str,
    default_branch: &str,
    pulls: &[RestPull],
    conflicts: &mut dyn FnMut(&RestPull) -> Option<bool>,
) -> Decision {
    let mut cands: Vec<(&RestPull, Vec<u32>)> = pulls
        .iter()
        .filter(|p| eligible(p, own_branch))
        .filter_map(|p| chain(p, pulls, default_branch).map(|c| (p, c)))
        // AC2: the child's depth is depth(P) + 1, with depth(P) = chain - 1.
        .filter(|(_, c)| c.len() as u64 <= u64::from(cfg.max_depth))
        .collect();
    if cands.is_empty() {
        return Decision::None(NoneReason::NoneEligible);
    }
    cands.sort_by_key(|(p, _)| age_key(p));
    cands.truncate(MAX_CANDIDATES);
    let hits: Vec<&(&RestPull, Vec<u32>)> = cands
        .iter()
        .filter(|(p, _)| conflicts(p) == Some(true))
        .collect();
    if hits.is_empty() {
        return Decision::None(NoneReason::NoConflict);
    }
    // AC4: the top of one chain if a single hit has every other hit below it;
    // otherwise the oldest (hits are already oldest first).
    let top = hits.iter().find(|(p, c)| {
        hits.iter()
            .all(|(q, _)| q.number == p.number || c.contains(&q.number))
    });
    let (p, c) = top.copied().unwrap_or(hits[0]);
    Decision::Stack {
        pr: p.number,
        branch: p.head_ref.clone().unwrap_or_default(),
        head: p.head_sha.clone().unwrap_or_default(),
        depth: c.len() as u32,
    }
}

/// What [`run`] returns: the decision and the fork point to print with it.
pub struct Outcome {
    pub decision: Decision,
    pub fork: Option<String>,
}

fn git_out(dir: &Path, args: &[&str]) -> Option<String> {
    let o = std::process::Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .ok()?;
    o.status
        .success()
        .then(|| String::from_utf8_lossy(&o.stdout).trim().to_string())
}

/// Run the verb in `worktree` for issue `issue`; `base` is the base the
/// Builder was already given (`None`/default branch = flat).
#[must_use]
pub fn run(worktree: &Path, issue: u32, base: Option<&str>) -> Outcome {
    let none = |r| Outcome {
        decision: Decision::None(r),
        fork: None,
    };
    let main_root: PathBuf = crate::worktree_state::main_checkout_root(worktree)
        .unwrap_or_else(|| worktree.to_path_buf());
    let cfg = stacking_on_conflict(&main_root);
    if !cfg.enabled {
        return none(NoneReason::Disabled);
    }
    if cfg.max_depth == 0 {
        return none(NoneReason::MaxDepth0);
    }
    let default_branch = crate::worktree_ops::clean::default_branch(&main_root)
        .unwrap_or_else(|| "main".to_string());
    if base.is_some_and(|b| !b.is_empty() && b != default_branch) {
        return none(NoneReason::ExplicitBase);
    }
    let own_branch = format!("feature/issue-{issue}");
    let pushed = git_out(worktree, &["ls-remote", "--heads", "origin", &own_branch]);
    if pushed.as_deref().is_none_or(|s| !s.is_empty()) {
        // Pushed already, or unanswerable: never rewrite pushed commits.
        return none(NoneReason::AlreadyPushed);
    }
    let gh = PathBuf::from(std::env::var("LOOM_GH_BIN").unwrap_or_else(|_| "gh".into()));
    let Ok(pulls) =
        list_open_pulls_cached_as("stack_parent.pr_list", &gh, Some(&main_root), None, MAX_PAGES)
    else {
        return none(NoneReason::ListingFailed);
    };
    let Some(fork) =
        git_out(worktree, &["merge-base", "HEAD", &format!("origin/{default_branch}")])
    else {
        return none(NoneReason::ListingFailed);
    };
    let deadline: Rc<Deadline> = Deadline::new(BUDGET);
    let mut test = |p: &RestPull| {
        let sha = p.head_sha.as_deref()?;
        if !ensure_pr_head(worktree, p.number, sha, &deadline) {
            return None;
        }
        merge_tree_conflicts(worktree, "HEAD", sha)
    };
    let decision = decide(cfg, &own_branch, &default_branch, &pulls, &mut test);
    Outcome {
        decision,
        fork: Some(fork),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pr(n: u32, base: &str, created: &str) -> RestPull {
        RestPull {
            number: n,
            state: "open".into(),
            draft: false,
            title: None,
            created_at: Some(created.into()),
            updated_at: None,
            author: None,
            labels: vec![],
            head_ref: Some(format!("feature/issue-{n}")),
            head_sha: Some(format!("sha{n}")),
            base_ref: Some(base.into()),
            body: None,
            author_association: None,
            author_is_bot: false,
            head_repo: None,
        }
    }

    fn cfg(max_depth: u32) -> StackingOnConflict {
        StackingOnConflict {
            enabled: true,
            max_depth,
        }
    }

    fn pick(pulls: &[RestPull], max: u32, hot: &[u32]) -> Decision {
        let mut t = |p: &RestPull| Some(hot.contains(&p.number));
        decide(cfg(max), "feature/issue-99", "main", pulls, &mut t)
    }

    fn stacked_on(d: &Decision) -> Option<u32> {
        match d {
            Decision::Stack { pr, .. } => Some(*pr),
            Decision::None(_) => None,
        }
    }

    #[test]
    fn flat_parent_gives_child_depth_one() {
        let p = [pr(1, "main", "2026-01-01")];
        let Decision::Stack { depth, branch, .. } = pick(&p, 3, &[1]) else {
            panic!("expected stack");
        };
        assert_eq!((depth, branch.as_str()), (1, "feature/issue-1"));
    }

    #[test]
    fn max_depth_caps_and_zero_never_stacks() {
        let p = [
            pr(1, "main", "2026-01-01"),
            pr(2, "feature/issue-1", "2026-01-02"),
            pr(3, "feature/issue-2", "2026-01-03"),
        ];
        // Only #3 conflicts: it has two open-PR ancestors, so a child is depth 3.
        assert_eq!(stacked_on(&pick(&p, 2, &[3])), None);
        assert_eq!(stacked_on(&pick(&p, 3, &[3])), Some(3));
        assert_eq!(pick(&p, 0, &[1]), Decision::None(NoneReason::NoneEligible));
    }

    #[test]
    fn same_chain_picks_top_disjoint_picks_oldest() {
        let chain = [
            pr(1, "main", "2026-01-01"),
            pr(2, "feature/issue-1", "2026-01-02"),
        ];
        assert_eq!(stacked_on(&pick(&chain, 3, &[1, 2])), Some(2));
        let disjoint = [pr(5, "main", "2026-01-03"), pr(4, "main", "2026-01-03")];
        // created_at tie broken by number.
        assert_eq!(stacked_on(&pick(&disjoint, 3, &[4, 5])), Some(4));
        let older = [pr(9, "main", "2026-01-01"), pr(4, "main", "2026-01-05")];
        assert_eq!(stacked_on(&pick(&older, 3, &[4, 9])), Some(9));
    }

    #[test]
    fn eligibility_filters() {
        let mut draft = pr(1, "main", "2026-01-01");
        draft.draft = true;
        let mut human = pr(2, "main", "2026-01-01");
        human.head_ref = Some("fix/thing".into());
        let own = pr(99, "main", "2026-01-01");
        let mut all = vec![draft, human, own];
        for (i, label) in [
            "loom:blocked",
            "loom:operator",
            "loom:frozen",
            "loom:changes-requested",
        ]
        .iter()
        .enumerate()
        {
            let mut p = pr(10 + i as u32, "main", "2026-01-01");
            p.labels = vec![(*label).into()];
            all.push(p);
        }
        let hot: Vec<u32> = all.iter().map(|p| p.number).collect();
        assert_eq!(pick(&all, 3, &hot), Decision::None(NoneReason::NoneEligible));
    }

    #[test]
    fn cycle_and_missing_hop_disqualify() {
        let cyc = [
            pr(1, "feature/issue-2", "2026-01-01"),
            pr(2, "feature/issue-1", "2026-01-02"),
        ];
        assert_eq!(pick(&cyc, 9, &[1, 2]), Decision::None(NoneReason::NoneEligible));
        let orphan = [pr(1, "feature/issue-77", "2026-01-01")];
        assert_eq!(pick(&orphan, 9, &[1]), Decision::None(NoneReason::NoneEligible));
    }

    #[test]
    fn unknown_conflict_never_stacks() {
        let p = [pr(1, "main", "2026-01-01")];
        let mut t = |_: &RestPull| None;
        let d = decide(cfg(3), "feature/issue-99", "main", &p, &mut t);
        assert_eq!(d, Decision::None(NoneReason::NoConflict));
    }

    #[test]
    fn disabled_lists_nothing() {
        // `run` short-circuits before the listing, so it never reaches `gh`.
        let dir = tempfile::tempdir().unwrap();
        let out = run(dir.path(), 1, None);
        assert_eq!(out.decision, Decision::None(NoneReason::Disabled));
    }

    fn git(dir: &Path, args: &[&str]) -> String {
        let o = std::process::Command::new("git")
            .current_dir(dir)
            .args([
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "-c",
                "commit.gpgsign=false",
            ])
            .args(args)
            .output()
            .unwrap();
        assert!(o.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&o.stderr));
        String::from_utf8_lossy(&o.stdout).trim().to_string()
    }

    fn commit_file(dir: &Path, name: &str, body: &str) {
        std::fs::write(dir.join(name), body).unwrap();
        git(dir, &["add", name]);
        git(dir, &["commit", "-qm", name]);
    }

    #[test]
    fn real_conflict_stacks_and_rebase_onto_replays_only_own_commits() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        git(d, &["init", "-q", "-b", "main"]);
        commit_file(d, "a.txt", "base\n");
        // Parent PR #1 edits a.txt; the Builder's HEAD edits it differently.
        git(d, &["checkout", "-qb", "feature/issue-1"]);
        commit_file(d, "a.txt", "parent\n");
        let parent = git(d, &["rev-parse", "HEAD"]);
        git(d, &["checkout", "-q", "main"]);
        git(d, &["checkout", "-qb", "feature/issue-99"]);
        commit_file(d, "a.txt", "child\n");
        commit_file(d, "b.txt", "other\n");
        let fork = git(d, &["merge-base", "HEAD", "main"]);
        let mut p = pr(1, "main", "2026-01-01");
        p.head_sha = Some(parent.clone());
        let mut t = |p: &RestPull| merge_tree_conflicts(d, "HEAD", p.head_sha.as_deref()?);
        let dec = decide(cfg(3), "feature/issue-99", "main", &[p], &mut t);
        assert_eq!(stacked_on(&dec), Some(1));
        // A CHANGELOG-only conflict does not stack.
        git(d, &["checkout", "-q", "main"]);
        commit_file(d, "CHANGELOG.md", "x\n");
        git(d, &["checkout", "-qb", "feature/issue-2"]);
        commit_file(d, "CHANGELOG.md", "x\ny\n");
        let cl = git(d, &["rev-parse", "HEAD"]);
        git(d, &["checkout", "-q", "feature/issue-99"]);
        assert_eq!(merge_tree_conflicts(d, "HEAD", &cl), Some(false));
        // Replay only the Builder's own commits onto the parent head. The
        // conflicting commit is resolved by taking the child side, as a
        // Builder would; the commit count must be unchanged.
        let before = git(d, &["rev-list", "--count", &format!("{fork}..HEAD")]);
        let out = std::process::Command::new("git")
            .current_dir(d)
            .args([
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "rebase",
                "-q",
                "-X",
                "theirs",
                "--onto",
                &parent,
                "--",
                &fork,
            ])
            .output()
            .unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        let after = git(d, &["rev-list", "--count", &format!("{parent}..HEAD")]);
        assert_eq!(before, after);
        git(d, &["merge-base", "--is-ancestor", &parent, "HEAD"]);
    }

    #[test]
    fn line_formats() {
        assert_eq!(
            Decision::None(NoneReason::NoConflict).line(None),
            "STACK-PARENT none reason=no-conflict"
        );
        let d = Decision::Stack {
            pr: 7,
            branch: "feature/issue-7".into(),
            head: "abc".into(),
            depth: 2,
        };
        assert_eq!(
            d.line(Some("def")),
            "STACK-PARENT pr=7 branch=feature/issue-7 head=abc fork=def depth=2"
        );
    }
}
