# Loom Orchestration - Repository Guide

This repository uses **Loom** for AI-powered development orchestration.

**Loom Version**: {{LOOM_VERSION}}
**Installation Date**: {{INSTALL_DATE}}

## What is Loom?

Loom is a CLI + daemon for AI-powered development orchestration. It coordinates AI development workers using git worktrees and a forge (GitHub or Gitea) as the coordination layer. It supports manual coordination (Manual Orchestration Mode) and continuous autonomous orchestration via a minimal spawn loop plus GitHub Actions cron workflows for support roles.

**Loom Repository**: https://github.com/rjwalters/loom

**Supported Forges**: GitHub (full support), Gitea (supported via forge abstraction layer). Forge type is auto-detected from your git remote URL. For Gitea, set `GITEA_TOKEN` or `FORGE_TOKEN` with an API token from your instance's `/user/settings/applications` page.

## Installing Loom

To install Loom into a target repository, run from the **Loom source repository**:

```bash
./install.sh /path/to/target-repo
```

**Options**:
- `--yes` or `-y`: Non-interactive mode (skips confirmation prompts)

**Installation Methods**:
The installer offers two methods:
1. **Quick Install** - Fast direct installation using the Rust `loom-daemon init` command. Good for personal projects or quick testing.
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

## Critical Rules

**Never use `gh pr merge`** — Always use `./.loom/scripts/merge-pr.sh <PR_NUMBER>` instead. The `gh pr merge` command attempts a local checkout which fails in worktrees. The merge script uses the forge API directly. A PreToolUse hook enforces this.

**Forge CLI note** — The `gh` commands shown throughout this document are for GitHub repositories. For Gitea repositories, Loom's scripts handle forge API calls internally; agents do not need to call `gh` directly. The label-based workflow is the same regardless of forge.

**`--permission-mode bypassPermissions` silently disables PreToolUse hooks** — If you invoke Claude Code with `--permission-mode bypassPermissions`, ALL PreToolUse hooks (including `guard-destructive.sh`) are skipped entirely and will not fire. Loom agents use `--dangerously-skip-permissions` instead, which runs Claude in non-interactive mode while still firing hooks. If you have a shell alias like `alias claude="claude --permission-mode bypassPermissions"`, your interactive sessions will have no hook protection. Use `--dangerously-skip-permissions` for automation that requires hooks to run.

## Orchestration Architecture

Loom decomposes development into three coordination tiers, with the forge (GitHub / Gitea) as the shared state:

```
┌─────────────────────────────────────────────────────────────────┐
│                    Tier 3: Human Observer                       │
│  - Watches system health, intervenes on blocked work            │
│  - Approves architectural proposals (loom:architect → loom:issue)│
│  - Handles edge cases and provides strategic direction          │
└─────────────────────────────────────────────────────────────────┘
                              │
                              │ observes/intervenes
                              ▼
┌─────────────────────────────────────────────────────────────────┐
│             Tier 2: Spawn loop + GitHub Actions cron            │
│  ./.loom/scripts/spawn-loop.sh                                  │
│   - Claims ready `loom:issue` items, detaches per-issue sweep   │
│   - Multi-account token rotation per spawn                      │
│  .github/workflows/loom-*.yml                                   │
│   - Cron-driven Champion / Curator / Judge / Auditor / Guide    │
└─────────────────────────────────────────────────────────────────┘
                              │
                              │ spawns/triggers
                              ▼
┌─────────────────────────────────────────────────────────────────┐
│                    Tier 1: /loom:sweep <issue>                  │
│   - Single-issue lifecycle: Curator → Builder → Judge → Doctor  │
│     → Merge                                                     │
│   - Mode C (#3384): PR-set back half (Judge / Doctor → Merge)   │
│   - Checkpoints under .loom/sweep-checkpoint/ for crash resume  │
└─────────────────────────────────────────────────────────────────┘
                              │
                              │ dispatches
                              ▼
┌─────────────────────────────────────────────────────────────────┐
│                    Worker Roles                                 │
│  Curator, Builder, Judge, Doctor, etc.                          │
│  - Execute single tasks (curate issue, build feature, review)   │
│  - Standalone - no knowledge of orchestration                   │
└─────────────────────────────────────────────────────────────────┘
```

### Tier summary

| Tier | Entry point | Purpose | Mode |
|------|-------------|---------|------|
| Tier 3 | Human | Oversight — approve proposals, handle edge cases | Observer |
| Tier 2 | `./.loom/scripts/spawn-loop.sh` + GH Actions cron | Multi-issue batch + scheduled support roles | Continuous / cron |
| Tier 1 | `/loom:sweep <issue>` | Issue lifecycle from creation to merge | Per-issue |
| Tier 0 | `/builder`, `/judge`, etc. | Task execution — single focused work units | Per-task |

### Tier responsibilities

**Tier 3 (Human Observer)**:
- Override Champion decisions on controversial proposals (Champion handles routine approvals)
- Monitor system health via the forge directly + `loom-status`
- Intervene for blocked issues or stuck agents
- Provide strategic direction on what to build

**Tier 2 (Spawn loop + GH Actions cron)**:
- The spawn loop polls `loom:issue` and spawns `/loom:sweep` children, one per ready issue, up to `MAX_PARALLEL` (default 3)
- The GitHub Actions workflows under `.github/workflows/loom-*.yml` run periodic support roles (Champion, Curator, Judge, Auditor, Guide) on cron schedules
- Architect / Hermit cadence (work generation) is currently manual — tracked under follow-up #3381

**Tier 1 (`/loom:sweep`)**:
- Fully autonomous once spawned
- Handles entire issue lifecycle including Judge review
- Checkpoints survive crashes — restarting `/loom:sweep N` resumes from the last completed phase

### When to use which tier

**Use `/loom:sweep <issue>`** (Tier 1) when:
- You have a specific issue to implement
- You want to orchestrate one issue through its full lifecycle
- Running manual orchestration mode

**Use the spawn loop** (`LOOM_USE_SPAWN_LOOP=1 ./.loom/scripts/spawn-loop.sh start`) when:
- You want autonomous multi-issue batch processing
- Multiple ready issues need to be claimed in parallel
- Running production-scale orchestration with multi-account token rotation

