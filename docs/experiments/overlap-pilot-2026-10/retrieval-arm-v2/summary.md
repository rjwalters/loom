# Overlap replay evaluation — rjwalters/loom (query policy `qp-v1`)

## Cohort

- pairs: 24 total, 24 leakage-controlled
- predictions: 48 issues present, 0 missing, 0 content/policy mismatch, 0 snapshots excluded (unreconstructable)
- pairs with both predictions: 24 (any prediction: 24)
- associations: 24 independent, 0 combined, 0 superseded, 0 ambiguous (non-independent pairs are evaluated separately from the primary cohort)

## Per-issue footprint (prediction vs actual PR changes)

- 48 issues scored: mean file precision 0.450, mean file recall 0.425
- mean interval precision 0.163, mean interval recall 0.613 (where comparable)

## Actual overlap distribution (outcome side, all scored pairs)

- n=24, min 0.000, p25 0.029, median 0.037, p75 0.067, max 0.158; pairs sharing ≥1 changed file: 22/24

## Conflict labels (counterfactual replay; observed recorded separately)

- textual conflict: 15, clean: 9, unknown: 0; observed production conflicts recorded in the manifest: 0

## Heuristics vs outcomes

| heuristic | Spearman vs actual file overlap (n) | AUC for conflict (n_pos/n_neg) |
|---|---|---|
| curator_baseline | n/a | n/a |
| file_jaccard | 0.906 (7) | 0.800 (5/2) |
| hub_weighted | 0.906 (7) | 0.800 (5/2) |
| line_overlap | 0.500 (3) | n/a |
| symbol_overlap | n/a | n/a |
| edit_edit | n/a | n/a |
| blend | 0.906 (7) | 0.800 (5/2) |

### Score-band outcome rates (held-out pairs; overlap event = shares ≥1 changed file)

- curator_baseline: low: n=0 rate=0.00 [0.00,0.00]; medium: n=0 rate=0.00 [0.00,0.00]; high: n=0 rate=0.00 [0.00,0.00]
- file_jaccard: low: n=6 rate=1.00 [0.61,1.00]; medium: n=1 rate=1.00 [0.21,1.00]; high: n=0 rate=0.00 [0.00,0.00]
- hub_weighted: low: n=6 rate=1.00 [0.61,1.00]; medium: n=1 rate=1.00 [0.21,1.00]; high: n=0 rate=0.00 [0.00,0.00]
- line_overlap: low: n=1 rate=1.00 [0.21,1.00]; medium: n=0 rate=0.00 [0.00,0.00]; high: n=2 rate=1.00 [0.34,1.00]
- symbol_overlap: low: n=0 rate=0.00 [0.00,0.00]; medium: n=0 rate=0.00 [0.00,0.00]; high: n=0 rate=0.00 [0.00,0.00]
- edit_edit: low: n=0 rate=0.00 [0.00,0.00]; medium: n=0 rate=0.00 [0.00,0.00]; high: n=0 rate=0.00 [0.00,0.00]
- blend: low: n=6 rate=1.00 [0.61,1.00]; medium: n=1 rate=1.00 [0.21,1.00]; high: n=0 rate=0.00 [0.00,0.00]

### Score-band conflict rates (held-out; separate event from overlap)

- curator_baseline: low: n=0 rate=0.00 [0.00,0.00]; medium: n=0 rate=0.00 [0.00,0.00]; high: n=0 rate=0.00 [0.00,0.00]
- file_jaccard: low: n=6 rate=0.67 [0.30,0.90]; medium: n=1 rate=1.00 [0.21,1.00]; high: n=0 rate=0.00 [0.00,0.00]
- hub_weighted: low: n=6 rate=0.67 [0.30,0.90]; medium: n=1 rate=1.00 [0.21,1.00]; high: n=0 rate=0.00 [0.00,0.00]
- line_overlap: low: n=1 rate=1.00 [0.21,1.00]; medium: n=0 rate=0.00 [0.00,0.00]; high: n=2 rate=1.00 [0.34,1.00]
- symbol_overlap: low: n=0 rate=0.00 [0.00,0.00]; medium: n=0 rate=0.00 [0.00,0.00]; high: n=0 rate=0.00 [0.00,0.00]
- edit_edit: low: n=0 rate=0.00 [0.00,0.00]; medium: n=0 rate=0.00 [0.00,0.00]; high: n=0 rate=0.00 [0.00,0.00]
- blend: low: n=6 rate=0.67 [0.30,0.90]; medium: n=1 rate=1.00 [0.21,1.00]; high: n=0 rate=0.00 [0.00,0.00]

## Split

- train: 17 pairs; held-out: 7 pairs. groups = connected components over shared issue/PR numbers, ordered by earliest cutoff; first 70% of groups train

## Recommendation

- selected mapping: **file_jaccard**
- Selected by TRAIN-split Spearman rank correlation against actual file overlap (selection frozen before held-out scoring; every published table reads held-out pairs only). A numeric score is a ranking estimate for the defined event, not a probability; bands are the published low/medium/high mapping with Wilson intervals. Small samples make this a provisional mapping — #9787 validates transfer prospectively.
- alternatives: file_jaccard (rho=0.270, n=17), hub_weighted (rho=0.270, n=17), blend (rho=0.270, n=17), curator_baseline (rho=0.000, n=1), edit_edit (rho=0.000, n=1), line_overlap (rho=-0.071, n=7), symbol_overlap (rho=-2.000, n=0)

## Limitations

- Line coordinates compare only on a common source basis; per-commit-parent derivations keep file/symbol metrics and mark line outcomes unknown
- Counterfactual replays merge the PRs' pinned head commits as-is: where a head contains upstream commits (base update), the replay folds them into that side — original-conflict endpoints are preferred in the manifest precisely to limit this
