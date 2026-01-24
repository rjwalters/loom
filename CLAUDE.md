# Loom Orchestration - Repository Guide

This repository uses **Loom** for AI-powered development orchestration.

**Loom Version**: {{LOOM_VERSION}}
**Loom Commit**: {{LOOM_COMMIT}}
**Installation Date**: {{INSTALL_DATE}}

## What is Loom?

Loom is a multi-terminal desktop application for macOS that orchestrates AI-powered development workers using git worktrees and GitHub as the coordination layer. It enables both automated orchestration (Tauri App Mode) and manual coordination (Manual Orchestration Mode).

**Loom Repository**: https://github.com/loomhq/loom

## Three-Layer Architecture

Loom uses a three-layer orchestration architecture for scalable automation:

```
┌─────────────────────────────────────────────────────────────────┐
│                    Layer 3: Human Observer                      │
│  - Watches system health and intervenes when needed             │
│  - Approves architectural proposals (loom:architect → loom:issue)│
│  - Handles edge cases and blocked issues                        │
│  - Provides strategic direction                                 │
└─────────────────────────────────────────────────────────────────┘
                              │
                              │ observes/intervenes
                              ▼
┌─────────────────────────────────────────────────────────────────┐
│                    Layer 2: Loom Daemon                         │
│  /loom - Continuous system orchestrator                         │
│  - Monitors system state (issue counts, PR status)              │
│  - Generates work (triggers Architect/Hermit when backlog low)  │
│  - Scales shepherd pool based on demand                         │
│  - Maintains daemon-state.json for crash recovery               │
└─────────────────────────────────────────────────────────────────┘
                              │
                              │ spawns/manages
                              ▼
┌─────────────────────────────────────────────────────────────────┐
│                    Layer 1: Shepherds                           │
│  /shepherd <issue> - Single-issue lifecycle orchestrator        │
│  - Coordinates: Curator → Builder → Judge → Doctor → Merge      │
│  - Handles full lifecycle including code review (Judge phase)   │
│  - Tracks progress in issue comments                            │
└─────────────────────────────────────────────────────────────────┘
                              │
                              │ triggers
                              ▼
┌─────────────────────────────────────────────────────────────────┐
│                    Worker Roles                                 │
│  Curator, Builder, Judge, Doctor, etc.                          │
│  - Execute single tasks (curate issue, build feature, review)   │
│  - Standalone - no knowledge of orchestration                   │
└─────────────────────────────────────────────────────────────────┘
```

### Layer Summary

| Layer | Role | Purpose | Mode |
|-------|------|---------|------|
| Layer 3 | Human | Oversight - approve proposals, handle edge cases | Observer |
| Layer 2 | `/loom` | System orchestration - work generation, shepherd scaling | Continuous daemon |
| Layer 1 | `/shepherd <issue>` | Issue orchestration - lifecycle from creation to merge | Per-issue |
| Layer 0 | `/builder`, `/judge`, etc. | Task execution - single focused work units | Per-task |

### Layer Responsibilities

**Layer 3 (Human Observer)**:
- Approve Architect/Hermit proposals (convert `loom:architect` → `loom:issue`)
- Monitor system health via daemon-state.json
- Intervene for blocked issues or stuck agents
- Provide strategic direction on what to build

**Layer 2 (Loom Daemon)**:
- Automatically maintained by `/loom` - no manual updates needed
- Triggers Architect/Hermit when issue backlog is low
- Spawns shepherds for ready issues (`loom:issue`)
- Tracks state for crash recovery

**Layer 1 (Shepherds)**:
- Fully autonomous once spawned
- Handles entire issue lifecycle including Judge review
- Uses `--force-merge` for autonomous operation or waits for human merge

### When to Use Which Layer

**Use `/shepherd <issue>`** (Layer 1) when:
- You have a specific issue to implement
- You want to orchestrate one issue through its full lifecycle
- Running manual orchestration mode

**Use `/loom`** (Layer 2) when:
- You want fully autonomous development
- The system should generate its own work
- Multiple issues need parallel processing
- Running production-scale orchestration

## Usage Modes

Loom supports two complementary workflows:

### 1. Manual Orchestration Mode (MOM)

Use Claude Code terminals with specialized roles for hands-on development coordination.

**Setup**:
1. Open Claude Code in this repository
2. Use slash commands to assume roles: `/builder`, `/judge`, `/curator`, etc.
3. Each terminal acts as a specialized agent following role guidelines

