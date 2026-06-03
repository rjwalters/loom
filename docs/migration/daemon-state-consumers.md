# Inventory: consumers of `daemon-state.json`, `.loom/progress/`, `/shepherd`, and `loom-daemon`

**Issue:** #3377 (Phase 2c of epic #3372 — shepherd/daemon deprecation)
**Status:** First-pass inventory; per-consumer preserve-compat shape design is deferred to follow-up issues.
**Scope:** Investigation only — no code changes other than this document.
**Date:** 2026-06-02

## Goal

Phase 3 of epic #3372 will delete the Python daemon brain (`loom_tools/daemon_v2/`), the Python shepherd brain (`loom_tools/shepherd/`), the `/shepherd` slash command, the `loom-daemon` CLI entry point, and the shell wrappers that drive them. Before that hard deletion can happen, every place in the codebase that **reads** `.loom/daemon-state.json`, `.loom/progress/shepherd-*.json`, or invokes `/shepherd` / `loom-daemon` needs a recorded disposition:

- **retire** — consumer is part of the deleted producer ecosystem and goes away with it.
- **port** — consumer should be rewritten to read `.loom/spawn-loop-state.json` (Phase 1, #3374) or another forge-derived source.
- **preserve-compat** — spawn loop emits a minimal-compat shape of `daemon-state.json` for this consumer; **shape design is out of scope here** (per-consumer follow-up).

Producers and their per-file writes have already been decided: the producers go away in Phase 3. This document concerns the **read** side only — what breaks if we delete the producers without a migration plan.

## Summary

| Disposition | Files | Notes |
|---|---:|---|
| **retire** (deleted with the producer) | 67 | Daemon/shepherd brain internals + their tests + producer-owned shell scripts. |
| **port** (rewrite against spawn-loop-state.json or forge) | 9 | Operator-facing CLIs and `loom-status` that humans still expect to work post-Phase-3. |
| **preserve-compat** (spawn loop emits minimal shape) | 0 | None recommended at this depth. See conclusion. |
| **docs-update** (text-only, no code consumer) | ~25 | Markdown that mentions the deprecated entry points; rewrite during Phase 3. |
| **already-decoupled** (no read — defensive listing only) | 5 | Tauri quickstart, MCP server, `.gitignore` writer in `loom-daemon init`, etc. |

**Total open-source consumers surveyed:** ~106 files.
**Total of those that are READ-side production code (the consumers Phase 3 must decide on):** **9 — all "port".**

The Tauri desktop app the issue speculated about does not exist in this repo. The `quickstarts/desktop/src-tauri/` directory is a Tauri *template* shipped to users and does not read daemon-state. The MCP server (`mcp-loom/`) talks to the Rust daemon via Unix socket, not by reading `daemon-state.json`. Both are listed under "already-decoupled" for completeness.

> **Why no preserve-compat recommendations?** Every read-side production consumer either:
> (a) is an introspection CLI (`loom-status`, `loom-backlog`, `loom-stuck-detection`, `loom-completions`) whose value comes from the daemon brain *being there* — once the brain is deleted, the field set the CLI reports on (`shepherds`, `support_roles`, `recent_failures`, `systematic_failure`) becomes meaningless, and porting to forge-derived state is cleaner than emitting a fake `daemon-state.json`; or
> (b) is a cross-session log (`issue-failures.json`, `recovery-events.json`) that already lives outside `daemon-state.json` and survives the deletion as-is.
> If a per-consumer review later decides a minimal-compat shape *is* worth it, that's a follow-up issue per #3377's "Out of Scope" section.

## Method

Codebase scans (as specified in the issue):

```bash
rg -l 'daemon-state\.json|daemon_state\.json|DaemonState' --type-add 'src:*.{ts,tsx,rs,py,sh,md}' --type src
rg -l '\.loom/progress|progress/shepherd' --type-add 'src:*.{ts,tsx,rs,py,sh,md}' --type src
rg -l '/shepherd\b|loom-daemon\b' --type-add 'src:*.{ts,tsx,rs,py,sh,md}' --type src
```

Each hit was opened and classified along three axes:

1. **Read or write?** Writes are producer-side and go away with the producer — only reads block Phase 3.
2. **Production or test?** Tests for retired modules retire with them.
3. **Surface category:** Tauri / MCP / operator CLI / shell script / documentation / Rust daemon init / installer.

## Category 1: Tauri desktop app

**Decision: retire / not-applicable.**

The issue speculated about a Tauri Health Dashboard, but this repo does not have a Tauri app. The closest things:

| Path | What it is | Reads daemon-state? | Disposition |
|---|---|---|---|
| `quickstarts/desktop/src-tauri/` | A Tauri **template** shipped to users as a quickstart for building their own desktop apps. No Loom-runtime integration. | No — pure scaffold. | **already-decoupled.** No action. |

**Rationale:** there is no in-tree Loom desktop UI that consumes daemon-state. Any downstream-installed Tauri app (e.g. a hypothetical sphere install) is covered by Phase 4 (#3382, downstream sphere migration) and is explicitly out of scope here.

## Category 2: MCP tools

**Decision: already-decoupled. No action required.**

The MCP server (`mcp-loom/`) communicates with the Rust daemon over a Unix domain socket (`.loom/loom-daemon.sock`), not by reading `daemon-state.json`. The relevant tool — `get_ui_state` — reads `.loom/config.json` and `.loom/state.json` (workspace/terminal config, not daemon orchestration state).

The issue named three speculative MCP tools (`get_health_metrics`, `get_active_alerts`, `read_state_file`). Verification:

| Tool name | Status |
|---|---|
| `read_state_file` | **Removed** (use `get_ui_state` instead — `mcp-loom/src/tools/ui.ts:195`, `mcp-loom/README.md:81`). |
| `get_health_metrics` | **Does not exist** in this repo. |
| `get_active_alerts` | **Does not exist** in this repo. |

| Path | Fields read | What it does | Disposition |
|---|---|---|---|
| `mcp-loom/src/tools/ui.ts` (`getUIState`) | `.loom/config.json` (terminals, version, offlineMode); `.loom/state.json` (terminals, nextAgentNumber, daemonPid). | Comprehensive UI state for `get_ui_state` MCP tool. Workspace/terminal info, *not* shepherd/pipeline state. | **already-decoupled** — does not read `daemon-state.json`. |
| `mcp-loom/src/shared/config.ts` | `.loom/loom-daemon.sock`, `.loom/daemon.log` paths (constants only). | Socket connect target; log file location for socket-based daemon RPC. | **already-decoupled** — refers to the Rust daemon socket, which is a separate concern from the Python `loom-daemon` CLI. (Note: the Rust binary is also named `loom-daemon`; it is **not** the deprecated Python entry point. See "Naming-collision warning" below.) |
| `mcp-loom/src/shared/daemon.ts` | None (socket I/O only). | Sends JSON requests to the Rust daemon over Unix socket. | **already-decoupled.** |

### Naming-collision warning

There are **two** binaries named `loom-daemon` in this repo:

1. **Python `loom-daemon`** — entry point registered by `loom-tools` package; runs `loom_tools.daemon_v2.cli.main()`. **This is what Phase 3 deletes.**
2. **Rust `loom-daemon`** — the binary at `loom-daemon/src/main.rs` that backs the MCP socket and the terminal/tmux pool. **This stays.**

Phase 3's grep-sweep needs to distinguish these. The Rust binary is invoked via `./target/release/loom-daemon`, `./.loom/scripts/start-daemon.sh`, and from inside installer scripts. The Python entry point is invoked via shell wrappers in `defaults/scripts/loom-daemon.sh` and `defaults/scripts/daemon.sh`. Any deletion sweep must skip `loom-daemon/src/`, `loom-daemon/tests/`, `loom-api/`, `scripts/start-daemon.sh`, `scripts/stop-daemon.sh`, and `scripts/daemon-headless.sh`.

## Category 3: Operator CLI (the "port" set)

**Decision: port. Rewrite to read `.loom/spawn-loop-state.json` and forge.**

These are user-facing CLIs (`loom-status`, `loom-backlog`, `loom-stuck-detection`, `loom-completions`, `loom-agent-metrics`, `loom-orphan-recovery`, `loom-daemon-cleanup`, `loom-validate-state`, `loom-health-monitor`) that operators still expect to work after Phase 3. Each currently reads `daemon-state.json` and/or `.loom/progress/`, but most of what they report (pipeline counts, blocked issues, in-flight PRs) is recoverable from forge (`gh issue list`, `gh pr list`) + the spawn loop's state file.

| Path | Fields read | What it does | Disposition | Notes |
|---|---|---|---|---|
| `loom-tools/src/loom_tools/status.py` | `daemon_state.completed_issues`, `daemon_state.total_prs_merged`, all shepherd/support-role entries, pipeline state. Reads via `read_daemon_state()`. | `loom-status` CLI — colored terminal summary of pipeline + daemon health. Heavily uses `snapshot.build_snapshot()`. | **port** | Lifetime totals (`completed_issues`, `total_prs_merged`) need a new home if we want to keep them. Pipeline counts can come from forge. Shepherd liveness goes away. |
| `loom-tools/src/loom_tools/snapshot.py` | `read_daemon_state()`, `read_progress_files()`, support-role idle timing, systematic failure state, blocked-issue retry metadata. | The data-collection library used by `loom-status`, `loom-health-monitor`, and the daemon iteration loop. ~85% of fields are daemon-brain-internal. | **port** (split) | This module is two things glued together: (a) a forge-query orchestrator used by operator CLIs (keep, port), (b) a daemon-iteration calculator (retire with daemon brain). Split into `forge_snapshot.py` (port) + `daemon_brain_snapshot.py` (retire). |
| `loom-tools/src/loom_tools/backlog.py` | `daemon_state.blocked_issue_retries`, retry-policy lookups via `snapshot.get_retry_policy()`. | `loom-backlog` CLI — bulk-triage blocked issues, apply retry policies, escalate to human queue. | **port** | Retry metadata lives in `daemon-state.json` ephemerally + `issue-failures.json` durably. Port to read **only** `issue-failures.json` (already cross-session). |
| `loom-tools/src/loom_tools/stuck_detection.py` | `read_progress_files()`, daemon shepherd entries for cross-checking. | `loom-stuck-detection` — multi-strategy detection (heartbeat staleness, missing milestones, error spikes). | **ported (#3392)** | Now reads `.loom/spawn-loop-state.json::running[].last_heartbeat` (added to spawn-loop.sh in same PR). Agent IDs use `sweep-<issue>` form. Retired: `missing_milestone:worktree_created` (no spawn-loop equivalent signal) and the daemon-state shepherd cross-check path. |
| `loom-tools/src/loom_tools/completions.py` | All daemon-state shepherds: `task_id`, `output_file`, `status`, plus `loom:building` GitHub query. | `loom-completions` — polls task output files to detect silent failures. | **ported (Phase 3.1.4, #3393)** | Primary path iterates `.loom/spawn-loop-state.json::running[*].output_file`; daemon-state.json read retained as fallback until Phase 3.4 (#3401). #3393 added `output_file` to spawn-loop task entries (`defaults/scripts/spawn-loop.sh::state_add_child`, `SpawnLoopTask` model). |
| `loom-tools/src/loom_tools/agent_metrics.py` | Reads `~/.loom/activity.db` (preferred) **or** falls back to `daemon-state.json` for `completed_issues` / `total_prs_merged`. | `loom-agent-metrics` — performance and cost metrics by role. | **port** | The fallback path is the only daemon-state read. Activity DB stays; drop the fallback. |
| `loom-tools/src/loom_tools/orphan_recovery.py` | All shepherd entries (status, task_id, issue, pr_number), `recent_failures`, progress files. | `loom-orphan-recovery` — detects orphaned shepherds (untracked `loom:building`, stale heartbeat). | **port** | Re-target at `.loom/spawn-loop-state.json::tasks[]` plus `gh issue list --label loom:building` cross-check. |
| `loom-tools/src/loom_tools/daemon_cleanup.py` | Reads `daemon-state.json` to identify completed sessions, rotation, log archival. | `loom-daemon-cleanup` — event-driven cleanup (`shepherd-complete`, `daemon-startup`, `prune-sessions`). | **port (partial)** | Session rotation goes away with the daemon. Log archival logic ports cleanly (it operates on `.loom/logs/`, not state). Rename to `loom-cleanup` and drop the session-rotation events. |
| `loom-tools/src/loom_tools/validate_state.py` | Direct JSON read of `daemon-state.json`, validates shepherd statuses, task ID format, timestamp fields. | `loom-validate-state` — schema validator for daemon state file. | **retire** | The thing being validated goes away. The 7-char task ID regex is reusable but trivially small. |
| `loom-tools/src/loom_tools/health_monitor.py` | Composes snapshot via `snapshot.build_snapshot()` plus `health-metrics.json` and `alerts.json`. | `loom-health-monitor` — proactive monitoring, alert generation, 24h history. | **port** | Composite score and alerting are useful post-Phase-3 if the inputs change. Inputs ride on `snapshot.py`'s port plan. Decide as a follow-up whether the health score recipe stays the same with forge-derived inputs. |

### Daemon-brain internals (retire)

These live under the producer's roof. They retire as a unit:

- `loom-tools/src/loom_tools/daemon.py` — old daemon loop (V1).
- `loom-tools/src/loom_tools/daemon_v2/` — all submodules: `cli.py`, `loop.py`, `context.py`, `command_poller.py`, `actions/completions.py`, `__init__.py`.
- `loom-tools/src/loom_tools/shepherd/` — full shepherd brain: `cli.py`, `context.py`, `phases/*.py`, `exit_codes.py`, `labels.py`.
- `loom-tools/src/loom_tools/common/state.py` — `read_daemon_state()`, `read_progress_files()`, `find_progress_for_issue()` helpers. Keep generic JSON helpers (`safe_parse_json`, atomic writes); split into a smaller module.
- `loom-tools/src/loom_tools/common/issue_failures.py` — durable failure log. **Keep** (it survives Phase 3 — see `loom-backlog` port).
- `loom-tools/src/loom_tools/common/systematic_failure.py` — daemon-brain-only.
- `loom-tools/src/loom_tools/milestones.py` — milestone writer. Retire with `/shepherd`; spawn loop emits its own simpler heartbeat shape.
- `loom-tools/src/loom_tools/models/daemon_state.py` — typed model for the state file. Retire with the file.
- `loom-tools/src/loom_tools/models/progress.py` — typed model for progress files. Retire with the files.

**Notes for the deletion PR:**

- `LoomPaths.daemon_state_file`, `LoomPaths.progress_dir`, `LoomPaths.progress_file()`, and related constants (`DAEMON_STATE_FILE`, `PROGRESS_DIR`) in `loom-tools/src/loom_tools/common/paths.py` go away with the producer.
- `LoomPaths` itself is reused broadly — keep it. Just drop the four daemon/progress members.

### Tests that retire alongside the producer

These tests exercise daemon/shepherd brain internals and retire with them. Listed for completeness; no migration needed.

`loom-tools/tests/daemon_v2/test_*.py` (all 12 files), `loom-tools/tests/daemon/test_*.py`, `loom-tools/tests/shepherd/test_*.py` (all), and the following top-level tests that exclusively cover retiring modules:

- `test_daemon.py`, `test_daemon_cleanup.py`, `test_daemon_diagnostic.py`, `test_snapshot.py`, `test_status.py`, `test_validate_state.py`, `test_systematic_failure.py`, `test_reset_failures.py`, `test_reset_failures_signal.py`, `test_retry_blocked.py`, `test_orphan_recovery.py`, `test_completions.py`, `test_iteration.py`, `test_support_role_reclaim.py`, `test_stuck_detection.py`, `test_budget_exhaustion.py`, `test_loom_shepherd_preflight.py`, `test_porcelain_parsing.py`, `test_backlog.py`, `test_clean.py` (touches `safe-worktree-cleanup` logic which has a `daemon-state.json` mention).

`test_models.py`, `test_paths.py`, `test_agent_metrics.py`, `test_auth_cache_contention.py`, `test_issue_failures.py`, `test_installation_verification.py`, and `loom-tools/tests/common/test_deprecation.py` test cross-cutting pieces; trim them rather than delete.

## Category 4: Shell scripts (`defaults/scripts/` and `scripts/`)

**Decision: retire most; port a small subset.**

Shell scripts are easier to classify because each script has a stated purpose at its top. The producer-side scripts retire; the operator-side scripts port.

### Shell scripts to **retire** (deleted with the producer)

These are owned by the daemon/shepherd brain or write `daemon-state.json`:

| Script | Purpose | Why retire |
|---|---|---|
| `defaults/scripts/daemon.sh` | Unified daemon start/stop/status wrapper. | Drives Python `loom-daemon`. Replaced by `spawn-loop.sh` + GH Actions. |
| `defaults/scripts/loom-daemon.sh` | Wrapper to invoke the Python daemon entry point. | Deprecated entry point (#3376). |
| `defaults/scripts/loom-shepherd.sh` | Wrapper to invoke `/shepherd`. | Deprecated entry point (#3376). |
| `defaults/scripts/spawn-shell-shepherd.sh` | Shell-only shepherd fallback. | Retires with `/shepherd`. |
| `defaults/scripts/rotate-daemon-state.sh` | Session rotation of `daemon-state.json`. | Producer-side; nothing to rotate. |
| `defaults/scripts/spawn-support-role.sh` | Decision logic for daemon to spawn support roles. | Daemon brain replacement is GH Actions cron (#3375). |
| `defaults/scripts/detect-systematic-failure.sh` | Writes `daemon-state.json::systematic_failure`. | Producer-side. |
| `defaults/scripts/record-blocked-reason.sh` | Writes `daemon-state.json::blocked_issue_retries`. | Producer-side. |
| `defaults/scripts/retry-blocked-issues.sh` | Reads retry cooldown from `daemon-state.json`. | Logic moves to spawn loop / `loom-backlog`. |
| `defaults/scripts/reset-failures.sh` | Resets failure counters across both `daemon-state.json` and `issue-failures.json`. | Producer-side; `issue-failures.json` reset is the only piece that survives. |
| `defaults/scripts/is-force-mode.sh` | Reads `daemon-state.json::force_mode`. | Force mode is daemon-mode-only. Goes away. |
| `defaults/scripts/spawn-loop.sh` | The Phase 1 spawn loop. | **Keep** — this is the survivor. Listed here only because grep matched. |
| `defaults/scripts/spawn-claude.sh` | Multi-account token rotation wrapper. | **Keep** — survives epic #3372 entirely. |
| `defaults/scripts/checkpoint.sh` | Sweep checkpoint helper (#3373). | **Keep** — companion to spawn loop. |
| `defaults/scripts/report-milestone.sh` | Writes `.loom/progress/shepherd-*.json`. | Retires with `/shepherd`. |
| `defaults/scripts/cleanup-progress.sh` | Cleans `.loom/progress/` files. | Producer-side; directory itself goes away. |
| `defaults/scripts/stale-building-check.sh` | Uses progress files. | Move to forge-only detection. |
| `defaults/scripts/recover-orphaned-shepherds.sh` | Operator-facing recovery; reads daemon-state and progress. | **port** with `orphan_recovery.py`. |
| `defaults/scripts/validate-daemon-state.sh` | Companion to `validate_state.py`. | Retire alongside its Python counterpart. |
| `defaults/scripts/loom-status.sh` | Legacy bash status script (deprecated; replaced by Python `loom-status`). | Retire. |
| `defaults/scripts/health-check.sh` | Legacy bash health check (deprecated; replaced by `loom-health-monitor`). | Retire. |
| `defaults/scripts/session-reflection.sh` | Daemon shutdown self-improvement step. | Daemon brain leaves. Useful behavior can be replanned post-Phase-3. |
| `defaults/scripts/archive-logs.sh` | Archives task outputs + daemon logs. | The log-archival logic ports cleanly; the daemon-state references are incidental. **port-trim.** |
| `defaults/scripts/analyze-test-failures.sh` | Parses `.loom/progress/shepherd-*.json`. | Retire with progress files. |
| `defaults/scripts/doctor-effectiveness.sh` | Parses `.loom/progress/shepherd-*.json`. | Retire with progress files. |
| `defaults/scripts/agent-wait-bg.sh` | Uses `.loom/progress/` for heartbeat polling. | Retire with progress files. |

### Shell scripts to **port** or **keep-mostly-intact**

| Script | Purpose | Disposition |
|---|---|---|
| `defaults/scripts/recover-orphaned-shepherds.sh` | Operator-facing recovery. | **port** with `orphan_recovery.py` (Category 3). |
| `defaults/scripts/archive-logs.sh` | Log archival. | **port-trim** — drop the daemon-state references; log-archival logic survives. |
| `defaults/scripts/verify-install.sh` | Verifies installation. References `.loom/daemon-state.json` and `.loom/progress/` in a help blurb only. | **docs-update** — remove the two help lines. |
| `defaults/scripts/tests/test-install-active-session.sh` | Tests `check-active-session.sh`. Writes test fixtures of `daemon-state.json` to verify install detection. | **port** alongside `check-active-session.sh`. |
| `scripts/install/check-active-session.sh` | Detects active Loom session before installer runs. Reads `daemon-state.json::running` + mtime. | **port** — re-target at `.loom/spawn-loop-state.json` (which the spawn loop already writes when running). |
| `scripts/safe-worktree-cleanup.sh` | Worktree cleanup with `daemon-state.json` lock check. | **port** — re-target at spawn loop's claim-locks instead. |
| `scripts/install-loom.sh` | Installer. Greps for the daemon/shepherd files to detect drift; treats `daemon-state.json` as ephemeral. | **docs-update + drift list update.** |
| `scripts/uninstall-loom.sh` | Counterpart to installer. | **docs-update.** |
| `scripts/test-installer.sh` | Installer integration test. | **port** alongside installer. |
| `scripts/start-daemon.sh`, `scripts/stop-daemon.sh`, `scripts/daemon-headless.sh` | **Rust** loom-daemon control scripts (different binary). | **already-decoupled** — unrelated to the Python daemon. Verify and leave alone. |
| `scripts/daemon-cleanup.sh` | Old shell daemon cleanup (vs Python `loom-daemon-cleanup`). | **retire.** |
| `scripts/test-daemon-scripts.sh` | Tests the daemon shell scripts. | **retire** with them. |
| `scripts/archive-logs.sh` | Top-level wrapper for `defaults/scripts/archive-logs.sh`. | **port-trim** with its target. |

### Installer touch points

Two installer files reference the deprecated entry points:

- `install.sh` (top-level installer dispatch) → mentions `loom-daemon`. Docs/help text only.
- `uninstall.sh` → counterpart; same.

Both are docs-update.

## Category 5: Rust daemon (`loom-daemon/`)

**Decision: already-decoupled.**

The Rust binary at `loom-daemon/src/main.rs` is a different binary from the deprecated Python `loom-daemon` CLI (see the **naming-collision warning** in Category 2). The Rust daemon does not read `daemon-state.json`.

The only Rust references to `daemon-state.json` are in `loom-daemon/src/init/post_init.rs`, where the binary writes `.gitignore` patterns during workspace initialization. These patterns list `daemon-state.json`, `[0-9][0-9]-daemon-state.json`, and `.loom/progress/` as files to ignore.

| Path | Fields read | What it does | Disposition |
|---|---|---|---|
| `loom-daemon/src/init/post_init.rs` | None (writes `.gitignore` patterns as static strings). | Adds ephemeral-files patterns to user `.gitignore` during install. | **docs-update / pattern-list-trim** — Phase 3 should drop `.loom/daemon-state.json`, `.loom/[0-9][0-9]-daemon-state.json`, `.loom/progress/`, `.loom/stuck-history.json`, `.loom/alerts.json`, `.loom/health-metrics.json` from this list. Replace with `.loom/spawn-loop-state.json`. The associated tests (lines 247, 249, 253, 283, 284, 297, 318, 322, 335, 502) also need their assertions updated. |

Other Rust references in `loom-daemon/src/main.rs`, `loom-daemon/src/init/git.rs`, `loom-daemon/src/init/mod.rs`, `loom-daemon/src/forge_parser.rs`, `loom-daemon/src/git_parser.rs`, `loom-daemon/src/activity/test_parser.rs` are help text or string parsing — not state-file consumers. **already-decoupled** for the purpose of Phase 3.

## Category 6: Documentation

**Decision: docs-update during Phase 3 (one sweep).**

Documentation references are surface-level — they don't break anything if Phase 3 deletes the producer, but they confuse users. Group them and update in a single Phase-3 pass.

| Path | Refs | Disposition |
|---|---:|---|
| `CLAUDE.md` (this repo's project memory) | ~30 mentions of `daemon-state.json`, `.loom/progress/`, `/shepherd`, `loom-daemon` across architecture, workflow, configuration sections. | **rewrite section by section** — daemon architecture diagram, "Daemon State File" subsection, "Shepherd Progress Milestones", "Required Terminal Configuration", "Session Rotation". The "Migration" subsection at the bottom is the authoritative announcement and stays current. |
| `defaults/CLAUDE.md` | The CLAUDE.md template shipped to installs. | Mirrors source CLAUDE.md changes. |
| `.loom/docs/daemon-reference.md` | Comprehensive daemon state file reference. | **retire** the bulk; keep a brief "deprecated, see spawn-loop-state.json" stub. |
| `.loom/docs/troubleshooting.md` | Operator commands like `cat .loom/daemon-state.json \| jq`. | **rewrite** to use spawn-loop equivalents. |
| `docs/guides/cli-reference.md` (67 refs) | CLI reference for `/shepherd`, `loom-daemon`, etc. | **rewrite** to feature `/loom:sweep` and spawn loop. |
| `docs/guides/ci-cd-setup.md` (77 refs) | CI/CD guide that mentions daemon integration. | **rewrite** — feature GH Actions cron (#3375) and spawn loop. |
| `docs/guides/getting-started.md` (23 refs) | First-run walkthrough. | **rewrite** to point at spawn loop instead of `/shepherd`. |
| `docs/guides/common-tasks.md`, `docs/guides/dev-workflow.md`, `docs/guides/development.md`, `docs/guides/troubleshooting.md`, `docs/guides/daemon-dev-mode.md`, `docs/guides/quickstart-tutorial.md`, `docs/guides/testing.md` | Various touch points. | **rewrite per page** during Phase 3 docs sweep. |
| `docs/mcp/README.md`, `docs/mcp/loom-terminals.md` | MCP docs. References mostly to the Rust daemon socket. | **light-edit** to clarify Rust-daemon vs. deprecated Python entry point. |
| `docs/agents.md`, `docs/api/README.md`, `docs/philosophy/loom-intelligence.md` | Architecture narrative. | **rewrite** the deprecated-component narrative. |
| `docs/adr/0004-worktree-paths-inside-workspace.md`, `docs/adr/0008-tmux-daemon-architecture.md` | Architecture Decision Records. | **add a follow-up ADR** that records the Phase 3 deprecation; don't rewrite the historical ADRs. |
| `CHANGELOG.md` | Historical entries naming `daemon-state.json`. | **no change** — historical records stay. The Phase 3 changelog entry will note the deletion. |
| `defaults/README.md`, `README.md`, `CONTRIBUTING.md` | Top-level docs. | **light-edit** to mention spawn loop + GH Actions. |
| `defaults/scripts/cli/loom-help.sh` | `loom --help` text. | **rewrite** for Phase 3. |
| `defaults/scripts/lib/deprecation.sh` | The deprecation-warning helper itself (Phase 2b). | **retire** in Phase 3 (warning is no longer needed after the deletion). |
| `defaults/scripts/check-host-sleep.sh` | Mentions `/sweep`, `/loom`, `/shepherd` in advisory text. | **light-edit** to drop `/shepherd`. |
| `defaults/scripts/validate-toolchain.sh` | Validates toolchain. May reference deprecated binaries. | **trim** the deprecated entries. |
| `defaults/hooks/methodology-inject.sh` | Methodology context injected into agent prompts. | **rewrite** for Phase 3. |
| `WORK_LOG.md`, `WORK_PLAN.md` | Personal/team logs. | **leave** unless explicitly stale. |
| `defaults/scripts/cleanup-progress.sh`, etc. | Already covered under shell-scripts. | Already classified. |
| `loom-daemon/tests/README.md` | Test README. | **light-edit.** |
| `mcp-loom/README.md` | MCP README, mentions `read_state_file` removal. | **already-decoupled** (already noting removal). |
| `mcp-loom/src/shared/config.ts` | Code, not docs — already covered. | — |
| `scripts/README.md` | Scripts README. | **rewrite** for retained scripts only. |
| `scripts/version.sh` | Version bumping script. References `loom-daemon` package. | **already-decoupled** (refers to Rust crate, not Python entry point — verify and leave). |

## Category 7: Helpers / wrappers / one-offs

| Path | Reads daemon-state? | Disposition |
|---|---|---|
| `loom-tools/src/loom_tools/common/deprecation.py` | The Phase 2b warning emitter for Python entry points. | **retire** in Phase 3. |
| `loom-tools/src/loom_tools/test_failure_analysis.py` | Reads `.loom/progress/shepherd-*.json` for test-failure pattern analysis. | **retire** with progress files. (The analysis itself is interesting; if it has standalone value, port it against builder output logs instead.) |
| `loom-tools/src/loom_tools/validate_phase.py` | References `/shepherd` in docstring; no state-file reads. | **light-edit** — drop `/shepherd` reference. |
| `loom-tools/src/loom_tools/common/paths.py` | Defines the path constants. | **trim** the four daemon/progress constants. |
| `loom-tools/src/loom_tools/clean.py` | `loom-clean` CLI. Reads `daemon-state.json` to decide whether worktrees are in use. | **port** — re-target at spawn-loop-state.json's `tasks[].issue` claim set. |

## Per-category dispositions (human-reviewable summary)

The issue's acceptance criteria require at least one human-reviewed disposition per category. Here is the explicit per-category recommendation, with rationale:

### Tauri dashboards: **n/a (already-decoupled)**

The repo does not have a Tauri desktop app that reads `daemon-state.json`. Phase 3 has no Tauri-side work to do.

### MCP tools: **n/a (already-decoupled)**

`mcp-loom` does not read `daemon-state.json` or `.loom/progress/` directly. Verified by grep — zero hits in `mcp-loom/src/`. The MCP server talks to the Rust daemon over Unix socket, which is a separate concern. Phase 3 has no MCP-side work to do.

### Operator CLI: **port**

All nine operator-facing CLIs that currently consume `daemon-state.json` or `.loom/progress/` (`loom-status`, `loom-backlog`, `loom-stuck-detection`, `loom-completions`, `loom-agent-metrics`, `loom-orphan-recovery`, `loom-daemon-cleanup`, `loom-health-monitor`, `loom-clean`) should be **ported** to read `.loom/spawn-loop-state.json` (Phase 1, #3374) and forge state. Per-port follow-up issues are needed because each CLI has a different shape:

1. `loom-status` — biggest port; ~80% of its data comes from forge already (via `gh_parallel_queries`), the rest needs a spawn-loop-state.json source.
2. `loom-backlog` — port to `issue-failures.json` only (already cross-session).
3. `loom-stuck-detection` — needs new heartbeat shape in spawn-loop-state.
4. `loom-completions` — depends on where spawn loop puts output files.
5. `loom-agent-metrics` — drop the `daemon-state.json` fallback; activity DB stays.
6. `loom-orphan-recovery` — straightforward port to forge cross-check.
7. `loom-daemon-cleanup` → rename to `loom-cleanup`; drop session-rotation events.
8. `loom-health-monitor` — composite score recipe needs re-evaluation post-port (follow-up).
9. `loom-clean` — port to spawn-loop claim set.

`loom-validate-state` retires (it validates a file that's going away).

**Rationale for port over preserve-compat:** the daemon-state file's shape exists because the daemon brain *was* computing all that state. Once the brain is deleted, emitting a fake daemon-state shape from the spawn loop is a maintenance trap — the shape suggests state that no longer exists. The CLIs are happier reading a smaller, honest spawn-loop-state file plus forge queries.

### Shell scripts: **retire most, port six**

Of ~30 daemon/shepherd-adjacent shell scripts in `defaults/scripts/`, only six need work:

- **port:** `recover-orphaned-shepherds.sh`, `archive-logs.sh` (trim), `check-active-session.sh`, `safe-worktree-cleanup.sh`.
- **docs-update:** `verify-install.sh`, `install-loom.sh`, `uninstall-loom.sh`, `tests/test-install-active-session.sh`.
- **keep:** `spawn-loop.sh`, `spawn-claude.sh`, `checkpoint.sh`, `sweep-checkpoint.sh`, and the Rust-daemon control scripts (`start-daemon.sh`, `stop-daemon.sh`, `daemon-headless.sh`).
- **retire:** everything else under "Shell scripts to retire" above (22 scripts).

**Rationale:** the deprecated entry points (`/shepherd`, `loom-daemon` Python CLI) are produced by these scripts. Removing them simplifies the surface area substantially. The six porting cases are operator-facing and worth the engineering.

### Documentation: **docs-update sweep during Phase 3**

~25 markdown files mention the deprecated entry points. The bulk of the changes are mechanical (search-and-replace `/shepherd` with `/loom:sweep`, `loom-daemon` with the appropriate Phase 1/2a entry, `.loom/daemon-state.json` with `.loom/spawn-loop-state.json` where the field is preserved). A single Phase-3 docs PR can land these together. Architecture-level documents (`docs/adr/`, `docs/philosophy/loom-intelligence.md`, `CLAUDE.md`'s "Three-Layer Architecture") need narrative rewrites, not search-and-replace — those should be a separate Phase-3 PR.

**Rationale:** documentation is the cheapest category and doesn't block code deletion. Bundle it with Phase 3 for atomic correctness; do not delay Phase 3 over docs.

### Rust daemon (`loom-daemon/`): **gitignore pattern trim only**

The Rust binary writes static `.gitignore` patterns in `post_init.rs`. Phase 3 should remove `.loom/daemon-state.json`, `.loom/[0-9][0-9]-daemon-state.json`, `.loom/progress/`, `.loom/stuck-history.json`, `.loom/alerts.json`, `.loom/health-metrics.json` from the pattern list (and update the unit tests that assert these patterns are present).

**Rationale:** the Rust daemon is a different binary from the deleted Python `loom-daemon` CLI. Phase 3 must not touch the Rust binary except for the trivial pattern-list cleanup.

## Conclusion: what Phase 3 deletes vs preserves

### Phase 3 deletes (no migration shim needed):

- **Python:** `loom_tools/daemon_v2/` (entire package), `loom_tools/shepherd/` (entire package), `loom_tools/daemon.py`, `loom_tools/snapshot.py` (after split — see below), `loom_tools/health_monitor.py` (after port), `loom_tools/validate_state.py`, `loom_tools/milestones.py`, `loom_tools/common/state.py::read_daemon_state` + `read_progress_files` + `find_progress_for_issue`, `loom_tools/common/systematic_failure.py`, `loom_tools/common/deprecation.py`, `loom_tools/models/daemon_state.py`, `loom_tools/models/progress.py`, `loom_tools/test_failure_analysis.py`.
- **Python tests:** the full retire-set listed under "Tests that retire alongside the producer".
- **Shell:** the 22 scripts in "Shell scripts to retire", plus `defaults/scripts/lib/deprecation.sh`, plus the test-only scripts `scripts/test-daemon-scripts.sh`.
- **`/shepherd`:** the Claude Code slash command at `defaults/.claude/commands/loom/shepherd.md` (referenced by `loom-shepherd.sh`).
- **Entry-point registrations** in `loom-tools/pyproject.toml` (or equivalent) for `loom-daemon` (Python), `loom-shepherd`, and any retiring CLIs (`loom-validate-state`, `loom-completions` if not ported, etc.).
- **Files on disk:** `.loom/daemon-state.json`, `.loom/[0-9][0-9]-daemon-state.json` archives, `.loom/progress/`, `.loom/alerts.json`, `.loom/health-metrics.json`, `.loom/stuck-history.json`, `.loom/issue-failures.json` (this last one is durable — decide separately whether to keep it for `loom-backlog` port), `.loom/daemon-metrics.json`, `.loom/baseline-health.json`. The Rust daemon's `post_init.rs` `.gitignore` writer drops these patterns too.

### Phase 3 preserves (port via follow-up issues):

- **The nine operator CLIs** listed under "Category 3: Operator CLI (the port set)" — each as its own follow-up issue.
- **`issue-failures.json`** as the durable failure log (`loom-backlog` consumer).
- **`activity.db`** as the metrics source (`loom-agent-metrics` consumer).
- **The `snapshot.py` *forge-query* half** — split out a `forge_snapshot.py` module that retains only the `gh_parallel_queries` orchestration and the issue-sorting/filtering helpers. The pipeline-health computation, support-role idle math, and systematic-failure state retire.
- **Rust daemon, MCP server, Tauri quickstart, Rust-daemon control scripts** — already decoupled; no changes.

### Phase 3 does NOT design a `daemon-state.json` compatibility shim.

Of the ~106 files surveyed, zero consumers warrant a preserve-compat shape. Every read-side consumer either retires with the producer or ports cleanly to spawn-loop-state.json + forge. If a per-consumer review later surfaces a justified preserve-compat need (e.g. an external downstream tool not in this repo), that's a Phase 4 / follow-up concern.

### Recommended Phase 3 PR sequencing

To keep Phase 3 PRs small and bisectable:

1. **PR 3.1 — Port the nine CLIs.** One PR per CLI (nine sub-PRs) so each port can land independently. Each PR introduces the spawn-loop-state.json read path while leaving the daemon-state.json read path as a fallback (so the CLI still works while the daemon is alive).
2. **PR 3.2 — Delete the daemon brain.** `daemon_v2/`, `daemon.py`, `snapshot.py` (split first), the producer shell scripts.
3. **PR 3.3 — Delete the shepherd brain.** `shepherd/`, `milestones.py`, `report-milestone.sh`, the `/shepherd` skill.
4. **PR 3.4 — Trim CLIs' daemon-state fallbacks.** Remove the daemon-state.json read paths added as fallback in PR 3.1.
5. **PR 3.5 — Rust daemon `.gitignore` pattern cleanup.** Trivial one-liner change + test updates in `post_init.rs`.
6. **PR 3.6 — Documentation sweep.** Search-and-replace pass across `docs/`, `CLAUDE.md`, `.loom/docs/`, `README.md`, etc.
7. **PR 3.7 — Architecture rewrites.** Narrative pass on `docs/adr/`, `docs/philosophy/loom-intelligence.md`, `CLAUDE.md` Three-Layer Architecture section.

PRs 3.1.x can land in parallel. PRs 3.2 and 3.3 should land sequentially. PR 3.4 depends on 3.2 and 3.3. PR 3.5 is independent. PRs 3.6 and 3.7 are independent of all the above (docs-only).

---

**End of inventory.** Per the issue, "preserve-compat shape design" for any specific consumer is deferred to a per-consumer follow-up issue. None are recommended at this depth.
