//! "Has this branch landed on the default branch?" — `clean --aggressive`'s
//! view of the one shared #7812 ladder.
//!
//! Since #8470 this module owns **no rung of its own**. The ladder — ancestry,
//! forge (merged PR whose head still equals the tip, #7872), tree equality —
//! lives once, in [`crate::worktree_cli::branch_landed::ladder`], and is the
//! same code `worktree.sh remove`'s squash-aware branch delete runs. What stays
//! here is only what is specific to the bulk reaper:
//!
//! - **The key.** The worktree's HEAD is the tip (it is already known from
//!   `git worktree list --porcelain`), and the issue number is expressed as the
//!   forge key `feature/issue-<n>` at this call site. A worktree with no
//!   `issue-N` branch skips the forge rung, exactly as before.
//! - **The forge transport.** REST first (`gh api .../pulls`, the separate and
//!   less-contended quota — `--aggressive` is a bulk pass), falling back to the
//!   ladder's own [`branch_landed::forge_probe`] only when REST cannot answer.
//! - **The output shape.** [`Landed`] reconstructs `Reachable` vs `Rewritten`
//!   from the evidence token, so the decision tree keeps reporting its two
//!   distinct removal reasons `reachable_from_origin_main` and `pr_merged`.
//!
//! # The strictness change (#8470)
//!
//! Before the convergence, *any* merged PR for `feature/issue-<n>` read as
//! `Rewritten`. The shared ladder requires the merged PR's head SHA to still
//! equal the worktree HEAD; a branch that moved past its merged head
//! (post-merge commits — unpushed work) now falls through to the tree rung,
//! which calls it `NotLanded` if those commits carry anything `origin/main`
//! lacks. `clean --aggressive` therefore KEEPS such a worktree where it used to
//! reap it. That is deliberate, and pinned by
//! `aggressive::tests::merged_pr_with_moved_tip_is_kept_not_reaped`.
//!
//! The answer is deliberately **three-way** ([`Landed`]): `Unknown` is a real
//! state that must never be coerced into a boolean at this boundary. Coercing
//! it to "landed" reaps a worktree holding unmerged work (data loss); coercing
//! it to "not landed" resurrects the pre-#4889 "can never clean up a
//! squash-merged branch" bug. [`super::aggressive::evaluate_aggressive_candidate`]
//! therefore carries it through its decision ladder as its own `Keep` arm.

use std::path::Path;
use std::process::Command;

use super::{clean, gh, naming};
use crate::worktree_cli::branch_landed::{
    self, Answer, Caps, Evidence, ForgeProbe, ForgeStatus, Verdict,
};

/// The default branch `clean --aggressive` measures against — unchanged by
/// the convergence (the rest of the aggressive pass is `origin/main`-keyed).
const DEFAULT_REV: &str = "origin/main";

/// Three-way answer to "has this branch landed?".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Landed {
    /// Landed: HEAD is reachable from `origin/main` (merge commit, or a
    /// fast-forward). The cheapest and most local proof.
    Reachable,
    /// Landed under rewritten SHAs: a merged PR whose head is exactly HEAD, or
    /// a tree-equality match. This is the squash-merge and rebase-merge case,
    /// where the branch's own commits are never reachable from the default
    /// branch.
    Rewritten,
    /// Provably NOT landed: the branch carries content `origin/main` does not
    /// have (tree comparison says so), or the forge answered negatively (no
    /// merged PR, or a merged PR whose head is not HEAD) and no local check
    /// could prove otherwise.
    NotLanded,
    /// Could not be determined — the forge probe failed (or was skipped) AND
    /// the tree comparison was unavailable. **Never** reap on this.
    Unknown,
}

impl Landed {
    /// Whether the default branch already contains everything the branch has.
    /// `Unknown` is deliberately false here — the fail-closed direction.
    #[must_use]
    pub fn is_landed(self) -> bool {
        matches!(self, Landed::Reachable | Landed::Rewritten)
    }

    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Landed::Reachable => "landed(reachable)",
            Landed::Rewritten => "landed(rewritten)",
            Landed::NotLanded => "not-landed",
            Landed::Unknown => "unknown",
        }
    }

    /// Reconstruct the aggressive view from the shared ladder's [`Answer`].
    ///
    /// Only the `ancestor` evidence is `Reachable`; every other landed
    /// evidence (`forge-merged-pr`, `merged-head-match`, `tree-equal`) is the
    /// rewritten-SHA case, reported as `pr_merged`.
    #[must_use]
    pub fn from_answer(answer: &Answer) -> Self {
        match answer.verdict {
            Verdict::Landed if answer.evidence == Evidence::Ancestor => Landed::Reachable,
            Verdict::Landed => Landed::Rewritten,
            Verdict::NotLanded => Landed::NotLanded,
            Verdict::Unknown => Landed::Unknown,
        }
    }
}