**When to use MOM**:
- Learning Loom workflows
- Direct control over agent actions
- Debugging and iterating on processes
- Working with smaller teams

**Example workflow**:
```bash
# Terminal 1: Builder working on feature
/builder
# Claims loom:issue issue, implements, creates PR

# Terminal 2: Judge reviewing PRs
/judge
# Reviews PR with loom:review-requested, provides feedback

# Terminal 3: Curator maintaining issues
/curator
# Enhances unlabeled issues, marks as loom:curated
```

### 2. Tauri App Mode

Launch the Loom desktop application for automated orchestration with visual terminal management.

**Setup**:
1. Install Loom app (see main repository for download)
2. Open Loom application
3. Select this repository as workspace
4. Configure terminals with roles and intervals
5. Start engine - terminals launch automatically

**When to use Tauri App**:
- Production-scale development
- Fully autonomous agent workflows
- Visual monitoring of multiple agents
- Hands-off orchestration

**Features**:
- Visual terminal multiplexing
- Real-time agent monitoring
- Autonomous mode with configurable intervals
- Persistent workspace configuration

### 3. Daemon Mode (Layer 2)

Run the Loom daemon for fully autonomous system orchestration.

**Setup**:
```bash
# Start the daemon (runs continuously)
/loom

# The daemon will:
# 1. Monitor system state every 60 seconds
# 2. Trigger Architect/Hermit when backlog is low
# 3. Spawn shepherds for ready issues
# 4. Ensure Guide and Champion keep running
```

**Example daemon workflow**:
```
Daemon Loop:
  ├── Assess: 2 ready issues, 1 building, 0 proposals
  ├── Generate: Trigger Architect (backlog < threshold)
  ├── Scale: Spawn shepherd-1 for issue #123
  ├── Scale: Spawn shepherd-2 for issue #456
  ├── Ensure: Guide running, Champion running
  └── Sleep 60s, repeat

Shepherd-1 (issue #123):
  └── Curator → Builder → Judge → Merge ✓

Shepherd-2 (issue #456):
  └── Curator → Builder → Judge → Doctor → Judge → Merge ✓
```

**Graceful shutdown**:
```bash
# Signal the daemon to stop
touch .loom/stop-daemon

# Daemon will:
# 1. Stop spawning new shepherds
# 2. Wait for active shepherds to complete (max 5 min)
# 3. Clean up state and exit
```

## Agent Roles

Loom provides specialized roles for different development tasks. Each role follows specific guidelines and uses GitHub labels for coordination.

### Orchestration Roles (Layer 1 & 2)

**Loom Daemon** (Autonomous 1min, `loom.md`) - *Layer 2*
- **Purpose**: System-level orchestration and work generation
- **Workflow**: Monitors state → triggers Architect/Hermit → scales shepherds → ensures support roles
- **When to use**: Fully autonomous development with automatic work generation

**Shepherd** (Manual, `shepherd.md`) - *Layer 1*
- **Purpose**: Single-issue lifecycle orchestration
- **Workflow**: Coordinates Curator → Builder → Judge → Doctor → Merge for one issue
- **When to use**: Orchestrating a specific issue through its full development lifecycle

### Worker Roles (Layer 0)

**Builder** (Manual, `builder.md`)
- **Purpose**: Implement features and fixes
- **Workflow**: Claims `loom:issue` → implements → tests → creates PR with `loom:review-requested`
- **When to use**: Feature development, bug fixes, refactoring

**Judge** (Autonomous 5min, `judge.md`)
- **Purpose**: Review pull requests
- **Workflow**: Finds `loom:review-requested` PRs → reviews → approves or requests changes
- **When to use**: Code quality assurance, automated reviews

**Champion** (Autonomous 10min, `champion.md`)
- **Purpose**: Auto-merge approved PRs
- **Workflow**: Finds `loom:pr` PRs → verifies safety criteria → auto-merges if safe
- **When to use**: Manual orchestration mode where humans review before merge
- **Note**: Not needed when shepherds use `--force-merge` (shepherds handle their own merges)

**Curator** (Autonomous 5min, `curator.md`)
- **Purpose**: Enhance and organize issues
- **Workflow**: Finds unlabeled issues → adds context → marks as `loom:curated` (human approves → `loom:issue`)
- **When to use**: Issue backlog maintenance, quality improvement

