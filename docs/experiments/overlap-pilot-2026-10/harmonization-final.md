# Harmonization — combined evidence, thresholds, and the recommended rule

> **Superseded by [DECISION.md](DECISION.md).** The audit withdrew this
> operating point: the source/input chronology and conflict attribution are
> invalid for deployment tuning. The OR rule flags 18 pairs (13 positives,
> 5 false positives), not 18 positive conflicts. “54/69 hub-only” was a
> misinterpretation of “no shared own-changed file.” Retained as experiment
> history; use the decision memo's corrections and validation plan.

Cohorts: **v2** (121 pairs, all-outcome baseline arm) and the **retrieval
subsample** (24 pairs enriched for collisions: all 13 substantive∩shared +
9 other overlap events + 2 controls). Retrieval features scored on the
subsample; skew/outcome labels on the full v2 set.

## 1. The label taxonomy that makes the data coherent

Raw `merge-tree` textual conflicts conflate three things:

| class | definition | v2 rate |
|---|---|---|
| **mechanical-hub** | conflict confined to `VERSION`, lockfiles, `CHANGELOG.md`, `.loom/install-metadata.json` — post-merge automation churn | 2/121 (but 54/121 under skew; the *files* follow the automation, not the issues) |
| **substantive** | conflict on ≥1 non-hub file | 67/121 |
| **clean** | both orders replay clean | 52/121 |

All stats below use **substantive** as the event. Mechanical-hub conflicts
are owned by the post-merge automation, not the dispatcher.

## 2. Signals measured

### Base-vintage skew (commits between the two PRs' fork points)

Substantive-conflict rate by skew bucket: 0% (skew 0) → 29% (1–25) →
71.8% (26–100) → 57.6% (101–300) → 64.7% (>300).

| rule (subsample n=24) | P | R | F1 | flag rate |
|---|---|---|---|---|
| skew ≥ 25 | 0.59 | 0.77 | 0.67 | 71% |
| skew ≥ 40 | 0.64 | 0.69 | 0.67 | 58% |

Full-cohort sweep (skew ≥ 25): P 0.64 / R 0.87 / F1 0.74.
Skew is a **drift proxy**: most of its conflicts are each-PR-vs-upstream,
not issue-vs-issue. That is why it fires so widely and why its FPs land on
mechanical-hub/clean pairs.

### Retrieval file overlap (auggie DirectContext, intent-agnostic)

| rule (subsample n=24) | P | R | F1 | flag rate |
|---|---|---|---|---|
| fj ≥ 0.01 (any shared retrieved file) | 0.80 | 0.62 | 0.70 | 42% |
| fj ≥ 0.1 (meaningful overlap) | 0.80 | 0.31 | 0.44 | 21% |

Per-issue footprint (n=48 issues): precision 0.450, recall 0.425, interval
precision 0.163 — retrieval is roughly half-right at file level, and the
misses audit shows retrieval scored **0.00 on 5/18 substantive-conflict
pairs** (it retrieved disjoint areas).

### Combined rules

| rule (subsample n=24) | P | R | F1 | flag rate |
|---|---|---|---|---|
| **fj ≥ 0.1 OR skew ≥ 40** | **0.72** | **1.00** | **0.84** | 75% |
| fj ≥ 0.01 AND skew ≥ 25 (high-precision) | 0.83 | 0.38 | 0.53 | 25% |

The OR rule caught **all 18 substantive conflicts** in the subsample:
retrieval covers the issue-vs-issue kernel (13), skew covers the
retrieval-blind drift conflicts (5). Its 5 FPs audited clean: 3 skew-drift
FPs (cosmetic — a wave that is merely re-based, which the dispatch loop
already does) and 1 retrieval FP (fj 0.375, merged clean — genuinely shared
files that did not collide).

## 3. The harmonized operating point (recommendation)

**Two-tier reporting, one enforcement rule:**

- **Tier 1 — rebase-and-proceed (cheap)**: flag any pair with skew ≥ 40.
  P 0.64. Action: rebase both branches onto a common fresh base before
  dispatch; most of these "conflicts" then dissolve (they are upstream
  drift, and rebase is already the loop's standard fix).
- **Tier 2 — serialize/combine (expensive)**: flag pairs with
  retrieval fj ≥ 0.1 **after** rebase. This is the true issue-vs-issue
  kernel; at the current n it is P 0.80 on subsample. Action: serialize the
  two candidates or have Curator combine them (#9781's choice).
- **Hub files**: exclude `VERSION`/lockfiles/`install-metadata.json`/
  `CHANGELOG.md` from Tier-2 signals entirely (still report them); they are
  automation-owned and would dominate naive overlap.
- **Report, don't enforce, until #9787**: the held-out pair-level n is 7;
  the numbers above are the *fitted* cohort. The rule is published as the
  recommended operating point for the shadow study to falsify prospectively.

## 4. What would change the thresholds

- Scaled cohort (drop same-base globally, prefer pre-cutoff-curated pairs)
  → tightens the skew operating point (the 25/40 boundary is noisy here).
- Intent-filtered retrieval features (#9784 footprint, edit∩edit overlap
  instead of raw file Jaccard) → expect Tier-2 precision to rise above 0.8;
  the raw feature mixes context reads with edits (per-issue precision 0.45).
- Symbol-level features require symbol evidence the current retrieval
  surface does not emit — still open.

## Reproduce

```
python3 harmonize.py              # sweeps → threshold-sweep.csv + tables
# retrieval scoring: overlap-replay score --manifest manifest-retrieval.json \
#   --outcomes-dir <v2-out> --predictions-dir predictions-v2 --out-dir ...
```
