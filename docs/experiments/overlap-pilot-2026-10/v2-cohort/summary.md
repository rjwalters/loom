# Overlap replay evaluation — rjwalters/loom (query policy `qp-v1`)

## Cohort

- pairs: 121 total, 121 leakage-controlled
- predictions: 0 issues present, 242 missing, 0 content/policy mismatch, 0 snapshots excluded (unreconstructable)
- pairs with both predictions: 0 (any prediction: 0)
- associations: 121 independent, 0 combined, 0 superseded, 0 ambiguous (non-independent pairs are evaluated separately from the primary cohort)

## Per-issue footprint (prediction vs actual PR changes)

_No issue has both a scored prediction and actual PR changes._

## Actual overlap distribution (outcome side, all scored pairs)

- n=121, min 0.000, p25 0.000, median 0.000, p75 0.000, max 0.158; pairs sharing ≥1 changed file: 22/121

## Conflict labels (counterfactual replay; observed recorded separately)

- textual conflict: 69, clean: 52, unknown: 0; observed production conflicts recorded in the manifest: 0

## Heuristics vs outcomes

| heuristic | Spearman vs actual file overlap (n) | AUC for conflict (n_pos/n_neg) |
|---|---|---|
| curator_baseline | 0.000 (3) | 0.500 (1/2) |
| file_jaccard | n/a | n/a |
| hub_weighted | n/a | n/a |
| line_overlap | n/a | n/a |
| symbol_overlap | n/a | n/a |
| edit_edit | n/a | n/a |
| blend | n/a | n/a |

### Score-band outcome rates (held-out pairs; overlap event = shares ≥1 changed file)

- curator_baseline: low: n=3 rate=0.00 [0.00,0.56]; medium: n=0 rate=0.00 [0.00,0.00]; high: n=0 rate=0.00 [0.00,0.00]
- file_jaccard: low: n=0 rate=0.00 [0.00,0.00]; medium: n=0 rate=0.00 [0.00,0.00]; high: n=0 rate=0.00 [0.00,0.00]
- hub_weighted: low: n=0 rate=0.00 [0.00,0.00]; medium: n=0 rate=0.00 [0.00,0.00]; high: n=0 rate=0.00 [0.00,0.00]
- line_overlap: low: n=0 rate=0.00 [0.00,0.00]; medium: n=0 rate=0.00 [0.00,0.00]; high: n=0 rate=0.00 [0.00,0.00]
- symbol_overlap: low: n=0 rate=0.00 [0.00,0.00]; medium: n=0 rate=0.00 [0.00,0.00]; high: n=0 rate=0.00 [0.00,0.00]
- edit_edit: low: n=0 rate=0.00 [0.00,0.00]; medium: n=0 rate=0.00 [0.00,0.00]; high: n=0 rate=0.00 [0.00,0.00]
- blend: low: n=0 rate=0.00 [0.00,0.00]; medium: n=0 rate=0.00 [0.00,0.00]; high: n=0 rate=0.00 [0.00,0.00]

### Score-band conflict rates (held-out; separate event from overlap)

- curator_baseline: low: n=3 rate=0.33 [0.06,0.79]; medium: n=0 rate=0.00 [0.00,0.00]; high: n=0 rate=0.00 [0.00,0.00]
- file_jaccard: low: n=0 rate=0.00 [0.00,0.00]; medium: n=0 rate=0.00 [0.00,0.00]; high: n=0 rate=0.00 [0.00,0.00]
- hub_weighted: low: n=0 rate=0.00 [0.00,0.00]; medium: n=0 rate=0.00 [0.00,0.00]; high: n=0 rate=0.00 [0.00,0.00]
- line_overlap: low: n=0 rate=0.00 [0.00,0.00]; medium: n=0 rate=0.00 [0.00,0.00]; high: n=0 rate=0.00 [0.00,0.00]
- symbol_overlap: low: n=0 rate=0.00 [0.00,0.00]; medium: n=0 rate=0.00 [0.00,0.00]; high: n=0 rate=0.00 [0.00,0.00]
- edit_edit: low: n=0 rate=0.00 [0.00,0.00]; medium: n=0 rate=0.00 [0.00,0.00]; high: n=0 rate=0.00 [0.00,0.00]
- blend: low: n=0 rate=0.00 [0.00,0.00]; medium: n=0 rate=0.00 [0.00,0.00]; high: n=0 rate=0.00 [0.00,0.00]

## Split

- train: 85 pairs; held-out: 36 pairs. groups = connected components over shared issue/PR numbers, ordered by earliest cutoff; first 70% of groups train

## Recommendation

- selected mapping: **curator_baseline**
- Selected by TRAIN-split Spearman rank correlation against actual file overlap (selection frozen before held-out scoring; every published table reads held-out pairs only). A numeric score is a ranking estimate for the defined event, not a probability; bands are the published low/medium/high mapping with Wilson intervals. Small samples make this a provisional mapping — #9787 validates transfer prospectively.
- alternatives: curator_baseline (rho=0.296, n=21), file_jaccard (rho=-2.000, n=0), hub_weighted (rho=-2.000, n=0), line_overlap (rho=-2.000, n=0), symbol_overlap (rho=-2.000, n=0), edit_edit (rho=-2.000, n=0), blend (rho=-2.000, n=0)

## Limitations

- 242 issue(s) had no frozen prediction — the labeled-pair cohort is limited to pairs with both predictions; retrieval-only pairs are reported separately
- Line coordinates compare only on a common source basis; per-commit-parent derivations keep file/symbol metrics and mark line outcomes unknown
- Counterfactual replays merge the PRs' pinned head commits as-is: where a head contains upstream commits (base update), the replay folds them into that side — original-conflict endpoints are preferred in the manifest precisely to limit this
