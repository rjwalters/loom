# Assume Loom Role

Intelligently select and assume an archetypal role from the Loom orchestration system based on recent activity patterns, then perform one iteration of work following that role's guidelines.

## Process

1. **Query recent activity**: Read last 30 minutes of activity log to understand what roles have been running
2. **Apply smart heuristics**: Filter out roles with no work, prioritize stale roles (see Smart Role Selection below)
3. **List available roles**: Check `defaults/roles/*.md` or `.loom/roles/*.md`
4. **Select optimal role**: Choose role most likely to have productive work based on heuristics
5. **Read the role definition**: Load the markdown file for the selected role
6. **Follow the role's workflow**: Complete ONE iteration only (one task, one PR review, one issue triage, etc.)
7. **Report results**: Summarize what you accomplished with links to issues/PRs modified

## Available Roles

- **builder.md** - Claim `loom:issue` issue, implement feature/fix, create PR with `loom:review-requested`
- **judge.md** - Review PR with `loom:review-requested`, approve or request changes, update labels
- **curator.md** - Find unlabeled issue, enhance with technical details, mark as `loom:curated`
- **champion.md** - Evaluate `loom:curated` issues, promote to `loom:issue` or provide feedback
- **architect.md** - Create architectural proposal issue with `loom:architect` label
- **hermit.md** - Analyze codebase complexity, create bloat removal issue with `loom:hermit`
- **healer.md** - Fix bug or address PR feedback, maintain existing PRs
- **guide.md** - Triage batch of issues, update priorities and labels for workflow
- **driver.md** - Execute direct task or command (plain shell, no specific workflow)

## Smart Role Selection

The `/loom` command uses activity log data to make intelligent role selection decisions, avoiding roles that recently had no work and prioritizing roles that haven't run recently.

### Heuristic Algorithm

```typescript
import { readRecentActivity } from './src/lib/activity-logger';

async function selectRoleWithActivityData(workspacePath: string): Promise<string> {
  // Step 1: Query recent activity (last 30 minutes)
  const recentActivity = await readRecentActivity(workspacePath, 100);
  const thirtyMinutesAgo = Date.now() - 30 * 60 * 1000;
  const recent = recentActivity.filter(
    e => new Date(e.timestamp).getTime() > thirtyMinutesAgo
  );

  // Step 2: Identify roles with 3+ consecutive "no work" results
  const noWorkRoles = new Set<string>();
  const roleGroups: Record<string, ActivityEntry[]> = {};

  // Group entries by role
  for (const entry of recent) {
    if (!roleGroups[entry.role]) {
      roleGroups[entry.role] = [];
    }
    roleGroups[entry.role].push(entry);
  }

  // Check for 3+ consecutive "no work"
  for (const [role, entries] of Object.entries(roleGroups)) {
    const lastThree = entries.slice(-3);
    if (lastThree.length >= 3 && lastThree.every(e => !e.work_found)) {
      noWorkRoles.add(role);
    }
  }

  // Step 3: Define all available roles
  const allRoles = ['builder', 'judge', 'curator', 'champion', 'architect', 'hermit', 'healer', 'guide'];

  // Step 4: Filter out roles with no work
  const availableRoles = allRoles.filter(r => !noWorkRoles.has(r));

  if (availableRoles.length === 0) {
    // All roles have no work - reset and default to builder
    console.log('[loom] All roles filtered out, defaulting to builder');
    return 'builder';
  }

  // Step 5: Prioritize roles that haven't run recently (staleness-based)
  const lastRuns = new Map<string, number>();
  for (const role of availableRoles) {
    const roleActivity = recent.filter(e => e.role === role);
    if (roleActivity.length > 0) {
      // Use timestamp of most recent run
      lastRuns.set(role, new Date(roleActivity[roleActivity.length - 1].timestamp).getTime());
    } else {
      // Never run - highest priority
      lastRuns.set(role, 0);
    }
  }

  // Step 6: Sort by last run time (oldest first = highest priority)
  availableRoles.sort((a, b) => {
    return (lastRuns.get(a) || 0) - (lastRuns.get(b) || 0);
  });

  // Return most stale (oldest) role
  const selectedRole = availableRoles[0];
  console.log(`[loom] Selected role: ${selectedRole} (last run: ${lastRuns.get(selectedRole) || 'never'})`);
  return selectedRole;
}
```

### Heuristic Details

**Time Window**: 30 minutes
- Recent enough to be relevant
- Old enough to have sufficient data for patterns

**No-Work Threshold**: 3 consecutive attempts without work
- 1-2 attempts: Could be temporary (timing, race condition)
- 3+ attempts: Likely no work available for this role currently

**Staleness Priority**: Roles sorted by last run time (oldest first)
- Ensures balanced distribution of work across all role types
- Prevents overusing popular roles (e.g., builder) while neglecting others
- Roles that have never run get highest priority

**Fallback Strategy**: If all roles filtered out, default to `builder`
- Builder is most likely to have work when new issues arrive
- Prevents the heuristic from blocking all work

### Benefits

