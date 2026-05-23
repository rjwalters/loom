# Sweep (MVP)

Process an explicit list of issues through the full shepherd lifecycle **sequentially, one at a time**, from the current Claude session — no external daemon required.

> **MVP scope.** This is a minimal first cut of `/sweep`. It accepts only an explicit list of issue numbers and runs them one after the other. Selectors (`label:`, `author:`, `epic:`, `topic:`), parallel waves, daemon-state coordination, dry-run, and the other features sketched in #3298 are **deliberately deferred** — see "Limitations" below and the open questions on #3298 for the full design discussion.
>
> If you need parallel orchestration today, use `/loom` (autonomous daemon). If you need a single-issue lifecycle, use `/shepherd <N>`. `/sweep` exists for the in-between case: "I have these 3 issues, run them sequentially, in this session, without spinning up a daemon."

## Arguments

**Arguments**: $ARGUMENTS

Parse the arguments as a whitespace-separated list of issue numbers. Each token must be a positive integer (optionally prefixed with `#`, e.g. `123` or `#123`).

**Validation rules:**

- At least one issue number must be supplied. If `$ARGUMENTS` is empty, display:
  ```
  Usage: /sweep <issue-number> [<issue-number> ...]

  MVP scope: explicit issue list only. See #3298 for the full design.
  ```
  and EXIT.
- Reject any token that is not a positive integer (after stripping a leading `#`). Display an error showing the offending token and EXIT.
- Deduplicate the list (preserve first-seen order).

## Examples

```bash
/sweep 123                       # Sequential lifecycle for issue 123
/sweep 123 456 789               # Sequential lifecycle for three issues in order
/sweep #1083 #1080               # Leading # is allowed
```

## Execution Model

`/sweep` is **synchronous and sequential**:

- Issues are processed in the order given.
- For each issue, run the full shepherd lifecycle (Curator → Builder → Judge → Doctor → Merge) **before** moving on to the next issue.
- **Do NOT parallelize.** Running multiple shepherd-like flows concurrently from a single Claude session is the failure mode tracked in #3289. Sequential execution is intentional in this MVP.
- **Do NOT write to `.loom/daemon-state.json`.** That file is owned by the standalone daemon. `/sweep` runs independently and must not race with the daemon on shepherd-slot bookkeeping. Reading `daemon-state.json` for situational awareness is fine; writing is not.

## Per-Issue Lifecycle

For each issue `N` in the parsed list, execute the full lifecycle below. **All stages are mandatory** — do not skip any stage (CLAUDE.md "Shepherd Lifecycle (MANDATORY)").

See `.claude/commands/loom/shepherd-lifecycle.md` for the canonical phase-by-phase reference, label state machine, and recovery procedures. The summary below tells you which skill to invoke at each phase; the lifecycle reference tells you what each phase does in detail.

### 1. Pre-flight check (per issue)

Before invoking any role skill for issue `N`:

1. **Verify the issue is open and not already in flight.**
   ```bash
   gh issue view N --json state,labels --jq '{state: .state, labels: [.labels[].name]}'
   ```
   - If the issue is closed, skip it and continue to the next issue (log a warning).
   - If the issue already has `loom:building`, skip it — another shepherd or builder is working on it. Log a warning and continue.
   - If the issue has `loom:blocked`, skip it. Log a warning and continue.

2. **Read the issue body before briefing any builder.** This is a non-negotiable rule from prior sweep sessions (a misleading title hid the real requirement in the body).
   ```bash
   gh issue view N --json title,body
   ```

### 2. Curator phase

If the issue does not already have `loom:curated` or `loom:issue`, run the curator skill on it.

- Load and follow the instructions in `.claude/commands/loom/curator.md` for issue `N`.
- Expected exit state: issue has `loom:curated`.

If the issue already has `loom:curated` or `loom:issue`, skip the curator phase.

### 3. Approval gate

The issue must reach `loom:issue` before Builder can claim it.

- If the issue already has `loom:issue`, proceed to Builder.
- Otherwise, promote it:
  ```bash
  gh issue edit N --remove-label "loom:curated" --add-label "loom:issue"
  ```

### 4. Builder phase

Load and follow the instructions in `.claude/commands/loom/builder.md` for issue `N`. The builder skill is responsible for:

- Claiming the issue (`loom:issue` → `loom:building`).
- Creating an issue worktree via `./.loom/scripts/worktree.sh N`.
- Implementing the change, running tests, committing.
- Pushing the branch and opening a PR labeled `loom:review-requested`.
- Closing references: `Closes #N` in the PR body.

Capture the PR number created by the builder. If the builder fails to open a PR, **stop processing this issue** (do not block the rest of the sweep — log and continue to the next issue).

### 5. Judge phase

Load and follow the instructions in `.claude/commands/loom/judge.md` for the PR opened by the builder.

