# Loom Orchestration - Repository Guide

This repository uses **Loom** for AI-powered development orchestration.

**Loom Version**: {{LOOM_VERSION}}
**Loom Commit**: {{LOOM_COMMIT}}
**Installation Date**: {{INSTALL_DATE}}

## What is Loom?

Loom is a multi-terminal desktop application for macOS that orchestrates AI-powered development workers using git worktrees and GitHub as the coordination layer. It enables both automated orchestration (Tauri App Mode) and manual coordination (Manual Orchestration Mode).

**Loom Repository**: https://github.com/rjwalters/loom

## Installing Loom

To install Loom into a target repository, run from the **Loom source repository**:

```bash
./install.sh /path/to/target-repo
```

**Options**:
- `--yes` or `-y`: Non-interactive mode (skips confirmation prompts)

**Installation Methods**:
The installer offers two methods:
1. **Quick Install** - Fast direct installation using loom-daemon init. Good for personal projects or quick testing.
2. **Full Install** - Creates GitHub issue, uses git worktree, syncs labels, creates PR. Recommended for team projects.

**Getting the Installer**:
Clone the Loom repository: https://github.com/rjwalters/loom

**Advanced Usage** (Full Install workflow only):
For more control, you can use `scripts/install-loom.sh` directly:
```bash
./scripts/install-loom.sh [OPTIONS] /path/to/target-repo
```
Additional options for `install-loom.sh`:
- `--force` or `-f`: Force overwrite existing files and enable auto-merge
- `--clean`: Uninstall first, then fresh install

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
- Override Champion decisions on controversial proposals (Champion handles routine approvals)
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
- Creates PR without waiting by default (stops at ready-to-merge), or use `--merge` for full automation with auto-merge

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

```bash
# Start the daemon (runs continuously)
/loom

# Start with merge mode for aggressive autonomous development
/loom --merge

# The daemon will:
# 1. Monitor system state every 60 seconds
# 2. Trigger Architect/Hermit when backlog is low
# 3. Spawn shepherds for ready issues
# 4. Ensure Guide and Champion keep running
```

**Dual Daemon Prevention**: `/loom` uses session ID tracking to prevent multiple daemon instances from running simultaneously. If a second daemon is started, it will detect the conflict and refuse to start. The `daemon_session_id` field in `daemon-state.json` (format: `timestamp-PID`) enables each daemon to verify it still owns the state file before writing updates. This prevents state corruption when Claude Code sessions are auto-continued.

**Merge Mode** (`--merge`):

When running with `--merge`, the daemon enables aggressive autonomous development:
- Champion auto-promotes all `loom:architect` and `loom:hermit` proposals
- Champion auto-promotes all `loom:curated` issues
- Shepherds auto-approve issues at Gate 1 (skip human approval)
- Shepherds auto-merge PRs at Gate 2 (after Judge approval)
- Audit trail with `[force-mode]` markers on all auto-promoted items
- Safety guardrails still apply (no force-push, respect `loom:blocked`)

**Merge mode does NOT skip code review.** The Judge phase always runs, even in merge mode. This is because GitHub's API prevents self-approval of PRs (`gh pr review --approve` fails when the same user created the PR). Loom's label-based review system (`loom:review-requested` -> `loom:pr`) works around this restriction and functions identically in both normal and merge modes. Merge mode's value is auto-promotion and auto-merge, not review bypass.

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

**Why use the shell wrapper?**
- **Deterministic loop behavior**: No LLM interpretation variability
- **Timeout protection**: Prevents hung iterations (default: 5 minutes)
- **Background operation**: Can run in background, screen, or tmux
- **Consistent logging**: Logs to `.loom/daemon.log`
- **Context isolation**: Each iteration is a fresh Claude session (no context accumulation)

**Trade-offs of shell wrapper**:
- Requires `claude` CLI in PATH
- Slightly higher latency per iteration (CLI startup)
- No conversation context between iterations (by design - this is a feature for long-running operation)

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
- **Purpose**: Evaluate pull requests
- **Workflow**: Finds `loom:review-requested` PRs → evaluates → approves or requests changes
- **When to use**: Code quality assurance, automated evaluations

