# Judge fan-out measurement runbook (issue #3748)

**Audience:** an operator with the Workflow tool — i.e. a person driving a
**top-level** Claude Code session, not a subagent.

**What this runbook is for:** executing the *deferred* measurement that #3739
(design sketch) and #3748 (hardened artifact + scaffold) could not perform. A
`loom-builder` subagent cannot run it, because:

- The Workflow tool (`agent()` / `parallel()` / `workflow()`) is exposed only to
  a top-level Claude Code session. A subagent dispatched via the Task tool has no
  access to it.
- Even if it did, driving the workflow from inside a subagent would add a
  **second nesting level** (subagent → workflow-agent), which is exactly the
  #3289 one-level-deep stall the evaluation forbids.

So the builder delivered everything *short of* invoking the Workflow tool: the
hardened workflow, a measurement-harness scaffold, an empty results template,
and tests. This runbook is how **you** finish the job.

> **Do not fabricate numbers.** The evaluation doc
> (`docs/research/dynamic-workflows-evaluation.md`) is explicit that no
> precision/recall/latency/cost numbers were invented, and #3748 keeps that
> invariant. If you did not measure it, leave the cell blank.

---

## Artifacts

| File | Role |
|------|------|
| `defaults/scripts/experiments/judge-fanout-workflow.js` | The workflow you invoke via the Workflow tool. Fan-out reviewers + adversarial verify + typed reduce. Read-only; returns a verdict. |
| `defaults/scripts/experiments/judge-fanout-corpus-runner.sh` | Scaffold that builds per-PR `args` from `gh pr diff` and emits an empty results table. Does **not** invoke the Workflow tool. |
| `defaults/scripts/experiments/judge-fanout-results-template.md` | Empty results table you fill in with real numbers. |
| `defaults/scripts/experiments/judge-fanout-experiment.sh` | Off-by-default gate. Syntax-checks the two above when `LOOM_JUDGE_FANOUT_EXPERIMENT=1`; never runs a live judge. |

Everything stays **off the production path**: none of this is wired into
`defaults/roles/judge.md` or the sweep Judge phase, and the flag stays off by
default.

---

## Step 1 — Select the corpus (3–5 PRs)

Pick 3–5 already-closed PRs with **known ground-truth outcomes** — a mix of:

- **merged** PRs (the fan-out should mostly `approve` these), and
- **rejected** PRs (the fan-out should catch the real defect and return
  `changes-requested`).

Prefer PRs whose real defects span the four dimensions
(`correctness`, `security-credential-surface`, `test-coverage`,
`perf-simplification`) so recall is actually exercised. Record each PR number and
its true outcome before running — that is your answer key.

## Step 2 — Build per-PR args + the empty results table

From the repo root (a normal shell is fine for this step — it is read-only on
GitHub):

```bash
cd defaults/scripts/experiments
./judge-fanout-corpus-runner.sh --out /tmp/jf 3721:merged 3699:rejected 3688:merged
```

This writes, under `/tmp/jf/`:

- `pr-<N>.args.json` — the exact `{ "pr": <N>, "diff": "<unified diff>" }` object
  the Workflow tool wants, one per PR.
- `results.md` — an empty results table, one row per PR (every measured cell
  blank).

`--dry-run` previews without touching anything; `--diff-only` skips the table.

## Step 3 — Invoke the workflow from a top-level session

This is the step only a top-level session can do. For each PR, invoke the
Workflow tool with the script path and that PR's args:

```
Workflow({
  scriptPath: "<abs path>/defaults/scripts/experiments/judge-fanout-workflow.js",
  args:       <paste the contents of pr-<N>.args.json>,
})
```

Alternatively, copy `judge-fanout-workflow.js` into a discovered `workflows/`
directory (user or project settings) so the CLI auto-loads it, then invoke it by
name.

**Guardrails to preserve while you run it:**

- **Top-level only, never a subagent** — keeps the workflow's `agent()` /
  `parallel()` calls at the first nesting level (#3289-safe).
- **Never from a `fable` session** — the fan-out reviewers play the Judge role,
  and the No-Fable-Judge invariant (#3702) says a Judge/reviewer model must never
  be `fable`. The workflow's `assertNotFable()` guard rejects an explicit
  `args.model: "fable"`, but a `fable` **session default** would still be
  inherited, so run from an opus/sonnet session. Optionally pass an explicit
  non-fable `args.model` to pin the reviewers.
- **Read-only** — the workflow returns a verdict object; it applies no labels,
  merges nothing, and creates no issues. Do not add side effects.

Capture, per PR: `result.verdict`, `result.findings` (verified),
`result.droppedUnverified`, plus the wall-clock latency and token cost of the
call (from the session transcript usage).

## Step 4 — Establish the single-pass baseline

For the same PRs, review each with **today's** single-pass Judge
(`defaults/roles/judge.md`) and record its verdict, latency, and token cost.
This is the A/B the whole exercise exists to produce.

## Step 5 — Record real numbers

Fill `results.md` (shape mirrors `judge-fanout-results-template.md`). Column
provenance:

- **Dimension findings** — raw findings summed across the four reviewers (before
  verify).
- **Verified findings** — `result.findings.length` (what the adversarial pass
  kept).
- **Precision** — `verified / (verified + droppedUnverified)` = `1 -
  unverified-nit rate`.
- **Recall** — dimensions that produced a *verified* finding, scored against the
  PR's known real defects.
- **Fan-out latency (s)** — wall-clock of the `Workflow({...})` call.
- **Token cost** — tokens/$ for the run (transcript usage).
- **Single-pass Judge baseline** — Step 4's numbers.

## Step 6 — Keep / kill call (still deferred, but now grounded)

With the measured table in hand, make the keep/kill decision the evaluation
deferred. If **keep**, file a follow-up proposing how the fan-out slots behind
the sweep Judge phase behind `LOOM_JUDGE_FANOUT_EXPERIMENT` / a config flag
(still opt-in). If **kill**, record why in `results.md` and leave the production
Judge path unchanged.

> This step is a **deferred operator action**, explicitly out of scope for the
> #3748 builder deliverable — it requires the measured numbers, which require the
> Workflow tool.
