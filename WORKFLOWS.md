# Loom Agent Workflows

This document describes the label-based workflows that coordinate multiple AI agents working together on a codebase.

## Overview

Loom uses GitHub labels as a coordination protocol. Each agent type has a specific role and watches for issues/PRs with particular labels. This creates a complete pipeline from idea generation through implementation to code review.

**Color-coded workflow:**
- 🔵 **Blue** (`loom:issue`, `loom:pr`) = Human action needed
- 🟢 **Green** (`loom:ready`) = Loom bot action needed
- 🟡 **Amber** (`loom:in-progress`) = Work in progress
- 🔴 **Red** (`loom:blocked`) = Blocked, needs help

See [scripts/LABEL_WORKFLOW.md](scripts/LABEL_WORKFLOW.md) for detailed documentation.

## Priority System

Issues can have an optional priority label to ensure urgent work gets immediate attention:

| Priority | Label | Worker Behavior |
|----------|-------|-----------------|
| 🔴 **Urgent** | `loom:urgent` | Workers check first, before all other issues |
| 🟢 **Normal** | *(no priority label)* | Workers use FIFO (oldest first) |

### Who Can Add Priority Labels?

- **User**: Ultimate authority, can add any time
- **Architect**: Can suggest during triage
- **Curator**: Can add when enhancing issues
- **Worker**: Should NOT add (conflict of interest)

### When to Use Urgent Priority

Use `loom:urgent` sparingly for:
- Security vulnerabilities requiring immediate patches
- Critical bugs affecting users or blocking all other work
- Production issues that need hotfixes
- Time-sensitive work that cannot wait

**Most issues should be normal priority** (no label). Urgent means "must be done NOW, before anything else."

## Agent Types

### 1. Architect Bot
**Role**: Improvement proposal generator

**Watches for**: N/A (proactively scans codebase)

**Creates**:
- Issues with `loom:proposal` label (blue badge - awaiting user approval)

**Interval**: 15 minutes (recommended autonomous)

**Scope**: Scans codebase across all domains:
- **Architecture & Features**: New features, API design, system improvements
- **Code Quality**: Refactoring, consistency, duplication, unused code
- **Documentation**: Outdated docs, missing explanations, API documentation
- **Testing**: Missing coverage, flaky tests, edge cases
- **CI/Build/Tooling**: Failing jobs, slow builds, outdated dependencies
- **Performance & Security**: Optimizations, vulnerabilities, resource leaks

**Workflow**:
```
1. Check if there are already 3+ open proposals (don't spam)
2. If < 3 proposals: Scan codebase for improvement opportunities
3. Create comprehensive issue with proposal
4. Add loom:proposal label immediately (blue badge)
5. Wait for user to remove loom:proposal label (approval)
```

**Important**: Architect does NOT triage issues created by others. Only creates proposals.

### 2. Curator Bot
**Role**: Enhances approved issues and marks them ready for implementation

**Watches for**:
- Issues without `loom:proposal` label (user has approved them)
- Excludes issues already marked `loom:ready` or `loom:in-progress`

**Creates**:
- `loom:ready` - Issues ready for worker implementation

**Interval**: 5 minutes (recommended autonomous)

**Workflow**:
```
1. Find approved issues (no loom:proposal label, not yet ready/in-progress)
2. Review issue description and requirements
3. Add implementation details, test plans, code references
4. Document multiple implementation options if complex
5. Add loom:ready when enhancement complete
```

### 3. Worker Bot
**Role**: Implements features and fixes bugs

**Watches for**:
- `loom:ready` - Issues ready to be implemented

**Creates**:
- `loom:in-progress` - Claims issue for implementation
- `loom:ready` - PRs ready for Reviewer
- `loom:blocked` - When stuck on implementation

**Interval**: Disabled by default (on-demand, one Worker per PR)

**Workflow**:
```
1. Find loom:ready issues (green badges)
2. Claim by removing loom:ready, adding loom:in-progress
3. Implement, test, commit
4. Create PR with "Closes #X", add loom:ready (green - ready for Reviewer)
5. Monitor PR and address Reviewer feedback
6. If blocked: add loom:blocked with explanation
```

### 4. Reviewer Bot
**Role**: Reviews pull requests

**Watches for**:
- `loom:ready` - PRs ready for review (green badges)

**Creates**:
- `loom:in-progress` - Claims PR for review (amber)
- `loom:pr` - Approved PRs ready for human to merge (blue)

**Interval**: 5 minutes (recommended autonomous)

