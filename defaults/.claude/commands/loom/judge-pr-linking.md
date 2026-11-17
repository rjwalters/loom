---
disable-model-invocation: true
---
# Judge: PR Description & Issue Linking — Check, Exceptions, and Direct Fixes

**Load when**: the PR body is missing a closing keyword, uses a non-closing reference, may auto-close a `Part of` issue via a stray keyword, or needs a minor description fix.

On-demand sub-skill of `judge.md` (split out in issue #13957): the core workflow stays in `judge.md`, which points here by the trigger above. Section references to the main workflow live in `judge.md`; existing split-outs `judge-reference.md` and `judge-ci-red.md` work the same way.

### PR Description and Issue Linking (CRITICAL)

**Before evaluating code, verify the PR will close its issue:**

```bash
# View PR description
gh pr view <number> --json body

# Check for magic keywords
# ✅ Look for: "Closes #X", "Fixes #X", or "Resolves #X"
# ⏸️ Intentional non-closing (partial increment): "Part of #X", "Contributes to #X" — see exception below
# ❌ Not acceptable: "Issue #X", "Addresses #X", "Related to #X"
```

**EXCEPTION — intentional partial increments (family/epic issues).** Before treating a missing closing keyword as a defect, check whether the non-closing reference is **deliberate**:

```bash
# Does the PR body already reference the issue with a non-closing keyword?
gh pr view <number> --json body -q .body | grep -Eiq 'part of #|contributes to #'

# Or is the referenced issue a family/epic that must stay open across increments?
gh issue view <issue-number> --json labels -q '.labels[].name' | grep -qx 'loom:epic'   # also check loom:epic-phase
```

If EITHER is true, the PR is a **partial increment** of a larger tracked body of work (a family/epic issue landed in slices). The absence of `Closes #N` is intentional — the issue must survive the merge so the remaining tracked work isn't dropped. In this case:

- Do NOT flag the missing closing keyword.
- Do NOT insert or rewrite a closing keyword (skip the auto-fix in "Minor PR Description Fixes" below).
- Verify the non-closing reference (`Part of #N` / `Contributes to #N`) is present so the PR stays discoverable; if it references the issue only as bare "Issue #N", ask the Builder to change it to `Part of #N` (do not "fix" it to `Closes #N`).
- **Verify no STRAY closing keyword targets #N anywhere else in the body (#4569).** `Part of #N` does not shield the issue: GitHub honors any `close`/`fix`/`resolve` keyword immediately followed by `#N` **anywhere** in the body — including buried in prose, a numbered list, or an "Operator follow-up (after merge)" section — and closes the issue on merge regardless. This has happened for real ("…then close #2" in a handoff checklist closed the very issue the PR declared `Contributes to #2`).

  ```bash
  # Run for the tracked issue N. ANY output is a defect that will close #N on merge.
  gh pr view <number> --json body -q .body \
    | grep -inE '\b(close[sd]?|fix(e[sd])?|resolve[sd]?)[[:space:]]+#N\b'
  ```

  This is a **minor PR description fix** you should make directly (it does not need a Builder round-trip): edit the body so the keyword is no longer adjacent to the reference — `then close the issue`, or `then close issue #N`. An intervening word breaks the link; moving the phrase to a different line does not.

**EXCEPTION — intentional no-issue PRs.** Some PRs legitimately have no issue (operator-session chore/hygiene work). Do not block one when BOTH hold: (1) the PR body references **no issue number at all** (no `Closes`, `Part of`, `Issue #N`, `Addresses #N`, `Related to #N`), and (2) the PR body, the Builder, or the operator **states** the PR is intentionally issue-less. Record `no linked issue: intentional` in your verdict and skip the missing-closing-keyword check. This requires the stated intent, not mere absence: a PR with no issue reference and no issue-less statement is still blocked as "Unclear which issue to reference". It also does not relax anything for a PR that references an issue with a non-closing word (`Issue #N`, `Addresses #N`, `Related to #N`) — those are handled exactly as above. Do not accept the statement at face value if the diff plainly implements an open issue; then treat the PR as referencing that issue and request the proper reference.
- Evaluate the code on its own merits and approve/reject normally.

**If PR description is missing "Closes #X" syntax (and the partial-increment exception above does NOT apply):**

1. **Comment with the issue immediately** - don't evaluate further until fixed
2. **Explain the problem** in your comment:

```bash
./.loom/scripts/post-verdict.sh <number> changes-requested "$VERDICT_SHA" --body "$(cat <<'EOF'
⚠️ **PR description must use GitHub auto-close syntax**

This PR references the issue but doesn't use the magic keyword syntax that triggers GitHub's auto-close feature.

**Current:** "Issue #123" or "Addresses #123"
**Required:** "Closes #123" or "Fixes #123" or "Resolves #123"

**Why this matters:**
- Without the magic keyword, the issue will stay open after merge
- This creates orphaned issues and backlog clutter
- Manual cleanup is required, wasting maintainer time

**How to fix:**
Edit the PR description to include "Closes #123" on its own line.

See Builder role docs for PR creation best practices.

I'll evaluate the code changes once the PR description is fixed.
EOF
)" && \
  gh pr edit <number> --remove-label "loom:review-requested" --remove-label "loom:reviewing" --add-label "loom:changes-requested"
```

3. **Wait for fix before evaluating code**

**Why this checkpoint matters:**

- Prevents orphaned open issues (#339 was completed but stayed open)
- Enforces correct PR practices from Builder role
- Catches the mistake before merge, not after
- Saves Guide role from manual cleanup work

**Approval checklist must include:**

- ✅ PR description uses "Closes #X" (or "Fixes #X" / "Resolves #X") — OR "Part of #X" / "Contributes to #X" for an intentional partial increment of a family/epic issue — OR no issue reference at all with an explicit statement that the PR is intentionally issue-less (record `no linked issue: intentional`)
- ✅ Issue number is correct and matches the work done
- ✅ Code quality meets standards (see sections below)
- ✅ Tests are adequate
- ✅ Documentation is complete

**Only approve if ALL criteria pass.** Don't let PRs merge without proper issue linking.

## Minor PR Description Fixes

**Before requesting changes for missing auto-close syntax, try to fix it directly.**

For minor documentation issues in PR descriptions (not code), Judges are empowered to make direct edits rather than blocking approval. This speeds up the evaluation process while maintaining code quality standards.

> **STOP — do not auto-fix intentional partial increments.** If the partial-increment exception above applies (the PR body already says `Part of #N` / `Contributes to #N`, or the referenced issue carries `loom:epic` / `loom:epic-phase`), the missing closing keyword is deliberate. Do NOT append `Closes #N` and do NOT rewrite the reference — doing so would auto-close a family/epic issue and silently drop its remaining tracked work. The auto-fix steps below apply ONLY to genuinely sloppy references (e.g. a plain one-issue-one-PR that wrote "Issue #N" instead of "Closes #N").

### When to Edit PR Descriptions Directly

**✅ Edit directly for:**
- Missing auto-close syntax (e.g., adding "Closes #123")
- Typos or formatting issues in PR description
- Adding missing test plan sections (if tests exist and pass)
- Clarifying PR title or description for consistency

**❌ Request changes for:**
- Missing tests or failing CI
- Code quality issues
- Architectural concerns
- Unclear which issue to reference (an ambiguous reference, or no reference and no intentional-issue-less statement)
- PR description doesn't match code changes
- Anything requiring code changes

### How to Edit PR Descriptions

**Step 1: Check if there's a related issue (and that this isn't an intentional partial increment)**

```bash
# Search for issues related to the PR (cached — see "Cached Forge Reads")
"$GH_READ" issue list --search "keyword from PR title" --limit 500

# View the PR to confirm issue number
gh pr view <number>

# Guard: skip the auto-fix entirely if this is a deliberate partial increment
gh pr view <number> --json body -q .body | grep -Eiq 'part of #|contributes to #' && echo "PARTIAL — do not add Closes"
gh issue view <issue-number> --json labels -q '.labels[].name' | grep -qx 'loom:epic' && echo "EPIC — do not add Closes"
```

**Step 2: Edit the PR description**

```bash
# Get current PR description
# Name the scratch file after the PR number, never a fixed constant like
# /tmp/pr-body.txt — wave subagents share one scratchpad directory, and a
# fixed filename lets a concurrent Judge/Doctor on a different PR overwrite
# yours between write and read (#6381). See comment-body-literal-path.md.
gh pr view <number> --json body -q .body > /tmp/pr-body-<number>.txt

# Edit the file to add "Closes #XXX" line
# (Use your editor or sed)
echo -e "\nCloses #123" >> /tmp/pr-body-<number>.txt

# Update PR with corrected description
gh pr edit <number> --body-file /tmp/pr-body-<number>.txt
```

**Step 3: Document the change in your comment** (run the Verdict-Time CAS Recheck immediately before the `gh pr edit` below)

```bash
# Comment with approval note about the fix
./.loom/scripts/post-verdict.sh <number> approved "$VERDICT_SHA" --body "$(cat <<'EOF'
✅ **Approved!** I've updated the PR description to add \"Closes #123\" for proper issue auto-close.

Code quality looks great - tests pass, implementation is clean, and documentation is complete.
EOF
)" && \
  gh pr edit <number> --remove-label "loom:review-requested" --remove-label "loom:reviewing" --add-label "loom:pr"
```

### Important Guidelines

1. **Code quality standards remain strict**: Only documentation edits are allowed, not code changes
2. **Never override an intentional partial increment**: If the PR uses `Part of #N` / `Contributes to #N`, or the referenced issue is `loom:epic` / `loom:epic-phase`, leave the reference as-is — do not "fix" it into a closing keyword
3. **Document your edits**: Always mention in your evaluation that you edited the PR description
4. **Verify the fix**: After editing, confirm the PR description now includes proper auto-close syntax
5. **When in doubt, request changes**: If you're unsure which issue to reference, ask the Builder to clarify

**Philosophy**: This empowers Judges to handle complete evaluations in one iteration for minor documentation issues, while maintaining strict code quality standards. The Builder's intent is preserved, and the evaluation process is faster.
