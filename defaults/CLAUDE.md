# Loom Orchestration - Repository Guide

This repository uses **Loom** for AI-powered development orchestration.

**Loom Version**: {{LOOM_VERSION}}
**Loom Commit**: {{LOOM_COMMIT}}
**Installation Date**: {{INSTALL_DATE}}

## What is Loom?

Loom is a multi-terminal desktop application for macOS that orchestrates AI-powered development workers using git worktrees and GitHub as the coordination layer. It enables both automated orchestration (Tauri App Mode) and manual coordination (Manual Orchestration Mode).

**Loom Repository**: https://github.com/loomhq/loom

## Three-Layer Architecture

Loom uses a three-layer orchestration architecture:

```
┌─────────────────────────────────────────────────────────────────┐
│                    Layer 3: Human Observer                      │
│  - Approves proposals (loom:architect → loom:issue)             │
│  - Handles edge cases and blocked issues                        │
└─────────────────────────────────────────────────────────────────┘
                              │ observes/intervenes
                              ▼
┌─────────────────────────────────────────────────────────────────┐
│                    Layer 2: Loom Daemon (/loom)                 │
│  - Spawns shepherds for ready issues                            │
│  - Triggers Architect/Hermit when backlog is low                │
│  - Maintains daemon-state.json for crash recovery               │
└─────────────────────────────────────────────────────────────────┘
                              │ spawns/manages
                              ▼
┌─────────────────────────────────────────────────────────────────┐
│                    Layer 1: Shepherds (/shepherd <issue>)       │
│  - Orchestrates full issue lifecycle                            │
│  - Coordinates: Curator → Builder → Judge → Doctor → Merge      │
└─────────────────────────────────────────────────────────────────┘
                              │ triggers
                              ▼
┌─────────────────────────────────────────────────────────────────┐
│                    Layer 0: Worker Roles                        │
│  /builder, /judge, /curator, /doctor, etc.                      │
└─────────────────────────────────────────────────────────────────┘
```

| Layer | Role | Purpose |
|-------|------|---------|
| Layer 3 | Human | Oversight - approve proposals, handle edge cases |
| Layer 2 | `/loom` | System orchestration - work generation, scaling |
| Layer 1 | `/shepherd` | Issue orchestration - full lifecycle |
| Layer 0 | `/builder` etc. | Task execution - single focused work |

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

### 3. Daemon Mode (Fully Autonomous)

Run the Loom daemon for fully autonomous system orchestration.

**Setup**:
```bash
# Start the daemon (runs continuously, or as background process)
/loom

# Or start as background process
/loom start

# Check daemon progress (read-only observer mode)
/loom status

# Stop the daemon gracefully
/loom stop
# Or: touch .loom/stop-daemon
```

**Key Principle: FULLY AUTONOMOUS**

The daemon makes ALL spawning and scaling decisions automatically:
- Shepherds are spawned automatically when `loom:issue` issues exist
- Architect/Hermit are triggered automatically when backlog < threshold
- Guide/Champion are respawned automatically on their intervals
- No human approval needed for ANY of the above

The human observer should NOT:
- Manually spawn shepherds or agents
- Manually trigger Architect/Hermit
- Override daemon scaling decisions
- "Help" by manually running agents

**Human intervention is ONLY required for**:
- Approving proposals: `loom:architect` -> `loom:issue`
- Approving proposals: `loom:hermit` -> `loom:issue`
- Handling `loom:blocked` issues
- Strategic direction changes

**When to use Daemon Mode**:
- Fully autonomous development
- System should generate its own work
- Multiple issues need parallel processing
- Production-scale orchestration

**Example workflow**:
```bash
# Start daemon
/loom start
# Daemon spawns in background, session becomes observer

# Check progress periodically
/loom status

# Or check state file directly
cat .loom/daemon-state.json | jq

# Approve architect proposals (human action)
gh issue edit 1050 --remove-label "loom:architect" --add-label "loom:issue"

# Handle blocked issues (human action)
gh issue view 1045 --comments
gh issue edit 1045 --remove-label "loom:blocked"

# When done
/loom stop
# Or: touch .loom/stop-daemon
```

**Why Autonomous Daemon?**

1. **Clear separation**: Daemon executes, human observes
2. **Fresh context**: Each subagent starts fresh
3. **True autonomy**: Daemon scales without human intervention
4. **No manual decisions**: All spawning is automatic based on thresholds


## Agent Roles

Loom provides specialized roles for different development tasks. Each role follows specific guidelines and uses GitHub labels for coordination.

### Available Roles

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
- **Note**: Not needed when shepherds use `--force-merge` mode

