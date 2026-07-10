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

- **Disk space in workspace**: Worktrees consume space inside the repository. _(Amended 2026-07 by #3530: CLI/daemon users can now opt in to an external worktree root — e.g. a dedicated volume — via `LOOM_WORKTREE_ROOT` / `worktree.root`. See the Amendment below. The default remains in-workspace.)_
- **Longer paths**: Nested inside `.loom/worktrees/` adds path length
- **Not user-configurable**: Users can't choose worktree location. _(Amended 2026-07 by #3530: the worktree root is now opt-in configurable for CLI/daemon users; the sandboxed-app default is unchanged. See the Amendment below.)_
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

**Originally rejected because**:
- Complexity in UI and configuration
- Sandbox restrictions still apply
- Most users won't care about location
- Harder to guarantee cleanup

_**Partially revisited 2026-07 by #3530.**_ The original rejection stands for the
default and for the sandboxed macOS app. But CLI/daemon operators running many
parallel agents against large repos hit real disk-pressure limits (disk-full
incidents that corrupt in-flight work), and a dedicated external volume is a
legitimate answer. The concerns above are addressed narrowly: the override is
**opt-in** (no UI surface — an env var / config key), sandbox restrictions are
sidestepped because sandboxed users simply don't set it, and cleanup is
preserved because every worktree-GC site resolves through the same helper (so
overridden worktrees are still garbage-collected). See the Amendment below.

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

## Amendment (2026-07, #3530): opt-in configurable worktree root

The core decision above — **worktrees default to `${workspacePath}/.loom/worktrees/`** — is unchanged. The sandboxed macOS app and every zero-config install behave exactly as before. This amendment records an **opt-in override** for CLI/daemon operators who need worktrees on a different volume.

### Motivation

Operators running many concurrent agents against a large repo can exhaust the workspace's disk. Worktrees under `.loom/worktrees/` are the dominant consumer, and disk-full incidents block git operations mid-task and corrupt in-flight work. A dedicated high-bandwidth external volume (e.g. an NVMe RAID) is a legitimate host for agent worktrees, but the historical hardcoded path offered no way to point at it. Symlinking `.loom/worktrees` was tried and is fragile — cleanup/GC sites compare resolved absolute paths that no longer match, and tooling that recreates the directory silently reverts the redirect.

### What changed

A single shared resolver, `loom_worktree_root()` in [`defaults/scripts/lib/worktree-root.sh`](../../defaults/scripts/lib/worktree-root.sh), resolves the worktree base with this precedence (first match wins):

1. **`LOOM_WORKTREE_ROOT`** environment variable
2. **`.loom/config.json` → `worktree.root`** (same key namespace as `worktree.linkPaths` from #3534; read via the same jq-guarded pattern)
3. **`${repo_root}/.loom/worktrees`** — the unchanged default

When an override is set, the base is namespaced by repo basename to avoid collisions when multiple workspaces share one volume:

```
${LOOM_WORKTREE_ROOT%/}/<repo-basename>/issue-<N>   (and pr-<N>)
```

The resolver is wired into the bash worktree lifecycle — construction sites (`worktree.sh`, `pr-worktree.sh`), cleanup discovery (`merge-pr.sh`), and the terminal-destroy GC-detection site (`agent-destroy.sh`, which now checks the resolved root instead of a hardcoded `.loom/worktrees/` substring so overridden worktrees are still garbage-collected).

### What did NOT change

- **Default behavior** is byte-for-byte identical when neither override is set.
- **`.loom-managed` sentinel** semantics — written inside the worktree dir wherever it lives; ownership is unaffected.
- **Locks** — `_worktree_locks_dir()` still resolves via the git common dir, so lock state stays in the main repo regardless of the worktree root.
- **Sandboxed-app default** — the macOS app does not set the override; the sandbox rationale in the original decision is intact.

### Constraints and known limitations

- The override must be an **absolute path**; a relative value is rejected with a warning and falls back to the default.
- Two repos with the **same basename** cloned under the same override root would share a namespace — a documented v1 limitation, not guarded.

### Scope of the #3530 implementation

The initial change covers the **bash-script + `.loom/config.json` surface only**. The Rust daemon comparison site (`loom-daemon/src/terminal.rs`, the `contains(".loom/worktrees")` GC check) and the Python `loom-tools` GC/CLI surface (`clean.py`, `common/paths.py`, `daemon_cleanup.py`, `orphan_recovery.py`, `worktree.py`) resolve the worktree path independently and are tracked as separate follow-up issues. Until those land, an overridden root is fully honored by the manual/CLI/`/loom:sweep` bash workflow but not yet by daemon-driven Rust cleanup or `loom-clean --aggressive`.
