# Shepherd Lifecycle Reference

This document contains detailed workflow implementation for the Shepherd role. For core role definition, principles, and phase flow overview, see `shepherd.md`.

## Graceful Shutdown - Integrated into Waits

Shutdown signal checking is integrated into `agent-wait-bg.sh`, which polls for signals during phase waits. This replaces the previous approach of checking only at phase boundaries.

### How It Works

`agent-wait-bg.sh` runs `agent-wait.sh` in the background and polls for shutdown signals every poll interval. When a signal is detected, it kills the background wait, and returns exit code 3.

### Signals Checked

| Signal | Scope | Detection |
|--------|-------|-----------|
| `.loom/stop-shepherds` file | All shepherds | File existence check |
| `loom:abort` label | Single issue | GitHub label check (requires `--issue`) |

### Handling Exit Code 3

When `agent-wait-bg.sh` returns exit code 3, the shepherd should clean up and exit:

```bash
# Wait with signal checking
./.loom/scripts/agent-wait-bg.sh "${ROLE}-issue-${ISSUE_NUMBER}" --timeout 900 --issue "$ISSUE_NUMBER"
WAIT_EXIT=$?

if [ "$WAIT_EXIT" -eq 3 ]; then
    echo "Shutdown signal detected during ${ROLE} phase"
    ./.loom/scripts/agent-destroy.sh "${ROLE}-issue-${ISSUE_NUMBER}"

    # Revert issue label so it can be picked up again
    LABELS=$(gh issue view $ISSUE_NUMBER --json labels --jq '.labels[].name')
    if echo "$LABELS" | grep -q "loom:building"; then
        gh issue edit $ISSUE_NUMBER --remove-label "loom:building" --add-label "loom:issue"
    fi
    if echo "$LABELS" | grep -q "loom:abort"; then
        gh issue edit $ISSUE_NUMBER --remove-label "loom:abort"
        gh issue comment $ISSUE_NUMBER --body "**Shepherd aborted** per \`loom:abort\` label. Issue returned to \`loom:issue\` state."
    else
        gh issue comment $ISSUE_NUMBER --body "**Shepherd graceful shutdown** - orchestration paused during ${ROLE} phase. Issue returned to \`loom:issue\` state."
    fi
    exit 0
fi
```

### Behavior Summary

| Signal Detected | When | Action |
|-----------------|------|--------|
| `.loom/stop-shepherds` | During wait | Kill worker wait, clean up, revert labels, exit |
| `loom:abort` label | During wait | Kill worker wait, clean up, revert labels, exit |
| No signal | Wait completes | Continue to label verification |

## tmux Worker Execution - Detailed Examples

### Phase-Specific Worker Execution

**Curator Phase:**
```bash
# Spawn curator worker in ephemeral tmux session
./.loom/scripts/agent-spawn.sh --role curator --name "curator-issue-${ISSUE}" --args "$ISSUE" --on-demand
./.loom/scripts/agent-wait-bg.sh "curator-issue-${ISSUE}" --timeout 600 --issue "$ISSUE"
WAIT_EXIT=$?
[ "$WAIT_EXIT" -eq 3 ] && { ./.loom/scripts/agent-destroy.sh "curator-issue-${ISSUE}"; handle_shutdown; }

# Verify completion by checking labels
LABELS=$(gh issue view $ISSUE --json labels --jq '.labels[].name')
echo "$LABELS" | grep -q "loom:curated" || echo "$LABELS" | grep -q "loom:issue"

# Clean up
./.loom/scripts/agent-destroy.sh "curator-issue-${ISSUE}"
```

