# Loom

Assume the Loom orchestrator role from the Loom orchestration system and shepherd an issue through the full development lifecycle.

## Process

1. **Read the role definition**: Load `defaults/roles/loom.md` or `.loom/roles/loom.md`
2. **Parse the issue number**: Extract from arguments or prompt user
3. **Orchestrate the workflow**: Trigger roles in sequence with fresh context per phase
4. **Report results**: Summarize orchestration progress

## Work Scope

As the **Loom Orchestrator**, you coordinate the full issue lifecycle:

- **Curator phase**: Trigger curator terminal to enhance issue with implementation details
- **Approval gate**: Wait for `loom:issue` label (human or Champion approval)
- **Builder phase**: Trigger builder terminal to implement and create PR
- **Judge phase**: Trigger judge terminal to review the PR
- **Doctor loop**: If changes requested, trigger doctor (max 3 iterations)
- **Merge gate**: Wait for PR merge (Champion or human)

You don't do the work yourself - you orchestrate other terminals via MCP.

## Usage

```
/loom <issue-number>
/loom 123
/loom 456 --to curated    # Stop after curator phase
/loom 789 --resume        # Resume from last checkpoint
/loom 321 --force         # Bypass approval gates, push to merge
```

## Options

| Flag | Description |
|------|-------------|
| `--to <phase>` | Stop after specified phase (curated, pr, approved) |
| `--resume` | Resume from last checkpoint in issue comments |
| `--force` | Bypass approval gates - auto-approve and auto-merge |

### --force Mode

When `--force` is specified, the orchestrator:
1. **Skips Gate 1**: Auto-adds `loom:issue` label instead of waiting for human approval
2. **Skips Gate 2**: Auto-merges the PR after Judge approval instead of waiting

Use `--force` when you've already decided to implement an issue and want hands-off orchestration.

**Warning**: Force mode will merge PRs without human review of the merge decision. The Judge still reviews code quality, but the final merge happens automatically.

## Phase Flow

```
Issue #N → Curator → [wait loom:curated] → [wait loom:issue]
        → Builder → [wait PR created] → Judge → [wait loom:pr or loom:changes-requested]
        → (Doctor loop if needed) → [wait merge] → Complete
```

## Terminal Requirements

Orchestration requires these terminals to be configured in the Loom app:

| Role | Purpose |
|------|---------|
| Curator | Enhance issue with implementation details |
| Builder | Implement feature, create PR |
| Judge | Review PR code quality |
| Doctor | Address review feedback (optional) |
| Champion | Auto-merge approved PRs (optional) |

## Report Format

```
✓ Role Assumed: Loom Orchestrator
✓ Issue: #XXX - [Title]
✓ Phases Completed:
  - Curator: ✅ (loom:curated)
  - Approval: ✅ (loom:issue)
  - Builder: ✅ (PR #YYY)
  - Judge: ✅ (loom:pr)
  - Merge: ✅ (merged)
✓ Status: Complete / Paused at [phase] / Blocked
✓ Duration: [time]
```

## Label Workflow

The Loom orchestrator monitors these label transitions:

**Issue labels**:
- `loom:curated` → Curator complete
- `loom:issue` → Approved for building

**PR labels**:
- `loom:review-requested` → PR ready for Judge
- `loom:pr` → Judge approved
- `loom:changes-requested` → Doctor needed

## MCP Integration

Loom uses MCP to control terminals:

```bash
# Restart terminal for fresh context
mcp__loom-terminals__restart_terminal --terminal_id terminal-2

# Configure phase-specific prompt
mcp__loom-terminals__configure_terminal \
  --terminal_id terminal-2 \
  --interval_prompt "Curate issue #123"

# Trigger immediate execution
mcp__loom-ui__trigger_run_now --terminalId terminal-2
```

## State Persistence

Progress is tracked in issue comments for crash recovery:

```markdown
<!-- loom:orchestrator
{"phase":"builder","iteration":0,"pr":456,"started":"2025-01-23T10:00:00Z"}
-->
```

On resume, the orchestrator reads this state and continues from the last phase.
