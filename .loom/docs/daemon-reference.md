# Loom Daemon Reference

> **⚠️ Stop-gap — daemon backend in flight (epic #3449, stop-gap #3451)**
>
> The "preserved, re-implemented" claim below describes the v0.10.0 target state. As of v0.9.1, `./.loom/scripts/daemon.sh` does **not** exist on `origin/main` — it was deleted in #3432 and is being rebuilt in epic #3449 (~4-6 weeks). Until that rebuild lands, all `./.loom/scripts/daemon.sh start|stop|status` commands in this page will fail with "no such file or directory". Use `./.loom/scripts/spawn-loop.sh` (headless) or `/loom:sweep <issue>` instead.

> **Status: PRESERVED, RE-IMPLEMENTED in v0.10.0.** The Python `loom-daemon`
> brain (`loom_tools/daemon_v2/`) and the `/shepherd` orchestrator are
> deleted in v0.10.0 as part of the shepherd/daemon-brain deprecation epic
> (#3372). **The shell-level daemon surface — `./.loom/scripts/daemon.sh` +
> tmux session runner + per-pane OAuth token rotation — is preserved.**
> The user-facing API is unchanged; only the internals are different.
>
> The historical contents of this page — `ISSUE_THRESHOLD` /
> `MAX_SHEPHERDS` tuning tables, the `daemon-state.json` schema,
> session-rotation procedures, shepherd pool sizing, etc. — described a
> Python brain that no longer exists. Those tunables are gone.

## What daemon mode is in v0.10.0

`./.loom/scripts/daemon.sh start` opens a tmux session whose panes each
run a fresh Claude Code session via `spawn-claude.sh` (one rotated OAuth
token per pane). This is the **"daemon-managed tmux sessions that launch
Loom agents as separate Claude Code sessions"** execution surface. It is
the long-running, multi-account-rotated counterpart to `/loom:sweep`'s
in-session subagent dispatch.

| Surface | Process model | Token model | Best for |
|---------|---------------|-------------|----------|
| `/loom:sweep` (subagent dispatch) | Single process, multiple subagents | Single OAuth token (parent's) | Operator-driven batches, ≤ hours of runtime |
| `./.loom/scripts/daemon.sh` (tmux) | Multiple processes (one per pane + sweep child) | Rotated per process via `spawn-claude.sh` | Multi-day autonomous operation across multiple accounts |

Both surfaces are first-class. Pick by runtime expectations. See
[`docs/migration/v0.10.0-shepherd-deprecation.md`](../../docs/migration/v0.10.0-shepherd-deprecation.md)
for the migration narrative.

## What is removed in v0.10.0

If you were relying on these, you have a migration:

| Removed | Replacement | Where |
|---------|-------------|-------|
| Python `loom-daemon` CLI entry point | `./.loom/scripts/daemon.sh start` (re-implemented) | This page |
| `/shepherd <issue>` slash command | `/loom:sweep <issue>` | `.claude/commands/loom/sweep.md` |
| `loom-shepherd` Python CLI | `claude -p "/loom:sweep <N>" --dangerously-skip-permissions` | Same skill, headless invocation |
| `.loom/daemon-state.json` | `.loom/spawn-loop-state.json` (smaller schema) | Written by `spawn-loop.sh`; see [`spawn-loop.sh status`](../../.loom/scripts/spawn-loop.sh) |
| `.loom/progress/shepherd-*.json` heartbeats | `.loom/sweep-checkpoint/issue-<N>.json` (#3373) | Written by `/loom:sweep` |
| `loom-validate-state` | Presence check on `.loom/spawn-loop-state.json` | Use `[[ -f .loom/spawn-loop-state.json ]]` |
| Support-role daemons running inside the Python brain | GitHub Actions cron workflows | `.github/workflows/loom-*.yml` (#3375) |

For the per-CLI breaking changes and field-level diff, see
[`docs/migration/v0.10.0-shepherd-deprecation.md § Per-CLI breaking
changes`](../../docs/migration/v0.10.0-shepherd-deprecation.md#per-cli-breaking-changes).

For the engineering inventory that drove the deletion decisions (which
consumers retire, which port, which are unchanged), see
[`docs/migration/daemon-state-consumers.md`](../../docs/migration/daemon-state-consumers.md).

## Why this file is intentionally minimal

The legacy schema and tuning advice on this page were a pre-Phase-3
reference for a multi-process Python daemon that polled the forge, scaled
a `shepherd-N` pool, ran work-generation triggers (Architect/Hermit), and
maintained a JSON state file as the canonical source of truth. **None of
that exists post-v0.10.0.** The shell-level daemon mode that survives is
deliberately thinner:

- The daemon **does not** generate work — Architect and Hermit cadence is
  out of scope for v0.10.0 and tracked under follow-up #3381.
- The daemon **does not maintain a shepherd-N pool**. Each ready issue
  detaches its own `claude -p "/loom:sweep N"` child via the spawn loop;
  concurrency is bounded by `MAX_PARALLEL` only.
- The daemon **does not track `pipeline_state`, `warnings`,
  `completed_issues`, `total_prs_merged`, or `last_*_trigger`**. The forge
  is the source of truth for pipeline state; failure counters live in
  `.loom/tokens/.failure_counts` (token-rotation only).
- Support roles run as **cron-driven** GitHub Actions workflows, not as
  long-running daemon-managed processes (though operators can opt to run
  them in tmux panes — see the migration guide). There is no
  `JUDGE_INTERVAL` / `CHAMPION_INTERVAL` / etc. to tune from a daemon
  config.

Re-creating a "compatibility-shape" `daemon-state.json` from the spawn
loop was considered and rejected — see the rationale in
`docs/migration/daemon-state-consumers.md` §"Conclusion: what Phase 3
deletes vs preserves".

## Rust `loom-daemon` (unrelated, still supported)

This page is about the **shell-level Loom daemon mode** + the deleted
**Python** `loom-daemon` CLI. The **Rust** binary at `loom-daemon/`
(Tauri-side IPC daemon, tmux session manager) is a different component
and is unaffected by Phase 3. Its source lives in `loom-daemon/src/`, its
tests in `loom-daemon/tests/`, and its release artifacts ship with the
rest of the Tauri quickstart.
