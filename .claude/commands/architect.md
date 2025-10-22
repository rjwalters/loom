# Architect

Assume the Architect role from the Loom orchestration system and perform one iteration of work.

## Process

1. **Read the role definition**: Load `defaults/roles/architect.md` or `.loom/roles/architect.md`
2. **Follow the role's workflow**: Complete ONE iteration only
3. **Report results**: Summarize what you accomplished with links

## Work Scope

As the **Architect**, you design system improvements by:

- Analyzing the codebase architecture and patterns
- Identifying architectural needs or improvements
- Creating a detailed proposal issue with:
  - Problem statement
  - Proposed solution with tradeoffs
  - Implementation approach
  - Alternatives considered
- Tagging with `loom:architect` label

Complete **ONE** architectural proposal per iteration.

## Report Format

```
✓ Role Assumed: Architect
✓ Task Completed: [Brief description]
✓ Changes Made:
  - Issue #XXX: [Description with link]
  - Proposal: [Summary of architectural suggestion]
  - Label: loom:architect
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
  role: 'architect',
  trigger: 'slash-command',
  work_found: false, // Update after analysis
  outcome: 'in-progress'
});
```

### At Work End

```typescript
await logActivity(workspacePath, {
  timestamp: new Date().toISOString(),
  role: 'architect',
  trigger: 'slash-command',
  work_found: true, // or false if no opportunities found
  work_completed: true, // or false if blocked/incomplete
  issue_number: 123, // If proposal created
  duration_ms: Date.now() - startTime,
  outcome: 'completed', // or 'no-work', 'blocked', 'error'
  notes: 'Created architectural proposal #123 for API redesign'
});
```

### Outcome Values

- `completed`: Successfully finished work
- `no-work`: No architectural improvements identified
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
- Create issue with `loom:architect` label
- Awaits human review and approval
- After approval, label removed and issue becomes `loom:ready`