**Workflow**:
```
1. Find loom:ready PRs (green badges)
2. Claim by removing loom:ready, adding loom:in-progress (amber)
3. Check out branch, run tests, review code
4. If changes needed: gh pr review --request-changes, keep loom:in-progress
5. If approved: gh pr review --approve, remove loom:in-progress, add loom:pr (blue)
```

### 5. Issues Bot
**Role**: Creates well-structured GitHub issues from user requests

**Watches for**: N/A (manual invocation)

**Creates**: Well-formatted issues with proper structure

**Interval**: Disabled (manual only)

**Workflow**:
```
1. User provides feature request or bug report
2. Structure into clear issue format
3. Add acceptance criteria, test plan
4. Include code references and context
5. Create issue (no label initially)
```

### 6. Default (Plain Shell)
**Role**: Standard terminal for manual commands

No automation. Used for manual git operations, system commands, etc.

## Complete Workflow Example

### Feature Implementation Flow

```
┌─────────────────────────────────────────────────────────────┐
│ 1. ARCHITECT CREATES PROPOSAL                               │
│    gh issue create                                          │
│    Title: "Add search functionality to terminal history"    │
│    gh issue edit <#> --add-label "loom:proposal"            │
│    (Blue badge - awaiting user approval)                    │
└─────────────────────┬───────────────────────────────────────┘
                      │
                      ↓
┌─────────────────────────────────────────────────────────────┐
│ 2. USER REVIEWS AND APPROVES                                │
│    Reviews issue with loom:proposal (blue badge)            │
│    Removes loom:proposal label to approve                   │
│    (Or closes issue to reject)                              │
└─────────────────────┬───────────────────────────────────────┘
                      │
                      ↓
┌─────────────────────────────────────────────────────────────┐
│ 3. CURATOR ENHANCES ISSUE                                   │
│    Finds approved issue #42 (no loom:proposal)              │
│    Adds implementation details:                             │
│    - Multiple implementation options                        │
│    - Dependencies and risks                                 │
│    - Test plan checklist                                    │
│    Adds loom:ready (green badge)                            │
└─────────────────────┬───────────────────────────────────────┘
                      │
                      ↓
┌─────────────────────────────────────────────────────────────┐
│ 4. WORKER IMPLEMENTS                                        │
│    Finds loom:ready issue #42 (green badge)                 │
│    Updates: removes loom:ready, adds loom:in-progress       │
│    (Amber badge)                                            │
│    Implements feature, writes tests                         │
│    Creates PR: "Closes #42", adds loom:ready                │
│    (Green badge - ready for review)                         │
└─────────────────────┬───────────────────────────────────────┘
                      │
                      ↓
┌─────────────────────────────────────────────────────────────┐
│ 5. REVIEWER REVIEWS PR                                      │
│    Finds loom:ready PR #50 (green badge)                    │
│    Updates: removes loom:ready, adds loom:in-progress       │
│    (Amber badge - reviewing)                                │
│    Checks out branch, runs tests                            │
│    Reviews code, provides feedback                          │
│    Approves: gh pr review --approve                         │
│    Removes loom:in-progress, adds loom:pr                   │
│    (Blue badge - ready for user to merge)                   │
└─────────────────────┬───────────────────────────────────────┘
                      │
                      ↓
┌─────────────────────────────────────────────────────────────┐
│ 6. USER MERGES PR                                           │
│    Reviews loom:pr PR (blue badge)                          │
│    Merges to main                                           │
│    Issue #42 automatically closes                           │
└─────────────────────────────────────────────────────────────┘
```

## Label Reference

| Label | Color | Used On | Created By | Meaning |
|-------|-------|---------|-----------|---------|
| `loom:proposal` | 🔵 Blue | Issues | Architect | Proposal awaiting user approval |
| `loom:ready` | 🟢 Green | Issues & PRs | Curator (issues) / Worker (PRs) | Issue ready for Worker OR PR ready for Reviewer |
| `loom:in-progress` | 🟡 Amber | Issues & PRs | Worker / Reviewer | Issue: Worker implementing<br>PR: Reviewer reviewing OR Worker addressing feedback |
| `loom:pr` | 🔵 Blue | PRs | Reviewer | Approved PR ready for human to merge |
| `loom:blocked` | 🔴 Red | Issues | Worker | Implementation blocked, needs help |

**Key insights**:
- **Blue badges** (`loom:proposal`, `loom:pr`) = Human action needed
- **Green badges** (`loom:ready`) = Bot action needed
- **Amber badges** (`loom:in-progress`) = Work in progress
- Users control the flow by removing `loom:proposal` to approve Architect suggestions.

## Commands Reference

