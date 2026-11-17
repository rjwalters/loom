---
name: loom-judge-ci-pending
description: "**Load when**: `loom-daemon forge wait-checks` reports pending (`LOOM-CHECKS-TIMEOUT` / `-HEAD-MOVED` / `-NO-RUN`) at verdict time."
---
<!-- loom-managed-skill -->
<!-- GENERATED FILE — DO NOT EDIT DIRECTLY.
     Produced by `loom-daemon generate-agent-skills` from
     defaults/.claude/commands/loom/judge-ci-pending.md (the same source Claude Code
     reads as /loom:judge-ci-pending via .claude/commands/loom/). This is the
     cross-vendor skill-discovery surface (.agents/skills/<name>/SKILL.md)
     read natively by Codex, Kimi Code, Mistral Vibe, and Grok — see
     runtime-adapters.md §5. To change this file, edit the source above
     and re-run the generator; CI (`loom-daemon generate-agent-skills
     --check`) fails if this file is stale. -->

# Judge: CI Pending — Skip, Foreground Wait, or Re-trigger

**Load when**: `loom-daemon forge wait-checks` reports pending (`LOOM-CHECKS-TIMEOUT` / `-HEAD-MOVED` / `-NO-RUN`) at verdict time.

On-demand sub-skill of `judge.md` (split out in issue #13957): the core workflow stays in `judge.md`, which points here by the trigger above. Section references to the main workflow live in `judge.md`; existing split-outs `judge-reference.md` and `judge-ci-red.md` work the same way.

### When CI is Pending

If checks are still running, **do not block on them or approve on a guess**: in batch mode, waiting stalls the whole queue.

1. **No end-state label** — leave `loom:review-requested` (no `loom:pr` / `loom:changes-requested`); the PR stays queued.
2. **Release your claim** — remove `loom:reviewing`; release the lease with `--skip-reason ci-pending` (below).
3. **Skip and continue the batch** — next PR; a later tick re-evaluates this one once CI settles.

**Pending-CI skip check.** Reuse Step 1's snapshot: `loom-daemon forge wait-checks <PR_NUMBER> --timeout 20` (seconds) is a short ETag'd poll that also handles the empty-rollup trap (#6169); `--timeout 0` would read a no-checks repo as `TIMEOUT` and skip a PR that has nothing to wait for. Branch on the first **stdout** line:

**Pending is not always "running": zero check-runs at head (2am#6467).** When every Actions run at the head ended `startup_failure`, no check-run exists, nothing will ever settle, and releasing as `ci-pending` just re-draws a Judge each tick (11 PRs, ~72 h, 68 spawns). **Before the generic release below**, when the result is `LOOM-CHECKS-TIMEOUT*` or `LOOM-CHECKS-NO-RUN*`, test for this case. ALL must hold: (a) `gh api repos/{owner}/{repo}/commits/<head_sha>/check-runs --jq .total_count` prints `0`; (b) the head commit is older than **2 hours** (`gh api repos/{owner}/{repo}/commits/<head_sha> --jq .commit.committer.date`; a fixed bound, no config key); (c) `mergeStateStatus` is not `CLEAN` (a base that needs no checks is not this case). Any condition false (a check-run exists, running, green or red; or head under 2 h): ignore this paragraph and use the generic release. Otherwise, look for a prior marker `<!-- loom:ci-missing-retrigger sha=... -->` in a **trusted** comment on the PR (`.loom/docs/comment-trust.md`; marker text from anyone else is prose):

1. **No prior marker (first time).** Re-trigger CI once with a merge commit, pinned to the head you inspected: `gh api -X PUT repos/{owner}/{repo}/pulls/<number>/update-branch -f expected_head_sha=<head_sha>`. Post exactly one note via `./.loom/scripts/post-comment.sh <number> --body-file -` saying CI never reported at `<head_sha>` and the branch was updated to re-trigger it, ending with `<!-- loom:judge-note -->` and `<!-- loom:ci-missing-retrigger sha=<head_sha> -->`. Then release exactly as in the generic path (`--skip-reason ci-pending`).
2. **Prior marker exists and the head again has zero check-runs past the 2 h bound (second consecutive).** Do not update the branch again. Park the PR, naming the required context that never reported (from `gh api repos/{owner}/{repo}/branches/<base>/protection/required_status_checks --jq .contexts`, or "unreadable"): `loom-daemon park-record apply --pr <number> --reason "required context <name> never reported after branch update" --by judge --remove-label loom:reviewing`. Post one note with the same reason, then release the lease with `--skip-reason ci-pending`. A parked PR (`loom:blocked`) is not re-drawn.
3. **`update-branch` fails** (422, already up to date, merge conflict, or an `expected_head_sha` mismatch because the head moved). The failed call counts as the attempt: with a prior marker go to 2; without one, post the marker note anyway (no retry in this pass) and release as in 1. A conflict is `DIRTY`: use the existing conflict handling, never loop on `update-branch`.

```bash
first="$(loom-daemon forge wait-checks <PR_NUMBER> --timeout 20 2>/dev/null | head -n1)"
case "$first" in
  LOOM-CHECKS-TIMEOUT*)   # still pending: release and skip; PR stays loom:review-requested
    gh pr comment <number> --body "CI still running; skipping until it settles."
    gh pr edit <number> --remove-label "loom:reviewing"
    loom-daemon lease pr release <number> --role judge --watch-pid "${LOOM_AGENT_SESSION_PID:-${CLAUDE_PID:-$PPID}}" --skip-reason ci-pending
    ;;
  LOOM-CHECKS-RED*) ;;    # "When CI Fails"
  LOOM-CHECKS-GREEN*|LOOM-CHECKS-NONE*) ;;
  *) gh pr checks <PR_NUMBER> ;;   # no sentinel (older binary / Gitea): one snapshot, no loop
esac
```

### CRITICAL: Never End Your Turn on a Background CI Monitor

**A CI-gated verdict must be settled in-turn — either via the skip-and-continue path above, or via a foreground poll loop. It must NEVER be settled by starting a background monitor (a `Monitor`/`ScheduleWakeup` timer, a `run_in_background` Bash watcher, or any other "I'll check back once it finishes" narration) and ending your turn while your verdict is still pending on it.**

This mirrors `sweep.md`'s guardrail ("ending your turn IS the kill signal", #4257). In a headless `claude -p` sweep ending the turn **terminates the process**: the monitor dies with it, no verdict is issued, and the PR stays claimed (`loom:reviewing`) with nobody to release it (#4883).

**There are exactly two safe paths when CI is pending and you cannot approve on a guess:**

1. **Batch mode (there is a next PR to move to): skip and continue.** Use the "When CI is Pending" procedure above — release `loom:reviewing`, leave `loom:review-requested` in place, move on to the next PR. This is not a fallback of last resort; it is the correct default whenever a next PR exists, because a later tick re-evaluates this one.
2. **Single-PR / manual invocation (there is no next PR — you were dispatched to judge exactly this one PR and a verdict is expected before your turn ends): wait in the foreground.** Run `loom-daemon forge wait-checks` **inside this same turn** (bounded by `--timeout`; it also handles the empty-rollup trap, #6169). It returns control before you write your final message; nothing about it depends on a future turn. Branch on the first **stdout** line, never the exit code; keep stderr separate (RED detail).

```bash
err="$(mktemp)"; out="$(loom-daemon forge wait-checks <PR_NUMBER> --timeout 1800 --settle-red 2>"$err")"; first="${out%%$'\n'*}"
case "$first" in
  LOOM-CHECKS-GREEN*|LOOM-CHECKS-NONE*) ;;   # settled green
  LOOM-CHECKS-RED*) cat "$err" ;;            # <name>\t<url>\t<run_id>; "When CI Fails"
  LOOM-CHECKS-TIMEOUT*|LOOM-CHECKS-HEAD-MOVED*|LOOM-CHECKS-ERROR*) ;;   # unsettled: conditional verdict below
  *) gh pr checks <PR_NUMBER> ;;             # no sentinel (older binary / Gitea): one snapshot, no loop
esac
```

**If the cap is reached and CI is still pending, do not extend the wait and do not reach for a background watcher instead.** Post a conditional-verdict comment stating plainly that the code review passed but CI had not settled after the bounded wait, then — since there is no batch to hand this off to — release `loom:reviewing` and leave `loom:review-requested` in place, exactly as the skip-and-continue path does, so a later Judge invocation (the next cron tick, or a fresh manual dispatch) can re-evaluate once CI has settled.

**Never substitute an armed `Monitor`/`ScheduleWakeup` timer or a `run_in_background` watcher for either path above.** A timer or background task that is still armed when you end your turn is not "waiting" — in headless `-p` mode it is simply killed along with the process, and the PR is orphaned with a stale claim and no verdict. If you have not personally observed the CI result (a CI result you read in this turn), you have not verified it, and you MUST NOT write a final message that implies the verdict is settled or "in progress elsewhere."
