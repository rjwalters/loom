# Fail closed: prove success, don't assume it

The costliest failures in this project have not been loud crashes. They have been
**silent successes that produced nothing** — a fit campaign that ran for a day and
emitted only null cells, a launcher that returned exit 0 after starting no useful
work, a "stop" that left 15 of 16 boxes running, a stale binary that silently
accepted a manifest it didn't understand. Every one shares a single root:

> **Success was the default, and failure had to be noticed.** Failures degraded
> into silent nulls, warnings-not-errors, disabled observability, and bare exit-0s.
> There was no automated answer to the one question that matters — *did this
> actually produce the result it was supposed to?* — so it fell to a human digging
> through logs and JSONL by hand.

The rule that kills this anti-pattern is the inversion:

> **Fail closed. Success must be positively proven; anything unproven is a failure.**

## For an agent, concretely

- A step is **not done** until you have verified it produced the artifact it was
  supposed to. Absence, `null`, an empty result set, "0 rows", or a bare exit-0 is
  **not** success — treat it as failure until you have evidence otherwise.
- Before you report that a run succeeded, answer with evidence: *did it produce the
  non-null, advancing result?* If you can't show that, say it failed or is
  unverified — never round "it launched" up to "it worked".
- A launch (a detached job, a dispatched sweep, a background command) is
  **unverified** until you have seen its intended output appear. Launching is not
  finishing.
- Never let a failure degrade into a warning you scroll past. If a warning means the
  run is not doing its job (telemetry not recorded, mirror couldn't start, a box
  wasn't stopped, the binary is stale), treat it as the error it is and surface it
  loudly.

## For the code and the pipeline (the load-bearing part)

A doc is the floor, not the fix — an instruction is forgettable, a red check is not.
Push each guarantee as far down this list as it will go:

1. **Make the silent state impossible.** An empty/unidentified result is an explicit
   error or `UNSCORABLE(reason)`, never a bare null that flows into a "ship" verdict.
   Exit codes reflect reality: a run that produced zero valid results exits non-zero.
2. **Closed-loop acceptance checks.** Each stage declares its success criterion as a
   machine check and verifies it before reporting done (a launcher confirms its job
   is streaming valid output; a gate counts only non-null rows).
3. **Warnings that mean "broken" become errors.** Fail closed on a disabled mirror,
   missing ingest credentials, an unstoppable box, or a stale/underversioned binary.
4. **Observability is load-bearing and alerted.** Emit the facts that answer "did it
   work?" (null-rate, OOM, duration) and alert on them, so a bad run surfaces in
   minutes without a human. The *absence* of that telemetry is itself an alarm.
5. **Pin the invariant with a CI test.** Encode each rule as a test that fails before
   the guard exists and passes after, so a regression can't quietly reintroduce the
   silent path. See [`ci-principles`](ci-principles.md).

This is a design principle, not a process gate: apply judgement about how far down
the list a given guarantee needs to go, but never ship the version where a failure
looks like a success.
