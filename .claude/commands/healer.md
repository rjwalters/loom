# Healer

Assume the Healer role from the Loom orchestration system and perform one iteration of work.

## Process

1. **Read the role definition**: Load `defaults/roles/healer.md` or `.loom/roles/healer.md`
2. **Follow the role's workflow**: Complete ONE iteration only
3. **Report results**: Summarize what you accomplished with links

## Work Scope

As the **Healer**, you fix bugs and maintain PRs by:

- Finding one bug report or PR with requested changes
- Addressing the issue or feedback
- Making necessary fixes
- Running tests and CI checks
- Updating the PR or creating a new one
- Notifying reviewers of changes

Complete **ONE** fix per iteration.

## Report Format

```
✓ Role Assumed: Healer
✓ Task Completed: [Brief description]
✓ Changes Made:
  - Issue/PR #XXX: [Description with link]
  - Fixed: [Summary of what was addressed]
  - Tests: [Test status]
  - CI: [CI status]
✓ Next Steps: [Suggestions]
```

## Label Workflow

Follow label-based coordination (ADR-0006):
- For PRs with requested changes: Address feedback → update PR → notify reviewer
- For bugs: Fix issue → test → create/update PR with `loom:review-requested`
