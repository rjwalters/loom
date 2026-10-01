# Overlap replay evaluation — rjwalters/loom (query policy `qp-v1`)

## Cohort

- pairs: 12 total, 12 leakage-controlled
- predictions: 24 issues present, 0 missing, 0 content/policy mismatch, 0 snapshots excluded (unreconstructable)
- pairs with both predictions: 12 (any prediction: 12)
- associations: 12 independent, 0 combined, 0 superseded, 0 ambiguous (non-independent pairs are evaluated separately from the primary cohort)

## Per-issue footprint (prediction vs actual PR changes)

- 24 issues scored: mean file precision 0.263, mean file recall 0.489
- mean interval precision 0.084, mean interval recall 0.540 (where comparable)

## Actual overlap distribution (outcome side, all scored pairs)

- n=12, min 0.000, p25 0.000, median 0.000, p75 0.000, max 0.091; pairs sharing ≥1 changed file: 3/12

## Conflict labels (counterfactual replay; observed recorded separately)

- textual conflict: 1, clean: 11, unknown: 0; observed production conflicts recorded in the manifest: 0

## Heuristics vs outcomes

| heuristic | Spearman vs actual file overlap (n) | AUC for conflict (n_pos/n_neg) |
|---|---|---|
| curator_baseline | n/a | n/a |
| file_jaccard | 0.000 (3) | 0.500 (1/2) |
| hub_weighted | 0.000 (3) | 0.500 (1/2) |
| line_overlap | n/a | n/a |
| symbol_overlap | n/a | n/a |
| edit_edit | n/a | n/a |
| blend | 0.000 (3) | 0.500 (1/2) |

### Score-band outcome rates (held-out pairs; overlap event = shares ≥1 changed file)

- curator_baseline: low: n=0 rate=0.00 [0.00,0.00]; medium: n=0 rate=0.00 [0.00,0.00]; high: n=0 rate=0.00 [0.00,0.00]
- file_jaccard: low: n=3 rate=0.00 [0.00,0.56]; medium: n=0 rate=0.00 [0.00,0.00]; high: n=0 rate=0.00 [0.00,0.00]
- hub_weighted: low: n=3 rate=0.00 [0.00,0.56]; medium: n=0 rate=0.00 [0.00,0.00]; high: n=0 rate=0.00 [0.00,0.00]
- line_overlap: low: n=0 rate=0.00 [0.00,0.00]; medium: n=0 rate=0.00 [0.00,0.00]; high: n=0 rate=0.00 [0.00,0.00]
- symbol_overlap: low: n=0 rate=0.00 [0.00,0.00]; medium: n=0 rate=0.00 [0.00,0.00]; high: n=0 rate=0.00 [0.00,0.00]
- edit_edit: low: n=0 rate=0.00 [0.00,0.00]; medium: n=0 rate=0.00 [0.00,0.00]; high: n=0 rate=0.00 [0.00,0.00]
- blend: low: n=3 rate=0.00 [0.00,0.56]; medium: n=0 rate=0.00 [0.00,0.00]; high: n=0 rate=0.00 [0.00,0.00]

### Score-band conflict rates (held-out; separate event from overlap)

- curator_baseline: low: n=0 rate=0.00 [0.00,0.00]; medium: n=0 rate=0.00 [0.00,0.00]; high: n=0 rate=0.00 [0.00,0.00]
- file_jaccard: low: n=3 rate=0.33 [0.06,0.79]; medium: n=0 rate=0.00 [0.00,0.00]; high: n=0 rate=0.00 [0.00,0.00]
- hub_weighted: low: n=3 rate=0.33 [0.06,0.79]; medium: n=0 rate=0.00 [0.00,0.00]; high: n=0 rate=0.00 [0.00,0.00]
- line_overlap: low: n=0 rate=0.00 [0.00,0.00]; medium: n=0 rate=0.00 [0.00,0.00]; high: n=0 rate=0.00 [0.00,0.00]
- symbol_overlap: low: n=0 rate=0.00 [0.00,0.00]; medium: n=0 rate=0.00 [0.00,0.00]; high: n=0 rate=0.00 [0.00,0.00]
- edit_edit: low: n=0 rate=0.00 [0.00,0.00]; medium: n=0 rate=0.00 [0.00,0.00]; high: n=0 rate=0.00 [0.00,0.00]
- blend: low: n=3 rate=0.33 [0.06,0.79]; medium: n=0 rate=0.00 [0.00,0.00]; high: n=0 rate=0.00 [0.00,0.00]

## Split

- train: 9 pairs; held-out: 3 pairs. groups = connected components over shared issue/PR numbers, ordered by earliest cutoff; first 70% of groups train

## Recommendation

- selected mapping: **curator_baseline**
- Selected by TRAIN-split Spearman rank correlation against actual file overlap (selection frozen before held-out scoring; every published table reads held-out pairs only). A numeric score is a ranking estimate for the defined event, not a probability; bands are the published low/medium/high mapping with Wilson intervals. Small samples make this a provisional mapping — #9787 validates transfer prospectively.
- alternatives: curator_baseline (rho=0.866, n=3), edit_edit (rho=0.866, n=3), file_jaccard (rho=0.476, n=9), hub_weighted (rho=0.476, n=9), blend (rho=0.476, n=9), line_overlap (rho=-0.500, n=3), symbol_overlap (rho=-2.000, n=0)

## Limitations

- Line coordinates compare only on a common source basis; per-commit-parent derivations keep file/symbol metrics and mark line outcomes unknown
- Counterfactual replays merge the PRs' pinned head commits as-is: where a head contains upstream commits (base update), the replay folds them into that side — original-conflict endpoints are preferred in the manifest precisely to limit this
