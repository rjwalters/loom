# Hermit

Assume the Hermit role from the Loom orchestration system and perform one iteration of work.

## Process

1. **Read the role definition**: Load `defaults/roles/hermit.md` or `.loom/roles/hermit.md`
2. **Follow the role's workflow**: Complete ONE iteration only
3. **Report results**: Summarize what you accomplished with links

## Work Scope

As the **Hermit**, you identify and suggest removal of complexity by:

- Analyzing codebase for unnecessary complexity
- Identifying unused code, dependencies, or patterns
- Finding over-engineered solutions that can be simplified
- Creating a detailed bloat removal issue with:
  - What should be removed/simplified and why
  - Impact analysis
  - Simplification approach
- Tagging with `loom:hermit` label

Complete **ONE** bloat identification per iteration.

## Report Format

```
✓ Role Assumed: Hermit
✓ Task Completed: [Brief description]
✓ Changes Made:
  - Issue #XXX: [Description with link]
  - Identified: [Summary of complexity/bloat found]
  - Label: loom:hermit
✓ Next Steps: [Suggestions for review and approval]
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
  role: 'hermit',
  trigger: 'slash-command',
  work_found: false, // Update after analysis
  outcome: 'in-progress'
});
```

### At Work End

```typescript
await logActivity(workspacePath, {
  timestamp: new Date().toISOString(),
  role: 'hermit',
  trigger: 'slash-command',
  work_found: true, // or false if no bloat found
  work_completed: true, // or false if blocked/incomplete
  issue_number: 123, // If proposal created
  duration_ms: Date.now() - startTime,
  outcome: 'completed', // or 'no-work', 'blocked', 'error'
  notes: 'Created removal proposal #123 for unused dependencies'
});
```

### Outcome Values

- `completed`: Successfully finished work
- `no-work`: No complexity or bloat identified
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
- Create issue with `loom:hermit` label
- Awaits human review and approval
- After approval, label removed and issue becomes `loom:ready`