**Curator** (Autonomous 5min, `curator.md`)
- **Purpose**: Enhance and organize issues
- **Workflow**: Finds unlabeled issues → adds context → marks as `loom:curated`
- **When to use**: Issue backlog maintenance, quality improvement
- **Note**: Human approves curated issues (`loom:curated` → `loom:issue`)

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
- `.loom/roles/builder.md`
- `.loom/roles/judge.md`
- `.loom/roles/curator.md`
- And more...

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
(created) → loom:review-requested → loom:pr → (merged)
           ↑ Builder                ↑ Judge    ↑ Human
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
- **`loom:pr`**: PR approved by Judge, ready for human to merge

**Proposal Labels**:
- **`loom:architect`**: Architectural proposal awaiting user approval
- **`loom:hermit`**: Simplification proposal awaiting user approval
- **`loom:curated`**: Issue enhanced by Curator, awaiting human approval

**Status Labels**:
- **`loom:blocked`**: Implementation blocked, needs help or clarification
- **`loom:urgent`**: Critical issue requiring immediate attention

## Git Worktree Workflow

Loom uses git worktrees to isolate agent work. Each issue gets its own worktree.

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
   gh issue list --label="!loom:issue,!loom:building,!loom:curating,!loom:treating,!loom:architect,!loom:hermit"
   ```

2. **Enhance issue**:
   ```bash
   # Add technical details, acceptance criteria, references
   gh issue edit 42 --body "Enhanced description..."
   ```

3. **Mark as ready**:
   ```bash
   gh issue edit 42 --add-label "loom:issue"
   ```

## Agent Performance Metrics

Agents can query their own performance metrics to make informed decisions. This enables self-aware behavior where agents can check their success rates, costs, and velocity.

### Via CLI Script

```bash
# Get overall metrics summary
./.loom/scripts/agent-metrics.sh summary

# Get effectiveness metrics for a specific role
./.loom/scripts/agent-metrics.sh effectiveness --role builder

# Get cost breakdown for a specific issue
./.loom/scripts/agent-metrics.sh costs --issue 123

# Get velocity trends
./.loom/scripts/agent-metrics.sh velocity

# JSON output for programmatic use
./.loom/scripts/agent-metrics.sh summary --format json --period week
```

### Available Metrics

- **Summary**: Total prompts, tokens, cost, issues worked, PRs created, success rate
- **Effectiveness**: Per-role success rates, average cost, average duration
- **Costs**: Cost per issue, tokens per issue, time spent
- **Velocity**: Issues closed, PRs merged, cycle time trends

### Use Cases

**Check if struggling with task type**:
```bash
./.loom/scripts/agent-metrics.sh effectiveness --role builder
# If success rate is low, consider escalating or trying a different approach
```

**Make informed decisions based on historical success**:
```bash
success_rate=$(./.loom/scripts/agent-metrics.sh --role builder --format json | jq '.success_rate')
if (( $(echo "$success_rate < 70" | bc -l) )); then
    echo "Consider escalating - success rate below threshold"
fi
```

### Data Source

Metrics are read from:
- Activity database: `~/.loom/activity.db` (if activity tracking is enabled)
- Daemon state: `.loom/daemon-state.json` (for completed counts)
- GitHub API: Issue/PR counts via `gh` CLI

## Configuration

### Workspace Configuration

Configuration is stored in `.loom/config.json` (gitignored, local to your machine):

```json
{
  "version": "2",
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

**Config Versioning**:

Loom uses an explicit `version` field to enable safe future migrations:

- **Current version**: `"2"` (string literal, not number)
- **Migration**: Happens automatically when loading config files
- **v1 → v2**: Detects configs missing `version` field or with `"version": "1"`
- **Future-proof**: Unknown versions throw clear error prompting user to upgrade Loom

**Example Migration**:
```typescript
// Old config (v1) without version field
{
  "nextAgentNumber": 4,
  "agents": [...]
}

// Automatically migrated to v2 on load
{
  "version": "2",
  "nextAgentNumber": 4,
  "agents": [...]
}

// Config auto-saved with version after migration
```

**Adding Future Versions** (example for v3):

When config schema changes:

1. Add new migration function in `src/lib/config.ts`:
   ```typescript
   function migrateFromV2(raw: unknown): LoomConfig {
     const v2 = raw as V2Config;

     // Transform v2 → v3 (e.g., rename field, add new required field)
     return {
       version: "3",
       terminals: v2.agents.map(transformAgent),
       // ... new fields ...
     };
   }
   ```

2. Update `migrateToLatest()` switch:
   ```typescript
   switch (version) {
     case "1": return migrateFromV2(migrateFromV1(raw));
     case "2": return migrateFromV2(raw);
     case "3": return raw as LoomConfig;
     default: throw new Error(`Unsupported version "${version}"`);
   }
   ```

3. Update version constant and save logic to use `"3"`

**Why Workspace-Specific Config?**
- Each git repo has independent agent numbering and terminal configurations
- Config persists across app restarts
- No parsing of agent names (users can rename freely)
- Stored in workspace, not in app directory
- Role assignments and autonomous settings preserved

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
