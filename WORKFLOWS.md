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
**Role**: Universal triage gatekeeper + improvement proposal generator

**Watches for**:
- Unlabeled issues (created by anyone - User, Worker, Reviewer, or Architect's own scans)

**Creates**:
- Unlabeled issues from codebase scans
- Adds `loom:issue` label after triaging issues

**Interval**: 15 minutes (recommended autonomous)

**Scope**: The Architect has two activities:
1. **Triage unlabeled issues**: Review ALL unlabeled issues, add `loom:issue` if viable or close if not
2. **Create new suggestions** (if no unlabeled issues exist): Scan codebase across all domains:
   - **Architecture & Features**: New features, API design, system improvements
   - **Code Quality**: Refactoring, consistency, duplication, unused code
   - **Documentation**: Outdated docs, missing explanations, API documentation
   - **Testing**: Missing coverage, flaky tests, edge cases
   - **CI/Build/Tooling**: Failing jobs, slow builds, outdated dependencies
   - **Performance & Security**: Optimizations, vulnerabilities, resource leaks

**Workflow**:
```
1. Check for unlabeled issues (gh issue list --label="")
2. If found: Triage each one - add loom:issue or close
3. If none: Scan codebase and create new unlabeled issue
4. Self-triage: Add loom:issue to own issues
5. Wait for user to remove loom:issue label (approval)
```

### 2. Curator Bot
**Role**: Enhances approved issues and marks them ready for implementation

**Watches for**:
- Unlabeled issues (no `loom:issue` label = user approved)

**Creates**:
- `loom:ready` - Issues ready for worker implementation

**Interval**: 5 minutes (recommended autonomous)

**Workflow**:
```
1. Find unlabeled issues (user has removed loom:issue = approved)
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
- Unlabeled issues - When discovering problems or opportunities during work
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
7. If discover issues: create unlabeled issue (Architect will triage)
```

### 4. Reviewer Bot
**Role**: Reviews pull requests

**Watches for**:
- `loom:ready` - PRs ready for review (green badges)

**Creates**:
- `loom:in-progress` - Claims PR for review (amber)
- `loom:pr` - Approved PRs ready for human to merge (blue)
- Unlabeled issues - Bugs or problems discovered in existing code

**Interval**: 5 minutes (recommended autonomous)

**Workflow**:
```
1. Find loom:ready PRs (green badges)
2. Claim by removing loom:ready, adding loom:in-progress (amber)
3. Check out branch, run tests, review code
4. If changes needed: gh pr review --request-changes, keep loom:in-progress
5. If approved: gh pr review --approve, remove loom:in-progress, add loom:pr (blue)
6. If discover bug in existing code: create unlabeled issue (Architect will triage)
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
│ 1. ARCHITECT CREATES SUGGESTION                             │
│    gh issue create (no label)                               │
│    Title: "Add search functionality to terminal history"    │
│    gh issue edit <#> --add-label "loom:architect-suggestion"│
└─────────────────────┬───────────────────────────────────────┘
                      │
                      ↓
┌─────────────────────────────────────────────────────────────┐
│ 2. USER REVIEWS AND ACCEPTS                                 │
│    Reviews issue with loom:architect-suggestion             │
│    Adds loom:accepted label to proceed                      │
└─────────────────────┬───────────────────────────────────────┘
                      │
                      ↓
┌─────────────────────────────────────────────────────────────┐
│ 3. CURATOR ENHANCES ISSUE                                   │
│    Finds loom:accepted issue #42                            │
│    Adds implementation details:                             │
│    - Multiple implementation options                        │
│    - Dependencies and risks                                 │
│    - Test plan checklist                                    │
│    Removes loom:accepted, adds loom:ready                   │
└─────────────────────┬───────────────────────────────────────┘
                      │
                      ↓
┌─────────────────────────────────────────────────────────────┐
│ 4. WORKER IMPLEMENTS                                        │
│    Finds loom:ready issue #42                               │
│    Updates: removes loom:ready, adds loom:in-progress      │
│    Implements feature, writes tests                         │
│    Creates PR: "Closes #42", adds loom:review-requested    │
└─────────────────────┬───────────────────────────────────────┘
                      │
                      ↓
┌─────────────────────────────────────────────────────────────┐
│ 5. REVIEWER REVIEWS PR                                      │
│    Finds loom:review-requested PR #50                       │
│    Updates: removes loom:review-requested, adds reviewing   │
│    Checks out branch, runs tests                            │
│    Reviews code, provides feedback                          │
│    Approves: gh pr review --approve                         │
│    Removes loom:reviewing                                   │
└─────────────────────┬───────────────────────────────────────┘
                      │
                      ↓
┌─────────────────────────────────────────────────────────────┐
│ 6. USER MERGES PR                                           │
│    Reviews approved PR                                      │
│    Merges to main                                           │
│    Issue #42 automatically closes                           │
└─────────────────────────────────────────────────────────────┘
```

### Feedback Loop: Worker Discovers Refactoring Opportunity

```
┌─────────────────────────────────────────────────────────────┐
│ Worker implements issue #42                                 │
│ Discovers: State management scattered across files         │
└─────────────────────┬───────────────────────────────────────┘
                      │
                      ↓
┌─────────────────────────────────────────────────────────────┐
│ Worker completes #42, then creates unlabeled issue:         │
│ gh issue create (no label)                                  │
│ Title: "Refactor state management to use reducer pattern"   │
│ Documents: problem, current code, proposed solution         │
└─────────────────────┬───────────────────────────────────────┘
                      │
                      ↓
┌─────────────────────────────────────────────────────────────┐
│ Architect triages unlabeled issue                           │
│ Evaluates priority and scope                                │
│ Adds loom:architect-suggestion                              │
│ Adds comment with guidance                                  │
└─────────────────────┬───────────────────────────────────────┘
                      │
                      ↓
┌─────────────────────────────────────────────────────────────┐
│ User reviews loom:architect-suggestion                      │
│ Adds loom:accepted to proceed                               │
└─────────────────────┬───────────────────────────────────────┘
                      │
                      ↓
┌─────────────────────────────────────────────────────────────┐
│ Curator processes loom:accepted refactor issue              │
│ Adds implementation details                                 │
│ Removes loom:accepted, adds loom:ready                      │
└─────────────────────┬───────────────────────────────────────┘
                      │
                      ↓
┌─────────────────────────────────────────────────────────────┐
│ Another worker picks up loom:ready refactor                 │
│ Implements, creates PR, requests review...                  │
└─────────────────────────────────────────────────────────────┘
```

### Feedback Loop: Reviewer Discovers Bug

```
┌─────────────────────────────────────────────────────────────┐
│ Reviewer reviews PR #50                                     │
│ Discovers bug in existing code (not introduced by this PR)  │
│ Bug: Terminal output corrupted with special characters      │
└─────────────────────┬───────────────────────────────────────┘
                      │
                      ↓
┌─────────────────────────────────────────────────────────────┐
│ Reviewer completes PR review, then creates unlabeled issue: │
│ gh issue create (no label)                                  │
│ Documents: reproduction, impact, root cause analysis        │
└─────────────────────┬───────────────────────────────────────┘
                      │
                      ↓
┌─────────────────────────────────────────────────────────────┐
│ Architect triages unlabeled issue                           │
│ Evaluates severity and priority                             │
│ Adds loom:architect-suggestion                              │
└─────────────────────┬───────────────────────────────────────┘
                      │
                      ↓
┌─────────────────────────────────────────────────────────────┐
│ User reviews loom:architect-suggestion                      │
│ Adds loom:accepted to proceed                               │
└─────────────────────┬───────────────────────────────────────┘
                      │
                      ↓
┌─────────────────────────────────────────────────────────────┐
│ Curator processes loom:accepted bug issue                   │
│ Adds test cases, acceptance criteria                        │
│ Removes loom:accepted, adds loom:ready                      │
└─────────────────────┬───────────────────────────────────────┘
                      │
                      ↓
┌─────────────────────────────────────────────────────────────┐
│ Worker picks up loom:ready bug fix                          │
│ Fixes bug, adds tests, creates PR...                        │
└─────────────────────────────────────────────────────────────┘
```

## Label Reference

| Label | Color | Used On | Created By | Meaning |
|-------|-------|---------|-----------|---------|
| (no label) | - | Issues | Anyone | Unreviewed issue - created by User, Worker, Reviewer, or Architect's scan |
| `loom:issue` | 🔵 Blue | Issues | Architect | Triaged issue awaiting user approval |
| `loom:ready` | 🟢 Green | Issues & PRs | Curator (issues) / Worker (PRs) | Issue ready for Worker OR PR ready for Reviewer |
| `loom:in-progress` | 🟡 Amber | Issues & PRs | Worker / Reviewer | Issue: Worker implementing<br>PR: Reviewer reviewing OR Worker addressing feedback |
| `loom:pr` | 🔵 Blue | PRs | Reviewer | Approved PR ready for human to merge |
| `loom:blocked` | 🔴 Red | Issues | Worker | Implementation blocked, needs help |

**Key insights**:
- **Blue badges** = Human action needed
- **Green badges** = Bot action needed
- **Amber badges** = Work in progress
- Users control the flow by removing `loom:issue` to approve suggestions.

## Commands Reference

### Architect
```bash
# Find unlabeled issues to triage
gh issue list --label="" --state=open

# Triage an issue (add blue badge)
gh issue edit <number> --add-label "loom:issue"

# Reject non-viable issue
gh issue close <number> --comment "Explanation of why not viable"

# Create new improvement suggestion (any domain)
gh issue create --title "..." --body "..."
gh issue edit <number> --add-label "loom:issue"
```

### User (Manual)
```bash
# Find issues awaiting approval (blue badges)
gh issue list --label="loom:issue" --state=open

# Approve an issue (remove blue badge)
gh issue edit <number> --remove-label "loom:issue"

# Reject an issue
gh issue close <number> --comment "Not needed because..."

# Find PRs ready to merge (blue badges)
gh pr list --label="loom:pr" --state=open

# Merge approved PR
gh pr merge <number>
```

### Curator
```bash
# Find approved issues (unlabeled, not loom:issue)
gh issue list --state=open | grep -v "loom:"

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

# Discover new issue during work (Architect will triage)
gh issue create --title "..." --body "..."
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

# Provide review
gh pr review <number> --approve --body "LGTM!"
gh pr edit <number> --remove-label "loom:in-progress" --add-label "loom:pr"

gh pr review <number> --request-changes --body "Issues found..."
# Keep loom:in-progress - Worker will address

# Discover bug in existing code (Architect will triage)
gh issue create --title "..." --body "..."
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
