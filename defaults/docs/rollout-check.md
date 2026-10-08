# Rollout Check

A merged PR that moves work between hosts can pass CI and review and still be wrong
on the live fleet (#10498 moved ETA authority to one host and nothing confirmed it
did what was intended). The rollout check makes that confirmation a tracked step.
It is **tracked, not automated**: no daemon feature runs it (automation is deferred
until the singleton watchdog, #10897 / #10898, lands).

## Which PRs

PRs that move work between hosts or change who emits a fleet signal: authority,
captain, singleton jobs, gating, capability routing. Other PRs carry no section.

## The section

The Builder adds `## Rollout check` to the PR body, naming:

- the production signal (a SigNoz query, metric, or daemon command), and
- the value expected after the fleet rolls.

"Verify in prod" is not a signal. The Judge requests changes if the section is
missing or vague.

## Who runs it, and where it is recorded

1. On approval the Judge copies the section onto the linked issue as a comment with
   an unchecked box (`- [ ] Rollout check: <signal> expects <value>`); for a
   `Part of #N` PR, onto #N.
2. After the next fleet roll (the merged version is deployed on every host), the
   Champion runs the signal, then ticks the box and comments the observed value.

## Escalation

If the observed value differs from expected, or the signal cannot be queried, the
Champion labels the issue `loom:operator` and comments the observed vs expected
values. A human decides whether to roll back or file a fix.