**Champion** (Autonomous 10min, `champion.md`)
- **Purpose**: Evaluate proposals and auto-merge approved PRs
- **Workflow**: Evaluates `loom:curated`, `loom:architect`, `loom:hermit` proposals → promotes to `loom:issue`. Also finds `loom:pr` PRs → verifies safety criteria → auto-merges if safe
- **When to use**: Default daemon mode - handles both proposal promotion and PR merging
- **Note**: Not needed for PR merging when shepherds run with `--merge` (shepherds handle their own merges)

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

**Auditor** (Autonomous 10min, `auditor.md`)
- **Purpose**: Validate main branch build and runtime
- **Workflow**: Pulls main → builds → tests → runs → creates bug issues if problems found
- **When to use**: Continuous integration health monitoring

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
- `.loom/roles/auditor.md` - Main branch validation

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
(created) → loom:architect → (evaluated) → loom:issue
           ↑ Architect       ↑ Champion    ↑ Ready for Builder

(created) → loom:hermit → (evaluated) → loom:issue
           ↑ Hermit       ↑ Champion    ↑ Ready for Builder

(created) → loom:auditor → (evaluated) → loom:issue
           ↑ Auditor       ↑ Champion    ↑ Ready for Builder
```

**Note**: Champion evaluates proposals from Architect, Hermit, and Auditor roles using the same 8 quality criteria as curated issues. Well-formed proposals are promoted automatically; only ambiguous or controversial proposals require human intervention.

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
- **`loom:architect`**: Architectural proposal awaiting Champion evaluation
- **`loom:hermit`**: Simplification proposal awaiting Champion evaluation
- **`loom:auditor`**: Bug discovered by Auditor during main branch validation
- **`loom:curated`**: Issue enhanced by Curator, awaiting Champion evaluation

**Override Labels**:
- **`loom:auto-merge-ok`**: Override size limit for auto-merge (applied by Judge or human)

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
- **Never delete worktrees manually**: Use `loom-clean` for cleanup (see warning below)
- **One worktree per issue**: Keeps work isolated and organized
- **Semantic naming**: Worktrees named `.loom/worktrees/issue-{number}`
- **Clean up when done**: Worktrees are automatically removed when PRs are merged

**WARNING: Never delete worktrees directly with `git worktree remove`**

Running `git worktree remove` while your shell is in or referencing the worktree directory will corrupt your shell state. Even basic commands like `pwd` will fail with "No such file or directory" errors.

If you need to clean up worktrees:
1. Use `loom-clean` or `loom-clean --force` (handles edge cases safely)
2. For stuck shepherds, use `./.loom/scripts/recover-orphaned-shepherds.sh`
3. Let worktrees auto-cleanup when PRs merge

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
  ],
  "champion": {
    "auto_merge_max_lines": 500
  }
}
```

**Champion Configuration**:

| Parameter | Default | Description |
|-----------|---------|-------------|
| `champion.auto_merge_max_lines` | 200 | Maximum total lines changed (additions + deletions) for auto-merge in normal mode. Set higher to allow larger PRs to auto-merge without resorting to force mode. |

The size limit can also be bypassed per-PR by adding the `loom:auto-merge-ok` label (applied by Judge or human to signal a large PR is safe to auto-merge). In force mode (`--merge`), the size limit is waived entirely.

### Daemon Configuration (Layer 2)

The Loom daemon uses these configuration parameters:

| Parameter | Default | Description |
|-----------|---------|-------------|
| `ISSUE_THRESHOLD` | 3 | Trigger Architect/Hermit when `loom:issue` count below this |
| `MAX_PROPOSALS` | 5 | Maximum pending proposal issues |
| `MAX_SHEPHERDS` | 3 | Maximum concurrent shepherd processes |
| `ISSUES_PER_SHEPHERD` | 2 | Scale factor: target = ready_issues / ISSUES_PER_SHEPHERD |
| `POLL_INTERVAL` | 60 | Seconds between daemon loop iterations |
| `ISSUE_STRATEGY` | fifo | Issue selection strategy (see below) |
| `SHELL_SHEPHERDS` | false | Use shell-based shepherds instead of LLM-based |

**Shell-Based Shepherds** (`LOOM_SHELL_SHEPHERDS`):

