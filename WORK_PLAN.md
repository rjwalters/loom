# Work Plan

Prioritized roadmap of upcoming work, maintained by the Guide role.

<!-- Maintained automatically by the Guide triage agent. Manual edits are fine but may be overwritten. -->

## Urgent

Issues flagged as highest priority (`loom:urgent`).

- **#3035**: ux: after MCP retry exhaustion in force mode, require manual re-spawn with no clear guidance
- **#3033**: bug: daemon startup does not validate global MCP binary paths
- **#3031**: bug: startup monitor misattributes failure as 'loom MCP not connected' when a different MCP fails

## Ready

Human-approved issues ready for implementation (`loom:issue`).

- **#3045**: arch: curator should run as always-on background role, decoupled from shepherd pipeline
- **#3044**: arch: daemon auto-spawning shepherds should be opt-in (--auto-build), not default behavior
- **#3042**: perf: judge and champion fixed intervals don't scale with PR queue depth — PRs wait unnecessarily *(PR #3046 approved, awaiting merge)*
- **#3041**: bug: 150 blocked issues re-attempted blindly with no backlog triage or cool-down strategy *(PR #3047 approved, awaiting merge)*
- **#3040**: perf: daemon poll interval (120s) too slow to assign ready work — shepherds sit idle with queued issues *(PR #3048 awaiting review)*
- **#3035**: ux: after MCP retry exhaustion in force mode *(also urgent)*
- **#3034**: bug: stale unprocessed signal files accumulate in .loom/signals/
- **#3033**: bug: daemon startup does not validate global MCP binary paths *(also urgent)*
- **#3031**: bug: startup monitor misattributes failure as 'loom MCP not connected' *(also urgent)*
- **#3026**: Remove unused monkeypatch parameter from test_matching_error_conflict_not_main_workspace *(PR #3036 approved, awaiting merge)*
- **#3024**: Extract shared keyword PR search into a reusable helper in builder.py

## In Progress

Issues currently being built (`loom:building`).

*(none currently)*

## PRs Awaiting Review

- **#3048**: perf: reduce daemon poll interval from 120s to 30s and add fast-path assignment (`loom:review-requested`)

## Approved (Awaiting Merge)

PRs that have passed review and are queued for Champion auto-merge (`loom:pr`).

- **#3047**: feat: add tiered retry strategy with per-error-class cooldowns and backlog prune command
- **#3046**: feat: switch judge and champion to batch processing mode
- **#3036**: refactor: remove unused monkeypatch parameter from test_matching_error_conflict_not_main_workspace

## Proposed

Issues awaiting Champion evaluation (`loom:curated`).

*(none currently)*

## Epics

No active epics.

## Backlog Balance

| Tier | Count |
|------|-------|
| Urgent | 3 |
| Ready (total loom:issue) | 11 |
| Approved PRs awaiting merge | 3 |
| PRs awaiting review | 1 |
| Curated (awaiting Champion) | 0 |

**Assessment (2026-02-23):** The pipeline is active with a strong batch of work completing. Three PRs are approved and queued for merge (#3036, #3046, #3047), and one new PR (#3048) awaits review. The urgent queue is full (3/3) — all three are MCP startup reliability issues forming a cohesive cluster: path validation (#3033), failure misattribution (#3031), and recovery UX (#3035). Architectural issues #3044 and #3045 represent the next wave of structural improvements once the bug cluster clears.