**Architect** (Autonomous 15min, `architect.md`)
- **Purpose**: Create architectural proposals
- **Workflow**: Analyzes codebase → creates proposal issues with `loom:architect`
- **When to use**: System design, technical decision making

**Hermit** (Autonomous 15min, `hermit.md`)
- **Purpose**: Identify code simplification opportunities
- **Workflow**: Analyzes complexity → creates removal proposals with `loom:hermit`
- **When to use**: Code simplification, reducing technical debt

**Doctor** (Manual, `doctor.md`)
- **Purpose**: Fix bugs and address PR feedback
- **Workflow**: Claims bug reports or addresses PR comments → fixes → pushes changes
- **When to use**: Bug fixes, PR maintenance

**Guide** (Autonomous 15min, `guide.md`)
- **Purpose**: Prioritize and triage issues
- **Workflow**: Reviews issue backlog → updates priorities → organizes labels
- **When to use**: Project planning, issue organization

**Driver** (Manual, `driver.md`)
- **Purpose**: Direct command execution
- **Workflow**: Plain shell environment for custom tasks
- **When to use**: Ad-hoc tasks, debugging, manual operations

### Role Definitions

Full role definitions with detailed guidelines are available in:
- `.loom/roles/loom.md` - Layer 2 daemon orchestration
- `.loom/roles/shepherd.md` - Layer 1 issue orchestration
- `.loom/roles/builder.md` - Feature implementation
- `.loom/roles/judge.md` - Code review
- `.loom/roles/curator.md` - Issue enhancement
- `.loom/roles/doctor.md` - Bug fixes and PR feedback
- `.loom/roles/champion.md` - Auto-merge approved PRs
- `.loom/roles/architect.md` - Architectural proposals
- `.loom/roles/hermit.md` - Code simplification
- `.loom/roles/guide.md` - Issue triage and prioritization

## Label-Based Workflow

Agents coordinate work through GitHub labels. This enables autonomous operation without direct communication.

### Label Flow

**Issue Lifecycle**:
```
(created) → loom:issue → loom:building → (closed)
           ↑ Curator      ↑ Builder

(created) → loom:curating → loom:curated → loom:issue
           ↑ Curator        ↑ Curator      ↑ Human approves

(bug) → loom:treating → (fixed)
       ↑ Doctor
```

**PR Lifecycle**:
```
(created) → loom:review-requested → loom:pr → (auto-merged)
           ↑ Builder                ↑ Judge    ↑ Champion
```

**Proposal Lifecycle**:
```
(created) → loom:architect → (approved) → loom:issue
           ↑ Architect       ↑ Human      ↑ Ready for Builder

(created) → loom:hermit → (approved) → loom:issue
           ↑ Hermit       ↑ Human      ↑ Ready for Builder
```

### Label Definitions

**Workflow Labels**:
- **`loom:issue`**: Issue approved for work, ready for Builder to claim
- **`loom:building`**: Builder is actively implementing this issue
- **`loom:curating`**: Curator is actively enhancing this issue
- **`loom:treating`**: Doctor is actively fixing this bug or addressing PR feedback
- **`loom:review-requested`**: PR ready for Judge to review
- **`loom:changes-requested`**: PR requires changes (Judge requested modifications)
- **`loom:pr`**: PR approved by Judge, ready for Champion to auto-merge

**Proposal Labels**:
- **`loom:architect`**: Architectural proposal awaiting user approval
- **`loom:hermit`**: Simplification proposal awaiting user approval
- **`loom:curated`**: Issue enhanced by Curator, awaiting human approval

**Status Labels**:
- **`loom:blocked`**: Implementation blocked, needs help or clarification
- **`loom:urgent`**: Critical issue requiring immediate attention

## Git Worktree Workflow

Loom uses git worktrees to isolate agent work. Loom supports two types of worktrees depending on the usage mode:

### Worktree Strategy Overview

**Terminal Worktrees** (`.loom/worktrees/terminal-N`):
- **Purpose**: Agent isolation in Tauri App Mode
- **When**: Created automatically for each terminal in the Loom desktop application
- **Why**: Allows multiple autonomous agents to work on different branches simultaneously without conflicts
- **Scope**: Per terminal/agent (persistent across app restarts)
- **Used in**: Tauri App Mode only

