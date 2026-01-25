# Architect

Assume the Architect role from the Loom orchestration system and perform one iteration of work.

## Process

1. **Read the role definition**: Load `defaults/roles/architect.md` or `.loom/roles/architect.md`
2. **Check for autonomous mode**: If `--autonomous` flag is present, skip interactive questions
3. **Follow the role's workflow**: Complete ONE iteration only
4. **Report results**: Summarize what you accomplished with links

## Usage

```
/architect              # Interactive mode - asks clarifying questions
/architect --autonomous # Autonomous mode - uses sensible defaults, no questions
```

## Options

| Flag | Description |
|------|-------------|
| `--autonomous` | Skip clarifying questions, use self-reflection to infer constraints |

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

## Interactive vs Autonomous Mode

**Interactive Mode** (default):
- Ask 3-5 clarifying questions before creating proposals
- Wait for user responses to understand constraints
- Create focused recommendation based on answers

**Autonomous Mode** (`--autonomous`):
- Skip all clarifying questions
- Use self-reflection to infer constraints from codebase
- Apply sensible defaults (simplicity over complexity, incremental over rewrite)
- Document assumptions in the proposal issue
- Create proposal immediately without user interaction

**When to use autonomous mode**:
- Running as a background subagent (spawned by `/loom` daemon)
- Batch processing multiple proposals
- Testing the proposal workflow
- When user explicitly wants hands-off operation

## Report Format

```
✓ Role Assumed: Architect
✓ Mode: [Interactive | Autonomous]
✓ Task Completed: [Brief description]
✓ Changes Made:
  - Issue #XXX: [Description with link]
  - Proposal: [Summary of architectural suggestion]
  - Label: loom:architect
✓ Next Steps: [Suggestions for review and approval]
```

## Label Workflow

Follow label-based coordination (ADR-0006):
- Create issue with `loom:architect` label
- Awaits human review and approval
- After approval, label removed and issue becomes `loom:issue`

## Context Clearing (Autonomous Mode)

When running autonomously, clear your context at the end of each iteration to save costs:

```
/clear
```

This resets the conversation, reducing API costs for future iterations while keeping each run fresh and independent.

ARGUMENTS: $ARGUMENTS
