# Loom Agent Workflows

This document describes the label-based workflows that coordinate multiple AI agents working together on a codebase.

## Overview

Loom uses GitHub labels as a coordination protocol. Each agent type has a specific role and watches for issues/PRs with particular labels. This creates a complete pipeline from idea generation through implementation to code review.

### 🌙 The Archetypal Cycle

In Loom, development follows an ancient pattern of archetypal forces working in harmony:

1. 🏛️ **The Architect** envisions → creates proposals (`loom:proposal`)
2. 🔍 **The Critic** questions → identifies bloat and simplification opportunities (`loom:critic-suggestion`)
3. 📚 **The Curator** refines → enhances and marks issues ready (`loom:ready`)
4. 🔮 **The Worker** manifests → implements and creates PRs (`loom:review-requested`)
5. 🔧 **The Fixer** heals → addresses review feedback and keeps PRs merge-ready
6. ⚖️ **The Reviewer** judges → maintains quality through discernment (`loom:approved`)

*Like the Tarot's Major Arcana, each role is essential to the whole. See [Agent Archetypes](docs/philosophy/agent-archetypes.md) for the mystical framework.*

### Color-coded Workflow

- 🔵 **Blue** = Human action needed (`loom:proposal`, `loom:approved`)
- 🟢 **Green** = Loom bot action needed (`loom:ready`, `loom:review-requested`)
- 🟡 **Amber** = Work in progress (`loom:in-progress`, `loom:reviewing`)
- 🔴 **Red** = Blocked or urgent (`loom:blocked`, `loom:urgent`)

See [scripts/LABEL_WORKFLOW.md](scripts/LABEL_WORKFLOW.md) for detailed label state machine documentation.

## Complete Feature Flow

```
ARCHITECT creates proposal (🔵 loom:proposal)
         ↓ (user removes loom:proposal to approve)
CURATOR enhances issue (🟢 loom:ready)
         ↓
WORKER implements (🟡 loom:in-progress → 🟢 loom:review-requested PR)
         ↓
REVIEWER reviews (🟡 loom:reviewing → 🔵 loom:approved)
         ↓
USER merges PR
```

## Priority System

**Maximum Urgent: 3 Issues**

The Triage agent maintains exactly 3 issues as `loom:urgent` (🔴 red). This prevents "everything is urgent" syndrome.

| Priority | Label | Worker Behavior |
|----------|-------|-----------------|
| 🔴 **Urgent** | `loom:urgent` | Workers check first |
| 🟢 **Normal** | *(no priority label)* | FIFO (oldest first) |

**Managed by**: Triage agent (autonomous, 15min interval)

### When to Mark Urgent

✅ Strategic impact (blocks 2+ issues, unblocks team)
✅ Time-sensitive (security, critical bugs, hotfixes)
✅ Quick wins (< 1 day, major impact)

❌ Nice-to-haves, can wait, uncertain value

## Dependency Tracking

Issues can declare prerequisites using GitHub task lists:

```markdown
## Dependencies

- [ ] #123: Database migration system
- [ ] #456: User authentication API

This issue cannot proceed until all dependencies above are complete.
```

**Key behaviors**:
- GitHub auto-checks boxes when linked issues close
- **Curator**: Only marks `loom:ready` if all dependencies checked
- **Worker**: Verifies dependencies before claiming
- Blocked issues get `loom:blocked` label

See full dependency workflow in [scripts/LABEL_WORKFLOW.md](scripts/LABEL_WORKFLOW.md).

## Agent Types Summary

| Agent | Interval | Autonomous | Watches For | Creates |
|-------|----------|-----------|-------------|---------|
| **Architect** | 15 min | Yes | N/A (scans codebase) | `loom:proposal` (blue) |
| **Critic** | 15 min | Yes | N/A (scans code/issues) | `loom:critic-suggestion` (blue) |
| **Curator** | 5 min | Yes | Approved issues | `loom:ready` (green) |
| **Triage** | 15 min | Yes | `loom:ready` | `loom:urgent` (red) |
| **Worker** | Manual | No | `loom:ready` | `loom:in-progress`, `loom:review-requested` |
| **Reviewer** | 5 min | Yes | `loom:review-requested` | `loom:reviewing`, `loom:approved` |
| **Fixer** | 5-10 min | Optional | PRs with changes/conflicts | `loom:review-requested` |
| **Issues** | Manual | No | N/A | Well-formatted issues |
| **Default** | Manual | No | N/A | Plain shell |

### Quick Role Descriptions

**Architect**: Scans codebase for improvements across all domains (architecture, code quality, docs, tests, CI, performance). Creates comprehensive proposals with `loom:proposal` label.

**Critic**: Identifies bloat, unused code, over-engineering. Creates removal proposals or adds simplification comments to existing issues.

**Curator**: Enhances approved issues with implementation details, test plans, multiple options. Marks as `loom:ready` when complete.

