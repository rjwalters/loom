# Loom Agent Workflows

This document describes the label-based workflows that coordinate multiple AI agents working together on a codebase.

## Overview

Loom uses GitHub labels as a coordination protocol. Each agent type has a specific role and watches for issues/PRs with particular labels. This creates a complete pipeline from idea generation through implementation to code review.

### 🌙 The Archetypal Cycle

In Loom, development follows an ancient pattern of archetypal forces working in harmony:

1. 🏛️ **The Architect** envisions → creates proposals (`loom:architect`)
2. 🔍 **The Hermit** questions → identifies bloat and simplification opportunities (`loom:hermit`)
3. 📚 **The Curator** refines → enhances and adds `loom:curated` (human then approves with `loom:issue`)
4. 🔮 **The Worker** manifests → implements and creates PRs (`loom:review-requested`)
5. 🔧 **The Fixer heals → claims with `loom:healing``, addresses review feedback (`loom:changes-requested` → `loom:review-requested`)
6. ⚖️ **The Reviewer** judges → maintains quality through discernment (`loom:pr`)

*Like the Tarot's Major Arcana, each role is essential to the whole. See [Agent Archetypes](docs/philosophy/agent-archetypes.md) for the mystical framework.*

### Color-coded Workflow

- 🔵 **Blue** = Human action needed
  - Issues: `loom:architect` (Architect suggestion awaiting approval)
  - PRs: `loom:pr` (Approved PR ready to merge)
- 🟢 **Green** = Loom bot action needed
  - Issues: `loom:curated` (Curator enhanced), `loom:issue` (human approved)
  - PRs: `loom:review-requested` (PR ready for Reviewer)
- 🟡 **Amber** = Work in progress
  - Issues: `loom:building` (Worker implementing)
  - PRs: `loom:changes-requested` (review feedback needed), `loom:building` (Fixer claiming PR)
- 🔴 **Red** = Blocked or urgent
  - `loom:blocked` (Blocked, needs help)
  - `loom:urgent` (High priority)

See [scripts/LABEL_WORKFLOW.md](scripts/LABEL_WORKFLOW.md) for detailed label state machine documentation.

## Complete Feature Flow

```
ARCHITECT creates proposal (🔵 loom:architect)
         ↓ (human removes loom:architect to approve)
CURATOR enhances issue (🟢 loom:curated)
         ↓ (human adds loom:issue to approve for work)
WORKER implements (🟡 loom:building → 🟢 loom:review-requested PR)
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
| **Architect** | 15 min | Yes | N/A (scans codebase) | `loom:architect` (blue) |
| **Hermit** | 15 min | Yes | N/A (scans code/issues) | `loom:hermit` (blue) |
| **Curator** | 5 min | Yes | Approved issues (no suggestion labels) | `loom:curated` (green) |
| **Triage** | 15 min | Yes | `loom:issue` | `loom:urgent` (red) |
| **Worker** | Manual | No | `loom:issue` | `loom:building`, `loom:review-requested` |
| **Reviewer** | 5 min | Yes | `loom:review-requested` | `loom:changes-requested`, `loom:pr` |
| **Fixer heals → claims with `loom:healing``, `loom:review-requested` |
| **Issues** | Manual | No | N/A | Well-formatted issues |
| **Default** | Manual | No | N/A | Plain shell |

### Quick Role Descriptions

**Architect**: Scans codebase for improvements across all domains (architecture, code quality, docs, tests, CI, performance). Creates comprehensive proposals with `loom:architect` label.

**Hermit**: Identifies bloat, unused code, over-engineering. Creates removal proposals or adds simplification comments to existing issues.

**Curator**: Enhances approved issues with implementation details, test plans, multiple options. Claims issues with `loom:building` before starting, removes it and adds `loom:curated` when complete. **Does not approve for work - human must add `loom:issue`.**

**Triage**: Dynamically prioritizes `loom:issue` issues, maintains top 3 as `loom:urgent` based on strategic impact and time sensitivity.

**Worker**: Implements `loom:issue` issues. Claims with `loom:building`, creates PR with `loom:review-requested`. Manages worktrees.

**Reviewer**: Reviews `loom:review-requested` PRs. Requests changes with `loom:changes-requested`, approves with `loom:pr` (ready for user to merge).

**Fixer heals → claims with `loom:healing``.

## Essential Commands

### Worker Workflow

```bash
# Find and claim approved issue
gh issue list --label="loom:issue"
gh issue edit 42 --remove-label "loom:issue" --add-label "loom:building"

# Create worktree and implement
./.loom/scripts/worktree.sh 42  # or: pnpm worktree 42 (in loom repo)
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

**Healers prioritize work in the following order:**

```bash
# Priority 1 (URGENT): Approved PRs with merge conflicts - BLOCKING
gh pr list --label="loom:approved" --state=open --search "is:open conflicts:>0" --json number,title,labels \
  | jq -r '.[] | select(.labels | all(.name != "loom:building")) | "#\(.number): \(.title)"'

# Priority 2 (NORMAL): PRs with review feedback
gh pr list --label="loom:changes-requested" --state=open --json number,title,labels \
  | jq -r '.[] | select(.labels | all(.name != "loom:building")) | "#\(.number): \(.title)"'

# Claim the PR before starting work
gh pr edit 50 --add-label "loom:building"

