# .claude/agents Directory - Not Used by Loom

This directory is intentionally empty. Loom does not use the `.claude/agents/` directory.

## Why This Directory is Empty

The `.claude/agents/` directory previously contained auto-generated agent files that duplicated content from `.loom/roles/*.md`. These files have been removed because:

1. **Slash commands use references**: Loom's slash commands (`.claude/commands/*.md`) reference `.loom/roles/*.md` directly
2. **No automatic delegation**: Loom doesn't use Claude Code's automatic agent delegation feature
3. **Unnecessary duplication**: Each agent file contained ~500 lines duplicating role definitions
4. **Hermit flagged for removal**: The auto-generated files were correctly identified as redundant

## How Loom Works

Loom uses **slash commands** for role-based workflows:

```bash
# Use slash commands to assume roles
/builder    # Implements features from .loom/roles/builder.md
/judge      # Reviews PRs from .loom/roles/judge.md
/curator    # Maintains issues from .loom/roles/curator.md
# ... and more
```

Each slash command:
- Is defined in `.claude/commands/<role>.md` (lightweight, ~40 lines)
- References the full role definition in `.loom/roles/<role>.md`
- Tells Claude to load the role definition and follow its workflow

## Role Definitions

All role definitions are in `.loom/roles/`:

- `.loom/roles/builder.md` - Implementation specialist
- `.loom/roles/judge.md` - Code review specialist
- `.loom/roles/curator.md` - Issue maintenance specialist
- `.loom/roles/architect.md` - System design specialist
- `.loom/roles/hermit.md` - Code simplification specialist
- `.loom/roles/healer.md` - Bug fix specialist
- `.loom/roles/guide.md` - Issue prioritization specialist
- `.loom/roles/driver.md` - General shell environment

## Historical Context

Prior to this change:
- `scripts/generate-agents.sh` generated `.claude/agents/*.md` files
- Each file contained YAML frontmatter + full role definition
- Files were copied during installation from `defaults/.claude/agents/`
- This was based on an assumption that Claude Code agents couldn't reference external files

The assumption was incorrect - slash commands can and do reference external files. The agent files were never actually used.

## Verification

Issue #571 tracks verification that removing `.claude/agents/` doesn't break functionality. Testing showed:
- ✅ All slash commands work correctly
- ✅ Autonomous agents run without errors
- ✅ Fresh installations work properly
- ✅ No errors about missing agent files

**Conclusion**: `.claude/agents/` was legacy code that has been safely removed.

---

**Related Issues**:
- #527 - Removed `.claude/agents/` directory
- #571 - Verified removal doesn't break functionality
