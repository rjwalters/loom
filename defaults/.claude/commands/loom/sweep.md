# Sweep

Process an explicit list of issues through the full shepherd lifecycle from the current Claude session — no external daemon required. Runs sequentially by default, or in **parallel waves** of up to `N` builders when `--builders-per-wave N` is supplied.

> **Scope.** This skill accepts an explicit list of issue numbers and processes them in waves. Selectors (`label:`, `author:`, `epic:`, `topic:`), `--dry-run`, and other knobs sketched in #3298 are **deliberately deferred** — see "Limitations" below.
>
> If you need fully autonomous orchestration with work generation, use `/loom`. If you need a single-issue lifecycle, use `/shepherd <N>`. `/sweep` exists for the in-between case: "I have these N issues, run them in this session, without spinning up a daemon."

## Arguments

**Arguments**: $ARGUMENTS

Parse the arguments as a whitespace-separated list of issue numbers, with one optional flag:

- **`--builders-per-wave N`** — dispatch up to `N` builders in parallel per wave. Default `1` (fully sequential, matching the MVP behaviour). `N` must be an integer `>= 1`.

Each non-flag token must be a positive integer (optionally prefixed with `#`, e.g. `123` or `#123`).

**Validation rules:**

- At least one issue number must be supplied. If `$ARGUMENTS` is empty, display:
  ```
  Usage: /sweep <issue-number> [<issue-number> ...] [--builders-per-wave N]

  See #3298 for the full design.
  ```
  and EXIT.
- Reject any non-flag token that is not a positive integer (after stripping a leading `#`). Display an error showing the offending token and EXIT.
- Deduplicate the issue list (preserve first-seen order).
- **`--builders-per-wave N` validation:**
  - Parse `N` as an integer. Reject non-integer values with a clear error and EXIT.
  - Reject `N < 1` (including `0` and negative values) with: `Error: --builders-per-wave must be >= 1 (got: <N>)` and EXIT. Do **not** silently default to `1`.
  - If `N > 3`, print a warning and continue: `WARNING: --builders-per-wave=<N> is unvalidated. N<=3 is recommended; N>=4 may exhaust context or hit rate limits. Proceeding at your own risk.`
  - If `N` exceeds the number of candidates at any wave, **silently clamp** to the candidate count for that wave. Do not warn, do not stall.

**Wave-size guidance:**

| `N` | Status |
|-----|--------|
| `1` | Default. Fully sequential (MVP-compatible). |
| `2` | **Recommended** starting point for parallel waves. |
| `3` | Tested and validated. Fine for routine use. |
| `>= 4` | Unvalidated. Warns at parse time. Operator discretion. |

The cap is **soft** — there is no hard upper bound. The warning is the only guard.

## Examples

```bash
/sweep 123                                    # Sequential lifecycle for issue 123
/sweep 123 456 789                            # Sequential lifecycle for three issues
/sweep #1083 #1080                            # Leading # is allowed
/sweep 123 456 789 --builders-per-wave 2      # Two builders per wave (recommended)
/sweep 1 2 3 4 5 6 --builders-per-wave 3      # Three builders per wave (validated)
/sweep 1 2 --builders-per-wave 5              # Silently clamps to 2 (candidate count)
```

## Execution Model

`/sweep` processes the issue list in **waves**:

- The candidate list is partitioned into waves of up to `N = --builders-per-wave` issues (default `1`).
- Issues are picked into waves in the order given. Within a wave, builders are dispatched in parallel; across waves, processing is sequential.
- **Each wave fully settles** (all builders → per-PR Judge → optional Doctor → merge) before the next wave starts.

### CRITICAL: One level deep — never spawn `/shepherd` as a subagent

`/sweep` dispatches `loom-builder`, `loom-judge`, and `loom-doctor` subagents **directly from this orchestrator session** in a single tool-call block. This is **one level deep** and is empirically safe for `N` up to at least 3.

**Do NOT, under any circumstances, dispatch `/shepherd` as a subagent from `/sweep`.** That would be two levels deep (parent Claude → `/shepherd` Task → builder/judge Task) and triggers the parallel-shepherd stall hazard tracked in #3289 (stream-pump dies on parallel grandchildren). The wave loop in this skill is the architectural answer to that race — preserve it.

