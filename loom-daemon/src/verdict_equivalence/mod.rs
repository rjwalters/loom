//! "Is the change in front of us still the change that was reviewed?" — the
//! three equivalence kinds a review verdict survives (Issue #9416).
//!
//! # The rule this implements
//!
//! A review verdict is a statement about **the change the PR makes**, not
//! about a commit id. [`crate::claim_reconciliation::decide_verdict`] stays a
//! pure function and still answers `Invalidate` for *any* head move off the
//! marker SHA (#5686 — no guessing from event shape). Strictly downstream of
//! that answer, this module re-computes **from the repository, never from a
//! comment or a marker** whether the reviewed change is still the change in
//! front of it:
//!
//! | Head moved by | Carried when | Kind |
//! |---|---|---|
//! | a tree-identical commit (a #8248/#8508 re-date push, an empty commit) | always | [`EquivalenceKind::Tree`] |
//! | a clean automatic merge of the base into the branch, possibly with tree-identical commits before or after it (#10875) | the merge is two-parent, its FIRST parent reduces to the reviewed head, its second parent is on the PR's base branch, and its tree equals `git merge-tree --write-tree <first> <base-parent>` | [`EquivalenceKind::CleanMerge`] |
//! | a rebase onto a newer base | the PR's own merge-base-relative patch is byte-identical | [`EquivalenceKind::RebasePatchIdentical`] |
//! | anything else (new commits, edited hunks, conflict resolution) | never | — |
//!
//! The first row shipped as #9124/#9576 and lives in
//! [`crate::forge_tree_unchanged`]; this module calls it rather than owning a
//! second copy. The other two rows are #9416's own, one module per kind:
//! [`clean_merge`] and [`patch_identity`].
//!
//! # What this module never does
//!
//! - **It never exempts CI.** Only the *review* (and, on Champion's side, a
//!   critical-file hold) is carried forward; every required check still re-runs
//!   against the new head, because the base genuinely moved. Nothing here
//!   touches a check, a status, or an auto-merge arm.
//! - **It never reads a marker as evidence.** A marker saying "patch
//!   unchanged" proves nothing — on a public repo an outsider, or another
//!   fleet's Loom install, can write any marker (#9548). Every answer below
//!   comes from `git` objects or from the forge's own compare endpoint.
//! - **It never infers from shape.** No commit message is parsed, no
//!   "looks like a rebase" heuristic exists, and a ref-update's shape is not
//!   evidence of anything.
//!
//! # Fail closed, everywhere
//!
//! [`Equivalence::Indeterminate`] is the answer to every question that cannot
//! be answered affirmatively: a missing git object, a shallow clone, a git
//! predating `merge-tree --write-tree`, a `merge-tree` conflict, a `gh`
//! outage, a truncated or patch-less compare response, a base ref that cannot
//! be resolved, a non-GitHub forge, a cwd that is not a git repository. Both
//! callers treat it exactly like [`Equivalence::Changed`]: invalidate the
//! verdict / re-arm the hold, the pre-#9416 behavior. The only thing an
//! unavailable comparison can ever cost is a redundant re-review — and since
//! #10134 it is never a SILENT one: [`assess`] returns why each kind could not
//! answer, and both callers surface it (the daemon pass in its log line and
//! stale-verdict comment, the shell guard in its `REASON`). The clean-merge
//! kind also fetches an absent head before giving up (see [`git_objects`]).
//!
//! # One implementation, two callers
//!
//! - in-process — `claim_reconciliation::verdict_invalidation::handle_invalidate`
//!   calls [`detect`] directly;
//! - out-of-process — `loom-daemon forge verdict-equivalent <pr> <reviewed>
//!   <head>` ([`handle`]) is what `defaults/scripts/verdict-staleness-guard.sh`
//!   shells out to.
//!
//! There is deliberately **no** second copy of any of this in shell. The
//! divergence that shape produces is exactly the #9576 incident (the guard had
//! no tree comparison while the daemon pass did, so PRs #9541/#9483 lost
//! verdicts the daemon would have kept), and
//! `.loom/docs/shell-language-policy.md` forbids it independently.

pub mod clean_merge;
pub mod git_objects;
pub mod patch_identity;

#[cfg(test)]
mod fetch_tests;
#[cfg(test)]
mod noop_chain_tests;
#[cfg(test)]
mod tests;

