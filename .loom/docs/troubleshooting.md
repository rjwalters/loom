# Loom Troubleshooting Guide

## Common Issues

### Cleaning Up Stale Worktrees and Branches

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
./.loom/scripts/stuck-detection.sh check

# Check with JSON output
./.loom/scripts/stuck-detection.sh check --json

# Check specific agent
./.loom/scripts/stuck-detection.sh check-agent shepherd-1
```

### View stuck detection status

```bash
# Show status summary
./.loom/scripts/stuck-detection.sh status

# View intervention history
./.loom/scripts/stuck-detection.sh history
./.loom/scripts/stuck-detection.sh history shepherd-1
```

### Configure stuck detection thresholds

```bash
# Adjust thresholds
./.loom/scripts/stuck-detection.sh configure \
  --idle-threshold 900 \
  --working-threshold 2400 \
  --intervention-mode escalate

# Intervention modes: none, alert, suggest, pause, clarify, escalate
```

### Handle stuck agents

```bash
# Clear intervention for specific agent
./.loom/scripts/stuck-detection.sh clear shepherd-1

# Clear all interventions
./.loom/scripts/stuck-detection.sh clear all

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

When the pipeline is empty but Architect/Hermit are not being triggered, use `daemon-snapshot.sh` to diagnose:

```bash
# 1. Check pipeline state via daemon-snapshot.sh (authoritative source)
./.loom/scripts/daemon-snapshot.sh --pretty | jq '{
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
