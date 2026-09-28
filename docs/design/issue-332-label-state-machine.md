# Issue #332: Revised Label State Machine

## Problem Statement

The current label workflow has several issues:
1. **Ambiguous `loom:ready`**: Overloaded for both issues AND PRs
2. **Automatic approval**: Curator marks issues ready without human approval
3. **Inconsistent naming**: `loom:proposal` vs `loom:hermit`
4. **Duplicate labels**: Both `loom:pr` and `loom:pr` for merged PRs
5. **Unclear external contributor path**: How do external issues enter the workflow?

## Design Goals

1. ✅ **Human-in-the-loop approval**: Explicit human approval before work begins
2. ✅ **Clear single-purpose labels**: Each label has one meaning
3. ✅ **Consistent naming**: All suggestion types follow same pattern
4. ✅ **Support external contributors**: Clear path for non-Architect issues
5. ✅ **Preserve Triage autonomy**: Triage can still set the urgent label (retired by #9244)

## New Label Set

### Issue Labels

| Label | Color | Set By | Meaning |
|-------|-------|--------|---------|
| `loom:architect` | 🟣 #9333EA | Architect | Proposal awaiting human review |
| `loom:hermit` | 🟣 #9333EA | Critic | Removal/simplification awaiting review |
| `loom:curated` | 🟢 #10B981 | Curator | Enhanced with implementation details |
| `loom:issue` | 🔵 #3B82F6 | **Human** | **Approved for work** (replaces `loom:ready`) |
| `loom:building` | 🟡 #F59E0B | Worker | Being implemented |
| `loom:blocked` | 🔴 #EF4444 | Anyone | Implementation blocked |
| urgent (retired, #9244) | 🔴 #DC2626 | Triage/Human | High priority (max 3) |

### PR Labels

| Label | Color | Set By | Meaning |
|-------|-------|--------|---------|
| `loom:review-requested` | 🟢 #10B981 | Worker/Fixer | Ready for Reviewer |
| `loom:changes-requested` | 🟡 #F59E0B | Reviewer | Needs fixes |
| `loom:pr` | 🔵 #3B82F6 | Reviewer | Approved, ready to merge |

**Removed Labels:**
- ❌ `loom:ready` (replaced by `loom:issue` for issues, `loom:review-requested` already exists for PRs)
- ❌ `loom:proposal` (renamed to `loom:architect`)
- ❌ `loom:pr` (duplicate of `loom:pr`)
- ❌ `loom:reviewing` (not needed - use assignee instead)

## State Transitions

### Issue Lifecycle: Internal (Architect-Generated)

```
┌─────────────────────────────────────────────────────────────┐
│ ARCHITECT: Creates proposal                                 │
│   Action: Create issue with loom:architect       │
└────────────────────────┬────────────────────────────────────┘
                         │
                         ↓
┌─────────────────────────────────────────────────────────────┐
│ HUMAN: Review proposal                                       │
│   Approve: Remove loom:architect                 │
│   Reject:  Close issue with comment                         │
└────────────────────────┬────────────────────────────────────┘
                         │ (approved - suggestion removed)
                         ↓
┌─────────────────────────────────────────────────────────────┐
│ CURATOR: Enhance issue                                       │
│   Action: Add implementation details                        │
│   Result: Add loom:curated                                  │
└────────────────────────┬────────────────────────────────────┘
                         │
                         ↓
┌─────────────────────────────────────────────────────────────┐
│ HUMAN: Approve for work                                      │
│   Action: Add loom:issue                                    │
│   Note:   This signals "ready for Worker"                   │
└────────────────────────┬────────────────────────────────────┘
                         │
                         ↓
┌─────────────────────────────────────────────────────────────┐
│ TRIAGE: (Optional) Prioritize                                │
│   Action: Add urgent label if strategic/time-sensitive      │
│   Note:   Max 3 issues can be urgent                        │
└────────────────────────┬────────────────────────────────────┘
                         │
                         ↓
┌─────────────────────────────────────────────────────────────┐
│ WORKER: Claim and implement                                  │
│   Action: Remove loom:issue, add loom:building           │
│   Result: Create PR with loom:review-requested              │
└─────────────────────────────────────────────────────────────┘
```

### Issue Lifecycle: External (Contributor-Generated)

```
┌─────────────────────────────────────────────────────────────┐
│ EXTERNAL CONTRIBUTOR: Creates issue                          │
│   Action: Create issue (no loom: labels initially)          │
└────────────────────────┬────────────────────────────────────┘
                         │
                         ↓
┌─────────────────────────────────────────────────────────────┐
│ CURATOR: Process external issue                              │
│   Action: Enhance issue with implementation details         │
│   Result: Add loom:curated                                  │
└────────────────────────┬────────────────────────────────────┘
                         │
                         ↓
┌─────────────────────────────────────────────────────────────┐
│ HUMAN: Triage and approve                                    │
│   Approve: Add loom:issue                                   │
│   Reject:  Close with explanation                           │
└────────────────────────┬────────────────────────────────────┘
                         │
                         ↓
                    (continues as internal flow)
```

### Issue Lifecycle: Critic-Generated

```
┌─────────────────────────────────────────────────────────────┐
│ CRITIC: Identifies bloat/over-engineering                    │
│   Action: Create issue with loom:hermit          │
└────────────────────────┬────────────────────────────────────┘
                         │
                         ↓
┌─────────────────────────────────────────────────────────────┐
│ HUMAN: Review suggestion                                     │
│   Approve: Remove loom:hermit                    │
│   Reject:  Close issue                                      │
└────────────────────────┬────────────────────────────────────┘
                         │ (approved - continues as internal flow)
                         ↓
                    (curator enhances...)
```

### PR Lifecycle

```
┌─────────────────────────────────────────────────────────────┐
│ WORKER: Creates PR                                           │
│   Action: Create PR with loom:review-requested              │
└────────────────────────┬────────────────────────────────────┘
                         │
                         ↓
┌─────────────────────────────────────────────────────────────┐
│ REVIEWER: Review PR                                          │
│   Approve: Remove loom:review-requested, add loom:pr        │
│   Changes: Remove loom:review-requested, add                │
│           loom:changes-requested                            │
└────────────────────────┬────────────────────────────────────┘
                         │
                ┌────────┴────────┐
                │                 │
                ↓                 ↓
    ┌───────────────────┐   ┌────────────────────────┐
    │ HUMAN: Merge      │   │ FIXER: Address feedback│
    │  Action: Merge PR │   │  Result: Add           │
    │                   │   │   loom:review-requested│
    └───────────────────┘   └────────┬───────────────┘
                                     │
                                     ↓
                            (returns to Reviewer)
```

## Agent Behavior Changes

### Curator

**Old Behavior:**
- Finds approved issues (no `loom:proposal`, not `loom:ready`/`loom:building`)
- Enhances issue
- **Automatically adds `loom:ready`** ❌

**New Behavior:**
- Finds approved issues (no suggestion labels, not `loom:issue`/`loom:building`)
- Enhances issue
- **Adds `loom:curated` only** ✅
- **Human must explicitly add `loom:issue`** ✅

### Worker

**Old Behavior:**
- Searches for `loom:ready` issues

**New Behavior:**
- Searches for `loom:issue` issues
- Prioritizes urgent issues first
- Claims by removing `loom:issue`, adding `loom:building`

### Triage

**Old Behavior:**
- Manages the urgent label on `loom:ready` issues

**New Behavior:**
- Manages the urgent label on `loom:issue` issues
- Still maintains max 3 urgent issues
- Can add/remove the urgent label autonomously

### Architect

**Old Behavior:**
- Creates issues with `loom:proposal`

**New Behavior:**
- Creates issues with `loom:architect`
- Consistent with Critic naming pattern

### Critic

**Behavior:** No change (already uses `loom:hermit`)

### Reviewer

**Behavior:** No change (already uses `loom:review-requested`, `loom:changes-requested`, `loom:pr`)

### Fixer

**Behavior:** No change (already uses `loom:changes-requested` → `loom:review-requested`)

## Migration Strategy

### Phase 1: Create New Labels

```bash
# Create new labels
gh label create "loom:architect" --color "9333EA" --description "Architect proposal awaiting human review"
gh label create "loom:curated" --color "10B981" --description "Enhanced by Curator, awaiting human approval"
gh label edit "loom:issue" --color "3B82F6" --description "Approved for work by human (replaces loom:ready)"
```

### Phase 2: Migrate Existing Issues

```bash
# Find issues with loom:proposal, rename to loom:architect
gh issue list --label="loom:proposal" --json number --jq '.[].number' | \
  xargs -I {} gh issue edit {} --remove-label "loom:proposal" --add-label "loom:architect"

# Find issues with loom:ready, change to loom:issue
gh issue list --label="loom:ready" --json number --jq '.[].number' | \
  xargs -I {} gh issue edit {} --remove-label "loom:ready" --add-label "loom:issue"
```

### Phase 3: Migrate Existing PRs

```bash
# PRs already use loom:review-requested correctly
# Remove loom:pr if present, replace with loom:pr
gh pr list --label="loom:pr" --json number --jq '.[].number' | \
  xargs -I {} gh pr edit {} --remove-label "loom:pr" --add-label "loom:pr"
```

### Phase 4: Delete Old Labels

```bash
# After confirming migration complete
gh label delete "loom:ready"
gh label delete "loom:proposal"
gh label delete "loom:pr"
gh label delete "loom:reviewing"  # if exists
```

## Benefits

1. ✅ **Clear human approval gate**: `loom:issue` is explicit approval signal
2. ✅ **Curator can't bypass human**: Must wait for human to add `loom:issue`
3. ✅ **Consistent naming**: All suggestions follow `-suggestion` pattern
4. ✅ **No label overloading**: Each label has single purpose
5. ✅ **External contributor path**: Clear flow through Curator → Human approval
6. ✅ **Preserved autonomy**: Triage can still manage priorities

## Implementation Checklist

- [ ] Create new labels in GitHub
- [ ] Update WORKFLOWS.md
- [ ] Update ADR 0006
- [ ] Update README.md
- [ ] Update all 8 role files (defaults/roles/)
- [ ] Update docs/philosophy/agent-archetypes.md
- [ ] Update docs/guides/architecture-patterns.md
- [ ] Migrate existing labeled issues/PRs
- [ ] Delete old labels
- [ ] Test with example workflow