**Issue Worktrees** (`.loom/worktrees/issue-N`):
- **Purpose**: Issue-specific work isolation for Builder agents
- **When**: Created manually by Builder when claiming an issue (both MOM and Tauri App)
- **Why**: Isolates work on specific issues with dedicated feature branches
- **Scope**: Per issue (temporary, cleaned up when PR is merged)
- **Used in**: Both Manual Orchestration Mode and Tauri App Mode

### When to Use Which Worktree Type

**Manual Orchestration Mode (Claude Code CLI)**:
- No terminal worktrees (agents work in main workspace initially)
- Builder creates issue worktrees via `./.loom/scripts/worktree.sh <issue-number>`
- Single agent per terminal, human-controlled

**Tauri App Mode (Autonomous Agents)**:
- Automatic terminal worktrees for agent isolation (`.loom/worktrees/terminal-N`)
- Builder ALSO creates issue worktrees when claiming work (`.loom/worktrees/issue-N`)
- Multiple autonomous agents can run simultaneously
- Builder works in issue worktree, not terminal worktree

### Creating Worktrees (for Agents)

When claiming an issue, create a worktree:

```bash
# Agent claims issue #42
gh issue edit 42 --remove-label "loom:issue" --add-label "loom:building"

# Create worktree for issue
./.loom/scripts/worktree.sh 42
# Creates: .loom/worktrees/issue-42
# Branch: feature/issue-42

# Change to worktree
cd .loom/worktrees/issue-42

# Do the work...
# ... implement, test, commit ...

# Push and create PR
git push -u origin feature/issue-42
gh pr create --label "loom:review-requested"
```

### Worktree Best Practices

- **Always use the helper script**: `./.loom/scripts/worktree.sh <issue-number>`
- **Never run git worktree directly**: The helper prevents nested worktrees
- **One worktree per issue**: Keeps work isolated and organized
- **Semantic naming**: Worktrees named `.loom/worktrees/issue-{number}`
- **Clean up when done**: Worktrees are automatically removed when PRs are merged

### Worktree Helper Commands

```bash
# Create worktree for issue
./.loom/scripts/worktree.sh 42

# Check if you're in a worktree
./.loom/scripts/worktree.sh --check

# Show help
./.loom/scripts/worktree.sh --help
```

## Development Workflow

### As a Builder (Manual Mode)

1. **Find ready issue**:
   ```bash
   gh issue list --label="loom:issue"
   ```

2. **Claim issue**:
   ```bash
   gh issue edit 42 --remove-label "loom:issue" --add-label "loom:building"
   ```

3. **Create worktree**:
   ```bash
   ./.loom/scripts/worktree.sh 42
   cd .loom/worktrees/issue-42
   ```

4. **Implement and test**:
   ```bash
   # Make changes...
   # Run tests...
   git add -A
   git commit -m "Implement feature X"
   ```

5. **Create PR**:
   ```bash
   git push -u origin feature/issue-42
   gh pr create --label "loom:review-requested" --body "Closes #42"
   ```

### As a Judge (Autonomous or Manual)

1. **Find PR to review**:
   ```bash
   gh pr list --label="loom:review-requested"
   ```

2. **Review PR**:
   ```bash
   gh pr checkout 123
   # Review code, run tests, check for issues
   ```

3. **Provide feedback**:
   ```bash
   # If changes needed:
   gh pr review 123 --request-changes --body "Feedback here"
   gh pr edit 123 --remove-label "loom:review-requested"

   # If approved:
   gh pr review 123 --approve
   gh pr edit 123 --remove-label "loom:review-requested" --add-label "loom:pr"
   ```

### As a Curator (Autonomous or Manual)

1. **Find unlabeled issues**:
   ```bash
   gh issue list --label="!loom:issue,!loom:building,!loom:architect,!loom:hermit,!loom:curated,!loom:curating"
   ```

2. **Enhance issue**:
   ```bash
   # Add technical details, acceptance criteria, references
   gh issue edit 42 --body "Enhanced description..."
   ```

3. **Mark as curated** (human will approve to add `loom:issue`):
   ```bash
   gh issue edit 42 --add-label "loom:curated"
   ```

## Configuration

### Workspace Configuration

Configuration is stored in `.loom/config.json` (gitignored, local to your machine):

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

### Daemon Configuration (Layer 2)

The Loom daemon uses these configuration parameters:

