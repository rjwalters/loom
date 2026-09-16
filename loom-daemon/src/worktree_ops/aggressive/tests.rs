use super::*;

fn wt() -> WorktreeInfo {
    WorktreeInfo {
        path: PathBuf::from("/repo/.loom/worktrees/issue-42"),
        head: Some("abc123".to_string()),
        branch: Some("refs/heads/feature/issue-42".to_string()),
        detached: false,
        locked: false,
        bare: false,
    }
}

/// [`evaluate_aggressive_candidate`] with the #5950 issue-open probe wired
/// to `CLOSED` — i.e. the ordinary aggressive-cleanup target, and the exact
/// pre-#5950 behavior (the gate is a no-op for a closed issue). Every case
/// that predates the gate goes through this so those expectations keep
/// asserting what they always asserted.
#[allow(clippy::too_many_arguments, clippy::fn_params_excessive_bools)]
fn eval_closed_issue(
    wt: &WorktreeInfo,
    is_bare_or_main: bool,
    pr_lookup: Option<(bool, bool)>,
    is_active_shepherd: bool,
    is_under_loom: bool,
    has_sentinel: bool,
    is_uncommitted: bool,
    landed: Landed,
    age_seconds: Option<u64>,
    min_age_seconds: u64,
    force: bool,
    safe: bool,
) -> (Decision, Reason) {
    evaluate_aggressive_candidate(
        wt,
        is_bare_or_main,
        pr_lookup,
        is_active_shepherd,
        is_under_loom,
        has_sentinel,
        is_uncommitted,
        landed,
        age_seconds,
        min_age_seconds,
        force,
        safe,
        Some(&|| "CLOSED".to_string()),
    )
}

/// #4877: an aggressive-mode failure must name the worktree, the
/// operation, and git's own message — not just bump `Errors: N`.
#[test]
fn record_error_names_worktree_operation_and_cause() {
    let mut stats = AggressiveStats::default();
    stats.record_error(
        "/repo/.loom/worktrees/issue-42 [feature/issue-42]",
        "git worktree remove --force",
        "fatal: validation failed, cannot remove working tree",
    );
    assert_eq!(stats.errors, 1);
    let detail = &stats.error_details[0];
    assert!(detail.contains("issue-42"), "must name the worktree: {detail}");
    assert!(detail.contains("git worktree remove --force"), "must name the op: {detail}");
    assert!(detail.contains("validation failed"), "must carry git's error: {detail}");
}

#[test]
fn bare_worktree_is_always_kept() {
    let mut w = wt();
    w.bare = true;
    let (d, r) = eval_closed_issue(
        &w,
        false,
        None,
        false,
        true,
        true,
        false,
        Landed::Reachable,
        None,
        86400,
        false,
        false,
    );
    assert_eq!(d, Decision::Keep);
    assert_eq!(r, Reason::BareMainWorktree);
}

#[test]
fn open_pr_beats_everything_else() {
    let w = wt();
    let (d, r) = eval_closed_issue(
        &w,
        false,
        Some((true, true)),
        false,
        true,
        true,
        false,
        Landed::Reachable,
        None,
        86400,
        true, // even with force
        false,
    );
    assert_eq!(d, Decision::Keep);
    assert_eq!(r, Reason::OpenPr);
}

#[test]
fn failed_pr_lookup_fails_closed() {
    let w = wt();
    let (d, r) = eval_closed_issue(
        &w,
        false,
        Some((false, false)),
        false,
        true,
        true,
        false,
        Landed::Reachable,
        None,
        86400,
        false,
        false,
    );
    assert_eq!(d, Decision::Keep);
    assert_eq!(r, Reason::PrLookupFailed);
}

#[test]
fn active_shepherd_is_kept() {
    let w = wt();
    let (d, r) = eval_closed_issue(
        &w,
        false,
        Some((false, true)),
        true,
        true,
        true,
        false,
        Landed::Reachable,
        None,
        86400,
        false,
        false,
    );
    assert_eq!(d, Decision::Keep);
    assert_eq!(r, Reason::ActiveShepherd);
}

#[test]
fn missing_sentinel_is_user_owned() {
    let w = wt();
    let (d, r) = eval_closed_issue(
        &w,
        false,
        Some((false, true)),
        false,
        true,
        false,
        false,
        Landed::Reachable,
        None,
        86400,
        false,
        false,
    );
    assert_eq!(d, Decision::Keep);
    assert_eq!(r, Reason::UserOwned);
}

