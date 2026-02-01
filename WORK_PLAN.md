# Work Plan

Prioritized roadmap of upcoming work, maintained by the Guide role.

<!-- Maintained automatically by the Guide triage agent. Manual edits are fine but may be overwritten. -->

## Urgent

Issues requiring immediate attention (`loom:urgent`).

*No urgent issues.*

## Ready

Human-approved issues ready for implementation (`loom:issue`).

*No ready issues.* The backlog is empty -- all approved work is currently being built.

## In Progress

Issues actively being worked by shepherds (`loom:building`).

- **#1881**: Split activity database module (db.rs) into domain-specific submodules *(tier:maintenance)*
- **#1883**: Split commands/activity.rs (4,117 lines) into domain-specific submodules *(tier:maintenance)*
- **#1888**: Decompose loom-daemon init.rs (2,316 lines) into focused initialization submodules *(tier:maintenance)*

## Proposed

Issues under evaluation (`loom:architect`, `loom:hermit`, `loom:curated`).

- **#1888**: Decompose loom-daemon init.rs -- also has `loom:architect` *(tier:maintenance)*
- **#1899**: Add post-worktree hook to pre-build daemon binary *(tier:goal-supporting, no workflow label)*

## Epics

Active epics and their phase progress (`loom:epic`).

### #1893: Reshape Loom into Analytics-First Claude Wrapper

Progress: **1/7 phases complete (14%)**

| Phase | Issue | Status |
|-------|-------|--------|
| Phase 1 | #1894 Config v3 & State Simplification | CLOSED |
| Phase 2 | #1895 Side-by-Side Layout (Terminal + Analytics) | OPEN |
| Phase 3 | #1896 Claude Code Session Manager | OPEN |
| Phase 4 | #1897 Input Logging Layer | OPEN |
| Phase 5 | #1898 File-Based Analytics Dashboard | OPEN |
| Phase 6 | #1901 Rewrite main.ts for Single-Session + Analytics | OPEN |
| Phase 7 | #1902 Remove Dead Multi-Terminal and Prediction Code | OPEN |

*All remaining phases are `tier:goal-advancing` but not yet promoted to `loom:issue`.*

## Backlog Balance

| Tier | Count |
|------|-------|
| Tier 1 (goal-advancing) | 7 (epic + 6 phases) |
| Tier 2 (goal-supporting) | 1 (#1899) |
| Tier 3 (maintenance) | 3 (#1881, #1883, #1888) |