When enabled, the daemon uses `loom-shepherd.sh` (Python by default, shell fallback) instead of spawning Claude Code with the `/shepherd` role:

```bash
# Enable shell-based shepherds
LOOM_SHELL_SHEPHERDS=true /loom --merge
```

| Mode | Script | Description |
|------|--------|-------------|
| Python (recommended) | `loom-shepherd.sh` (via Python) | Deterministic orchestration, ~80% token reduction |
| LLM (default) | `/shepherd` role | LLM-interpreted orchestration, more flexible but higher cost |

Script-based shepherds provide:
- **No token accumulation**: Each phase runs in fresh Claude session
- **Deterministic behavior**: Conditionals vs LLM reasoning
- **Configurable polling**: Script sleep vs LLM polling overhead
- **Debuggable**: Read script vs conversation history

**Issue Selection Strategy** (`LOOM_ISSUE_STRATEGY`):

Controls the order in which shepherds pick up issues from the ready queue. The `loom:urgent` label always takes precedence regardless of strategy.

| Strategy | Description |
|----------|-------------|
| `fifo` | **Default.** Oldest issues first (FIFO). Prevents starvation where new issues indefinitely deprioritize older ones. |
| `lifo` | Newest issues first (LIFO). Original GitHub CLI default behavior. |
| `priority` | Same as `fifo` but explicitly named. Issues with `loom:urgent` label first (oldest to newest), then remaining issues oldest to newest. |

**Priority behavior:**
- Issues with `loom:urgent` label are **always** processed first, regardless of strategy
- Within the urgent partition, issues are sorted by age (oldest first)
- Non-urgent issues are then sorted according to the selected strategy

**Example:**
```bash
# Use FIFO (default) - prevents issue starvation
LOOM_ISSUE_STRATEGY=fifo /loom

# Use LIFO - newest issues first (for fast iteration)
LOOM_ISSUE_STRATEGY=lifo /loom

# Priority mode - explicit about urgent-first ordering
LOOM_ISSUE_STRATEGY=priority /loom
```

**Session Reflection Configuration**:

The daemon runs a reflection stage during graceful shutdown to identify improvements and optionally create upstream issues.

```json
{
  "reflection": {
    "enabled": true,              // Enable reflection stage
    "auto_create_issues": false,  // Require user consent
    "min_session_duration": 300,  // Skip for sessions < 5 min
    "upstream_repo": "rjwalters/loom",
    "categories": ["bug", "enhancement", "documentation"]
  }
}
```

| Parameter | Default | Description |
|-----------|---------|-------------|
| `enabled` | true | Enable/disable reflection stage |
| `auto_create_issues` | false | Auto-create issues without prompting |
| `min_session_duration` | 300 | Minimum session duration (seconds) to trigger reflection |
| `upstream_repo` | rjwalters/loom | Repository for improvement issues |

**Manual Reflection**:
```bash
# Run reflection manually (e.g., after crash recovery)
./.loom/scripts/session-reflection.sh

# Preview without creating issues
./.loom/scripts/session-reflection.sh --dry-run

# Output analysis as JSON
./.loom/scripts/session-reflection.sh --json
```

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

**Required Terminal Configuration for Daemon**:

| Terminal ID | Role | Purpose |
|-------------|------|---------|
| shepherd-1, shepherd-2, shepherd-3 | shepherd.md | Issue orchestration pool |
| terminal-architect | architect.md | Work generation (proposals) |
| terminal-hermit | hermit.md | Simplification proposals |
| terminal-guide | guide.md | Backlog triage (always running) |
| terminal-champion | champion.md | Auto-merge (always running) |

### Model Selection Strategy

Loom uses different AI models optimized for each role's task complexity. Model preferences are defined in each role's JSON metadata file via the `suggestedModel` field.

**Model assignments by role**:

