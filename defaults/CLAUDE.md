# Loom Orchestration - Repository Guide

This repository uses **Loom** for AI-powered development orchestration.

**Loom Version**: {{LOOM_VERSION}}
**Installation Date**: {{INSTALL_DATE}}

## What is Loom?

Loom is a CLI + daemon for AI-powered development orchestration. It coordinates AI development workers using git worktrees and a forge (GitHub or Gitea) as the coordination layer. It supports manual coordination (Manual Orchestration Mode), continuous autonomous orchestration via the Rust `loom-daemon` binary (MCP-level dispatch + pub/sub + monitoring), and GitHub Actions cron schedules for periodic support roles.

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
│            Tier 2: loom-daemon + GitHub Actions cron            │
│  loom-daemon (Rust binary, MCP-level surface)                   │
│   - dispatch_sweep / list_sweeps (sweep registry)               │
│   - subscribe_to_events / publish_event (event bus)             │
│   - get_sweep_status / cancel_sweep / tail_sweep_log            │
│   - Multi-account token rotation per dispatch                   │
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
| Tier 2 | `loom-daemon` (MCP) + GH Actions cron | Multi-issue dispatch + scheduled support roles | Continuous / cron |
| Tier 1 | `/loom:sweep <issue>` | Issue lifecycle from creation to merge | Per-issue |
| Tier 0 | `/builder`, `/judge`, etc. | Task execution — single focused work units | Per-task |

### Tier responsibilities

**Tier 3 (Human Observer)**:
- Override Champion decisions on controversial proposals (Champion handles routine approvals)
- Monitor system health via the forge directly + `loom-status`
- Intervene for blocked issues or stuck agents
- Provide strategic direction on what to build

**Tier 2 (`loom-daemon` + GH Actions cron)**:
- The Rust `loom-daemon` binary exposes MCP tools for dispatching `/loom:sweep` children with multi-account OAuth token rotation (`mcp__loom__dispatch_sweep`), tracking running sweeps (`mcp__loom__list_sweeps`, `mcp__loom__get_sweep_status`), pub/sub eventing on a frozen 6-topic taxonomy (`mcp__loom__publish_event`, `mcp__loom__subscribe_to_events`), and cancellation (`mcp__loom__cancel_sweep`). State is in-memory only — the forge is the source of truth for queue state.
- The GitHub Actions workflows under `.github/workflows/loom-*.yml` run periodic support roles (Champion, Curator, Judge, Auditor, Guide) on cron schedules.
- Architect / Hermit cadence (work generation) is currently manual — tracked under follow-up #3381.

**Tier 1 (`/loom:sweep`)**:
- Fully autonomous once spawned
- Handles entire issue lifecycle including Judge review
- Checkpoints survive crashes — restarting `/loom:sweep N` resumes from the last completed phase

### When to use which tier

**Use `/loom:sweep <issue>`** (Tier 1) when:
- You have a specific issue to implement
- You want to orchestrate one issue through its full lifecycle
- Running manual orchestration mode

**Use `mcp__loom__dispatch_sweep`** (against a running `loom-daemon`) when:
- You want autonomous multi-issue dispatch with multi-account token rotation
- Multiple sweeps need to run in parallel under daemon supervision
- You need to monitor sweep lifecycle events (pub/sub) or cancel in-flight sweeps
- Running production-scale orchestration where the daemon's reaper task and in-memory registry are the source of truth for "what is currently dispatched"

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

