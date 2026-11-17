---
name: loom-judge-merge-conflicts
description: "**Load when**: `mergeStateStatus` is `DIRTY`, or you are about to simulate or inspect a merge (never mutate the main checkout's index or stash)."
---
<!-- loom-managed-skill -->
<!-- GENERATED FILE — DO NOT EDIT DIRECTLY.
     Produced by `loom-daemon generate-agent-skills` from
     defaults/.claude/commands/loom/judge-merge-conflicts.md (the same source Claude Code
     reads as /loom:judge-merge-conflicts via .claude/commands/loom/). This is the
     cross-vendor skill-discovery surface (.agents/skills/<name>/SKILL.md)
     read natively by Codex, Kimi Code, Mistral Vibe, and Grok — see
     runtime-adapters.md §5. To change this file, edit the source above
     and re-run the generator; CI (`loom-daemon generate-agent-skills
     --check`) fails if this file is stale. -->

# Judge: Merge Conflicts — Rebase Check and Automated Update

**Load when**: `mergeStateStatus` is `DIRTY`, or you are about to simulate or inspect a merge (never mutate the main checkout's index or stash).

On-demand sub-skill of `judge.md` (split out in issue #13957): the core workflow stays in `judge.md`, which points here by the trigger above. Section references to the main workflow live in `judge.md`; existing split-outs `judge-reference.md` and `judge-ci-red.md` work the same way.

## Rebase Check (BEFORE Evaluation)

**After checkout, check the PR for merge conflicts with main before starting code evaluation** (being behind main is fine, #12063).

This catches merge conflicts early in the evaluation cycle, preventing wasted effort on code that will need to be rebased anyway.

> ### ⛔ NEVER mutate the main checkout's real git index, run a throwaway test-merge, or touch the stash stack during a merge simulation or inspection
>
> **Your session starts in the shared main checkout**; per "PR Branch Isolation" above you move into an isolated worktree before touching PR code, never `gh pr checkout` in place. You do **not** own a disposable git index, a disposable branch, or a disposable stash stack **in the main checkout itself**. Writing its real index, creating a throwaway test-merge branch, or popping/dropping/clearing its stash destroys shared state for every role after you.
>
> **NEVER run any of these against the main checkout** to "simulate a merge", preview a tree, or inspect conflicts:
>
> - **`git read-tree`** (bare, or `git read-tree <tree>` **without** an isolated `GIT_INDEX_FILE`) — a bare `git read-tree` equals `--empty`: it silently empties the index, turning **every tracked file into a phantom staged deletion**. The working tree and `HEAD` are untouched and **no reflog entry is written**, so the damage is near-invisible until the next `git add -A` commits it.
> - **`git commit-tree`** piped from a `read-tree`-populated index.
> - **`git reset`**, **`git rm --cached`**, **`git add`**, or **`git checkout .`** used "just to simulate" a merge or a conflicting state.
> - **A throwaway test-merge branch** (`git checkout -b tmp-test && git merge <pr-branch>`, or the reverse — merging the PR branch into main on a scratch branch) created **in the main checkout** to eyeball how a merge resolves. There is no such thing as a disposable branch in shared state: the checkout, the index, and the stash stack it touches are all live for every other role.
> - **Any stash-stack mutation** (`git stash pop` / `git stash drop` / `git stash clear`) run **in the main checkout** for any reason, including "just to get a clean tree for a test-merge." The main checkout's stash stack is **operator-owned** — it may hold deliberately preserved diagnostic state (e.g. sweep-contamination evidence parked for investigation) with no marker distinguishing "safe to pop" from "evidence, do not touch." The 2026-07-28 incident this rule exists for: a Judge's throwaway main-checkout test-merge inadvertently `git stash pop`'d a preserved stash entry; the pop happened to conflict, so nothing was lost that time, but a clean pop would have silently destroyed it with no recovery path. (`git stash push` / `apply` / `list` are non-destructive and are not the concern here — the danger is specifically `pop`/`drop`/`clear`.) The destructive-command guard asks for confirmation on these three subcommands when the cwd resolves to the main checkout (`guards.stashScope` / `LOOM_GUARD_STASH_SCOPE`, see `defaults/docs/guard-hooks.md`) — but do not rely on the guard catching it; the rule is to never issue the command there in the first place. **In a headless run that ask has nobody to answer it, so it stalls exactly like a deny** — which is why "don't do it" now comes with a replacement rather than only a prohibition: when you genuinely need a clean tree in the primary clone (a baseline `shellcheck`/`cargo clippy`/test run to diff against), use `./.loom/scripts/worktree.sh stash-push main` … `./.loom/scripts/worktree.sh stash-pop main` (#6076). It anchors to `refs/loom/stash-baseline/main` instead of `refs/stash`, so the operator's stack is untouched and nothing asks. To reconcile a quarantined `loom-quarantine:` entry, **replay, don't pop**: `git stash show -p <ref> | git -C .loom/worktrees/issue-<N> apply -`.
>
> **Instead, use the index-free approach** (the same one `doctor.md` uses — see `doctor.md`'s merge-conflict check, `git merge-tree origin/main | grep -q "^+<<<<<<<"`):
>
> ```bash
> # Merge preview — writes to the object store, NEVER the working index:
> git merge-tree --write-tree <base> <branch>
>
> # Conflict detection only (older two-arg form):
> git merge-tree <base> <branch>
> ```
>
> `git merge-tree` is the right tool for **conflict detection only** — it answers "does this merge cleanly?" without a working tree. When an integration check genuinely needs a real working tree (e.g. to run the test suite against the merged result, not just detect conflicts), do it **inside the already-isolated worktree you're evaluating in** (the builder's `.loom/worktrees/issue-N`, or one created via `pr-worktree.sh`) — merge `origin/main` **into the PR branch there**, never the reverse, and never in the main checkout:
>
> ```bash
> # Inside the isolated PR-branch worktree (NOT the main checkout):
> git fetch origin
> git merge --no-commit --no-ff origin/main
> # ...inspect the merged working tree / run tests...
> git merge --abort   # always undo — this worktree stays on the PR branch, not a merge commit
> ```
>
> This gives the identical integration signal a main-checkout test-merge would, with zero main-checkout mutation: the worktree's own index and working tree are disposable, the main checkout's are not.
>
> If you genuinely must populate an index (you almost never do), **isolate it** so the real index is never touched:
>
> ```bash
> GIT_INDEX_FILE="$(mktemp)" git read-tree <tree>
> ```
>
> **Why this matters:** bare `read-tree` empties the live index, leaves the working tree and `HEAD` untouched, and writes **no reflog entry**, so recovery is hard and the corruption is easy to miss; a stash pop is similarly silent-on-conflict-free-success with no reflog trace of what was lost. Every role that operates in the main checkout (Judge, Champion, Auditor, Guide) is exposed to the same hazard — prefer `git merge-tree --write-tree` for conflict detection, the merge-origin-into-the-PR-branch-worktree pattern for anything needing a working tree, and reach for index-mutating plumbing only under an isolated `GIT_INDEX_FILE`. Never touch the main checkout's stash stack.

### Check Merge State

```bash
gh pr view <number> --json mergeStateStatus --jq '.mergeStateStatus'
```

| Status | Action |
|--------|--------|
| `CLEAN` | Continue to evaluation |
| `BEHIND` | Continue to evaluation, no update push (see BEHIND below) |
| `DIRTY` | Attempt automated rebase (see If DIRTY section below) |
| `BLOCKED`/`UNSTABLE` | Continue to evaluation (CI issue, not branch issue) |

### If DIRTY: Attempt Automated Rebase

**When a PR has merge conflicts, attempt an automated update before routing to
Doctor** — this reduces the Doctor→Judge→Merge cycle for simple conflicts.
**Update = merge** `origin/main` in, unless `loom-daemon branch-update strategy`
prints `rebase` (#10771): where the repo bars rebase, a rebase deletes other
agents' merge commits and orphans reviewed heads. Merge mode never force-pushes;
a rejected plain push means someone pushed — fall back, never force.

**Both `gh pr edit` fallback writes below are verdict-label writes** — run the
Verdict-Time CAS Recheck immediately before each (see above) and abort instead
of writing if your claim is lost or another Judge's verdict already landed.

```bash
PR_NUMBER=<number>
MERGE_STATE=$(gh pr view $PR_NUMBER --json mergeStateStatus --jq '.mergeStateStatus')

if [ "$MERGE_STATE" = "DIRTY" ]; then
    # Checkout PR branch (worktree-aware — see "PR Branch Isolation")
    ISSUE_NUM=$(gh pr view $PR_NUMBER --json headRefName --jq '.headRefName' | sed 's/feature\/issue-//')
    if [ -d ".loom/worktrees/issue-${ISSUE_NUM}" ]; then
        cd ".loom/worktrees/issue-${ISSUE_NUM}"
    else
        ./.loom/scripts/pr-worktree.sh $PR_NUMBER
        cd ".loom/worktrees/pr-${PR_NUMBER}"
    fi

    GATE_OK=true
    # Detached HEAD: skip the update, fall back to change request (below)
    BRANCH=$(git symbolic-ref --short HEAD 2>/dev/null) || GATE_OK=false
    PUSH_LEASE_SHA=$(git rev-parse HEAD)  # pin BEFORE the update, never re-read (#9487)
    BRANCH_UPDATE=$(loom-daemon branch-update strategy) || BRANCH_UPDATE=merge  # unknown = merge
    git fetch origin main

    if [ "$GATE_OK" = true ]; then
        if [ "$BRANCH_UPDATE" = rebase ]; then git rebase origin/main; else git merge --no-ff -m "Merge origin/main into $BRANCH (Judge: resolve conflicts)" origin/main; fi || GATE_OK=false
    fi
    # Version gate before the push; on failure never hand-patch the version-bearing files yourself or run `version.sh bump`.
    if [ "$GATE_OK" = true ] && [ -x ./.loom/scripts/version-check-gate.sh ] && ! ./.loom/scripts/version-check-gate.sh --fix-hint "then push."; then
        GATE_OK=false
    fi
    # Discard guard (#9282): rc 0 = BLOCKED.
    if [ "$GATE_OK" = true ]; then
        RG=0; loom-daemon forge rebase-guard --base origin/main || RG=$?
        [ "$RG" = 0 ] && GATE_OK=false
        [ "$RG" -le 1 ] || echo "WARNING: rebase-guard gave no answer (rc=$RG) - pushing UNCHECKED (#9282)" >&2
    fi
    if [ "$GATE_OK" = true ]; then  # lease fence (#9401)
        loom-daemon lease pr fence $PR_NUMBER --role judge --watch-pid "${LOOM_AGENT_SESSION_PID:-${CLAUDE_PID:-$PPID}}"
        [[ $? =~ ^[02]$ ]] || exit 1  # lost: stand down, no verdict
    fi
    if [ "$GATE_OK" = true ] && if [ "$BRANCH_UPDATE" = rebase ]; then git push --force-with-lease="$BRANCH:$PUSH_LEASE_SHA"; else git push origin "HEAD:$BRANCH"; fi; then
        gh pr comment $PR_NUMBER --body "🔀 Automatically updated the branch from main ($BRANCH_UPDATE) to resolve merge conflicts. Proceeding with code evaluation."
    else
        echo "Update, gate or push failed - falling back to change request"
        git rebase --abort 2>/dev/null || git merge --abort 2>/dev/null || true
        ./.loom/scripts/post-verdict.sh $PR_NUMBER changes-requested "$VERDICT_SHA" --body "$(cat <<'EOF'
❌ **Changes Requested - Merge Conflict**

This PR has merge conflicts with main that could not be resolved automatically.

Please bring the branch up to date and resolve them: `git fetch origin && git merge origin/main`, then a plain `git push` (rebase only if `loom-daemon branch-update strategy` prints `rebase`, then push with a pinned `--force-with-lease=<branch>:<sha>`).

Will re-evaluate once conflicts are resolved, or the Doctor role will handle this.
EOF
)" && \
        gh pr edit $PR_NUMBER --remove-label "loom:review-requested" --remove-label "loom:reviewing" --add-label "loom:changes-requested" --add-label "loom:merge-conflict"
    fi
fi
```

**Edge cases for the DIRTY update:**

| Scenario | Handling |
|----------|----------|
| Push permission denied | Abort, fall back to change request |
| Concurrent push during the update | Merge mode: the plain push is rejected (non-fast-forward). Rebase mode: only the *pinned* lease rejects it — a bare `--force-with-lease` trusts the shared tracking ref and can silently overwrite (#9487). Either way fall back; never force |
| Detached HEAD after checkout | Skip the update, fall back to change request |
| Update succeeds but CI may fail | Continue to evaluation - CI verification handles this |
| Update would empty the PR or drop a hand-made merge resolution | `rebase-guard` blocks (#9282, was #8298); fall back to change request |

### BEHIND: review as-is (#12063)

`BEHIND` is not a blocker: the ruleset does not require branches to be up to
date, so a BEHIND PR merges as it is. Do **not** merge or rebase `origin/main`
into it: that push moves the head, voids your own verdict and costs a CI run
for nothing. Review the head in front of you, and do not re-judge an approved
PR only because `main` moved. Only `DIRTY` (a real conflict) is updated here
or routed to Doctor.

### Simple vs Complex Conflicts

**Simple conflicts (Judge resolves):**
- Both sides adding to same list/config (e.g., `pyproject.toml` entry points, `package.json` scripts)
- Whitespace or formatting conflicts
- Independent additions to same file (non-overlapping)

**Complex conflicts (Doctor handles):**
- Overlapping code changes in same function/block
- Conflicting logic or behavior changes
- Structural changes (renamed files, moved code)
- Multiple files with interdependent conflicts

### For Simple Conflicts (Judge Resolves)

```bash
# Resolve the conflict (e.g., keep both additions), `git add <resolved-files>`, then finish
if [ "$BRANCH_UPDATE" = rebase ]; then git rebase --continue; else git commit --no-edit; fi

# Version gate before this push too.
if [ -x ./.loom/scripts/version-check-gate.sh ] && ! ./.loom/scripts/version-check-gate.sh --fix-hint "then push."; then
  echo "Version-bearing files out of sync after update (see BLOCKER:/Fix: above) - aborting push"
  exit 1
fi
RG=0; loom-daemon forge rebase-guard --base origin/main || RG=$?
[ "$RG" = 0 ] && exit 1
[ "$RG" = 1 ] || echo "WARNING: rebase-guard gave no answer (rc=$RG) - pushing UNCHECKED (#9282)" >&2

loom-daemon lease pr fence <number> --role judge --watch-pid "${LOOM_AGENT_SESSION_PID:-${CLAUDE_PID:-$PPID}}"
[[ $? =~ ^[02]$ ]] || exit 1  # lease lost: stand down
if [ "$BRANCH_UPDATE" = rebase ]; then git push --force-with-lease="$BRANCH:$PUSH_LEASE_SHA"; else git push origin "HEAD:$BRANCH"; fi
gh pr comment <number> --body "🔀 Updated the branch from main and resolved a merge conflict (both sides added entries to config)"
```

### Version-Bearing-File-Only Conflicts: superseded (#7743)

The recurring "VERSION bump goes stale before review lands" race (#7684) is now
structurally impossible: since #7743 no PR may edit a version-bearing file's value
(bumps are `.github/workflows/version-bump-on-merge.yml`'s job, after merge; see
`check-defaults-version-bump.sh --forbid-bump`; `version-check-gate.sh` owns the
`Cargo.lock`/`mcp-loom/package-lock.json` uncommitted-bump case, #7705). This section
intentionally has no recipe: a conflict confined to version-bearing files means the
invariant was violated; treat it as a **complex conflict** (below) and flag it.

**Scope of that impossibility (#7823):** it covers *git rebase conflicts*, not
every way a stale branch can surface a version-bearing-file complaint. A
PR whose branch predates a sibling `defaults/`-touching merge is simply
*behind* `main`'s automated bump — no conflict, nothing hand-edited. The
`--forbid-bump` gate compares against `merge-base(base, head)` precisely so
that drift stays invisible, so if you nonetheless see a
`PRs Must Not Hand-Edit Version-Bearing Files` failure reporting a *downgrade*
(`'0.0.N' -> '0.0.M'` with M < N) on a PR whose own commits never touch those
files, that is a stale branch or a CI checkout with insufficient history — ask
for a rebase, do **not** treat it as a violation of the new invariant.

### For Complex Conflicts (Request Changes)

Run the Verdict-Time CAS Recheck immediately before the `gh pr edit` below.

```bash
git rebase --abort 2>/dev/null || git merge --abort
./.loom/scripts/post-verdict.sh <number> changes-requested "$VERDICT_SHA" --body "$(cat <<'FEEDBACK'
❌ **Changes Requested - Merge Conflict**

This PR has merge conflicts with main that require manual resolution:

**Conflicting files:**
- `src/foo.ts` - overlapping changes in `processData()` function

Please merge main into your branch (rebase only where `loom-daemon branch-update strategy` says so) and resolve conflicts, or the Doctor role will handle this.

I'll evaluate the code once conflicts are resolved.
FEEDBACK
)" && \
  gh pr edit <number> --remove-label "loom:review-requested" --remove-label "loom:reviewing" --add-label "loom:changes-requested"
```

### Edge Cases

- **Rebase succeeds but CI fails**: Continue with evaluation (CI failure is a code issue, not a conflict issue)
- **Rebase creates new test failures**: Continue evaluation - Judge catches this during normal CI check phase
- **Multiple conflicting files**: If ANY conflict is complex, treat entire rebase as complex (request changes)

### Relationship with Doctor

**Current division:**
- **Doctor**: Addresses `loom:changes-requested` feedback, resolves conflicts on labeled PRs
- **Judge**: Evaluates code quality, approves/requests changes

**Why Judge handles simple rebases:**
- Judge already has the PR checked out
- Simple rebase takes seconds vs full Doctor cycle
- Keeps evaluation flow uninterrupted
- Doctor focuses on actual code fixes, not routine rebases

**When to defer to Doctor:**
- Complex conflicts requiring code understanding
- Any uncertainty about conflict resolution
- Conflicts in test files (might need test updates)
