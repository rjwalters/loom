# ADR-0004: Git Worktree Paths Inside Workspace

## Status

Accepted

## Context

Loom uses git worktrees to provide isolated working directories for agent terminals. Each agent can work on a different feature branch without conflicts.

The decision was **where to create worktrees**:
- Inside the workspace at `.loom/worktrees/`
- Outside the workspace at `../loom-worktrees/`
- In a system temp directory `/tmp/loom-worktrees/`
- Anywhere the user chooses

macOS app sandboxing was a key constraint - apps have limited filesystem access outside their designated containers.

## Decision

**Always create worktrees inside the workspace** at:
```
${workspacePath}/.loom/worktrees/${terminalId}
```

Example: `/Users/rwalters/GitHub/loom/.loom/worktrees/terminal-2`

This path is:
- Already gitignored (`.gitignore` line 34)
- Inside workspace boundaries (sandbox-compatible)
- Predictable and consistent
- Automatically cleaned up by daemon when terminals destroyed

## Consequences

### Positive

- **Sandbox-compatible**: Works with macOS app sandboxing restrictions
- **Gitignored**: Worktrees don't pollute `git status` or get committed
- **Automatic cleanup**: Daemon detects `.loom/worktrees/` pattern and removes on terminal destruction
- **Predictable paths**: Easy to find worktrees for debugging
- **No filesystem escapes**: All worktrees contained within workspace
- **Simple permissions**: No need for extra filesystem access grants
- **Clear ownership**: Worktrees belong to the workspace, not global system

### Negative

- **Disk space in workspace**: Worktrees consume space inside the repository
- **Longer paths**: Nested inside `.loom/worktrees/` adds path length
- **Not user-configurable**: Users can't choose worktree location
- **Potential confusion**: Users might not expect files in `.loom/` directory

## Alternatives Considered

### 1. Worktrees Outside Workspace (`../loom-worktrees/`)

**Rejected because**:
- Breaks macOS app sandboxing (filesystem access violation)
- Unpredictable behavior when workspace at filesystem root
- Harder to clean up (no clear ownership)
- Path conflicts if multiple Loom workspaces in same parent

### 2. System Temp Directory (`/tmp/loom-worktrees/`)

**Rejected because**:
- Worktrees lost on system reboot
- No clear association with workspace
- macOS temp cleanup could remove worktrees unexpectedly
- Harder to debug (scattered across filesystem)

### 3. User Home Directory (`~/.loom/worktrees/`)

**Rejected because**:
- Multiple workspaces would conflict
- Requires complex namespacing scheme
- No clear cleanup strategy
- Not workspace-specific

### 4. User-Configurable Path

**Rejected because**:
- Complexity in UI and configuration
- Sandbox restrictions still apply
- Most users won't care about location
- Harder to guarantee cleanup

## Implementation Details

**Worktree Creation** (`src/lib/worktree-manager.ts`):
```typescript
const worktreePath = `${workspacePath}/.loom/worktrees/${terminalId}`;
// Execute: mkdir -p "${worktreePath}"
// Execute: git worktree add "${worktreePath}" HEAD
```

**Daemon Auto-Cleanup** (`loom-daemon/src/terminal.rs:87-102`):
```rust
if working_directory.contains("/.loom/worktrees/") {
    Command::new("git")
        .arg("worktree").arg("remove")
        .arg(&working_directory)
        .arg("--force")
        .output().ok();
}
```

**Developer Workflow**:
```bash
# ✅ CORRECT - Inside workspace
git worktree add .loom/worktrees/issue-84 -b feature/issue-84 main

# ❌ WRONG - Outside workspace
git worktree add ../loom-issue-84 -b feature/issue-84 main
```

## References

- Implementation: `src/lib/worktree-manager.ts`, `loom-daemon/src/terminal.rs`
- Related: ADR-0008 (tmux + Daemon Architecture)
- `.gitignore` line 34: `.loom/worktrees/` exclusion
- macOS Sandbox: https://developer.apple.com/library/archive/documentation/Security/Conceptual/AppSandboxDesignGuide/
