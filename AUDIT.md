# Role Definition Audit: loom:issue Label Gate

## Overview

This document audits all role definitions to ensure they have clear, explicit instructions about the `loom:issue` label gate, which maintains human-in-the-loop approval.

## Audit Date

2025-10-25

## Label Gate Policy

**`loom:issue` label indicates human-approved work ready for implementation.**

### Who CAN add `loom:issue`:
- ✅ **Humans** (repository maintainers)
- ✅ **Champion** (authorized to approve on behalf of humans)

### Who CANNOT add `loom:issue`:
- ❌ **Curator** - Creates `loom:curated`, waits for human approval
- ❌ **Architect** - Creates `loom:architect`, waits for human approval
- ❌ **Hermit** - Creates `loom:hermit`, waits for human approval
- ❌ **Guide** - Manages priorities, doesn't approve work
- ❌ **Builder** - Only consumes `loom:issue`, never adds it
- ❌ **Judge** - Reviews PRs, doesn't work with issues
- ❌ **Doctor** - Fixes PR feedback, doesn't work with issues

## Role-by-Role Audit

### ✅ Builder (.loom/roles/builder.md)

**Status**: PASS - Explicit prohibition

**Current Instructions**:
```
IMPORTANT: Ignore External Issues

- NEVER work on issues with the `external` label
- External issues are submitted by non-collaborators
- Focus only on issues labeled `loom:issue` without the `external` label
```

**Label Instructions**:
- ✅ Clear that Builder ONLY works on `loom:issue`
- ✅ States "Find work: gh issue list --label='loom:issue'"
- ✅ Workflow shows claiming (not adding) `loom:issue`

**Recommendations**: NONE - Instructions are clear

---

### ⚠️ Curator (.loom/roles/curator.md)

**Status**: NEEDS IMPROVEMENT - Implicit prohibition

**Current Instructions** (scattered throughout):
```
- Add loom:curated label
- Users approve curated issues by:
  - Adding loom:issue label (if ready for work)
```

**Issues**:
- ❌ Instructions are scattered across multiple sections
- ❌ No prominent warning about NOT adding `loom:issue`
- ❌ Relies on implication rather than explicit prohibition

**Recommendations**:
1. Add explicit warning box at top of role definition
2. State clearly: "NEVER add loom:issue label - only humans can approve work"
3. Consolidate label workflow into single section

---

### ⚠️ Architect (.loom/roles/architect.md)

**Status**: NEEDS IMPROVEMENT - Implicit prohibition

**Current Instructions**:
```
Create new issue with:
- Title: [Architect] <proposal title>
- Body: Complete proposal
- Label: loom:architect

If approved by human:
- They will add loom:issue label
- Builder will implement
```

**Issues**:
- ❌ No explicit prohibition statement
- ❌ No warning box
- ❌ Relies on workflow description alone

**Recommendations**:
1. Add explicit warning: "NEVER add loom:issue - wait for human approval"
2. Add prominent warning box in "Your Role" section
3. Clarify approval flow upfront

---

### ⚠️ Hermit (.loom/roles/hermit.md)

**Status**: NEEDS IMPROVEMENT - Minimal mention

**Current Instructions**:
```
Create removal proposal issue with:
- Label: loom:hermit
- Wait for human approval before implementation
```

**Issues**:
- ❌ Mentions `loom:issue` only 2 times
- ❌ No explicit prohibition
- ❌ No warning about label gate

**Recommendations**:
1. Add explicit statement: "NEVER add loom:issue to your proposals"
2. Add warning box at top
3. Clarify that human adds `loom:issue` after approving proposal

---

### ✅ Champion (.loom/roles/champion.md)

**Status**: PASS - Clear authorization

**Current Instructions**:
```
Champion is AUTHORIZED to add loom:issue label as proxy for human approval.

Label Management Powers:
- Add loom:issue to approve curated/architect/hermit proposals
- Add loom:urgent for critical issues
```

**Label Instructions**:
- ✅ Explicitly states Champion CAN add `loom:issue`
- ✅ Clear about proxy approval authority
- ✅ Lists when to approve (curated/architect/hermit proposals)

**Recommendations**: NONE - Instructions are clear

---

### ⚠️ Guide (.loom/roles/guide.md)

**Status**: NEEDS IMPROVEMENT - Ambiguous

