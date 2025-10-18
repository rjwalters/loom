# Loom Agent Workflows

This document describes the label-based workflows that coordinate multiple AI agents working together on a codebase.

## Overview

Loom uses GitHub labels as a coordination protocol. Each agent type has a specific role and watches for issues/PRs with particular labels. This creates a complete pipeline from idea generation through implementation to code review.

### 🌙 The Archetypal Cycle

In Loom, development follows an ancient pattern of archetypal forces working in harmony:

1. 🏛️ **The Architect** envisions → creates proposals (`loom:architect-suggestion`)
2. 🔍 **The Critic** questions → identifies bloat and simplification opportunities (`loom:critic-suggestion`)
3. 📚 **The Curator** refines → enhances and adds `loom:curated` (human then approves with `loom:issue`)
4. 🔮 **The Worker** manifests → implements and creates PRs (`loom:review-requested`)
5. 🔧 **The Fixer** heals → addresses review feedback (`loom:changes-requested` → `loom:review-requested`)
6. ⚖️ **The Reviewer** judges → maintains quality through discernment (`loom:pr`)

*Like the Tarot's Major Arcana, each role is essential to the whole. See [Agent Archetypes](docs/philosophy/agent-archetypes.md) for the mystical framework.*

### Color-coded Workflow

- 🔵 **Blue** = Human action needed
  - Issues: `loom:architect-suggestion` (Architect suggestion awaiting approval)
  - PRs: `loom:pr` (Approved PR ready to merge)
- 🟢 **Green** = Loom bot action needed
  - Issues: `loom:curated` (Curator enhanced), `loom:issue` (human approved)
  - PRs: `loom:review-requested` (PR ready for Reviewer)
- 🟡 **Amber** = Work in progress
  - Issues: `loom:in-progress` (Worker implementing)
  - PRs: `loom:changes-requested` (Fixer addressing review feedback)
- 🔴 **Red** = Blocked or urgent
  - `loom:blocked` (Blocked, needs help)
  - `loom:urgent` (High priority)

See [scripts/LABEL_WORKFLOW.md](scripts/LABEL_WORKFLOW.md) for detailed label state machine documentation.

## Complete Feature Flow

```
ARCHITECT creates proposal (🔵 loom:architect-suggestion)
         ↓ (human removes loom:architect-suggestion to approve)
CURATOR enhances issue (🟢 loom:curated)
         ↓ (human adds loom:issue to approve for work)
WORKER implements (🟡 loom:in-progress → 🟢 loom:review-requested PR)
         ↓
REVIEWER reviews (🟡 loom:changes-requested OR 🔵 loom:pr)
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
| **Architect** | 15 min | Yes | N/A (scans codebase) | `loom:architect-suggestion` (blue) |
| **Critic** | 15 min | Yes | N/A (scans code/issues) | `loom:critic-suggestion` (blue) |
| **Curator** | 5 min | Yes | Approved issues (no suggestion labels) | `loom:curated` (green) |
| **Triage** | 15 min | Yes | `loom:issue` | `loom:urgent` (red) |
| **Worker** | Manual | No | `loom:issue` | `loom:in-progress`, `loom:review-requested` |
| **Reviewer** | 5 min | Yes | `loom:review-requested` | `loom:changes-requested`, `loom:pr` |
| **Fixer** | 5-10 min | Optional | `loom:changes-requested` | `loom:review-requested` |
| **Issues** | Manual | No | N/A | Well-formatted issues |
| **Default** | Manual | No | N/A | Plain shell |

### Quick Role Descriptions

**Architect**: Scans codebase for improvements across all domains (architecture, code quality, docs, tests, CI, performance). Creates comprehensive proposals with `loom:architect-suggestion` label.

**Critic**: Identifies bloat, unused code, over-engineering. Creates removal proposals or adds simplification comments to existing issues.

**Curator**: Enhances approved issues with implementation details, test plans, multiple options. Adds `loom:curated` when complete. **Does not approve for work - human must add `loom:issue`.**

**Triage**: Dynamically prioritizes `loom:issue` issues, maintains top 3 as `loom:urgent` based on strategic impact and time sensitivity.

**Worker**: Implements `loom:issue` issues. Claims with `loom:in-progress`, creates PR with `loom:review-requested`. Manages worktrees.

**Reviewer**: Reviews `loom:review-requested` PRs. Requests changes with `loom:changes-requested`, approves with `loom:pr` (ready for user to merge).

**Fixer**: Addresses review feedback and resolves merge conflicts. Transitions `loom:changes-requested` → `loom:review-requested` after fixes.

## Essential Commands

### Worker Workflow

```bash
# Find and claim approved issue
gh issue list --label="loom:issue"
gh issue edit 42 --remove-label "loom:issue" --add-label "loom:in-progress"

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