**Builder Phase:**
```bash
# Spawn builder worker with worktree isolation
./.loom/scripts/agent-spawn.sh --role builder --name "builder-issue-${ISSUE}" --args "$ISSUE" \
    --worktree ".loom/worktrees/issue-${ISSUE}" --on-demand
./.loom/scripts/agent-wait-bg.sh "builder-issue-${ISSUE}" --timeout 1800 --issue "$ISSUE"
WAIT_EXIT=$?
[ "$WAIT_EXIT" -eq 3 ] && { ./.loom/scripts/agent-destroy.sh "builder-issue-${ISSUE}"; handle_shutdown; }

# Verify completion by checking for PR
PR_NUMBER=$(gh pr list --search "Closes #${ISSUE}" --json number --jq '.[0].number')

# Clean up (worktree stays for judge/doctor phases)
./.loom/scripts/agent-destroy.sh "builder-issue-${ISSUE}"
```

**Judge Phase:**
```bash
# Spawn judge worker
./.loom/scripts/agent-spawn.sh --role judge --name "judge-issue-${ISSUE}" --args "$PR_NUMBER" --on-demand
./.loom/scripts/agent-wait-bg.sh "judge-issue-${ISSUE}" --timeout 900 --issue "$ISSUE"
WAIT_EXIT=$?
[ "$WAIT_EXIT" -eq 3 ] && { ./.loom/scripts/agent-destroy.sh "judge-issue-${ISSUE}"; handle_shutdown; }

# Verify completion by checking PR labels
LABELS=$(gh pr view $PR_NUMBER --json labels --jq '.labels[].name')
if echo "$LABELS" | grep -q "loom:pr"; then
    PHASE="gate2"  # Approved
elif echo "$LABELS" | grep -q "loom:changes-requested"; then
    PHASE="doctor"  # Needs fixes
fi

# Clean up
./.loom/scripts/agent-destroy.sh "judge-issue-${ISSUE}"
```

**Doctor Phase:**
```bash
# Spawn doctor worker
./.loom/scripts/agent-spawn.sh --role doctor --name "doctor-issue-${ISSUE}" --args "$PR_NUMBER" --on-demand
./.loom/scripts/agent-wait-bg.sh "doctor-issue-${ISSUE}" --timeout 900 --issue "$ISSUE"
WAIT_EXIT=$?
[ "$WAIT_EXIT" -eq 3 ] && { ./.loom/scripts/agent-destroy.sh "doctor-issue-${ISSUE}"; handle_shutdown; }

# Verify completion by checking for review-requested label
LABELS=$(gh pr view $PR_NUMBER --json labels --jq '.labels[].name')
echo "$LABELS" | grep -q "loom:review-requested"

# Clean up
./.loom/scripts/agent-destroy.sh "doctor-issue-${ISSUE}"
```

### Complete Orchestration Example

```bash
# Shepherd orchestrating issue #123
ISSUE=123

# Helper for shutdown signal handling
handle_shutdown() {
    LABELS=$(gh issue view $ISSUE --json labels --jq '.labels[].name')
    if echo "$LABELS" | grep -q "loom:building"; then
        gh issue edit $ISSUE --remove-label "loom:building" --add-label "loom:issue"
    fi
    echo "Shutdown signal detected - exiting gracefully"
    exit 0
}

# Phase 1: Curator
echo "Starting Curator phase for issue #${ISSUE}..."
./.loom/scripts/agent-spawn.sh --role curator --name "curator-issue-${ISSUE}" --args "$ISSUE" --on-demand
./.loom/scripts/agent-wait-bg.sh "curator-issue-${ISSUE}" --timeout 600 --issue "$ISSUE"
WAIT_EXIT=$?
[ "$WAIT_EXIT" -eq 3 ] && { ./.loom/scripts/agent-destroy.sh "curator-issue-${ISSUE}"; handle_shutdown; }
./.loom/scripts/agent-destroy.sh "curator-issue-${ISSUE}"
echo "Curator phase complete"

# Gate 1: Wait for approval (or auto-approve in force mode)
if [ "$FORCE_MODE" = "true" ]; then
    gh issue edit $ISSUE --add-label "loom:issue"
fi

# Phase 2: Builder
echo "Starting Builder phase for issue #${ISSUE}..."
./.loom/scripts/agent-spawn.sh --role builder --name "builder-issue-${ISSUE}" --args "$ISSUE" --on-demand
./.loom/scripts/agent-wait-bg.sh "builder-issue-${ISSUE}" --timeout 1800 --issue "$ISSUE"
WAIT_EXIT=$?
[ "$WAIT_EXIT" -eq 3 ] && { ./.loom/scripts/agent-destroy.sh "builder-issue-${ISSUE}"; handle_shutdown; }
PR_NUMBER=$(gh pr list --search "Closes #${ISSUE}" --json number --jq '.[0].number')
./.loom/scripts/agent-destroy.sh "builder-issue-${ISSUE}"
echo "Builder phase complete - PR #${PR_NUMBER} created"

# Phase 3: Judge
echo "Starting Judge phase for PR #${PR_NUMBER}..."
./.loom/scripts/agent-spawn.sh --role judge --name "judge-issue-${ISSUE}" --args "$PR_NUMBER" --on-demand
./.loom/scripts/agent-wait-bg.sh "judge-issue-${ISSUE}" --timeout 900 --issue "$ISSUE"
WAIT_EXIT=$?
[ "$WAIT_EXIT" -eq 3 ] && { ./.loom/scripts/agent-destroy.sh "judge-issue-${ISSUE}"; handle_shutdown; }
./.loom/scripts/agent-destroy.sh "judge-issue-${ISSUE}"
echo "Judge phase complete"

# Continue with Doctor loop and merge as needed...
```

