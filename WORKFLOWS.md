# Loom Agent Workflows

This document describes the label-based workflows that coordinate multiple AI agents working together on a codebase.

## Overview

Loom uses GitHub labels as a coordination protocol. Each agent type has a specific role and watches for issues/PRs with particular labels. This creates a complete pipeline from idea generation through implementation to code review.

## Agent Types

### 1. Architect Bot
**Role**: Proposes new features and architectural improvements

**Watches for**:
- `loom:refactor-suggestion` - Reviews refactoring suggestions from workers
- `loom:bug-suggestion` - Reviews bug reports from reviewers

**Creates**:
- `loom:architect-suggestion` - New feature proposals and architectural changes

**Interval**: 15 minutes (recommended autonomous)

**Workflow**:
```
1. Review suggestions from other roles (refactors, bugs)
2. Approve (remove label) or reject (close with explanation)
3. Scan codebase for architectural opportunities
4. Create new issues with loom:architect-suggestion
```

### 2. Curator Bot
**Role**: Enhances unlabeled issues and marks them ready for implementation

**Watches for**:
- Issues with no labels (newly accepted by user)

**Creates**:
- `loom:ready` - Issues ready for worker implementation

**Interval**: 5 minutes (recommended autonomous)

**Workflow**:
```
1. Find unlabeled issues (user has accepted suggestion)
2. Review issue description and requirements
3. Add implementation details, test plans, code references
4. Document multiple implementation options if complex
5. Mark as loom:ready when enhancement complete
```

### 3. Worker Bot
**Role**: Implements features and fixes bugs

**Watches for**:
- `loom:ready` - Issues ready to be implemented

**Creates**:
- `loom:in-progress` - Claims issue for implementation
- `loom:refactor-suggestion` - Refactoring opportunities discovered
- `loom:review-requested` - PRs ready for review
- `loom:blocked` - When stuck on implementation

**Interval**: Disabled by default (on-demand)

**Workflow**:
```
1. Find loom:ready issues
2. Claim by removing loom:ready, adding loom:in-progress
3. Implement, test, commit
4. Create PR with "Closes #X", add loom:review-requested
5. If blocked: add loom:blocked with explanation
6. If discover refactoring need: create loom:refactor-suggestion
```

### 4. Reviewer Bot
**Role**: Reviews pull requests

**Watches for**:
- `loom:review-requested` - PRs ready for review

**Creates**:
- `loom:reviewing` - Claims PR for review
- `loom:bug-suggestion` - Bugs discovered in existing code

**Interval**: 5 minutes (recommended autonomous)

**Workflow**:
```
1. Find loom:review-requested PRs
2. Claim by removing loom:review-requested, adding loom:reviewing
3. Check out branch, run tests, review code
4. Approve or request changes via gh pr review
5. Remove loom:reviewing when complete
6. If discover bug in existing code: create loom:bug-suggestion
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
│    gh issue create --label "loom:architect-suggestion"      │
│    Title: "Add search functionality to terminal history"    │
└─────────────────────┬───────────────────────────────────────┘
                      │
                      ↓
┌─────────────────────────────────────────────────────────────┐
│ 2. USER REVIEWS AND ACCEPTS                                 │
│    Removes loom:architect-suggestion label                  │
│    Issue becomes unlabeled                                  │
└─────────────────────┬───────────────────────────────────────┘
                      │
                      ↓
┌─────────────────────────────────────────────────────────────┐
│ 3. CURATOR ENHANCES ISSUE                                   │
│    Finds unlabeled issue #42                                │
│    Adds implementation details:                             │
│    - Multiple implementation options                        │
│    - Dependencies and risks                                 │
│    - Test plan checklist                                    │
│    Marks as loom:ready                                      │
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
│ Worker completes #42, then creates new issue:               │
│ gh issue create --label "loom:refactor-suggestion"          │
│ Title: "Refactor state management to use reducer pattern"   │
│ Documents: problem, current code, proposed solution         │
└─────────────────────┬───────────────────────────────────────┘
                      │
                      ↓
┌─────────────────────────────────────────────────────────────┐
│ Architect reviews loom:refactor-suggestion                  │
│ Evaluates priority and scope                                │
│ Approves: removes loom:refactor-suggestion                  │
│ Adds comment with guidance                                  │
└─────────────────────┬───────────────────────────────────────┘
                      │
                      ↓
┌─────────────────────────────────────────────────────────────┐
│ Curator processes unlabeled refactor issue                  │
│ Adds implementation details                                 │
│ Marks as loom:ready                                         │
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
│ Reviewer completes PR review, then creates new issue:       │
│ gh issue create --label "loom:bug-suggestion"               │
│ Documents: reproduction, impact, root cause analysis        │
└─────────────────────┬───────────────────────────────────────┘
                      │
                      ↓
┌─────────────────────────────────────────────────────────────┐
│ Architect reviews loom:bug-suggestion                       │
│ Evaluates severity and priority                             │
│ Approves: removes loom:bug-suggestion                       │
└─────────────────────┬───────────────────────────────────────┘
                      │
                      ↓
┌─────────────────────────────────────────────────────────────┐
│ Curator processes unlabeled bug issue                       │
│ Adds test cases, acceptance criteria                        │
│ Marks as loom:ready                                         │
└─────────────────────┬───────────────────────────────────────┘
                      │
                      ↓
┌─────────────────────────────────────────────────────────────┐
│ Worker picks up loom:ready bug fix                          │
│ Fixes bug, adds tests, creates PR...                        │
└─────────────────────────────────────────────────────────────┘
```

