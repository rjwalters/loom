# Loom Label Workflow

This document describes how GitHub labels coordinate work between AI agents and humans in Loom.

## Core Concept: Color-Coded Workflow

Labels use color to indicate who should act:

- **🔵 Blue** = Human action needed
- **🟢 Green** = Loom bot action needed
- **🟡 Amber** = Work in progress
- **🔴 Red** = Blocked, needs help

## Label Definitions

| Label | Color | Used On | Meaning |
|-------|-------|---------|---------|
| `loom:issue` | Blue (3B82F6) | Issues | New issue awaiting user triage/approval |
| `loom:ready` | Green (10B981) | Issues & PRs | Issue ready for Worker OR PR ready for Reviewer |
| `loom:in-progress` | Amber (F59E0B) | Issues & PRs | Issue: Worker implementing<br>PR: Reviewer reviewing OR Worker addressing feedback |
| `loom:pr` | Blue (3B82F6) | PRs | Approved by Reviewer, ready for human to merge |
| `loom:blocked` | Red (EF4444) | Issues | Implementation blocked, needs help |

## Complete Workflow

### Issue Lifecycle

```
┌─────────────────────────────────────────────────────────┐
│ 1. ISSUE CREATION                                       │
│    Anyone creates unlabeled issue                       │
│    (User, Worker, Reviewer, or Architect scan)          │
└────────────────────┬────────────────────────────────────┘
                     │
                     ↓
┌─────────────────────────────────────────────────────────┐
│ 2. ARCHITECT TRIAGE                                     │
│    Architect finds unlabeled issues                     │
│    Adds: loom:issue (BLUE)                              │
│    (or closes if not viable)                            │
└────────────────────┬────────────────────────────────────┘
                     │
                     ↓
┌─────────────────────────────────────────────────────────┐
│ 3. USER APPROVAL                                        │
│    User reviews blue loom:issue badges                  │
│    Removes: loom:issue (approves)                       │
│    (or closes issue to reject)                          │
└────────────────────┬────────────────────────────────────┘
                     │
                     ↓
┌─────────────────────────────────────────────────────────┐
│ 4. CURATOR ENHANCEMENT                                  │
│    Curator finds unlabeled, approved issues             │
│    Adds implementation details, test plans              │
│    Adds: loom:ready (GREEN)                             │
└────────────────────┬────────────────────────────────────┘
                     │
                     ↓
┌─────────────────────────────────────────────────────────┐
│ 5. WORKER IMPLEMENTATION                                │
│    Worker finds green loom:ready issues                 │
│    Removes: loom:ready                                  │
│    Adds: loom:in-progress (AMBER)                       │
│    Implements feature, creates commits                  │
│    Creates PR with "Closes #X"                          │
│    Adds to PR: loom:ready (GREEN - ready for Reviewer)  │
└────────────────────┬────────────────────────────────────┘
                     │
                     ↓
                  (Issue stays loom:in-progress until PR merged)
                     │
                     ↓
┌─────────────────────────────────────────────────────────┐
│ 6. PR MERGED                                            │
│    Issue automatically closes (via "Closes #X")         │
│    Worker terminal can be discarded                     │
└─────────────────────────────────────────────────────────┘
```

### PR Lifecycle

```
┌─────────────────────────────────────────────────────────┐
│ 1. PR CREATED BY WORKER                                 │
│    Worker creates PR with "Closes #X"                   │
│    Adds: loom:ready (GREEN - ready for Reviewer)        │
└────────────────────┬────────────────────────────────────┘
                     │
                     ↓
┌─────────────────────────────────────────────────────────┐
│ 2. REVIEWER REVIEWS                                     │
│    Reviewer finds green loom:ready PRs                  │
│    Removes: loom:ready                                  │
│    Adds: loom:in-progress (AMBER - reviewing)           │
│    Checks out branch, runs tests, reviews code          │
└────────────────────┬────────────────────────────────────┘
                     │
                     ↓
           ┌─────────┴──────────┐
           │                    │
           ↓                    ↓
    Changes Needed         Approved
           │                    │
           ↓                    ↓
┌──────────────────────┐ ┌──────────────────────┐
│ gh pr review         │ │ gh pr review         │
│ --request-changes    │ │ --approve            │
│                      │ │                      │
│ Keeps:               │ │ Removes:             │
│ loom:in-progress     │ │ loom:in-progress     │
│                      │ │                      │
│ Worker addresses     │ │ Adds:                │
│ feedback, comments   │ │ loom:pr (BLUE)       │
│ when ready           │ │                      │
│                      │ │ Ready for human!     │
└──────────┬───────────┘ └──────────┬───────────┘
           │                        │
           │                        ↓
           │              ┌──────────────────────┐
           │              │ HUMAN MERGE          │
           │              │ User sees blue badge │
           │              │ Merges PR            │
           │              │ Issue auto-closes    │
           │              └──────────────────────┘
           │
           └──> Loop back to Reviewer
```