### Observability

All worker sessions are attachable for live observation:
```bash
# Watch builder working on issue 42
tmux -L loom attach -t loom-builder-issue-42

# List all active worker sessions
tmux -L loom list-sessions
```

## Waiting for Completion

After spawning a worker with `agent-spawn.sh`, use `agent-wait-bg.sh` to wait for completion while checking shutdown signals. The script runs `agent-wait.sh` in the background and polls for `stop-shepherds` file and `loom:abort` label during the wait.

Exit codes from `agent-wait-bg.sh`:
- **0**: Agent completed normally
- **1**: Timeout reached
- **2**: Session not found
- **3**: Shutdown signal detected (clean up and exit)

After `agent-wait-bg.sh` returns with exit code 0, always verify success by checking labels — the worker may have encountered issues:

### Label Verification

```bash
# After curator phase
LABELS=$(gh issue view $ISSUE --json labels --jq '.labels[].name')
if echo "$LABELS" | grep -q "loom:curated"; then
  echo "Curator phase complete"
elif echo "$LABELS" | grep -q "loom:blocked"; then
  echo "Issue is blocked"
  exit 1
fi
```

### PR Label Verification

For PR-related phases:

```bash
# Find PR for issue
PR_NUMBER=$(gh pr list --search "Closes #$ISSUE" --json number --jq '.[0].number')

# Check PR labels
LABELS=$(gh pr view $PR_NUMBER --json labels --jq '.labels[].name')
if echo "$LABELS" | grep -q "loom:pr"; then
  echo "PR approved, ready for merge"
elif echo "$LABELS" | grep -q "loom:changes-requested"; then
  echo "Changes requested, triggering Doctor"
fi
```

## State Tracking

### Progress Comments

Track progress in issue comments for crash recovery:

```bash
# Add progress comment with hidden state
gh issue comment <number> --body "$(cat <<'EOF'
## Loom Orchestration Progress

| Phase | Status | Timestamp |
|-------|--------|-----------|
| Curator | Complete | 2025-01-23T10:00:00Z |
| Builder | In Progress | 2025-01-23T10:05:00Z |
| Judge | Pending | - |
| Doctor | Pending | - |
| Merge | Pending | - |

<!-- loom:orchestrator
{"phase":"builder","iteration":0,"pr":null,"started":"2025-01-23T10:05:00Z"}
-->
EOF
)"
```

### Resuming on Restart

When `/shepherd <number>` is invoked, check for existing progress:

