# merge-pr.sh → Rust orchestrator: inventory and migration plan (#8191)

Operator ruling (2026-10-16, #8191): proceed with a full Rust orchestrator and a
thin shell entry point. This document is the inventory that ruling asks for
before any further slice is cut. Figures were measured on `main` @ `1dbc3360e`
(2026-10-16); re-derive them before relying on them.

## What is already in Rust

- `loom-daemon merge-pr <verb>`: 29 distinct verbs called from the script,
  80 entries under `loom-daemon/src/merge_pr/` (~10.6k lines).
- Every decision-bearing predicate has a verb. #8164 (`revalidate-head`) and
  #8112 (`verdict-contradiction`) are fixed. The file-size ratchet entry is gone.

## What remains in `defaults/scripts/merge-pr.sh`

`wc -l` reports 2,842 lines, but they are dense (many one-line functions); lines
are not a usable size measure. The remaining shell is orchestration:

| Region (approx. lines) | Responsibility | Irreversible effect |
|---|---|---|
| 281-530 | `--help`, repo-root resolution, forge/`gh` selection, arg parsing, PR fetch, `pr-state` gate, `--expected-head` | none |
| 531-1000 | pre-merge guards: stacked children, version-bump collision, workflow scope, hold-state, `loom:pr`, verdict contradiction, tree/required checks | none (may exit 1/5/7/8) |
| 996-1450 | partial-increment close-conflict + label reset; closed-issue `loom:building` strip | label/issue writes |
| 1466-1700 | notify cleared blockers; stacked-child reconcile / retarget | PR retarget, comments |
| 1698-2060 | mergeable recheck, wait-for-checks, head sync | `git push` of a sync merge |
| 2057-2260 | guard revalidation, `--auto` settle, dry-run, merge retry loop | **forge merge** |
| 2260-2340 | post-merge verify and bookkeeping | label writes |
| 2332-2977 | remote branch delete, worktree removal, local branch delete, primary-checkout cleanup | **branch/worktree deletion** |

Shell libraries it sources and must be replaced or shelled to from Rust:
`lib/forge-helpers.sh` (≈25 `forge_*`/`loom_*` entry points in use, e.g.
`forge_get_pr_nocache` ×12, `forge_pr_close_targets`, `forge_get_check_runs`,
`forge_merge_pr`), `lib/branch-landed.sh`, `lib/default-branch.sh`,
`lib/worktree-removal-log.sh`.

## Exit-code contract (must not change)

`0` merged · `1` failed/refused · `3` head moved · `4` retired, never emitted ·
`5` `--auto` settle timeout, or required context missing/pending · `7`
sequenced hold · `8` `--expected-head` mismatch. 3-7 mean "nothing merged,
nothing failed: re-queue". Table: `defaults/docs/merge-pr-exit-code-exceptions.md`.

## Retained suites (the specification)

37 `defaults/scripts/tests/test-merge-pr-*.sh` suites, 15,251 lines (the issue
body says 22; the Curator said 23; both are stale). Largest:
`partial-increment` 1,273, `unstable-fallback` 1,019, `worktree-path` 1,005,
`merge-ordering-guard` 638, `head-mismatch` 631.

**Source-text assertions (AC 4).** Roughly 27 suites assert on the script's
*text* (`grep`/`awk`/`sed` over `$MERGE_PR_SRC`, `assert_grep`/`refute_grep`),
for example by extracting `_check_required_checks()` or the `--auto` block with
`awk` and sourcing it. A thin `exec` stub makes every one of these unsatisfiable
as written. Heaviest, by matching-line count:
`stale-mergeable` 20, `worktree-path` 16, `head-mismatch` 14,
`local-branch-cleanup` 10, `dirty-worktree-guard` 8, `required-checks` 7,
`merge-method` 7, `unstable-fallback` 7, `expected-head` 7, then 18 more with
1-5. The line counts are an upper bound on assertions that need handling, not a
count of assertions. These cannot be run "unchanged" against the port. Each
must either be re-pointed at behaviour (drive the stub with the existing mock
`gh`/`git` and assert on output, exit code and the recorded calls) or retired
under the three-part test in `.loom/docs/verification-recipes.md` §6, recorded
in the suite with the `retired()` helper (already present in
`test-merge-pr-unstable-fallback.sh`), with a named successor at least as
strong. The retire-or-repoint decision is made per suite in the slice that
moves the code it pins, never in bulk at the end.

**Behavioural suites** (mock `gh`/`git` on `PATH`, run the script, assert on
output and exit code): `partial-increment`, `auto-reconcile`,
`closed-issue-cleanup`, `wait-for-checks-empty-settle`,
`notify-cleared-blockers`, `hold-state-trust`, `label-freshness`,
`workflow-scope-preflight`. These are the real regression net and should pass
unchanged. A constraint follows from this: the mocks match on `argv`, so the
Rust orchestrator must issue the same `gh`/`git` invocations (same arguments,
same order) that the shell does today, or the mock must be updated under the
same three-part test.

## `LOOM_SCRIPT_HELPER_MISSING_RC` (AC 2)

The stub runs before any daemon verb exists, so it needs its own exit code for
"the binary could not be resolved or lacks the `merge-pr run` verb". It must
not be `0` (reads as merged) and must not be `1`, `3`, `4`, `5`, `7` or `8`
(each already means something to Champion). Proposal: a dedicated code in the
`EX_CONFIG` family (78), matching the token-pool convention, plus a
remediation line that reuses the existing `_mp_daemon_roll_hint` text. The
stub, not the daemon, owns that text, because it has to work when the daemon
is the thing that is missing. To be settled in the slice that adds the stub.

## Slice plan

Work from the outside in on the *irreversible tail*, so each slice leaves a
working script and the next slice's inputs already exist as Rust. Every slice
is `Part of #8191`; only the last closes it.

1. **Post-merge cleanup** — **DONE** (this slice; see "Slice 1 status" below).
   Original scope (lines ~2332-2977, plus `_remove_loom_worktree`,
   `_maybe_delete_local_branch`, `_worktree_cleanup_decide`,
   `_find_worktree_by_branch`): `merge-pr post-merge-cleanup`. Highest
   cost-of-being-wrong (branch and worktree deletion), and every predicate it
   needs (`cleanup-paths`, `worktree-preserve`, `discovered-worktree`,
   `worktree-teardown`, `remove-gate`) already exists. Retires/re-points
   `worktree-path`, `local-branch-cleanup`, `dirty-worktree-guard`,
   `primary-worktree-guard`, `primary-checkout-advice`,
   `worktree-remove-diagnosis`, `worktree-awk`.
2. **Post-merge bookkeeping** (partial-increment reset, closed-issue building
   strip, cleared-blocker notify, stacked-child reconcile).
3. **Merge loop** (wait-for-checks, head sync + #8164 retry, merge retry,
   response classification). The one forge-merge call site.
4. **Pre-merge guard sequence** (the ordered gates, `--dry-run` rendering,
   exit 1/5/7/8 mapping).
5. **Front end** (arg parsing, forge/`gh` resolution, PR fetch,
   `--expected-head`) and the stub, with `LOOM_SCRIPT_HELPER_MISSING_RC`.
   This slice removes the last source-text dependency and closes #8191.

Per-slice acceptance: the suites that still apply pass unchanged; a differential
test (shell vs Rust on one frozen corpus, never regenerated per side) for any
rewritten predicate; no behaviour change, no new merge-flow semantics, no
deletion of unmerged branches or worktrees.


## Slice 1 status: post-merge cleanup (landed with `merge-pr post-merge-cleanup`)

What moved: the whole `CLEANUP_WORKTREE` block and the nine functions behind it
(`_remove_loom_worktree`, `_maybe_delete_local_branch`, `_worktree_cleanup_decide`,
`_issue_is_closed_for_cleanup`, `_find_worktree_by_branch`, `_worktree_branch_for`,
`_primary_worktree_path`, `_is_primary_worktree_path`, `_mp_worktree`; ~580 shell
lines) into `loom-daemon/src/merge_pr/post_merge_cleanup.rs`, with the existing
predicates (`cleanup_paths`, `worktree_preserve`, `discovered_worktree`,
`remove_gate`, `dirty_guard`, `worktree_teardown`, `issue_close_gate`,
`branch_delete`, `cargo_target`, `removal_log`) called in-process in the order the
shell called them. The shell keeps `_post_merge_cleanup`, a ~50-line wrapper.

Design decisions a later slice should know about:

- **Forge reads stay in the shell, asked for lazily.** `forge_pr_close_targets`,
  `forge_get_issue_state` and `branch_has_landed` are shared with Gitea and other
  callers, and the retired code only paid for them when it needed them. The verb
  prints `LOOM-NEED close-targets|issue-state <N>|landed` and exits 3 having done
  nothing; the wrapper fetches the fact and calls again. Every fact is demanded
  before the first mutation (`Run::mutated` makes a late demand resolve to the
  fail-safe answer instead of asking), so the retry is replay-safe. Slices 2-4
  can reuse this protocol for the facts their steps need until the forge reads
  themselves move.
- **Positive terminator.** A finished run ends with `LOOM-POST-MERGE-CLEANUP
  DONE`; exit 0 without it is treated as "did not run" (a stale or muted binary),
  as `dirty-guard`'s `CLEAN` sentinel does.
- **Fail direction is unchanged: toward removing nothing.** A verb that did not
  run removes no worktree and deletes no branch. This is slightly wider than
  before (an old daemon used to still attempt the local-branch delete), and
  deliberately so: there is no half-answered state left to fall out of.
- **Argv fidelity.** The `git` calls the shell made (`worktree list --porcelain`,
  `status --porcelain`, `worktree remove --force`, `worktree prune`) are issued
  with the same arguments in the same order; the ledger line is the Rust writer's
  (byte-identical format), with the daemon's pid rather than the shell's.

Suites: 8 retained suites that extracted the deleted functions were re-pointed
at the real wrapper + real verb against real git repos through
`defaults/scripts/tests/lib/post-merge-cleanup-harness.sh` (`worktree-path`,
`local-branch-cleanup`, `dirty-worktree-guard`, `primary-worktree-guard`,
`primary-checkout-advice`, `worktree-remove-diagnosis`, `worktree-awk`, plus the
reclaim assertions in `test-cargo-target-dir-reclaim.sh` and
`test-per-worktree-target-dir.sh`). Source-text assertions are retired in the
suites under the three-part test with named successors. `worktree-path`'s inline
bash *models* of the decision tree (Tests 4-6) now drive the real thing, which is
a strictly stronger check. Four suites (`merge-ordering-guard`,
`notify-cleared-blockers`, `verdict-label-guard`, `workflow-scope-preflight`)
fail identically on `main` in this environment and are untouched.
