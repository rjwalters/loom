# Shepherd

Orchestrate issue lifecycle via the shell-based shepherd script.

## Arguments

**Arguments**: $ARGUMENTS

Parse the issue number and any flags from the arguments.

## Supported Options

| Flag | Description |
|------|-------------|
| `--force-pr` | Auto-approve issue, run through Judge, stop at `loom:pr` |
| `--force-merge` | Auto-approve, resolve conflicts, auto-merge after approval |
| `--to <phase>` | Stop after specified phase (curated, pr, approved) |
| `--task-id <id>` | Continue from previous checkpoint |

## Examples

```bash
/shepherd 123                    # Normal orchestration (wait for human approval)
/shepherd 123 --force-pr         # Auto-approve, stop at reviewed PR
/shepherd 123 --force-merge      # Fully automated, auto-merge after review
/shepherd 123 --to curated       # Stop after curation phase
```

## Execution

Invoke the shell script with all provided arguments:

```bash
./.loom/scripts/shepherd-loop.sh $ARGUMENTS
```

Run this command now. Report the exit status when complete.

## Reference Documentation

For detailed orchestration workflow, phase definitions, and troubleshooting:
- **Lifecycle details**: `.claude/commands/shepherd-lifecycle.md`
- **Shell script**: `.loom/scripts/shepherd-loop.sh`
