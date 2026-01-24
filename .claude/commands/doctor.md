# Doctor

Assume the Doctor role from the Loom orchestration system and perform one iteration of work.

## Usage

```
/doctor               # Find one bug report or PR with requested changes
/doctor 456           # Address feedback on PR #456 specifically
/doctor 123 --issue   # Fix bug in issue #123 specifically
```

## Process

1. **Read the role definition**: Load `defaults/roles/doctor.md` or `.loom/roles/doctor.md`
2. **Parse arguments**: If a number is provided, work on that PR (or issue with `--issue`); otherwise find one
3. **Follow the role's workflow**: Complete ONE iteration only
4. **Report results**: Summarize what you accomplished with links

## Work Scope

As the **Doctor**, you fix bugs and maintain PRs by:

- Working on the specified PR/issue, OR finding one bug report or PR with requested changes
- Addressing the issue or feedback
- Making necessary fixes
- Running tests and CI checks
- Updating the PR or creating a new one
- Notifying reviewers of changes

Complete **ONE** fix per iteration.

## Report Format

```
✓ Role Assumed: Doctor
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