| Parameter | Default | Description |
|-----------|---------|-------------|
| `ISSUE_THRESHOLD` | 3 | Trigger Architect/Hermit when `loom:issue` count below this |
| `MAX_PROPOSALS` | 5 | Maximum pending proposal issues |
| `MAX_SHEPHERDS` | 3 | Maximum concurrent shepherd processes |
| `ISSUES_PER_SHEPHERD` | 2 | Scale factor: target = ready_issues / ISSUES_PER_SHEPHERD |
| `POLL_INTERVAL` | 60 | Seconds between daemon loop iterations |

**Daemon State File** (`.loom/daemon-state.json`):

```json
{
  "started_at": "2026-01-23T10:00:00Z",
  "last_poll": "2026-01-23T11:30:00Z",
  "running": true,
  "shepherds": {
    "shepherd-1": {
      "issue": 123,
      "started": "2026-01-23T10:15:00Z"
    },
    "shepherd-2": {
      "issue": null,
      "idle_since": "2026-01-23T11:00:00Z"
    }
  },
  "last_architect_trigger": "2026-01-23T10:00:00Z",
  "last_hermit_trigger": "2026-01-23T10:30:00Z"
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

### Custom Roles

Create custom roles by adding files to `.loom/roles/`:

```bash
# Create custom role definition
cat > .loom/roles/my-role.md <<EOF
# My Custom Role

You are a specialist in {{workspace}}.

## Your Role
...
EOF

# Optional: Add metadata
cat > .loom/roles/my-role.json <<EOF
{
  "name": "My Custom Role",
  "description": "Brief description",
  "defaultInterval": 300000,
  "defaultIntervalPrompt": "Continue working",
  "autonomousRecommended": false,
  "suggestedWorkerType": "claude"
}
EOF
```

### Branch Protection

Loom works best with branch protection enabled on your default branch. Branch protection ensures all changes go through the PR workflow and prevents accidental direct commits.

#### During Installation

The installation script optionally configures branch protection:

**Interactive mode**: Prompts you to enable protection
```bash
./scripts/install-loom.sh /path/to/repo
# Will prompt: Configure branch protection rules for 'main' branch? (y/N)
```

**Non-interactive mode**: Skips branch protection (configure manually)
```bash
./scripts/install-loom.sh --yes /path/to/repo
# Skips protection setup for automation safety
```

#### Manual Configuration

Configure branch protection after installation:

```bash
./scripts/install/setup-branch-protection.sh /path/to/repo main
```

Or configure via GitHub Settings:
1. Go to: `Settings > Branches` in your repository
2. Add rule for your default branch (usually `main`)
3. Enable:
   - Require pull request reviews (1 approval)
   - Dismiss stale reviews on new commits
   - Prevent force pushes
   - Prevent branch deletion

#### Protection Rules Applied

The setup script configures these rules:
- ✅ Require pull request before merging
- ✅ Require 1 approval (can be bypassed by admins)
- ✅ Dismiss stale reviews when new commits pushed
- ✅ Prevent force pushes
- ✅ Prevent branch deletion

#### Why Branch Protection?

**Enforces Loom workflow**:
- All changes require pull requests
- PRs require Judge review before merge
- Prevents bypassing label-based coordination
- Maintains audit trail of all changes

**Requirements**:
- Admin permissions on target repository
- GitHub CLI authenticated
- Default branch must exist

**Troubleshooting**:
If setup fails, it's usually due to:
- Lacking admin permissions (ask repo owner)
- Branch doesn't exist yet (push at least one commit)
- GitHub API unreachable (check network/auth)

## Troubleshooting

### Common Issues

**Cleaning Up Stale Worktrees and Branches**:

Use the `clean.sh` helper script to restore your repository to a clean state:

```bash
# Interactive mode - prompts for confirmation (default)
./.loom/scripts/clean.sh

# Preview mode - shows what would be cleaned without making changes
./.loom/scripts/clean.sh --dry-run

# Non-interactive mode - auto-confirms all prompts (for CI/automation)
./.loom/scripts/clean.sh --force

# Deep clean - also removes build artifacts (target/, node_modules/)
./.loom/scripts/clean.sh --deep

# Combine flags
./.loom/scripts/clean.sh --deep --force  # Non-interactive deep clean
./.loom/scripts/clean.sh --deep --dry-run  # Preview deep clean
```

**What clean.sh does**:
- Removes worktrees for closed GitHub issues (prompts per worktree in interactive mode)
- Deletes local feature branches for closed issues
- Cleans up Loom tmux sessions
- (Optional with `--deep`) Removes `target/` and `node_modules/` directories

**IMPORTANT**: For **CI pipelines and automation**, always use `--force` flag to prevent hanging on prompts:
```bash
./.loom/scripts/clean.sh --force  # Non-interactive, safe for automation
```

**Manual cleanup** (if needed):
```bash
# List worktrees
git worktree list