Concretely, when this skill says "dispatch builders for the wave", that means: in a single tool-call block, invoke `loom-builder` once per issue in the wave (e.g., three parallel `Task` calls if `N=3`). It does **not** mean invoke `/shepherd` three times.

If a future maintainer is tempted to "simplify" by replacing the wave-loop with parallel `/shepherd` calls: don't. Read #3289, then read this section again.

### Other constraints

- **Do NOT write to `.loom/daemon-state.json`.** That file is owned by the standalone daemon. `/sweep` runs independently and must not race with the daemon on shepherd-slot bookkeeping. Reading `daemon-state.json` for situational awareness is fine; writing is not.

## Wave Lifecycle

For each wave `W` (partition of the issue list into chunks of up to `--builders-per-wave` candidates, processed in given order), execute the full lifecycle below. **All stages are mandatory** for every issue — do not skip any stage (CLAUDE.md "Shepherd Lifecycle (MANDATORY)").

See `.claude/commands/loom/shepherd-lifecycle.md` for the canonical phase-by-phase reference, label state machine, and recovery procedures. The summary below tells you which skill to invoke at each phase; the lifecycle reference tells you what each phase does in detail.

### 1. Per-issue pre-flight (still per-issue, before the wave dispatch)

For each issue `N` in the wave, before any role skill is invoked:

1. **Verify the issue is open and not already in flight.**
   ```bash
   gh issue view N --json state,labels --jq '{state: .state, labels: [.labels[].name]}'
   ```
   - If the issue is closed, skip it (log a warning). It does NOT contribute to this wave.
   - If the issue already has `loom:building`, skip it — another shepherd or builder is working on it. Log a warning. Does NOT contribute to this wave.
   - If the issue has `loom:blocked`, skip it. Log a warning. Does NOT contribute to this wave.

2. **Read the issue body before briefing any builder.** This is a non-negotiable rule from prior sweep sessions (a misleading title hid the real requirement in the body).
   ```bash
   gh issue view N --json title,body
   ```

> **Pre-flight skip rule.** If `K` of the wave's `N` candidates are skipped at pre-flight, dispatch only `N - K` builders for this wave. **Do not pull a candidate forward** from the next wave to backfill. Wave boundaries stay clean, and the next wave runs at its originally planned size.

### 2. Curator phase (still per-issue, before the wave dispatch)

For each surviving issue `N` in the wave:

- If the issue does not already have `loom:curated` or `loom:issue`, run the curator skill on it.
  - Load and follow the instructions in `.claude/commands/loom/curator.md` for issue `N`.
  - Expected exit state: issue has `loom:curated`.
- If the issue already has `loom:curated` or `loom:issue`, skip the curator phase.

Curator runs sequentially per-issue within wave setup — it is cheap and does not benefit from parallelism here.

### 3. Approval gate (per-issue)

Each issue must reach `loom:issue` before the Builder can claim it.

- If the issue already has `loom:issue`, proceed.
- Otherwise, promote it:
  ```bash
  gh issue edit N --remove-label "loom:curated" --add-label "loom:issue"
  ```

### 4. Builder phase (parallel within the wave)

Dispatch up to `min(--builders-per-wave, surviving-candidates-in-wave)` `loom-builder` subagents **in a single tool-call block** from this orchestrator session. **Do NOT invoke `/shepherd` as a subagent here** — see the "One level deep" rule in Execution Model above.

Each builder is responsible for:

- Claiming its issue (`loom:issue` → `loom:building`).
- Creating an issue worktree via `./.loom/scripts/worktree.sh N`.
- Implementing the change, running tests, committing.
- Pushing the branch and opening a PR labeled `loom:review-requested`.
- Closing references: `Closes #N` in the PR body.

**Await all builders in the wave** before proceeding to Judge. Collect each builder's PR number (or failure marker).

**Per-builder failure isolation.** If builder for issue `#A` fails to open a PR (build error, test failure, unrecoverable conflict, etc.), log it and **continue** with the other builders' PRs in this wave. The failed issue is recorded as `blocked (builder failed)` in the summary. Do NOT abort the wave. Do NOT skip Judge for the other PRs.

### 5. Judge phase (sequential per PR within the wave)

For each PR successfully opened in the wave, in the order the builders completed (or any deterministic order — wave-internal ordering is not load-bearing), run the Judge phase sequentially:

