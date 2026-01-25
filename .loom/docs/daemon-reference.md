# Loom Daemon Reference

Detailed configuration and state management for the Loom daemon (Layer 2).

## Daemon Configuration Parameters

| Parameter | Default | Description |
|-----------|---------|-------------|
| `ISSUE_THRESHOLD` | 3 | Trigger Architect/Hermit when `loom:issue` count below this |
| `MAX_PROPOSALS` | 5 | Maximum pending proposal issues |
| `MAX_SHEPHERDS` | 3 | Maximum concurrent shepherd processes |
| `ISSUES_PER_SHEPHERD` | 2 | Scale factor: target = ready_issues / ISSUES_PER_SHEPHERD |
| `POLL_INTERVAL` | 60 | Seconds between daemon loop iterations |
| `ISSUE_STRATEGY` | fifo | Issue selection strategy (see below) |

## Issue Selection Strategy

Set via `LOOM_ISSUE_STRATEGY` environment variable. Controls the order in which shepherds pick up issues from the ready queue. The `loom:urgent` label always takes precedence regardless of strategy.

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

## Daemon State File

The daemon state file (`.loom/daemon-state.json`) provides comprehensive information for debugging, crash recovery, and system observability.

### Full State Structure

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

### Shepherd Status Values

- `working` - Actively processing an issue
- `idle` - No issue assigned, waiting for work
- `errored` - Encountered an error, may need intervention
- `paused` - Manually paused via signal or stuck detection

### Idle Reasons

- `no_ready_issues` - No issues with `loom:issue` label available
- `at_capacity` - All shepherd slots filled
- `completed_issue` - Just finished an issue, waiting for next
- `rate_limited` - Paused due to API rate limits
- `shutdown_signal` - Paused due to graceful shutdown

### Warning Types

- `blocked_pr` - PR has merge conflicts or failed checks
- `shepherd_error` - Shepherd encountered recoverable error
- `role_failure` - Support role failed to complete
- `stuck_agent` - Agent detected as stuck

## Session Rotation

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

## Required Terminal Configuration

| Terminal ID | Role | Purpose |
|-------------|------|---------|
| shepherd-1, shepherd-2, shepherd-3 | shepherd.md | Issue orchestration pool |
| terminal-architect | architect.md | Work generation (proposals) |
| terminal-hermit | hermit.md | Simplification proposals |
| terminal-guide | guide.md | Backlog triage (always running) |
| terminal-champion | champion.md | Auto-merge (always running) |
| terminal-doctor | doctor.md | PR conflict resolution (always running) |