use std::path::{Path, PathBuf};

use crate::forge_tree_unchanged::{tree_unchanged, verdict_tree_carveout_enabled};

/// Env kill switch for the two equivalence kinds #9416 adds, nested inside
/// [`crate::forge_tree_unchanged::VERDICT_TREE_CARVEOUT_ENABLED_ENV`]: turning
/// the outer switch off disables all three kinds, turning this one off leaves
/// the already-shipped tree-identical kind (#9124/#9576) in place and disables
/// only the clean-merge and rebase-patch-identical kinds.
///
/// Defaults to ON for the same reason the outer switch does: each kind fires
/// only on a positive proof and fails closed whenever that proof is
/// unavailable, so it can only ever *reduce* exposure relative to invalidating
/// on every head move. `0`/`false`/`no`/`off` disables it on **both** paths.
pub const VERDICT_EQUIVALENCE_ENABLED_ENV: &str = "LOOM_VERDICT_EQUIVALENCE";

/// Are the two #9416 equivalence kinds enabled? See
/// [`VERDICT_EQUIVALENCE_ENABLED_ENV`].
#[must_use]
pub fn verdict_equivalence_enabled() -> bool {
    match std::env::var(VERDICT_EQUIVALENCE_ENABLED_ENV) {
        Ok(v) => !matches!(v.trim().to_ascii_lowercase().as_str(), "0" | "false" | "no" | "off"),
        Err(_) => true,
    }
}

/// Which equivalence carried the verdict. Recorded verbatim in the re-anchor
/// comment's own marker so the history stays auditable — a reader can always
/// tell *why* a verdict outlived a head move, and re-derive it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EquivalenceKind {
    /// The two heads carry byte-identical trees (#9124/#9576).
    Tree,
    /// The new head is the clean automatic merge of the base into the reviewed
    /// head ([`clean_merge`]).
    CleanMerge,
    /// The PR's own merge-base-relative patch is byte-identical across the move
    /// ([`patch_identity`]).
    RebasePatchIdentical,
}

impl EquivalenceKind {
    /// The stable token written into the `<!-- loom:verdict-equivalence -->`
    /// marker and printed by the CLI. These three strings are a contract: the
    /// operator's rule names them (`tree`, `clean-merge`,
    /// `rebase-patch-identical`) and the shell guard quotes them back into its
    /// `REASON` line.
    #[must_use]
    pub fn token(self) -> &'static str {
        match self {
            Self::Tree => "tree",
            Self::CleanMerge => "clean-merge",
            Self::RebasePatchIdentical => "rebase-patch-identical",
        }
    }

    /// One sentence naming the evidence, for the re-anchor comment and the
    /// guard's `REASON`.
    #[must_use]
    pub fn evidence(self) -> &'static str {
        match self {
            Self::Tree => {
                "the two commits' trees are byte-identical (the compare reports zero file \
                 differences), so the reviewed code IS the code at the new head"
            }
            Self::CleanMerge => {
                "the new head is the reviewed head plus only clean automatic merges of this PR's \
                 base branch and tree-identical commits: each merge's second parent is a commit on \
                 the base branch, its first parent reduces to the reviewed head, and its tree is \
                 exactly what `git merge-tree --write-tree` produces from those two — no hand \
                 edits and no conflict resolution"
            }
            Self::RebasePatchIdentical => {
                "the PR's own patch relative to its merge base is byte-identical before and \
                 after the move (same file set, same statuses, same resulting blob ids, same \
                 patch text), so the change that was reviewed is still exactly the change in \
                 front of us"
            }
        }
    }
}

/// The three-valued answer. `Changed` and `Indeterminate` are treated
/// identically by both callers (invalidate / re-arm); they are distinguished
/// only so the CLI can report "provably different" separately from "could not
/// tell", which is what makes a degraded host visible instead of silent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Equivalence {
    /// Proven equivalent by the named kind — the verdict carries forward.
    Equivalent(EquivalenceKind),
    /// Positively proven to be a different change — re-review.
    Changed,
    /// No answer. Re-review, exactly as before #9416.
    Indeterminate,
}

/// Per-kind evidence, used inside [`clean_merge`] and [`patch_identity`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Evidence {
    /// This kind's conditions are all affirmatively met.
    Proven,
    /// This kind's conditions are affirmatively NOT met (e.g. the head is not
    /// a merge commit at all, or the two patches provably differ).
    Refuted,
    /// This kind could not be evaluated. Never an assumed equivalence.
    Indeterminate,
}

