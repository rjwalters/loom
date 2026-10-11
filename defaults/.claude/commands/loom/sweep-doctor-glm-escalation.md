# Sweep: bounded GLM Doctor rejections (#11359)

Load only when a Doctor phase is about to dispatch AND `runtimes.rolePreference.doctor`
(or the fleet `runtimes.preference`) can resolve a GLM tap. Applies identically to
Modes A/B (`sweep-wave-lifecycle.md` §6) and Mode C (`sweep-mode-c-lifecycle.md` C1b).
It is a retry policy on the **existing** Doctor cycle and the **existing**
`sweep.escalation` ladder; it adds no ladder and never changes `sweep.max_doctor_cycles`.

## Config

`sweep.glmDoctorRejectionsBeforeEscalation` = K, a positive integer, default `2`.
Anything else is a config error naming the key (`loom-daemon validate`, and exit 78
from `doctor-rejection`); do not guess a value.

## State (in the sweep checkpoint, key `doctor_escalation`)

Written only through `sweep-checkpoint.sh` (needs the issue checkpoint; without one the
policy is inactive and Doctor uses its role preference). Later `write`s retain it; a
`curator-done`/`builder-done` write or `delete` resets it (fresh sweep); an absent key
(legacy checkpoint) means zero rejections. It is keyed to the PR number.

## Procedure

1. **After every completed Doctor attempt** (it pushed and re-labeled the PR), before the
   follow-up Judge, record the head it produced and where it ACTUALLY ran, from the
   `worker run --json` report (`runtime`, `model_profile`), not from configuration:
   ```bash
   ./.loom/scripts/sweep-checkpoint.sh doctor-attempt N --pr P --head <headRefOid> --runtime <runtime> --profile <model_profile>
   ```
2. **On each Judge rejection**, before dispatching the next Doctor and in addition to the
   `judge-rejected` write, report the head the verdict reviewed:
   ```bash
   ./.loom/scripts/sweep-checkpoint.sh doctor-rejection N --pr P --head <reviewed-head>
   ```
   It counts only a rejection of the head a completed GLM Doctor produced, once (a
   replay prints `replay`). An initial Builder rejection, a stale/unrelated head, a
   Claude attempt, a launcher failure, no seat, or a rebase pass prints `not-counted`.
3. **Before each Doctor dispatch** ask for the route (`<cap>` is the effective
   `sweep.max_doctor_cycles`, including any granted grace cycle):
   ```bash
   ./.loom/scripts/sweep-checkpoint.sh doctor-route N --pr P --attempt <attempt> --cap <cap>
   ```
   - `route=preference` -> unchanged: `worker run --role doctor --pr P --json`.
   - `route=cap-exhausted` -> do NOT run another Doctor; block per the cap rule. K
     never extends the cap (with the default cap of 1 the cap stops first).
   - `route=claude reason=...` -> run `worker run --role doctor --pr P --escalate-claude
     "<reason>" --json` (it never launches or falls back to the GLM tap), then dispatch the
     Doctor as a Claude Task with the model from `sweep.escalation`
     (`ladder[min(attempt - 1, len - 1)]`; pins and a disabled ladder resolve as in the
     Execution Model, and neither may send the attempt back to GLM). A non-Claude
     `LOOM_RUNTIME_DOCTOR`/`LOOM_RUNTIME` pin makes `--escalate-claude` exit 78 with a
     diagnostic: park the PR per the cap rule and say so.
4. **Attribution**: the `doctor-done` write carries the real model; log the escalation
   reason with it (`PR #P Doctor escalated to Claude: <reason>`). No seat or exhausted
   Claude capacity takes the existing bounded wait/failure path; log the escalation
   reason, never return to the GLM tap. If the GLM pool is empty the preference already
   falls back to Claude by its own rules: step 1 then records a non-GLM attempt and
   nothing counts.

An instruction to "reuse the GLM tap once more" after `route=claude` is out of policy;
the latch holds for the PR until the checkpoint resets.
