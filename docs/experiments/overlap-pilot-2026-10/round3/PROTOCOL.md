# Research round 3: falsify the measurement before estimating a policy

Frozen before new oracle results; starting commit `3c66cf176`.

## Questions and claims permitted

1. Can the previous forced-base replay label a conflict when Git's actual
   recorded ancestry yields a clean merge? This is a measurement diagnostic,
   not an estimate of issue-pair collision prevalence.
2. Can a small Git control set distinguish true textual collisions, disjoint
   edits, upstream drift, and clean merges with semantic interaction?
3. How many old retrieval cases can be shown to satisfy pre-implementation
   input requirements? A count of recoverable cases is useful even if zero.

No new provider calls or fitted thresholds are needed for questions 1–2.
The existing 121-pair pool is already inspected; all reanalysis of it is
explicitly exploratory, never a new holdout.

## Primary diagnostic

For every resolvable pair in `v2-cohort/manifest-v2.json`, freeze its existing
head SHAs and compare:

- **Previous instrument:** `git merge-tree --write-tree --merge-base=<declared>
  <head-a> <head-b>`.
- **Native topology control:** `git merge-tree --write-tree <head-a> <head-b>`.

Run each with swapped sides, retain exit status, merge bases, conflicted paths,
and stdout/stderr hashes. Exit 0 = clean; 1 = textual conflict; other = unknown.
Record whether one head is ancestor of the other and whether the declared base
is an ancestor of both heads. A native clean merge is **not** proof that two
independently reconstructed patches would be clean. In particular, containment
can mean one implementation already landed in the other's history.

Primary result: the full paired confusion table between the two instruments,
including unknowns. Report the pre-specified ancestry strata and pair IDs for
every disagreement. Do not pick a new threshold from this analysis.

## Controlled cases

Create isolated fixture repositories on AWS and record all commit IDs:

1. Different files, independent branches: both instruments clean on the true base.
2. Different edits to the same line: native textual conflict in both orientations.
3. A branch adds only its own file after an upstream change; another branch
   forks before that upstream change and adds another file. The true merge
   base is clean; an inappropriate later forced base may invent a modify/delete
   or content conflict. Report the actual result, not an assumed expectation.
4. A producer and consumer change disjoint files: A-alone and B-alone pass a
   deterministic component check; combined code fails despite a clean merge.
   This establishes only the distinction between textual and semantic labels.

The fixture program and checks must be archived with results for rerunning.
Fixtures establish instrument behavior, not real-world effectiveness.

## Reproducibility and stopping

Use an isolated AWS research directory and Git >=2.38. Record Git/Python/OS
versions, source hashes, input-manifest checksum, run times, full per-pair
results, and missing objects. Do not modify a production checkout, rebase a
real branch, or change scheduling. A failed read is unknown, never clean.

Complete the finite 121-pair audit and controlled cases once. Publish both
positive and negative results. Separate what this resolves from the remaining
need for pre-dispatch snapshots, patch-preserving common-base reconstruction,
cheap-baseline comparisons, and a policy-level prospective holdout.