/// Is `s` a plausible commit SHA — 7-40 lowercase hex digits, the same shape
/// `verdict-staleness-guard.sh`'s marker regex accepts and
/// [`crate::forge_tree_unchanged`] enforces for the same reason: these values
/// are interpolated into a `gh api` path and the marker SHA originates in a PR
/// comment, i.e. untrusted external content.
pub(crate) fn is_sha(s: &str) -> bool {
    (7..=40).contains(&s.len())
        && s.chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
}

/// Is `s` safe to interpolate into a `gh api` path as a ref name? Branch names
/// legitimately contain `/`, `.`, `-` and `_`, so a blanket alphanumeric test
/// is too strict — but `..`, a leading `-`, an empty value, or anything outside
/// that set could address a different endpoint entirely and is refused.
pub(crate) fn is_safe_ref(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 255
        && !s.starts_with('-')
        && !s.contains("..")
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '/'))
}

/// One `gh api <path>` read, returning raw stdout on exit 0 and `None` on any
/// failure. `cwd` is what resolves the `{owner}/{repo}` placeholders, so a
/// daemon managing several roots must pass the root it is asking about —
/// the same contract [`crate::forge_tree_unchanged::tree_unchanged`] has.
pub(crate) fn gh_api(
    op: &'static str,
    gh_bin: &Path,
    cwd: Option<&Path>,
    path: &str,
) -> Option<Vec<u8>> {
    // #10089: counted via the facade under `op`.
    crate::claim_reconciliation::gh_call::ok_stdout(optional_cwd(
        op,
        gh_bin,
        cwd,
        ["api", path].iter().copied(),
    ))
}

/// A facade read of `gh_bin` in `cwd` (when given), carrying `args`.
fn optional_cwd<'a>(
    op: &'static str,
    gh_bin: &Path,
    cwd: Option<&Path>,
    args: impl Iterator<Item = &'a str>,
) -> crate::gh_invocation::GhInvocation {
    use crate::gh_invocation::{AccessIntent, GhInvocation, GhTarget, Operation};
    let mut inv = GhInvocation::new(
        Operation::new(op),
        AccessIntent::Read,
        GhTarget::None,
        crate::claim_reconciliation::gh_call::GH_TIMEOUT,
    )
    .program(gh_bin)
    .args(args);
    if let Some(dir) = cwd {
        inv = inv.current_dir(dir);
    }
    inv
}

/// Does `descendant` descend from (or equal) `ancestor`, per the forge's own
/// three-dot compare status? `ahead`/`identical` prove it; `behind`/`diverged`
/// disprove it; anything else is no answer.
///
/// Used only to confirm that the second parent of a merge really is a commit on
/// the PR's base branch. Without that confirmation the clean-merge kind would
/// carry a verdict across a clean merge of an *arbitrary* branch, whose content
/// nobody reviewed — which is the whole hazard, not an edge case.
pub(crate) fn descends_from(
    gh_bin: &Path,
    cwd: Option<&Path>,
    ancestor: &str,
    descendant: &str,
) -> Option<bool> {
    #[derive(serde::Deserialize)]
    struct Status {
        status: String,
    }
    if !is_sha(ancestor) || !is_safe_ref(descendant) {
        return None;
    }
    let body = gh_api(
        "verdict.compare",
        gh_bin,
        cwd,
        &format!("repos/{{owner}}/{{repo}}/compare/{ancestor}...{descendant}"),
    )?;
    let parsed: Status = serde_json::from_slice(&body).ok()?;
    match parsed.status.as_str() {
        "ahead" | "identical" => Some(true),
        "behind" | "diverged" => Some(false),
        _ => None,
    }
}

