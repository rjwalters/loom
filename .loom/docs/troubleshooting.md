# Loom Troubleshooting Guide

## Common Issues

### Hooks not firing (`guard-destructive.sh` not blocking commands)

**Symptom**: Commands that should be blocked or confirmed by `guard-destructive.sh` (e.g., `git reset --hard`, `gh issue close`) are executing without any prompt or denial.

**Root cause**: Claude Code's `--permission-mode bypassPermissions` flag skips ALL PreToolUse hooks entirely. If Claude Code is invoked with this flag, hooks never run — not even safety hooks like `guard-destructive.sh`.

**How to diagnose**:
```bash
# Check if you have a shell alias that sets bypassPermissions
alias claude 2>/dev/null || echo "no alias"

# Check if Loom scripts are using the correct flag
grep -r 'permission-mode' .loom/scripts/ .loom/roles/ 2>/dev/null
```

**The two flags behave differently**:

| Flag | Hooks fire? | Use case |
|------|-------------|----------|
| `--dangerously-skip-permissions` | ✅ YES | Loom automation (agents use this) |
| `--permission-mode bypassPermissions` | ❌ NO | Fully bypasses all permission checks AND hooks |

**Fix**: If you have a shell alias using `--permission-mode bypassPermissions`, change it to use `--dangerously-skip-permissions` instead:

```bash
# WRONG - hooks silently disabled:
alias claude="claude --permission-mode bypassPermissions"

# CORRECT - hooks still fire:
alias claude="claude --dangerously-skip-permissions"
```

Note: `--dangerously-skip-permissions` still skips interactive permission prompts (so agents can run non-interactively), but hooks are executed. This is the intended mode for Loom agents.

**Verify the fix**: After updating your alias, restart your shell and confirm hooks fire by checking the hook error log:
```bash
# Hook invocations log errors here:
cat .loom/logs/hook-errors.log
```

If the log is absent or empty and hooks aren't blocking, confirm Claude Code is invoked with `--dangerously-skip-permissions` (not `bypassPermissions`).

### Cleaning Up Stale Worktrees and Branches

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

### Labels out of sync

```bash
# Re-sync labels from configuration
gh label sync --file .github/labels.yml
```

Label sync is a manual/install-time step (`./scripts/install/sync-labels.sh .`),
not something CI re-applies when `.github/labels.yml` changes. If a label is
defined in `labels.yml` but missing from the live repo, applying it fails with
`failed to update 1 issue` (the standard `gh` error for "label does not exist").
Run the sync script — or create the one label directly — to reconcile:

```bash
gh label list --search operator                      # empty => not provisioned
gh label create "loom:operator-only" --color F97316 \
  --description "Requires human action outside automation (credentials, infra, hardware); sweep/shepherd skip"
```

**GitHub caps label descriptions at 100 characters.** A `labels.yml` entry with a
longer description fails to sync (HTTP 422 "description is too long") and the label
silently never gets created. Keep descriptions at or under 100 chars.

#### `loom:blocked` vs `loom:operator-only`

These two status labels look similar but mean different things to the automation:

- **`loom:blocked`** — work is *automatable* but currently waiting on a dependency
  (another issue, an unmerged PR, missing context). The intent is "unblock it, then
  a Builder can proceed."
- **`loom:operator-only`** — work requires a *human to act outside automation
  entirely* (rotating credentials, infra changes, hardware access, manual deploys).
  Sweep/shepherd skip these in pre-flight rather than attempting them; a human must
  do the work off-automation before the issue can proceed.

Reaching for `loom:blocked` when you mean `loom:operator-only` conflates "waiting on
a dependency" with "needs a human action," which muddies the daemon/sweep skip
semantics. Use `loom:operator-only` for the human-must-act-off-automation case.

### Daemon won't start

```bash
# Check daemon logs
tail -f ~/.loom/daemon.log
```

### Claude Code not found

```bash
# Ensure Claude Code CLI is in PATH
which claude

# Install if missing (see Claude Code documentation)
```

### Sweep output invisible when invoked with `2>&1`