#[test]
fn outside_loom_root_is_user_owned_even_with_sentinel() {
    let w = wt();
    let (d, r) = eval_closed_issue(
        &w,
        false,
        Some((false, true)),
        false,
        false,
        true,
        false,
        Landed::Reachable,
        None,
        86400,
        false,
        false,
    );
    assert_eq!(d, Decision::Keep);
    assert_eq!(r, Reason::UserOwned);
}

#[test]
fn uncommitted_changes_are_kept_unless_forced() {
    let w = wt();
    let (d, r) = eval_closed_issue(
        &w,
        false,
        Some((false, true)),
        false,
        true,
        true,
        true,
        Landed::Reachable,
        None,
        86400,
        false,
        false,
    );
    assert_eq!(d, Decision::Keep);
    assert_eq!(r, Reason::Uncommitted);

    let (d2, _) = eval_closed_issue(
        &w,
        false,
        Some((false, true)),
        false,
        true,
        true,
        true,
        Landed::Reachable,
        None,
        86400,
        true,
        false,
    );
    assert_eq!(d2, Decision::Remove);
}

#[test]
fn reachable_head_is_removed_regardless_of_age() {
    let w = wt();
    let (d, r) = eval_closed_issue(
        &w,
        false,
        Some((false, true)),
        false,
        true,
        true,
        false,
        Landed::Reachable,
        Some(1), // 1 second old — would fail the age gate if reached
        86400,
        false,
        false,
    );
    assert_eq!(d, Decision::Remove);
    assert_eq!(r, Reason::ReachableFromOriginMain);
}

#[test]
fn unreachable_and_too_recent_is_kept() {
    let w = wt();
    let (d, r) = eval_closed_issue(
        &w,
        false,
        Some((false, true)),
        false,
        true,
        true,
        false,
        Landed::NotLanded,
        Some(10),
        86400,
        false,
        false,
    );
    assert_eq!(d, Decision::Keep);
    assert_eq!(r, Reason::TooRecent);
}

#[test]
fn unreachable_and_old_enough_is_kept_without_force() {
    let w = wt();
    let (d, r) = eval_closed_issue(
        &w,
        false,
        Some((false, true)),
        false,
        true,
        true,
        false,
        Landed::NotLanded,
        Some(999_999),
        86400,
        false,
        false,
    );
    assert_eq!(d, Decision::Keep);
    assert_eq!(r, Reason::UnreachableHead);
}

#[test]
fn unreachable_and_old_enough_is_removed_with_force() {
    let w = wt();
    let (d, r) = eval_closed_issue(
        &w,
        false,
        Some((false, true)),
        false,
        true,
        true,
        false,
        Landed::NotLanded,
        Some(999_999),
        86400,
        true,
        false,
    );
    assert_eq!(d, Decision::Remove);
    assert_eq!(r, Reason::ForceOverrideUnreachable);
}

/// #5735: `--safe --force` must NOT lose work that has no merged PR
/// backing it. `--safe` is documented as "merged-PR-only mode" — the
/// unreachable-HEAD fallback (step 9) must stay a `Keep` under `safe`
/// regardless of `force`, even though plain `force` (no `safe`) still
/// overrides it (see `unreachable_and_old_enough_is_removed_with_force`
/// above).
#[test]
fn safe_mode_keeps_unreachable_head_even_with_force() {
    let w = wt();
    let (d, r) = eval_closed_issue(
        &w,
        false,
        Some((false, true)),
        false,
        true,
        true,
        false,
        Landed::NotLanded, // PR not merged either — nothing lands this work
        Some(999_999),
        86400,
        true, // --force
        true, // --safe: must NOT override the unreachable-HEAD skip
    );
    assert_eq!(d, Decision::Keep);
    assert_eq!(r, Reason::UnreachableHead);
}

/// #5735: `--safe` narrows step 9 (the unreachable-HEAD fallback) only —
/// it must remain purely additive everywhere else. A merged PR (step 7,
/// the actual "merged-PR-only" removal path `--safe` is meant to allow)
/// still removes the worktree even when `safe` is set.
#[test]
fn safe_mode_still_removes_when_pr_is_merged() {
    let w = wt();
    let (d, r) = eval_closed_issue(
        &w,
        false,
        Some((false, true)),
        false,
        true,
        true,
        false,
        Landed::Rewritten, // HEAD not reachable (squash-merged) ...but the PR is merged
        Some(999_999),
        86400,
        false, // --force not even needed
        true,  // --safe
    );
    assert_eq!(d, Decision::Remove);
    assert_eq!(r, Reason::PrMerged);
}

