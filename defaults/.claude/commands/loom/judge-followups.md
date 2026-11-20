---
disable-model-invocation: true
---
# Judge: Handling Minor Concerns & Raising Concerns

**Load when**: a minor concern is worth tracking (follow-up issue) or you found a bug unrelated to the current PR.

On-demand sub-skill of `judge.md` (split out in issue #13957): the core workflow stays in `judge.md`, which points here by the trigger above. Section references to the main workflow live in `judge.md`; existing split-outs `judge-reference.md` and `judge-ci-red.md` work the same way.

## Handling Minor Concerns

When you identify issues during evaluation, take concrete action - never leave concerns as "notes for future" without creating an issue.

### Decision Framework

**If the concern should block merge:**
- Request changes with specific guidance
- Remove `loom:review-requested`, add `loom:changes-requested`
- Include clear explanation of what needs fixing

**If the concern is minor but worth tracking:**
1. Create a follow-up issue to track the work
2. Reference the new issue in your approval comment
3. Approve the PR and add `loom:pr` label

**If the concern is not worth tracking:**
- Don't mention it in the evaluation at all

**Never leave concerns as "note for future"** - they will be forgotten and undermine code quality over time.

### Creating Follow-up Issues

**When to create follow-up issues:**
- Documentation inconsistencies (like outdated color references)
- Minor refactoring opportunities (not critical but would improve code)
- Test coverage gaps (existing tests pass but could be more comprehensive)
- Non-critical bugs (workarounds exist, low impact)

**Observed vs. inferred — separate what you measured from what you guess:**

A code review gives you real evidence of *that* something is wrong — a diff,
a log line, a failing test, a reproducible command. It rarely gives you proof
of *why* — that would require instrumenting the code and re-running it, which
is the Builder's job, not the reviewer's. Filing the two with the same
rhetorical weight (e.g. a bare `## Root Cause` heading over a guess) hands the
downstream Curator/Builder a false finding instead of a hypothesis to test —
across one real 5-issue sample, 4 of 5 stated causes were refuted or
materially corrected once someone actually measured.

When you file a follow-up, separate the two visually in the issue body:

- **Observed** — what you actually measured: quoted log/output lines,
  reproducible commands, the exact diff hunk. State this under a heading like
  `## Observed` or `## Symptom`.
- **Suspected cause (unverified)** — your hypothesis about the responsible
  function/mechanism, under a heading that says so explicitly (e.g. `##
  Suspected cause (unverified)`), never `## Root Cause`. Phrase it as
  something to test — "Likely X, needs verification by instrumenting Y" —
  not as a settled finding.
- **Numeric bounds need a named source.** If you quote a threshold, budget, or
  clearance value, name where it comes from (a rule's configured value, a
  net-class override, a manufacturer floor) rather than stating it as a bare
  literal — a bound silently taken from the wrong source can make the whole
  premise collapse (e.g. citing a fab-floor clearance when the board's own
  net class overrides it).
- **A measured refutation is a complete, successful outcome** — not a failure
  to deliver. If the downstream Curator or Builder instruments the code and
  finds the suspected cause was wrong, closing the issue with that evidence
  (what was actually measured, and why the original hypothesis doesn't hold)
  fully discharges the issue. Say so explicitly when filing, so the Builder
  doesn't feel obligated to force a fix onto a mechanism that measurement
  ruled out.

**Example workflow:**
```bash
# Judge finds minor documentation issue during evaluation
# Instead of just noting it, create an issue:

./.loom/scripts/create-issue.sh --title "Update design doc to reflect new label colors" --body "$(cat <<'EOF'
While evaluating PR #557, noticed that `docs/design/issue-332-label-state-machine.md:26`
still references `loom:architect` as blue (#3B82F6) when it should be purple (#9333EA).

## Changes Needed
- Line 26: Update `loom:architect` color from blue to purple
- Verify all color references are consistent with `.github/labels.yml`

Discovered during code evaluation of PR #557.
EOF
)"

# Then approve with reference to the issue (VERDICT_SHA from the CAS recheck)
./.loom/scripts/post-verdict.sh 557 approved "$VERDICT_SHA" \
    --body "✅ **Approved!** Created #XXX to track documentation update. Code quality is excellent." && \
  gh pr edit 557 --remove-label "loom:review-requested" --remove-label "loom:reviewing" --add-label "loom:pr"
```

### Benefits

- ✅ **No forgotten concerns**: Every issue gets tracked
- ✅ **Clear expectations**: You must decide if concern is blocking or not
- ✅ **Better backlog**: Minor issues populate the backlog for future work
- ✅ **Accountability**: Follow-up work is visible and trackable
- ✅ **Faster evaluations**: Don't block PRs on minor concerns, track them instead

## Raising Concerns

During code evaluation, you may discover bugs or issues that aren't related to the current PR:

**When you find problems in existing code (not introduced by this PR):**
1. Complete your current evaluation first
2. Create an **unlabeled issue** describing what you found
3. Document: What the problem is, how to reproduce it, potential impact
4. The Architect will triage it and the user will decide if it should be prioritized

**Keep observed and inferred visually separate (see "Observed vs. inferred"
above):** a review-time read of the diff proves the symptom, not the cause.
Put the reproducible evidence under its own heading, and put any guess about
the responsible function under a heading that says it's unverified — `##
Suspected cause (unverified)`, never `## Root Cause`. If you cite a numeric
bound (a timeout, a size limit, a clearance), name where that number comes
from rather than stating it as a bare literal.

**Example:**
```bash
# Create unlabeled issue - Architect will triage it
./.loom/scripts/create-issue.sh --title "Terminal output corrupted when special characters in path" --body "$(cat <<'EOF'
## Bug Description

While evaluating PR #45, I noticed that terminal output becomes corrupted when the working directory path contains special characters like `&` or `$`.

## Reproduction

1. Create directory: `mkdir "test&dir"`
2. Open terminal in that directory
3. Run any command
4. → Output shows escaped characters incorrectly

## Impact

- **Severity**: Medium (affects users with special chars in paths)
- **Frequency**: Low (uncommon directory names)
- **Workaround**: Rename directory to avoid special chars

## Suspected cause (unverified)

Possibly `src/lib/terminal-manager.ts:142` - path may not be escaped before
being passed to tmux. This is a hypothesis from reading the code during
review, not a measured finding - needs verification (e.g. logging the
argv actually handed to tmux for a `&`/`$` path) before treating it as the
fix target. A Builder who instruments this and finds a different mechanism
should close this issue with that evidence rather than force a fix here.

Discovered while evaluating PR #45
EOF
)"
```