# Fix and signal ready for re-review (amber → green, remove in-progress)
gh pr checkout 50
# ... address feedback or resolve conflicts ...
pnpm check:ci
git push
gh pr edit 50 --remove-label "loom:changes-requested" --remove-label "loom:building" --add-label "loom:review-requested"
```

### Curator Workflow

```bash
# Find approved issues (no suggestion labels, not loom:issue/in-progress)
gh issue list --state=open --json number,title,labels \
  --jq '.[] | select(([.labels[].name] | inside(["loom:architect", "loom:hermit", "loom:curated", "loom:issue", "loom:building"]) | not)) | "#\(.number) \(.title)"'

# Claim the issue before starting enhancement
gh issue edit 42 --add-label "loom:building"

# Enhance issue (add details, test plans, implementation options)
# ...

# Mark as curated and unclaim
gh issue edit 42 --remove-label "loom:building" --add-label "loom:curated"
```

### User (Manual) Workflow

```bash
# Review proposals
gh issue list --label="loom:architect"
gh issue edit 42 --remove-label "loom:architect"  # Approve
gh issue close 42 --comment "Not needed"           # Reject

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
| `loom:architect` | 🔵 Blue | Architect | Architect suggestion awaiting human approval |
| `loom:hermit` | 🔵 Blue | Hermit | Removal/simplification awaiting human approval |
| `loom:curating` | 🟡 Amber | Curator | Curator actively enhancing issue |
| `loom:curated` | 🟢 Green | Curator | Curator enhanced, awaiting human approval for work |
| `loom:issue` | 🟢 Green | Human | Human approved, ready for Worker to implement |
| `loom:building` | 🟡 Amber | Worker | Worker actively implementing |
| `loom:healing` | 🟡 Amber | Healer | Healer actively fixing bug or addressing feedback |
| `loom:blocked` | 🔴 Red | Any agent | Implementation blocked, needs help |
| `loom:urgent` | 🔴 Dark Red | Triage | High priority (max 3) |

### PR Labels

| Label | Color | Created By | Meaning |
|-------|-------|-----------|---------|
| `loom:review-requested` | 🟢 Green | Worker/Fixer | PR ready for Reviewer |
| `loom:changes-requested` | 🟡 Amber | Reviewer | PR needs fixes from Fixer |
| `loom:healing` | 🟡 Amber | Fixer | Fixer actively addressing review feedback |
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
2. Remove `loom:architect` to approve, or close to reject
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

## Common Mistakes

### Missing GitHub Auto-Close Keywords (Critical)

**Problem**: PR uses "Issue #123" or "Addresses #123" instead of magic keywords, leaving issues open after merge.

**Impact**:
- ❌ Completed issues stay open (appears incomplete)
- ❌ Stale backlog clutter
- ❌ Manual cleanup work for maintainers
- ❌ Confusion about project status

**Root Cause**: GitHub **only** auto-closes issues when PRs use specific keywords:
- ✅ `Closes #X`
- ✅ `Fixes #X`
- ✅ `Resolves #X`
- ✅ `Closing #X`
- ✅ `Fixed #X`
- ✅ `Resolved #X`

**Wrong vs Right:**

```markdown
# ❌ WRONG - Issue stays open after merge
## Summary
This PR implements the feature requested in issue #123.

## Changes
...

# ✅ CORRECT - Issue auto-closes on merge
## Summary
Implement new feature to improve user experience.

## Changes
...

Closes #123
```

**Prevention (Multi-Layered):**

1. **Builder** (Prevention):
   - Always use `gh pr create` with "Closes #X" in body
   - See Builder role docs for PR creation checklist
   - Put keyword on its own line at end of description

2. **Judge** (Checkpoint):
   - Verify PR description has magic keyword BEFORE reviewing code
   - Request changes immediately if missing
   - Don't approve PRs without proper issue linking

3. **Guide** (Verification):
   - Check recently merged PRs (every 15-30 min)
   - Find orphaned issues that should have closed
   - Manually close with explanatory comment

**Example Fix (Guide role):**

```bash
# Found: PR #344 merged but issue #339 still open
gh issue close 339 --comment "✅ **Closing completed issue**

This issue was completed in PR #344 (merged 2025-10-18) but stayed open because the PR didn't use the magic keyword syntax.

**What happened:**
- PR #344 used 'Issue #339' instead of 'Closes #339'
- GitHub only auto-closes with specific keywords
- Manual closure now to clean up backlog

**To prevent this:** See Builder role docs on PR creation - always use 'Closes #X' syntax."
```

**References:**
- Builder role: `defaults/roles/builder.md` - PR creation requirements
- Judge role: `defaults/roles/judge.md` - PR link verification checklist
- Guide role: `defaults/roles/guide.md` - Verification procedures

### Forgetting to Update PR Labels

**Problem**: Worker creates PR but forgets to add `loom:review-requested` label.

**Impact**: Reviewer (Judge) won't find the PR, delaying review.

**Prevention**:
```bash
# Always include label when creating PR
gh pr create --label "loom:review-requested"
```

### Claiming Blocked Dependencies

**Problem**: Worker claims issue before checking if dependencies are complete.

**Impact**: Gets stuck mid-work, wastes time.

**Prevention**:
```bash
# Before claiming, check for Dependencies section
gh issue view 42 --comments

# Only claim if all checkboxes are marked
# Dependencies
# - [x] #120: Database migration
# - [x] #121: API endpoint
```

### Multiple Agents Claiming Same Work

**Problem**: Two Workers claim the same `loom:issue` simultaneously.

**Impact**: Duplicate work, wasted effort.

**Prevention**:
- Remove `loom:issue` label immediately after claiming
- If you see an issue already has `loom:building`, don't claim it
- FIFO queue: claim oldest first

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