**Triage**: Dynamically prioritizes `loom:ready` issues, maintains top 3 as `loom:urgent` based on strategic impact and time sensitivity.

**Worker**: Implements `loom:ready` issues. Claims with `loom:in-progress`, creates PR with `loom:review-requested`. Manages worktrees.

**Reviewer**: Reviews `loom:review-requested` PRs. Claims with `loom:reviewing`, approves with `loom:approved` (ready for user to merge).

**Fixer**: Addresses review feedback and resolves merge conflicts. Transitions `loom:reviewing` → `loom:review-requested` after fixes.

## Essential Commands

### Worker Workflow

```bash
# Find and claim ready issue
gh issue list --label="loom:ready"
gh issue edit 42 --remove-label "loom:ready" --add-label "loom:in-progress"

# Create worktree and implement
pnpm worktree 42
cd .loom/worktrees/issue-42
# ... implement ...
pnpm check:ci

# Create PR
git push -u origin feature/issue-42
gh pr create --label "loom:review-requested"
```

### Reviewer Workflow

```bash
# Find and claim PR
gh pr list --label="loom:review-requested"
gh pr edit 50 --remove-label "loom:review-requested" --add-label "loom:reviewing"

# Review
gh pr checkout 50
pnpm check:all

# Approve or request changes
gh pr review 50 --approve
gh pr edit 50 --remove-label "loom:reviewing" --add-label "loom:approved"
```

### Curator Workflow

```bash
# Find approved issues (no loom:proposal, not ready/in-progress)
gh issue list --state=open --json number,title,labels \
  --jq '.[] | select(([.labels[].name] | inside(["loom:proposal", "loom:ready", "loom:in-progress"]) | not)) | "#\(.number) \(.title)"'

# Enhance and mark ready
gh issue edit 42 --add-label "loom:ready"
```

### User (Manual) Workflow

```bash
# Review proposals
gh issue list --label="loom:proposal"
gh issue edit 42 --remove-label "loom:proposal"  # Approve
gh issue close 42 --comment "Not needed"          # Reject

# Merge approved PRs
gh pr list --label="loom:approved"
gh pr merge 50
```

## Label Reference

### Issue Labels

| Label | Color | Meaning |
|-------|-------|---------|
| `loom:proposal` | 🔵 Blue | Architect suggestion awaiting approval |
| `loom:critic-suggestion` | 🔵 Blue | Removal/simplification awaiting approval |
| `loom:ready` | 🟢 Green | Ready for Worker to implement |
| `loom:in-progress` | 🟡 Amber | Worker actively implementing |
| `loom:blocked` | 🔴 Red | Implementation blocked, needs help |
| `loom:urgent` | 🔴 Dark Red | High priority (max 3) |

### PR Labels

| Label | Color | Meaning |
|-------|-------|---------|
| `loom:review-requested` | 🟢 Green | Ready for Reviewer |
| `loom:reviewing` | 🟡 Amber | Reviewer actively reviewing |
| `loom:approved` | 🔵 Blue | Approved, ready for human to merge |

## Configuration

Each role has default settings in `defaults/roles/<role>.json`:

```json
{
  "name": "Curator Bot",
  "defaultInterval": 300000,
  "defaultIntervalPrompt": "Find approved issues...",
  "autonomousRecommended": true,
  "suggestedWorkerType": "claude"
}
```

Users can override these in the Terminal Settings modal.

## Best Practices

### For Users
1. Review suggestions promptly (blue labels need approval)
2. Remove `loom:proposal` to approve, or close to reject
3. Merge `loom:approved` PRs after final review

### For Agents
1. **Stay in your lane**: Don't do other roles' work
2. **Complete current task first**: Don't get sidetracked
3. **Document thoroughly**: Future agents need context
4. **Use labels correctly**: Workflow coordination depends on it
5. **Reference issues**: Always link related work

### For Autonomous Operation
1. **Best combination**: Curator + Reviewer + Architect autonomous
2. **Worker**: Usually manual (one per PR)
3. **Intervals**: Curator/Reviewer 5min, Architect/Triage 15min
4. **Monitor blocked**: Auto-resolve or escalate

## Troubleshooting

**Issue stuck without labels**
→ Curator should pick up within 5min (if autonomous)
→ Manually add `loom:ready` if urgent

**PR not reviewed**
→ Reviewer may be disabled
→ Manually review or remove label

**Multiple agents claiming same item**
→ First agent should remove trigger label
→ Race condition: coordinate manually

## Complete Workflow Documentation

For detailed workflows including:
- Dependency lifecycle examples
- Agent-specific command references
- Troubleshooting guides
- Future enhancements

See [scripts/LABEL_WORKFLOW.md](scripts/LABEL_WORKFLOW.md) for comprehensive documentation.

---

**For detailed agent workflows and command references, see [scripts/LABEL_WORKFLOW.md](scripts/LABEL_WORKFLOW.md).**

Last updated: Issue #312 - Split large documentation files for token efficiency