**Wave parallelism default (#3566)**: when `--builders-per-wave` is omitted, `/loom:sweep` auto-resolves the wave size at Stage -1 from the chosen backend and scratch-volume disk headroom — the daemon detached-process path (isolated OS processes, not nested subagents) targets up to **10**, while the in-session subagent path stays at the **#3289-safe cap of 3**. The disk gate measures the **worktree-root filesystem** (`LOOM_WORKTREE_ROOT` / `worktree.root`, #3539/#3541), so a dedicated scratch volume rarely binds. Passing an explicit `--builders-per-wave N` overrides auto. `--dry-run` prints the resolved size, mechanism, and gating reason. See `.claude/commands/loom/sweep.md` → "Resolve auto wave size".

### 3. Daemon Mode (`loom-daemon` + MCP tools)

The Rust `loom-daemon` binary is the Tier 2 dispatch backend. It is a single long-lived process exposing a Unix-socket IPC surface and a paired `mcp-loom` MCP server. Each IPC `Request` variant maps 1:1 to an MCP tool, so any MCP client — most commonly a Claude Code session running `/loom:sweep` — can dispatch sweeps, observe registry state, subscribe to lifecycle events, and cancel in-flight work.

**MCP surface** (Phases A–C of epic #3449, all shipped):

| MCP tool | Purpose | Phase |
|----------|---------|-------|
| `mcp__loom__dispatch_sweep` | Dispatch a sweep for an issue (token rotation via `spawn-claude.sh`) | A (#3452) |
| `mcp__loom__list_sweeps` | Enumerate running sweeps in the in-memory registry | A (#3452) |
| `mcp__loom__publish_event` | Publish a sweep-lifecycle event on the in-memory bus | B (#3453) |
| `mcp__loom__subscribe_to_events` | Stream topic-filtered events to a subscriber | B (#3453) |
| `mcp__loom__get_sweep_status` | Inspect a running sweep's state (PID, phase, started_at) | C (#3455) |
| `mcp__loom__tail_sweep_log` | Tail `.loom/logs/sweep-issue-<N>.log` | C (#3455) |
| `mcp__loom__cancel_sweep` | Cancel a running sweep (SIGTERM → grace → SIGKILL) | C (#3455) |
| `mcp__loom__tail_event_bus` | Tail the event bus without subscribing to a topic | C (#3455) |

**Event taxonomy (frozen for v0.10.0)**: `sweep.issue.{N}.phase`, `sweep.issue.{N}.blocker`, `sweep.issue.{N}.exited`, `sweep.issue.{N}.crashed`, `sweep.global.dispatch`, `sweep.global.completed`. New topics require a follow-up issue — the v0.10.0 set is intentionally frozen.

**`/loom:sweep` backend detection (Stage -1, Phase D #3454)**: the skill probes whether the daemon is reachable (a Ping over the IPC socket with a 500ms timeout) AND whether a multi-account token pool exists (`.loom/tokens/` contains ≥ 2 `ACCOUNT_KEY_*` entries). **Strict AND** — either probe failing falls through to in-process subagent dispatch (the existing Mode A/B/C lifecycle, no behaviour change for solo-token operators). Mode C (`--prs`) always uses subagent dispatch; the daemon does not handle PR-set dispatch in v0.10.0. The `--no-daemon` flag forces subagent dispatch unconditionally.

**Per-sweep state on disk**:

| File | Purpose |
|------|---------|
| `.loom/logs/sweep-issue-<N>.log` | Per-issue child output (tailable via `mcp__loom__tail_sweep_log`) |
| `.loom/sweep-checkpoint/issue-<N>.json` | Crash-resume checkpoint (#3373); the sweep skill reads it on entry and skips already-completed phases |

The daemon **does not** poll the forge for ready issues, **does not** maintain a `shepherd-N` pool, and **does not** drive support roles on cron. Those responsibilities live in `mcp__loom__dispatch_sweep` (operator-driven enqueue) and the GitHub Actions cron workflows (periodic support roles).

For the full surface — IPC request/response variants, event-bus internals, registry behaviour, reaper semantics — see [`.loom/docs/daemon-reference.md`](.loom/docs/daemon-reference.md).

> **Legacy spawn loop deprecated**: `defaults/scripts/spawn-loop.sh` (Phase 1, #3374) is deprecated as of Phase E of #3449. It emits a stderr warning on every `start` / `status` / `stop` invocation and will be deleted in v0.11.0. Use `mcp__loom__dispatch_sweep` against `loom-daemon` instead. Suppress the warning with `LOOM_SUPPRESS_DEPRECATION=1` while you migrate. See [the migration guide](../docs/migration/v0.10.0-shepherd-deprecation.md).

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

Architect and Hermit cadence (work-generation triggers) is intentionally out of scope here — see follow-up #3381. Post-v0.10.0, the schedule-driven cron workflows are the recommended path for support roles since the Python daemon brain is removed. Operators who want multi-account-rotated dispatch (rather than the single-CI-token GH Actions surface) should use `mcp__loom__dispatch_sweep` against `loom-daemon` from a Claude Code session — each dispatched sweep picks a fresh OAuth token via `spawn-claude.sh`.

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

> **Note**: the historical `shepherd.md` (single-issue orchestrator) role file was removed in v0.10.0 along with the `/shepherd` slash command — see [the migration guide](../docs/migration/v0.10.0-shepherd-deprecation.md). Its orchestration responsibilities moved to `/loom:sweep` (Tier 1) and the `loom-daemon` + GH Actions cron (Tier 2). The `loom.md` role file is preserved and documents the daemon-mode operator surface: a Claude Code session that observes the running `loom-daemon` via MCP tools (`mcp__loom__list_sweeps`, `mcp__loom__get_sweep_status`, `mcp__loom__subscribe_to_events`) and dispatches new work via `mcp__loom__dispatch_sweep`. The historical Python brain (`loom_tools/daemon_v2/`) is gone; the MCP-level surface is the supported coordination point. The worker-role markdown files above are unchanged.

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

### Daemon Configuration (Tier 2)

The Rust `loom-daemon` binary is the load-bearing Tier 2 dispatch backend. It is a single long-lived process that holds the sweep registry, the event bus, and the reaper task in memory — there is no on-disk state file the operator needs to touch. The forge is the source of truth for queue state; the daemon's in-memory registry tracks only currently-dispatched sweeps.

**Process model**: the daemon runs as a detached background process. Each `mcp__loom__dispatch_sweep` request fork+execs a fresh `claude -p "/loom:sweep N"` child via `defaults/scripts/spawn-claude.sh`, picking an OAuth token from the `.loom/tokens/` pool (token rotation only works at process-spawn boundaries; the daemon never holds the token itself). The reaper task ticks every 30 seconds, sweeping dead PIDs out of the registry and emitting `sweep.issue.{N}.exited` / `sweep.issue.{N}.crashed` events.

**MCP tools available** (each maps 1:1 to a daemon IPC `Request` variant):

| MCP tool | Description |
|----------|-------------|
| `mcp__loom__dispatch_sweep` | Dispatch a sweep for an issue (returns a sweep ID) |
| `mcp__loom__list_sweeps` | Enumerate registry entries |
| `mcp__loom__get_sweep_status` | Inspect a single sweep's state |
| `mcp__loom__cancel_sweep` | SIGTERM → grace → SIGKILL a running sweep |
| `mcp__loom__tail_sweep_log` | Tail `.loom/logs/sweep-issue-<N>.log` |
| `mcp__loom__publish_event` | Publish a sweep-lifecycle event |
| `mcp__loom__subscribe_to_events` | Topic-filtered event stream |
| `mcp__loom__tail_event_bus` | Untopiced event tail |

See [`.loom/docs/daemon-reference.md`](.loom/docs/daemon-reference.md) for the wire protocol, the frozen 6-topic event taxonomy, and the registry/reaper semantics.

**Per-sweep state on disk**:

| File | Purpose |
|------|---------|
| `.loom/logs/sweep-issue-<N>.log` | Per-issue child output (tailable via `mcp__loom__tail_sweep_log`) |
| `.loom/sweep-checkpoint/issue-<N>.json` | Crash-resume checkpoint (#3373); sweep skill reads it on entry |

**Issue selection** is operator-driven via `mcp__loom__dispatch_sweep` — the daemon does not autonomously claim items from the `loom:issue` queue. (The `/loom:sweep` skill is the typical caller; Stage -1 backend detection picks an issue and dispatches it when both daemon and pool probes succeed.)

**Sweep checkpoints** (`.loom/sweep-checkpoint/issue-<N>.json`, gitignored) — the per-issue checkpoint format is owned by the sweep skill (#3373). When a sweep child crashes, the next dispatch reads the checkpoint and skips already-completed phases.

**Scheduled support roles** run as separate GitHub Actions cron jobs under `.github/workflows/loom-*.yml`. They have no persistent state on the Loom side; each tick is a fresh `claude -p "/<role>" --dangerously-skip-permissions` invocation.

> **Legacy spawn-loop state**: the v0.9.x state file `.loom/spawn-loop-state.json` is still written by the deprecated `spawn-loop.sh` (scheduled for deletion in v0.11.0). The daemon does not consume it. Operators who need to observe running sweeps should call `mcp__loom__list_sweeps` against the daemon instead.

### Model Selection Strategy

Model selection is a first-class orchestration concern (issue #3477, Phase 1). Each worker's model is resolved through a fixed precedence chain — highest first:

1. **Explicit dispatch param** — `mcp__loom__dispatch_sweep`'s optional `model` argument (daemon path), an explicit `--model` flag passed to `spawn-claude.sh` / `claude-wrapper.sh`, or an operator-requested model for an in-session sweep.
2. **Workspace override** — `.loom/config.json` → `terminals[].roleConfig.model` (optional). Pin exact IDs here (e.g., `claude-sonnet-4-6`) when your workspace needs deterministic cost/behavior.
3. **Role default** — `.loom/roles/<role>.json` → `suggestedModel` (ships as an alias). The `/loom:sweep` skill passes the resolved model to role subagents via the Task tool's `model` parameter.
4. **Session default** — when nothing above resolves, NO `--model` flag (and no Task `model` param) is emitted at all, and the worker inherits the parent session/CLI default. This is the zero-config behavior: nothing configured means nothing changes.

The spawn plumbing also honors a `LOOM_MODEL` environment variable (`spawn-claude.sh`, `claude-wrapper.sh`): it is injected as `--model <value>` unless an explicit `--model` is already present in the args. Retries inside `claude-wrapper.sh` always reuse the same model — transport-level failures (token exhaustion, crashes, 5xx) are not quality signals and never change the model.

**Escalation on Judge rejection (`sweep.escalation`, Phase 2, issue #3481)**:

The `/loom:sweep` orchestrator escalates one rung up a capability ladder when the Judge rejects a PR (`loom:changes-requested`) and a Doctor is dispatched to address the feedback. The escalation decision is made by the sweep orchestrator at Doctor-dispatch time — never by `claude-wrapper.sh` retries, never by worker self-assessment. Mode C (`--prs`) inherits the same rule for its Doctor phase (step C1b).

The ladder is configured in `.loom/config.json`:

```json
{
  "sweep": {
    "escalation": ["sonnet", "opus"]
  }
}
```

| Value | Behavior |
|-------|----------|
| Key absent | Default ladder `["sonnet", "opus"]` applies |
| `[]` or `false` | Escalation disabled entirely (pure Phase 1 behavior) |
| Non-empty array | As configured; rungs accept aliases or pinned IDs |

**Precedence interaction**: escalation replaces only tier 3 (`suggestedModel`) / tier 4 (session default) resolution for the rejection-triggered Doctor. Tier 1 (explicit dispatch param) and tier 2 (`roleConfig.model` workspace pin) always win — pins are never overridden. `ladder[0]` never overrides anything either: first attempts of every role use the unmodified precedence chain, and the ladder only fires on rejection (the rejection-triggered Doctor gets `ladder[1]`).

**Cap interaction**: escalation composes with — and does not extend — the single Doctor→Judge cycle cap. A second Judge rejection blocks the PR rather than dispatching another Doctor, so a configured third rung (e.g., a frontier model) is dormant until a future issue raises the cap. The sweep checkpoint's optional `attempt` field (`sweep-checkpoint.sh write N doctor-done --attempt 2`) is forward-compat bookkeeping for that future; absent means attempt 1, and legacy checkpoints without the field read cleanly.

**Suggested models by role** (`suggestedModel`, live as the role-default tier):

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

**Valid model values**: aliases (`haiku`, `sonnet`, `opus`) or pinned model IDs (e.g., `claude-sonnet-4-6`).

- **haiku**: Fast, cheap - for simple status checks and monitoring
- **sonnet**: Balanced - for structured tasks with clear criteria
- **opus**: Most capable - for complex reasoning and implementation

**Aliases vs pinned IDs**: shipped role JSONs use aliases so defaults stay sensible across model releases with zero maintenance. The GitHub Actions cron workflows (`.github/workflows/loom-*.yml`) are the exception — they pin exact IDs because scheduled support roles are predictable, cost-sensitive load and a stale pin is visible and cheap to bump in the consuming repo.

**Workspace override example** (`.loom/config.json`):

```json
{
  "terminals": [
    {
      "id": "terminal-1",
      "name": "Builder",
      "role": "claude-code-worker",
      "roleConfig": {
        "workerType": "claude",
        "roleFile": "builder.md",
        "model": "claude-sonnet-4-6"
      }
    }
  ]
}
```

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

Loom ships with two built-in Bash `PreToolUse` guard hooks, both registered under the `Bash` matcher and firing independently:

- **`guard-destructive.sh`** — the generic repository-hygiene guard: catastrophic denies (`rm -rf /`, force-push to `main`, `gh repo delete`, fork bombs, curl-pipe-to-shell, cloud/SQL destruction), the segment-parsed lifecycle/cloud-CLI checks, and the `guards.sqlDdl` / `guards.cloudCli` / `guards.rmScope` toggle machinery. Nothing about this guard is Loom-specific; it is slated to move to Repo Skills (companion issue [rjwalters/repo#13](https://github.com/rjwalters/repo/issues/13)), which will own the generic half once it ships. Until then it keeps shipping and working in Loom exactly as before.
- **`guard-loom-workflow.sh`** — the thin, Loom-workflow-specific guard (issue #3604): the `gh pr merge` → `merge-pr.sh` redirect and the `pip install -e` worktree block (keyed on `LOOM_WORKTREE_PATH`, issue #2495). These two guards are specific to the Loom worktree/merge workflow and stay Loom-owned.

You can also add project-specific guards to protect read-only directories from accidental edits (see below).

### SQL DDL/DML Guard Opt-Out (`guards.sqlDdl` / `LOOM_GUARD_SQL`)

`guard-destructive.sh` blocks SQL DDL/DML patterns — `DROP DATABASE`, `DROP TABLE`, `DROP SCHEMA`, `TRUNCATE TABLE`, and `DELETE FROM` without a `WHERE` clause. For most repos this is a useful safety net, but for a project that is **itself a database engine** (e.g. a SQLite-compatible engine running a SQL conformance suite) those statements are the product's own dev/test vocabulary and the guard is a category error — the match is a case-insensitive substring, so it even fires when the words appear in a comment or a `--description` label.

Such repos can opt out of the SQL guard while keeping every other guard (`rm -rf /`, force-push to `main`, `gh repo delete`, `aws s3 rb`, `aws iam delete`, etc.) fully active.

The SQL guard is **on by default**. It is resolved in this order (highest precedence first):

1. **`LOOM_GUARD_SQL` env var** — `0`/`false`/`no` disables the SQL guard; `1`/`true`/`yes` forces it on. Overrides the config value.
2. **`.loom/config.json`** — `guards.sqlDdl` (default `true` when absent). Set it to `false` to disable:
   ```json
   {
     "guards": {
       "sqlDdl": false
     }
   }
   ```
3. **Default** — `true` (guard on).

The config read is best-effort: a missing, empty, or malformed `.loom/config.json` falls through to guard-ON and never causes the hook to exit non-zero. Only the SQL DDL/DML blocks are affected — disabling the SQL guard does not weaken any other guard.

**Examples**:

```bash
# Disable the SQL guard for a single command (e.g. a one-off dev query)
LOOM_GUARD_SQL=0 vibesql -c "DROP TABLE t"

# Persist the opt-out for the whole repo
#   .loom/config.json  ->  { "guards": { "sqlDdl": false } }

# Force the SQL guard on for one command even when the repo opts out
LOOM_GUARD_SQL=1 psql -c "DROP TABLE users"
```

### Cloud CLI Guard Opt-Out (`guards.cloudCli` / `LOOM_GUARD_CLOUD`)

`guard-destructive.sh` asks for confirmation on **mutating** cloud/container CLI calls — `aws ec2 run-instances`/`create-*`/`stop-instances`/`start-instances`/`terminate-instances`, `aws s3 rm`/`rb`/`cp`/`mv`/`sync`, other mutating `aws <service> <verb>` forms, and `docker rm`/`rmi`/`stop`/`kill`/`restart`. Read-only calls (`aws ec2 describe-instances`, `aws s3 ls`, `aws lambda list-functions`, `docker ps`, `docker logs`, etc.) are **not** prompted. For a repo whose *purpose* is managing cloud infrastructure (launch/stop/terminate dev VMs, build/tear-down containers), even the mutating asks are workflow friction rather than a safety win.

Such repos can opt out of the cloud/docker ASK category while keeping every other guard active — including the genuinely catastrophic cloud denies (`aws s3 rm ... --recursive`, `aws s3 rb`, `aws iam delete-*`, `aws cloudformation delete-stack`, `docker system prune`), which are **never** gated by this toggle and stay hard denies even with the cloud guard off.

The cloud guard is **on by default**. It is resolved in this order (highest precedence first):

1. **`LOOM_GUARD_CLOUD` env var** — `0`/`false`/`no` disables the cloud/docker ASK category; `1`/`true`/`yes` forces it on. Overrides the config value.
2. **`.loom/config.json`** — `guards.cloudCli` (default `true` when absent). Set it to `false` to disable:
   ```json
   {
     "guards": {
       "cloudCli": false
     }
   }
   ```
3. **Default** — `true` (guard on).

The config read is best-effort: a missing, empty, or malformed `.loom/config.json` falls through to guard-ON and never causes the hook to exit non-zero. Only the cloud/docker ASK patterns are affected — disabling the cloud guard does not weaken the catastrophic cloud denies or any other guard.

Note: `aws ec2 terminate-instances` is an **ask** (not a hard deny) so a legitimate VM-teardown workflow is possible; with `guards.cloudCli:false` / `LOOM_GUARD_CLOUD=0` it passes through without prompting.

**Examples**:

```bash
# Tear down a dev VM without a prompt for a single command
LOOM_GUARD_CLOUD=0 aws ec2 terminate-instances --instance-ids i-1234

# Persist the opt-out for a cloud-management repo
#   .loom/config.json  ->  { "guards": { "cloudCli": false } }

# Force the cloud guard on for one command even when the repo opts out
LOOM_GUARD_CLOUD=1 aws ec2 terminate-instances --instance-ids i-1234
```

### Repo-Scoped rm Guard (`guards.rmScope` / `LOOM_RM_SCOPE`)

By default (as of #3628), `guard-destructive.sh` runs in **`repo` mode**: it blocks the **catastrophic** `rm -rf` targets — root (`/`), the user's `$HOME`, and any bare top-level directory (`/tmp`, `/var`, `/etc`, …) — **and** additionally denies any `rm -rf` target that is neither inside the repo/worktree areas nor on a built-in **ephemeral allowlist**. So an outside-repo deep path like `rm -rf /Users/someone/important` is **denied** out of the box. This is the safe-by-default behaviour (ADR Option B); it is a **behaviour change** from the pre-#3628 permissive default.

Repos that need the old permissive behaviour — block only catastrophic targets and **allow** every deeper subpath, including subpaths outside the repository — can **opt out** to `off` (a.k.a. `permissive`) mode. The catastrophic top-level deny stays active in both modes, so bare `/tmp` and `/` are always blocked regardless.

The rm-scope guard is **repo (on) by default**. It is resolved in this order (highest precedence first):

1. **`LOOM_RM_SCOPE` env var** — `repo` forces repo mode; `off`/`0`/`no`/`permissive` forces the permissive opt-out; unset falls through to the config/default. Overrides the config value.
2. **`.loom/config.json`** — `guards.rmScope`. An explicit `"off"` (or its synonym `"permissive"`) opts out to permissive mode; an absent key, any other value, or malformed JSON resolves to `"repo"` (the safe default):
   ```json
   {
     "guards": {
       "rmScope": "off"
     }
   }
   ```
3. **Default** — repo (safe-by-default, outside-repo deep `rm` denied).

The config read is best-effort: a missing, empty, or malformed `.loom/config.json` falls through to **repo** (the safe default) and never causes the hook to exit non-zero. The permissive opt-out does not weaken any other guard — the catastrophic denies stay active.

**In-scope targets** (allowed under `repo` mode):

- Anything under the **repo root** (resolved from the command's `cwd`).
- Anything under the **worktree root** — resolved with the same precedence as `loom_worktree_root()`: `LOOM_WORKTREE_ROOT` env → `.loom/config.json → worktree.root` → the default `<repo>/.loom/worktrees`. This admits an external scratch volume (e.g. `worktree.root: "/Volumes/scratch/wt"`).
- The **ephemeral allowlist**: system temp roots and the Claude scratchpad.

**Ephemeral allowlist prefixes**. `normalize_abs_path()` is **lexical only** — it does **not** resolve symlinks — so on macOS each temp root is listed in **both** its symlink form and its `/private` target:

| Symlink form | `/private` target |
|--------------|-------------------|
| `/tmp/…` | `/private/tmp/…` |
| `/var/tmp/…` | `/private/var/tmp/…` |
| `/var/folders/…` (`$TMPDIR`) | `/private/var/folders/…` |

Plus the Claude scratchpad glob `*/claude-*/*/scratchpad/*`. A **bare** temp root (`/tmp`, `/private/tmp`, …) is never admitted here — bare `/tmp` is already caught by the catastrophic top-level deny, and prefix matches carry a trailing `/` so a name-prefix sibling like `/tmpfoo/x` is **not** admitted by the `/tmp/` entry.

**Examples**:

```bash
# Default (repo mode) — no config needed:
rm -rf /Users/someone/important   # DENIED (outside repo, safe default)
rm -rf /tmp/build-cache/x         # allowed (ephemeral allowlist)
rm -rf ./dist                     # allowed (under repo)

# Opt out to the old permissive behaviour for a whole repo:
#   .loom/config.json  ->  { "guards": { "rmScope": "off" } }        # or "permissive"

# One-off env opt-out — force permissive for a single command:
LOOM_RM_SCOPE=off rm -rf /Users/someone/scratch       # allowed (permissive)

# Force repo mode for one command even when the repo opts out:
LOOM_RM_SCOPE=repo rm -rf /Users/someone/important    # DENIED (outside repo)
```

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

**Topic context** (`topics/<name>.md`): Injected when the prompt matches the topic keyword. By default the filename is matched as an **anchored** token — the topic name must appear either as a slash command (`/loom:<name>` or `/repo:<name>`) or as a standalone word that is not part of a flag or path segment. So `security.md` injects on "check the security model" or `/loom:security`, but a "release" topic does **not** inject on `cargo build --release` or `target/release`. For custom matching, create a sidecar `.pattern` file with a regex (e.g., `security.pattern` containing `security|auth|token|credential`); the sidecar overrides the filename fallback entirely.

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

`/loom:sweep` automatically runs `./.loom/scripts/check-host-sleep.sh` at startup and warns when the host can sleep. This is **advisory only** — Loom never blocks on it. Heed the warning before walking away from a long run.

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

**Orphaned task recovery (daemon dispatch crashes)**:

When a dispatched sweep child crashes abruptly and the daemon's reaper hasn't yet caught up, sweep state may diverge briefly from forge state. `loom-orphan-recovery` (the ported successor to the historical `recover-orphaned-shepherds.sh`) reconciles this:

```bash
# Check for orphaned tasks (dry run)
loom-orphan-recovery

# Actually recover orphaned state
loom-orphan-recovery --recover

# JSON output for automation
loom-orphan-recovery --json
```

**What it detects** (post-v0.10.0):
- Dead PIDs still listed by `mcp__loom__list_sweeps`
- `loom:building` issues with no live sweep in the daemon registry
- Stale lock dirs at `.loom/locks/issue-<N>/` for closed/merged issues

**What it recovers**:
- Returns orphaned issues from `loom:building` to `loom:issue`
- Adds recovery comments to affected issues
- Removes stale lock dirs

The daemon's 30-second reaper task usually catches this autonomously; `loom-orphan-recovery` is the manual cross-check.

### Stuck Agent Detection

`loom-stuck-detection` checks for stuck sweep children by combining the daemon registry (via `mcp__loom__list_sweeps`) with `.loom/sweep-checkpoint/issue-<N>.json` checkpoint timestamps.

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
| `dead_pid` | (instant) | PID in daemon registry is no longer alive |
| `error_spike` | 5 errors | Multiple errors in `.loom/logs/sweep-issue-N.log` |

The pre-v0.10.0 indicators `no_progress`, `extended_work`, and `looping` are no longer tractable post-deletion of `.loom/progress/` — see [the migration guide § Per-CLI breaking changes](../docs/migration/v0.10.0-shepherd-deprecation.md#per-cli-breaking-changes) for the diff.

### Daemon Troubleshooting

**Inspect the daemon registry**:
```bash
# From a Claude Code session: list all currently-dispatched sweeps
# mcp__loom__list_sweeps

# Or inspect a single sweep
# mcp__loom__get_sweep_status --sweep_id <id>

# Tail per-sweep output
# mcp__loom__tail_sweep_log --issue 123
```

**Cancel a stuck sweep**:
```bash
# From a Claude Code session
# mcp__loom__cancel_sweep --sweep_id <id>
# Sends SIGTERM, waits the configured grace window, then SIGKILL.
# The checkpoint survives so the next dispatch resumes from the last phase.
```

**Stuck sweep child** (without using MCP tools):

A sweep child whose pid is alive but whose `.loom/sweep-checkpoint/issue-<N>.json` mtime is stale is likely stuck. Recovery:

```bash
# Check checkpoint mtime
ls -la .loom/sweep-checkpoint/issue-123.json

# Look at the child's log for errors
tail -200 .loom/logs/sweep-issue-123.log

# Kill the stuck pid; the daemon reaper will detect it within 30 seconds and
# emit a `sweep.issue.123.crashed` event. The checkpoint survives so the next
# `mcp__loom__dispatch_sweep` resumes from the last completed phase.
```

**Subscribe to sweep events for live debugging**:
```bash
# mcp__loom__subscribe_to_events --topic "sweep.issue.123.*"
# mcp__loom__tail_event_bus
```

See [`.loom/docs/daemon-reference.md`](.loom/docs/daemon-reference.md) for the full event taxonomy and IPC surface.

**Work generation (Architect / Hermit) not running**:

**This is by design post-v0.10.0.** Neither the daemon nor the GH Actions cron generates work for Architect / Hermit — that cadence is tracked under follow-up #3381. For now, trigger them manually when the queue is empty:

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

## Migration: v0.10.0 shepherd/daemon deprecation

Loom's orchestration architecture migration (epic #3372) deleted the shepherd brain (`loom-tools/src/loom_tools/shepherd/`), the Python daemon brain (`loom-tools/src/loom_tools/daemon_v2/`), and the `/shepherd` slash command. Epic #3449 then rebuilt the **daemon surface as a Rust binary** (`loom-daemon`) that exposes MCP tools for dispatch, monitoring, and pub/sub eventing — phases A through D have all shipped on main; phase E (this work) deprecates the v0.9.x `defaults/scripts/spawn-loop.sh` interim implementation.

| Phase | Issue | What shipped | Status |
|-------|-------|-----------|--------|
| Phase 1 | #3374 | Minimal multi-account spawn loop (`spawn-loop.sh`) | shipped (deprecated in Phase E) |
| Phase 2a | #3375 | GitHub Actions workflows for support roles | shipped (disabled by default) |
| Phase 2b | #3376 | Soft-deprecation warnings on deprecated entry points | shipped |
| Phase 3 | #3378 | Deletion of shepherd brain, Python daemon brain, `/shepherd` skill | shipped |
| Phase 4 | #3382 | Coordinated downstream sphere-install migration | shipped |
| #3449 Phase A | #3452 | `loom-daemon`: `dispatch_sweep`, `list_sweeps`, in-memory registry, reaper task | shipped |
| #3449 Phase B | #3453 | `loom-daemon`: event bus (tokio broadcast), 6 frozen topics, `publish_event` / `subscribe_to_events` IPC | shipped |
| #3449 Phase C | #3455 | MCP tools: `get_sweep_status`, `tail_sweep_log`, `subscribe_to_events`, `publish_event`, `cancel_sweep`, `tail_event_bus`; `.loom/docs/daemon-reference.md` rewrite | shipped |
| #3449 Phase D | #3454 | `/loom:sweep` Stage -1 backend detection (strict-AND daemon + pool probe) | shipped |
| #3449 Phase E | #3456 | `spawn-loop.sh` deprecation warning + operator-doc rewrite | this PR |

**v1.0.0 is intentionally unscheduled.** Loom remains pre-1.0 while the architecture settles. The migration guide filename `docs/migration/v0.10.0-shepherd-deprecation.md` is named for the release that ships the deletions.

**Removed entry points** (no longer present in v0.10.0+):

| Removed | Replacement |
|---------|-------------|
| `loom-daemon` (Python entry point) | Rust `loom-daemon` binary + `mcp__loom__dispatch_sweep` + GitHub Actions schedules |
| `loom-shepherd` CLI / `/shepherd` slash command | `/loom:sweep <issue>` for the same per-issue lifecycle |

**Deprecated entry points (still functional in v0.10.x, removed in v0.11.0)**:

| Deprecated | Replacement | Warning |
|------------|-------------|---------|
| `defaults/scripts/spawn-loop.sh` | `mcp__loom__dispatch_sweep` against `loom-daemon` | Stderr banner on every invocation (Phase E of #3449); suppressible with `LOOM_SUPPRESS_DEPRECATION=1` |

## Migrating off shepherd / spawn-loop (downstream consumers)

If you installed Loom via `scripts/install-loom.sh` (or `install.sh`), the shepherd brain and Python daemon brain are already gone; the spawn-loop deprecation in this phase is your final migration step before v0.11.0 deletes it.

### What you can still rely on (v0.10.0)

These surfaces are **not** going away and are the supported replacements:

| Capability | Replacement | Where |
|------------|-------------|-------|
| Single-issue lifecycle (curator → builder → judge → doctor → merge) | `/loom:sweep <issue>` | `.claude/commands/loom/sweep.md` |
| PR-side back half (judge / doctor → judge / merge for an existing open PR set) | `/loom:sweep --prs <pr-number-list>` (Mode C, #3384) | `.claude/commands/loom/sweep.md` |
| **Multi-account dispatch with monitoring and pub/sub** | `mcp__loom__dispatch_sweep`, `mcp__loom__list_sweeps`, `mcp__loom__get_sweep_status`, `mcp__loom__subscribe_to_events`, `mcp__loom__cancel_sweep`, `mcp__loom__tail_sweep_log`, `mcp__loom__publish_event`, `mcp__loom__tail_event_bus` | `loom-daemon` (Rust binary) + `mcp-loom` MCP server |
| `/loom:sweep` backend detection | Stage -1 strict-AND probe (daemon reachable AND multi-account pool present) | `.claude/commands/loom/sweep.md` Stage -1 |
| Periodic support roles (Champion, Curator, Judge, Auditor, Guide) | GitHub Actions cron workflows | `.github/workflows/loom-*.yml`, Phase 2a, #3375 |
| Per-task multi-account token rotation | `./.loom/scripts/spawn-claude.sh` + `.loom/tokens/` | Unchanged |
| Label-based coordination (`loom:issue` → `loom:building` → `loom:pr` → merged) | Unchanged | `.github/labels.yml` |
| Worktree workflow (`./.loom/scripts/worktree.sh`) | Unchanged | `.loom/scripts/worktree.sh` |
| `merge-pr.sh` for worktree-safe PR merging | Unchanged | `.loom/scripts/merge-pr.sh` |

### What is being deprecated (and what to do)

| Deprecated surface | Status | What to do |
|--------------------|--------|------------|
| `defaults/scripts/spawn-loop.sh` | Deprecated in v0.10.x (Phase E of #3449), deletion in v0.11.0 | Migrate to `mcp__loom__dispatch_sweep` against `loom-daemon`. The script still works through v0.10.x but emits a stderr warning on every `start` / `status` / `stop` invocation. Suppress with `LOOM_SUPPRESS_DEPRECATION=1` if you need the noise gone during migration. |
| `loom-shepherd` Python CLI (already removed in v0.10.0) | Removed | Replace shell invocations with `claude -p "/loom:sweep <N>" --dangerously-skip-permissions`. The lifecycle phases are identical; checkpointing (#3373) is preserved. |
| `/shepherd` slash command (already removed in v0.10.0) | Removed | Use `/loom:sweep <issue>` instead. |
| `loom-shepherd` subagent (already removed in v0.10.0) | Removed | Stop dispatching to it from custom slash commands or hooks. |
| `.loom/daemon-state.json` and `.loom/progress/shepherd-*.json` consumers | Producers removed | Replace reads with `mcp__loom__list_sweeps` / `mcp__loom__get_sweep_status` against the daemon, plus forge queries. See `docs/migration/daemon-state-consumers.md` for the per-consumer disposition. |

**Suppression**: set `LOOM_SUPPRESS_DEPRECATION=1` to silence the deprecation warnings emitted from the deprecated shell entry points. Sphere installs and other downstream automation mid-migration can use this env var to keep their logs clean during the v0.10.x → v0.11.0 window.

**Helpers**: the warning text is centralized in two places so removal in v0.11.0 is a single-PR sweep:

- Shell: `defaults/scripts/spawn-loop.sh::_deprecation_warn()` — fires on every `start` / `status` / `stop` subcommand.

### Coordination and questions

- **Daemon-rebuild epic**: #3449 (Phases A–E shipped; Phase F is the dedicated daemon-rebuild migration guide).
- **Original shepherd-deprecation epic**: #3372.
- **Consumer inventory + disposition table**: `docs/migration/daemon-state-consumers.md` — read this if you have code that imports from `loom_tools.daemon_v2` or `loom_tools.shepherd`, or reads `.loom/daemon-state.json` or `.loom/progress/`.
- **Full migration guide**: [`docs/migration/v0.10.0-shepherd-deprecation.md`](../docs/migration/v0.10.0-shepherd-deprecation.md).
- **Architect/Hermit cadence (work generation)** after the Python brain is removed — tracked in #3381.

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
