# Work Plan

Prioritized roadmap of upcoming work, maintained by the Guide role.

<!-- Maintained automatically by the Guide triage agent. Manual edits are fine but may be overwritten. -->

## Urgent

Issues requiring immediate attention (`loom:urgent`).

- **#2250**: install-loom.sh: hook files deleted during reinstall — Safety hooks silently disabled after reinstall, agents lose destructive command protection
- **#2249**: Fresh install doesn't include loom CLI script or hooks directory — Fresh install produces non-functional Loom installation
- **#2247**: Reinstall deletes custom project-specific slash commands — Data loss of user's custom commands during reinstall

All 3 urgent issues are **installer bugs** discovered during reinstall testing. They collectively break the install/reinstall pathway.

## Ready

Human-approved issues ready for implementation (`loom:issue`).

- **#2248**: Fresh install gitignores .loom/config.json but previous install tracked it

## In Progress

Issues actively being worked by builders (`loom:building`).

- **#2245**: Loom reinstall leaves working tree in broken state with uncommitted deletions (PR #2252 in review)
- **#2244**: Add dual-mode GitHub API layer with REST fallback (building, no PR yet)
- **#2243**: Worktree cleanup breaks shell when CWD is inside deleted worktree (PR #2251 in review)

## Under Curation

- **#2246**: Reinstall uninstall step fails on non-empty worktree directories (`loom:curating`)

## Proposed

Issues under evaluation (`loom:architect`, `loom:hermit`, `loom:curated`).

*No proposed issues awaiting evaluation.*

## Epics

No active epics.

## Backlog Balance

| Tier | Count |
|------|-------|
| Tier 1 (goal-advancing) | 0 |
| Tier 2 (goal-supporting) | 0 |
| Tier 3 (maintenance) | 0 |

**Note:** The current backlog is entirely installer-related bugs (7 issues total: 3 urgent, 1 ready, 1 curating, 2 building with PRs in review). This is a focused cluster of related problems discovered during reinstall testing on 2026-02-14. No tier labels have been assigned yet — all are bugs rather than feature work. Once the installer issues are resolved, the pipeline will need new proposals from Architect/Hermit to generate feature work.
