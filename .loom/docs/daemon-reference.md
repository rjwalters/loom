# Loom Daemon Reference (Deprecated)

> **Status: DEPRECATED.** The Python `loom-daemon` brain and `/shepherd`
> orchestrator are being deleted in Loom **v1.0.0** as part of the
> shepherd/daemon deprecation epic (#3372). The historical contents of this
> page — `ISSUE_THRESHOLD` / `MAX_SHEPHERDS` tuning tables, the
> `daemon-state.json` schema, session-rotation procedures, shepherd
> pool sizing, etc. — described a brain that no longer exists.

If you are looking for the replacements, see:

| Old (deprecated) | New (supported) | Where |
|------------------|-----------------|-------|
| `./.loom/scripts/daemon.sh start` / Python `loom-daemon` CLI | `LOOM_USE_SPAWN_LOOP=1 ./.loom/scripts/spawn-loop.sh start` | [Spawn-loop mode](../../CLAUDE.md#3-spawn-loop-mode-phase-1-opt-in) |
| `.loom/daemon-state.json` | `.loom/spawn-loop-state.json` | Written by `spawn-loop.sh`; see [`spawn-loop.sh status`](../../.loom/scripts/spawn-loop.sh) |
| `.loom/progress/shepherd-*.json` heartbeats | `.loom/sweep-checkpoint/issue-<N>.json` (#3373) | Written by `/loom:sweep` |
| `/shepherd <issue>` slash command | `/loom:sweep <issue>` | `.claude/commands/loom/sweep.md` |
| Support-role daemons (Champion, Curator, Judge, Auditor, Guide) | GitHub Actions cron workflows | `.github/workflows/loom-*.yml` (#3375) |

For the migration narrative — including per-CLI breaking changes, how to
enable the spawn loop, and how to opt in to the GitHub Actions workflows —
see [`docs/migration/v1.0.0-shepherd-deprecation.md`](../../docs/migration/v1.0.0-shepherd-deprecation.md).

For the engineering inventory that drove the deletion decisions (which
consumers retire, which port, which are unchanged), see
[`docs/migration/daemon-state-consumers.md`](../../docs/migration/daemon-state-consumers.md).

## Why this file is intentionally minimal

The legacy schema and tuning advice on this page were a pre-Phase-3 reference
for a multi-process Python daemon that polled the forge, scaled a
`shepherd-N` pool, ran work-generation triggers (Architect/Hermit), and
maintained a JSON state file as the canonical source of truth. None of that
exists post-v1.0.0:

- The spawn loop has **no work-generation triggers** — Architect and Hermit
  cadence is out of scope for Phase 1 and tracked under follow-up #3381.
- The spawn loop **does not maintain a shepherd-N pool**. Each ready issue
  detaches its own `claude -p "/loom:sweep N"` child; concurrency is bounded
  by `MAX_PARALLEL` only.
- The spawn loop **does not track `pipeline_state`, `warnings`,
  `completed_issues`, `total_prs_merged`, or `last_*_trigger`**. The forge
  is the source of truth for pipeline state; failure counters live in
  `.loom/tokens/.failure_counts` (token-rotation only).
- Support roles are **cron-driven** under GitHub Actions, not long-running
  processes — there is no `JUDGE_INTERVAL` / `CHAMPION_INTERVAL` / etc. to
  tune from a daemon config.

Re-creating a "compatibility-shape" `daemon-state.json` from the spawn loop
was considered and rejected — see the rationale in
`docs/migration/daemon-state-consumers.md` §"Conclusion: what Phase 3 deletes
vs preserves".

## Rust `loom-daemon` (unrelated, still supported)

This page is about the deleted **Python** `loom-daemon` CLI. The **Rust**
binary at `loom-daemon/` (Tauri-side IPC daemon, tmux session manager) is a
different component and is unaffected by Phase 3. Its source lives in
`loom-daemon/src/`, its tests in `loom-daemon/tests/`, and its release
artifacts ship with the rest of the Tauri quickstart.
