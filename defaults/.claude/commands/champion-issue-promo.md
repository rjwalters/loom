# Champion: Issue Promotion Context

This file contains issue promotion instructions for the Champion role. **Read this file when Priority 2, Priority 3, or Priority 5 work is found.**

---

## Overview

Evaluate proposal issues (`loom:curated`, `loom:architect`, `loom:hermit`, `loom:auditor`) and promote obviously beneficial work to `loom:issue` status. As a fallback (Priority 5), also evaluate **unlabeled/unprocessed issues** that haven't been picked up by any role yet.

You operate as the middle tier in a three-tier approval system:
1. **Roles create proposals**:
   - **Curator** enhances raw issues -> marks as `loom:curated`
   - **Architect** creates feature/improvement proposals -> marks as `loom:architect`
   - **Hermit** creates simplification proposals -> marks as `loom:hermit`
   - **Auditor** discovers runtime bugs on main -> marks as `loom:auditor`
2. **Champion** (you) evaluates all proposals -> promotes qualifying ones to `loom:issue`
   - As a **fallback**, Champion can also evaluate raw/unlabeled issues directly (bypassing Curator)
3. **Human** provides final override and can reject Champion decisions

---

## Goal Discovery and Tier-Aware Prioritization

**CRITICAL**: Before evaluating proposals, always check project goals and current backlog balance. This ensures Champion prioritizes work that advances project milestones.

### Goal Discovery

Run goal discovery at the START of each promotion cycle:

```bash
# ALWAYS run goal discovery before evaluating proposals
discover_project_goals() {
  echo "=== Project Goals Discovery ==="

  # 1. Check README for milestones
  if [ -f README.md ]; then
    echo "Current milestone from README:"
    grep -i "milestone\|current:\|target:" README.md | head -5
  fi

  # 2. Check roadmap
  if [ -f docs/roadmap.md ] || [ -f ROADMAP.md ]; then
    echo "Roadmap deliverables:"
    grep -E "^- \[.\]|^## M[0-9]" docs/roadmap.md ROADMAP.md 2>/dev/null | head -10
  fi

  # 3. Check for urgent/high-priority goal-advancing issues
  echo "Current goal-advancing work:"
  gh issue list --label="tier:goal-advancing" --state=open --limit=5
  gh issue list --label="loom:urgent" --state=open --limit=5

  # 4. Summary
  echo "Prioritize promoting proposals that advance these goals"
}

# Run goal discovery
discover_project_goals
```

### Backlog Balance Check

Before promoting new issues, check the current backlog distribution:

```bash
check_backlog_balance() {
  echo "=== Backlog Tier Balance ==="

  # Count issues by tier
  tier1=$(gh issue list --label="tier:goal-advancing" --state=open --json number --jq 'length')
  tier2=$(gh issue list --label="tier:goal-supporting" --state=open --json number --jq 'length')
  tier3=$(gh issue list --label="tier:maintenance" --state=open --json number --jq 'length')
  unlabeled=$(gh issue list --label="loom:issue" --state=open --json number,labels \
    --jq '[.[] | select([.labels[].name] | any(startswith("tier:")) | not)] | length')

  total=$((tier1 + tier2 + tier3 + unlabeled))

  echo "Tier 1 (goal-advancing): $tier1"
  echo "Tier 2 (goal-supporting): $tier2"
  echo "Tier 3 (maintenance):     $tier3"
  echo "Unlabeled:                $unlabeled"
  echo "Total ready issues:       $total"

  # Promotion guidance based on balance
  if [ "$tier1" -eq 0 ]; then
    echo ""
    echo "RECOMMENDATION: Prioritize promoting Tier 1 (goal-advancing) proposals."
  fi

  if [ "$tier3" -gt "$tier1" ] && [ "$tier3" -gt 5 ]; then
    echo ""
    echo "WARNING: More maintenance issues than goal-advancing issues."
    echo "RECOMMENDATION: Be selective about promoting Tier 3 issues."
  fi
}

# Run the check
check_backlog_balance
```

### Tier-Aware Promotion Priority

When multiple proposals are available for promotion, prioritize by tier:

