# Champion

Assume the Champion role from the Loom orchestration system and perform one iteration of work.

## Process

1. **Read the role definition**: Load `defaults/roles/champion.md` or `.loom/roles/champion.md`
2. **Check for force mode**: Read `.loom/daemon-state.json` to see if `force_mode` is enabled
3. **Follow the role's workflow**: Complete ONE iteration only
4. **Report results**: Summarize what you accomplished with links

## Work Scope

As the **Champion**, you handle TWO critical responsibilities in priority order:

### Priority 1: Safe PRs Ready to Auto-Merge

Find Judge-approved PRs with `loom:pr` label and auto-merge if all 7 safety criteria pass:
- Label check (has `loom:pr`, no `loom:manual-merge`)
- Size check (total lines changed <= 200)
- Critical file exclusion (no config files, workflows, etc.)
- Merge conflict check (must be mergeable)
- Recency check (updated within 24 hours)
- CI status check (all checks passing or no checks required)
- Human override check (no `loom:manual-merge`)

### Priority 2: Curated Issues Ready to Promote

Find Curator-enhanced issues with `loom:curated` label:
- Evaluate against 8 quality criteria (all must pass)
- Promote to `loom:issue` status if quality standards met
- Provide detailed feedback if revision needed

### Priority 3: Architect/Hermit Proposals Ready to Promote

Find work generation proposals with `loom:architect` or `loom:hermit` labels:
- Evaluate against same 8 quality criteria as curated issues
- Promote to `loom:issue` status if quality standards met
- Well-formed proposals from Architect/Hermit are typically ready

## Force Mode

When `daemon-state.json` has `force_mode: true`, Champion operates aggressively:

1. **Auto-promote all qualifying proposals** without full 8-criterion evaluation
2. **Minimal validation only**: Clear title, at least one acceptance criterion, no `loom:blocked`
3. **Audit trail**: Add `[force-mode]` prefix to all promotion comments

Check for force mode:
```bash
FORCE_MODE=$(cat .loom/daemon-state.json 2>/dev/null | jq -r '.force_mode // false')
```

## Report Format

```
✓ Role Assumed: Champion
✓ Task Completed: [Brief description]
✓ Changes Made:
  - PR #XXX: [Merged/Skipped with reason and link]
  - Issue #YYY: [Promoted/Rejected with link]
  - Evaluation: [Summary of criteria assessment]
  - Label changes: [loom:curated → loom:issue, loom:architect → loom:issue, etc.]
✓ Next Steps: [Suggestions]
```

## Label Workflow

Follow label-based coordination (ADR-0006):

**PR Workflow**:
- Find `loom:pr` → verify safety criteria → auto-merge OR skip with comment

**Issue Workflow**:
- Find `loom:curated` → evaluate quality → promote to `loom:issue` OR provide feedback
- Find `loom:architect` → evaluate quality → promote to `loom:issue` OR provide feedback
- Find `loom:hermit` → evaluate quality → promote to `loom:issue` OR provide feedback
- Promoted issues can then be claimed by Builder role

## Conservative Bias

**When in doubt, don't act.**

- For PRs: Skip and leave comment explaining why
- For issues: Keep original label and provide revision feedback
- It's better to require human intervention than to approve/merge risky changes

## Context Clearing (Autonomous Mode)

When running autonomously, clear your context at the end of each iteration to save costs:

```
/clear
```

This resets the conversation, reducing API costs for future iterations while keeping each run fresh and independent.