/// Answer [`Landed`] for one worktree's HEAD / issue number, against the real
/// forge.
///
/// `issue_num` becomes the forge key `feature/issue-<n>`; `None` skips the
/// forge rung, exactly as the pre-#7812 code did for a worktree with no
/// `issue-N` branch. The ladder only makes the (rate-limited) forge
/// round-trip when ancestry already failed.
#[must_use]
pub fn probe(repo_root: &Path, head_sha: Option<&str>, issue_num: Option<u32>) -> Landed {
    probe_with(
        repo_root,
        head_sha,
        issue_num,
        &|branch| forge_probe_rest_first(repo_root, branch),
        Caps::detect(),
    )
}

/// [`probe`] with the forge round-trip and git capabilities injected — the
/// seam the aggressive suite uses to pin the #8470 strictness change against a
/// real throwaway repo, offline.
#[must_use]
pub fn probe_with(
    repo_root: &Path,
    head_sha: Option<&str>,
    issue_num: Option<u32>,
    forge: &dyn Fn(&str) -> ForgeProbe,
    caps: Caps,
) -> Landed {
    let forge_key = issue_num.map(naming::branch_name);
    let default_sha = branch_landed::resolve_commit(repo_root, DEFAULT_REV);
    let answer = branch_landed::ladder(
        repo_root,
        head_sha,
        default_sha.as_deref(),
        forge_key.as_deref(),
        "",
        forge,
        caps,
    );
    Landed::from_answer(&answer)
}

/// The forge rung's transport for the bulk reaper: REST first, then the
/// ladder's own probe.
///
/// `gh pr list` goes through the routinely-exhausted GraphQL quota, while
/// `gh api .../pulls` uses the separate, less-contended REST pool. A REST
/// failure is `Unknown` for the fallback, never "not merged" (#7812).
fn forge_probe_rest_first(repo_root: &Path, branch: &str) -> ForgeProbe {
    clean::repo_owner_rest(repo_root)
        .and_then(|owner| merged_head_rest(repo_root, &owner, branch))
        .unwrap_or_else(|| branch_landed::forge_probe(repo_root, branch))
}

#[derive(serde::Deserialize)]
struct RestPr {
    state: String,
    #[serde(default)]
    merged_at: Option<String>,
    #[serde(default)]
    closed_at: Option<String>,
    #[serde(default)]
    head: Option<RestHead>,
}

#[derive(serde::Deserialize)]
struct RestHead {
    #[serde(default)]
    sha: Option<String>,
}

/// `repos/{owner}/{repo}/pulls?state=all&head=<owner>:<branch>` — same query
/// as [`clean::check_pr_status_for_branch_rest`], but carrying the merged
/// PR's head SHA the tip-match rung needs. `None` = REST could not answer.
fn merged_head_rest(repo_root: &Path, owner: &str, branch: &str) -> Option<ForgeProbe> {
    let path =
        format!("repos/{{owner}}/{{repo}}/pulls?state=all&head={owner}:{branch}&per_page=30");
    let mut cmd = Command::new("gh");
    cmd.args(["api", &path]).current_dir(repo_root);
    crate::credential_preflight::apply_gh_config_for_root(&mut cmd, repo_root);
    let out = gh::bounded_output(cmd, gh::GH_PROBE_TIMEOUT)?;
    if !out.status.success() {
        return None;
    }
    let rows = serde_json::from_slice::<Vec<RestPr>>(&out.stdout).ok()?;
    rest_rows_to_probe(rows)
}