/// The PR's base branch name, read from the forge. `None` on any failure, or
/// for a name this code will not interpolate into an API path.
pub(crate) fn pr_base_ref(gh_bin: &Path, cwd: Option<&Path>, pr: u32) -> Option<String> {
    #[derive(serde::Deserialize)]
    struct Pr {
        #[serde(rename = "baseRefName")]
        base_ref_name: String,
    }
    // #10089: counted via the facade (`verdict.pr_base_ref`).
    let repo_flag = crate::claim_reconciliation::gh_call::loom_repo_flag();
    let pr = pr.to_string();
    let args = ["pr", "view", pr.as_str(), "--json", "baseRefName"]
        .into_iter()
        .chain(repo_flag.iter().map(String::as_str));
    let out = crate::claim_reconciliation::gh_call::ok_stdout(
        optional_cwd("verdict.pr_base_ref", gh_bin, cwd, args)
            .forge_op(crate::forge_call_stats::ops::PR_VIEW_STATE),
    )?;
    let parsed: Pr = serde_json::from_slice(&out).ok()?;
    is_safe_ref(&parsed.base_ref_name).then_some(parsed.base_ref_name)
}

/// The repository the local-git kinds run in: the caller's root when it named
/// one (the daemon pass always does), else the process cwd (the CLI path).
fn repo_dir(cwd: Option<&Path>) -> Option<PathBuf> {
    match cwd {
        Some(dir) => Some(dir.to_path_buf()),
        None => std::env::current_dir().ok(),
    }
}

/// Does a verdict rendered against `reviewed` still describe `head`, and by
/// which equivalence?
///
/// Tried cheapest-and-strongest first: the already-shipped tree-identical test
/// (one compare call, no local objects), then the clean-merge test (local
/// `merge-tree`, which only applies when the head really is a merge of the
/// base, fetching an absent head first — #10134), then the patch-identity test
/// (two compare calls, no local objects, and the only one that can answer
/// across a rebase whose old head the object store may no longer hold).
///
/// Costs API calls only for a head move the caller has ALREADY decided to
/// invalidate on — never on the common `Fresh` path.
#[must_use]
pub fn detect(
    gh_bin: &Path,
    cwd: Option<&Path>,
    pr: u32,
    reviewed: &str,
    head: &str,
) -> Equivalence {
    assess(gh_bin, cwd, pr, reviewed, head).equivalence
}

/// [`detect`]'s answer plus, when it is [`Equivalence::Indeterminate`], WHY
/// each kind could not answer (Issue #10134).
///
/// Indeterminate still fails closed — both callers clear the verdict exactly as
/// before — but a clear caused by "could not compare" (an unfetchable commit, a
/// `gh` outage, a shallow clone) must be visible as such, not indistinguishable
/// from a real content change. `unavailable` is empty for every `Equivalent`
/// answer and for a `Changed` every kind could weigh in on; a `Changed` reached
/// only because a stronger kind could not run keeps its reasons (see
/// [`Assessment::fail_closed_note`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Assessment {
    pub equivalence: Equivalence,
    pub unavailable: Vec<String>,
}

impl Assessment {
    fn determinate(equivalence: Equivalence) -> Self {
        Self {
            equivalence,
            unavailable: Vec::new(),
        }
    }

    fn indeterminate(unavailable: Vec<String>) -> Self {
        Self {
            equivalence: Equivalence::Indeterminate,
            unavailable,
        }
    }

    /// The reasons as one line, for a log entry, a CLI diagnostic or a PR
    /// comment. `None` for a determinate answer.
    #[must_use]
    pub fn unavailable_note(&self) -> Option<String> {
        (self.equivalence == Equivalence::Indeterminate && !self.unavailable.is_empty())
            .then(|| self.unavailable.join("; "))
    }

    /// Why a clear is fail-closed rather than proven: the [`Self::unavailable_note`]
    /// of an `Indeterminate`, or — for a `Changed` reached only because an
    /// earlier, stronger kind could not run (e.g. the incident's unfetchable
    /// head, whose base-touches-a-shared-file shape always refutes the patch
    /// kind) — that kind's reasons, prefixed so the patch refutation is not
    /// mistaken for a proven content change. `None` for `Equivalent` and for a
    /// `Changed` every kind weighed in on.
    #[must_use]
    pub fn fail_closed_note(&self) -> Option<String> {
        match self.equivalence {
            Equivalence::Indeterminate => self.unavailable_note(),
            Equivalence::Changed if !self.unavailable.is_empty() => Some(format!(
                "the PR's diff against its base differs (rebase-patch-identical refuted — a base \
                 change touching a file this PR also changes does that too), but a stronger \
                 check could not run: {}",
                self.unavailable.join("; ")
            )),
            _ => None,
        }
    }
}

