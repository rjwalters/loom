# ADR-0015: In-Builder Test-First Checkpoint — PR-Body Signal, Advisory on Absence, Blocking on Contradiction

## Status

Accepted. Implemented in the same PR as this ADR (issue #5849).

## Context

Issue #5844/PR #5852 (`docs/research/atomic-claude-evaluation.md`) evaluated
[damusix/atomic-claude](https://github.com/damusix/atomic-claude) (MIT) and
verdicted **adapt** on its maker/checker TDD split: `agents/atomic-implementer.md`
requires "write failing test first... run it, confirm it fails for the right
reason... implement... run again, confirm green" (new behavior) or "write a
test that reproduces the bug... then fix... then confirm green" (bug fixes),
and `agents/atomic-reviewer.md` independently **re-verifies** that claim rather
than trusting it: "run tests yourself, confirm... If implementer's claim
doesn't match reality → `🔴 bug`."

Loom already runs the cross-session half of that evaluator-optimizer shape —
Builder implements, Judge reviews in a fresh context, on the PR (`CLAUDE.md`
§"Sweep Lifecycle"). What's missing is the **in-Builder** discipline: nothing
in `builder.md` requires the test to exist before the fix, and nothing gives
Judge a checkable trace of whether that happened. Issue #5849 asked this
Builder dispatch to resolve three open design questions rather than merely
restate them:

1. A concrete, checkable signal for "test written before fix."
2. Advisory vs. blocking, with rationale.
3. Explicit scope: in-Builder discipline only, not a replacement for the
   Builder → Judge PR cycle.

## Decision

### 1. The signal: a required `TDD:` line in the PR body's Test Plan section

Every PR whose diff touches executing code (not docs-only, not pure config)
must carry one line of the form:

```
TDD: yes — <test path> written first; failed for the right reason before the fix, passes after
TDD: no — <reason: pure refactor with existing coverage | docs/config-only | design/investigation issue, no code | …>
```

**Why a PR-body line, not a commit-order check.** A commit-order check (`git
log` showing a test-touching commit strictly before the fix commit) was the
other candidate raised in #5849. Rejected as the *primary* signal because Loom
builders frequently produce a single squashed commit per PR (`merge-pr.sh`
squash-merges by default), so "the test commit came first" is often
unobservable after the fact, and a Builder amending/rewriting history to
satisfy a commit-order check would fight the workflow rather than describe it.
A prose line is simple to produce, works identically for the new-feature and
bug-fix cases `atomic-implementer.md` distinguishes, and — critically — is
independently checkable against the diff by Judge (see below), which is the
property a commit-order check would only partially give anyway (order without
content).

**Where it lives**: `defaults/.claude/commands/loom/builder-pr.md`, in the
canonical PR body template (`## Test Plan` section) and a new named
subsection Builder reads before creating the PR.

### 2. Advisory on absence, blocking on contradiction

Not a single advisory/blocking toggle — the enforcement differs by failure
mode, mirroring how Judge already treats the adjacent "## Test Plan" section
(`judge.md` § "Test Plan Execution": "No test plan in PR → Note absence in
evaluation; don't block approval") and how `atomic-reviewer.md` itself treats
an *unverified claim* (a hard bug) differently from an *absent claim* (nothing
to re-verify):

| Case | Judge action |
|---|---|
| `TDD:` line **absent** entirely | Note the absence in the evaluation comment. **Does not block approval.** |
| `TDD: no — <reason>` present | Accept if the reason is plausible for the diff (e.g. docs/config-only, refactor with pre-existing coverage). **Does not block.** |
| `TDD: yes — <path>` present, diff **corroborates** it (the referenced test file/path appears in the changed-files list) | Accept as verified. |
| `TDD: yes — <path>` present, diff **contradicts** it (no test file touched at all, or the referenced path was not changed) | **Blocking** — request changes, same class of finding as any other inaccurate claim in a PR description. |

**Rationale for the split, not a single toggle:**

- **Hard-blocking on absence from day one would misfire on exactly the issue
  that produced this ADR.** #5849 itself is a design-investigation issue with
  no application code to write a test for. A repo runs many such issues
  (docs, ADRs, config, research) where "write a failing test first" has no
  referent. A blanket gate would force either a bureaucratic `TDD: no — N/A`
  ritual on every PR (low signal, pure friction) or, worse, train Builders to
  paste a placeholder line to satisfy the gate — the "superficial fix" failure
  mode `builder.md` § "Root Cause Verification" already names.
- **A false `yes` is categorically different from a missing line.** A false
  claim is not a missing-discipline problem, it's a misrepresentation problem
  — the exact class of thing Judge already blocks on (an acceptance-criteria
  checkbox claimed done that isn't; a claimed-passing test suite that
  actually fails). `atomic-reviewer.md`'s own design agrees: it treats a
  claim contradicted by reality as `🔴 bug`, not a style note. Blocking there
  costs nothing in false positives (Judge is comparing a specific claimed path
  against the actual diff, a cheap and unambiguous check) and closes exactly
  the gap atomic-claude's maker/checker split targets: an implementer's
  unverified self-report.
- **This is not a `buildGate` extension.** `buildGate` (`.loom/docs/build-gate.md`)
  is deliberately a deterministic, host-side, orchestrator-run check
  (has-commits / has-real-changes / build-passes) with no access to PR
  metadata and no semantic judgment — its own doc's rule of thumb is "if a
  check's outcome can differ between hosts, or requires judgment, it doesn't
  belong in the gate." Classifying whether a `TDD: no` reason is *plausible*
  for a given diff, or whether a referenced test path is the *right* one, is
  exactly the judgment call `buildGate` is designed to exclude. Judge — which
  already reads the PR body, the diff, and exercises judgment on every other
  claim in the description — is the natural fit, not a new deterministic gate
  stage.

### 3. Scope: in-Builder discipline only

This checkpoint governs what one Builder does inside its own PR turn. It does
not change, replace, or shortcut the `Builder → Judge → Doctor → Merge`
lifecycle (`CLAUDE.md` § "Sweep Lifecycle"), does not add a new label state,
and does not give Judge authority it didn't already have (Judge already
requests changes for inaccurate PR-description claims — this ADR just names
the `TDD:` line as one specific claim worth checking, per the "adapt, not
adopt" verdict in #5844's evaluation: Loom keeps the PR-level cross-context
review as the loop; this closes only the narrower in-Builder gap).

## Consequences

### Positive

- Judge gets a cheap, targeted way to catch the exact failure atomic-claude's
  reviewer targets — an implementer's unverified "tests pass" claim — without
  duplicating its whole implement→review loop machinery.
- Non-code and investigation-shaped PRs (like this one) are not penalized;
  the advisory-on-absence default means the mechanism degrades gracefully for
  the issues where "test-first" has no referent.
- No new label state, no new gate stage, no new script — the enforcement
  point is Judge's existing PR-body reading + diff comparison, already part
  of every review.

### Negative

- Relies on Judge actually reading and checking the line every pass (no
  automated grep/CI enforcement at merge time). A future follow-up could add
  a lightweight `merge-pr.sh`-side grep as a backstop, mirroring the
  `Closes #N` / `Part of #N` contradiction check already there — deliberately
  **not** done here; see Alternatives.
- The `TDD: no` reason is currently free text, so a Builder could write an
  implausible reason for a diff that clearly touches new behavior. Judge is
  expected to apply the same judgment it already applies to every other PR
  claim; no new automated plausibility check is added.

## Alternatives Considered

- **Commit-order check (test commit before fix commit) as the primary
  signal.** Rejected as primary — see "The signal" above (squash-merge
  workflow makes order frequently unobservable after the fact); could still
  be added later as a *secondary*, best-effort corroboration when multiple
  commits exist, but is not required by this decision.
- **Hard-blocking `buildGate` extension** (fail the gate if no test file
  changed alongside a non-doc diff). Rejected: `buildGate` has no PR-body
  access and no judgment surface; a pure file-glob heuristic ("did *.rs
  change alongside *_test.rs") produces both false positives (a bug fix in
  one file with the regression test already covering it) and false negatives
  (renamed/relocated test files), with no way to read a Builder's stated
  rationale the way Judge can.
- **Full port of the implement→review loop** (a Builder-internal stuck-fix
  escalation / N-iteration soft-stop mirroring
  `commands/subagent-implementation.md`). Rejected in #5844's evaluation
  itself: this would duplicate the PR-level `Builder → Judge → Doctor` cycle
  Loom already runs for the identical purpose.
- **`merge-pr.sh`-side grep backstop at merge time** (mirroring the
  `Closes #N` contradiction backstop). Left as an explicit future option (see
  Consequences → Negative) rather than shipped now — the Judge-side check
  already covers the primary case, and adding a second enforcement surface
  before the first one has any operating history would be premature.

## References

- Source issue: [#5849](https://github.com/rjwalters/loom/issues/5849)
- Research: `docs/research/atomic-claude-evaluation.md` § 3 (from #5844/#5852)
- Implementation: `defaults/.claude/commands/loom/builder-pr.md` §
  "Test-First Discipline (TDD line)", `defaults/.claude/commands/loom/judge.md`
  § "Test-First (TDD) Claim Verification", `defaults/.claude/commands/loom/builder.md`
  (pointer only)
- Related: `.loom/docs/build-gate.md` (why this is not a `buildGate` check)