## Agent Roles and Commands

### Architect Bot

**Watches for:** Unlabeled issues

**Actions:**
```bash
# Find unlabeled issues to triage
gh issue list --state=open --label=""

# Triage as viable issue
gh issue edit <#> --add-label "loom:issue"

# Reject as not viable
gh issue close <#> --comment "Reason..."
```

### User (Manual - Human in the Loop)

**Watches for:** Blue badges (`loom:issue`, `loom:pr`)

**Actions:**
```bash
# Find issues awaiting approval (blue badges)
gh issue list --label="loom:issue"

# Approve issue (remove blue badge)
gh issue edit <#> --remove-label "loom:issue"

# Find PRs ready to merge (blue badges)
gh pr list --label="loom:pr"

# Merge approved PR
gh pr merge <#>
```

### Curator Bot

**Watches for:** Unlabeled, approved issues (no `loom:issue` label)

**Actions:**
```bash
# Find approved issues (unlabeled, not loom:issue, open)
gh issue list --state=open | grep -v "loom:"

# Enhance and mark as ready (green badge)
gh issue edit <#> --add-label "loom:ready"
```

### Worker Bot

**Watches for:** Green badges on issues (`loom:ready`)

**Actions:**
```bash
# Find issues ready to implement (green badges)
gh issue list --label="loom:ready"

# Claim issue (green → amber)
gh issue edit <#> --remove-label "loom:ready" --add-label "loom:in-progress"

# Create PR with green badge (ready for Reviewer)
gh pr create --title "..." --body "Closes #X" --label "loom:ready"

# Mark blocked if stuck (amber → red)
gh issue edit <#> --add-label "loom:blocked"

# Address Reviewer feedback
# - PR keeps loom:in-progress while Worker fixes
# - Worker comments on PR when changes ready
# - Reviewer will re-review
```

### Reviewer Bot

**Watches for:** Green badges on PRs (`loom:ready`)

**Actions:**
```bash
# Find PRs ready to review (green badges)
gh pr list --label="loom:ready"

# Start review (green → amber)
gh pr edit <#> --remove-label "loom:ready" --add-label "loom:in-progress"

# Check out and test
gh pr checkout <#>
pnpm check:all

# Request changes (keeps amber badge)
gh pr review <#> --request-changes --body "Issues found..."
# PR keeps loom:in-progress - Worker will address

# Approve PR (amber → blue for human)
gh pr review <#> --approve --body "LGTM!"
gh pr edit <#> --remove-label "loom:in-progress" --add-label "loom:pr"
```

## Visual Workflow Summary

**What each color means:**

- 🔵 **Blue badge** = "Human, please look at this!"
  - `loom:issue` on issues = "Triage/approve this issue"
  - `loom:pr` on PRs = "Merge this PR"

- 🟢 **Green badge** = "Loom bots, take action!"
  - `loom:ready` on issues = "Worker: implement this"
  - `loom:ready` on PRs = "Reviewer: review this"

- 🟡 **Amber badge** = "Work in progress, please wait"
  - `loom:in-progress` on issues = "Worker is implementing"
  - `loom:in-progress` on PRs = "Reviewer reviewing OR Worker fixing"

- 🔴 **Red badge** = "Blocked, needs help"
  - `loom:blocked` on issues = "Worker stuck, needs assistance"

## Key Principles

### 1. Worker Owns Issue Until Merge

- Worker claims issue by adding `loom:in-progress` (amber)
- Worker creates PR with `loom:ready` (green - for Reviewer)
- Worker monitors PR and addresses Reviewer feedback
- Issue keeps `loom:in-progress` until PR merges
- Worker terminal can be discarded after merge

### 2. Simple Color Scanning

Users can quickly scan for:
- **Blue badges** = "I need to do something"
- **Green badges** = "Bots will handle these"
- **Amber badges** = "Work is happening"
- **Red badges** = "Something is stuck"

### 3. Minimal Label Transitions

Issues progress through 3 states:
```
(none) → loom:issue → (none) → loom:ready → loom:in-progress → (closed)
        (blue)     (approved) (green)      (amber)
```

PRs have even simpler lifecycle:
```
(none) → loom:ready → loom:in-progress → loom:pr → (merged)
        (green)      (amber)             (blue)
```

## Workflow Examples

### Example 1: Feature Implementation (Happy Path)

