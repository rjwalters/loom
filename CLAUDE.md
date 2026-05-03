# Loom Orchestration - Repository Guide

This repository uses **Loom** for AI-powered development orchestration.

**Loom Version**: 0.7.0
**Installation Date**: {{INSTALL_DATE}}

## What is Loom?

Loom is a multi-terminal desktop application for macOS that orchestrates AI-powered development workers using git worktrees and a forge (GitHub or Gitea) as the coordination layer. It enables both automated orchestration (Tauri App Mode) and manual coordination (Manual Orchestration Mode).

**Loom Repository**: https://github.com/rjwalters/loom

## Three-Layer Architecture

```
┌─────────────────────────────────────────────────────────────────┐
│                    Layer 3: Human Observer                      │
│  - Watches system health and intervenes when needed             │
│  - Overrides Champion on controversial proposals                │
└─────────────────────────────────────────────────────────────────┘
                              │ observes/intervenes
                              ▼
┌─────────────────────────────────────────────────────────────────┐
│                    Layer 2: Loom Daemon                         │
│  /loom - Continuous system orchestrator                         │
│  - Monitors state, generates work, scales shepherds             │
└─────────────────────────────────────────────────────────────────┘
                              │ spawns/manages
                              ▼
┌─────────────────────────────────────────────────────────────────┐
│                    Layer 1: Shepherds                           │
│  /shepherd <issue> - Single-issue lifecycle orchestrator        │
│  - Coordinates: Curator → Builder → Judge → Doctor → Merge      │
└─────────────────────────────────────────────────────────────────┘
                              │ triggers
                              ▼
┌─────────────────────────────────────────────────────────────────┐
│                    Worker Roles                                 │
│  Curator, Builder, Judge, Doctor, etc.                          │
│  - Execute single tasks (curate issue, build feature, review)   │
└─────────────────────────────────────────────────────────────────┘
```

| Layer | Role | Purpose | Mode |
|-------|------|---------|------|
| Layer 3 | Human | Oversight - approve proposals, handle edge cases | Observer |
| Layer 2 | `/loom` | System orchestration - work generation, shepherd scaling | Continuous daemon |
| Layer 1 | `/shepherd <issue>` | Issue orchestration - lifecycle from creation to merge | Per-issue |
| Layer 0 | `/builder`, `/judge`, etc. | Task execution - single focused work units | Per-task |

**Use `/shepherd <issue>`** when you have a specific issue to implement.
**Use `/loom`** for fully autonomous development with work generation.

## Usage Modes

### 1. Manual Orchestration Mode (MOM)

Use Claude Code terminals with specialized roles for hands-on development:

1. Open Claude Code in this repository
2. Use slash commands: `/builder`, `/judge`, `/curator`, etc.
3. Each terminal acts as a specialized agent

### 2. Tauri App Mode

Launch the Loom desktop application for automated orchestration:

1. Install Loom app, open it, select this repository
2. Configure terminals with roles and intervals
3. Start engine - terminals launch automatically

### 3. Daemon Mode (Layer 2)

```bash
./.loom/scripts/daemon.sh start                  # auto-build enabled (default)
./.loom/scripts/daemon.sh start --no-auto-build  # support roles only
```

```bash
/loom           # Activate daemon orchestration (daemon must be started first)
/loom --merge   # Aggressive autonomous development
```

**Default mode** (no flags): Daemon manages support roles (judge, champion, doctor, auditor, curator, guide) AND auto-spawns shepherds from the `loom:issue` queue. Once orchestration is activated via `/loom`, issues are picked up and built automatically.

**Support-only mode** (`--no-auto-build` flag): Daemon manages support roles only. Shepherds are NOT auto-spawned. Use `/shepherd <N>` to shepherd specific issues manually. The `--auto-build` / `-a` flag is retained for backward compatibility but is now a no-op (already the default).

