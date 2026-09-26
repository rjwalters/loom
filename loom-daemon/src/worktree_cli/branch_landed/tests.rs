//! Unit tests for the `branch_landed` ladder.
//!
//! The forge rung is injected ([`probe_with`]), so every case here runs
//! offline against a real throwaway git repo — no `gh`, no `loom-daemon`, no
//! network. The end-to-end evidence (the retained
//! `test-worktree-remove-squash-merge.sh`, driven through the real binary)
//! lives in `loom-daemon/tests/worktree_remove_verb.rs`.

use super::*;
use std::process::Command as Cmd;

/// A host whose `git` predates 2.38, i.e. has no `merge-tree --write-tree`.
///
/// Supplied as a VALUE rather than by setting `LOOM_BRANCH_LANDED_GIT_VERSION`.
/// That variable is process-global: while a test held it at `2.30.0`, every
/// other test in this binary running concurrently also lost rung 4, so
/// `a_forge_head_that_does_not_match_the_tip_is_not_landed` and
/// `an_unreachable_forge_stays_distinguishable_from_a_forge_negative` passed
/// in isolation and failed in a full run. `#[serial]` would not have fixed it
/// — it only orders the serial tests against each other, not against the
/// parallel ones. No test in this module touches the environment now.
const ANCIENT_GIT: Caps = Caps { merge_tree: false };

struct Repo {
    dir: tempfile::TempDir,
}

impl Repo {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let r = Self { dir };
        r.git(&["init", "-q", "-b", "main"]);
        r.git(&["config", "user.email", "t@t"]);
        r.git(&["config", "user.name", "t"]);
        r.git(&["commit", "--allow-empty", "-q", "-m", "init"]);
        r
    }

    fn path(&self) -> &Path {
        self.dir.path()
    }

    fn git(&self, args: &[&str]) -> String {
        let out = Cmd::new("git")
            .arg("-C")
            .arg(self.dir.path())
            .args(args)
            .output()
            .expect("git runs");
        assert!(out.status.success(), "git {args:?} failed: {out:?}");
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// A branch off `main` carrying one new file. Returns its tip.
    fn feature(&self, name: &str, file: &str) -> String {
        self.git(&["checkout", "-q", "-b", name]);
        std::fs::write(self.dir.path().join(file), "content\n").expect("write");
        self.git(&["add", "-A"]);
        self.git(&["commit", "-q", "-m", file]);
        let tip = self.git(&["rev-parse", "HEAD"]);
        self.git(&["checkout", "-q", "main"]);
        tip
    }
}

fn never_asked() -> Box<dyn Fn(&str) -> ForgeProbe> {
    Box::new(|_| panic!("the forge must not be consulted on this path"))
}

fn forge_found(sha: &str) -> Box<dyn Fn(&str) -> ForgeProbe> {
    let sha = sha.to_string();
    Box::new(move |_| ForgeProbe {
        status: ForgeStatus::Found,
        head_sha: Some(sha.clone()),
        number: Some("77".to_string()),
    })
}

fn forge_unavailable() -> Box<dyn Fn(&str) -> ForgeProbe> {
    Box::new(|_| ForgeProbe::unavailable())
}

fn forge_not_found() -> Box<dyn Fn(&str) -> ForgeProbe> {
    Box::new(|_| ForgeProbe {
        status: ForgeStatus::NotFound,
        head_sha: None,
        number: None,
    })
}

// ---------------------------------------------------------------------------
// Rung 1 — ancestry
// ---------------------------------------------------------------------------

/// Reachability proves `landed` and short-circuits: the forge must not be
/// consulted at all, which is what keeps the cheap local rung cheap.
#[test]
fn ancestry_proves_landed_without_a_forge_round_trip() {
    let r = Repo::new();
    r.feature("feature/ff", "ff.txt");
    r.git(&["merge", "-q", "--ff-only", "feature/ff"]);

    let a = probe_with(r.path(), "feature/ff", Some("main"), "", &never_asked());
    assert_eq!(a.verdict, Verdict::Landed);
    assert_eq!(a.evidence, Evidence::Ancestor);
    assert_eq!(a.forge_status, ForgeStatus::Skipped);
}

// ---------------------------------------------------------------------------
// Rung 3 — the forge
// ---------------------------------------------------------------------------

