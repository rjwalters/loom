# Loom

Assume the Loom Daemon role and run the **continuous** Layer 2 system orchestrator.

## Process

1. **Read the role definition**: Load `.loom/roles/loom.md` or `defaults/roles/loom.md`
2. **Initialize state**: Load or create `.loom/daemon-state.json`
3. **Run thin parent loop**: Spawn iteration subagents that do the actual work
4. **Run until cancelled**: Continue until Ctrl+C or stop signal

## Two-Tier Architecture (Context Management)

The daemon uses a **subagent-per-iteration** architecture to prevent context accumulation:

```
┌────────────────────────────────────────────┐
│  Parent Loop (stays minimal)               │
│  - Check shutdown signal                   │
│  - Spawn iteration subagent                │
│  - Receive 1-line summary                  │
│  - Sleep(POLL_INTERVAL)                    │
│  - Repeat                                  │
└──────────────────┬─────────────────────────┘
                   │ spawns (blocking)
                   ▼
┌────────────────────────────────────────────┐
│  Iteration Subagent (fresh context)        │
│  1. Read .loom/daemon-state.json           │
│  2. Assess system (gh commands)            │
│  3. Check completions (TaskOutput)         │
│  4. Spawn shepherds (Task, background)     │
│  5. Spawn work generation                  │
│  6. Ensure support roles                   │
│  7. Save state to JSON                     │
│  8. Return 1-line summary                  │
└────────────────────────────────────────────┘
```

**Why this architecture?**
- Parent accumulates only ~100 bytes per iteration (summaries)
- Iteration subagent gets fresh context each time
- Can run for hours/days without context compaction issues
- State continuity maintained via JSON file

## Work Scope

As the **Loom Daemon** (Layer 2), you **continuously** orchestrate the system:

- **Run thin parent loop**: Spawns iteration subagents, sleeps, repeats
- **Iteration subagents do all work**: Assess state, spawn shepherds, trigger work generation
- **Track state** in `.loom/daemon-state.json` for crash recovery

You don't do the orchestration work directly - you spawn iteration subagents that handle each iteration with fresh context.

## Usage

```
/loom                     # Start continuous daemon (runs until cancelled)
/loom --force             # Start with force mode (auto-promotes proposals)
/loom iterate             # Execute single iteration (used by parent loop)
/loom iterate --force     # Single iteration with force mode
/loom status              # Report current system state only
/loom spawn 123           # Manually spawn shepherd for issue #123
/loom stop                # Create stop signal for graceful shutdown
```

## Commands

| Command | Description |
|---------|-------------|
| (none) | Start thin parent loop (spawns iteration subagents) |
| `--force` | Enable force mode (Champion auto-promotes proposals) |
| `iterate` | Execute single daemon iteration (returns summary) |
| `status` | Report system state without starting loop (see Status Command below) |
| `spawn <issue>` | Manually spawn a shepherd subagent for specific issue |
| `stop` | Create `.loom/stop-daemon` for graceful shutdown |

## Status Command

The `status` command is a **read-only observation interface** for Layer 3 (human observer). It displays the current system state without taking any action.

**To run status**, execute the helper script:

```bash
# Display formatted status
./.loom/scripts/loom-status.sh

# Get status as JSON for scripting
./.loom/scripts/loom-status.sh --json
```

**Status shows**:
- Daemon status (running/stopped, uptime)
- System state (issue counts by label)
- Shepherd pool status (active/idle, assigned issues)
- Support role status (Architect, Hermit, Guide, Champion)
- Session statistics (completed issues, PRs merged)
- Available Layer 3 interventions

**Important**: `/loom status` is for Layer 3 observation - it never modifies state.
- `/loom` = Run the daemon (Layer 2 executor role)
- `/loom status` = Observe the system (Layer 3 observer role)

## Parent Loop (Thin)

The parent loop runs **continuously** until cancelled, but does minimal work:

