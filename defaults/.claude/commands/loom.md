# Loom Daemon

You are the Layer 2 Loom Daemon in the {{workspace}} repository. This skill invokes the Python daemon for autonomous development orchestration.

## Execution

Arguments provided: `{{ARGUMENTS}}`

### Mode Selection

```
IF arguments start with "help":
    -> Display the help content from the HELP REFERENCE section below
    -> If a sub-topic is provided (e.g., "help roles"), show only that section
    -> Do NOT run the daemon script
    -> EXIT after displaying help

ELSE IF arguments contain "health":
    -> Run: ./.loom/scripts/loom-daemon.sh --health
    -> Display the health report and EXIT

ELSE IF arguments contain "status":
    -> Run: ./.loom/scripts/loom-daemon.sh --status
    -> Display the status and EXIT

ELSE:
    -> Run the Python daemon with provided arguments
    -> The daemon runs continuously until stopped
```

### Running the Daemon

Execute the following command:

```bash
./.loom/scripts/loom-daemon.sh {{ARGUMENTS}}
```

The daemon will:
1. Run pre-flight checks (gh, claude, tmux availability)
2. Rotate previous daemon state
3. Initialize state and metrics files
4. Run startup cleanup (orphan recovery, stale artifacts)
5. Enter the main loop:
   - Capture system snapshot
   - Check for completed shepherds
   - Spawn shepherds for ready issues
   - Spawn support roles (interval and demand-based)
   - Auto-promote proposals (in force mode)
   - Sleep until next iteration
6. Run shutdown cleanup on exit

### Commands Quick Reference

| Command | Description |
|---------|-------------|
| `/loom` | Start daemon in normal mode |
| `/loom --merge` | Start in force mode (auto-promote, auto-merge) |
| `/loom --force` | Alias for --merge |
| `/loom -t 180` | Run for 3 hours then gracefully stop |
| `/loom --timeout-min 60 --merge` | Merge mode for 1 hour |
| `/loom --debug` | Start with debug logging |
| `/loom status` | Check if daemon is running |
| `/loom health` | Show daemon health status |
| `/loom help` | Show comprehensive help guide |
| `/loom help <topic>` | Show help for a specific topic |

### Graceful Shutdown

To stop the daemon gracefully:
```bash
touch .loom/stop-daemon
```

The daemon checks this file between iterations and exits cleanly.

### Environment Variables

| Variable | Default | Description |
|----------|---------|-------------|
| `LOOM_POLL_INTERVAL` | 120 | Seconds between iterations |
| `LOOM_MAX_SHEPHERDS` | 3 | Maximum concurrent shepherds |
| `LOOM_ISSUE_THRESHOLD` | 3 | Trigger work generation when issues < this |
| `LOOM_ARCHITECT_COOLDOWN` | 1800 | Seconds between architect triggers |
| `LOOM_HERMIT_COOLDOWN` | 1800 | Seconds between hermit triggers |

## Run Now

Execute this command and report when complete:

```bash
./.loom/scripts/loom-daemon.sh {{ARGUMENTS}}
```

---

## HELP REFERENCE

When the user runs `/loom help`, display the content below formatted as markdown. If the user provides a sub-topic (e.g., `/loom help roles`), display only the matching section. If no sub-topic or an unrecognized sub-topic is given, display all sections.

### Available sub-topics

List these when showing the full help or when the sub-topic is unrecognized:

```
/loom help              - Show this full help guide
/loom help quick-start  - Getting started in 60 seconds
/loom help roles        - All available agent roles
/loom help commands     - Slash command reference
/loom help workflow     - Label-based workflow overview
/loom help daemon       - Daemon mode and configuration
/loom help shepherd     - Single-issue orchestration
/loom help worktrees    - Git worktree workflow
/loom help labels       - Label state machine reference
/loom help troubleshoot - Common issues and fixes
```

---

### Sub-topic: quick-start

**Getting Started with Loom**

Loom orchestrates AI-powered development using GitHub issues, labels, and git worktrees.

**Try it now - Manual Mode (one terminal per role):**

```bash
# 1. Start as a Builder and work on an issue
/builder

# 2. In another terminal, review PRs as a Judge
/judge

# 3. Or curate issues to add implementation guidance
/curator
```

**Try it now - Autonomous Mode (daemon manages everything):**

```bash
# Start the daemon - it spawns shepherds, triggers work generation, and auto-merges
/loom --merge

# Or start conservatively (human approves merges)
/loom

# Check daemon health anytime
/loom health
```

**Try it now - Single Issue (shepherd handles the full lifecycle):**

```bash
# Orchestrate one issue from curation through merge
/shepherd 123 --merge
```