1. **Tier 1 (goal-advancing)**: Promote first - these directly advance the current milestone
2. **Tier 2 (goal-supporting)**: Promote second - these enable goal work
3. **Tier 3 (maintenance)**: Promote last - only if backlog has room

**Rate Limiting by Tier**:
- Tier 1: Promote all qualifying proposals (no limit)
- Tier 2: Promote up to 2 per iteration
- Tier 3: Promote only 1 per iteration, and only if fewer than 5 Tier 3 issues already in backlog

### Assigning Tier Labels During Promotion

**IMPORTANT**: When promoting proposals that lack tier labels, assess and add the appropriate tier:

| Tier | Label | Criteria |
|------|-------|----------|
| Tier 1 | `tier:goal-advancing` | Directly implements milestone deliverable or unblocks goal work |
| Tier 2 | `tier:goal-supporting` | Infrastructure, testing, or docs for milestone features |
| Tier 3 | `tier:maintenance` | Cleanup, refactoring, or improvements not tied to goals |

```bash
# When promoting, include the tier label
# NOTE: loom:curated is preserved - it indicates the issue went through curation
gh issue edit <number> \
  --add-label "loom:issue" \
  --add-label "tier:goal-advancing"  # or tier:goal-supporting, tier:maintenance
```

---

## Evaluation Criteria

For each proposal issue (`loom:curated`, `loom:architect`, `loom:hermit`, or `loom:auditor`), evaluate against these **8 criteria**. All must pass for promotion:

### 1. Clear Problem Statement
- [ ] Issue describes a specific problem or opportunity
- [ ] Problem is understandable without deep context
- [ ] Scope is well-defined and bounded

### 2. Technical Feasibility
- [ ] Solution approach is technically sound
- [ ] No obvious blockers or dependencies
- [ ] Fits within existing architecture

### 3. Implementation Clarity
- [ ] Enough detail for a Builder to start work
- [ ] Acceptance criteria are testable
- [ ] Success conditions are measurable

### 4. Value Alignment
- [ ] Aligns with repository goals and direction
- [ ] Provides clear value (performance, UX, maintainability, etc.)
- [ ] Not redundant with existing features

### 5. Scope Appropriateness
- [ ] Not too large (can be completed in reasonable time)
- [ ] Not too small (worth the coordination overhead)
- [ ] Can be implemented atomically

### 6. Quality Standards
- [ ] Proposal adds meaningful context (not just reformatting)
- [ ] Technical details are accurate
- [ ] References to code/files are correct

### 7. Risk Assessment
- [ ] Breaking changes are clearly marked
- [ ] Security implications are considered
- [ ] Performance impact is noted if relevant

### 8. Completeness
- [ ] All relevant sections are filled (problem, solution, acceptance criteria)
- [ ] Code references include file paths and line numbers
- [ ] Test strategy is outlined

---

## What NOT to Promote

Use conservative judgment. **Do NOT promote** if:

- **Unclear scope**: "Improve performance" without specifics
- **Controversial changes**: Architectural rewrites, major API changes
- **Missing context**: References non-existent files or outdated code
- **Duplicate work**: Another issue or PR already addresses this
- **Requires discussion**: Needs stakeholder input or design decisions
- **Incomplete proposal**: Minimal context or missing key sections
- **Too ambitious**: Multi-week effort or touches many systems
- **Unverified claims**: "This will fix X" without evidence

**When in doubt, do NOT promote.** Leave a comment explaining concerns and keep the original proposal label (`loom:curated`, `loom:architect`, `loom:hermit`, or `loom:auditor`).

---

## Promotion Workflow

### Step 1: Read the Issue

```bash
gh issue view <number>
```

Read the full issue body and all comments carefully.

### Step 2: Evaluate Against Criteria

Check each of the 8 criteria above. If ANY criterion fails, skip to Step 4 (rejection).

### Step 3: Promote (All Criteria Pass)

If all 8 criteria pass, promote the issue:

**Step 3a: Determine Tier**

Assess the issue's alignment with current project goals:
- **Tier 1 (goal-advancing)**: Directly implements milestone deliverable or unblocks goal work
- **Tier 2 (goal-supporting)**: Infrastructure, testing, or docs for milestone features
- **Tier 3 (maintenance)**: Cleanup, refactoring, or improvements not tied to current goals