```bash
# Read issue comments for existing state
STATE=$(gh issue view <number> --comments --json body \
  --jq '.comments[].body | capture("<!-- loom:orchestrator\\n(?<json>.*)\\n-->"; "m") | .json')

if [ -n "$STATE" ]; then
  PHASE=$(echo "$STATE" | jq -r '.phase')
  echo "Resuming from phase: $PHASE"
else
  echo "Starting fresh orchestration"
fi
```

## Full Orchestration Workflow

### Step 1: Check State

```bash
# Analyze issue state
LABELS=$(gh issue view <number> --json labels --jq '.labels[].name')

# Determine starting phase
# IMPORTANT: Always ensure curation happens before building
if echo "$LABELS" | grep -q "loom:building"; then
  PHASE="builder"  # Already claimed, skip to monitoring
elif echo "$LABELS" | grep -q "loom:curated"; then
  # Issue has been curated
  if echo "$LABELS" | grep -q "loom:issue"; then
    PHASE="builder"  # Curated AND approved - ready for building
  else
    PHASE="gate1"    # Curated but waiting for approval
  fi
else
  # Issue has NOT been curated - always run curator first
  # Even if loom:issue is present, curation ensures quality
  PHASE="curator"
fi
```

### Step 2: Curator Phase

```bash
if [ "$PHASE" = "curator" ]; then
  # Spawn ephemeral curator worker
  ./.loom/scripts/agent-spawn.sh --role curator --name "curator-issue-${ISSUE_NUMBER}" --args "$ISSUE_NUMBER" --on-demand
  ./.loom/scripts/agent-wait-bg.sh "curator-issue-${ISSUE_NUMBER}" --timeout 600 --issue "$ISSUE_NUMBER"
  WAIT_EXIT=$?
  [ "$WAIT_EXIT" -eq 3 ] && { ./.loom/scripts/agent-destroy.sh "curator-issue-${ISSUE_NUMBER}"; handle_shutdown; }
  ./.loom/scripts/agent-destroy.sh "curator-issue-${ISSUE_NUMBER}"

  # Verify completion
  LABELS=$(gh issue view $ISSUE_NUMBER --json labels --jq '.labels[].name')
  if ! echo "$LABELS" | grep -q "loom:curated\|loom:issue"; then
    echo "Curator did not complete successfully"
    exit 1
  fi

  # Update progress
  update_progress "curator" "complete"
fi
```

### Step 3: Gate 1 - Approval

```bash
if [ "$PHASE" = "gate1" ]; then
  # Check if --force-pr or --force-merge mode - auto-approve
  if [ "$FORCE_PR" = "true" ] || [ "$FORCE_MERGE" = "true" ]; then
    echo "Force mode: auto-approving issue"
    gh issue edit $ISSUE_NUMBER --add-label "loom:issue"
    gh issue comment $ISSUE_NUMBER --body "**Auto-approved** via \`/shepherd --force-pr\` or \`--force-merge\`"
  else
    # Wait for human or Champion to promote to loom:issue
    TIMEOUT=1800  # 30 minutes
    START=$(date +%s)

    while true; do
      LABELS=$(gh issue view $ISSUE_NUMBER --json labels --jq '.labels[].name')
      if echo "$LABELS" | grep -q "loom:issue"; then
        echo "Issue approved for implementation"
        break
      fi

      NOW=$(date +%s)
      if [ $((NOW - START)) -gt $TIMEOUT ]; then
        echo "Timeout waiting for approval"
        gh issue comment $ISSUE_NUMBER --body "Orchestration paused: waiting for approval (loom:issue label)"
        exit 0
      fi

      sleep 30
    done
  fi
fi
```

### Step 4: Builder Phase