**Key concepts:**
- Issues flow through labels: `loom:curated` -> `loom:issue` -> `loom:building` -> PR -> merged
- Each role manages specific label transitions
- Agents coordinate through labels, not direct communication
- Work happens in git worktrees (`.loom/worktrees/issue-N`)

---

### Sub-topic: roles

**Agent Roles**

Loom has three layers of roles:

**Layer 2 - System Orchestration:**

| Command | Role | What it does |
|---------|------|-------------|
| `/loom` | Daemon | Runs continuously. Monitors pipeline, spawns shepherds, triggers work generation. |

**Layer 1 - Issue Orchestration:**

| Command | Role | What it does |
|---------|------|-------------|
| `/shepherd <N>` | Shepherd | Orchestrates a single issue through its full lifecycle: Curator -> Builder -> Judge -> Doctor -> Merge. |

**Layer 0 - Task Execution (Worker Roles):**

| Command | Role | What it does |
|---------|------|-------------|
| `/builder` | Builder | Implements features/fixes from `loom:issue` issues, creates PRs |
| `/judge` | Judge | Reviews PRs with `loom:review-requested`, approves or requests changes |
| `/curator` | Curator | Enhances issues with implementation guidance, marks `loom:curated` |
| `/doctor` | Doctor | Fixes PR feedback, resolves merge conflicts |
| `/champion` | Champion | Evaluates proposals, auto-merges approved PRs |
| `/architect` | Architect | Creates architectural proposals for new features |
| `/hermit` | Hermit | Identifies code simplification opportunities |
| `/guide` | Guide | Prioritizes and triages the issue backlog |
| `/auditor` | Auditor | Validates main branch builds and catches regressions |
| `/driver` | Driver | Plain shell for ad-hoc commands |
| `/imagine` | Bootstrapper | Bootstrap new projects with Loom |

---

### Sub-topic: commands

**Slash Command Reference**

**Daemon commands:**
```
/loom                          Start daemon in normal mode
/loom --merge                  Start in merge mode (auto-promote, auto-merge)
/loom -t 180                   Run for 3 hours then stop
/loom --timeout-min 60 --merge Merge mode with 1-hour timeout
/loom --debug                  Start with debug logging
/loom status                   Check if daemon is running
/loom health                   Show daemon health report
/loom help                     Show this help guide
/loom help <topic>             Show help for a specific topic
```

**Shepherd commands:**
```
/shepherd 123                  Orchestrate issue #123 (stop after PR approval)
/shepherd 123 --merge          Full automation including auto-merge
/shepherd 123 --to curated     Stop after curation phase
```

**Worker commands (with optional issue/PR number):**
```
/builder                       Find and implement the next loom:issue
/builder 42                    Implement issue #42 directly
/judge                         Find and review the next PR
/judge 100                     Review PR #100 directly
/curator                       Find and curate the next issue
/doctor                        Find and fix the next PR with feedback
```

---

### Sub-topic: workflow

**Label-Based Workflow**

Agents coordinate exclusively through GitHub labels. Here is how an issue flows through the system:

```
1. Issue Created (no loom labels)
       |
       v
2. /curator enhances -> adds "loom:curated"
       |
       v
3. Champion (or human) approves -> adds "loom:issue"
       |
       v
4. /builder claims -> removes "loom:issue", adds "loom:building"
       |
       v
5. Builder creates PR -> adds "loom:review-requested" to PR
       |
       v
6. /judge reviews PR -> removes "loom:review-requested"
       |                  adds "loom:pr" (approved)
       |              OR  adds "loom:changes-requested" (needs work)
       |
       v
7. /champion auto-merges -> PR merged, issue auto-closes
```

**If changes are requested:**
```
6b. /doctor fixes feedback -> removes "loom:changes-requested"
                               adds "loom:review-requested"
        |
        v
    Back to step 6 (Judge reviews again)
```

**Proposal flow (Architect/Hermit):**
```
/architect or /hermit creates proposal -> "loom:architect" or "loom:hermit"
       |
       v
/champion evaluates -> promotes to "loom:issue" if approved
```

---

### Sub-topic: daemon

**Daemon Mode**

The daemon (`/loom`) is the Layer 2 orchestrator that runs continuously and manages the entire development pipeline.

**Starting the daemon:**
```bash
/loom                  # Normal mode - human approves merges
/loom --merge          # Merge mode - auto-promote and auto-merge
/loom -t 120 --merge   # Merge mode, stop after 2 hours
```

**What the daemon does each iteration:**
1. Captures system snapshot (issues, PRs, labels)
2. Checks for completed shepherds
3. Spawns new shepherds for ready `loom:issue` issues
4. Triggers Architect/Hermit when backlog is low
5. Sleeps until next iteration (default: 120 seconds)

**Stopping the daemon:**
```bash
touch .loom/stop-daemon    # Graceful shutdown (finishes current work)
```

**Configuration (environment variables):**

