# Work Plan

Prioritized roadmap of upcoming work, maintained by the Guide role.

<!-- Maintained automatically by the Guide triage agent. Manual edits are fine but may be overwritten. -->

## Urgent

Issues requiring immediate attention (`loom:urgent`).

- **#2264**: Runtime crash on startup: daemon schema migration ordering bug — SQLite error on startup when existing `activity.db` has older schema; migration ordering creates indexes before columns exist
- **#2250**: install-loom.sh: hook files deleted during reinstall — Safety hooks silently disabled after reinstall, agents lose destructive command protection (PR #2294 in review)
- **#2247**: Reinstall deletes custom project-specific slash commands — Data loss of user's custom commands during reinstall (PR #2286 in review)

Two of the 3 urgent issues (#2250, #2247) have active PRs awaiting Judge review. #2264 is a newly identified crash bug.

## Ready

Human-approved issues ready for implementation (`loom:issue`).

- **#2290**: Replace husky/lint-staged with simple .githooks/ directory — Remove 2 devDependencies and 17 stub files; fixes worktree hook breakage (#2284)

## In Progress

Issues currently being built (`loom:building`).

- **#2291**: post-worktree hook: full cargo rebuild causes lock contention and blocks parallel worktrees

## PRs Awaiting Review

Pull requests with `loom:review-requested` label.

- **PR #2297**: fix: add TTY fallback in claude-wrapper for non-terminal contexts
- **PR #2295**: fix: copy loom-daemon binary instead of rebuilding in worktrees
- **PR #2294**: fix: install hooks via loom-daemon init to prevent loss during reinstall
- **PR #2286**: fix: preserve custom slash commands during reinstall

## Proposed

Issues awaiting Champion evaluation.

- **#2296**: Shepherd cannot detect when builder wrapper is in retry loop vs actively working *(curated)*
- **#2284**: Pre-commit hook hangs in worktrees: npx not found in node_modules/.bin *(curated)*
- **#2262**: Complete analytics pipeline UI integration (Phases 3-5) *(architect)*

## Epics

No active epics.

## Backlog Balance

| Tier | Count |
|------|-------|
| Tier 1 (goal-advancing) | 0 |
| Tier 2 (goal-supporting) | 3 |
| Tier 3 (maintenance) | 1 |

**Note:** The backlog is lean — only 4 issues with `loom:issue` (3 urgent + 1 ready) and 1 actively building. The pipeline is healthy with 4 PRs awaiting review, 2 of which address urgent issues. The focus has shifted from installer bugs (largely resolved today with 14 PRs merged) to runtime stability (#2264 crash) and build infrastructure (#2290 husky removal, #2291 cargo lock contention). No Tier 1 goal-advancing issues exist; consider promoting from proposals when current urgent work clears.
