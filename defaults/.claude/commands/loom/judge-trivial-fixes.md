---
disable-model-invocation: true
---
# Judge: Fixing Trivial Code Issues During Evaluation

**Load when**: you spot a trivial, non-controversial code issue (unused import, typo, whitespace) during evaluation.

On-demand sub-skill of `judge.md` (split out in issue #13957): the core workflow stays in `judge.md`, which points here by the trigger above. Section references to the main workflow live in `judge.md`; existing split-outs `judge-reference.md` and `judge-ci-red.md` work the same way.

## Fixing Trivial Code Issues During Evaluation

**For trivial, non-controversial code fixes, fix them directly rather than requesting changes.**

This reduces unnecessary round-trips where a one-line fix creates a full change request cycle.

### What Qualifies as "Trivial"

**✅ Fix directly:**
- Unused imports
- Typos in comments or strings
- Minor whitespace/formatting issues
- Missing trailing newlines
- Simple linting fixes that don't change behavior
- Obvious typos in variable names (within local scope only)

**❌ Request changes instead:**
- Any logic changes
- API or interface changes
- Test behavior changes
- Anything requiring judgment about correctness
- Changes to public-facing variable/function names
- Fixes that might have unintended side effects

### How to Fix Trivial Issues

**Step 1: Check out the PR branch (worktree-aware)**

```bash
# Use existing worktree if available (see "PR Branch Isolation" and
# "Worktree-Aware Code Access")
ISSUE_NUM=$(gh pr view <number> --json headRefName --jq '.headRefName' | sed 's/feature\/issue-//')
if [ -d ".loom/worktrees/issue-${ISSUE_NUM}" ]; then
    cd ".loom/worktrees/issue-${ISSUE_NUM}"
else
    ./.loom/scripts/pr-worktree.sh <number> && cd ".loom/worktrees/pr-<number>"
fi
```

**Step 2: Make the fix**

```bash
# Example: Remove unused import
# Edit the file directly
```

**Step 3: Commit with clear message**

```bash
git add -A
git commit -m "Remove unused import (during evaluation)"
```

**Step 4: Push to the PR branch** (behind the lease fence)

```bash
loom-daemon lease pr fence <number> --role judge --watch-pid "${LOOM_AGENT_SESSION_PID:-${CLAUDE_PID:-$PPID}}"; [[ $? =~ ^[02]$ ]] && git push
```

**Step 5: Note the fix in your approval comment** (run the Verdict-Time CAS Recheck immediately before the `gh pr edit` below)

```bash
./.loom/scripts/post-verdict.sh <number> approved "$VERDICT_SHA" --body "$(cat <<'EOF'
✅ **Approved!**

Fixed during evaluation:
- Removed unused `tempfile` import in `src/utils.py`

Code quality is excellent, tests pass, implementation is solid.
EOF
)" && \
  gh pr edit <number> --remove-label "loom:review-requested" --remove-label "loom:reviewing" --add-label "loom:pr"
```

### Important Guidelines

1. **Keep fixes truly trivial**: If you're unsure, request changes instead
2. **Document your fixes**: Always mention what you fixed in the approval comment
3. **Don't change behavior**: Only fix issues that have zero impact on functionality
4. **One type of fix per commit**: Keep evaluation fixes separate and clear
5. **Preserve Builder's style**: Match the existing code style in the PR

### Why This Matters

**Without direct fixes:**
1. Judge requests changes for unused import
2. Builder/Doctor fixes the one-line issue
3. PR goes back to evaluation queue
4. Judge evaluates again and approves

**With direct fixes:**
1. Judge fixes the unused import directly
2. Judge approves in the same evaluation iteration

This saves significant time and reduces coordination overhead for issues that take seconds to fix.