| Variable | Default | Description |
|----------|---------|-------------|
| `LOOM_POLL_INTERVAL` | 120 | Seconds between iterations |
| `LOOM_MAX_SHEPHERDS` | 3 | Max concurrent shepherds |
| `LOOM_ISSUE_THRESHOLD` | 3 | Trigger work generation below this count |
| `LOOM_ARCHITECT_COOLDOWN` | 1800 | Seconds between architect triggers |
| `LOOM_HERMIT_COOLDOWN` | 1800 | Seconds between hermit triggers |
| `LOOM_ISSUE_STRATEGY` | fifo | Issue selection: fifo, lifo, or priority |

**Merge mode** auto-promotes proposals and auto-merges PRs after Judge approval. It does NOT skip code review - the Judge always runs.

---

### Sub-topic: shepherd

**Shepherd - Single-Issue Orchestration**

The shepherd (`/shepherd <issue>`) orchestrates one issue through its complete lifecycle.

**Usage:**
```bash
/shepherd 123            # Stop after PR is approved
/shepherd 123 --merge    # Full automation including auto-merge
/shepherd 123 --to curated  # Stop after curation
```

**Lifecycle phases:**
```
1. Curator phase   - Enhance issue with implementation guidance
2. Builder phase   - Create worktree, implement, test, create PR
3. Judge phase     - Review PR, approve or request changes
4. Doctor phase    - Fix any requested changes (if needed)
5. Merge phase     - Auto-merge the approved PR (with --merge)
```

The shepherd tracks progress via milestones in `.loom/progress/` and writes checkpoints for crash recovery.

---

### Sub-topic: worktrees

**Git Worktree Workflow**

Loom uses git worktrees to isolate work per issue.

**Creating a worktree:**
```bash
./.loom/scripts/worktree.sh 42       # Creates .loom/worktrees/issue-42
cd .loom/worktrees/issue-42           # Branch: feature/issue-42
```

**Worktree locations:**
- `.loom/worktrees/issue-N` - Per-issue work (Builder creates these)
- `.loom/worktrees/terminal-N` - Per-terminal isolation (Tauri App only)

**Rules:**
- Always use `./.loom/scripts/worktree.sh` (never `git worktree` directly)
- Never delete worktrees manually - use `loom-clean`
- Worktrees auto-clean when PRs are merged

**Cleanup:**
```bash
loom-clean              # Interactive cleanup of stale worktrees
loom-clean --force      # Non-interactive cleanup
loom-clean --deep       # Also remove build artifacts
```

---

### Sub-topic: labels

**Label Reference**

**Workflow labels (issue lifecycle):**

| Label | Meaning | Set by |
|-------|---------|--------|
| `loom:curating` | Curator is actively enhancing | Curator |
| `loom:curated` | Issue enhanced, awaiting approval | Curator |
| `loom:issue` | Approved and ready for work | Champion/Human |
| `loom:building` | Builder is implementing | Builder |
| `loom:blocked` | Work is blocked | Builder |
| `loom:urgent` | Critical priority | Guide/Human |

**Workflow labels (PR lifecycle):**

| Label | Meaning | Set by |
|-------|---------|--------|
| `loom:review-requested` | PR ready for review | Builder |
| `loom:changes-requested` | PR needs fixes | Judge |
| `loom:pr` | PR approved, ready to merge | Judge |
| `loom:auto-merge-ok` | Override size limit for merge | Judge/Human |

**Proposal labels:**

| Label | Meaning | Set by |
|-------|---------|--------|
| `loom:architect` | Architecture proposal | Architect |
| `loom:hermit` | Simplification proposal | Hermit |
| `loom:auditor` | Bug found by Auditor | Auditor |

---

### Sub-topic: troubleshoot

**Troubleshooting**

**Issue stuck in `loom:building`:**
```bash
./.loom/scripts/stale-building-check.sh --recover
```

**Orphaned shepherds after daemon crash:**
```bash
./.loom/scripts/recover-orphaned-shepherds.sh --recover
```

**Labels out of sync:**
```bash
gh label sync --file .github/labels.yml
```

**Stale worktrees/branches:**
```bash
loom-clean --force
```

**Daemon won't start (dual instance):**
```bash
rm -f .loom/daemon-state.json    # Clear stale state
/loom                             # Restart
```

**Stop daemon gracefully:**
```bash
touch .loom/stop-daemon
```

**Check daemon health:**
```bash
/loom health
/loom status
```

**Merge PRs from worktrees (never use `gh pr merge`):**
```bash
./.loom/scripts/merge-pr.sh <PR_NUMBER>
```

**Reference documentation:**
- Daemon details: `/loom-reference`
- Shepherd lifecycle: `/shepherd-lifecycle`
- Full troubleshooting: `.loom/docs/troubleshooting.md`
