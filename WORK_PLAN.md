# Work Plan

Prioritized roadmap of upcoming work, maintained by the Guide role.

<!-- Maintained automatically by the Guide triage agent. Manual edits are fine but may be overwritten. -->

## Urgent

Issues requiring immediate attention (`loom:urgent`).

- **#2917**: bug: systematic failure escalation leaves loom:issue + loom:blocked on issue simultaneously

Only 1 urgent issue. Pipeline is lean — most recent work focused on critical bug fixes over the past 48 hours (60+ PRs merged 2026-02-18/19), dramatically improving shepherd reliability, thinking stall handling, and output visibility.

## Ready

Human-approved issues ready for implementation (`loom:issue`).

- **#2917**: bug: systematic failure escalation leaves loom:issue + loom:blocked on issue simultaneously *(also urgent)*

## In Progress

Issues currently being built (`loom:building`).

*(none — shepherd-1 is working on issue #2893 but hasn't claimed the label yet)*

## PRs Awaiting Review

No PRs currently awaiting review.

## Proposed

Issues awaiting Champion evaluation (`loom:curated`).

- **#2926**: Enhancement: recover uncommitted builder changes from worktree after thinking stall kill
- **#2924**: Enhancement: detect and recover when feature branch is checked out in main worktree
- **#2921**: Bug: thinking stall retry exhaustion classified as builder_unknown_failure
- **#2920**: bug: builder thinking stall retry budget of 1 is insufficient when model is under load
- **#2919**: bug: systematic failure counter is cross-issue, causing false positive blocking
- **#2918**: Bug: worktree creation failure incorrectly increments builder systematic failure counter
- **#2916**: Bug: builder checkpoint commits land on local main branch
- **#2915**: bug: get_pr_for_issue body search returns false positives from cross-repo references
- **#2914**: thinking stall retry inherits stale planning checkpoint, causing immediate exit-0 failure
- **#2912**: bug: low-output builder failures exit with code 1 (unknown) instead of structured error class
- **#2911**: bug: startup monitor incorrectly dismisses project MCP failures as global-plugin-only
- **#2910**: bug: rebase phase fails hard with no worktree even when PR is CLEAN on GitHub
- **#2909**: bug: agent config dir is not re-initialized between builder retry attempts
- **#2907**: bug: force-mode stale branch cleanup closes builder-created PRs waiting for review
- **#2906**: bug: shepherd does not comment on GitHub issue when rate-limit abort prevents builder startup
- **#2905**: bug: get_pr_for_issue body search produces false positives *(possible dup of #2915)*
- **#2897**: force-mode failure budget exhausted too quickly when shepherd is retried multiple times
- **#2896**: prior-run checkpoint commits land on local main with wrong issue number
- **#2895**: dirty-main recovery: _find_source_issues_for_dirty_files uses file existence not git modification
- **#2836**: bug: judge test runs in worktree import main-branch editable install, causing false failures

Issues being curated (`loom:curating`):

- **#2961**: bug: builder phase validation fails with FAILED when PR exists due to API propagation race in retry loop

Issues without labels (need curation):

- **#2930**: Add tests for pr-body.md happy path in builder and validate_phase
- **#2928**: Add test coverage for pre-written PR body in _build_direct_completion_pr_body and _build_recovery_pr_body
- **#2894**: bug: thinking-stall retry budget (1 retry) too small for systematic stalls
- **#2893**: bug: shepherd subprocess skips auth pre-flight, masking expired-auth as thinking stall *(being built by shepherd-1)*

## Epics

No active epics.

## Backlog Balance

| Tier | Count |
|------|-------|
| Tier 1 (goal-advancing) | 0 |
| Tier 2 (goal-supporting) | ~20 (curated, awaiting Champion) |
| Tier 3 (maintenance) | 4 (no labels, need curation) |

**Assessment (2026-02-19):** The pipeline experienced a massive 60+ PR burst over 48 hours, resolving critical shepherd reliability, thinking stall, and output visibility bugs. The backlog has grown with 20+ curated issues awaiting Champion promotion. The only ready issue (#2917) is urgent and should be claimed immediately.

Champion needs to evaluate the curated backlog and promote the highest-value issues. Curated issues cluster around:
1. **Systematic failure counter bugs** (#2918, #2919) — cross-issue contamination and incorrect increments
2. **Checkpoint/recovery bugs** (#2914, #2916, #2895, #2896) — stale checkpoints and wrong-branch commits
3. **Builder/shepherd edge cases** (#2907, #2909, #2910, #2911, #2912) — misclassification and config issues
4. **False positives** (#2905, #2915) — get_pr_for_issue cross-repo contamination (potential duplicates)
