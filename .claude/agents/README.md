# ⚠️ DO NOT EDIT THESE FILES DIRECTLY

This directory contains **auto-generated** Claude Code agent files.

## Context: Loom Repository vs Target Repositories

This README serves two contexts:

### 1. In the Loom Repository (Development)

When working on Loom itself:
- **Source**: `defaults/roles/*.md` (edit these)
- **Generated**: `defaults/.claude/agents/*.md` (auto-generated)
- **Script**: `scripts/generate-agents.sh`

```
defaults/roles/builder.md (SOURCE - edit this)
    ↓
    pnpm generate:agents
    ↓
defaults/.claude/agents/builder.md (GENERATED - don't edit)
```

### 2. In Target Repositories (After Installation)

When Loom is installed into a target repository:
- **Source**: `.loom/roles/*.md` (copied from `defaults/roles/` during installation)
- **Agents**: `.claude/agents/*.md` (copied from `defaults/.claude/agents/` during installation)
- These files are **pre-generated** and ready to use

In target repos, both directories are installed from Loom:
- `.loom/roles/` ← copied from `defaults/roles/`
- `.claude/agents/` ← copied from `defaults/.claude/agents/`

## Why This Pattern?

Claude Code agents **cannot reference external files** - they must be self-contained. We solve this by:

1. **Loom repo**: Maintain source in `defaults/roles/`, generate `.claude/agents/`
2. **Target repos**: Install both as-is, `.loom/roles/` is source of truth
3. **Regeneration**: Only needed in Loom repo when updating default roles

## Making Changes (Loom Repository Only)

If you're working on the Loom repository and need to update default roles:

1. **Edit source files**: `defaults/roles/*.md`
2. **Regenerate agents**: `pnpm generate:agents`
3. **Commit both**: Source and generated files

**DO NOT edit `.claude/agents/*.md` manually** - they will be overwritten.

## See Also

- **defaults/roles/README.md**: Full documentation on the source of truth pattern
- **scripts/generate-agents.sh**: Generation script implementation (Loom repo only)