```
for pr in wave_prs:
    judge(pr)               # may approve or request changes
    if changes_requested:
        doctor(pr)          # one Doctor->Judge cycle (see step 6)
    if still_approved:
        merge(pr)           # step 7
```

- Load and follow the instructions in `.claude/commands/loom/judge.md` for the PR.
- The judge uses `gh pr comment` (NOT `gh pr review --approve`) because GitHub's self-review API restriction applies — see `judge.md` for the full explanation.
- Expected exit states per PR:
  - **Approve** → PR labeled `loom:pr`. Continue to Merge (step 7) for this PR, then advance to the next PR in the wave.
  - **Request changes** → PR labeled `loom:changes-requested`. Continue to Doctor (step 6) **inline for this PR**, then re-judge, then merge or block.

**Why sequential and not parallel?** Parallel Judges add coordination complexity without clear benefit — each judge needs to checkout the PR and reason about it independently. Defer parallel-judge to a future issue if benchmarks justify it.

### 6. Doctor phase (inline per PR, only if Judge requested changes)

If Judge requests changes on PR `#X` mid-wave, run a **single inline Doctor→Judge cycle** for `#X` before moving to the next PR's Judge:

- Load and follow the instructions in `.claude/commands/loom/doctor.md` for PR `#X`.
- Doctor addresses the judge's feedback, commits the fixes, and pushes.
- On completion, re-label the PR from `loom:changes-requested` back to `loom:review-requested` and **re-run the Judge phase** (step 5) for this PR.
- **Limit: a single Doctor→Judge cycle per PR.** If Judge still requests changes after one Doctor pass, mark this PR as blocked, log a warning, and proceed to the next PR in the wave (do NOT block the wave on it).

The Doctor cycle for `#X` does **not** block other PRs in the wave — but because Judge runs sequentially per-PR within the wave, the next PR's Judge waits for `#X`'s Doctor→Judge cycle to settle before it starts. This is the intended sequencing.

### 7. Merge (per PR)

Use the dedicated merge script (CLAUDE.md "Merging PRs" mandate — never `gh pr merge`):

```bash
./.loom/scripts/merge-pr.sh <PR_NUMBER> --auto
```

The script merges via the forge API and cleans up the worktree.

### 8. Wave settled → advance to next wave

Once every PR in the wave has reached a terminal state (merged, blocked, or builder-failed), advance to the next wave. Do not start the next wave's builders until the current wave's PRs are all settled.

## Summary Output

When the entire list has been processed, print a summary table that includes wave membership for each issue:

```
/sweep complete. Processed M issue(s) across W wave(s):

  #123  → merged  (PR #456)                                              [wave 1]
  #124  → blocked (judge requested changes, doctor cycle exhausted)      [wave 1]
  #125  → skipped (already in flight: loom:building)                     [wave 1]
  #126  → blocked (builder failed: build error)                          [wave 2]
  #127  → merged  (PR #459)                                              [wave 2]

Total: 2 merged, 2 blocked, 1 skipped.
```

Wave annotation makes it easier to triage failures (e.g., "every issue in wave 2 failed → probably a base-branch problem, not the issues themselves").

## Stop Conditions

Stop processing and print the summary when any of these conditions hold:

- The issue list is exhausted.
- The user interrupts (Ctrl-C or explicit stop).
- An unrecoverable error occurs (e.g., `gh` is not authenticated, repository state is broken). Log the error and exit.

This skill does **not** implement disk-pressure checks, max-waves caps, or doctor-cycle global limits — those are deferred (see Limitations).

## Daemon Coexistence

`/sweep` does not require the daemon and does not interact with `.loom/daemon-state.json` for writes. If the daemon is running, `/sweep` and the daemon may both try to claim the same `loom:issue` label.

**Coexistence behavior:** before the first wave, check whether the daemon is running. If it is, warn the user once at the start of the sweep:

```bash
PID=$(cat .loom/daemon-loop.pid 2>/dev/null)
if [[ -n "$PID" ]] && kill -0 "$PID" 2>/dev/null; then
  echo "⚠️  Loom daemon is running (PID $PID). /sweep will race with the daemon"
  echo "   for issues in the loom:issue queue. Consider stopping the daemon first:"
  echo "       ./.loom/scripts/daemon.sh stop"
fi
```