| Role | Model | Rationale |
|------|-------|-----------|
| Loom Daemon | `sonnet` | Iteration logic is complex - needs reliable instruction following |
| Shepherd | `sonnet` | Orchestration is systematic with clear state transitions |
| Builder | `opus` | Complex implementation requires deep reasoning |
| Judge | `opus` | Code review needs thorough understanding |
| Curator | `sonnet` | Issue enhancement is structured |
| Doctor | `sonnet` | PR fixes are usually targeted and scoped |
| Architect | `opus` | System design requires sophisticated thinking |
| Hermit | `sonnet` | Code removal analysis is pattern-based |
| Champion | `sonnet` | Proposal evaluation has clear criteria |
| Guide | `sonnet` | Triage is systematic |
| Driver | `sonnet` | General-purpose default |

**Valid model values**: `haiku`, `sonnet`, `opus`

- **haiku**: Fast, cheap - for simple status checks and monitoring
- **sonnet**: Balanced - for structured tasks with clear criteria
- **opus**: Most capable - for complex reasoning and implementation

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
  "suggestedModel": "sonnet",
  "defaultInterval": 300000,
  "defaultIntervalPrompt": "Continue working",
  "autonomousRecommended": false,
  "suggestedWorkerType": "claude"
}
EOF
```

### Branch Rulesets

Loom works best with a GitHub ruleset enabled on your default branch. Rulesets ensure all changes go through the PR workflow and prevent accidental direct commits.

#### During Installation

The installation script optionally configures a branch ruleset:

**Interactive mode**: Prompts you to enable the ruleset
```bash
./scripts/install-loom.sh /path/to/repo
# Will prompt: Configure branch ruleset for 'main' branch? (y/N)
```

**Non-interactive mode**: Skips ruleset setup (configure manually)
```bash
./scripts/install-loom.sh --yes /path/to/repo
# Skips ruleset setup for automation safety
```

#### Manual Configuration

Configure the branch ruleset after installation:

```bash
./scripts/install/setup-branch-protection.sh /path/to/repo main
```

Or configure via GitHub Settings:
1. Go to: `Settings > Rules > Rulesets` in your repository
2. Create a new ruleset targeting the default branch
3. Enable:
   - Prevent branch deletion
   - Prevent force pushes
   - Require linear history (squash merges only)
   - Require pull requests (0 approvals)
   - Dismiss stale reviews on new commits

#### Ruleset Rules Applied

The setup script configures these rules:
- ✅ Prevent branch deletion
- ✅ Prevent force pushes
- ✅ Require linear history (squash merges only)
- ✅ Require pull request before merging (0 approvals)
- ✅ Dismiss stale reviews when new commits pushed

#### Why Rulesets?

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

### Repository Settings

Loom works best with specific repository settings that support the automated workflow. These settings optimize merge behavior and enable auto-merge capabilities.

#### During Installation

The installation script optionally configures repository settings:

**Interactive mode**: Prompts you to configure settings
```bash
./scripts/install-loom.sh /path/to/repo
# Will prompt: Configure repository merge and auto-merge settings? (y/N)
```

**Non-interactive mode**: Skips repository settings (configure manually)
```bash
./scripts/install-loom.sh --yes /path/to/repo
# Skips settings for automation safety
```

#### Manual Configuration

Configure repository settings after installation:

```bash
./scripts/install/setup-repository-settings.sh /path/to/repo
```

Preview changes without applying (dry-run mode):

```bash
./scripts/install/setup-repository-settings.sh /path/to/repo --dry-run
```

Or configure via GitHub Settings:
1. Go to: `Settings > General` in your repository
2. Scroll to "Pull Requests" section
3. Configure merge options as described below

#### Settings Applied

The setup script configures these repository settings:

| Setting | Value | Why |
|---------|-------|-----|
| `allow_merge_commit` | false | Disabled - use squash merge instead |
| `allow_squash_merge` | true | Default merge strategy - flattens PR to single commit |
| `allow_rebase_merge` | false | Disabled - use squash merge instead |
| `delete_branch_on_merge` | true | Auto-cleanup feature branches after merge |
| `allow_auto_merge` | true | Enables Champion role to auto-merge approved PRs |
| `allow_update_branch` | true | Suggests keeping branches up-to-date with base |

#### Why These Settings?

- **Squash merge only**: Flattens each PR into a single commit for clean history; each issue becomes one atomic commit on main
- **Delete branches after merge**: Prevents accumulation of stale branches from issue worktrees
- **Auto-merge enabled**: Required for Champion role to automatically merge approved PRs
- **Suggest updating branches**: Helps agents keep branches current with main

**Requirements**:
- Admin permissions on target repository
- GitHub CLI authenticated

**Troubleshooting**:
If setup fails, it's usually due to:
- Lacking admin permissions (ask repo owner)
- GitHub API unreachable (check network/auth)

## Troubleshooting

### Common Issues

**Merging PRs from worktrees**:

Use `merge-pr.sh` instead of `gh pr merge` to avoid worktree checkout errors:
```bash
./.loom/scripts/merge-pr.sh <PR_NUMBER>
```

This merges via the GitHub API (no local checkout), deletes the remote branch, and optionally cleans up the local worktree with `--cleanup-worktree`. All Loom roles (Shepherd, Champion) use this script automatically.

---

**Cleaning Up Stale Worktrees and Branches**:

Use the `loom-clean` command to restore your repository to a clean state:

```bash
# Interactive mode - prompts for confirmation (default)
loom-clean