## Label Reference

| Label | Created By | Reviewed By | Meaning |
|-------|-----------|-------------|---------|
| `loom:architect-suggestion` | Architect | User | New feature or architectural change proposal |
| `loom:refactor-suggestion` | Worker | Architect | Refactoring opportunity discovered during implementation |
| `loom:bug-suggestion` | Reviewer | Architect | Bug discovered in existing code during review |
| (no label) | User/Architect | Curator | Accepted suggestion awaiting enhancement |
| `loom:ready` | Curator | Worker | Issue ready for implementation |
| `loom:in-progress` | Worker | - | Issue currently being implemented |
| `loom:blocked` | Worker | User/Architect | Implementation blocked, needs help |
| `loom:review-requested` | Worker | Reviewer | PR ready for code review |
| `loom:reviewing` | Reviewer | - | PR currently under review |

## Commands Reference

### Architect
```bash
# Review suggestions from other roles
gh issue list --label="loom:refactor-suggestion" --state=open
gh issue list --label="loom:bug-suggestion" --state=open

# Approve suggestion (remove label)
gh issue edit <number> --remove-label "loom:refactor-suggestion"

# Reject suggestion
gh issue close <number> --comment "Explanation..."

# Create new feature suggestion
gh issue create --label "loom:architect-suggestion" --title "..." --body "..."
```

### Curator
```bash
# Find unlabeled issues to enhance
gh issue list --label="" --state=open

# Mark issue as ready
gh issue edit <number> --add-label "loom:ready"
```

### Worker
```bash
# Find ready issues
gh issue list --label="loom:ready" --state=open

# Claim issue
gh issue edit <number> --remove-label "loom:ready" --add-label "loom:in-progress"

# Create PR with review request
gh pr create --title "..." --body "Closes #X" --label "loom:review-requested"

# Mark blocked
gh issue edit <number> --add-label "loom:blocked"
gh issue comment <number> --body "Blocked because..."

# Create refactor suggestion
gh issue create --label "loom:refactor-suggestion" --title "..." --body "..."
```

### Reviewer
```bash
# Find PRs to review
gh pr list --label="loom:review-requested" --state=open

# Claim PR for review
gh pr edit <number> --remove-label "loom:review-requested" --add-label "loom:reviewing"

# Check out and test
gh pr checkout <number>
pnpm check:ci

# Provide review
gh pr review <number> --approve --body "LGTM! ..."
gh pr review <number> --request-changes --body "Issues found..."

# Complete review
gh pr edit <number> --remove-label "loom:reviewing"

# Create bug suggestion
gh issue create --label "loom:bug-suggestion" --title "..." --body "..."
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
