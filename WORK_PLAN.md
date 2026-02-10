# Work Plan

Prioritized roadmap of upcoming work, maintained by the Guide role.

<!-- Maintained automatically by the Guide triage agent. Manual edits are fine but may be overwritten. -->

## Urgent

Issues requiring immediate attention (`loom:urgent`).

- **#2201**: Daemon needs strategy for issues that exceed single-session context budget *(tier:goal-advancing)*
- **#2205**: Daemon should report when stalled waiting on human input *(tier:goal-supporting)*
- **#2200**: Installer doesn't copy guard-destructive.sh hook to target repo *(tier:goal-supporting)*

## Ready

Human-approved issues ready for implementation (`loom:issue`).

- **#2202**: Remove orphaned ab_testing.rs backend (1,341 LOC dead code) *(tier:maintenance)*

## In Progress

Issues actively being worked by shepherds (`loom:building`).

- **#2199**: Daemon should capture shepherd output on kill for post-mortem debugging *(loom:curated)*
- **#2198**: Shepherd spawns without writing progress file -- silent failure mode *(loom:curated)*
- **#2197**: Stale heartbeat detection too slow -- 8+ minutes to reclaim stuck shepherd *(loom:curated)*

## Proposed

Issues under evaluation (`loom:architect`, `loom:hermit`, `loom:curated`).

*No proposed issues awaiting evaluation. All curated issues have been promoted to `loom:issue`.*

## Epics

No active epics. Previous epic #1893 (Reshape Loom into Analytics-First Claude Wrapper) completed all 7 phases and is now closed.

## Backlog Balance

| Tier | Count |
|------|-------|
| Tier 1 (goal-advancing) | 1 (#2201) |
| Tier 2 (goal-supporting) | 2 (#2205, #2200) |
| Tier 3 (maintenance) | 1 (#2202) |

**Note:** The backlog is lean. The 3 building issues (#2197-#2199) are all daemon reliability improvements that complement the urgent queue. Once building completes, the pipeline will need new proposals from Architect/Hermit roles.
