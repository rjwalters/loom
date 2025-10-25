# Terminal Worktree Feature - Testing Guide

## Overview

This document describes how to test the terminal worktree creation feature implemented in issue #752.

## What Was Implemented

### Core Functionality

1. **Terminal Worktree Manager** (`src/lib/terminal-worktree-manager.ts`)
   - Creates git worktrees for each terminal (`terminal-1`, `terminal-2`, etc.)
   - Writes role-specific CLAUDE.md files from `.loom/roles/<roleFile>`
   - Cleans up worktrees and branches on workspace reset

2. **Workspace Integration**
   - `workspace-start.ts`: Creates worktrees before terminal creation
   - `workspace-reset.ts`: Creates worktrees on factory reset
   - `workspace-cleanup.ts`: Cleans up terminal worktrees

3. **Terminal Creation Flow**
   - Step 1: Create terminal worktrees with role-specific CLAUDE.md
   - Step 2: Create terminals in their respective worktrees
   - Step 3: Save config with worktree paths

## Testing Strategy

### Manual Testing (Recommended)

#### Test 1: Fresh Workspace Start

**Objective**: Verify terminals created in worktrees with role-specific CLAUDE.md

**Steps**:
1. Clean up existing worktrees:
   ```bash
   ./.loom/scripts/clean.sh --force
   ```

2. Open Loom app and select workspace

3. Click "Start Engine" (or factory reset + start)

4. Verify worktrees created:
   ```bash
   ls -la .loom/worktrees/
   # Should show: terminal-1, terminal-2, terminal-3, terminal-4, terminal-5
   ```

5. Check CLAUDE.md content for each terminal:
   ```bash
   for i in {1..5}; do
     echo "=== terminal-$i ==="
     head -10 .loom/worktrees/terminal-$i/CLAUDE.md
     echo
   done
   ```

6. Verify terminals running in worktrees:
   ```bash
   tmux -L loom list-sessions
   # Should show terminals in .loom/worktrees/terminal-N paths
   ```

**Expected Results**:
- 5 terminal worktrees created in `.loom/worktrees/`
- Each worktree has `CLAUDE.md` with content from corresponding role file:
  - `terminal-1`: Content from `.loom/roles/curator.md`
  - `terminal-2`: Content from `.loom/roles/builder.md`
  - `terminal-3`: Content from `.loom/roles/judge.md`
  - `terminal-4`: Content from `.loom/roles/doctor.md`
  - `terminal-5`: Content from `.loom/roles/guide.md`
- Each terminal tmux session shows worktree path as working directory

#### Test 2: Factory Reset

**Objective**: Verify old worktrees cleaned up and new ones created

**Steps**:
1. With terminals running, click "Factory Reset"

2. Confirm reset dialog

3. Click "Start Engine"

4. Verify worktrees recreated:
   ```bash
   ls -la .loom/worktrees/
   git worktree list
   git branch | grep worktree/
   ```

**Expected Results**:
- Old worktrees removed
- New worktrees created with fresh branches
- No orphaned worktrees or branches

#### Test 3: Terminal with Custom Role

**Objective**: Verify custom roles work correctly

**Steps**:
1. Create custom role file:
   ```bash
   cat > .loom/roles/test-role.md <<EOF
   # Test Role

   You are a test agent.
   EOF
   ```

2. Modify `.loom/config.json` to add terminal with `test-role.md`

3. Restart engine

4. Check worktree CLAUDE.md has test role content:
   ```bash
   cat .loom/worktrees/terminal-X/CLAUDE.md
   ```

**Expected Results**:
- Terminal worktree created with test role content in CLAUDE.md

#### Test 4: Worktree Creation Failure Handling

**Objective**: Verify graceful degradation when worktree creation fails

**Steps**:
1. Make `.loom/worktrees/` read-only:
   ```bash
   chmod 000 .loom/worktrees/
   ```

2. Start engine

3. Check error messages and terminal behavior

4. Restore permissions:
   ```bash
   chmod 755 .loom/worktrees/
   ```

**Expected Results**:
- Error logged for worktree creation failure
- Toast notification showing which terminals failed
- Failed terminals start in main workspace
- Successful terminals start in worktrees

