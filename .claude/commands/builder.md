# Builder

Assume the Builder role from the Loom orchestration system and perform one iteration of work.

## Process

1. **Read the role definition**: Load `defaults/roles/builder.md` or `.loom/roles/builder.md`
2. **Follow the role's workflow**: Complete ONE iteration only
3. **Report results**: Summarize what you accomplished with links

## Work Scope

As the **Builder**, you implement features and fixes by:

- Finding one `loom:ready` issue
- Claiming it (remove `loom:ready`, add `loom:in-progress`)
- Creating a worktree with `./.loom/scripts/worktree.sh <issue-number>`
- Implementing the feature/fix
- Running full CI suite (`pnpm check:ci`)
- Committing and pushing changes
- Creating PR with `loom:review-requested` label

Complete **ONE** issue implementation per iteration.

## Report Format

```
✓ Role Assumed: Builder
✓ Task Completed: [Brief description]
✓ Changes Made:
  - Issue #XXX: [Description with link]
  - PR #XXX: [Description with link]
  - Label changes: loom:ready → loom:in-progress, PR tagged loom:review-requested
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
  role: 'builder',
  trigger: 'slash-command',
  work_found: false, // Update after checking for work
  outcome: 'in-progress'
});
```

### At Work End

```typescript
await logActivity(workspacePath, {
  timestamp: new Date().toISOString(),
  role: 'builder',
  trigger: 'slash-command',
  work_found: true, // or false if no work found
  work_completed: true, // or false if blocked/incomplete
  issue_number: 123, // If applicable
  duration_ms: Date.now() - startTime,
  outcome: 'completed', // or 'no-work', 'blocked', 'error'
  notes: 'Implemented feature X, created PR #456'
});
```

### Outcome Values

- `completed`: Successfully finished work
- `no-work`: No issues found to work on
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
- Issues: `loom:ready` → `loom:in-progress` → closed
- PRs: Create with `loom:review-requested` label for Judge review