/// [`detect`], with the reasons. See [`Assessment`].
#[must_use]
pub fn assess(
    gh_bin: &Path,
    cwd: Option<&Path>,
    pr: u32,
    reviewed: &str,
    head: &str,
) -> Assessment {
    assess_with(
        verdict_tree_carveout_enabled(),
        verdict_equivalence_enabled(),
        gh_bin,
        cwd,
        pr,
        reviewed,
        head,
    )
}

#[allow(clippy::too_many_arguments)]
fn assess_with(
    carveout_enabled: bool,
    new_kinds_enabled: bool,
    gh_bin: &Path,
    cwd: Option<&Path>,
    pr: u32,
    reviewed: &str,
    head: &str,
) -> Assessment {
    if !is_sha(reviewed) || !is_sha(head) {
        return Assessment::indeterminate(vec!["an argument is not a bare hex SHA".into()]);
    }
    // The outer kill switch covers all three kinds: with the carve-out off,
    // nothing is asked and nothing is answered.
    if !carveout_enabled {
        return Assessment::indeterminate(vec![
            "verdict equivalence is switched off (LOOM_VERDICT_TREE_CARVEOUT)".into(),
        ]);
    }

    // Kind 1 — tree-identical (#9124/#9576). Owned by forge_tree_unchanged.
    let mut unavailable = Vec::new();
    match tree_unchanged(gh_bin, cwd, reviewed, head) {
        Some(true) => {
            return Assessment::determinate(Equivalence::Equivalent(EquivalenceKind::Tree))
        }
        Some(false) => {}
        None => unavailable.push(format!("tree: forge compare {reviewed}...{head} unavailable")),
    }

    if !new_kinds_enabled {
        unavailable
            .push("clean-merge / rebase kinds switched off (LOOM_VERDICT_EQUIVALENCE)".into());
        return Assessment::indeterminate(unavailable);
    }

    // Both remaining kinds are relative to the PR's base branch: the
    // clean-merge kind must confirm the merged parent is base content, and the
    // patch-identity kind compares two merge-base-relative diffs. An
    // unresolvable base is no answer.
    let Some(base_ref) = pr_base_ref(gh_bin, cwd, pr) else {
        unavailable.push(format!("could not read PR #{pr}'s base branch from the forge"));
        return Assessment::indeterminate(unavailable);
    };

    // Kind 2 — the clean automatic merge of the base into the branch. Fetches
    // an absent head first (#10134).
    match repo_dir(cwd) {
        Some(repo) => {
            match clean_merge::assess(gh_bin, cwd, &repo, Some(pr), reviewed, head, &base_ref) {
                (Evidence::Proven, _) => {
                    return Assessment::determinate(Equivalence::Equivalent(
                        EquivalenceKind::CleanMerge,
                    ))
                }
                (_, Some(why)) => unavailable.push(format!("clean-merge: {why}")),
                (_, None) => {}
            }
        }
        None => unavailable.push("clean-merge: no local repository to compare in".into()),
    }

    // Kind 3 — the PR's own patch is byte-identical. Deliberately last: it is
    // the broadest test (it also catches a merge whose objects are not local),
    // and the only one that works when the reviewed head has been orphaned by a
    // force-push, because it never touches the object store.
    match patch_identity::evidence(gh_bin, cwd, reviewed, head, &base_ref) {
        Evidence::Proven => {
            Assessment::determinate(Equivalence::Equivalent(EquivalenceKind::RebasePatchIdentical))
        }
        // #10134: still Changed (fail closed), but keep any reason an earlier
        // kind could not answer — dropping it would make a failed fetch in
        // the incident's shared-file shape clear the verdict silently.
        Evidence::Refuted => Assessment {
            equivalence: Equivalence::Changed,
            unavailable,
        },
        Evidence::Indeterminate => {
            unavailable.push(
                "rebase-patch-identical: forge compare unavailable or inconclusive (gh failure, \
                 truncated or patch-less file list, or two empty diffs)"
                    .into(),
            );
            Assessment::indeterminate(unavailable)
        }
    }
}

