# ADR-0019: The `loom:curating` Label Claim Is Sufficient Serialization for the Dep-Recheck Post

## Status

**Accepted** — 2026-09-18, decided while implementing #8254 Gap 1 (the issue
asked for a recorded decision, not a specific implementation). Reversible: the
revisit trigger below says exactly what evidence would reopen it.

## Context

Curator's "Re-check Idempotency" flow (`defaults/roles/curator.md`, #4986/#7617)
decides whether a `loom:blocked` issue's Dependencies re-check is worth
commenting on, by comparing a `CONCLUSION_HASH` against the hash embedded in the
last re-check comment. The posting sequence is:

```
dep-recheck-fingerprint.sh dep-recheck   (read-only, unclaimed)
dep-recheck-fingerprint.sh decide        (read-only, unclaimed)
gh issue edit --add-label loom:curating  (claim)
gh issue comment ...                     (post)
gh issue edit --remove-label loom:curating
```

This flow replaced a downstream-local predecessor
(`premise-recheck.sh post`, 2AMLogic/2am#685, retired by 2am#876) whose `post`
verb did two things the current flow does not:

1. It acquired a **POSIX-atomic `mkdir` lock** keyed on
   (repo, issue, marker-prefix).
2. It **re-ran the decision fresh under that lock**, closing the window between
   a caller's own read and its write.

`loom:curating` is the mutual exclusion now. Adding a label is **not** a
compare-and-swap: two passes can both read "unclaimed" and both add the label,
and the forge returns success to both. The under-lock re-decide is gone
entirely.

2am#890 (ported here as #8254) asked for this to be decided deliberately rather
than left as an unexamined regression. This ADR is that decision.

## Decision

**Accept the `loom:curating` label claim as sufficient serialization for the
dep-recheck post. Do not add a CAS-shaped claim, and do not restore an
under-lock re-decide.**

The reasoning, in the order it actually carried weight:

**1. The failure the lock was built for was never a tight race.** The
duplicate-comment incidents that motivated it posted the *same* conclusion
1.17h, 1.75h, 7.79h and 10.15h apart (2am#298) and 15.6h apart (2am#557) — and,
in this repo, over weeks on #6335/#6805. Those gaps are three to four orders of
magnitude larger than the claim→post window, which is a handful of seconds. No
lock of any kind would have prevented them. The actual cause was **hash
non-determinism**: each pass computed a different `CONCLUSION_HASH` for an
unchanged blocking condition, so each pass correctly read "changed conclusion →
always comment". That cause has been removed in three steps —
#7281 (`UNKNOWN` mergeability fails safe instead of flickering), #7362 (the
label component narrowed to superseding-block labels only), and #8254 Gap 2,
this ADR's sibling change (free-text `--block-reason`/`--orthogonal`
canonicalized before hashing). The lock was treating a symptom whose cause is
now fixed.

**2. The label claim is stronger than the lock in the dimension that matters
here.** The `mkdir` lock was host-local and explicitly never reached across
hosts. Loom routinely runs several hosts against one repo, so the lock would
not have serialized the realistic concurrent case even in principle. A
forge-side label is visible to every host.

**3. The residual race is narrow, self-limiting, and cheap when it fires.** To
lose, two passes must interleave inside a seconds-wide window *and* both
compute the same hash. The consequence is exactly one duplicate re-check
comment — which the very next pass suppresses, because `PRIOR_HASH` now matches.
No state is corrupted, no verdict is wrong, no work is lost. The blast radius of
the residual race is strictly smaller than the blast radius of the bug the lock
was written for.

**4. A CAS claim is not available cheaply.** The forge's label API has no
conditional-add primitive, so a compare-and-swap would have to be *emulated* —
add, read back, compare, stand down — on the hot path of every Curator pass over
every `loom:blocked` issue. That is extra forge API calls in a system where
GraphQL quota exhaustion is a live operational failure mode (#5047, epic #4432),
spent to prevent one duplicate comment. The trade does not clear.

**5. One claim mechanism, repo-wide.** Every other role claims with a plain,
non-atomic label edit: Builder's `loom:building`, Judge's `loom:reviewing`,
Doctor's `loom:treating`, and Curator's own `loom:curating` for ordinary
curation (ADR-0006). Making the dep-recheck post — the *lowest*-consequence
action in the set — the one exception would introduce a second claim mechanism
where the risk is smallest, which is precisely backwards.

### Revisit trigger (falsifiable)

Reopen this decision if duplicate same-hash dep-recheck comments are ever
observed **less than about a minute apart**. That spacing is the signature of an
actual claim race, and is distinguishable from the hours-to-weeks-apart hash
churn of 2am#298/#557 and #6335/#6805, which this ADR asserts was the whole of
the observed failure. Absent that evidence, a CAS claim is speculative work.

## Consequences

### Positive

- No new forge API calls on a path that runs over every `loom:blocked` issue on
  every Curator pass.
- One claim mechanism across every role; nothing new to learn, test, or keep
  consistent.
- Cross-host coverage that the retired host-local `mkdir` lock never had.
- The non-atomicity is now *named* rather than tacit, with a falsifiable trigger
  for revisiting it.

### Negative

- A double-claim remains possible. Worst case is one duplicate re-check comment,
  self-suppressing on the next pass.
- There is no under-lock re-decide, so a conclusion that changes in the seconds
  between `decide` and the post is reported one pass late — the next pass sees a
  different hash and comments then.
- If the residual race ever does fire, the diagnosis will be harder than it
  would have been with a lock, because there is no lock-acquisition record to
  inspect. The revisit trigger above exists to make that detectable from comment
  timestamps alone.

## Alternatives Considered

**Restore a host-local `mkdir` lock.** Rejected: it does not serialize across
hosts, which is the only concurrency Loom's fleet actually has, and it would not
have prevented any of the incidents cited as motivation.

**Emulate CAS on the label (add → read back → compare → stand down).** Rejected
on cost: extra forge calls on a hot path, under a quota that is already an
operational constraint, to prevent a single duplicate comment. See point 4.

**Reuse the lease record (epic #6165: #6179 writes it, #6286 consumes it) as a
claim token.** Rejected on fit, not on maturity: the lease record is a
*liveness* signal — "a sweep on host H is still alive on this issue", renewed
by an idempotent PATCH of one comment and read as the last gate before
reclaiming a stale `loom:building` claim. It is scoped to a dispatched sweep,
not to a Curator pass over a `loom:blocked` issue, and writing one is itself a
plain unconditional write: it would give the dep-recheck post no atomicity it
does not already have, at the cost of a comment write plus a read on the same
hot path point 4 rejects.

**Leave it undecided.** Rejected — that is the state #8254 was filed to end.
An unexamined gap costs more than an accepted one, because the next reader has
to re-derive the evidence from scratch.

## References

- Related GitHub Issues: #8254 (this decision, Gap 1; Gap 2 is the sibling
  canonicalization fix), #7617 (claim-before-post discipline), #7281 and #7362
  (the determinism fixes that removed the real cause), #6165 / #6179 / #6286
  (the lease record and its reclamation consumer),
  #5047 / #4432 (forge quota pressure), #6335 / #6805 (the local churn
  incidents)
- Upstream report: 2AMLogic/2am#890, #876, #685, #298, #557
- Related ADRs: [ADR-0006](0006-label-based-workflow-coordination.md)
  (labels as the coordination state machine)