## Usage Modes

Loom supports three complementary workflows:

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

### 2. Single-issue lifecycle: `/loom:sweep <issue>`

Run a complete Curator → Builder → Judge → Doctor → Merge lifecycle on one issue from within Claude Code:

```text
/loom:sweep 123
```

Or from a script:

```bash
claude -p "/loom:sweep 123" --dangerously-skip-permissions
```

`/loom:sweep` also supports a **PR-set mode (Mode C, #3384)** that drives Judge / Doctor → Judge / Merge from an existing open-PR set without re-running Curator or Builder:

```text
/loom:sweep --prs 456 789
```

Checkpoints (#3373) under `.loom/sweep-checkpoint/issue-<N>.json` survive crashes — restarting `/loom:sweep N` resumes from the last completed phase.

### 3. Spawn-Loop Mode (Phase 1, opt-in)

A minimal alternative to the full daemon for multi-account `/loom:sweep` launching (#3374, Phase 1 of the shepherd/daemon deprecation epic #3372). Polls `loom:issue`, atomically claims ready issues (label flip + `mkdir`-based file lock under `.loom/locks/issue-<N>/`), and detaches `claude -p "/loom:sweep N"` per issue — each spawn picks its own OAuth token via `spawn-claude.sh`. No work generation, no support-role triggers, no shepherd-N pool-slot bookkeeping.

```bash
# Opt-in gate is required (protects existing daemon users from accidental dual-orchestration)
LOOM_USE_SPAWN_LOOP=1 ./.loom/scripts/spawn-loop.sh start

./.loom/scripts/spawn-loop.sh status
./.loom/scripts/spawn-loop.sh stop          # or: touch .loom/stop-spawn-loop
```

| File | Purpose |
|------|---------|
| `.loom/spawn-loop.pid` | Loop PID (gitignored) |
| `.loom/spawn-loop-state.json` | `{started_at, running:[{issue, pid, started_at, token}]}` (gitignored) |
| `.loom/logs/spawn-loop.log` | Timestamped spawn/exit/error entries |
| `.loom/logs/sweep-issue-<N>.log` | Per-issue child output |
| `.loom/locks/issue-<N>/` | Atomic claim lock dir (gitignored) |
| `.loom/stop-spawn-loop` | Touch to request graceful shutdown |

**Crash recovery**: if a child dies while a `.loom/sweep-checkpoint/issue-<N>.json` exists, the loop flips the issue back to `loom:issue` so the next tick re-spawns; the sweep skill itself (#3373) reads the checkpoint on entry and skips already-completed phases.

**Coexistence with daemon mode**: the `daemon.sh` shell wrapper is **preserved** in v0.10.0 (the Python `loom-daemon` brain it historically called is removed; the shell launcher is re-implemented around the spawn loop and tmux). The spawn loop and `daemon.sh` are two ways to run the same Tier-2 orchestration — the spawn loop is the headless minimal driver, while `daemon.sh` adds a tmux multi-pane container with per-pane OAuth token rotation. If both are running, they will race for `loom:issue` items; pick one in practice.

**Tunables (env)**: `MAX_PARALLEL=3`, `POLL_INTERVAL=30`, `SHUTDOWN_GRACE_SEC=300`, `LOOM_REPO=owner/repo` (override remote auto-detection).

### Scheduled Support Roles (Phase 2a, opt-in)

GitHub Actions workflows under `.github/workflows/loom-*.yml` provide a daemon-free way to run the periodic support roles (Champion, Curator, Judge, Auditor, Guide) on cron schedules that match the daemon's historical intervals (Phase 2a of #3372, see #3375). Each workflow checks out the repo, installs the Claude CLI, and runs `claude -p "/<role>" --dangerously-skip-permissions` for one tick of work — no Loom-side state file, no long-running process.

| Workflow | Role | Schedule (commented) |
|----------|------|----------------------|
| `loom-champion.yml` | `/champion` | `*/10 * * * *` |
| `loom-curator.yml`  | `/curator`  | `*/5 * * * *`  |
| `loom-judge.yml`    | `/judge`    | `*/5 * * * *`  |
| `loom-auditor.yml`  | `/auditor`  | `*/10 * * * *` |
| `loom-guide.yml`    | `/guide`    | `*/15 * * * *` |

**Disabled by default.** Every shipped workflow has its `schedule:` block commented out so forks don't burn Actions minutes accidentally. To opt in on a fork:

1. Add a `CLAUDE_API_KEY` repository secret (Settings -> Secrets and variables -> Actions). Workflows run on a single API key — token rotation is for per-task spawns only; scheduled support roles are predictable load that doesn't benefit from rotation.
2. Uncomment the `schedule:` / `- cron:` lines in each `.github/workflows/loom-*.yml` you want to enable.
3. Optionally trigger a run via `workflow_dispatch` (the Actions UI's "Run workflow" button) to smoke-test before the next scheduled tick.

Architect and Hermit cadence (work-generation triggers) is intentionally out of scope here — see follow-up #3381. Post-v0.10.0, the schedule-driven cron workflows are the recommended path for support roles since the Python daemon brain is removed. Operators who want support roles co-located with the issue-work pane can still run them in tmux via `./.loom/scripts/daemon.sh` (each pane gets its own rotated OAuth token, unlike the GH Actions workflows which use a single API key) — **but note: `./.loom/scripts/daemon.sh` is currently absent on `origin/main` (deleted in #3432, rebuild in flight under epic #3449, ~4-6 weeks); until that lands, use the GH Actions workflows or `./.loom/scripts/spawn-loop.sh`.**

## Agent Roles

Loom provides specialized roles for different development tasks. Each role follows specific guidelines and uses GitHub labels for coordination.

### Worker Roles

**Builder** (Manual, `builder.md`)
- **Purpose**: Implement features and fixes
- **Workflow**: Claims `loom:issue` → implements → tests → creates PR with `loom:review-requested`
- **When to use**: Feature development, bug fixes, refactoring

**Judge** (Cron 5min via GH Actions, `judge.md`)
- **Purpose**: Evaluate pull requests
- **Workflow**: Finds `loom:review-requested` PRs → evaluates → approves or requests changes
- **When to use**: Code quality assurance, automated evaluations

**Champion** (Cron 10min via GH Actions, `champion.md`)
- **Purpose**: Evaluate proposals and auto-merge approved PRs
- **Workflow**: Evaluates `loom:curated`, `loom:architect`, `loom:hermit` proposals → promotes to `loom:issue`. Also finds `loom:pr` PRs → verifies safety criteria → auto-merges if safe
- **When to use**: Default cron mode — handles both proposal promotion and PR merging
- **Note**: `/loom:sweep` Mode C (PR-set) can also merge from its own session; Champion's cron is the standing safety net for PRs not picked up by an interactive sweep.

**Curator** (Cron 5min via GH Actions, `curator.md`)
- **Purpose**: Enhance and organize issues
- **Workflow**: Finds unlabeled issues → adds context → marks as `loom:curated` (human approves → `loom:issue`)
- **When to use**: Issue backlog maintenance, quality improvement

**Architect** (Manual, `architect.md`)
- **Purpose**: Create architectural proposals
- **Workflow**: Analyzes codebase → creates proposal issues with `loom:architect`
- **When to use**: System design, technical decision making
- **Cadence**: Manual today; automated scheduling is tracked under follow-up #3381.

**Hermit** (Manual, `hermit.md`)
- **Purpose**: Identify code simplification opportunities
- **Workflow**: Analyzes complexity → creates removal proposals with `loom:hermit`
- **When to use**: Code simplification, reducing technical debt
- **Cadence**: Manual today; automated scheduling is tracked under follow-up #3381.

**Doctor** (Manual, `doctor.md`)
- **Purpose**: Fix bugs and address PR feedback
- **Workflow**: Claims bug reports or addresses PR comments → fixes → pushes changes
- **When to use**: Bug fixes, PR maintenance

**Guide** (Cron 15min via GH Actions, `guide.md`)
- **Purpose**: Prioritize and triage issues
- **Workflow**: Reviews issue backlog → updates priorities → organizes labels
- **When to use**: Project planning, issue organization

**Driver** (Manual, `driver.md`)
- **Purpose**: Direct command execution
- **Workflow**: Plain shell environment for custom tasks
- **When to use**: Ad-hoc tasks, debugging, manual operations

**Auditor** (Cron 10min via GH Actions, `auditor.md`)
- **Purpose**: Validate main branch build and runtime
- **Workflow**: Pulls main → builds → tests → runs → creates bug issues if problems found
- **When to use**: Continuous integration health monitoring

### Role Definitions

Full role definitions with detailed guidelines are available in:
- `.loom/roles/builder.md` - Feature implementation
- `.loom/roles/judge.md` - Code review
- `.loom/roles/curator.md` - Issue enhancement
- `.loom/roles/doctor.md` - Bug fixes and PR feedback
- `.loom/roles/champion.md` - Auto-merge approved PRs
- `.loom/roles/architect.md` - Architectural proposals
- `.loom/roles/hermit.md` - Code simplification
- `.loom/roles/guide.md` - Issue triage and prioritization
- `.loom/roles/auditor.md` - Main branch validation

> **Stop-gap — daemon backend in flight (v0.10.0 rebuild, epic #3449)**
>
> `./.loom/scripts/daemon.sh` does not exist on `origin/main` as of v0.9.1; the dispatcher was deleted in #3432 and is being rebuilt in epic #3449 (~4-6 weeks, scheduled for v0.10.0). The note below describes the intended target state. Until Phase A through E of #3449 land, daemon operator commands (`./.loom/scripts/daemon.sh start|stop|status`) will fail with "no such file or directory". Use `./.loom/scripts/spawn-loop.sh` for headless multi-issue dispatch in the interim. Tracker: #3451 (this stop-gap), #3449 (rebuild epic).

> **Note**: the historical `shepherd.md` (single-issue orchestrator) role file was removed in v0.10.0 along with the `/shepherd` slash command — see [the migration guide](../docs/migration/v0.10.0-shepherd-deprecation.md). Its orchestration responsibilities moved to `/loom:sweep` (Tier 1) and the spawn loop + GH Actions cron (Tier 2). The `loom.md` role file is preserved and documents the daemon-mode operator surface (`./.loom/scripts/daemon.sh` + tmux + token-rotated separate Claude Code sessions — see stop-gap warning above re: in-flight rebuild #3449); the Python brain it historically referenced (`loom_tools/daemon_v2/`) is removed in v0.10.0, but the shell-level daemon surface stays. The worker-role markdown files above are unchanged.

## Label-Based Workflow

Agents coordinate work through forge labels (GitHub or Gitea). This enables autonomous operation without direct communication.

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

Loom uses git worktrees to isolate agent work on issues.

### Worktree Strategy Overview

**Issue Worktrees** (`.loom/worktrees/issue-N`):
- **Purpose**: Issue-specific work isolation for Builder agents
- **When**: Created by Builder when claiming an issue
- **Why**: Isolates work on specific issues with dedicated feature branches
- **Scope**: Per issue (temporary, cleaned up when PR is merged)

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

### Worktree Ownership Model

Loom distinguishes between worktrees it created and worktrees the user created. Cleanup tooling only acts on Loom-managed worktrees.

- **Loom-managed**: Worktrees created by `./.loom/scripts/worktree.sh`. These live under `.loom/worktrees/` and contain a `.loom-managed` sentinel file in their root. `merge-pr.sh`, `agent-destroy.sh`, and `loom-clean` may remove them automatically.
- **User-managed**: Any other worktree — anything not under `.loom/worktrees/`, or anything under `.loom/worktrees/` that lacks the `.loom-managed` sentinel. Loom tooling and Loom-aware agents MUST NOT remove these. They survive merges, agent shutdowns, and `loom-clean` runs.

To disable all Loom-side worktree removal for a session (e.g., when running Loom inline from an editor-provisioned worktree), set `LOOM_PRESERVE_WORKTREE=1`. Both `merge-pr.sh` and `agent-destroy.sh` honor this flag.

### Worktree Best Practices

- **Always use the helper script**: `./.loom/scripts/worktree.sh <issue-number>` (it writes the `.loom-managed` sentinel automatically)
- **Never run git worktree directly**: The helper prevents nested worktrees
- **Never delete worktrees manually**: Use `loom-clean` for cleanup (see warning below)
- **One worktree per issue**: Keeps work isolated and organized
- **Semantic naming**: Worktrees named `.loom/worktrees/issue-{number}`
- **Clean up when done**: Loom-managed worktrees are automatically removed when their PR merges. User-provisioned worktrees are never touched.

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

# Cone-mode sparse checkout - materialize only listed paths + safety set
# Useful in large monorepos to keep per-worktree disk usage small
./.loom/scripts/worktree.sh 42 --sparse src/lib defaults/scripts

# Convert a sparse worktree back to a full checkout
./.loom/scripts/worktree.sh 42 --full

# Check if you're in a worktree
./.loom/scripts/worktree.sh --check

# Show help
./.loom/scripts/worktree.sh --help
```

**Sparse-Mode Notes**:
- `--sparse` and `--full` are mutually exclusive
- `--sparse` requires at least one path
- Always-included safety set: `.claude/`, `.loom/`, `.githooks/`, `scripts/`,
  plus all tracked top-level files (extend via `LOOM_WORKTREE_ALWAYS_INCLUDE`)
- Sparse-checkout config is written to the per-worktree config only, never to
  shared `.git/config` (prevents the stale-config trap from breaking
  `actions/checkout` on self-hosted runners)
- Re-running `--sparse` is idempotent: same cone is a no-op, different cone
  replaces the cone; `--full` on an already-full worktree is also a no-op

## Development Workflow

### Sweep Lifecycle (MANDATORY)

When implementing issues — whether manually, via `/loom:sweep`, or by spawning subagents — **all stages of the lifecycle must be executed in order**. Do not skip stages.

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

**When spawning subagents to handle an issue**: each subagent must run the full lifecycle, not just the builder phase. If parallelizing multiple issues, each agent must independently execute Curator → Builder → Judge → Doctor → Merge. Simply creating a PR and labeling it `loom:review-requested` is only the Builder stage — the work is not complete until the PR has been reviewed and merged.

**When using `/loom:sweep`**: the skill handles all stages automatically. Prefer `/loom:sweep <issue>` over manual orchestration to avoid accidentally skipping stages.

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

3. **Provide feedback** (use comments + labels, not `gh pr review` which fails with self-review restriction):
   ```bash
   # If changes needed:
   gh pr comment 123 --body "Changes needed: ..." && \
     gh pr edit 123 --remove-label "loom:review-requested" --add-label "loom:changes-requested"

   # If approved:
   gh pr comment 123 --body "LGTM! Approved." && \
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

Configuration is stored in `.loom/config.json` (committed to git for team sharing):

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

**Post-Builder Quality Gate (`buildGate`)**:

An optional deterministic gate runs after the builder agent exits but before PR creation. When any of three checks fails (has-commits, has-real-changes, build-passes), the orchestrator releases the issue claim and no PR is opened. This is opt-in via `.loom/config.json`:

```json
{
  "buildGate": {
    "enabled": true,
    "command": "cargo build --workspace",
    "realChangeGlobs": ["*.rs", "*.toml", "Cargo.lock"],
    "timeoutSeconds": 600
  }
}
```

Repos without a `buildGate` block see zero behavior change. See `.loom/docs/build-gate.md` for the full schema and failure semantics.

### Spawn-Loop Configuration (Tier 2)

> **Stop-gap — daemon backend in flight (v0.10.0 rebuild, epic #3449)**
>
> The paragraph below claims `./.loom/scripts/daemon.sh` "now wraps the spawn loop with tmux + per-pane token rotation". On `origin/main` as of v0.9.1, that file does not exist (deleted in #3432, rebuild in flight under epic #3449, ~4-6 weeks). Until the rebuild lands, the only working multi-issue dispatch backend is `./.loom/scripts/spawn-loop.sh` (headless) or the GitHub Actions cron workflows.

The spawn loop replaces the historical Python daemon brain. Its surface is intentionally narrow — there are no work-generation triggers, no pool-slot bookkeeping, and no state-file-based pipeline tracking. The shell-level daemon surface (`./.loom/scripts/daemon.sh`) is preserved and now wraps the spawn loop with tmux + per-pane token rotation. See [`.loom/docs/daemon-reference.md`](.loom/docs/daemon-reference.md) and [the migration guide](../docs/migration/v0.10.0-shepherd-deprecation.md) for details.

**Tunables (env)**:

| Variable | Default | Description |
|----------|---------|-------------|
| `MAX_PARALLEL` | 3 | Maximum concurrent `/loom:sweep` children |
| `POLL_INTERVAL` | 30 | Seconds between `loom:issue` polls |
| `SHUTDOWN_GRACE_SEC` | 300 | Seconds to wait for in-flight children at shutdown |
| `LOOM_USE_SPAWN_LOOP` | (unset) | Opt-in gate, required to start the loop |
| `LOOM_REPO` | (auto) | Override remote auto-detection (`owner/repo`) |

**State file** (`.loom/spawn-loop-state.json`, gitignored):

```json
{
  "started_at": "2026-06-04T10:00:00Z",
  "running": [
    {
      "issue": 123,
      "pid": 49281,
      "started_at": "2026-06-04T10:15:00Z",
      "token": "agent-3.token"
    }
  ]
}
```

That's the entire schema. Pipeline state, warnings, completed-issue history, and work-generation cooldowns are not tracked — the forge is the source of truth for queue state, and the spawn loop is intentionally minimal.

**Issue selection** is FIFO within the `loom:issue` queue, with the `loom:urgent` label taking precedence (urgent-first, then oldest-first). The pre-v0.10.0 `LOOM_ISSUE_STRATEGY` env var (with `lifo` / `priority` alternatives) no longer applies; the strategy lives inside the spawn loop and is not currently configurable.

**Sweep checkpoints** (`.loom/sweep-checkpoint/issue-<N>.json`, gitignored) — the per-issue checkpoint format is owned by the sweep skill (#3373). When a sweep child crashes, the spawn loop flips the issue back to `loom:issue` so the next tick re-spawns; the sweep skill itself reads the checkpoint on entry and skips already-completed phases.

**Scheduled support roles** run as separate GitHub Actions cron jobs under `.github/workflows/loom-*.yml`. They have no persistent state on the Loom side; each tick is a fresh `claude -p "/<role>" --dangerously-skip-permissions` invocation.

### Model Selection Strategy

Loom subagents inherit the model of the parent conversation. Agent definitions do not specify a `model:` field in their frontmatter, so the model used depends on how the parent session was launched. This avoids per-model quota bucket exhaustion where a hardcoded model assignment causes rate limit failures even when other model quotas are available.

Each role ships JSON metadata with a `suggestedModel` field that records the historically-validated model for that role. The metadata is informational only — Claude Code subagent routing still inherits from the parent conversation.

**Suggested models by role**:

| Role | Model | Rationale |
|------|-------|-----------|
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

## Custom Guard Hooks

Loom ships with built-in guard hooks (`guard-destructive.sh` for dangerous Bash commands). You can add project-specific guards to protect read-only directories from accidental edits.

### Protecting Read-Only Directories

Many projects have directories that should never be modified by agents (vendor code, generated files, external SDKs, process design kits). Loom provides a template hook for this.

**Setup**:

1. Copy the template to your hooks directory:
   ```bash
   cp defaults/hooks/guard-readonly-dirs.sh.template .loom/hooks/guard-readonly-dirs.sh
   chmod +x .loom/hooks/guard-readonly-dirs.sh
   ```

2. Edit `.loom/hooks/guard-readonly-dirs.sh` and add your protected directories:
   ```bash
   PROTECTED_DIRS=(
       "vendor/"
       "third_party/"
       "generated/"
   )
   ```

3. Register the hook in `.claude/settings.json`:
   ```json
   {
     "hooks": {
       "PreToolUse": [
         {
           "matcher": "Edit|Write",
           "hooks": [{ "type": "command", "command": ".loom/hooks/guard-readonly-dirs.sh" }]
         }
       ]
     }
   }
   ```

**How it works**: The hook intercepts Edit and Write tool calls, resolves the target file path to an absolute path, and checks whether it falls within any of the listed directories (relative to the repository root). If it does, the edit is blocked with a clear error message. The hook follows the same error-handling patterns as `guard-destructive.sh` (ERR trap, jq fallback, never exits non-zero).

**Interaction with other hooks**: This hook uses the `Edit|Write` matcher, while `guard-destructive.sh` uses the `Bash` matcher, so they do not conflict. If `guard-worktree-paths.sh` is also active (same `Edit|Write` matcher), both hooks run in sequence -- if either denies, the action is blocked.

**Template location**: `defaults/hooks/guard-readonly-dirs.sh.template`

## Methodology Injection Framework

Loom provides an opt-in methodology injection hook that automatically injects project-specific context into every agent session. This is useful for domain knowledge, coding conventions, design rules, or any context that agents need to do their job well.

### Quick Start

1. Create the context directory in your repository:
   ```bash
   mkdir -p .loom/context/roles .loom/context/topics
   ```

2. Add a universal context file (injected on every prompt):
   ```bash
   cat > .loom/context/universal.md << 'EOF'
   # Project Rules
   - Use TypeScript strict mode
   - All functions must have JSDoc comments
   - Run tests before creating PRs
   EOF
   ```

3. The hook is already registered in `.claude/settings.json`. It activates automatically when `.loom/context/` exists and silently does nothing when the directory is absent.

### Context File Structure

```
.loom/context/
├── config.json              # Optional configuration
├── universal.md             # Injected on every prompt
├── roles/
│   ├── builder.md           # Injected when LOOM_ROLE=builder
│   ├── judge.md             # Injected when LOOM_ROLE=judge
│   └── ...
└── topics/
    ├── security.md          # Injected when prompt matches "security"
    ├── security.pattern     # Optional: custom regex pattern for matching
    ├── database.md          # Injected when prompt matches "database"
    └── ...
```

**Universal context** (`universal.md`): Always injected when the context directory exists. Use for project-wide rules and conventions.

**Role context** (`roles/<role>.md`): Injected when the `LOOM_ROLE` environment variable matches the filename, or when a slash command (e.g., `/builder`) is detected in the prompt. Role names are case-insensitive.

**Topic context** (`topics/<name>.md`): Injected when the prompt matches a keyword pattern. By default, the filename is used as the regex pattern (e.g., `security.md` matches prompts containing "security"). For custom patterns, create a sidecar `.pattern` file with a regex (e.g., `security.pattern` containing `security|auth|token|credential`).

### Configuration

Create `.loom/context/config.json` to customize behavior:

```json
{
  "max_context_chars": 8000,
  "enabled": true,
  "inject_universal": true,
  "inject_role": true,
  "inject_topics": true
}
```

| Parameter | Default | Description |
|-----------|---------|-------------|
| `max_context_chars` | 8000 | Maximum total characters injected (prevents overwhelming the context window) |
| `enabled` | true | Set to false to disable injection without removing files |
| `inject_universal` | true | Whether to inject `universal.md` |
| `inject_role` | true | Whether to inject role-specific context |
| `inject_topics` | true | Whether to inject topic-matched context |

### How It Works

The `methodology-inject.sh` hook runs as a `UserPromptSubmit` hook alongside `skill-router.sh`. On each prompt:

1. Checks for `.loom/context/` directory -- exits silently if absent
2. Reads `universal.md` if present
3. Detects the active role via `LOOM_ROLE` env var or prompt slash command
4. Scans `topics/` files, matching prompt against filename or sidecar `.pattern` regex
5. Concatenates matching content, capped at `max_context_chars`
6. Returns the collected context as `additionalContext`

The hook follows the same error-handling patterns as other Loom hooks: it never exits non-zero, logs errors to `.loom/logs/hook-errors.log`, and fails silently on any unexpected error.

### Example Context Files

Example context files are provided in `defaults/hooks/example-context/` to guide setup. Copy them to your `.loom/context/` directory and customize:

```bash
cp -r defaults/hooks/example-context/* .loom/context/
```

## Troubleshooting


### Common Issues

**Overnight / long-running orchestration: keep the host awake (#3350)**:

`/loom:sweep` and the spawn loop automatically run `./.loom/scripts/check-host-sleep.sh` at startup and warn when the host can sleep. This is **advisory only** — Loom never blocks on it. Heed the warning before walking away from a long run.

- **macOS:** user-idle sleep assertions (Amphetamine, `caffeinate -dimsu`, etc.) do **not** reliably defeat Maintenance Sleep on Apple Silicon. Use `sudo pmset -c sleep 0` for AC-only sleep disable, or flip your sleep manager's "allow system sleep when display is off" toggle to OFF. Restore with `sudo pmset -c sleep 1` afterwards.
- **systemd Linux:** wrap the session in `systemd-inhibit --what=idle:sleep --who=loom --why=loom -- <cmd>`. This is reliable.

If you want to invoke the check manually:

```bash
./.loom/scripts/check-host-sleep.sh         # full warning (or success line)
./.loom/scripts/check-host-sleep.sh --quiet # stderr warning only, no stdout line
```

**Merging PRs from worktrees**:

Use `merge-pr.sh` instead of `gh pr merge` to avoid worktree checkout errors:
```bash
./.loom/scripts/merge-pr.sh <PR_NUMBER>
```

This merges via the GitHub API (no local checkout), deletes the remote branch, and cleans up the local worktree by default. Pass `--no-cleanup-worktree` to skip worktree cleanup (e.g., when other terminals may have their CWD inside the worktree). All Loom flows (`/loom:sweep`, Champion's auto-merge step) use this script automatically.

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
# Re-sync labels from configuration (GitHub)
gh label sync --file .github/labels.yml

# Or use the Loom sync script (works with both GitHub and Gitea)
./scripts/install/sync-labels.sh .
```

**Daemon won't start**:
```bash
# Check daemon logs
tail -f ~/.loom/daemon.log
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

**Orphaned task recovery (spawn-loop crashes)**:

When the spawn loop crashes or is terminated abruptly, sweep children may be left with stale `.loom/locks/issue-<N>/` lock directories or stale entries in `.loom/spawn-loop-state.json`. `loom-orphan-recovery` (the ported successor to the historical `recover-orphaned-shepherds.sh`) handles this:

```bash
# Check for orphaned tasks (dry run)
loom-orphan-recovery

# Actually recover orphaned state
loom-orphan-recovery --recover

# JSON output for automation
loom-orphan-recovery --json
```

**What it detects** (post-v0.10.0):
- Stale pids in `.loom/spawn-loop-state.json` (tasks whose process no longer exists)
- `loom:building` issues with no live task in the spawn loop
- Stale lock dirs at `.loom/locks/issue-<N>/` for closed/merged issues

**What it recovers**:
- Removes stale entries from `.loom/spawn-loop-state.json`
- Returns orphaned issues from `loom:building` to `loom:issue`
- Adds recovery comments to affected issues
- Removes stale lock dirs

**Automatic recovery on spawn-loop startup**:
The spawn loop runs orphan recovery at startup. This ensures it starts with a clean claim set after a crash.

### Stuck Agent Detection

`loom-stuck-detection` checks for stuck sweep children using `.loom/spawn-loop-state.json` task pids and `.loom/sweep-checkpoint/issue-<N>.json` checkpoint timestamps.

**Check for stuck agents**:
```bash
# Run stuck detection check
loom-stuck-detection check

# Check with JSON output
loom-stuck-detection check --json

# Check a specific issue
loom-stuck-detection check-issue 123
```

**Stuck indicators** (post-v0.10.0):

| Indicator | Default Threshold | Description |
|-----------|-------------------|-------------|
| `stale_heartbeat` | 5 minutes | No checkpoint update for extended time |
| `dead_pid` | (instant) | PID in spawn-loop-state.json is no longer alive |
| `error_spike` | 5 errors | Multiple errors in `.loom/logs/sweep-issue-N.log` |

The pre-v0.10.0 indicators `no_progress`, `extended_work`, and `looping` are no longer tractable post-deletion of `.loom/progress/` — see [the migration guide § Per-CLI breaking changes](../docs/migration/v0.10.0-shepherd-deprecation.md#per-cli-breaking-changes) for the diff.

### Spawn-Loop Troubleshooting

**Check spawn-loop state**:
```bash
# Status summary (human-readable)
./.loom/scripts/spawn-loop.sh status

# Or read the state file directly
cat .loom/spawn-loop-state.json | jq

# Check if loop is running
test -f .loom/spawn-loop.pid && ps -p "$(cat .loom/spawn-loop.pid)" -o pid,etime,command

# List active sweep children
jq '.running[] | {issue, pid, started_at}' .loom/spawn-loop-state.json
```

**Graceful shutdown**:
```bash
# Signal the spawn loop to stop accepting new work and drain in-flight children
./.loom/scripts/spawn-loop.sh stop
# or, equivalently:
touch .loom/stop-spawn-loop
```

The loop honors `SHUTDOWN_GRACE_SEC` (default 300s) before SIGKILL'ing any remaining sweep children.

**Force stop** (use with caution):
```bash
# Remove stop signal if it was set but never picked up
rm -f .loom/stop-spawn-loop

# Hard-kill the loop process
test -f .loom/spawn-loop.pid && kill -9 "$(cat .loom/spawn-loop.pid)" || true
rm -f .loom/spawn-loop.pid
```

**Stuck sweep child**:

A sweep child whose pid is alive but whose `.loom/sweep-checkpoint/issue-<N>.json` mtime is stale is likely stuck. Recovery:

```bash
# Check checkpoint mtime
ls -la .loom/sweep-checkpoint/issue-123.json

# Look at the child's log for errors
tail -200 .loom/logs/sweep-issue-123.log

# Kill the stuck pid; the loop will detect the dead pid on the next tick
# and release the claim (the checkpoint survives, so the issue will resume
# from its last completed phase the next time the loop spawns it).
jq '.running[] | select(.issue==123) | .pid' .loom/spawn-loop-state.json | xargs -I{} kill {}
```

**Work generation (Architect / Hermit) not running**:

**This is by design post-v0.10.0.** The spawn loop does not generate work — Architect and Hermit cadence is tracked under follow-up #3381. If you need new work generated automatically, run Architect/Hermit on a cron via the Phase 2a GitHub Actions pattern (`.github/workflows/loom-*.yml`); the existing five shipped workflows cover Champion / Curator / Judge / Auditor / Guide, but Architect and Hermit cron workflows are not yet shipped.

For now, trigger them manually when the queue is empty:

```bash
claude -p "/architect" --dangerously-skip-permissions
claude -p "/hermit"    --dangerously-skip-permissions
```

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

## Migration: deprecations targeted for v0.10.0

> **Stop-gap — daemon "preserved" claim is currently aspirational (epic #3449, stop-gap #3451)**
>
> The next paragraph says daemon mode "is preserved as a user-facing surface" and that `./.loom/scripts/daemon.sh` "continues to provide a tmux session container". On `origin/main` as of v0.9.1, `./.loom/scripts/daemon.sh` does **not** exist — it was deleted in #3432 and is being rebuilt in epic #3449 over an estimated 4-6 weeks for v0.10.0. The shepherd/Python-daemon-brain deletions are real and shipped; the daemon-shell rebuild is in flight. Until #3449 ships, use `./.loom/scripts/spawn-loop.sh` (headless) or GitHub Actions cron workflows.

Loom is in the middle of an orchestration-architecture migration (epic #3372). In **v0.10.0** (the next planned minor release), the shepherd brain (`loom-tools/src/loom_tools/shepherd/`), the Python daemon brain (`loom-tools/src/loom_tools/daemon_v2/`), and the `/shepherd` slash command will be **deleted** in favour of a minimal spawn loop (#3374) + GitHub Actions workflows (#3375). **Daemon mode itself is preserved as a user-facing surface** — `./.loom/scripts/daemon.sh` continues to provide a tmux session container with multi-account token rotation, re-implemented around the spawn loop. The phased rollout is:

| Phase | Issue | What ships | Status |
|-------|-------|-----------|--------|
| Phase 1 | #3374 | Minimal multi-account spawn loop (`./.loom/scripts/spawn-loop.sh`) | shipped |
| Phase 2a | #3375 | GitHub Actions workflows for support roles (Champion, Curator, Judge, Auditor, Guide) | shipped (workflows disabled by default) |
| Phase 2b | #3376 | **Soft-deprecation warnings on deprecated entry points** | shipped |
| Phase 3 | TBD | Deletion of shepherd brain, Python daemon brain, and `/shepherd` skill; re-implementation of `daemon.sh` around spawn loop + tmux | **v0.10.0** |
| Phase 4 | #3382 | Coordinated downstream sphere-install migration (deprecation banners in installed templates + this migration section) | shipped |

**v1.0.0 is intentionally unscheduled.** Loom remains pre-1.0 while the architecture settles. The migration guide filename
`docs/migration/v0.10.0-shepherd-deprecation.md` is named for the release that will ship the deletions.

**Deprecated entry points (still functional, now warn on use):**

| Deprecated | Replacement | Warning emitted from |
|------------|-------------|----------------------|
| `loom-daemon` (Python entry point) | `./.loom/scripts/daemon.sh` (preserved, re-implemented) OR `./.loom/scripts/spawn-loop.sh` (headless) + GitHub Actions schedules | `loom_tools.daemon_v2.cli.main()` |
| `loom-shepherd` CLI / `/shepherd` invocations | `/loom:sweep <issue>` for the same lifecycle | `loom_tools.shepherd.cli.main()` |
| `/shepherd` slash command (`defaults/.claude/commands/loom/shepherd.md`) | `/loom:sweep <issue>` | Markdown header instructs the LLM to emit the warning |

> **Note**: `./.loom/scripts/daemon.sh` was listed as deprecated in 0.9.1 under the original plan that anticipated removing daemon mode entirely. **That plan was revised**: the shell-level daemon surface is preserved in v0.10.0. The warning attached to `daemon.sh start` in 0.9.1 will be withdrawn in v0.10.0. The Python `loom-daemon` CLI warning escalates to a genuine "command not found" error.

**Suppression**: set `LOOM_SUPPRESS_DEPRECATION=1` to silence the warnings emitted from Python and shell entry points. The `/shepherd` markdown skill warning always renders by design — operators should explicitly migrate, not silence. Sphere installs and other downstream automation that haven't migrated yet can use this env var to keep their logs clean during the deprecation window.

**Helpers**: the warning text is centralized in two places so removal in Phase 3 is a single-PR sweep:

- Python: `loom_tools.common.deprecation.warn_deprecated(component, replacement, ref="#3372")`
- Shell: `source .loom/scripts/lib/deprecation.sh; warn_deprecated <component> <replacement> [ref]` — the bash helper is safe to source (no side effects).

## Migrating off shepherd (downstream consumers, Phase 4 of #3372)

> **Stop-gap reminder — daemon "preserved" claim is currently aspirational (epic #3449, stop-gap #3451)**
>
> The references to `./.loom/scripts/daemon.sh` below describe the intended v0.10.0 target state. As of v0.9.1, that file does **not** exist on `origin/main` (deleted in #3432, rebuild in flight under epic #3449, ~4-6 weeks). Until that lands, downstream consumers should treat the "daemon.sh preserved" prose as forward-looking and use `./.loom/scripts/spawn-loop.sh` (headless) or GitHub Actions cron workflows.

If you installed Loom via `scripts/install-loom.sh` (or `install.sh`), the shepherd brain (`loom-tools/src/loom_tools/shepherd/`), the Python daemon brain (`loom-tools/src/loom_tools/daemon_v2/`), and the `/shepherd` slash command are scheduled for **deletion in v0.10.0** (Phase 3 of epic #3372, tracked as part of #3382). **The user-facing daemon mode surface — `./.loom/scripts/daemon.sh` + tmux + token rotation — is preserved**, just re-implemented around the spawn loop. This section is the migration guide for downstream consumers; it complements the upstream phase table in the section above.

### What you can still rely on (post-v0.10.0)

These surfaces are **not** going away and are the supported replacements:

| Capability | Replacement | Where |
|------------|-------------|-------|
| Single-issue lifecycle (curator → builder → judge → doctor → merge) | `/loom:sweep <issue>` | `.claude/commands/loom/sweep.md` |
| PR-side back half (judge / doctor → judge / merge for an existing open PR set) | `/loom:sweep --prs <pr-number-list>` (Mode C, #3384) | `.claude/commands/loom/sweep.md` |
| **Daemon-managed tmux session with per-pane token rotation** | `./.loom/scripts/daemon.sh start` | **Preserved**, re-implemented around spawn loop |
| Multi-issue / multi-account batch orchestration (headless) | `LOOM_USE_SPAWN_LOOP=1 ./.loom/scripts/spawn-loop.sh` | Phase 1, #3374 |
| Periodic support roles (Champion, Curator, Judge, Auditor, Guide) | GitHub Actions cron workflows | `.github/workflows/loom-*.yml`, Phase 2a, #3375 |
| Per-task multi-account token rotation | `./.loom/scripts/spawn-claude.sh` + `.loom/tokens/` | Unchanged |
| Label-based coordination (`loom:issue` → `loom:building` → `loom:pr` → merged) | Unchanged | `.github/labels.yml` |
| Worktree workflow (`./.loom/scripts/worktree.sh`) | Unchanged | `.loom/scripts/worktree.sh` |
| `merge-pr.sh` for worktree-safe PR merging | Unchanged | `.loom/scripts/merge-pr.sh` |

### What is being deprecated (and what to do)

| Deprecated surface | Soft-deprecated since | What to do |
|--------------------|----------------------|------------|
| `loom-daemon` Python CLI | Phase 2b (#3376) | Replace direct calls with `./.loom/scripts/daemon.sh start` (preserved, re-implemented) for the tmux multi-account surface, or `./.loom/scripts/spawn-loop.sh` for the headless minimal surface. |
| `loom-shepherd` Python CLI | Phase 2b (#3376) | Replace shell invocations with `claude -p "/loom:sweep <N>" --dangerously-skip-permissions`. The lifecycle phases are identical; checkpointing (#3373) is preserved. |
| `/shepherd` slash command | Phase 2b (#3376) | Use `/loom:sweep <issue>` instead. Both run Curator → Builder → Judge → Doctor → Merge; the sweep skill is the maintained path. |
| `loom-shepherd` subagent (`.claude/agents/loom-shepherd.md`) | Phase 4 (this section, #3382) | Stop dispatching to it from custom slash commands or hooks. The agent file will be removed in v0.10.0 along with the role definition it points at (`.loom/roles/shepherd.md`). The `loom-daemon` subagent is preserved and now documents the shell-level daemon surface. |
| `.loom/daemon-state.json` and `.loom/progress/shepherd-*.json` consumers | Will be silent after v0.10.0 | The producers are being deleted (the Python brain wrote them). The per-consumer disposition table lives in `docs/migration/daemon-state-consumers.md` (PR #3389) — read it before writing new code that depends on either file. Most operator CLIs are slated to port to `.loom/spawn-loop-state.json` + forge queries; a few retire entirely. |

### Three migration paths for downstream consumers

You have three valid choices for handling v0.10.0. None of them require you to migrate before Phase 3 ships — they just determine what happens on your next `install-loom.sh` run.

**(a) Migrate now (recommended for active deployments).** Switch to `/loom:sweep` + spawn loop + GitHub Actions workflows on your main branch before Phase 3 ships. If you had `./.loom/scripts/daemon.sh start` in your automation, no operator-side change is needed — the API is preserved; only the internals change. Your `install-loom.sh` run after Phase 3 lands will cleanly install the new defaults with no manual cleanup.

**(b) Defer migration (acceptable for downstream timelines).** Coordinate with the Loom maintainer to time Phase 3's merge with a migration window that suits your release schedule. The mechanic is a comment thread on #3382. Soft-deprecation warnings (Phase 2b) will continue rendering in your logs during the deferral; use `LOOM_SUPPRESS_DEPRECATION=1` to silence the Python/shell variants if needed (the `/shepherd` markdown skill warning always renders).

**(c) Pin to a pre-Phase-3 Loom version (acceptable, but opts you out of upgrades).** Stop running `install-loom.sh` against your repo, or pin `loom` to a specific tag in your install scripts. This is a valid operator choice — it simply means you stop receiving Loom upgrades until you choose to migrate. The installer is idempotent and will not overwrite your pinned files on a no-op invocation, but rerunning it after pinning intentionally to a new Loom version will replace them.

### Why this matters for the installed `defaults/` tree

When Phase 3 ships in v0.10.0, the next time you run `scripts/install-loom.sh` (or `install.sh`) against your repo, these files will **disappear from `defaults/`** and therefore from your repo's `.claude/agents/` and `.claude/commands/loom/`:

- `defaults/.claude/agents/loom-shepherd.md`
- `defaults/.claude/commands/loom/shepherd.md`
- `defaults/.claude/commands/loom/shepherd-lifecycle.md`
- `defaults/roles/shepherd.md`

These are **preserved**:

- `defaults/.claude/agents/loom-daemon.md` (retitled to document the shell-level daemon surface)
- `defaults/roles/loom.md` (likewise)
- `./.loom/scripts/daemon.sh` (re-implemented around spawn loop + tmux + token rotation)

If you have custom slash commands, hooks, or scripts that reference any of the **deleted** paths, the next install will quietly stop installing them. The deprecation banners on the template files (added in Phase 4, this issue) exist so you see the warning on your **very next** install — well before Phase 3 actually removes them.

### Coordination and questions

- **Upstream tracker**: #3382 ("Phase 4: sphere downstream coordination tracker"). Open a comment there if you need to coordinate timing, request a deferral window, or report a missing migration path.
- **Consumer inventory + disposition table**: `docs/migration/daemon-state-consumers.md` (PR #3389) — read this if you have code that imports from `loom_tools.daemon_v2` or `loom_tools.shepherd`, or reads `.loom/daemon-state.json` or `.loom/progress/`.
- **Migration guide**: `docs/migration/v0.10.0-shepherd-deprecation.md`.
- **Phase 2d follow-up**: Architect/Hermit work-generation cadence after the Python brain is removed — tracked in #3381.

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