✅ **Adaptive**: Adjusts to changing work availability in real-time
✅ **Efficient**: Avoids wasting time on roles with no work
✅ **Balanced**: Distributes work evenly across role types over time
✅ **Fast**: Simple filtering and sorting, completes in < 10ms for typical workloads

### Example Scenarios

**Scenario 1: Builder has no work**
```
Recent activity:
- builder: no work (3 consecutive)
- judge: no work (2 consecutive)
- curator: no work (1 time)

Result: Skip builder, prioritize curator (oldest without threshold)
```

**Scenario 2: Judge hasn't run recently**
```
Recent activity:
- builder: completed (5 min ago)
- curator: no work (10 min ago)
- judge: (never run in last 30 min)

Result: Select judge (highest staleness)
```

**Scenario 3: All roles have no work**
```
Recent activity:
- All roles: 3+ consecutive no work

Result: Default to builder (most likely to have work when issues arrive)
```

## Work Scope

Complete **ONE** meaningful task following the selected role's guidelines, then **stop and report**.

### Task Examples by Role

**Builder**: Claim one `loom:issue` issue → implement → test → commit → create PR
**Judge**: Review one PR with `loom:review-requested` → provide feedback → approve/request changes
**Curator**: Find one unlabeled issue → add context → tag as `loom:curated`
**Champion**: Evaluate `loom:curated` issues (max 2) → promote to `loom:issue` or provide feedback
**Architect**: Identify one architectural need → create detailed proposal issue
**Hermit**: Analyze codebase section → identify bloat → create removal issue
**Healer**: Fix one bug or address one PR comment → update PR → push changes
**Guide**: Review issue backlog → update priorities → add/remove labels for batch
**Driver**: Execute one direct command or task per user instruction

## Report Format

After completing your iteration, report:

```
✓ Role Assumed: [Role Name]
✓ Task Completed: [Brief description]
✓ Changes Made:
  - [Issue/PR #XXX]: [Description with link]
  - [Label changes]
  - [Files modified if applicable]
✓ Next Steps: [Suggestions for follow-up or ready state]
```

## Label Workflow

Follow the label-based coordination system (ADR-0006):

- Issues: `loom:curated` → `loom:issue` → `loom:building` → closed
- PRs: `loom:review-requested` → `loom:pr` → merged
- Proposals: `loom:architect` → reviewed → implemented or closed
- Suggestions: `loom:hermit` → reviewed → implemented or closed

## Activity Logging

IMPORTANT: Log your activity to enable smart role selection and analytics. Use `trigger: 'heuristic'` to mark this as automatic role selection.

### At Work Start

```typescript
import { logActivity } from './src/lib/activity-logger';

const startTime = Date.now();
const workspacePath = process.cwd(); // Or use workspace detection
const selectedRole = '<role-name>'; // e.g., 'builder', 'judge', etc.

await logActivity(workspacePath, {
  timestamp: new Date().toISOString(),
  role: selectedRole,
  trigger: 'heuristic', // IMPORTANT: Use 'heuristic' for /loom command
  work_found: false, // Update after checking for work
  outcome: 'in-progress'
});
```

### At Work End

```typescript
await logActivity(workspacePath, {
  timestamp: new Date().toISOString(),
  role: selectedRole,
  trigger: 'heuristic', // IMPORTANT: Use 'heuristic' for /loom command
  work_found: true, // or false if no work found
  work_completed: true, // or false if blocked/incomplete
  issue_number: 123, // If applicable
  duration_ms: Date.now() - startTime,
  outcome: 'completed', // or 'no-work', 'blocked', 'error'
  notes: 'Selected role via /loom, completed task'
});
```

### Outcome Values

- `completed`: Successfully finished work
- `no-work`: No work found for selected role
- `blocked`: Found work but couldn't proceed (dependencies, etc.)
- `error`: Encountered error during execution

### Trigger Value

For the `/loom` command, ALWAYS use:
- `trigger: 'heuristic'` - Indicates automatic role selection (NOT manual /builder, /judge, etc.)

This allows tracking which work came from random selection vs explicit role invocation.

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

## Notes

- This command simulates one terminal's work in the Loom multi-terminal orchestration system
- Multiple Claude Code sessions can run `/loom` in parallel for distributed work
- Each iteration should be atomic and complete (don't leave partial work)
- Always update labels appropriately to signal state to other roles
- Check for existing work to avoid conflicts (e.g., don't claim already `in-progress` issues)

## Example Usage

```bash
# In Claude Code terminal
/loom

# Claude responds:
"🎭 Analyzing recent activity... Judge hasn't run in 15 minutes and builder has no work.

Assuming the Judge role for this iteration.

Looking for PRs with loom:review-requested...
Found PR #401 - 'Add terminal restart functionality'

[Performs detailed review following judge.md guidelines]

✓ Role Assumed: Judge (selected via smart heuristic - staleness priority)
✓ Task Completed: Reviewed PR #401
✓ Changes Made:
  - PR #401: Added detailed review comment with 3 suggestions
  - Label changed: loom:review-requested → loom:pr
  - Approved PR for merge
✓ Next Steps: Ready for maintainer to merge

Review complete!"
```