```bash
if [ "$PHASE" = "builder" ]; then
  # Spawn ephemeral builder worker
  ./.loom/scripts/agent-spawn.sh --role builder --name "builder-issue-${ISSUE_NUMBER}" --args "$ISSUE_NUMBER" --on-demand
  ./.loom/scripts/agent-wait-bg.sh "builder-issue-${ISSUE_NUMBER}" --timeout 1800 --issue "$ISSUE_NUMBER"
  WAIT_EXIT=$?
  [ "$WAIT_EXIT" -eq 3 ] && { ./.loom/scripts/agent-destroy.sh "builder-issue-${ISSUE_NUMBER}"; handle_shutdown; }
  ./.loom/scripts/agent-destroy.sh "builder-issue-${ISSUE_NUMBER}"

  # Find the PR
  PR_NUMBER=$(gh pr list --search "Closes #$ISSUE_NUMBER" --state open --json number --jq '.[0].number')
  if [ -z "$PR_NUMBER" ]; then
    echo "Builder did not create a PR"
    exit 1
  fi
  echo "PR #$PR_NUMBER created"

  update_progress "builder" "complete" "$PR_NUMBER"
fi
```

### Step 5: Judge Phase

```bash
if [ "$PHASE" = "judge" ]; then
  # Spawn ephemeral judge worker
  ./.loom/scripts/agent-spawn.sh --role judge --name "judge-issue-${ISSUE_NUMBER}" --args "$PR_NUMBER" --on-demand
  ./.loom/scripts/agent-wait-bg.sh "judge-issue-${ISSUE_NUMBER}" --timeout 900 --issue "$ISSUE_NUMBER"
  WAIT_EXIT=$?
  [ "$WAIT_EXIT" -eq 3 ] && { ./.loom/scripts/agent-destroy.sh "judge-issue-${ISSUE_NUMBER}"; handle_shutdown; }
  ./.loom/scripts/agent-destroy.sh "judge-issue-${ISSUE_NUMBER}"

  # Check review result
  # Note: Judge uses label-based reviews (comment + label change), not GitHub's
  # review API, so self-approval is not a problem. See judge.md for details.
  LABELS=$(gh pr view $PR_NUMBER --json labels --jq '.labels[].name')
  if echo "$LABELS" | grep -q "loom:pr"; then
    echo "PR approved"
    PHASE="gate2"
  elif echo "$LABELS" | grep -q "loom:changes-requested"; then
    echo "Changes requested"
    PHASE="doctor"
  fi
fi
```

### Step 6: Doctor Loop

```bash
MAX_DOCTOR_ITERATIONS=3
DOCTOR_ITERATION=0

while [ "$PHASE" = "doctor" ] && [ $DOCTOR_ITERATION -lt $MAX_DOCTOR_ITERATIONS ]; do
  # Spawn ephemeral doctor worker
  ./.loom/scripts/agent-spawn.sh --role doctor --name "doctor-issue-${ISSUE_NUMBER}" --args "$PR_NUMBER" --on-demand
  ./.loom/scripts/agent-wait-bg.sh "doctor-issue-${ISSUE_NUMBER}" --timeout 900 --issue "$ISSUE_NUMBER"
  WAIT_EXIT=$?
  [ "$WAIT_EXIT" -eq 3 ] && { ./.loom/scripts/agent-destroy.sh "doctor-issue-${ISSUE_NUMBER}"; handle_shutdown; }
  ./.loom/scripts/agent-destroy.sh "doctor-issue-${ISSUE_NUMBER}"

  # Verify doctor completed
  LABELS=$(gh pr view $PR_NUMBER --json labels --jq '.labels[].name')
  if echo "$LABELS" | grep -q "loom:review-requested"; then
    echo "Doctor completed, returning to Judge"
    PHASE="judge"
  fi

  DOCTOR_ITERATION=$((DOCTOR_ITERATION + 1))

  # If we've returned to judge phase, run the judge again
  if [ "$PHASE" = "judge" ]; then
    ./.loom/scripts/agent-spawn.sh --role judge --name "judge-issue-${ISSUE_NUMBER}" --args "$PR_NUMBER" --on-demand
    ./.loom/scripts/agent-wait-bg.sh "judge-issue-${ISSUE_NUMBER}" --timeout 900 --issue "$ISSUE_NUMBER"
    WAIT_EXIT=$?
    [ "$WAIT_EXIT" -eq 3 ] && { ./.loom/scripts/agent-destroy.sh "judge-issue-${ISSUE_NUMBER}"; handle_shutdown; }
    ./.loom/scripts/agent-destroy.sh "judge-issue-${ISSUE_NUMBER}"

    # Check result
    LABELS=$(gh pr view $PR_NUMBER --json labels --jq '.labels[].name')
    if echo "$LABELS" | grep -q "loom:pr"; then
      PHASE="gate2"
      break
    elif echo "$LABELS" | grep -q "loom:changes-requested"; then
      PHASE="doctor"
      # Continue loop
    fi
  fi
done

if [ $DOCTOR_ITERATION -ge $MAX_DOCTOR_ITERATIONS ]; then
  gh issue comment $ISSUE_NUMBER --body "**Orchestration blocked**: Maximum Doctor iterations ($MAX_DOCTOR_ITERATIONS) reached without approval. Manual intervention required."
  gh issue edit $ISSUE_NUMBER --add-label "loom:blocked"
  exit 1
fi
```

