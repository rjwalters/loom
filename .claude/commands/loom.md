# Assume Loom Role

Select and assume an archetypal role from the Loom orchestration system using a priority-based heuristic, then perform one iteration of work following that role's guidelines.

## Process

1. **Run work detection**: Execute the heuristic commands below to check for available work
2. **Select optimal role**: Use the priority-based decision tree to choose the best role
3. **Report selection**: Explain which role was chosen and why (transparency)
4. **Read the role definition**: Load the markdown file for the selected role
5. **Follow the role's workflow**: Complete ONE iteration only (one task, one PR review, one issue triage, etc.)
6. **Report results**: Summarize what you accomplished with links to issues/PRs modified

### Step 1: Work Detection Commands

Run these commands to detect available work across all roles:

```bash
# Priority 1: URGENT work
URGENT_ISSUES=$(gh issue list --label="loom:issue" --label="loom:urgent" --state=open --json number 2>/dev/null | jq length 2>/dev/null || echo "0")
CHANGES_REQUESTED=$(gh pr list --label="loom:changes-requested" --state=open --json number 2>/dev/null | jq length 2>/dev/null || echo "0")

# Priority 2: READY work
READY_ISSUES=$(gh issue list --label="loom:issue" --state=open --json number 2>/dev/null | jq length 2>/dev/null || echo "0")
REVIEW_REQUESTED=$(gh pr list --label="loom:review-requested" --state=open --json number 2>/dev/null | jq length 2>/dev/null || echo "0")

# Priority 3: ENHANCEMENT work
UNLABELED=$(gh issue list --state=open --json number,labels 2>/dev/null | jq '[.[] | select(([.labels[].name] | inside(["loom:architect", "loom:hermit", "loom:curated", "loom:issue", "loom:in-progress"]) | not))] | length' 2>/dev/null || echo "0")
ARCHITECT_COUNT=$(gh issue list --label="loom:architect" --state=open --json number 2>/dev/null | jq length 2>/dev/null || echo "0")
```

### Step 2: Priority-Based Role Selection

Use this decision tree to select the optimal role:

```bash
# Priority 1: URGENT work (highest priority)
if [ "$URGENT_ISSUES" -gt 0 ]; then
  ROLE="builder"
  echo "🔴 URGENT: Selected Builder ($URGENT_ISSUES urgent issue(s) need immediate implementation)"

elif [ "$CHANGES_REQUESTED" -gt 0 ]; then
  ROLE="healer"
  echo "🔴 URGENT: Selected Healer ($CHANGES_REQUESTED PR(s) need fixes from review feedback)"

# Priority 2: READY work (high priority)
elif [ "$READY_ISSUES" -gt 0 ] || [ "$REVIEW_REQUESTED" -gt 0 ]; then
  # Both ready: weighted random (Builder 3x more likely than Judge)
  if [ "$READY_ISSUES" -gt 0 ] && [ "$REVIEW_REQUESTED" -gt 0 ]; then
    TOTAL=$((READY_ISSUES * 3 + REVIEW_REQUESTED * 2))
    RAND=$((RANDOM % TOTAL))
    if [ "$RAND" -lt $((READY_ISSUES * 3)) ]; then
      ROLE="builder"
      echo "🟢 READY: Selected Builder ($READY_ISSUES issue(s), $REVIEW_REQUESTED PR(s) - weighted random)"
    else
      ROLE="judge"
      echo "🟢 READY: Selected Judge ($READY_ISSUES issue(s), $REVIEW_REQUESTED PR(s) - weighted random)"
    fi
  # Only issues ready
  elif [ "$READY_ISSUES" -gt 0 ]; then
    ROLE="builder"
    echo "🟢 READY: Selected Builder ($READY_ISSUES issue(s) ready for implementation)"
  # Only PRs ready
  else
    ROLE="judge"
    echo "🟢 READY: Selected Judge ($REVIEW_REQUESTED PR(s) ready for review)"
  fi

# Priority 3: ENHANCEMENT work (medium priority)
elif [ "$UNLABELED" -gt 0 ]; then
  ROLE="curator"
  echo "🔵 ENHANCEMENT: Selected Curator ($UNLABELED unlabeled issue(s) need curation)"

elif [ "$ARCHITECT_COUNT" -lt 3 ]; then
  ROLE="architect"
  echo "🔵 ENHANCEMENT: Selected Architect ($(( 3 - ARCHITECT_COUNT )) proposal slot(s) available)"

# Priority 4: MAINTENANCE (fallback)
else
  # Rotate between Hermit and Guide
  if [ $((RANDOM % 2)) -eq 0 ]; then
    ROLE="hermit"
    echo "⚪ MAINTENANCE: Selected Hermit (scanning for code bloat and complexity)"
  else
    ROLE="guide"
    echo "⚪ MAINTENANCE: Selected Guide (triaging backlog and priorities)"
  fi
fi
```

### Step 3: Execute Selected Role

After selection, assume the chosen role and follow its workflow:

```bash
# Example: If Builder was selected
# Read defaults/roles/builder.md and follow its guidelines
```

## Role Selection Heuristic

