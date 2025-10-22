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

## Activity Logging

IMPORTANT: Log your activity to enable smart role selection and analytics.

### At Work Start

```typescript
import { logActivity } from './src/lib/activity-logger';

const startTime = Date.now();
const workspacePath = process.cwd(); // Or use workspace detection

await logActivity(workspacePath, {
  timestamp: new Date().toISOString(),
  role: 'healer',
  trigger: 'slash-command',
  work_found: false, // Update after checking for work
  outcome: 'in-progress'
});
```

### At Work End

```typescript
await logActivity(workspacePath, {
  timestamp: new Date().toISOString(),
  role: 'healer',
  trigger: 'slash-command',
  work_found: true, // or false if no work found
  work_completed: true, // or false if blocked/incomplete
  issue_number: 123, // If applicable
  duration_ms: Date.now() - startTime,
  outcome: 'completed', // or 'no-work', 'blocked', 'error'
  notes: 'Fixed bug #123, addressed PR feedback'
});
```

### Outcome Values

- `completed`: Successfully finished work
- `no-work`: No bugs or PRs needing fixes
- `blocked`: Found work but couldn't proceed (dependencies, etc.)
- `error`: Encountered error during execution

### Error Handling

Wrap logging in try/catch to ensure it never breaks your work:

```typescript
try {
  await logActivity(workspacePath, { /* ... */ });
} catch (error) {
  console.error('[activity-logger] Failed to log:', error);
  // Continue with work
}
```

## Label Workflow

Follow label-based coordination (ADR-0006):
- For PRs with requested changes: Address feedback → update PR → notify reviewer
- For bugs: Fix issue → test → create/update PR with `loom:review-requested`