/// The squash case the whole primitive exists for: after a squash merge the
/// branch is provably NOT an ancestor of main, and the forge's head-SHA match
/// is what authorises a force-delete.
#[test]
fn a_squash_merged_branch_is_landed_when_the_forge_head_matches_the_tip() {
    let r = Repo::new();
    let tip = r.feature("feature/sq", "sq.txt");
    r.git(&["merge", "-q", "--squash", "feature/sq"]);
    r.git(&["commit", "-q", "-m", "squash"]);
    assert!(
        !Cmd::new("git")
            .arg("-C")
            .arg(r.path())
            .args(["merge-base", "--is-ancestor", "feature/sq", "main"])
            .status()
            .expect("git")
            .success(),
        "precondition: a squashed branch is not an ancestor of main"
    );

    let a = probe_with(r.path(), "feature/sq", Some("main"), "", &forge_found(&tip));
    assert_eq!(a.verdict, Verdict::Landed);
    assert_eq!(a.evidence, Evidence::ForgeMergedPr);
    assert_eq!(a.forge_status, ForgeStatus::Found);
    assert_eq!(a.pr_number.as_deref(), Some("77"));
}

/// #7872: a merged PR for the branch NAME is not enough. A tip that moved past
/// it (unpushed work, or the next partial-increment slice reusing the name) is
/// a mismatch and must fall through to the tree check.
#[test]
fn a_forge_head_that_does_not_match_the_tip_is_not_landed() {
    let r = Repo::new();
    r.feature("feature/extra", "extra.txt");

    let a = probe_with(
        r.path(),
        "feature/extra",
        Some("main"),
        "",
        &forge_found("0000000000000000000000000000000000dead"),
    );
    assert_eq!(a.verdict, Verdict::NotLanded);
    assert_eq!(a.evidence, Evidence::TreeDiffers);
}

/// A branch that resolves to no local ref at all must not be declared landed
/// purely because the forge has a same-named merged PR (#7872) — the case with
/// the LEAST evidence is not a free pass.
#[test]
fn an_unresolvable_branch_is_never_landed_on_a_forge_answer_alone() {
    let r = Repo::new();
    let a = probe_with(
        r.path(),
        "feature/never-existed",
        Some("main"),
        "",
        &forge_found("0000000000000000000000000000000000dead"),
    );
    assert_ne!(a.verdict, Verdict::Landed);
}

// ---------------------------------------------------------------------------
// Rung 4 — tree equality, and the fail-closed floor
// ---------------------------------------------------------------------------

/// Tree equality is the SHA-independent proof: a squash merge whose forge
/// probe is unavailable is still `landed`, entirely offline.
#[test]
fn tree_equality_answers_landed_when_the_forge_cannot() {
    let r = Repo::new();
    r.feature("feature/tree", "tree.txt");
    r.git(&["merge", "-q", "--squash", "feature/tree"]);
    r.git(&["commit", "-q", "-m", "squash"]);

    let a = probe_with(r.path(), "feature/tree", Some("main"), "", &forge_unavailable());
    assert_eq!(a.verdict, Verdict::Landed);
    assert_eq!(a.evidence, Evidence::TreeEqual);
    // The caller must still be able to tell the forge was never reached.
    assert_eq!(a.forge_status, ForgeStatus::Unavailable);
}

/// An unmerged branch with an unreachable forge is `not-landed` via the tree
/// check, and `forge_status` still says the safety check could not be
/// attempted — the distinction `_maybe_delete_local_branch` turns into its
/// "Could not query the forge" note.
#[test]
fn an_unreachable_forge_stays_distinguishable_from_a_forge_negative() {
    let r = Repo::new();
    r.feature("feature/unmerged", "unmerged.txt");

    let a = probe_with(r.path(), "feature/unmerged", Some("main"), "", &forge_unavailable());
    assert_eq!(a.verdict, Verdict::NotLanded);
    assert_eq!(a.forge_status, ForgeStatus::Unavailable);

    let b = probe_with(r.path(), "feature/unmerged", Some("main"), "", &forge_not_found());
    assert_eq!(b.verdict, Verdict::NotLanded);
    assert_eq!(b.forge_status, ForgeStatus::NotFound);
}

/// The fail-closed floor. With no tree check available (simulated pre-2.38
/// git) and an unavailable forge, nothing answered — and `unknown` must NOT
/// collapse into either boolean.
#[test]
fn nothing_answering_is_unknown_not_a_boolean() {
    let r = Repo::new();
    r.feature("feature/dark", "dark.txt");

    let a = probe_with_caps(
        r.path(),
        "feature/dark",
        Some("main"),
        "",
        &forge_unavailable(),
        ANCIENT_GIT,
    );
    assert_eq!(a.verdict, Verdict::Unknown);
    assert_eq!(a.evidence, Evidence::Inconclusive);
}