```
iteration = 0
while not cancelled:
    iteration += 1

    # 1. Check for shutdown signal
    if exists(".loom/stop-daemon"):
        graceful_shutdown()
        break

    # 2. Spawn iteration subagent (does ALL the real work)
    force_flag = "--force" if force_mode else ""
    result = Task(
        description=f"Daemon iteration {iteration}",
        prompt=f"/loom iterate {force_flag}",
        subagent_type="general-purpose",
        run_in_background=False,  # Wait for completion
        model="haiku"  # Fast, cheap for iteration work
    )

    # 3. Result is just a summary line (~50-100 chars)
    print(f"Iteration {iteration}: {result}")

    # 4. Check for shutdown after iteration
    if "SHUTDOWN_SIGNAL" in result:
        break

    # 5. Sleep and repeat
    sleep(POLL_INTERVAL)
```

**Key points:**
- Parent only accumulates short summary strings
- All gh commands, TaskOutput checks, and subagent spawning happens in iteration subagent
- Iteration subagent context is discarded after each iteration
- State continuity via `.loom/daemon-state.json`

## Iteration Subagent

When `/loom iterate` is invoked, execute exactly ONE iteration:

```
1. Load .loom/daemon-state.json
2. Check shutdown signal → return "SHUTDOWN_SIGNAL" if found
3. Assess system state (gh commands)
4. Check subagent completions (non-blocking TaskOutput)
5. Auto-spawn shepherds (background Task calls)
6. Auto-trigger work generation (Architect/Hermit)
7. Auto-ensure support roles (Guide, Champion, Doctor)
8. Check stuck agents
9. Save state to JSON
10. Return compact summary (one line)
```

The iteration subagent returns a summary like:
```
ready=5 building=2 +shepherd=#123 +architect
```

## Spawning Shepherds

Use the Task tool with `run_in_background: true` to spawn parallel shepherds:

```python
Task(
    description="Shepherd issue #123",
    prompt="/shepherd 123 --force-merge",
    subagent_type="general-purpose",
    run_in_background=True
)
```

This enables up to 3 shepherds running concurrently.

## Configuration

| Parameter | Default | Description |
|-----------|---------|-------------|
| `MAX_SHEPHERDS` | 3 | Maximum concurrent shepherd subagents |
| `POLL_INTERVAL` | 30s | Time between status checks |

## Status Report

```
═══════════════════════════════════════════════════
  LOOM DAEMON STATUS
═══════════════════════════════════════════════════
  Status: Running
  Uptime: 2h 15m

  System State:
    Ready issues (loom:issue): 5
    Building (loom:building): 2
    PRs pending review: 1

  Shepherds: 2/3 active
    shepherd-1: Issue #123 (running 45m)
    shepherd-2: Issue #456 (running 12m)
    shepherd-3: idle

  Session Stats:
    Issues completed: 3
    PRs merged: 3
═══════════════════════════════════════════════════
```

## State Persistence

State tracked in `.loom/daemon-state.json`:

```json
{
  "started_at": "2026-01-23T10:00:00Z",
  "running": true,
  "shepherds": {
    "shepherd-1": {
      "issue": 123,
      "task_id": "abc123",
      "output_file": "/path/to/output"
    }
  },
  "completed_issues": [100, 101, 102]
}
```

## Graceful Shutdown

```bash
# Option 1: Create stop signal
touch .loom/stop-daemon

# Option 2: Use command
/loom stop

# Daemon will:
# 1. Stop spawning new shepherds
# 2. Wait for active shepherds to complete (max 5 min)
# 3. Clean up state
# 4. Exit
```

## Layer 1 vs Layer 2

| Layer | Role | Purpose |
|-------|------|---------|
| Layer 1 | **Shepherd** (`/shepherd 123`) | Orchestrates single issue lifecycle |
| Layer 2 | **Loom Daemon** (`/loom`) | Spawns shepherds, manages pool continuously |

Use `/shepherd` to shepherd a specific issue. Use `/loom` to run the continuous orchestrator that manages multiple shepherds.

ARGUMENTS: $ARGUMENTS