```bash
# 1. Architect creates and triages issue
gh issue create --title "Add search to terminal history"
gh issue edit 42 --add-label "loom:issue"

# 2. User approves (removes blue badge)
gh issue edit 42 --remove-label "loom:issue"

# 3. Curator enhances (adds green badge)
gh issue edit 42 --add-label "loom:ready"

# 4. Worker claims (green → amber)
gh issue edit 42 --remove-label "loom:ready" --add-label "loom:in-progress"

# 5. Worker creates PR (green badge for Reviewer)
gh pr create --title "Add search to terminal history" \
  --body "Closes #42" \
  --label "loom:ready"

# 6. Reviewer reviews (green → amber)
gh pr edit 50 --remove-label "loom:ready" --add-label "loom:in-progress"
gh pr checkout 50
pnpm check:all

# 7. Reviewer approves (amber → blue for human)
gh pr review 50 --approve
gh pr edit 50 --remove-label "loom:in-progress" --add-label "loom:pr"

# 8. User merges (blue badge)
gh pr merge 50
# Issue 42 auto-closes
```

### Example 2: Reviewer Requests Changes

```bash
# ... same as above through step 6 ...

# 7. Reviewer requests changes (keeps amber)
gh pr review 50 --request-changes --body "Please add tests for edge cases"
# PR keeps loom:in-progress

# 8. Worker addresses feedback
# - Worker fixes code
# - Worker comments: "Added tests, ready for re-review"
# - PR still has loom:in-progress

# 9. Reviewer re-reviews (keeps amber during review)
gh pr checkout 50
pnpm check:all

# 10. Reviewer approves (amber → blue)
gh pr review 50 --approve
gh pr edit 50 --remove-label "loom:in-progress" --add-label "loom:pr"

# 11. User merges
gh pr merge 50
```

### Example 3: Worker Discovers Issue During Work

```bash
# 1. Worker is implementing issue 42
# Worker discovers: State management needs refactoring

# 2. Worker creates new unlabeled issue
gh issue create --title "Refactor state management to use reducer pattern" \
  --body "Discovered during #42 implementation..."

# 3. Worker completes original issue 42
# Don't get distracted!

# 4. Later, Architect triages new issue
gh issue edit 55 --add-label "loom:issue"

# 5. User approves
gh issue edit 55 --remove-label "loom:issue"

# 6. Curator enhances
gh issue edit 55 --add-label "loom:ready"

# 7. Another worker picks it up...
```

## Label Setup

Run the label setup utility to create all workflow labels:

```bash
# Test script (dry run)
./scripts/test-label-setup.sh

# From Loom UI (future)
# Tools → Setup Workflow Labels...
```

Labels are idempotent - safe to run multiple times.

## Migration from Old Labels

If you have existing repos with old label scheme:

**Old labels to delete:**
- `loom:architect-suggestion` → now `loom:issue`
- `loom:accepted` → workflow simplified, not needed
- `loom:review-requested` → workflow simplified, not needed
- `loom:reviewing` → now uses `loom:in-progress`

**Migration commands:**
```bash
# Delete old labels
gh label delete "loom:architect-suggestion" --yes
gh label delete "loom:accepted" --yes
gh label delete "loom:review-requested" --yes
gh label delete "loom:reviewing" --yes

# Create new labels (or use setupLoomLabels() in UI)
./scripts/test-label-setup.sh
```

## Troubleshooting

### Issue stuck without label
→ Architect should triage within 15 minutes (if autonomous)
→ User can manually add `loom:issue` if urgent

### Issue stuck with blue `loom:issue` badge
→ Needs user approval
→ User must remove `loom:issue` label to proceed

### Issue stuck with green `loom:ready` badge
→ No Worker available/enabled
→ User can manually claim or create Worker terminal

### PR stuck with green `loom:ready` badge
→ No Reviewer available/enabled
→ User can manually review or merge

### PR stuck with amber `loom:in-progress` badge
→ Either Reviewer is reviewing OR Worker is addressing feedback
→ Check PR comments for status
→ If stalled, ping the Worker or Reviewer

## Best Practices

### For Users (Humans)

1. **Scan for blue badges** = Your action needed
2. **Trust green badges** = Bots will handle
3. **Ignore amber badges** = Work in progress
4. **Watch red badges** = May need intervention

### For Agents (Bots)

1. **Architect**: Add `loom:issue` to all viable new issues
2. **Curator**: Add `loom:ready` after enhancement
3. **Worker**: Claim with `loom:in-progress`, create PRs with `loom:ready`
4. **Reviewer**: Move PRs from green → amber → blue
5. **All agents**: Create unlabeled issues for discoveries

### For Autonomous Operation

1. **Recommended autonomous bots**: Architect, Curator, Reviewer
2. **Worker**: Usually manual (one Worker per PR, discarded after merge)
3. **Interval settings**:
   - Architect: 15 minutes (scan for new issues)
   - Curator: 5 minutes (enhance approved issues)
   - Reviewer: 5 minutes (review ready PRs)
