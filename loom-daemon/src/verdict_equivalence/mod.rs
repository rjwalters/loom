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
//! | a clean automatic merge of the base into the branch | the new head is a two-parent merge whose FIRST parent is the reviewed head, whose second parent is on the PR's base branch, and whose tree equals `git merge-tree --write-tree <reviewed> <base-parent>` | [`EquivalenceKind::CleanMerge`] |
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
//! unavailable comparison can ever cost is a redundant re-review.
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
mod tests;

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

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
                "the new head is a two-parent merge whose first parent is the reviewed head, \
                 whose second parent is a commit on this PR's base branch, and whose tree is \
                 exactly what `git merge-tree --write-tree` produces from those two — i.e. the \
                 clean automatic merge of the base, with no hand edits and no conflict resolution"
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
pub(crate) fn gh_api(gh_bin: &Path, cwd: Option<&Path>, path: &str) -> Option<Vec<u8>> {
    let mut cmd = Command::new(gh_bin);
    cmd.arg("api").arg(path);
    if let Some(dir) = cwd {
        cmd.current_dir(dir);
        // #5401: a cross-owner managed repo needs its own owner's
        // installation-token GH_CONFIG_DIR (no-op for single-owner fleets).
        crate::credential_preflight::apply_gh_config_for_root(&mut cmd, dir);
    }
    let out = cmd
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(out.stdout)
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
    let mut cmd = Command::new(gh_bin);
    cmd.arg("pr")
        .arg("view")
        .arg(pr.to_string())
        .arg("--json")
        .arg("baseRefName");
    if let Some(dir) = cwd {
        cmd.current_dir(dir);
        crate::credential_preflight::apply_gh_config_for_root(&mut cmd, dir);
    }
    if let Ok(repo) = std::env::var("LOOM_REPO") {
        cmd.arg("--repo").arg(repo);
    }
    let out = cmd
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let parsed: Pr = serde_json::from_slice(&out.stdout).ok()?;
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
/// base), then the patch-identity test (two compare calls, no local objects,
/// and the only one that can answer across a rebase whose old head the object
/// store may no longer hold).
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
    detect_with(
        verdict_tree_carveout_enabled(),
        verdict_equivalence_enabled(),
        gh_bin,
        cwd,
        pr,
        reviewed,
        head,
    )
}

/// [`detect`] with both kill switches passed in explicitly, so the
/// "switched off => nothing asked, no answer" contract is unit-testable without
/// mutating process env (the same split
/// [`crate::forge_tree_unchanged`] uses for its own switch).
#[allow(clippy::too_many_arguments)]
fn detect_with(
    carveout_enabled: bool,
    new_kinds_enabled: bool,
    gh_bin: &Path,
    cwd: Option<&Path>,
    pr: u32,
    reviewed: &str,
    head: &str,
) -> Equivalence {
    if !is_sha(reviewed) || !is_sha(head) {
        return Equivalence::Indeterminate;
    }
    // The outer kill switch covers all three kinds: with the carve-out off,
    // nothing is asked and nothing is answered.
    if !carveout_enabled {
        return Equivalence::Indeterminate;
    }

    // Kind 1 — tree-identical (#9124/#9576). Owned by forge_tree_unchanged.
    if tree_unchanged(gh_bin, cwd, reviewed, head) == Some(true) {
        return Equivalence::Equivalent(EquivalenceKind::Tree);
    }

    if !new_kinds_enabled {
        return Equivalence::Indeterminate;
    }

    // Both remaining kinds are relative to the PR's base branch: the
    // clean-merge kind must confirm the merged parent is base content, and the
    // patch-identity kind compares two merge-base-relative diffs. An
    // unresolvable base is no answer.
    let Some(base_ref) = pr_base_ref(gh_bin, cwd, pr) else {
        return Equivalence::Indeterminate;
    };

    // Kind 2 — the clean automatic merge of the base into the branch.
    let repo = repo_dir(cwd);
    if let Some(repo) = repo.as_deref() {
        if clean_merge::evidence(gh_bin, cwd, repo, reviewed, head, &base_ref) == Evidence::Proven {
            return Equivalence::Equivalent(EquivalenceKind::CleanMerge);
        }
    }

    // Kind 3 — the PR's own patch is byte-identical. Deliberately last: it is
    // the broadest test (it also catches a merge whose objects are not local),
    // and the only one that works when the reviewed head has been orphaned by a
    // force-push, because it never touches the object store.
    match patch_identity::evidence(gh_bin, cwd, reviewed, head, &base_ref) {
        Evidence::Proven => Equivalence::Equivalent(EquivalenceKind::RebasePatchIdentical),
        Evidence::Refuted => Equivalence::Changed,
        Evidence::Indeterminate => Equivalence::Indeterminate,
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
/// - exit 1 with **nothing on stdout** — no answer.
///
/// Both determinate answers exit 0 on purpose, for the same reason
/// `forge tree-unchanged` does: the *answer* is on stdout and exit 1 means only
/// "no answer", so every failure mode — an absent binary, a clap error from a
/// daemon predating this verb, a `gh` outage, a non-GitHub forge, the kill
/// switch — collapses into the identical fail-closed arm. A caller MUST key on
/// the `EQUIVALENCE_KIND=` line and treat its absence as "re-review".
pub fn handle(pr: u32, reviewed: &str, head: &str) -> anyhow::Result<()> {
    let gh = crate::forge_cmd::gh_bin();
    match detect(Path::new(&gh), None, pr, reviewed, head) {
        Equivalence::Equivalent(kind) => {
            println!("VERDICT_EQUIVALENT=1");
            println!("EQUIVALENCE_KIND={}", kind.token());
            std::process::exit(0);
        }
        Equivalence::Changed => {
            println!("VERDICT_EQUIVALENT=0");
            std::process::exit(0);
        }
        Equivalence::Indeterminate => {
            eprintln!(
                "loom-daemon forge verdict-equivalent: could not decide whether a verdict \
                 rendered against {reviewed} still describes {head} on PR #{pr} (a `gh` failure, \
                 an unparsable or truncated compare, a base ref that could not be resolved, a \
                 missing git object, a shallow clone, a git predating `merge-tree --write-tree`, \
                 a `merge-tree` conflict, an argument that is not a bare hex SHA, or a kill \
                 switch). No answer — callers must treat this as \"the change may have \
                 changed\" and re-review."
            );
            std::process::exit(1);
        }
    }
}