/// #5177 AC1: a squash-merged worktree has an unreachable HEAD (its commits
/// are never an ancestor of origin/main) yet its PR is merged — it must be
/// removed, not retained under `UnreachableHead`.
#[test]
fn unreachable_but_pr_merged_is_removed() {
    let w = wt();
    let (d, r) = eval_closed_issue(
        &w,
        false,
        Some((false, true)), // no OPEN pr, lookup ok
        false,
        true,
        true,
        false,             // not uncommitted
        Landed::Rewritten, // HEAD not reachable (squash-merged) ...but the PR is merged
        Some(999_999),     // old enough that the age gate would otherwise not matter
        86400,
        false, // no --force needed
        false,
    );
    assert_eq!(d, Decision::Remove);
    assert_eq!(r, Reason::PrMerged);
}

/// #5177 AC2: the merged-PR check is ADDITIVE — it must never override the
/// uncommitted-work guard. Uncommitted changes win even when the PR merged.
#[test]
fn uncommitted_is_kept_even_when_pr_merged() {
    let w = wt();
    let (d, r) = eval_closed_issue(
        &w,
        false,
        Some((false, true)),
        false,
        true,
        true,
        true,              // uncommitted work present
        Landed::Rewritten, // HEAD not reachable PR merged
        None,
        86400,
        false, // not forced
        false,
    );
    assert_eq!(d, Decision::Keep);
    assert_eq!(r, Reason::Uncommitted);
}

/// #5177 AC2: an open PR still beats the merged-PR path — a worktree whose
/// branch has an OPEN pr is kept regardless of any merged-status probe.
#[test]
fn open_pr_still_beats_pr_merged() {
    let w = wt();
    let (d, r) = eval_closed_issue(
        &w,
        false,
        Some((true, true)), // OPEN pr present
        false,
        true,
        true,
        false,
        Landed::Rewritten, // even if a merged-status probe somehow also said yes
        None,
        86400,
        false,
        false,
    );
    assert_eq!(d, Decision::Keep);
    assert_eq!(r, Reason::OpenPr);
}

// --- #5950: the issue-open gate ---------------------------------------

fn issue_state(state: &'static str) -> impl Fn() -> String {
    move || state.to_string()
}

/// #5950 AC: the incident's exact shape — an OPEN issue, no PR opened yet,
/// local commits that were never pushed (so HEAD is not reachable from
/// `origin/main`), a clean working tree, and an old-enough worktree. Before
/// the gate this was `ForceOverrideUnreachable` (a removal) under plain
/// `--force`; it must now be preserved, because nothing lands that work.
#[test]
fn open_issue_with_unpushed_commits_and_no_pr_is_kept_even_with_force() {
    let w = wt();
    let (d, r) = evaluate_aggressive_candidate(
        &w,
        false,
        Some((false, true)), // no OPEN pr — none has been created yet
        false,               // no claim-lock: a manually run Builder session has none
        true,
        true,
        false,             // working tree itself is clean — everything is committed locally
        Landed::NotLanded, // ...but those commits are unpushed ⇒ unreachable from origin/main and no merged PR lands them either
        Some(999_999),     // old enough that the age gate does not save it
        86400,
        true,  // --force
        false, // no --safe
        Some(&issue_state("OPEN")),
    );
    assert_eq!(d, Decision::Keep, "an open issue's unlanded work must survive --force");
    assert_eq!(r, Reason::IssueStillOpen);
}

/// #5950: `--force` documents itself as overriding *uncommitted changes*.
/// While the issue is open that override would destroy a live Builder's
/// in-progress edits, so the gate must beat it — even when HEAD is
/// reachable from `origin/main` (a freshly created worktree that has not
/// committed yet, which is the state a Builder spends its first minutes in
/// and which the age gate never even sees, since reachability is checked
/// first).
#[test]
fn open_issue_with_uncommitted_work_is_kept_even_with_force() {
    let w = wt();
    let (d, r) = evaluate_aggressive_candidate(
        &w,
        false,
        Some((false, true)),
        false,
        true,
        true,
        true,              // uncommitted edits in flight
        Landed::Reachable, // HEAD still == origin/main (nothing committed yet)
        Some(1),
        86400,
        true, // --force would otherwise override the uncommitted guard
        false,
        Some(&issue_state("OPEN")),
    );
    assert_eq!(d, Decision::Keep);
    assert_eq!(r, Reason::IssueStillOpen);
}

/// #5950: fail closed. An `UNKNOWN` issue state (the forge probe failed) is
/// not "CLOSED", so it must preserve — same contract as
/// `clean::classify_worktree`'s `state != "CLOSED"` and this tree's own
/// `PrLookupFailed`.
#[test]
fn unknown_issue_state_fails_closed() {
    let w = wt();
    let (d, r) = evaluate_aggressive_candidate(
        &w,
        false,
        Some((false, true)),
        false,
        true,
        true,
        false,
        Landed::NotLanded,
        Some(999_999),
        86400,
        true,
        false,
        Some(&issue_state("UNKNOWN")),
    );
    assert_eq!(d, Decision::Keep);
    assert_eq!(r, Reason::IssueStillOpen);
}