/// Pure classification of the REST rows (issue #6746: a merged row anywhere
/// in the list wins, not just the newest). `None` when no row could be
/// classified at all — the fallback's cue.
fn rest_rows_to_probe(rows: Vec<RestPr>) -> Option<ForgeProbe> {
    let mut any_definitive = rows.is_empty();
    for row in rows {
        match clean::classify_pr_row(&row.state, row.merged_at.as_deref(), row.closed_at.as_deref())
        {
            clean::PrStatus::Merged { .. } => {
                let head = row
                    .head
                    .and_then(|h| h.sha)
                    .filter(|s| !s.trim().is_empty());
                return Some(match head {
                    Some(sha) => ForgeProbe {
                        status: ForgeStatus::Found,
                        head_sha: Some(sha),
                        number: None,
                    },
                    // A merged PR whose head cannot be read cannot satisfy the
                    // tip-match rule, and must not read as a negative either.
                    None => ForgeProbe::unavailable(),
                });
            }
            clean::PrStatus::Unknown => {}
            _ => any_definitive = true,
        }
    }
    any_definitive.then_some(ForgeProbe {
        status: ForgeStatus::NotFound,
        head_sha: None,
        number: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_landed_is_true_for_both_landed_variants() {
        assert!(Landed::Reachable.is_landed());
        assert!(Landed::Rewritten.is_landed());
    }

    /// The whole point of the three-way answer: `Unknown` must never read as
    /// landed, and must stay distinguishable from `NotLanded` (which `--force`
    /// may legitimately override, while `Unknown` may not).
    #[test]
    fn unknown_is_not_landed_and_is_distinct_from_not_landed() {
        assert!(!Landed::Unknown.is_landed());
        assert!(!Landed::NotLanded.is_landed());
        assert_ne!(Landed::Unknown, Landed::NotLanded);
    }

    #[test]
    fn as_str_labels_every_variant_distinctly() {
        let labels = [
            Landed::Reachable.as_str(),
            Landed::Rewritten.as_str(),
            Landed::NotLanded.as_str(),
            Landed::Unknown.as_str(),
        ];
        for (i, a) in labels.iter().enumerate() {
            for b in &labels[i + 1..] {
                assert_ne!(a, b, "labels must be distinguishable in logs");
            }
        }
    }

    fn answer(verdict: Verdict, evidence: Evidence) -> Answer {
        Answer {
            verdict,
            evidence,
            pr_number: None,
            pr_head_sha: None,
            forge_status: ForgeStatus::Skipped,
        }
    }

    /// The two removal reasons survive the convergence: only `ancestor` is
    /// `Reachable` (`reachable_from_origin_main`); every other landed
    /// evidence is `Rewritten` (`pr_merged`).
    #[test]
    fn from_answer_reconstructs_reachable_versus_rewritten_from_evidence() {
        assert_eq!(
            Landed::from_answer(&answer(Verdict::Landed, Evidence::Ancestor)),
            Landed::Reachable
        );
        for ev in [
            Evidence::ForgeMergedPr,
            Evidence::MergedHeadMatch,
            Evidence::TreeEqual,
        ] {
            assert_eq!(Landed::from_answer(&answer(Verdict::Landed, ev)), Landed::Rewritten);
        }
        assert_eq!(
            Landed::from_answer(&answer(Verdict::NotLanded, Evidence::MergedHeadMismatch)),
            Landed::NotLanded
        );
        assert_eq!(
            Landed::from_answer(&answer(Verdict::Unknown, Evidence::Inconclusive)),
            Landed::Unknown
        );
    }

    /// An empty HEAD can prove nothing locally, and with no issue number the
    /// forge is not asked — `Unknown`, never `NotLanded`.
    #[test]
    fn empty_head_sha_with_no_issue_number_is_unknown() {
        let repo = std::path::Path::new("/nonexistent-repo-for-unit-test");
        let forge = |_: &str| -> ForgeProbe { panic!("forge must not be asked without a key") };
        assert_eq!(
            probe_with(repo, Some(""), None, &forge, Caps { merge_tree: true }),
            Landed::Unknown
        );
    }

    /// A repo that does not exist cannot answer either local check, and the
    /// forge is not consulted without an issue number — so the verdict is
    /// `Unknown`, not a silent "not landed" that `--force` could then reap.
    #[test]
    fn unresolvable_repo_with_no_issue_number_is_unknown() {
        let repo = std::path::Path::new("/nonexistent-repo-for-unit-test");
        assert_eq!(probe(repo, Some("deadbeef"), None), Landed::Unknown);
    }

    fn row(state: &str, merged_at: Option<&str>, sha: Option<&str>) -> RestPr {
        RestPr {
            state: state.to_string(),
            merged_at: merged_at.map(str::to_string),
            closed_at: None,
            head: Some(RestHead {
                sha: sha.map(str::to_string),
            }),
        }
    }

    #[test]
    fn rest_rows_prefer_a_merged_row_and_carry_its_head_sha() {
        let p = rest_rows_to_probe(vec![
            row("closed", None, Some("newer")),
            row("closed", Some("2026-01-01T00:00:00Z"), Some("merged-head")),
        ])
        .expect("definitive");
        assert_eq!(p.status, ForgeStatus::Found);
        assert_eq!(p.head_sha.as_deref(), Some("merged-head"));
    }

    #[test]
    fn rest_rows_without_a_merge_are_a_definitive_negative() {
        assert_eq!(
            rest_rows_to_probe(vec![row("open", None, Some("x"))]).map(|p| p.status),
            Some(ForgeStatus::NotFound)
        );
        assert_eq!(rest_rows_to_probe(Vec::new()).map(|p| p.status), Some(ForgeStatus::NotFound));
    }

    /// A merged PR whose head SHA is unreadable can neither satisfy the
    /// tip-match rule nor count as a negative.
    #[test]
    fn rest_merged_row_without_head_sha_is_unavailable() {
        assert_eq!(
            rest_rows_to_probe(vec![row("closed", Some("2026-01-01T00:00:00Z"), None)])
                .map(|p| p.status),
            Some(ForgeStatus::Unavailable)
        );
    }

    #[test]
    fn rest_rows_that_classify_nothing_defer_to_the_fallback() {
        assert!(rest_rows_to_probe(vec![row("weird", None, None)]).is_none());
    }
}
