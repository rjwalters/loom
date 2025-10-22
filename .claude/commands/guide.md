# Guide

Assume the Guide role from the Loom orchestration system and perform one iteration of work.

## Process

1. **Read the role definition**: Load `defaults/roles/guide.md` or `.loom/roles/guide.md`
2. **Follow the role's workflow**: Complete ONE iteration only
3. **Report results**: Summarize what you accomplished with links

## Work Scope

As the **Guide**, you prioritize and organize work by:

- Triaging a batch of issues in the backlog
- Updating priorities based on project goals
- Adding or removing labels for workflow coordination
- Identifying related issues that should be grouped
- Suggesting milestone assignments
- Closing stale or duplicate issues

Complete **ONE** triage batch per iteration.

## Report Format

```
✓ Role Assumed: Guide
✓ Task Completed: [Brief description]
✓ Changes Made:
  - Triaged: [Number] issues
  - Issue #XXX: [Changes made]
  - Issue #YYY: [Changes made]
  - Label updates: [Summary]
  - Closed: [Any stale/duplicate issues]
✓ Next Steps: [Suggestions for prioritization]
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
  role: 'guide',
  trigger: 'slash-command',
  work_found: false, // Update after checking backlog
  outcome: 'in-progress'
});
```

### At Work End

```typescript
await logActivity(workspacePath, {
  timestamp: new Date().toISOString(),
  role: 'guide',
  trigger: 'slash-command',
  work_found: true, // or false if no triage needed
  work_completed: true, // or false if blocked/incomplete
  duration_ms: Date.now() - startTime,
  outcome: 'completed', // or 'no-work', 'blocked', 'error'
  notes: 'Triaged 5 issues, updated priorities'
});
```

### Outcome Values

- `completed`: Successfully finished work
- `no-work`: No issues needing triage
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
- Review issue backlog and update labels appropriately
- Ensure issues are properly categorized
- Identify issues ready for `loom:ready` label
- Close duplicates or stale issues