/// The body of the re-anchor comment the daemon pass posts when a verdict is
/// carried forward.
///
/// Two markers, both load-bearing and deliberately separate:
///
/// - `<!-- loom:verdict-sha sha=<head> verdict=<token> -->` is **byte-for-byte
///   the shape the existing scanners match** (`claim_reconciliation`'s
///   `extract_latest_verdict_sha` and `verdict-staleness-guard.sh`'s
///   `MARKER_TEST` both anchor on the trailing ` -->`). Nothing may be added
///   inside it — an extra `key=value` there would make every scanner read the
///   verdict as unanchored.
/// - `<!-- loom:verdict-equivalence kind=… from=… to=… -->` is the new,
///   additive record of WHICH equivalence applied. It is an audit trail, never
///   an input: no code path treats it as evidence, precisely because a marker
///   is prose and anyone can write one (#9548).
#[must_use]
pub fn reanchor_body(
    label: &str,
    marker_token: &str,
    kind: EquivalenceKind,
    reviewed: &str,
    head: &str,
) -> String {
    format!(
        "<!-- loom:verdict-sha sha={head} verdict={marker_token} -->\n\
         <!-- loom:verdict-equivalence kind={kind_token} from={reviewed} to={head} -->\n\
         **Verdict re-anchored — the head moved, but the reviewed change did not**\n\n\
         This PR's `{label}` verdict was recorded for `{reviewed}`. The head is now `{head}`, \
         and the fleet re-derived from the repository — never from any comment or marker — that \
         the two describe the same change. Equivalence kind: **`{kind_token}`** — {evidence}.\n\n\
         Re-anchoring instead of clearing: sending an unchanged change back through Judge buys \
         another full review of content already reviewed and nothing else. The marker is \
         updated to `{head}`, so a future GENUINE change is still caught by the ordinary \
         staleness check.\n\n\
         **CI is not exempted.** Every required check re-runs against `{head}` — the base really \
         did move, which is the whole point. Only the *review* carries over.\n\n\
         ---\n\
         *Automated by loom-daemon claim reconciliation (#9124, #9416)*",
        kind_token = kind.token(),
        evidence = kind.evidence(),
    )
}

/// Handle `loom-daemon forge verdict-equivalent <pr> <reviewed> <head>`. Never
/// returns (exits).
///
/// Prints machine-readable `KEY=VALUE` lines and exits:
///
/// - `VERDICT_EQUIVALENT=1` + `EQUIVALENCE_KIND=<token>`, exit 0 — a verdict
///   rendered against `<reviewed>` still describes `<head>`.
/// - `VERDICT_EQUIVALENT=0`, exit 0 — it provably does not.
/// - exit 1 with **nothing on stdout** — no answer; ONE stderr line names why
///   (#10134), which the shell guard carries into its `REASON`.
///
/// Both determinate answers exit 0 on purpose, for the same reason
/// `forge tree-unchanged` does: the *answer* is on stdout and exit 1 means only
/// "no answer", so every failure mode — an absent binary, a clap error from a
/// daemon predating this verb, a `gh` outage, a non-GitHub forge, the kill
/// switch — collapses into the identical fail-closed arm. A caller MUST key on
/// the `EQUIVALENCE_KIND=` line and treat its absence as "re-review".
pub fn handle(pr: u32, reviewed: &str, head: &str) -> anyhow::Result<()> {
    let gh = crate::forge_cmd::gh_bin();
    let assessment = assess(Path::new(&gh), None, pr, reviewed, head);
    match assessment.equivalence {
        Equivalence::Equivalent(kind) => {
            println!("VERDICT_EQUIVALENT=1");
            println!("EQUIVALENCE_KIND={}", kind.token());
            std::process::exit(0);
        }
        Equivalence::Changed => {
            println!("VERDICT_EQUIVALENT=0");
            if let Some(why) = assessment.fail_closed_note() {
                eprintln!(
                    "loom-daemon forge verdict-equivalent: PR #{pr} {reviewed} -> {head} not \
                     shown equivalent — re-review (fail closed). Why: {why}"
                );
            }
            std::process::exit(0);
        }
        Equivalence::Indeterminate => {
            // #10134: ONE line naming WHY, so verdict-staleness-guard.sh can
            // carry it into its STALE `REASON` instead of a silent clear.
            eprintln!(
                "loom-daemon forge verdict-equivalent: could not decide whether the verdict for \
                 {reviewed} still describes {head} on PR #{pr} — re-review (fail closed). Why: {}",
                assessment
                    .unavailable_note()
                    .unwrap_or_else(|| "no reason recorded".into())
            );
            std::process::exit(1);
        }
    }
}