### Architect
```bash
# Check existing proposals (don't spam)
gh issue list --label="loom:proposal" --state=open

# Create new improvement proposal (any domain)
gh issue create --title "..." --body "..."

# Add proposal label (blue badge - awaiting user approval)
gh issue edit <number> --add-label "loom:proposal"
```

### User (Manual)
```bash
# Find proposals awaiting approval (blue badges)
gh issue list --label="loom:proposal" --state=open

# Approve a proposal (remove blue badge)
gh issue edit <number> --remove-label "loom:proposal"

# Reject a proposal
gh issue close <number> --comment "Not needed because..."

# Find PRs ready to merge (blue badges)
gh pr list --label="loom:pr" --state=open

# Merge approved PR
gh pr merge <number>
```

### Curator
```bash
# Find approved issues (no loom:proposal, not yet ready/in-progress)
gh issue list --state=open --json number,title,labels \
  --jq '.[] | select(([.labels[].name] | inside(["loom:proposal", "loom:ready", "loom:in-progress"]) | not)) | "#\(.number) \(.title)"'

# Mark issue as ready (add green badge)
gh issue edit <number> --add-label "loom:ready"
```

### Worker
```bash
# Find ready issues (green badges)
gh issue list --label="loom:ready" --state=open

# Claim issue (green → amber)
gh issue edit <number> --remove-label "loom:ready" --add-label "loom:in-progress"

# Create PR with green badge (ready for Reviewer)
gh pr create --title "..." --body "Closes #X" --label "loom:ready"

# Mark blocked (amber → red)
gh issue edit <number> --add-label "loom:blocked"
gh issue comment <number> --body "Blocked because..."
```

### Reviewer
```bash
# Find PRs ready to review (green badges)
gh pr list --label="loom:ready" --state=open

# Claim PR for review (green → amber)
gh pr edit <number> --remove-label "loom:ready" --add-label "loom:in-progress"

# Check out and test
gh pr checkout <number>
pnpm check:all

# Approve PR
gh pr review <number> --approve --body "LGTM!"
gh pr edit <number> --remove-label "loom:in-progress" --add-label "loom:pr"

# Request changes
gh pr review <number> --request-changes --body "Issues found..."
# Keep loom:in-progress - Worker will address
```

## Configuration

Each role has default settings in `.loom/roles/<role>.json`:

```json
{
  "name": "Curator Bot",
  "description": "Processes unlabeled issues",
  "defaultInterval": 300000,
  "defaultIntervalPrompt": "Find unlabeled issues...",
  "autonomousRecommended": true,
  "suggestedWorkerType": "claude"
}
```

Users can override these defaults in the Terminal Settings modal.

## Best Practices

### For Users

1. **Review suggestions promptly**: Architect, worker, and reviewer suggestions need approval
2. **Remove suggestion labels to accept**: Unlabeled = approved for processing
3. **Close unwanted suggestions**: Don't leave suggestions hanging
4. **Review PRs before merging**: Approved ≠ automatically merge

### For Agents

1. **Stay in your lane**: Don't do other roles' work
2. **Complete current task first**: Don't get sidetracked by discoveries
3. **Document thoroughly**: Future agents need context
4. **Use labels correctly**: Label workflow keeps everyone coordinated
5. **Reference issues**: Always link to related work

### For Autonomous Operation

1. **Curator + Reviewer + Architect**: Best combination for autonomous mode
2. **Worker**: Usually manual, autonomous only for maintenance work
3. **Interval settings**: Curator/Reviewer 5min, Architect 15min
4. **Monitor blocked issues**: Auto-resolve or escalate to user

## Troubleshooting

### Issue stuck without labels
→ Curator should pick it up within 5 minutes (if autonomous)
→ Manually add `loom:ready` if urgent

### Issue labeled loom:ready but not claimed
→ Worker agents may be disabled
→ Manually assign or claim with different worker

### PR labeled loom:review-requested but not reviewed
→ Reviewer agent may be disabled
→ Manually review or remove label to skip

### Multiple agents claiming same issue/PR
→ Labels should prevent this (first agent removes trigger label)
→ If race condition: coordinate manually, one agent backs off

## Future Enhancements

- **Automatic label transitions**: Remove manual label management
- **Priority labels**: `P0`, `P1`, `P2` for urgent vs normal vs low priority
- **Specialization labels**: `frontend`, `backend`, `ui`, `api` for agent specialization
- **Automated merging**: Auto-merge approved PRs after CI passes
- **Workload balancing**: Distribute issues across multiple worker agents
- **Progress tracking**: Dashboards showing agent activity and velocity