# Preview mode - shows what would be cleaned without making changes
loom-clean --dry-run

# Non-interactive mode - auto-confirms all prompts (for CI/automation)
loom-clean --force

# Deep clean - also removes build artifacts (target/, node_modules/)
loom-clean --deep

# Combine flags
loom-clean --deep --force  # Non-interactive deep clean
loom-clean --deep --dry-run  # Preview deep clean
```

**What loom-clean does**:
- Removes worktrees for closed GitHub issues (prompts per worktree in interactive mode)
- Deletes local feature branches for closed issues
- Cleans up Loom tmux sessions
- (Optional with `--deep`) Removes `target/` and `node_modules/` directories

**IMPORTANT**: For **CI pipelines and automation**, always use `--force` flag to prevent hanging on prompts:
```bash
loom-clean --force  # Non-interactive, safe for automation
```

**Manual cleanup** (if needed, but use with caution):

**WARNING**: Running `git worktree remove` while your shell is in the worktree directory will corrupt your shell state. Always ensure you've navigated out of the worktree first, or use `loom-clean` which handles this safely.

```bash
# First, ensure you're NOT in the worktree you're removing
cd /path/to/main/repo

# List worktrees
git worktree list

# Remove specific stale worktree (only after navigating out!)
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

**Orphaned issues stuck in loom:building state**:

When an agent crashes or is cancelled while building, issues can get stuck in `loom:building` state without a PR. Use the stale-building-check script to detect and recover these:

```bash
# Check for stale building issues (dry run)
./.loom/scripts/stale-building-check.sh

# Show detailed progress
./.loom/scripts/stale-building-check.sh --verbose

# Auto-recover stale issues (resets to loom:issue)
./.loom/scripts/stale-building-check.sh --recover

# JSON output for automation
./.loom/scripts/stale-building-check.sh --json
```

**Configuration via environment**:
- `STALE_THRESHOLD_HOURS=2` - Hours before issue without PR is considered stale
- `STALE_WITH_PR_HOURS=24` - Hours before issue with stale PR is flagged

**What it does**:
- Finds issues with `loom:building` label that have been stuck
- Checks if there's an associated PR (by branch name or body reference)
- Issues without PRs older than threshold are flagged/recovered
- Issues with stale PRs are flagged but not auto-recovered (need manual review)

**Orphaned shepherd recovery (daemon crashes)**:

When a daemon session crashes or is terminated abruptly, shepherds may be left in an orphaned state with stale task IDs and inconsistent labels. The `recover-orphaned-shepherds.sh` script handles this:

```bash
# Check for orphaned shepherds (dry run)
./.loom/scripts/recover-orphaned-shepherds.sh

# Show detailed progress
./.loom/scripts/recover-orphaned-shepherds.sh --verbose

# Actually recover orphaned state
./.loom/scripts/recover-orphaned-shepherds.sh --recover

# JSON output for automation
./.loom/scripts/recover-orphaned-shepherds.sh --json
```

**What it detects**:
- Stale task IDs in daemon-state.json (tasks that no longer exist)
- loom:building issues without active shepherds
- Progress files with stale heartbeats (no activity for >5 minutes)
- Mismatches between daemon-state and GitHub labels