### Step 7: Gate 2 - Merge

```bash
if [ "$PHASE" = "gate2" ]; then
  # Check if --force-pr mode - stop here, don't merge
  if [ "$FORCE_PR" = "true" ]; then
    echo "Force-pr mode: stopping at loom:pr state"
    gh issue comment $ISSUE_NUMBER --body "**PR approved** - stopping at \`loom:pr\` per \`--force-pr\`. Ready for human merge."
    exit 0
  fi

  # Check if --force-merge mode - auto-merge with conflict resolution
  if [ "$FORCE_MERGE" = "true" ]; then
    echo "Force-merge mode: auto-merging PR"

    # IMPORTANT: Worktree Checkout Error Handling
    # ============================================
    # When running from a worktree, `gh pr merge` may succeed on GitHub but fail
    # locally with: "fatal: 'main' is already used by worktree at '/path/to/repo'"
    #
    # This is EXPECTED behavior - the merge completes remotely but git can't switch
    # to main locally because another worktree already has it checked out.
    #
    # Solution: Always verify PR state via GitHub API rather than relying on exit code.
    # The exit code of `gh pr merge` is unreliable when running from worktrees.

    MERGE_OUTPUT=$(gh pr merge $PR_NUMBER --squash --delete-branch 2>&1)
    MERGE_EXIT=$?

    # Always verify actual merge state via GitHub API (exit code is unreliable in worktrees)
    PR_STATE=$(gh pr view $PR_NUMBER --json state --jq '.state')

    if [ "$PR_STATE" = "MERGED" ]; then
      # Merge succeeded - any error was just the local checkout failure (expected in worktrees)
      if [ $MERGE_EXIT -ne 0 ]; then
        if echo "$MERGE_OUTPUT" | grep -q "already used by worktree"; then
          echo "PR merged successfully (local checkout skipped - worktree conflict is expected)"
        else
          echo "PR merged successfully (non-fatal local error ignored)"
        fi
      else
        echo "PR merged successfully"
      fi
    else
      # Merge actually failed on GitHub - this is a real error that needs handling
      echo "Merge failed (PR state: $PR_STATE)"

      # Check for merge conflicts
      MERGEABLE=$(gh pr view $PR_NUMBER --json mergeable --jq '.mergeable')
      if [ "$MERGEABLE" = "CONFLICTING" ]; then
        echo "Attempting conflict resolution..."
        git fetch origin main
        git checkout $BRANCH_NAME 2>/dev/null || git checkout -b $BRANCH_NAME origin/$BRANCH_NAME
        git merge origin/main --no-edit || {
          # Auto-resolve conflicts if possible
          git checkout --theirs .
          git add -A
          git commit -m "Resolve merge conflicts (auto-resolved)"
        }
        git push origin $BRANCH_NAME

        # Retry merge and verify via API (not exit code)
        gh pr merge $PR_NUMBER --squash --delete-branch 2>&1
        PR_STATE=$(gh pr view $PR_NUMBER --json state --jq '.state')
        if [ "$PR_STATE" = "MERGED" ]; then
          echo "PR merged successfully after conflict resolution"
        else
          echo "Merge failed after conflict resolution"
          exit 1
        fi
      else
        echo "Merge failed: $MERGE_OUTPUT"
        exit 1
      fi
    fi

    gh issue comment $ISSUE_NUMBER --body "**Auto-merged** PR #$PR_NUMBER via \`/shepherd --force-merge\`"
  else
    # Trigger Champion or wait for human merge
    CHAMPION_TERMINAL="terminal-5"  # if exists

    mcp__loom__trigger_run_now --terminalId $CHAMPION_TERMINAL

    # Wait for merge
    TIMEOUT=1800  # 30 minutes
    START=$(date +%s)

    while true; do
      PR_STATE=$(gh pr view $PR_NUMBER --json state --jq '.state')
      if [ "$PR_STATE" = "MERGED" ]; then
        echo "PR merged successfully"
        break
      elif [ "$PR_STATE" = "CLOSED" ]; then
        echo "PR was closed without merging"
        exit 1
      fi

      NOW=$(date +%s)
      if [ $((NOW - START)) -gt $TIMEOUT ]; then
        echo "Timeout waiting for merge"
        gh issue comment $ISSUE_NUMBER --body "Orchestration complete: PR #$PR_NUMBER is approved and ready for merge."
        exit 0
      fi

      sleep 30
    done
  fi
fi
```

