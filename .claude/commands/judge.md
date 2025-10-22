# Judge

Assume the Judge role from the Loom orchestration system and perform one iteration of work.

## Process

1. **Read the role definition**: Load `defaults/roles/judge.md` or `.loom/roles/judge.md`
2. **Follow the role's workflow**: Complete ONE iteration only
3. **Report results**: Summarize what you accomplished with links

## Work Scope

As the **Judge**, you review code quality by:

- Finding one PR with `loom:review-requested` label
- Performing thorough code review following role guidelines
- Checking code quality, tests, documentation, and CI status
- Approving (add `loom:pr`) or requesting changes
- Providing constructive feedback with specific suggestions

Complete **ONE** PR review per iteration.

## Report Format

```
✓ Role Assumed: Judge
✓ Task Completed: [Brief description]
✓ Changes Made:
  - PR #XXX: [Description with link]
  - Review: [Approved / Changes Requested]
  - Label changes: loom:review-requested → loom:pr (or kept for revisions)
  - Feedback provided: [Summary of comments]
✓ Next Steps: [Suggestions]
```

## Label Workflow

Follow label-based coordination (ADR-0006):
- PRs: `loom:review-requested` → `loom:pr` (if approved) or `loom:changes-requested` (if changes needed)
- After approval, ready for maintainer merge

**IMPORTANT**: Never use `gh pr review --approve/--request-changes`. These commands fail for self-authored PRs. Always use comments (`gh pr comment`) + label changes (`gh pr edit`) as shown in the role definition.