/// #5950: the deliberate carve-out. A partial-increment slice (`Part of
/// #N`) merges while the family issue #N stays open indefinitely — its
/// worktree holds nothing but landed work, so aggressive mode must still
/// reclaim it. Without this, the gate would make `--aggressive` useless for
/// the single largest class of vestigial worktrees in this repo.
#[test]
fn open_issue_with_merged_pr_and_clean_tree_is_still_removed() {
    let w = wt();
    let (d, r) = evaluate_aggressive_candidate(
        &w,
        false,
        Some((false, true)),
        false,
        true,
        true,
        false,             // clean working tree
        Landed::Rewritten, // squash-merged ⇒ unreachable ...but the PR is merged: the work IS landed
        Some(999_999),
        86400,
        false, // no --force needed
        false,
        Some(&issue_state("OPEN")),
    );
    assert_eq!(d, Decision::Remove);
    assert_eq!(r, Reason::PrMerged);
}

/// #5950: same carve-out via the other landed-work signal — HEAD already
/// reachable from `origin/main` with a clean tree loses nothing.
#[test]
fn open_issue_with_reachable_head_and_clean_tree_is_still_removed() {
    let w = wt();
    let (d, r) = evaluate_aggressive_candidate(
        &w,
        false,
        Some((false, true)),
        false,
        true,
        true,
        false,             // clean working tree
        Landed::Reachable, // HEAD is on origin/main
        Some(999_999),
        86400,
        false,
        false,
        Some(&issue_state("OPEN")),
    );
    assert_eq!(d, Decision::Remove);
    assert_eq!(r, Reason::ReachableFromOriginMain);
}

/// #5950: worktrees with no `issue-N` branch (detached, `pr-NNNN`, arbitrary
/// user paths) have no issue state to consult — `None` — and the tree must
/// behave exactly as it did before the gate existed.
#[test]
fn no_issue_number_leaves_the_decision_tree_unchanged() {
    let mut w = wt();
    w.branch = None;
    w.detached = true;
    let (d, r) = evaluate_aggressive_candidate(
        &w,
        false,
        None, // branchless ⇒ no PR lookup either
        false,
        true,
        true,
        false,
        Landed::NotLanded,
        Some(999_999),
        86400,
        true,
        false,
        None,
    );
    assert_eq!(d, Decision::Remove);
    assert_eq!(r, Reason::ForceOverrideUnreachable);
}

/// #5950: the gate is *purely subtractive on removals* — it must never turn
/// a pre-existing `Keep` into a `Remove`, nor change which guard reports a
/// keep that an earlier (cheaper, purely local) gate already made. An open
/// issue whose worktree is user-owned still reports `UserOwned`.
#[test]
fn earlier_local_gates_still_win_over_the_issue_gate() {
    let w = wt();
    let (d, r) = evaluate_aggressive_candidate(
        &w,
        false,
        Some((false, true)),
        false,
        true,
        false, // no .loom-managed sentinel
        false,
        Landed::NotLanded,
        Some(999_999),
        86400,
        true,
        false,
        Some(&issue_state("OPEN")),
    );
    assert_eq!(d, Decision::Keep);
    assert_eq!(r, Reason::UserOwned);
}

/// #5950: the probe is lazy — a worktree settled by a purely local gate
/// must cost no forge round-trip at all.
#[test]
fn issue_state_is_not_probed_when_a_local_gate_settles_it() {
    let w = wt();
    let probed = std::cell::Cell::new(0_u32);
    let probe = || {
        probed.set(probed.get() + 1);
        "OPEN".to_string()
    };
    let (d, r) = evaluate_aggressive_candidate(
        &w,
        false,
        Some((true, true)), // open PR settles it immediately
        false,
        true,
        true,
        false,
        Landed::NotLanded,
        None,
        86400,
        true,
        false,
        Some(&probe),
    );
    assert_eq!(d, Decision::Keep);
    assert_eq!(r, Reason::OpenPr);
    assert_eq!(probed.get(), 0, "the forge must not be probed for an already-settled worktree");
}

#[test]
fn enumerate_git_worktrees_returns_empty_on_non_repo() {
    let dir = tempfile::tempdir().unwrap();
    assert!(enumerate_git_worktrees(dir.path()).is_empty());
}

// --- end-to-end `clean_aggressive` regression coverage (#5735) --------