**Merge Mode** enables: auto-promotion of proposals, auto-merge after Judge approval, audit trail with `[force-mode]` markers, safety guardrails still apply. Merge mode does **not** skip the Judge phase (code review always runs due to GitHub's self-review API restriction). Merge mode implies `--auto-build`.

**Batch Orchestration Pattern**: In normal mode, the `loom:curated` -> `loom:issue` transition requires a human to promote the issue (the human approval gate). In batch/high-throughput sessions, use `--merge` mode to have Champion auto-promote `loom:curated` issues to `loom:issue`, replacing the manual gate with Champion's automated quality evaluation. No separate `loom:approved` label is needed -- `--merge` makes the existing lifecycle fully automatic.

**Graceful shutdown**: `./.loom/scripts/daemon.sh stop` (or `touch .loom/stop-daemon`)

## Agent Roles

### Orchestration Roles

| Role | File | Purpose |
|------|------|---------|
| Loom Daemon | `loom.md` | System orchestration, work generation (Layer 2) |
| Shepherd | `shepherd.md` | Single-issue lifecycle orchestration (Layer 1) |

### Worker Roles

| Role | File | Purpose | Mode |
|------|------|---------|------|
| Builder | `builder.md` | Implement features and fixes | Manual |
| Judge | `judge.md` | Evaluate pull requests | Autonomous 5min |
| Champion | `champion.md` | Evaluate proposals, auto-merge PRs | Autonomous 10min |
| Curator | `curator.md` | Enhance and organize issues | Autonomous 5min |
| Architect | `architect.md` | Create architectural proposals | Autonomous 15min |
| Hermit | `hermit.md` | Identify simplification opportunities | Autonomous 15min |
| Doctor | `doctor.md` | Fix bugs and address PR feedback | Manual |
| Guide | `guide.md` | Prioritize and triage issues | Autonomous 15min |
| Driver | `driver.md` | Direct command execution | Manual |
| Auditor | `auditor.md` | Validate main branch build and runtime | Autonomous 10min |

Full role definitions: `.loom/roles/*.md`

## Label-Based Workflow

Agents coordinate through GitHub labels. See `.github/labels.yml` for full definitions.

### Label Flow

**Issue Lifecycle**:
```
(created) → loom:issue → loom:building → (closed)
           ↑ Curator      ↑ Builder

(created) → loom:curating → loom:curated → loom:issue
           ↑ Curator        ↑ Curator      ↑ Champion approves
```

**PR Lifecycle**:
```
(created) → loom:review-requested → loom:pr → (auto-merged)
           ↑ Builder                ↑ Judge    ↑ Champion
```

**Proposal Lifecycle**:
```
(created) → loom:architect/loom:hermit/loom:auditor → (evaluated) → loom:issue
           ↑ Architect/Hermit/Auditor                 ↑ Champion    ↑ Ready for Builder
```

**Epic Lifecycle**: `loom:epic` → Champion creates phased `loom:architect` + `loom:epic-phase` issues.

> **Note on label cleanup**: Loom intentionally does **not** remove labels from closed issues or merged PRs (e.g., `loom:pr` remains on merged PRs). Labels on closed/merged items are harmless — all agents filter by open state — and skipping post-close label removal saves gh API calls. Do not implement label cleanup on merge/close (see issue #2838).

## Git Worktree Workflow

Loom uses git worktrees to isolate agent work.

**Terminal Worktrees** (`.loom/worktrees/terminal-N`): Agent isolation in Tauri App Mode only.

**Issue Worktrees** (`.loom/worktrees/issue-N`): Issue-specific work for Builder agents.

### Creating Worktrees

```bash
# Claim issue and create worktree
gh issue edit 42 --remove-label "loom:issue" --add-label "loom:building"
./.loom/scripts/worktree.sh 42
cd .loom/worktrees/issue-42

# Work, commit, push, create PR
git push -u origin feature/issue-42
gh pr create --label "loom:review-requested"
```

### Best Practices

- Always use `./.loom/scripts/worktree.sh <issue-number>`
- Never run `git worktree` directly (helper prevents nested worktrees)
- Worktrees auto-removed when PRs merged

### Merging PRs

**Never use `gh pr merge`** -- always use `./.loom/scripts/merge-pr.sh <PR_NUMBER>` instead. The `gh pr merge` command attempts a local checkout which fails when the PR branch is linked to a worktree. The merge script merges via the forge API directly and handles worktree cleanup automatically.

```bash
./.loom/scripts/merge-pr.sh <PR_NUMBER>         # Standard merge with worktree cleanup
./.loom/scripts/merge-pr.sh <PR_NUMBER> --auto   # Auto-confirm (for automation)
./.loom/scripts/merge-pr.sh <PR_NUMBER> --dry-run # Preview without merging
```

## Development Workflow

### Shepherd Lifecycle (MANDATORY)

When implementing issues — whether manually, via `/shepherd`, or by spawning subagents — **all stages of the shepherd lifecycle must be executed in order**. Do not skip stages.

```
Curator → Builder → Judge → Doctor (if needed) → Merge
```

| Stage | What happens | Skip allowed? |
|-------|-------------|---------------|
| **Curator** | Enrich the issue with technical details, acceptance criteria, scope | No |
| **Builder** | Implement, test, commit, create PR | No |
| **Judge** | Review the PR, approve or request changes | No |
| **Doctor** | Fix issues from judge feedback | Only if judge approves |
| **Merge** | Champion auto-merges approved PRs | No |

**When spawning subagents to shepherd issues**: each subagent must run the full lifecycle, not just the builder phase. If parallelizing multiple issues, each agent must independently execute Curator → Builder → Judge → Doctor → Merge. Simply creating a PR and labeling it `loom:review-requested` is only the Builder stage — the work is not complete until the PR has been reviewed and merged.

**When using `/shepherd`**: the skill handles this automatically. Prefer `/shepherd <issue>` over manual orchestration to avoid accidentally skipping stages.

### Builder Workflow

1. Find issue: `gh issue list --label="loom:issue"`
2. Claim: `gh issue edit 42 --remove-label "loom:issue" --add-label "loom:building"`
3. Create worktree: `./.loom/scripts/worktree.sh 42 && cd .loom/worktrees/issue-42`
4. Implement, test, commit
5. Create PR: `git push -u origin feature/issue-42 && gh pr create --label "loom:review-requested" --body "Closes #42"`

### Judge Workflow

1. Find PR: `gh pr list --label="loom:review-requested"`
2. Review: `gh pr checkout 123`
3. Approve: `gh pr comment 123 --body "LGTM! Approved." && gh pr edit 123 --remove-label "loom:review-requested" --add-label "loom:pr"`
4. Or request changes: `gh pr comment 123 --body "Changes needed: ..." && gh pr edit 123 --remove-label "loom:review-requested" --add-label "loom:changes-requested"`

**Note**: Use `gh pr comment` instead of `gh pr review --approve` — GitHub's API prevents self-review, and Loom agents often create and review the same PR. Labels are the coordination mechanism.

### Curator Workflow

1. Find unlabeled issues: `gh issue list --label="!loom:issue,!loom:building,!loom:architect,!loom:hermit,!loom:curated,!loom:curating"`
2. Enhance issue with technical details
3. Mark curated: `gh issue edit 42 --add-label "loom:curated"`

## Configuration

### Workspace Configuration

Configuration stored in `.loom/config.json` (committed to git for team sharing):

```json
{
  "nextAgentNumber": 3,
  "terminals": [
    {
      "id": "terminal-1",
      "name": "Builder",
      "role": "builder",
      "roleConfig": {
        "workerType": "claude",
        "roleFile": "builder.md",
        "targetInterval": 0,
        "intervalPrompt": ""
      }
    }
  ]
}
```

### Daemon Configuration

See `.loom/docs/daemon-reference.md` for detailed daemon configuration including:
- Configuration parameters (ISSUE_THRESHOLD, MAX_SHEPHERDS, etc.)
- Issue selection strategies (fifo, lifo, priority)
- Daemon state file structure
- Session rotation

**Daemon State File** (`.loom/daemon-state.json`):

The daemon state file provides comprehensive information for debugging, crash recovery, and system observability.

```json
{
  "started_at": "2026-01-23T10:00:00Z",
  "last_poll": "2026-01-23T11:30:00Z",
  "running": true,
  "iteration": 42,
  "daemon_session_id": "1706400000-12345",

  "shepherds": {
    "shepherd-1": {
      "status": "working",
      "issue": 123,
      "task_id": "abc123",
      "output_file": "/tmp/claude/.../abc123.output",
      "started": "2026-01-23T10:15:00Z",
      "last_phase": "builder",
      "pr_number": null
    },
    "shepherd-2": {
      "status": "idle",
      "issue": null,
      "idle_since": "2026-01-23T11:00:00Z",
      "idle_reason": "no_ready_issues",
      "last_issue": 100,
      "last_completed": "2026-01-23T10:58:00Z"
    }
  },

  "pipeline_state": {
    "ready": ["#1083", "#1080"],
    "building": ["#1044"],
    "review_requested": ["PR #1056"],
    "changes_requested": ["PR #1059"],
    "ready_to_merge": ["PR #1058"],
    "blocked": [
      {
        "type": "pr",
        "number": 1059,
        "reason": "merge_conflicts",
        "detected_at": "2026-01-23T11:20:00Z"
      }
    ],
    "last_updated": "2026-01-23T11:30:00Z"
  },

  "warnings": [
    {
      "time": "2026-01-23T11:10:00Z",
      "type": "blocked_pr",
      "severity": "warning",
      "message": "PR #1059 has merge conflicts",
      "context": {"pr_number": 1059, "requires_role": "doctor"},
      "acknowledged": false
    }
  ],

  "completed_issues": [100, 101, 102],
  "total_prs_merged": 3,
  "last_architect_trigger": "2026-01-23T10:00:00Z",
  "last_hermit_trigger": "2026-01-23T10:30:00Z"
}
```

**Shepherd Status Values**:
- `working` - Actively processing an issue
- `idle` - No issue assigned, waiting for work
- `errored` - Encountered an error, may need intervention
- `paused` - Manually paused via signal or stuck detection

**Idle Reasons**:
- `no_ready_issues` - No issues with `loom:issue` label available
- `at_capacity` - All shepherd slots filled
- `completed_issue` - Just finished an issue, waiting for next
- `rate_limited` - Paused due to API rate limits
- `shutdown_signal` - Paused due to graceful shutdown

**Warning Types**:
- `blocked_pr` - PR has merge conflicts or failed checks
- `shepherd_error` - Shepherd encountered recoverable error
- `role_failure` - Support role failed to complete
- `stuck_agent` - Agent detected as stuck

### Shepherd Progress Milestones

Shepherds report progress milestones to `.loom/progress/` for daemon visibility. This enables:
- Real-time progress monitoring without parsing output files
- Heartbeat-based stuck detection (more reliable than file timestamps)
- Phase-level visibility into shepherd activity
- Better debugging when issues arise

**Progress File** (`.loom/progress/shepherd-{task_id}.json`):

```json
{
  "task_id": "a7dc1e0",
  "issue": 123,
  "mode": "default",
  "started_at": "2026-01-25T10:00:00Z",
  "current_phase": "builder",
  "last_heartbeat": "2026-01-25T10:15:00Z",
  "status": "working",
  "milestones": [
    {"event": "started", "timestamp": "2026-01-25T10:00:00Z", "data": {"issue": 123, "mode": "default"}},
    {"event": "phase_entered", "timestamp": "2026-01-25T10:01:00Z", "data": {"phase": "curator"}},
    {"event": "phase_entered", "timestamp": "2026-01-25T10:05:00Z", "data": {"phase": "builder"}},
    {"event": "worktree_created", "timestamp": "2026-01-25T10:06:00Z", "data": {"path": ".loom/worktrees/issue-123"}},
    {"event": "heartbeat", "timestamp": "2026-01-25T10:15:00Z", "data": {"action": "running tests"}}
  ]
}
```

**Milestone Events**:

| Event | When | Data |
|-------|------|------|
| `started` | Shepherd begins orchestration | `issue`, `mode` |
| `phase_entered` | Enters new orchestration phase | `phase` (curator, builder, judge, etc.) |
| `phase_completed` | Phase finishes processing | `phase`, `duration_seconds`, `status` |
| `worktree_created` | Worktree created for issue | `path` |
| `first_commit` | First commit made in worktree | `sha` |
| `pr_created` | Pull request created | `pr_number` |
| `heartbeat` | Periodic during long operations | `action` (description) |
| `completed` | Orchestration finished | `pr_merged` |
| `blocked` | Work is blocked | `reason`, `details` |
| `error` | Encountered an error | `error`, `will_retry` |
| `judge_retry` | Judge phase retry attempted | `attempt`, `max_retries`, `reason` |
| `checkpoint_saved` | Builder checkpoint written | `stage`, `recovery_path`, `details` |
| `checkpoint_loaded` | Resuming from checkpoint | `stage`, `recovery_path`, `skip_stages` |

**Reporting Milestones**:

Shepherds use the `report-milestone.sh` script:

```bash
# Report shepherd started (mode is "default", "force", or "wait")
./.loom/scripts/report-milestone.sh started --task-id abc123 --issue 42 --mode default

# Report phase transition
./.loom/scripts/report-milestone.sh phase_entered --task-id abc123 --phase builder

# Report phase completion
./.loom/scripts/report-milestone.sh phase_completed --task-id abc123 --phase builder --duration-seconds 120 --status success

# Report heartbeat during long operation
./.loom/scripts/report-milestone.sh heartbeat --task-id abc123 --action "running tests"

# Report PR created
./.loom/scripts/report-milestone.sh pr_created --task-id abc123 --pr-number 456

# Report completion
./.loom/scripts/report-milestone.sh completed --task-id abc123 --pr-merged

# Report error (with retry)
./.loom/scripts/report-milestone.sh error --task-id abc123 --error "build failed" --will-retry

# Report judge retry
./.loom/scripts/report-milestone.sh judge_retry --task-id abc123 --attempt 1 --max-retries 3 --reason "no review submitted"
```

**Daemon Snapshot Integration**:

The `loom-tools snapshot` command includes shepherd progress in its output:

```json
{
  "shepherds": {
    "progress": [
      {
        "task_id": "a7dc1e0",
        "issue": 123,
        "current_phase": "builder",
        "last_heartbeat": "2026-01-25T10:15:00Z",
        "status": "working",
        "heartbeat_age_seconds": 45,
        "heartbeat_stale": false
      }
    ],
    "stale_heartbeat_count": 0
  }
}
```

**Stuck Detection with Milestones**:

The `loom-stuck-detection` command uses milestones for more accurate detection:

| Indicator | Threshold | Description |
|-----------|-----------|-------------|
| `stale_heartbeat` | 2 minutes | No heartbeat for extended time |
| `missing_milestone:worktree_created` | 5 minutes | Expected worktree not created |
| `extended_work` | 30 minutes | Same phase for too long |

**Progress File Cleanup**:

Progress files are automatically cleaned up:
- On `shepherd-complete` event (completed issues)
- On `daemon-startup` (stale files from previous session)
- On `periodic` cleanup (files older than 24 hours)

**Session Rotation**:

When a new daemon session starts, the existing `daemon-state.json` is automatically rotated to preserve session history:

```
.loom/
├── daemon-state.json          # Current session (always this name)
├── 00-daemon-state.json       # First archived session
├── 01-daemon-state.json       # Second archived session
├── 02-daemon-state.json       # Third archived session
└── ...
```

**Why session rotation?**
- Debugging patterns across multiple sessions
- Analyzing daemon behavior over time
- Post-mortem analysis when issues occur
- Understanding long-term trends in the development pipeline

**Configuration**:
- `LOOM_MAX_ARCHIVED_SESSIONS` - Maximum sessions to keep (default: 10)

**Commands**:
```bash
# Preview session rotation
./.loom/scripts/rotate-daemon-state.sh --dry-run

# Manually prune old sessions
./.loom/scripts/daemon-cleanup.sh prune-sessions

# Keep more archived sessions
./.loom/scripts/rotate-daemon-state.sh --max-sessions 20
```

Archived sessions include a `session_summary` field with final statistics:
```json
{
  "session_summary": {
    "session_id": 5,
    "archived_at": "2026-01-24T15:30:00Z",
    "issues_completed": 12,
    "prs_merged": 10,
    "total_iterations": 156
  }
}
```

**Required Terminal Configuration for Daemon**:

| Terminal ID | Role | Purpose |
|-------------|------|---------|
| shepherd-1, shepherd-2, shepherd-3 | shepherd.md | Issue orchestration pool |
| terminal-architect | architect.md | Work generation (proposals) |
| terminal-hermit | hermit.md | Simplification proposals |
| terminal-guide | guide.md | Backlog triage (always running) |
| terminal-champion | champion.md | Auto-merge (always running) |
| terminal-doctor | doctor.md | PR conflict resolution (always running) |
| terminal-auditor | auditor.md | Main branch validation (always running) |
| terminal-curator | curator.md | Issue enrichment background role (always running) |

### Custom Roles

Create custom roles by adding files to `.loom/roles/`:

```bash
cat > .loom/roles/my-role.md <<EOF
# My Custom Role
You are a specialist in {{workspace}}.
## Your Role
...
EOF
```

### Branch Rulesets

Loom works best with a GitHub ruleset enabled on the default branch. During installation:

```bash
./scripts/install-loom.sh /path/to/repo  # Interactive, prompts for ruleset
./scripts/install-loom.sh --yes /path/to/repo  # Non-interactive, skip ruleset
```

Manual configuration: `./scripts/install/setup-branch-protection.sh /path/to/repo main`

### Repository Settings

Configure merge settings during installation or manually:

```bash
./scripts/install/setup-repository-settings.sh /path/to/repo
./scripts/install/setup-repository-settings.sh /path/to/repo --dry-run  # Preview
```

Settings applied: squash merge only (no merge commits/rebase), delete branches on merge, auto-merge enabled.

### Multi-Account Token Pool

For environments that rotate among multiple Claude OAuth accounts, Loom can bootstrap a per-account token pool at `.loom/tokens/` from numbered triples in `.env`:

```env
ACCOUNT_EMAIL_1=user1@example.com
ACCOUNT_KEY_1=sk-ant-oat01-...
ACCOUNT_TOKEN_FILE_1=user1.token
```

Run `loom-tokens bootstrap` to materialize the pool:

```bash
loom-tokens bootstrap            # Idempotent — only writes new/missing tokens.
loom-tokens bootstrap --dry-run  # Preview without writing.
loom-tokens bootstrap --force    # Overwrite on-disk tokens that have drifted from .env.
```

Each account becomes `.loom/tokens/<file>.token` (mode `0600`). An `index.json` manifest is written alongside with sha256 fingerprints (8 chars) for drift detection — **no secret material is stored in the manifest**. Numbering gaps are allowed; partial triples are skipped with a warning.

`.loom/tokens/` is gitignored. The pool is consumed by external rotation logic (e.g. a `claude-wrapper.sh` that picks the least-used token); only the bootstrap step is provided here.

#### Account health probe + ranking

Once bootstrapped, `loom-tokens check` probes each account for current rate-limit headers and (optionally) writes a JSON ranking that the spawn-time selector can consume:

```bash
loom-tokens check                  # Probe + print human table
loom-tokens check --ranking        # Probe + write .loom/tokens/.ranking atomically
loom-tokens check --json           # Emit full JSON report to stdout
./.loom/scripts/probe-tokens.sh    # Cron-friendly wrapper for periodic invocation
```

The probe sends a minimal `POST /v1/messages` request (1 input, 1 output token) and parses rate-limit response headers. The header parser matches by **suffix** (`-5h-utilization`, `-7d-utilization`, `-7d-reset`) so future renames of the `anthropic-ratelimit-tokens-*` prefix still work; the full header set is logged on the first probe of each run.

Status assignment: `available` (utilizations < 95%), `exhausted` (`7d_utilization >= 0.95`), `rate_limited` (current 429), `blocked` (401 auth failure or token listed in `.bad_tokens`). Probe failures (network, timeout, 5xx) are logged and skipped — one bad account does not abort the run.

OAuth tokens shaped `sk-ant-oat01-*` are sent with `Authorization: Bearer` + `anthropic-beta: oauth-2025-04-20`; plain API keys use `x-api-key`.

Cron example (probe every 10 minutes):

```cron
*/10 * * * * cd /path/to/repo && ./.loom/scripts/probe-tokens.sh --ranking >> .loom/logs/probe-tokens.log 2>&1
```

See `.loom/docs/troubleshooting.md` for detailed troubleshooting including:
- Cleaning up stale worktrees and branches
- Stuck agent detection and intervention
- Daemon troubleshooting
- Common issues and solutions

**Quick fixes**:

```bash
loom-clean --force                       # Clean stale worktrees/branches
./.loom/scripts/stale-building-check.sh --recover  # Recover stuck issues
./.loom/scripts/recover-orphaned-shepherds.sh --recover  # Recover orphaned shepherds after crash
gh label sync --file .github/labels.yml  # Re-sync labels (GitHub only)
touch .loom/stop-daemon                  # Graceful daemon shutdown
```

## MCP Hooks

Loom provides a unified MCP server (`mcp-loom`) for programmatic control. See the mcp-loom package README for full tool documentation.

**Key tools**: `list_terminals`, `create_terminal`, `send_terminal_input`, `get_agent_metrics`, `trigger_start`, `stop_engine`

**Setup**:
```bash
./scripts/setup-mcp.sh  # Generates .mcp.json
```

**Agent metrics** for self-aware behavior:
```bash
./.loom/scripts/agent-metrics.sh --role builder  # Check your effectiveness
mcp__loom__get_agent_metrics --command summary --period week
```

## Token Rotation (Multi-Account Claude Code)

For Pro/Max plans, Loom supports rotating between multiple Claude Code OAuth tokens. This spreads load across accounts and recovers automatically when a single token hits its weekly limit.

### Setup

1. Add account credentials to `.env` at the workspace root:
   ```env
   ACCOUNT_KEY_1=sk-ant-oat01-...
   ACCOUNT_TOKEN_FILE_1=robb-personal.token
   ACCOUNT_KEY_2=sk-ant-oat01-...
   ACCOUNT_TOKEN_FILE_2=robb-work.token
   ```
2. Run `loom-tokens bootstrap` to materialize per-account `.token` files into `.loom/tokens/` (mode 0600, parent dir 0700). See issue #3234.
3. Spawn agents through `.loom/scripts/spawn-claude.sh` instead of invoking `claude` directly. The wrapper selects a token using a 3-tier algorithm (ranking → allowlist → random), exports `CLAUDE_CODE_OAUTH_TOKEN`, then `exec`s `claude` (or pass `--use-wrapper` to layer on top of `claude-wrapper.sh` for retry behavior).

### Selection algorithm (`loom_tools.tokens.select`)

Three tiers, falling through to the next when the current tier yields nothing:

1. **Ranking** — `.loom/tokens/.ranking` (pipe-delimited `name|status`, refreshed every <10 min). Picks the first non-`exhausted`/non-`blocked` token.
2. **Allowlist** — `.loom/tokens/.allowlist` (one name per line). Random pick from allowed accounts.
3. **Random** — uniform pick from all `*.token` files.

Tokens marked bad in `.loom/tokens/.bad_tokens` are skipped at every tier.

### Bad-token tracking (`loom_tools.tokens.bad_tokens`)

When a token returns `TOKEN_EXPIRED` or `TOKEN_EXHAUSTED`, callers append an entry to `.loom/tokens/.bad_tokens`. Writes are guarded with a `mkdir`-based lock (POSIX-atomic, macOS-compatible — `flock` is **not** used because it isn't available on stock macOS). Reads use word-boundary regex so `agent-1` and `agent-10` don't collide.

### Error classification (`.loom/scripts/lib/classify-error.sh`)

The `classify_error <output> <exit_code>` function returns one of `SUCCESS`, `TIMEOUT`, `CWD_DELETED`, `TOKEN_EXPIRED`, `TOKEN_EXHAUSTED`, `RECOVERABLE`. Critical fix from #3233: exit code is checked **before** output substring matching — clean exits (`exit_code == 0`) always return `SUCCESS` regardless of stdout content. The previous lean-genius implementation returned `RECOVERABLE` for clean exits whose stdout contained substrings like `500` or `rate limit`.

### Worktree handling

When invoked from a worktree, `spawn-claude.sh` resolves the canonical repo root via `git rev-parse --git-common-dir` and locates `.loom/tokens/` there — never in the worktree's path. This avoids each worktree maintaining its own bad-tokens list.

### Hard-fail on missing pool

`spawn-claude.sh` exits `78` (`EX_CONFIG`) with a message instructing the user to run `loom-tokens bootstrap` when `.loom/tokens/` is absent or all tokens are bad. It does **not** silently fall back to keychain — that path belongs in `loom-daemon` (#3236), and only when token rotation has not been configured at all.

### Operator CLI (`loom-tokens pin/unpin/unblock`)

Operators can restrict the rotation pool to a subset of accounts (an "allowlist") and manually un-blacklist accounts marked bad. Auto-recovery prevents pin-induced lockouts.

```bash
loom-tokens pin agent-3 agent-7   # Set allowlist to exactly these
loom-tokens pin add agent-2       # Append (idempotent)
loom-tokens pin remove agent-3    # Remove
loom-tokens pin status            # Show current allowlist
loom-tokens unpin                 # Delete allowlist (back to full pool)

loom-tokens unblock agent-1       # Remove one entry from .bad_tokens
loom-tokens unblock --all         # Clear .bad_tokens entirely
```

**Validation**: `pin` accepts only exact bootstrapped account names — substring/fuzzy matches are rejected. The allowlist is sorted, deduplicated, and `mkdir`-lock guarded so concurrent operator commands don't drop entries.

**Reason-aware bad-token TTL**: bad-tokens entries with reason `auth` (401) ignore `LOOM_TOKENS_BAD_TTL` (default 21600s = 6h) and persist until `loom-tokens unblock`. Other reasons expire automatically.

**Auto-unpin** (`failure_counts`): the wrapper tracks consecutive `TOKEN_EXHAUSTED` failures per account in `.loom/tokens/.failure_counts` (JSON). When **every** account in the allowlist hits the threshold (default 5), the wrapper auto-clears `.allowlist` and `.failure_counts` with a loud stderr log line. Operators can re-pin afterwards. The threshold is `>= 5`, so a 6th failure does not silently exceed; it still triggers (idempotent at-or-above).

Counters are reset on:
- a successful spawn for that account, or
- any operator allowlist mutation (`pin`, `unpin`, `add`, `remove`).

**Empty-pool guard**: if the selector finds the allowlist minus `.bad_tokens` is empty, `spawn-claude.sh` exits `78` (`EX_CONFIG`) with operator instructions. It refuses to silently auto-clear `.bad_tokens` — that masks real auth problems (lean-genius failure mode 3).

### Tests

```bash
PYTHONPATH=loom-tools/src python3 -m pytest loom-tools/tests/tokens/ -v
bash .loom/scripts/tests/test-spawn-claude.sh
```

## Forge Authentication

### GitHub

Loom uses the `gh` CLI for all GitHub operations. By default it uses the credential from `gh auth login`, which has access to all repositories. To scope access to a single repository, create a fine-grained PAT and set `export GH_TOKEN=github_pat_xxx` before running Loom.

See `.loom/docs/github-authentication.md` for the detailed setup guide, required token permissions per role, and troubleshooting.

### Gitea

For Gitea repositories, Loom uses the Gitea API with token authentication. Set `GITEA_TOKEN` or `FORGE_TOKEN` environment variable with an API token created at `<your-gitea-instance>/user/settings/applications`. The token needs repository read/write permissions (issues, pull requests, labels).

See `.loom/docs/forge-authentication.md` for the complete authentication guide covering both GitHub and Gitea.

## Releasing

Use `scripts/version.sh` to manage versions across all packages:

```bash
./scripts/version.sh              # Show current version
./scripts/version.sh check        # Verify all files are in sync
./scripts/version.sh bump patch   # Bump patch (minor, major also supported)
./scripts/version.sh bump patch --tag  # Bump + commit + tag
./scripts/version.sh set 1.0.0 --tag   # Set explicit version + commit + tag
```

**Full release flow** (use `/release` skill for guided process):
```bash
./scripts/version.sh bump patch --tag
git push origin main --tags
gh release create vX.Y.Z --title "vX.Y.Z" --notes "Release notes..."
```

The script updates all 7 version-bearing files (`package.json`, `mcp-loom/package.json`, `src-tauri/tauri.conf.json`, 3 `Cargo.toml` files, `CLAUDE.md`) plus `Cargo.lock`. The GitHub Actions release workflow (`.github/workflows/release.yml`) triggers on GitHub Release creation (`release: types: [created]`), NOT on tag push. You must create a GitHub Release via `gh release create` to trigger the build.

## Resources

- **Main Repository**: https://github.com/rjwalters/loom
- **Role Definitions**: `.loom/roles/*.md`
- **Label Definitions**: `.github/labels.yml`
- **Troubleshooting**: `.loom/docs/troubleshooting.md`
- **Daemon Reference**: `.loom/docs/daemon-reference.md`
- **GitHub Authentication**: `.loom/docs/github-authentication.md`
- **Forge Authentication** (GitHub + Gitea): `.loom/docs/forge-authentication.md`
- **Scripts**: `.loom/scripts/`

---

**Generated by Loom Installation Process**
Last updated: {{INSTALL_DATE}}