**Current Instructions**:
```
Workflow:
- Review issue backlog
- Update priorities (loom:urgent label)
- Organize labels
```

**Issues**:
- ❌ Mentions `loom:issue` 6 times but never states prohibition
- ❌ Might organize/review `loom:issue` issues but shouldn't add label
- ❌ Role is about triage, not approval

**Recommendations**:
1. Add explicit: "Guide DOES NOT approve work - never add loom:issue"
2. Clarify that Guide can VIEW `loom:issue` issues but not add label
3. Distinguish between organizing existing labels vs adding approval labels

---

### ❓ Judge (.loom/roles/judge.md)

**Status**: ACCEPTABLE - Works on PRs not issues

**Current Instructions**:
- Focuses on PR review
- Works with `loom:review-requested` and `loom:pr` labels
- No mention of `loom:issue` at all

**Issues**:
- ℹ️ Doesn't need `loom:issue` instructions (works on PRs)
- ℹ️ Could add note for completeness

**Recommendations**:
1. (Optional) Add note: "Judge reviews PRs, not issues - does not use loom:issue label"

---

### ❓ Doctor (.loom/roles/doctor.md)

**Status**: ACCEPTABLE - Works on PRs/fixes not issue approval

**Current Instructions**:
- Fixes bugs and PR feedback
- No mention of `loom:issue` at all

**Issues**:
- ℹ️ Doesn't need `loom:issue` instructions (works on existing issues/PRs)
- ℹ️ Could add note for completeness

**Recommendations**:
1. (Optional) Add note: "Doctor fixes existing issues/PRs - does not approve new work"

---

### ℹ️ Driver (.loom/roles/driver.md)

**Status**: N/A - Manual shell role

**Current Instructions**:
- Plain shell environment
- No label workflows

**Issues**: None

**Recommendations**: None needed

---

## Summary Statistics

| Role | Mentions | Explicit Prohibition | Clear Instructions | Status |
|------|----------|---------------------|-------------------|--------|
| Builder | 24 | ✅ Yes | ✅ Yes | PASS |
| Curator | 18 | ❌ No | ⚠️ Implicit | NEEDS IMPROVEMENT |
| Architect | 5 | ❌ No | ⚠️ Implicit | NEEDS IMPROVEMENT |
| Hermit | 2 | ❌ No | ⚠️ Minimal | NEEDS IMPROVEMENT |
| Champion | 9 | ✅ Yes (authorized) | ✅ Yes | PASS |
| Guide | 6 | ❌ No | ⚠️ Ambiguous | NEEDS IMPROVEMENT |
| Judge | 0 | N/A | N/A | ACCEPTABLE |
| Doctor | 0 | N/A | N/A | ACCEPTABLE |
| Driver | 0 | N/A | N/A | N/A |

## Required Changes

### High Priority (Label Approval Roles)

1. **Curator**: Add explicit prohibition warning
2. **Architect**: Add explicit prohibition warning
3. **Hermit**: Add explicit prohibition warning
4. **Guide**: Clarify non-approval role

### Low Priority (Optional Improvements)

5. **Judge**: Add note about PR-only scope
6. **Doctor**: Add note about fix-only scope

## Recommended Warning Template

For roles that CANNOT add `loom:issue`, add this warning box:

```markdown
## ⚠️ IMPORTANT: Label Gate Policy

**NEVER add the `loom:issue` label to issues.**

Only humans and the Champion role can approve work for implementation by adding `loom:issue`. Your role is to [prepare/propose/triage] issues, not approve them.

**Your workflow**:
1. [Do your work: curate/propose/triage]
2. Add your role's label: `loom:[curator|architect|hermit|etc]`
3. **WAIT for human approval**
4. Human adds `loom:issue` if approved
5. Builder implements approved work
```

For roles authorized to add `loom:issue` (Champion):

```markdown
## ✅ Authorization: Label Gate

**You ARE authorized to add `loom:issue` as a proxy for human approval.**

This is a special privilege. Use it to approve high-quality proposals that are ready for implementation.
```

## Next Steps

1. Update Curator role with explicit prohibition
2. Update Architect role with explicit prohibition
3. Update Hermit role with explicit prohibition
4. Update Guide role to clarify non-approval scope
5. (Optional) Add clarifying notes to Judge and Doctor
6. Test updated roles with agents to verify clarity