Instead of random selection, the `/loom` command uses a **priority-based heuristic** that checks for available work before selecting a role. This improves efficiency by ensuring each iteration does meaningful work.

### Priority Levels

1. **🔴 URGENT** - Critical work requiring immediate attention
2. **🟢 READY** - Work ready for implementation or review
3. **🔵 ENHANCEMENT** - Curation, proposals, and quality improvements
4. **⚪ MAINTENANCE** - Triage, organization, and exploration

### Decision Algorithm

The heuristic runs these checks in order:

```bash
# Priority 1: URGENT work
URGENT_ISSUES=$(gh issue list --label="loom:issue" --label="loom:urgent" --state=open --json number | jq length)
CHANGES_REQUESTED=$(gh pr list --label="loom:changes-requested" --state=open --json number | jq length)

if [ "$URGENT_ISSUES" -gt 0 ]; then
  ROLE="builder"
  echo "🔴 Selected Builder (urgent issues: $URGENT_ISSUES)"
elif [ "$CHANGES_REQUESTED" -gt 0 ]; then
  ROLE="healer"
  echo "🔴 Selected Healer (PRs need fixes: $CHANGES_REQUESTED)"

# Priority 2: READY work
elif [ ... ready issues or PRs exist ... ]; then
  # Weighted random between Builder (3x) and Judge (2x)
  ROLE="builder" or "judge"
  echo "🟢 Selected $ROLE (ready work available)"

# Priority 3: ENHANCEMENT work
elif [ ... unlabeled issues exist ... ]; then
  ROLE="curator"
  echo "🔵 Selected Curator (unlabeled issues: $UNLABELED)"

# Priority 4: MAINTENANCE (fallback)
else
  # Rotate between Architect, Hermit, Guide
  ROLE="architect|hermit|guide"
  echo "⚪ Selected $ROLE (maintenance mode)"
fi
```

### Why This Works

- **Higher efficiency**: ~80-90% of `/loom` runs do meaningful work (vs 60-70% with random)
- **Better resource use**: Urgent work gets immediate attention
- **Transparent**: Each selection reports WHY the role was chosen
- **Flexible**: Easy to tune weights and priorities

## Available Roles

- **builder.md** - Claim `loom:ready` issue, implement feature/fix, create PR with `loom:review-requested`
- **judge.md** - Review PR with `loom:review-requested`, approve or request changes, update labels
- **curator.md** - Find unlabeled issue, enhance with technical details, mark as `loom:ready`
- **architect.md** - Create architectural proposal issue with `loom:architect` label
- **hermit.md** - Analyze codebase complexity, create bloat removal issue with `loom:hermit`
- **healer.md** - Fix bug or address PR feedback, maintain existing PRs
- **guide.md** - Triage batch of issues, update priorities and labels for workflow
- **driver.md** - Execute direct task or command (plain shell, no specific workflow)

## Work Scope

Complete **ONE** meaningful task following the selected role's guidelines, then **stop and report**.

### Task Examples by Role

**Builder**: Claim one `loom:ready` issue → implement → test → commit → create PR
**Judge**: Review one PR with `loom:review-requested` → provide feedback → approve/request changes
**Curator**: Find one unlabeled issue → add context → tag as `loom:ready`
**Architect**: Identify one architectural need → create detailed proposal issue
**Hermit**: Analyze codebase section → identify bloat → create removal issue
**Healer**: Fix one bug or address one PR comment → update PR → push changes
**Guide**: Review issue backlog → update priorities → add/remove labels for batch
**Driver**: Execute one direct command or task per user instruction

## Report Format

After completing your iteration, report:

```
✓ Role Assumed: [Role Name]
✓ Task Completed: [Brief description]
✓ Changes Made:
  - [Issue/PR #XXX]: [Description with link]
  - [Label changes]
  - [Files modified if applicable]
✓ Next Steps: [Suggestions for follow-up or ready state]
```

## Label Workflow

Follow the label-based coordination system (ADR-0006):

- Issues: `loom:ready` → `loom:in-progress` → closed
- PRs: `loom:review-requested` → `loom:approved` → merged
- Proposals: `loom:architect` → reviewed → implemented or closed
- Suggestions: `loom:hermit` → reviewed → implemented or closed

## Notes

- This command simulates one terminal's work in the Loom multi-terminal orchestration system
- Multiple Claude Code sessions can run `/loom` in parallel for distributed work
- Each iteration should be atomic and complete (don't leave partial work)
- Always update labels appropriately to signal state to other roles
- Check for existing work to avoid conflicts (e.g., don't claim already `in-progress` issues)

## Example Usage

```bash
# In Claude Code terminal
/loom

# Claude responds:
"🎭 Rolling random role... Assuming the Judge role for this iteration.

Looking for PRs with loom:review-requested...
Found PR #401 - 'Add terminal restart functionality'

[Performs detailed review following judge.md guidelines]

✓ Role Assumed: Judge
✓ Task Completed: Reviewed PR #401
✓ Changes Made:
  - PR #401: Added detailed review comment with 3 suggestions
  - Label changed: loom:review-requested → loom:approved
  - Approved PR for merge
✓ Next Steps: Ready for maintainer to merge

Review complete!"
```