**What it recovers**:
- Resets orphaned shepherds to idle state in daemon-state.json
- Returns orphaned issues from `loom:building` to `loom:issue`
- Adds recovery comments to affected issues
- Marks stale progress files as errored

**Automatic recovery on daemon startup**:
The daemon automatically runs orphaned shepherd recovery during startup via `daemon-cleanup.sh daemon-startup`. This ensures the daemon starts with clean state after a crash.

**Configuration via environment**:
- `LOOM_HEARTBEAT_STALE_THRESHOLD=300` - Seconds before heartbeat is stale (default: 5 minutes)

### Stuck Agent Detection

The Loom daemon automatically detects stuck or struggling agents and can trigger interventions.

**Check for stuck agents**:
```bash
# Run stuck detection check
loom-stuck-detection check

# Check with JSON output
loom-stuck-detection check --json

# Check specific agent
loom-stuck-detection check-agent shepherd-1
```

**View stuck detection status**:
```bash
# Show status summary
loom-stuck-detection status

# View intervention history
loom-stuck-detection history
loom-stuck-detection history shepherd-1
```

**Configure stuck detection thresholds**:
```bash
# Adjust thresholds
loom-stuck-detection configure \
  --idle-threshold 900 \
  --working-threshold 2400 \
  --intervention-mode escalate

# Intervention modes: none, alert, suggest, pause, clarify, escalate
```

**Handle stuck agents**:
```bash
# Clear intervention for specific agent
loom-stuck-detection clear shepherd-1

# Clear all interventions
loom-stuck-detection clear all

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

When the pipeline is empty but Architect/Hermit are not being triggered, diagnose with:

```bash
# 1. Check pipeline state via loom-tools snapshot (authoritative source)
python3 -m loom_tools.snapshot | jq '{
  ready: .computed.total_ready,
  needs_work_gen: .computed.needs_work_generation,
  architect_cooldown_ok: .computed.architect_cooldown_ok,
  hermit_cooldown_ok: .computed.hermit_cooldown_ok,
  recommended_actions: .computed.recommended_actions
}'

# Expected output when pipeline empty and work generation should trigger:
# {
#   "ready": 0,
#   "needs_work_gen": true,
#   "architect_cooldown_ok": true,
#   "hermit_cooldown_ok": true,
#   "recommended_actions": ["trigger_architect", "trigger_hermit", "wait"]
# }

# 2. Check if triggers have ever fired
jq '.last_architect_trigger, .last_hermit_trigger' .loom/daemon-state.json

# If both are null with ready=0, work generation never triggered

# 3. Verify proposal counts aren't at max
echo "Architect proposals: $(gh issue list --label 'loom:architect' --state open --json number --jq 'length')"
echo "Hermit proposals: $(gh issue list --label 'loom:hermit' --state open --json number --jq 'length')"
# Max is 2 per role by default

# 4. Force trigger manually (for testing)
# Run daemon iteration with debug mode to see all decisions:
/loom iterate --debug
```

**Common causes:**
- **Cooldown not elapsed**: Default is 30 minutes between triggers. Check `last_*_trigger` timestamps.
- **Proposals at max**: If 2+ architect/hermit proposals exist, new ones won't trigger.
- **Iteration not acting on recommended_actions**: The daemon iteration must explicitly check for `trigger_architect` and `trigger_hermit` in the snapshot's `recommended_actions` array.

## Health Monitoring

Loom provides proactive health monitoring for extended unattended autonomous operation. The health system tracks throughput, latency, error rates, and resource usage to detect degradation patterns before they become critical.

### Health Score

The health score (0-100) is computed from multiple factors:
- **Throughput trend** - Declining throughput reduces score
- **Queue depth trend** - Growing queues reduce score
- **Error rate** - Increasing errors reduce score
- **Resource availability** - Near capacity limits reduce score
- **Stuck agents** - Agents without heartbeats reduce score

Score ranges:
| Range | Status | Description |
|-------|--------|-------------|
| 90-100 | Excellent | System operating optimally |
| 70-89 | Good | Normal operation, minor issues |
| 50-69 | Fair | Some degradation detected |
| 30-49 | Warning | Significant issues, attention needed |
| 0-29 | Critical | Immediate intervention required |

### Health Monitoring CLI

```bash
# View current health status
./.loom/scripts/health-check.sh

