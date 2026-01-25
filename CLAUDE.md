# Loom Orchestration - Repository Guide

This repository uses **Loom** for AI-powered development orchestration.

**Loom Version**: {{LOOM_VERSION}}
**Loom Commit**: {{LOOM_COMMIT}}
**Installation Date**: {{INSTALL_DATE}}

## What is Loom?

Loom is a multi-terminal desktop application for macOS that orchestrates AI-powered development workers using git worktrees and GitHub as the coordination layer. It enables both automated orchestration (Tauri App Mode) and manual coordination (Manual Orchestration Mode).

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
/loom           # Start daemon (runs continuously)
/loom --force   # Aggressive autonomous development
```

**Force Mode** enables: auto-promotion of proposals, audit trail with `[force-mode]` markers, safety guardrails still apply.

**Graceful shutdown**: `touch .loom/stop-daemon`

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
| Judge | `judge.md` | Review pull requests | Autonomous 5min |
| Champion | `champion.md` | Evaluate proposals, auto-merge PRs | Autonomous 10min |
| Curator | `curator.md` | Enhance and organize issues | Autonomous 5min |
| Architect | `architect.md` | Create architectural proposals | Autonomous 15min |
| Hermit | `hermit.md` | Identify simplification opportunities | Autonomous 15min |
| Doctor | `doctor.md` | Fix bugs and address PR feedback | Manual |
| Guide | `guide.md` | Prioritize and triage issues | Autonomous 15min |
| Driver | `driver.md` | Direct command execution | Manual |

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
(created) → loom:architect/loom:hermit → (evaluated) → loom:issue
           ↑ Architect/Hermit            ↑ Champion    ↑ Ready for Builder
```

**Epic Lifecycle**: `loom:epic` → Champion creates phased `loom:architect` + `loom:epic-phase` issues.

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

## Development Workflow

### Builder Workflow

1. Find issue: `gh issue list --label="loom:issue"`
2. Claim: `gh issue edit 42 --remove-label "loom:issue" --add-label "loom:building"`
3. Create worktree: `./.loom/scripts/worktree.sh 42 && cd .loom/worktrees/issue-42`
4. Implement, test, commit
5. Create PR: `git push -u origin feature/issue-42 && gh pr create --label "loom:review-requested" --body "Closes #42"`

### Judge Workflow

1. Find PR: `gh pr list --label="loom:review-requested"`
2. Review: `gh pr checkout 123`
3. Approve: `gh pr review 123 --approve && gh pr edit 123 --remove-label "loom:review-requested" --add-label "loom:pr"`
4. Or request changes: `gh pr review 123 --request-changes --body "Feedback"`

### Curator Workflow

1. Find unlabeled issues: `gh issue list --label="!loom:issue,!loom:building,!loom:architect,!loom:hermit,!loom:curated,!loom:curating"`
2. Enhance issue with technical details
3. Mark curated: `gh issue edit 42 --add-label "loom:curated"`

## Configuration

### Workspace Configuration

Configuration stored in `.loom/config.json` (gitignored):

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
  "mode": "force-pr",
  "started_at": "2026-01-25T10:00:00Z",
  "current_phase": "builder",
  "last_heartbeat": "2026-01-25T10:15:00Z",
  "status": "working",
  "milestones": [
    {"event": "started", "timestamp": "2026-01-25T10:00:00Z", "data": {"issue": 123, "mode": "force-pr"}},
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
| `worktree_created` | Worktree created for issue | `path` |
| `first_commit` | First commit made in worktree | `sha` |
| `pr_created` | Pull request created | `pr_number` |
| `heartbeat` | Periodic during long operations | `action` (description) |
| `completed` | Orchestration finished | `pr_merged` |
| `blocked` | Work is blocked | `reason`, `details` |
| `error` | Encountered an error | `error`, `will_retry` |

**Reporting Milestones**:

Shepherds use the `report-milestone.sh` script:

```bash
# Report shepherd started
./.loom/scripts/report-milestone.sh started --task-id abc123 --issue 42 --mode force-pr

# Report phase transition
./.loom/scripts/report-milestone.sh phase_entered --task-id abc123 --phase builder

# Report heartbeat during long operation
./.loom/scripts/report-milestone.sh heartbeat --task-id abc123 --action "running tests"

# Report PR created
./.loom/scripts/report-milestone.sh pr_created --task-id abc123 --pr-number 456

# Report completion
./.loom/scripts/report-milestone.sh completed --task-id abc123 --pr-merged

# Report error (with retry)
./.loom/scripts/report-milestone.sh error --task-id abc123 --error "build failed" --will-retry
```

**Daemon Snapshot Integration**:

The `daemon-snapshot.sh` script includes shepherd progress in its output:

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

The `stuck-detection.sh` script uses milestones for more accurate detection:

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

### Branch Protection

Loom works best with branch protection enabled. During installation:

```bash
./scripts/install-loom.sh /path/to/repo  # Interactive, prompts for protection
./scripts/install-loom.sh --yes /path/to/repo  # Non-interactive, skip protection
```

Manual configuration: `./scripts/install/setup-branch-protection.sh /path/to/repo main`

### Repository Settings

Configure merge settings during installation or manually:

```bash
./scripts/install/setup-repository-settings.sh /path/to/repo
./scripts/install/setup-repository-settings.sh /path/to/repo --dry-run  # Preview
```

Settings applied: merge commits only (no squash/rebase), delete branches on merge, auto-merge enabled.

## Troubleshooting

See `.loom/docs/troubleshooting.md` for detailed troubleshooting including:
- Cleaning up stale worktrees and branches
- Stuck agent detection and intervention
- Daemon troubleshooting
- Common issues and solutions

**Quick fixes**:

```bash
./.loom/scripts/clean.sh --force        # Clean stale worktrees/branches
./.loom/scripts/stale-building-check.sh --recover  # Recover stuck issues
gh label sync --file .github/labels.yml  # Re-sync labels
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

## Resources

- **Main Repository**: https://github.com/rjwalters/loom
- **Role Definitions**: `.loom/roles/*.md`
- **Label Definitions**: `.github/labels.yml`
- **Troubleshooting**: `.loom/docs/troubleshooting.md`
- **Daemon Reference**: `.loom/docs/daemon-reference.md`
- **Scripts**: `.loom/scripts/`

---

**Generated by Loom Installation Process**
Last updated: {{INSTALL_DATE}}
