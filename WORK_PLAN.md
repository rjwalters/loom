# Work Plan

Prioritized roadmap of upcoming work, maintained by the Guide role.

<!-- Maintained automatically by the Guide triage agent. Manual edits are fine but may be overwritten. -->

## Urgent

No urgent issues currently open.

## Ready

Human-approved issues ready for implementation (`loom:issue`).

- **#3026**: Remove unused monkeypatch parameter from test_matching_error_conflict_not_main_workspace
- **#3025**: Move 'import re' to top-level in worktree.py _handle_feature_branch_in_main_worktree
- **#3024**: Extract shared keyword PR search into a reusable helper in builder.py

## In Progress

Issues currently being built (`loom:building`).

*(none currently)*

## PRs Awaiting Review

No PRs currently awaiting review (`loom:review-requested`).

## Proposed

Issues awaiting Champion evaluation (`loom:curated`).

- **#3026**: Remove unused monkeypatch parameter from test_matching_error_conflict_not_main_workspace *(also ready)*
- **#3025**: Move 'import re' to top-level in worktree.py _handle_feature_branch_in_main_worktree *(also ready)*
- **#3024**: Extract shared keyword PR search into a reusable helper in builder.py *(also ready)*
- **#3023**: Remove unused _make_result helper in TestGetPrForIssue

## Epics

No active epics.

## Backlog Balance

| Tier | Count |
|------|-------|
| Tier 1 (urgent/ready) | 3 |
| Tier 2 (curated, awaiting Champion) | 4 |
| Tier 3 (no labels, need curation) | 0 |

**Assessment (2026-02-23):** The pipeline has experienced a major burst of activity since 2026-02-19 — 46 additional PRs merged across 2026-02-22 and 2026-02-23 (PRs #2966–#3028). This resolves a large wave of shepherd reliability, systematic failure classification, spawn-signal handling, duplicate-PR prevention, checkpoint/stale-branch bugs, and infrastructure fixes.

The backlog is now very lean. The 20+ curated issues that were pending Champion promotion as of 2026-02-19 have been almost entirely built and merged. Only 4 curated issues remain, all small code quality / test improvements:

1. **Cleanup/refactor** (#3023, #3025): Remove unused test helpers and move imports to module scope
2. **Deduplication** (#3024): Extract shared keyword PR search into a reusable helper
3. **Test quality** (#3026): Remove unused monkeypatch parameter from a test

The three `loom:issue`-labeled items (#3024, #3025, #3026) are ready for immediate builder pickup. The pipeline should accelerate work generation (Architect/Hermit proposals) to replenish the backlog now that the immediate bug backlog is resolved.