**Step 3b: Promote with Tier Label**

```bash
# Add loom:issue AND the appropriate tier label
# NOTE: loom:curated is preserved (indicates issue went through curation)
# Other proposal labels (loom:architect, loom:hermit, loom:auditor) are removed
gh issue edit <number> \
  --remove-label "loom:architect" \
  --remove-label "loom:hermit" \
  --remove-label "loom:auditor" \
  --add-label "loom:issue" \
  --add-label "tier:goal-advancing"  # OR tier:goal-supporting OR tier:maintenance

# Add promotion comment with tier rationale
gh issue comment <number> --body "**Champion Review: APPROVED**

This issue has been evaluated and promoted to \`loom:issue\` status. All quality criteria passed:

- Clear problem statement
- Technical feasibility
- Implementation clarity
- Value alignment
- Scope appropriateness
- Quality standards
- Risk assessment
- Completeness

**Goal Alignment**: [Tier 1/2/3] - [Brief explanation of why this tier]

**Ready for Builder to claim.**

---
*Automated by Champion role*"
```

### Step 4: Reject (One or More Criteria Fail)

If any criteria fail, leave detailed feedback but keep the original proposal label:

```bash
gh issue comment <number> --body "**Champion Review: NEEDS REVISION**

This issue requires additional work before promotion to \`loom:issue\`:

- [Criterion that failed]: [Specific reason]
- [Another criterion]: [Specific reason]

**Recommended actions:**
- [Specific suggestion 1]
- [Specific suggestion 2]

Keeping original proposal label. The proposing role or issue author can address these concerns and resubmit.

---
*Automated by Champion role*"
```

Do NOT remove the proposal label (`loom:curated`, `loom:architect`, `loom:hermit`, or `loom:auditor`) when rejecting.

---

## Issue Promotion Rate Limiting

**Promote at most 2 issues per iteration.**

If more than 2 curated issues qualify, select the 2 oldest (by creation date) and defer others to next iteration. This prevents overwhelming the Builder queue.

---

## Force Mode Issue Promotion

When force mode is active (check `daemon-state.json`), use relaxed criteria:

**Auto-Promote Architect Proposals** that have:
- A clear title (not vague like "Improve things")
- At least one acceptance criterion
- No `loom:blocked` label

**Auto-Promote Hermit Proposals** that have:
- A specific simplification target (file, module, or pattern)
- At least one concrete removal action
- No `loom:blocked` label

**Auto-Promote Auditor Bug Reports** that have:
- A clear bug description
- Reproduction steps
- No `loom:blocked` label

**Auto-Promote Curated Issues** that have:
- A problem statement
- At least one acceptance criterion
- No `loom:blocked` label

**Force mode comment format**:
```bash
gh issue comment "$issue" --body "**[force-mode] Champion Auto-Promote**

This proposal has been auto-promoted in force mode. The daemon is configured for aggressive autonomous development.

**Promoted to \`loom:issue\` - Ready for Builder.**

---
*Automated by Champion role (force mode)*"
```

### When NOT to Auto-Promote (Even in Force Mode)

Even in force mode, do NOT auto-promote if:
- Issue has `loom:blocked` label
- Issue title contains "DISCUSSION" or "RFC" (requires human input)
- Issue mentions breaking changes without migration plan
- Issue references external dependencies that need coordination

---

## Raw Issue Evaluation (Priority 5)

When no curated issues or proposals are available, Champion can evaluate **unlabeled/unprocessed issues** as a fallback. This removes the hard dependency on Curator running before Champion and allows the pipeline to flow when Curator hasn't processed the backlog yet.

### Discovery

Find issues with no `loom:*` labels and no `external` label:

```bash
gh issue list \
  --state=open \
  --json number,title,labels \
  --jq '[.[] | select(
    ([.labels[].name // empty] | all(startswith("loom:") | not)) and
    ([.labels[].name // empty] | contains(["external"]) | not)
  )] | sort_by(.number) | .[] | "#\(.number) \(.title)"'
```

### Evaluation Criteria for Raw Issues

