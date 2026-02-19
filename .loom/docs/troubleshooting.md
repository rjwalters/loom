# Loom Troubleshooting Guide

## Common Issues

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

### Terminal won't start (Tauri App)

```bash
# Check daemon logs
tail -f ~/.loom/daemon.log

# Check terminal logs
tail -f /tmp/loom-terminal-1.out
```

### Claude Code not found

```bash
# Ensure Claude Code CLI is in PATH
which claude

# Install if missing (see Claude Code documentation)
```

### Shepherd output invisible when invoked with `2>&1`

When `loom-shepherd.sh` is run with `2>&1` redirection (e.g., from Claude Code's Bash tool for long-running processes), output may be silently dropped. This is because the Bash tool's capture buffer can be exhausted by a long-running child process when both stdout and stderr are forced through the same pipe.

**Workaround** — use a file redirect:

```bash
# Redirect to file, then cat the result
./.loom/scripts/loom-shepherd.sh 123 -m > /tmp/shepherd-123.log 2>&1
cat /tmp/shepherd-123.log
```

**Built-in log file** — the shepherd wrapper automatically tees all output to `.loom/logs/loom-shepherd-issue-N.log`. The log path is printed on the very first line:

```
[INFO] Shepherd log: /path/to/.loom/logs/loom-shepherd-issue-123.log
```

If output is invisible in your terminal, check this log file:

```bash
cat .loom/logs/loom-shepherd-issue-123.log
# or follow in real time:
tail -f .loom/logs/loom-shepherd-issue-123.log
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

The Loom daemon automatically detects stuck or struggling agents and can trigger interventions.

### Check for stuck agents

```bash
# Run stuck detection check
loom-stuck-detection check

# Check with JSON output
loom-stuck-detection check --json

# Check specific agent
loom-stuck-detection check-agent shepherd-1
```

### View stuck detection status

```bash
# Show status summary
loom-stuck-detection status

# View intervention history
loom-stuck-detection history
loom-stuck-detection history shepherd-1
```

### Configure stuck detection thresholds

```bash
# Adjust thresholds
loom-stuck-detection configure \
  --idle-threshold 900 \
  --working-threshold 2400 \
  --intervention-mode escalate

# Intervention modes: none, alert, suggest, pause, clarify, escalate
```

### Handle stuck agents

```bash
# Clear intervention for specific agent
loom-stuck-detection clear shepherd-1

# Clear all interventions
loom-stuck-detection clear all

# Resume a paused agent
./.loom/scripts/signal.sh clear shepherd-1
```

### Stuck indicators

| Indicator | Default Threshold | Description |
|-----------|-------------------|-------------|
| `no_progress` | 10 minutes | No output written to task output file |
| `extended_work` | 30 minutes | Working on same issue without creating PR |
| `looping` | 3 occurrences | Repeated similar error patterns |
| `error_spike` | 5 errors | Multiple errors in short period |

### Intervention types

| Type | Trigger | Action |
|------|---------|--------|
| `alert` | Low severity | Write to `.loom/interventions/`, human reviews |
| `suggest` | Medium severity | Suggest role switch (e.g., Builder -> Doctor) |
| `pause` | High severity | Auto-pause via signal.sh, requires manual restart |
| `clarify` | Error spike | Suggest requesting clarification from issue author |
| `escalate` | Critical | Full escalation: pause + alert + human notification |

## Daemon Troubleshooting (Layer 2)

### Check daemon state

```bash
# View current daemon state
cat .loom/daemon-state.json | jq

# Check if daemon is running
jq '.running' .loom/daemon-state.json

# View active shepherds
jq '.shepherds | to_entries[] | select(.value.issue != null)' .loom/daemon-state.json
```

### Graceful shutdown

```bash
# Signal daemon to stop
touch .loom/stop-daemon

# Monitor shutdown progress
watch -n 5 'cat .loom/daemon-state.json | jq ".shepherds"'
```

### Force stop (use with caution)

```bash
# Remove stop signal if exists
rm -f .loom/stop-daemon

# Clear daemon state (will restart fresh)
rm -f .loom/daemon-state.json
```

### Stuck shepherd

```bash
# Check shepherd assignments
jq '.shepherds' .loom/daemon-state.json

# Check if assigned issue is blocked
gh issue view <issue-number> --json labels --jq '.labels[].name'

# Manually clear stuck shepherd (daemon will reassign)
jq '.shepherds["shepherd-1"] = {"issue": null, "idle_since": "'$(date -u +%Y-%m-%dT%H:%M:%SZ)'"}' \
  .loom/daemon-state.json > tmp.json && mv tmp.json .loom/daemon-state.json
```

### Work generation not triggering

When the pipeline is empty but Architect/Hermit are not being triggered, use `loom-tools snapshot` to diagnose:

```bash
# 1. Check pipeline state via loom-tools snapshot (authoritative source)
python3 -m loom_tools.snapshot --pretty | jq '{
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

| Cause | Symptom | Solution |
|-------|---------|----------|
| Cooldown not elapsed | `architect_cooldown_ok: false` | Wait 30 minutes or reset timestamps |
| Proposals at max | 2+ open `loom:architect` or `loom:hermit` issues | Promote or close existing proposals |
| Iteration not acting on recommended_actions | `trigger_architect` in actions but `last_architect_trigger: null` | Bug in iteration - verify loom.md implementation |

**Reset cooldowns manually (for testing):**

```bash
# Reset cooldown timestamps to force immediate trigger
jq '.last_architect_trigger = "2020-01-01T00:00:00Z" | .last_hermit_trigger = "2020-01-01T00:00:00Z"' \
  .loom/daemon-state.json > tmp.json && mv tmp.json .loom/daemon-state.json
```