Do not auto-stop the daemon. Do not block on this warning — proceed with the sweep.

Per-issue, the pre-flight check (step 1) already detects `loom:building` and skips, which is the natural defense against races: if the daemon claimed an issue first, `/sweep` will see `loom:building` and skip.

## Constraints

- **Wave model, one level deep.** When `--builders-per-wave > 1`, dispatch `loom-builder` / `loom-judge` / `loom-doctor` subagents **directly from this orchestrator session** in a single tool-call block. **Never invoke `/shepherd` as a subagent from `/sweep`** — that is the two-levels-deep pattern that triggers the #3289 stall. See "CRITICAL: One level deep" in the Execution Model.
- **Per-PR Judge is sequential within a wave.** Builders parallelize, judges do not. Don't parallelize judges without a separate design pass.
- **Single Doctor→Judge cycle per PR.** Inline within the wave. If it still fails, the PR is blocked — do not retry indefinitely.
- **No new labels.** Use only the existing Loom label set (see `.github/labels.yml`).
- **No `gh pr merge`.** Always use `./.loom/scripts/merge-pr.sh`.
- **No daemon-state writes.** Read-only access to `daemon-state.json` for situational awareness.
- **Read the issue body** (`gh issue view N --json body`) before briefing the builder. Don't rely on the title.
- **Skip operator-only issues** (issues that require human action, such as releases or credential changes). Log and move on.

## Limitations (Deferred for Follow-up Issues)

The full `/sweep` design in #3298 includes many features that are intentionally **not** part of this skill yet. Each of these is a candidate follow-up issue:

| Feature | Status | Notes |
|---------|--------|-------|
| Parallel waves (`--builders-per-wave N`) | **Implemented (#3316)** | Soft cap at N=3 (warns above). One level deep — no `/shepherd` subagent. |
| Selectors (`label:`, `author:`, `epic:#N`, `topic:`, bare invocation) | Deferred (#3318) | Currently only explicit issue numbers are accepted. |
| `--dry-run` | Deferred (#3319) | Useful for validating a candidate list before committing to side effects. |
| `--max-waves` cap | Deferred | Operator-level brake on long sweeps. |
| `--paused-merge` / `--no-judge` | Deferred | Merge-mode variants for trusted batches. |
| `--include-blocked` (unblock pass) | Deferred | Currently `/sweep` skips `loom:blocked` issues outright. |
| `--curator-also` (parallel curators on `loom:triage`) | Deferred | Parallel triage is a separate orchestration question. |
| Config-driven defaults (`.loom/config.json` keys `sweep.*`) | Deferred | No knobs to configure yet. |
| Disk-pressure stop condition | Deferred | Wave sequencing limits disk usage; revisit if waves grow large. |
| Doctor-cycle counting across PRs | Deferred | Single Doctor→Judge cycle limit per PR is enforced inline. |
| Parallel Judges within a wave | Deferred | Sequential per-PR Judge today; needs benchmarking before parallelizing. |
| Cross-wave backfill on pre-flight skips | Won't fix | Intentionally clean wave boundaries — see step 1 of the Wave Lifecycle. |
| Spinoff-issue filing for out-of-scope discoveries | Deferred | Build it once we have richer summary output to surface them cleanly. |
| Daemon `pipeline_state` situational awareness reads | Deferred | Skill only warns when the daemon is running. |
| Top-level vs namespaced naming (`/sweep` vs `/loom:sweep`) | Open question | Ships as `/sweep` per the original task brief; rename later if convention favors `/loom:sweep`. See #3298 open question #1. |

For the full design discussion (including the open questions raised by the curator), see issue #3298.

## Reference Documentation

- **Shepherd lifecycle**: `.claude/commands/loom/shepherd-lifecycle.md` — canonical per-issue lifecycle.
- **Builder skill**: `.claude/commands/loom/builder.md`
- **Judge skill**: `.claude/commands/loom/judge.md`
- **Doctor skill**: `.claude/commands/loom/doctor.md`
- **Curator skill**: `.claude/commands/loom/curator.md`
- **Label definitions**: `.github/labels.yml`
- **Merge script**: `./.loom/scripts/merge-pr.sh`
- **Original proposal & open questions**: issue #3298
- **Parallel-shepherd stall hazard**: issue #3289