# Review
gh pr checkout 50
pnpm check:all

# Approve (green → blue)
gh pr review 50 --approve
gh pr edit 50 --remove-label "loom:review-requested" --add-label "loom:pr"

# Request changes (green → amber)
gh pr review 50 --request-changes
gh pr edit 50 --remove-label "loom:review-requested" --add-label "loom:changes-requested"
```

### Fixer Workflow

```bash
# Find PRs needing fixes
gh pr list --label="loom:changes-requested"

# Fix and signal ready for re-review (amber → green)
gh pr checkout 50
# ... address feedback ...
pnpm check:ci
git push
gh pr edit 50 --remove-label "loom:changes-requested" --add-label "loom:review-requested"
```

### Curator Workflow

```bash
# Find approved issues (no suggestion labels, not loom:issue/in-progress)
gh issue list --state=open --json number,title,labels \
  --jq '.[] | select(([.labels[].name] | inside(["loom:architect-suggestion", "loom:critic-suggestion", "loom:issue", "loom:in-progress"]) | not)) | "#\(.number) \(.title)"'

# Enhance and mark as curated
gh issue edit 42 --add-label "loom:curated"
```

### User (Manual) Workflow

```bash
# Review proposals
gh issue list --label="loom:architect-suggestion"
gh issue edit 42 --remove-label "loom:architect-suggestion"  # Approve
gh issue close 42 --comment "Not needed"                      # Reject

# Approve curated issues for work
gh issue list --label="loom:curated"
gh issue edit 42 --add-label "loom:issue"  # Approve for work

# Merge approved PRs
gh pr list --label="loom:pr"
gh pr merge 50
```

## Label Reference

### Issue Labels

| Label | Color | Set By | Meaning |
|-------|-------|--------|---------|
| `loom:architect-suggestion` | 🔵 Blue | Architect | Architect suggestion awaiting human approval |
| `loom:critic-suggestion` | 🔵 Blue | Critic | Removal/simplification awaiting human approval |
| `loom:curated` | 🟢 Green | Curator | Curator enhanced, awaiting human approval for work |
| `loom:issue` | 🟢 Green | Human | Human approved, ready for Worker to implement |
| `loom:in-progress` | 🟡 Amber | Worker | Worker actively implementing |
| `loom:blocked` | 🔴 Red | Any agent | Implementation blocked, needs help |
| `loom:urgent` | 🔴 Dark Red | Triage | High priority (max 3) |

### PR Labels

| Label | Color | Created By | Meaning |
|-------|-------|-----------|---------|
| `loom:review-requested` | 🟢 Green | Worker/Fixer | PR ready for Reviewer |
| `loom:changes-requested` | 🟡 Amber | Reviewer | PR needs fixes from Fixer |
| `loom:pr` | 🔵 Blue | Reviewer | Approved PR ready for human to merge |

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
2. Remove `loom:architect-suggestion` to approve, or close to reject
3. Add `loom:issue` to curated issues to approve for work
4. Merge `loom:pr` PRs after final review

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
→ Manually add `loom:issue` if urgent

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

Last updated: Issue #332 - Revised label state machine with human approval gate