/// A definitive forge negative still stands when the tree check cannot run.
#[test]
fn a_forge_negative_survives_an_unavailable_tree_check() {
    let r = Repo::new();
    r.feature("feature/nope", "nope.txt");

    let a = probe_with_caps(
        r.path(),
        "feature/nope",
        Some("main"),
        "",
        &forge_not_found(),
        ANCIENT_GIT,
    );
    assert_eq!(a.verdict, Verdict::NotLanded);
    assert_eq!(a.evidence, Evidence::ForgeNoMergedPr);
}

/// The `LOOM_BRANCH_LANDED_GIT_VERSION` seam still exists and still means what
/// the shell means by it — pinned on the pure predicate, so proving it costs
/// no process-global mutation.
#[test]
fn the_merge_tree_version_gate_matches_the_shell() {
    assert!(!merge_tree_in_version("2.30.0"));
    assert!(!merge_tree_in_version("2.37.9"));
    assert!(merge_tree_in_version("2.38.0"));
    assert!(merge_tree_in_version("2.43.0"));
    assert!(merge_tree_in_version("3.0.0"));
    // Unparseable is NOT a free pass: no version, no rung 4.
    assert!(!merge_tree_in_version("not-a-version"));
    assert!(!merge_tree_in_version(""));
}

// ---------------------------------------------------------------------------
// Rung 2 — the caller's hint
// ---------------------------------------------------------------------------

/// A matching hint is `landed` with zero forge calls; a mismatching one skips
/// the forge entirely (we already know what it would say).
#[test]
fn a_caller_hint_short_circuits_the_forge_in_both_directions() {
    let r = Repo::new();
    let tip = r.feature("feature/hint", "hint.txt");
    r.git(&["merge", "-q", "--squash", "feature/hint"]);
    r.git(&["commit", "-q", "-m", "squash"]);

    let a = probe_with(r.path(), "feature/hint", Some("main"), &tip, &never_asked());
    assert_eq!(a.verdict, Verdict::Landed);
    assert_eq!(a.evidence, Evidence::MergedHeadMatch);
    assert_eq!(a.forge_status, ForgeStatus::Hinted);

    let b = probe_with(
        r.path(),
        "feature/hint",
        Some("main"),
        "0000000000000000000000000000000000dead",
        &never_asked(),
    );
    assert_eq!(b.forge_status, ForgeStatus::Hinted);
    // Tree-equal (it WAS squash-merged), so the verdict is still landed — but
    // via the tree rung, proving the hint mismatch never consulted the forge.
    assert_eq!(b.evidence, Evidence::TreeEqual);
}

// ---------------------------------------------------------------------------
// Token parity with the shell
// ---------------------------------------------------------------------------

/// Every verdict / evidence / forge-status token this module can emit must
/// appear verbatim in `lib/branch-landed.sh`, which is still the definition
/// `merge-pr.sh` and `cleanup-branches.sh` consume. A rename on either side
/// fails here rather than silently splitting the vocabulary in two.
#[test]
fn tokens_match_the_shell_twin() {
    let lib =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../defaults/scripts/lib/branch-landed.sh");
    let Ok(sh) = std::fs::read_to_string(&lib) else {
        return; // not a full checkout
    };
    for token in [
        Verdict::Landed.as_str(),
        Verdict::NotLanded.as_str(),
        Verdict::Unknown.as_str(),
        Evidence::Ancestor.as_str(),
        Evidence::MergedHeadMatch.as_str(),
        Evidence::ForgeMergedPr.as_str(),
        Evidence::TreeEqual.as_str(),
        Evidence::TreeDiffers.as_str(),
        Evidence::TreeConflict.as_str(),
        Evidence::ForgeNoMergedPr.as_str(),
        Evidence::MergedHeadMismatch.as_str(),
        Evidence::Inconclusive.as_str(),
        ForgeStatus::Found.as_str(),
        ForgeStatus::NotFound.as_str(),
        ForgeStatus::Unavailable.as_str(),
        ForgeStatus::Hinted.as_str(),
        ForgeStatus::Skipped.as_str(),
    ] {
        assert!(sh.contains(token), "token {token:?} missing from {lib:?}");
    }
    // The forge query itself is observable behaviour: a Gitea host has no
    // `gh`, so the `loom-daemon forge` preference is part of the contract.
    assert!(sh.contains("loom-daemon forge"));
    assert!(sh.contains("--state merged"));
    assert!(sh.contains("headRefOid,number"));
}