fn git(dir: &Path, args: &[&str]) {
    let ok = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap()
        .status
        .success();
    assert!(ok, "git {args:?} failed in {}", dir.display());
}

/// Build a repo with a real `origin/main` remote-tracking ref (via a bare
/// "origin" and a push) plus one `.loom-managed`, `.loom/worktrees/`-nested
/// worktree whose HEAD is a commit made *after* the push — i.e. genuinely
/// unreachable from `origin/main`, exactly the "closed-unmerged PR" /
/// "unpushed commits" shape from the issue's repro. The worktree is
/// left detached (no branch) so the decision tree never needs a `gh`
/// call (`pr_lookup` short-circuits to `None` for a branchless worktree).
fn repo_with_unreachable_worktree() -> (tempfile::TempDir, PathBuf) {
    let origin_dir = tempfile::tempdir().unwrap();
    git(origin_dir.path(), &["init", "-q", "--bare"]);

    let repo_dir = tempfile::tempdir().unwrap();
    git(repo_dir.path(), &["init", "-q", "--initial-branch=main"]);
    git(repo_dir.path(), &["config", "user.email", "loom@example.com"]);
    git(repo_dir.path(), &["config", "user.name", "Loom Test"]);
    git(repo_dir.path(), &["commit", "-q", "--allow-empty", "-m", "seed"]);
    git(
        repo_dir.path(),
        &[
            "remote",
            "add",
            "origin",
            origin_dir.path().to_str().unwrap(),
        ],
    );
    git(repo_dir.path(), &["push", "-q", "origin", "main"]);

    // A commit that lands ONLY in the worktree, never pushed — unreachable
    // from origin/main by construction.
    let wt_path = repo_dir
        .path()
        .join(".loom")
        .join("worktrees")
        .join("pr-9999");
    git(
        repo_dir.path(),
        &[
            "worktree",
            "add",
            "-q",
            "--detach",
            wt_path.to_str().unwrap(),
            "main",
        ],
    );
    // Real content, not `--allow-empty` (#7812): the point of this fixture
    // is work that would be LOST, and since the landed check compares
    // trees, a content-free commit is (correctly) landed — see
    // `landed::probe`'s tree-equality rung (`landed.rs`). That behavior
    // itself (content-free commit => landed) is exercised only by the
    // shared shell primitive's own suite, not here on the Rust side:
    // `defaults/scripts/tests/test-branch-landed.sh:138-145` ("a
    // content-free commit is landed (tree equality)") (#7872).
    std::fs::write(wt_path.join("unpushed.txt"), "work that only exists here\n").unwrap();
    git(&wt_path, &["add", "-A"]);
    git(&wt_path, &["commit", "-q", "-m", "unpushed work"]);
    std::fs::write(wt_path.join(LOOM_MANAGED_SENTINEL), "").unwrap();

    (repo_dir, wt_path)
}

/// #5735 AC: `--safe --force --dry-run` must not lose a worktree whose
/// HEAD is unreachable from `origin/main` and has no merged PR — it must
/// stay classified `Skip (HEAD not on origin/main — would lose work)`,
/// not get folded into the removal total.
#[test]
fn safe_force_dry_run_keeps_unreachable_worktree() {
    let (repo_dir, _wt_path) = repo_with_unreachable_worktree();

    let stats = clean_aggressive(
        repo_dir.path(),
        /* dry_run */ true,
        /* force */ true,
        /* safe */ true,
        0,
    );

    assert_eq!(stats.removed, 0, "safe mode must not remove the unreachable worktree");
    assert_eq!(
        stats.forced_unreachable, 0,
        "nothing was forced past the safety skip under --safe"
    );
    assert_eq!(
        stats.skipped_unreachable, 1,
        "the unreachable worktree must still be counted as skipped"
    );
}

/// Contrast case: plain `--force` (no `--safe`) still overrides the skip
/// (documented, pre-existing behavior) — but the override must be
/// reported under the distinct `forced_unreachable` counter, not folded
/// silently into `removed`.
#[test]
fn force_without_safe_removes_but_counts_it_as_forced() {
    let (repo_dir, _wt_path) = repo_with_unreachable_worktree();

    let stats = clean_aggressive(
        repo_dir.path(),
        /* dry_run */ true,
        /* force */ true,
        /* safe */ false,
        0,
    );

    assert_eq!(stats.removed, 1, "plain --force still overrides the unreachable-HEAD skip");
    assert_eq!(
        stats.forced_unreachable, 1,
        "the override must be visible via a distinct counter, not folded into `removed`"
    );
    assert_eq!(stats.skipped_unreachable, 0);
}
