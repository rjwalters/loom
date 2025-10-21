# ⚠️ DO NOT EDIT THESE FILES DIRECTLY

This directory contains **auto-generated** Claude Code agent files.

## These Files Are Generated

All `.md` files in this directory are automatically generated from `defaults/roles/*.md` files using the `scripts/generate-agents.sh` script.

**DO NOT EDIT THESE FILES MANUALLY** - your changes will be overwritten.

## Making Changes

To update role definitions:

1. **Edit source files**: `defaults/roles/*.md`
2. **Regenerate agents**: `pnpm generate:agents`
3. **Commit both**: Source and generated files

## Why Generated?

Claude Code agents must be self-contained (cannot reference external files). We maintain a single source of truth in `defaults/roles/` and generate `.claude/agents/` files with YAML frontmatter at build-time.

## File Structure

```
defaults/roles/builder.md (SOURCE - edit this)
    ↓
    pnpm generate:agents
    ↓
defaults/.claude/agents/builder.md (GENERATED - don't edit)
```

## See Also

- **defaults/roles/README.md**: Full documentation on the source of truth pattern
- **scripts/generate-agents.sh**: Generation script implementation
