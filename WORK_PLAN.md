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

- **#2253**: App crashes on launch when daemon is unavailable (SIGABRT in did_finish_launching) — Tauri app crash on startup
- **#2248**: Fresh install gitignores .loom/config.json but previous install tracked it — Git state conflict on upgrade
- **#2246**: Reinstall uninstall step fails on non-empty worktree directories — Installer cleanup failure
- **#2244**: Add dual-mode GitHub API layer with REST fallback — Infrastructure reliability improvement

## In Progress

No issues currently being built.

## Proposed

No issues awaiting evaluation.

## Epics

No active epics.

## Backlog Balance

| Tier | Count |
|------|-------|
| Tier 1 (goal-advancing) | 0 |
| Tier 2 (goal-supporting) | 5 |
| Tier 3 (maintenance) | 2 |

**Note:** The backlog is 7 issues total (3 urgent + 4 ready). Five are installer-related bugs, one is a Tauri crash bug, and one is an API infrastructure feature. No issues are currently building — the pipeline is idle and ready for builders. Tier labels have been assigned: installer and stability bugs as goal-supporting, upgrade edge cases as maintenance.