When `claude -p "/loom:sweep N"` is run with `2>&1` redirection (e.g., from Claude Code's Bash tool for long-running processes), output may be silently dropped. This is because the Bash tool's capture buffer can be exhausted by a long-running child process when both stdout and stderr are forced through the same pipe.

**Workaround** — use a file redirect:

```bash
# Redirect to file, then cat the result
claude -p "/loom:sweep 123" --dangerously-skip-permissions > /tmp/sweep-123.log 2>&1
cat /tmp/sweep-123.log
```

**Built-in log file** — when the spawn loop spawns a sweep child, it automatically tees all output to `.loom/logs/sweep-issue-N.log`. If output is invisible in your terminal, check this log file:

```bash
cat .loom/logs/sweep-issue-123.log
# or follow in real time:
tail -f .loom/logs/sweep-issue-123.log
```

### API Error: 400 due to tool use concurrency issues

This error occurs when Claude Code's parallel tool execution causes malformed API message structures. See the dedicated guide: [Tool Use Concurrency Errors](./tool-use-concurrency-errors.md)

**Quick recovery**:
```bash
# In Claude Code, run:
/rewind
```

**Prevention** - Add to `~/.claude/CLAUDE.md`:
```markdown
# Force Sequential Tool Execution
Execute tools sequentially, never in parallel.
Process one tool call at a time.
Wait for tool_result before initiating next tool execution.
```

**Common triggers**:
- Multiple parallel file operations (Read, Write, Edit)
- Using print mode (`-p` flag) instead of interactive mode
- PostToolUse hooks that interfere with message structure
- Editing files while they're open in an IDE

### Orphaned issues stuck in loom:building state

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

## Stuck Agent Detection

`loom-stuck-detection` (post-v0.10.0) checks for stuck sweep children using `.loom/spawn-loop-state.json` task pids and `.loom/sweep-checkpoint/issue-<N>.json` checkpoint timestamps.

### Check for stuck agents

```bash
# Run stuck detection check
loom-stuck-detection check

# Check with JSON output
loom-stuck-detection check --json

# Check a specific issue
loom-stuck-detection check-issue 123
```

### Stuck indicators (post-v0.10.0)

| Indicator | Default Threshold | Description |
|-----------|-------------------|-------------|
| `stale_heartbeat` | 5 minutes | No checkpoint update for extended time |
| `dead_pid` | (instant) | PID in spawn-loop-state.json is no longer alive |
| `error_spike` | 5 errors | Multiple errors in `.loom/logs/sweep-issue-N.log` |

The pre-v0.10.0 indicators `missing_milestone:worktree_created` and `extended_work` were retired when the Python daemon brain (`daemon_v2/`) was removed — see [the migration guide § Per-CLI breaking changes](../../docs/migration/v0.10.0-shepherd-deprecation.md#per-cli-breaking-changes) for the field-level diff. The shell-level daemon surface (`./.loom/scripts/daemon.sh`) is preserved but does not write progress files, so milestone-based heuristics no longer apply.

## Spawn-Loop Troubleshooting

The spawn loop replaces the historical daemon brain; orchestration state lives at `.loom/spawn-loop-state.json`.

### Check spawn-loop state

```bash
# View current spawn-loop state
./.loom/scripts/spawn-loop.sh status

# Or read the state file directly
cat .loom/spawn-loop-state.json | jq

# Check if loop is running
test -f .loom/spawn-loop.pid && ps -p "$(cat .loom/spawn-loop.pid)" -o pid,etime,command

# List active sweep children
jq '.running[] | {issue, pid, started_at}' .loom/spawn-loop-state.json
```

### Graceful shutdown

```bash
# Signal the spawn loop to stop accepting new work and drain in-flight children
./.loom/scripts/spawn-loop.sh stop
# or, equivalently:
touch .loom/stop-spawn-loop
```

The loop honors `SHUTDOWN_GRACE_SEC` (default 300s) before SIGKILL'ing any remaining sweep children.

### Force stop (use with caution)

```bash
# Remove stop signal if it was set but never picked up
rm -f .loom/stop-spawn-loop

# Hard-kill the loop process
test -f .loom/spawn-loop.pid && kill -9 "$(cat .loom/spawn-loop.pid)" || true
rm -f .loom/spawn-loop.pid
```

### Stuck sweep child

A sweep child whose pid is alive but whose `.loom/sweep-checkpoint/issue-<N>.json` mtime is stale is likely stuck. To recover:

```bash
# Check checkpoint mtime
ls -la .loom/sweep-checkpoint/issue-123.json

# Look at the child's log for errors
tail -200 .loom/logs/sweep-issue-123.log

# If you need to kill it manually:
jq '.running[] | select(.issue==123) | .pid' .loom/spawn-loop-state.json | xargs -I{} kill {}

# The loop will detect the dead pid on the next tick and release the claim
# (the checkpoint survives, so the issue will resume from its last completed phase
# the next time the loop spawns it)
```

### Spawn-loop is not picking up issues

Issues need the `loom:issue` label (human-approved, ready for work) to be eligible. If the queue looks empty but the loop is idle, check:

```bash
# 1. Confirm there are ready issues
gh issue list --label "loom:issue" --state open

# 2. Confirm the claim locks aren't stale (a previous crash may have left lock dirs)
ls -la .loom/locks/

# 3. Confirm the opt-in gate is set
echo "LOOM_USE_SPAWN_LOOP=$LOOM_USE_SPAWN_LOOP"

# 4. Look at recent loop activity
tail -100 .loom/logs/spawn-loop.log
```

If `.loom/locks/issue-<N>/` exists for a closed/merged issue, remove it manually — the next tick will then claim that slot if a new ready issue lands.

### Work generation (Architect / Hermit) not running

**This is by design post-v0.10.0.** The spawn loop does not generate work — Architect and Hermit cadence is tracked under follow-up #3381. If you need new work generated automatically, run Architect/Hermit on a cron via the Phase 2a GitHub Actions pattern (`.github/workflows/loom-*.yml`); the existing five shipped workflows cover Champion / Curator / Judge / Auditor / Guide, but Architect and Hermit cron workflows are not yet shipped.

For now, trigger them manually when the queue is empty:

```bash
claude -p "/architect" --dangerously-skip-permissions
claude -p "/hermit"    --dangerously-skip-permissions
```
