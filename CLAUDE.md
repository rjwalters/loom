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