# Remove specific stale worktree
git worktree remove .loom/worktrees/issue-42 --force

# Prune orphaned worktrees
git worktree prune
```

**Labels out of sync**:
```bash
# Re-sync labels from configuration
gh label sync --file .github/labels.yml
```

**Terminal won't start (Tauri App)**:
```bash
# Check daemon logs
tail -f ~/.loom/daemon.log

# Check terminal logs
tail -f /tmp/loom-terminal-1.out
```

**Claude Code not found**:
```bash
# Ensure Claude Code CLI is in PATH
which claude

# Install if missing (see Claude Code documentation)
```

### Stuck Agent Detection

The Loom daemon automatically detects stuck or struggling agents and can trigger interventions.

**Check for stuck agents**:
```bash
# Run stuck detection check
./.loom/scripts/stuck-detection.sh check

# Check with JSON output
./.loom/scripts/stuck-detection.sh check --json

# Check specific agent
./.loom/scripts/stuck-detection.sh check-agent shepherd-1
```

**View stuck detection status**:
```bash
# Show status summary
./.loom/scripts/stuck-detection.sh status

# View intervention history
./.loom/scripts/stuck-detection.sh history
./.loom/scripts/stuck-detection.sh history shepherd-1
```

**Configure stuck detection thresholds**:
```bash
# Adjust thresholds
./.loom/scripts/stuck-detection.sh configure \
  --idle-threshold 900 \
  --working-threshold 2400 \
  --intervention-mode escalate

# Intervention modes: none, alert, suggest, pause, clarify, escalate
```

**Handle stuck agents**:
```bash
# Clear intervention for specific agent
./.loom/scripts/stuck-detection.sh clear shepherd-1

# Clear all interventions
./.loom/scripts/stuck-detection.sh clear all

# Resume a paused agent
./.loom/scripts/signal.sh clear shepherd-1
```

**Stuck indicators**:
| Indicator | Default Threshold | Description |
|-----------|-------------------|-------------|
| `no_progress` | 10 minutes | No output written to task output file |
| `extended_work` | 30 minutes | Working on same issue without creating PR |
| `looping` | 3 occurrences | Repeated similar error patterns |
| `error_spike` | 5 errors | Multiple errors in short period |

**Intervention types**:
| Type | Trigger | Action |
|------|---------|--------|
| `alert` | Low severity | Write to `.loom/interventions/`, human reviews |
| `suggest` | Medium severity | Suggest role switch (e.g., Builder -> Doctor) |
| `pause` | High severity | Auto-pause via signal.sh, requires manual restart |
| `clarify` | Error spike | Suggest requesting clarification from issue author |
| `escalate` | Critical | Full escalation: pause + alert + human notification |

### Daemon Troubleshooting (Layer 2)

**Check daemon state**:
```bash
# View current daemon state
cat .loom/daemon-state.json | jq

# Check if daemon is running
jq '.running' .loom/daemon-state.json

# View active shepherds
jq '.shepherds | to_entries[] | select(.value.issue != null)' .loom/daemon-state.json
```

**Graceful shutdown**:
```bash
# Signal daemon to stop
touch .loom/stop-daemon

# Monitor shutdown progress
watch -n 5 'cat .loom/daemon-state.json | jq ".shepherds"'
```

**Force stop** (use with caution):
```bash
# Remove stop signal if exists
rm -f .loom/stop-daemon

# Clear daemon state (will restart fresh)
rm -f .loom/daemon-state.json
```

**Stuck shepherd**:
```bash
# Check shepherd assignments
jq '.shepherds' .loom/daemon-state.json

# Check if assigned issue is blocked
gh issue view <issue-number> --json labels --jq '.labels[].name'

# Manually clear stuck shepherd (daemon will reassign)
jq '.shepherds["shepherd-1"] = {"issue": null, "idle_since": "'$(date -u +%Y-%m-%dT%H:%M:%SZ)'"}' \
  .loom/daemon-state.json > tmp.json && mv tmp.json .loom/daemon-state.json
