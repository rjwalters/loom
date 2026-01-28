# Shepherd (Shell Script)

This command invokes the shell script-based shepherd orchestration.

## Usage

```bash
/shepherd-sh <issue-number> [options]
```

## What This Command Does

This is a thin wrapper that calls `./.loom/scripts/shepherd-loop.sh` with the provided arguments. The shell script handles all orchestration deterministically without LLM interpretation.

## Arguments

**Arguments**: $ARGUMENTS

## Options

- `--force-pr` - Auto-approve issue, run through Judge, stop at `loom:pr`
- `--force-merge` - Auto-approve, resolve conflicts, auto-merge after approval
- `--to <phase>` - Stop after specified phase (curated, pr, approved)

## Why Shell Script?

The shell-based shepherd provides:

1. **No token accumulation** - Each phase runs in fresh Claude session
2. **Deterministic behavior** - Shell conditionals vs LLM reasoning
3. **Configurable polling** - Shell sleep vs LLM polling overhead
4. **Debuggable** - Read shell script vs conversation history
5. **Reduced cost** - >80% token reduction compared to LLM shepherd

## Execution

Execute the shell script with the provided arguments:

```bash
./.loom/scripts/shepherd-loop.sh $ARGUMENTS
```

Run this command now and exit when complete.