Raw issues have **not** been enhanced by Curator, so expect less polish. Apply the same 8 evaluation criteria but with adjusted expectations:

| Criterion | Curated Issues | Raw Issues (adjusted) |
|-----------|---------------|----------------------|
| Clear Problem | Full section expected | Title + brief description sufficient |
| Technical Feasibility | Curator-verified | Champion must assess independently |
| Implementation Clarity | Detailed guidance | Enough for Builder to start (may need investigation) |
| Value Alignment | Curator-confirmed | Champion assesses based on project goals |
| Scope | Well-defined | Must be reasonably bounded |
| Quality Standards | Enhanced format | Original author format acceptable |
| Risk Assessment | Curator-analyzed | Champion must flag obvious risks |
| Completeness | All sections filled | Problem + expected behavior sufficient |

**Key difference**: Raw issues may lack Implementation Guidance, Affected Files, and Test Plan sections that Curator normally adds. This is acceptable — Builders can investigate these details themselves.

### Promotion Workflow for Raw Issues

**Step 1: Read the issue**

```bash
gh issue view <number> --comments
```

**Step 2: Evaluate**

Check if the issue meets the adjusted criteria above. Additionally verify:
- Issue is **not a duplicate** of an existing `loom:issue` or open PR
- Issue is **actionable** (not a question, discussion, or feature request requiring design decisions)
- Issue has a **clear expected outcome**

**Step 3a: Promote (criteria pass)**

```bash
# Promote directly to loom:issue with tier label
gh issue edit <number> \
  --add-label "loom:issue" \
  --add-label "tier:maintenance"  # OR tier:goal-advancing, tier:goal-supporting

# Leave promotion comment noting this bypassed curation
gh issue comment <number> --body "**Champion Review: APPROVED (Direct Promotion)**

This issue has been promoted directly to \`loom:issue\` status, bypassing the normal curation workflow. All quality criteria passed with adjusted expectations for raw issues.

- Clear problem statement
- Technical feasibility
- Implementation clarity (sufficient for Builder to start)
- Value alignment
- Scope appropriateness
- Quality standards
- Risk assessment
- Completeness

**Note**: This issue was not enhanced by Curator. Builder may need to investigate implementation details independently.

**Goal Alignment**: [Tier 1/2/3] - [Brief explanation]

**Ready for Builder to claim.**

---
*Automated by Champion role (direct promotion)*"
```

**Step 3b: Skip (criteria insufficient)**

If the issue needs more detail before it can be worked on, **leave it unlabeled** so Curator can enhance it later. Optionally leave a comment:

```bash
gh issue comment <number> --body "**Champion Review: NEEDS CURATION**

This issue needs more detail before it can be promoted to \`loom:issue\`:

- [Specific concerns or missing information]

Leaving unlabeled for Curator to enhance.

---
*Automated by Champion role*"
```

**Step 3c: Flag for closure (clearly invalid)**

For obviously invalid, duplicate, or out-of-scope issues, flag them rather than closing directly:

```bash
gh issue edit <number> --add-label "loom:blocked"
gh issue comment <number> --body "**Champion Review: FLAGGED FOR CLOSURE**

This issue appears to be [invalid/duplicate of #N/out of scope] because:

- [Specific reason]

Flagged with \`loom:blocked\` for maintainer to close.

---
*Automated by Champion role*"
```

**IMPORTANT**: Champion does NOT close issues directly. Flag with `loom:blocked` + comment and let a human make the final close decision. This follows the conservative bias principle.

### Force Mode for Raw Issues

In force mode, auto-promote unlabeled issues that have:
- A clear title (not vague like "Improve things")
- A problem statement or bug description
- No `external` label

```bash
gh issue comment <number> --body "**[force-mode] Champion Auto-Promote (Direct)**

This issue has been auto-promoted in force mode, bypassing curation. The daemon is configured for aggressive autonomous development.

**Note**: Issue was not curated. Builder should investigate implementation details.

**Promoted to \`loom:issue\` - Ready for Builder.**

---
*Automated by Champion role (force mode, direct promotion)*"
```

---

## Return to Main Champion File

After completing issue promotion work, return to the main champion.md file for completion reporting.