- The judge uses `gh pr comment` (NOT `gh pr review --approve`) because GitHub's self-review API restriction applies — see `judge.md` for the full explanation.
- Expected exit states:
  - **Approve** → PR labeled `loom:pr`. Continue to Merge (step 7).
  - **Request changes** → PR labeled `loom:changes-requested`. Continue to Doctor (step 6).

### 6. Doctor phase (only if Judge requested changes)

Load and follow the instructions in `.claude/commands/loom/doctor.md` for the PR.

- Doctor addresses the judge's feedback, commits the fixes, and pushes.
- On completion, re-label the PR from `loom:changes-requested` back to `loom:review-requested` and **re-run the Judge phase** (step 5).
- Limit: a single Doctor→Judge cycle per PR in this MVP. If Judge still requests changes after one Doctor pass, mark the PR as blocked, log a warning, and continue to the next issue.

### 7. Merge

Use the dedicated merge script (CLAUDE.md "Merging PRs" mandate — never `gh pr merge`):

```bash
./.loom/scripts/merge-pr.sh <PR_NUMBER> --auto
```

The script merges via the forge API and cleans up the worktree.

### 8. Move on

Once issue `N` is fully processed (merged, blocked, or skipped), move to the next issue in the list. Do not start work on issue `N+1` until issue `N` has reached a terminal state.

## Summary Output

When the entire list has been processed, print a summary table:

```
/sweep complete. Processed M issue(s):

  #123  → merged  (PR #456)
  #124  → blocked (judge requested changes, doctor cycle exhausted)
  #125  → skipped (already in flight: loom:building)
  #126  → merged  (PR #459)

Total: 2 merged, 1 blocked, 1 skipped.
```

## Stop Conditions

Stop processing and print the summary when any of these conditions hold:

- The issue list is exhausted.
- The user interrupts (Ctrl-C or explicit stop).
- An unrecoverable error occurs (e.g., `gh` is not authenticated, repository state is broken). Log the error and exit.

The MVP does **not** implement disk-pressure checks, max-waves caps, or doctor-cycle global limits — those are deferred (see Limitations).

## Daemon Coexistence

`/sweep` does not require the daemon and does not interact with `.loom/daemon-state.json` for writes. If the daemon is running, `/sweep` and the daemon may both try to claim the same `loom:issue` label.

**MVP behavior:** before processing each issue, check whether the daemon is running. If it is, warn the user once at the start of the sweep:

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

- **Sequential only.** Do not invoke builder/judge/doctor on multiple issues in parallel from this skill. (See #3289.)
- **No new labels.** Use only the existing Loom label set (see `.github/labels.yml`).
- **No `gh pr merge`.** Always use `./.loom/scripts/merge-pr.sh`.
- **No daemon-state writes.** Read-only access to `daemon-state.json` for situational awareness.
- **Read the issue body** (`gh issue view N --json body`) before briefing the builder. Don't rely on the title.
- **Skip operator-only issues** (issues that require human action, such as releases or credential changes). Log and move on.

## Limitations (Deferred for Follow-up Issues)

The full `/sweep` design in #3298 includes many features that are intentionally **not** part of this MVP. Each of these is a candidate follow-up issue:

| Feature | Status | Notes |
|---------|--------|-------|
| Selectors (`label:`, `author:`, `epic:#N`, `topic:`, bare invocation) | Deferred | MVP accepts only explicit issue numbers. |
| Parallel waves (`--builders-per-wave`, `--max-waves`) | Deferred | #3289 stall hazard with parallel shepherds — needs design work before re-introducing parallelism. |
| `--dry-run` | Deferred | Useful for validating a candidate list before committing to side effects. |
| `--paused-merge` / `--no-judge` | Deferred | Merge-mode variants for trusted batches. |
| `--include-blocked` (unblock pass) | Deferred | Currently `/sweep` skips `loom:blocked` issues outright. |
| `--curator-also` (parallel curators on `loom:triage`) | Deferred | Parallel triage is a separate orchestration question. |
| Config-driven defaults (`.loom/config.json` keys `sweep.*`) | Deferred | No knobs to configure yet. |
| Disk-pressure stop condition | Deferred | Single-issue-at-a-time sequencing already limits disk usage. |
| Doctor-cycle counting across PRs | Deferred | MVP uses a single Doctor→Judge cycle limit per PR. |
| Spinoff-issue filing for out-of-scope discoveries | Deferred | Build it once we have the parallel/wave machinery to surface them cleanly. |
| Daemon `pipeline_state` situational awareness reads | Deferred | MVP only warns when the daemon is running. |
| Top-level vs namespaced naming (`/sweep` vs `/loom:sweep`) | Open question | This MVP ships as `/sweep` per the task brief; can be renamed if mainline convention favors `loom:sweep`. See #3298 open question #1. |

For the full design discussion (including the four open questions raised by the curator), see issue #3298.

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