### Step 8: Complete

```bash
# Final status report
gh issue comment $ISSUE_NUMBER --body "$(cat <<EOF
## Orchestration Complete

Issue #$ISSUE_NUMBER has been successfully shepherded through the development lifecycle:

| Phase | Status |
|-------|--------|
| Curator | Enhanced with implementation details |
| Approval | Approved for implementation |
| Builder | Implemented in PR #$PR_NUMBER |
| Judge | Code review passed |
| Merge | PR merged |

**Total orchestration time**: $DURATION

<!-- loom:orchestrator
{"phase":"complete","pr":$PR_NUMBER,"completed":"$(date -u +%Y-%m-%dT%H:%M:%SZ)"}
-->
EOF
)"
```

## Prerequisites

The shepherd requires these scripts in `.loom/scripts/`:
- `agent-spawn.sh` — spawn ephemeral tmux worker sessions
- `agent-wait-bg.sh` — wait for worker completion with shutdown signal checking
- `agent-wait.sh` — wait for worker completion (used by `agent-wait-bg.sh` internally)
- `agent-destroy.sh` — clean up worker sessions

No terminal pre-configuration is needed — workers are created on-demand per phase.

## Error Handling Details

### Worker Spawn Failure

If `agent-spawn.sh` fails:

```bash
# Retry once for transient failures
if ! ./.loom/scripts/agent-spawn.sh --role "$ROLE" --name "${ROLE}-issue-${ISSUE}" --args "$ARGS" --on-demand; then
    sleep 5
    if ! ./.loom/scripts/agent-spawn.sh --role "$ROLE" --name "${ROLE}-issue-${ISSUE}" --args "$ARGS" --on-demand; then
        echo "ERROR: Failed to spawn $ROLE worker after retry"
        gh issue edit $ISSUE --add-label "loom:blocked"
        gh issue comment $ISSUE --body "**Orchestration blocked**: Failed to spawn $ROLE worker."
        exit 1
    fi
fi
```

### Worker Timeout

If `agent-wait-bg.sh` returns a non-zero exit code:

```bash
WAIT_EXIT=$?
if [ "$WAIT_EXIT" -eq 3 ]; then
    echo "Shutdown signal detected - cleaning up"
    ./.loom/scripts/agent-destroy.sh "${ROLE}-issue-${ISSUE}"
    handle_shutdown
elif [ "$WAIT_EXIT" -eq 1 ]; then
    echo "Worker timed out - destroying session"
    ./.loom/scripts/agent-destroy.sh "${ROLE}-issue-${ISSUE}" --force
    # Check if the worker made partial progress via labels
fi
```