```

**Work generation not triggering**:
```bash
# Check issue count vs threshold
echo "Ready issues: $(gh issue list --label 'loom:issue' --state open --json number --jq 'length')"
echo "Threshold: 3 (default)"

# Check cooldown timestamps
jq '.last_architect_trigger, .last_hermit_trigger' .loom/daemon-state.json

# Check proposal count
echo "Proposals: $(gh issue list --label 'loom:architect,loom:hermit' --state open --json number --jq 'length')"
```

## MCP Hooks for Programmatic Control

Loom provides MCP (Model Context Protocol) servers that allow Claude Code to programmatically control the Loom application. This enables automation, testing, and advanced workflows.

### Available MCP Servers

**loom-terminals** - Terminal management via daemon socket:
- `list_terminals` - List all active terminal sessions
- `get_terminal_output` - Get recent output from a terminal
- `get_selected_terminal` - Get info about the currently selected terminal
- `send_terminal_input` - Send input to a terminal
- `create_terminal` - Create a new terminal session
- `delete_terminal` - Delete a terminal session
- `restart_terminal` - Restart a terminal preserving its config
- `configure_terminal` - Update terminal settings (name, role, interval)
- `set_primary_terminal` - Set which terminal is selected in the UI
- `clear_terminal_history` - Clear terminal scrollback and log file
- `check_tmux_server_health` - Check tmux server status
- `get_tmux_server_info` - Get tmux server details
- `toggle_tmux_verbose_logging` - Enable tmux debug logging

**loom-ui** - UI control via file-based IPC:
- `read_console_log` - Read browser console logs
- `read_state_file` - Read workspace state
- `read_config_file` - Read workspace config
- `get_heartbeat` - Check if Loom app is running
- `get_ui_state` - Get comprehensive UI state (terminals, workspace, engine)
- `trigger_start` - Start engine with confirmation dialog
- `trigger_force_start` - Start engine without confirmation
- `trigger_factory_reset` - Reset workspace with confirmation
- `trigger_force_factory_reset` - Reset workspace without confirmation
- `trigger_restart_terminal` - Restart a specific terminal
- `stop_engine` - Stop all terminals and clean up
- `trigger_run_now` - Execute interval prompt immediately
- `get_random_file` - Get random file from workspace

### Example Usage

```bash
# Create a terminal with specific role
mcp__loom-terminals__create_terminal --name "Builder" --role "builder"

# Configure autonomous operation
mcp__loom-terminals__configure_terminal \
  --terminal_id terminal-1 \
  --target_interval 300000 \
  --interval_prompt "Check for new issues"

# Trigger immediate autonomous run
mcp__loom-ui__trigger_run_now --terminalId terminal-1

# Stop all terminals
mcp__loom-ui__stop_engine

# Get comprehensive state
mcp__loom-ui__get_ui_state
```

### MCP Server Configuration

Add these MCP servers to your Claude Code configuration:

```json
{
  "mcpServers": {
    "loom-terminals": {
      "command": "node",
      "args": ["/path/to/loom/mcp-loom-terminals/dist/index.js"],
      "env": {
        "LOOM_WORKSPACE": "/path/to/your/workspace"
      }
    },
    "loom-ui": {
      "command": "node",
      "args": ["/path/to/loom/mcp-loom-ui/dist/index.js"],
      "env": {
        "LOOM_WORKSPACE": "/path/to/your/workspace"
      }
    }
  }
}
```

## Resources

### Loom Documentation

- **Main Repository**: https://github.com/loomhq/loom
- **Getting Started**: https://github.com/loomhq/loom#getting-started
- **Role Definitions**: See `.loom/roles/*.md` in this repository
- **Workflow Details**: See `.loom/AGENTS.md` in this repository

### Local Configuration

- **Configuration**: `.loom/config.json` (your local terminal setup)
- **Role Definitions**: `.loom/roles/*.md` (default and custom roles)
- **Scripts**: `.loom/scripts/` (helper scripts for worktrees, etc.)
- **GitHub Labels**: `.github/labels.yml` (label definitions)

## Support

For issues with Loom itself:
- **GitHub Issues**: https://github.com/loomhq/loom/issues
- **Documentation**: https://github.com/loomhq/loom/blob/main/CLAUDE.md

For issues specific to this repository:
- Use the repository's normal issue tracker
- Tag issues with Loom-related labels when applicable

---

**Generated by Loom Installation Process**
Last updated: {{INSTALL_DATE}}
