# Judge: Fast Paths — Conflict-Only Fast-Track and Docs-Only Fast Path

**Load when**: a trusted `<!-- loom:conflict-only -->` marker is present (fast-track), or step 4b's docs-only allowlist exactly matches the changed-file list.

On-demand sub-skill of `judge.md` (split out in issue #13957): the core workflow stays in `judge.md`, which points here by the trigger above. Section references to the main workflow live in `judge.md`; existing split-outs `judge-reference.md` and `judge-ci-red.md` work the same way.

## Fast-Track Evaluation (Conflict-Only Resolution)

When Doctor resolves **only merge conflicts** without making substantive code changes, they signal this with a special marker. This enables an abbreviated evaluation process that significantly reduces re-evaluation time.

### Detecting Fast-Track Eligibility

**Step 1: Check for the conflict-only marker in PR comments**

```bash
# Trusted authors only (#9548); non-zero exit = full evaluation
loom-daemon forge trusted-comments --fetch <PR_NUMBER> | jq -e 'any(.[]; .body | contains("<!-- loom:conflict-only -->"))'
```

If it exits 0, the PR is eligible for fast-track evaluation.

### Fast-Track Evaluation Process

When the `<!-- loom:conflict-only -->` marker is present:

**1. Verify the diff is truly conflict-resolution-only:**

```bash
# Compare the new commit(s) against the previous evaluation point
# Look for ONLY these types of changes:
# - Merge conflict markers resolved
# - Package lock regeneration
# - Import reordering
# - Whitespace normalization
# - An ID-only renumber (see below), when Doctor's comment has an
#   `ID renumber: <OLD_ID> -> <NEW_ID> in <path>` line
gh pr diff <PR_NUMBER>
```

**Verifying an ID-only renumber:** compare the PR's own diff (`gh pr diff`, i.e. against the current base) with the diff you reviewed before the resolution; it must differ only in the `<OLD_ID>` -> `<NEW_ID>` token and its same-file references in the PR's own added lines, and touch only `<path>`. `<NEW_ID>` must be the next free ID on `main`. Do **not** diff head-to-head (`git diff <old>..<new>`): in merge mode the new head is a merge commit carrying every change `main` accumulated since `<old>`, so a head-to-head diff reports `main`'s drift as extra files and hunks and the fast-track is unreachable. A changed row cell, a row moved out of the table, a second file, or a citation of `<OLD_ID>` in another file or the PR body is a red flag: run the full evaluation.

**2. Check for unexpected changes:**

Red flags that should trigger a full evaluation instead:
- New logic or functionality
- Modified test assertions
- Changed function signatures
- New error handling
- Documentation updates beyond conflict resolution
- A claimed ID renumber whose PR diff (`gh pr diff`, as in the verification above — never a head-to-head diff) changes any row content, touches a second file, or leaves the old ID cited elsewhere

**3. Verify CI passes — and reconcile formal reviews/inline threads:**

```bash
loom-daemon forge wait-checks <PR_NUMBER> --timeout 20   # seconds; GREEN/NONE (Step 1)
gh pr view <PR_NUMBER> --json mergeStateStatus --jq '.mergeStateStatus'
# A conflict-only re-push does not resolve anyone's outstanding review (#7647).
./.loom/scripts/check-review-feedback.sh --number <PR_NUMBER> --head-sha "$REVIEW_HEAD_SHA"
```

**4. Approve with fast-track audit trail** (run the Verdict-Time CAS Recheck immediately before the `gh pr edit` below):

```bash
./.loom/scripts/post-verdict.sh <PR_NUMBER> approved "$VERDICT_SHA" --body "$(cat <<'EOF'
✅ **Approved (Fast-Track Evaluation)**

This re-evaluation used the abbreviated fast-track process because:
- Doctor signaled conflict-only resolution (`<!-- loom:conflict-only -->`)
- Diff verified to contain only merge resolution changes
- All CI checks pass
- No unexpected code changes detected

<!-- loom:fast-track-evaluation -->
EOF
)" && \
  gh pr edit <PR_NUMBER> --remove-label "loom:review-requested" --remove-label "loom:reviewing" --add-label "loom:pr"
```

### Escalation to Full Evaluation

If the fast-track check reveals unexpected changes:

```bash
gh pr comment <PR_NUMBER> --body "$(cat <<'EOF'
⚠️ **Full Evaluation Required**

Fast-track evaluation was requested but unexpected changes were detected:
- [List unexpected changes here]

Proceeding with full code evaluation instead of fast-track approval.

<!-- loom:fast-track-escalated -->
EOF
)"
# Then continue with standard full evaluation process
```

### Why Fast-Track Matters

| Metric | Full Evaluation | Fast-Track |
|--------|-----------------|------------|
| Typical duration | 123+ seconds | ~30 seconds |
| Code analysis depth | Full | Diff verification only |
| CI verification | Required | Required |
| Use case | New code, logic changes | Conflict resolution only |

**Benefits:**
- Reduces Doctor→Judge→Merge cycle time by ~75%
- Frees Judge capacity for PRs that need deep evaluation
- Maintains audit trail of evaluation approach used
- Automatic fallback to full evaluation if issues detected

## Docs-Only Fast Path (WORK_LOG / WORK_PLAN / README, #6134)

Parent #6107's telemetry showed the dominant cost of a docs-only
`WORK_LOG.md`/`WORK_PLAN.md`/`README.md` PR (e.g. Guide's `create_docs_pr()`)
is not CI compute — `.github/workflows/ci.yml`'s `dorny/paths-filter` already
skips every build/test job for this diff shape — it is the Judge review +
Champion merge **agent/token cycle**: a full code evaluation (Evaluation
Focus Areas, Test Execution, Live Verification, a worktree checkout) spent on
a diff that is provably three non-executing files.

**This fast path is diff-shape-based, not marker-based, and applies to ANY
PR** whose changed-file list qualifies — not only Guide-authored ones.
Eligibility is decided by mechanically re-deriving the changed-file list from
the forge on every pass; a PR's own stated intent, title, or any marker in
its body is never sufficient by itself (the smuggling risk this issue's AC2
guards against: a diff that also touches `.github/workflows/*.yml` or
`.loom/config.json` must never qualify just because it claims to be "docs
only").

### Detecting Eligibility

**Step 1: Fetch the full changed-file list — the paginated REST endpoint,
never `gh pr view --json files`** (that field silently truncates at 100
files with no error, the same #4613 truncation bug Champion's critical-file
check guards against):

```bash
PR_NUMBER=<number>
FILES=$(gh api "repos/{owner}/{repo}/pulls/$PR_NUMBER/files" --paginate --jq '.[].filename')
```

**Step 2: Check every file against the exact-match allowlist** — three
root-level filenames, matched exactly, never by substring or nested path (so
`docs/README.md`, `mcp-loom/README.md`, and `notes/WORK_LOG.md` are all
ineligible; only the repo-root file qualifies):

```bash
# Docs-only fast path (#6134): eligible ONLY if every changed file is EXACTLY
# one of these three root-level filenames, or a month-rotated archive (#12636,
# DOCS_FAST_PATH_ARCHIVE_RE below). This is the same allowlist
# Champion's merge-risk criterion re-derives independently (see champion-pr-merge.md)
# — the two checks are mirrored, not shared, so a change to one without the
# other is a visible drift, not a silent gap.
DOCS_FAST_PATH_ALLOWLIST=("WORK_LOG.md" "WORK_PLAN.md" "README.md")
# Archive files (#12636): ONLY the month-rotated archives, month 01-12, one
# directory deep. Never a directory or *.md allowance.
DOCS_FAST_PATH_ARCHIVE_RE='^(work_log/WORK_LOG|work_plan/WORK_PLAN)_[0-9]{4}-(0[1-9]|1[0-2])\.md$'

docs_only_fast_path_check() {
  # Reads a newline-separated file list on stdin. Echoes "ELIGIBLE" if every
  # file is an exact match against DOCS_FAST_PATH_ALLOWLIST, or
  # "NOT ELIGIBLE: <file>" for the first file that is not — mirrors the
  # exit-on-first-mismatch shape of Champion's critical-file check-loop.
  local file matched
  local saw_any=0
  while IFS= read -r file; do
    [ -z "$file" ] && continue
    saw_any=1
    matched=0
    for allowed in "${DOCS_FAST_PATH_ALLOWLIST[@]}"; do
      if [ "$file" = "$allowed" ]; then
        matched=1
        break
      fi
    done
    if [ "$matched" -ne 1 ] && [[ "$file" =~ $DOCS_FAST_PATH_ARCHIVE_RE ]]; then
      matched=1
    fi
    if [ "$matched" -ne 1 ]; then
      echo "NOT ELIGIBLE: $file"
      return 0
    fi
  done
  if [ "$saw_any" -eq 0 ]; then
    echo "NOT ELIGIBLE: empty file list"
    return 0
  fi
  echo "ELIGIBLE"
}

ELIGIBILITY=$(printf '%s\n' "$FILES" | docs_only_fast_path_check)
```

If `$ELIGIBILITY` is anything other than the literal string `ELIGIBLE`
(including an empty diff, or a diff that mixes an allowlisted file with
anything else — a source file, a nested README, `.github/workflows/*.yml`,
`.loom/config.json`, or any other path) — **do not use this fast path.**
Proceed with the normal Primary Queue steps 5-11 (checkout, quality checks,
test plan execution, full code evaluation).

### Fast-Path Evaluation Process

When `$ELIGIBILITY` is `ELIGIBLE`:

**1. Skip the expensive steps** — no worktree checkout (step 5), no quality
checks (step 7), no test plan execution (step 7b), and no code evaluation
(the "Evaluation Focus Areas" sections below). There is no code to lint,
type-check, or functionally test in a diff confined to these three files.

**1b. Still reconcile formal reviews and inline threads** (step 8b, "Formal
Review & Inline Thread Reconciliation" above). Eligibility here is a statement
about the *diff shape*, not about the *feedback*: anyone can leave a formal
`CHANGES_REQUESTED` review on a three-file docs PR, and this fast path skips
code evaluation, never review reconciliation. `post-verdict.sh` enforces it on
the approval write below regardless (#7647).

**2. Still verify CI status** (step 8, "CI Status Check" above) — a docs-only
diff can still fail a structural CI job (`claude-md-budget`,
`agents-md-sync`, `docs-defaults-parity`, `dangling-links`,
`gitignore-convergence`) even though the build/test matrix is skipped by the
path filter. Apply the same When CI Fails / When CI is Pending handling as
the normal flow — this fast path shortcuts code review, not CI verification.

**3. Approve with fast-path audit trail** (run the Verdict-Time CAS Recheck
immediately before the `gh pr edit` below):

```bash
./.loom/scripts/post-verdict.sh <PR_NUMBER> approved "$VERDICT_SHA" --body "$(cat <<EOF
✅ **Approved (Docs-Only Fast Path)**

This PR's entire changed-file list was verified — via the paginated files
API, not the PR's stated intent or any marker — to be an exact match against
{WORK_LOG.md, WORK_PLAN.md, README.md}: $FILES

A diff confined to this exact set needs no full code evaluation (no lint,
type-check, tests, or functional review apply). All CI checks pass.

<!-- loom:docs-fast-path-evaluation -->
EOF
)" && \
  gh pr edit <PR_NUMBER> --remove-label "loom:review-requested" --remove-label "loom:reviewing" --add-label "loom:pr"
```

### Why This Fast Path Is Safe

- **Mechanical, not judgment-based**: eligibility is a literal string-equality
  check over a freshly-fetched file list, not an LLM inference from the PR's
  title or description.
- **Independently re-verified by Champion**: Champion's `merge-risk` criterion (see
  "Docs-only fast path" in champion-pr-merge.md) re-runs its own copy of this
  same check against its own fresh paginated-files read — it never trusts
  this comment's claim, so a compromised or buggy Judge pass cannot smuggle a
  non-docs change through Champion.
- **Narrowly scoped**: the allowlist is three exact root-level filenames — no
  glob, no directory prefix, no "looks like docs" heuristic. A diff that adds
  a fourth file of any kind, anywhere, including another `.md` file, is
  ineligible.
- **CI is still a gate**: this only shortcuts code evaluation, never CI
  verification — a docs-only diff that trips a structural check still gets
  `loom:ci-failure` / `loom:changes-requested` exactly as it would in the
  full flow.
