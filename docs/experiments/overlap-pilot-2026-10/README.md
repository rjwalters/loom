# Overlap pilot — first real run (2026-10-01)

Baseline-only execution of the #9785 historical replay on real
`rjwalters/loom` history: 12 leakage-controlled issue pairs, Curator-baseline
predictions, actual PR outcomes from git at pinned SHAs, counterfactual
conflict replay in both orders. **No retrieval predictions yet** — the
Augment arm lands when an `AUGMENT_API_TOKEN` is provisioned (#9783).

## Method (reproducible)

1. **Population** — merged PRs (400 most recent) linked to exactly one issue
   via `Closes #N`; issues joined by number.
2. **Pairing** — two issues implemented by *distinct* merged PRs, both
   created within 21 days of each other, both merged *after* the later
   creation (cutoff = later creation; both issues fully known at cutoff).
   8,474 candidate pairs; 92 shared the same PR base commit (exact
   common-source line comparability). `candidate-pairs.json` holds the full
   candidate set.
3. **Shortlist** — greedy issue-disjoint selection over same-base pairs,
   12 pairs / 24 issues.
4. **Leakage guard** — per issue, the forge timeline was queried for
   `edited` events: **all 24 issues have zero post-creation edits**, so the
   current body IS the as-of-cutoff snapshot (`reconstructed: true`,
   method recorded per snapshot).
5. **Curator baseline** — `## / ### Affected Files` sections parsed from
   issue bodies; where absent, from Curator comments that existed at the
   cutoff and were never edited. **12/24 issues have a baseline**; the other
   12 are `affected_files_known: false` — triage happened after the cutoff,
   so the baseline did not exist at prediction time and is honestly unknown,
   not empty. (This is the main n-limiting factor: only 3 pairs have both
   sides baselined.)
6. **Outcomes** — `loom-daemon overlap-replay outcomes` on a full clone
   (pinned base/head SHAs; PR `own_commits` from the forge API guard against
   base-update contamination; `git merge-tree --write-tree --merge-base=`
   conflict replay in both orders on the declared common source).
7. **Scoring** — `overlap-replay score` (baseline-only; missing predictions
   are a recorded stratum, never fabricated zeros).

Exact verbs (run on the AWS runner against `/root/loom`):

```
loom-daemon overlap-replay validate --manifest manifest.json --json
loom-daemon overlap-replay outcomes --manifest manifest.json --repo /root/loom --out-dir OUT
loom-daemon overlap-replay score  --manifest manifest.json --outcomes-dir OUT --out-dir REPORT
```

## Results (baseline-only, n=12 pairs)

- **Collisions are rare in this cohort**: 3/12 pairs shared ≥1 changed file
  (max file-Jaccard 0.091); **1/12 would have textually conflicted**
  (`p-8944-8787`, on `VERSION`/`Cargo.toml`/`Cargo.lock`/`package.json` —
  mechanical hub files, the exact shape the hub-weighted ablation exists to
  down-rank vs semantic collisions).
- **Curator baseline rank signal**: train-split Spearman ρ = 0.866 at
  **n = 3** (pairs with both sides baselined). The held-out tables are
  honestly empty — 3 pairs all fall in train. Suggestive, not conclusive.
- All merges replayed clean except the one conflict above; shared-file
  pairs merged cleanly (actual overlap ≠ actual conflict, as predicted by
  the two-label design).

## Limitations

- n = 12 pairs (3 scorable for the baseline). A smaller recoverable cohort
  is reported explicitly per #9785's acceptance criteria.
- Final-landed heads only: pre-integration-repair heads are not recoverable
  from the forge API retroactively, so original-conflict severity may be
  underestimated for the clean pairs.
- No retrieval predictions (credential gap) — the labeled-pair retrieval
  arm is still pending; #9787's prospective shadow study is the next
  evidence tier after scaling this cohort.
- Pairing window (21 days) and same-base preference are selection choices,
  recorded here; widening the window and dropping same-base (the
  contamination handling covers it) grows the cohort.

## Scaling checklist (next run)

1. Drop the same-base filter (own_commits + contamination flag handle it) —
   opens the full 8,474-pair candidate set.
2. Prefer pairs where both issues were *curated* before the cutoff (raises
   baseline-scoring n).
3. Widen to 400+ PRs of history and to the kicad-tools fleet repo for a
   second domain.
4. Provision `AUGMENT_API_TOKEN` (#9783) to switch on the retrieval arm and
   re-run the identical manifest — the frozen-prediction contract is
   already wired.