# JSON output for automation
./.loom/scripts/health-check.sh --json

# Collect and store metrics (called by daemon)
./.loom/scripts/health-check.sh --collect

# View alerts
./.loom/scripts/health-check.sh --alerts

# Acknowledge an alert
./.loom/scripts/health-check.sh --acknowledge <alert-id>

# View metric history (last 4 hours)
./.loom/scripts/health-check.sh --history 4
```

### Alert Types

| Alert Type | Description | Severity |
|------------|-------------|----------|
| `stuck_agents` | Agents without recent heartbeats | warning/critical |
| `high_error_rate` | Consecutive iteration failures | warning/critical |
| `resource_exhaustion` | Session budget near limits | warning/critical |
| `queue_growth` | Ready queue growing without progress | warning |
| `throughput_decline` | Significant throughput drop | warning |

### Health Files

| File | Purpose |
|------|---------|
| `.loom/health-metrics.json` | Historical health metrics (24-hour retention) |
| `.loom/alerts.json` | Active and acknowledged alerts |

### Configuration

Health monitoring thresholds can be configured via environment variables:

```bash
LOOM_HEALTH_RETENTION_HOURS=24       # Metric retention period
LOOM_THROUGHPUT_DECLINE_THRESHOLD=50 # % decline to trigger alert
LOOM_QUEUE_GROWTH_THRESHOLD=5        # Queue growth count threshold
LOOM_STUCK_AGENT_THRESHOLD=10        # Minutes without heartbeat
LOOM_ERROR_RATE_THRESHOLD=20         # % error rate threshold
```

Or via `.loom/config.json`:

```json
{
  "health_monitoring": {
    "enabled": true,
    "collect_interval_minutes": 5,
    "retention_hours": 24,
    "thresholds": {
      "throughput_decline_percent": 50,
      "queue_growth_count": 5,
      "stuck_agent_minutes": 10,
      "error_rate_percent": 20
    }
  }
}
```

### MCP Health Tools

The following MCP tools are available for health monitoring:

| Tool | Description |
|------|-------------|
| `get_health_metrics` | Get current health score and latest metrics |
| `get_health_history` | Get historical metrics for trend analysis |
| `get_active_alerts` | Get unacknowledged alerts |
| `acknowledge_alert` | Acknowledge an alert by ID |

### UI Health Dashboard

Access the Health Dashboard via:
- Menu: View > Health Dashboard
- Keyboard: Cmd+H (macOS) / Ctrl+H (Windows/Linux)

The dashboard displays:
- Health score gauge with status indicator
- Current metrics grid (throughput, queues, errors, resources)
- Active alerts with acknowledge button
- Historical trends with sparkline visualizations

## MCP Hooks for Programmatic Control

Loom provides a unified MCP (Model Context Protocol) server that allows Claude Code to programmatically control the Loom application. This enables automation, testing, and advanced workflows.

### Unified MCP Server

All Loom MCP tools are provided through a single `mcp-loom` package, which consolidates log monitoring, terminal management, and UI control tools.

**Log Tools:**
- `tail_daemon_log` - Tail daemon log file
- `tail_tauri_log` - Tail Tauri application log
- `list_terminal_logs` - List available terminal output logs
- `tail_terminal_log` - Tail a specific terminal's output log

**Terminal Tools:**
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
- `start_autonomous_mode` - Start autonomous mode for all terminals
- `stop_autonomous_mode` - Stop autonomous mode
- `launch_interval` - Manually trigger interval prompt
- `get_agent_metrics` - Get agent performance metrics for self-aware behavior

**UI Tools:**
- `read_console_log` - Read browser console logs
- `read_state_file` - Read workspace state
- `read_config_file` - Read workspace config
- `get_heartbeat` - Check if Loom app is running
- `get_ui_state` - Get comprehensive UI state (terminals, workspace, engine)
- `trigger_start` - Start engine with confirmation dialog
- `trigger_force_start` - Start engine without confirmation
- `trigger_factory_reset` - Reset workspace with confirmation
- `trigger_force_factory_reset` - Reset workspace without confirmation
- `get_health_metrics` - Get health score and latest metrics
- `get_health_history` - Get historical metrics for trend analysis
- `get_active_alerts` - Get unacknowledged alerts
- `acknowledge_alert` - Acknowledge an alert by ID
- `trigger_restart_terminal` - Restart a specific terminal
- `stop_engine` - Stop all terminals and clean up
- `trigger_run_now` - Execute interval prompt immediately
- `get_random_file` - Get random file from workspace

### Example Usage

```bash
# Create a terminal with specific role
mcp__loom__create_terminal --name "Builder" --role "builder"