### Automated Testing

#### Unit Test (Future)

```typescript
// Example unit test for terminal-worktree-manager.ts
describe("createTerminalWorktree", () => {
  it("should create worktree with role-specific CLAUDE.md", async () => {
    const config = {
      terminalId: "terminal-1",
      terminalName: "Curator",
      roleFile: "curator.md",
      workspacePath: "/path/to/workspace",
    };

    const result = await createTerminalWorktree(config);

    expect(result.worktreePath).toBe(
      "/path/to/workspace/.loom/worktrees/terminal-1"
    );
    expect(result.claudeMdPath).toBe(
      "/path/to/workspace/.loom/worktrees/terminal-1/CLAUDE.md"
    );
    // Verify CLAUDE.md content matches curator.md
  });
});
```

## Verification Checklist

### Functionality
- [ ] Terminal worktrees created in `.loom/worktrees/terminal-N/`
- [ ] Git worktrees with branches `worktree/terminal-N`
- [ ] Role-specific CLAUDE.md written to each worktree
- [ ] Terminals start in worktree directories
- [ ] Worktrees cleaned up on factory reset
- [ ] Branches cleaned up with worktrees

### Error Handling
- [ ] Graceful fallback when worktree creation fails
- [ ] Error logging for debugging
- [ ] User notification via toast messages
- [ ] Terminals start in main workspace if worktree fails

### Edge Cases
- [ ] Custom role files work correctly
- [ ] Missing role files default to driver.md
- [ ] Existing worktrees cleaned up before creation
- [ ] Orphaned worktrees handled correctly

### Integration
- [ ] Config saved with correct worktree paths
- [ ] State reflects worktree paths
- [ ] Terminal state parser works in worktrees
- [ ] Agent launcher starts agents in worktrees

## Known Limitations

1. **Tauri Shell Command**: Uses `@tauri-apps/plugin-shell` for git commands
   - Requires shell plugin to be configured
   - May need additional error handling for different git versions

2. **Role File Fallback**: Defaults to `driver.md` if role file missing
   - Consider warning user about missing role files

3. **Parallel Worktree Creation**: Creates worktrees concurrently
   - Git worktree commands should be safe to run in parallel
   - Monitor for potential race conditions

## Troubleshooting

### Worktree Creation Fails

**Symptoms**: Error in logs, toast notification, terminals start in main workspace

**Diagnosis**:
```bash
# Check git worktree status
git worktree list

# Check for permission issues
ls -la .loom/worktrees/

# Check daemon logs
tail -f ~/.loom/daemon.log
```

**Solutions**:
- Verify git version supports worktrees (>= 2.5)
- Check file permissions on `.loom/worktrees/`
- Manually clean up stale worktrees: `./.loom/scripts/clean.sh`

### CLAUDE.md Not Updated

**Symptoms**: Terminal shows repository CLAUDE.md instead of role content

**Diagnosis**:
```bash
# Check if CLAUDE.md exists in worktree
cat .loom/worktrees/terminal-1/CLAUDE.md

# Check if role file exists
ls -la .loom/roles/
```

**Solutions**:
- Verify role file path in config.json
- Check role file exists and is readable
- Restart terminal or workspace

### Orphaned Worktrees

**Symptoms**: Worktrees remain after cleanup

**Diagnosis**:
```bash
git worktree list
git branch | grep worktree/
```

**Solutions**:
```bash
# Manual cleanup
git worktree remove .loom/worktrees/terminal-X --force
git branch -D worktree/terminal-X

# Or use cleanup script
./.loom/scripts/clean.sh --force
```

## Implementation Files

- `src/lib/terminal-worktree-manager.ts` - Core worktree management
- `src/lib/workspace-start.ts` - Integration with workspace start
- `src/lib/workspace-reset.ts` - Integration with factory reset
- `src/lib/workspace-cleanup.ts` - Cleanup logic
- `scripts/test-worktree-creation.sh` - Test helper script

## References

- Issue #752: Terminal worktrees not created during initialization
- CLAUDE.md: Role-specific instructions for agents
- `.loom/roles/*.md`: Role definition files