# Configure autonomous operation
mcp__loom__configure_terminal \
  --terminal_id terminal-1 \
  --target_interval 300000 \
  --interval_prompt "Check for new issues"

# Trigger immediate autonomous run
mcp__loom__trigger_run_now --terminalId terminal-1

# Stop all terminals
mcp__loom__stop_engine

# Get comprehensive state
mcp__loom__get_ui_state

# View logs
mcp__loom__tail_daemon_log --lines 50
```

### Agent Performance Metrics

Agents can query their own performance metrics to make informed decisions. This enables self-aware behavior where agents can:
- Check if struggling with a task type and escalate
- Select approaches based on historical success rates
- Monitor costs and adjust behavior accordingly

**Via MCP Tool**:
```bash
# Get overall metrics summary
mcp__loom__get_agent_metrics --command summary --period week

# Get effectiveness metrics for a specific role
mcp__loom__get_agent_metrics --command effectiveness --role builder

# Get cost breakdown for a specific issue
mcp__loom__get_agent_metrics --command costs --issue 123

# Get velocity trends
mcp__loom__get_agent_metrics --command velocity
```

**Via CLI Script**:
```bash
# Get my metrics as a builder
./.loom/scripts/agent-metrics.sh --role builder

# Check effectiveness by role
./.loom/scripts/agent-metrics.sh effectiveness

# Get cost for specific issue
./.loom/scripts/agent-metrics.sh costs --issue 123

# JSON output for programmatic use
./.loom/scripts/agent-metrics.sh summary --format json
```

**Metrics Available**:
- **Summary**: Total prompts, tokens, cost, issues worked, PRs created, success rate
- **Effectiveness**: Per-role success rates, average cost, average duration
- **Costs**: Cost per issue, tokens per issue, time spent
- **Velocity**: Issues closed, PRs merged, cycle time trends

**Example Use Case (Agent Escalation)**:
```bash
# Check if success rate is below threshold
success_rate=$(./loom/scripts/agent-metrics.sh --role builder --format json | jq '.success_rate')
if (( $(echo "$success_rate < 70" | bc -l) )); then
    echo "Consider escalating - success rate below threshold"
fi
```

### MCP Server Configuration

Add the unified Loom MCP server to your Claude Code configuration:

```json
{
  "mcpServers": {
    "loom": {
      "command": "node",
      "args": ["/path/to/loom/mcp-loom/dist/index.js"],
      "env": {
        "LOOM_WORKSPACE": "/path/to/your/workspace"
      }
    }
  }
}
```

Or run the setup script to generate `.mcp.json` automatically:

```bash
./scripts/setup-mcp.sh
```

## Resources

### Loom Documentation

- **Main Repository**: https://github.com/rjwalters/loom
- **Getting Started**: https://github.com/rjwalters/loom#getting-started
- **Role Definitions**: See `.loom/roles/*.md` in this repository

### Local Configuration

- **Configuration**: `.loom/config.json` (your local terminal setup)
- **Role Definitions**: `.loom/roles/*.md` (default and custom roles)
- **Scripts**: `.loom/scripts/` (helper scripts for worktrees, etc.)
- **GitHub Labels**: `.github/labels.yml` (label definitions)

## Support

For issues with Loom itself:
- **GitHub Issues**: https://github.com/rjwalters/loom/issues
- **Documentation**: https://github.com/rjwalters/loom/blob/main/CLAUDE.md

For issues specific to this repository:
- Use the repository's normal issue tracker
- Tag issues with Loom-related labels when applicable

---

**Generated by Loom Installation Process**
Last updated: {{INSTALL_DATE}}
