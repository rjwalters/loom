# Harmonization — threshold sweeps (v2 cohort, n=121)

## Base-vintage skew → substantive conflict

**Direction: HIGH skew predicts substantive conflict** (mechanical upstream drift).

| skew >= t | P | R | F1 | flag-rate | n |
|---|---|---|---|---|---|
| 0 | 0.55 | 1.00 | 0.71 | 100% | 121 |
| 3 | 0.56 | 1.00 | 0.72 | 98% | 121 |
| 10 | 0.60 | 0.97 | 0.74 | 90% | 121 |
| 25 | 0.64 | 0.87 | 0.74 | 74% | 121 |
| 40 | 0.67 | 0.78 | 0.72 | 64% | 121 |
| 60 | 0.66 | 0.64 | 0.65 | 54% | 121 |
| 100 | 0.60 | 0.45 | 0.51 | 41% | 121 |
| 150 | 0.58 | 0.33 | 0.42 | 31% | 121 |
| 0 | 0.18 | 1.00 | 0.31 | 100% | 121 |
| 3 | 0.18 | 1.00 | 0.31 | 98% | 121 |
| 10 | 0.17 | 0.86 | 0.29 | 90% | 121 |
| 25 | 0.17 | 0.68 | 0.27 | 74% | 121 |
| 40 | 0.17 | 0.59 | 0.26 | 64% | 121 |
| 60 | 0.14 | 0.41 | 0.21 | 54% | 121 |
| 100 | 0.06 | 0.14 | 0.08 | 41% | 121 |
| 150 | 0.05 | 0.09 | 0.07 | 31% | 121 |

(low-skew direction for reference — no signal, as expected):

| skew <= t | P | R | F1 | n |
|---|---|---|---|---|
| 0 | 0.00 | 0.00 | 0.00 | 121 |
| 25 | 0.28 | 0.13 | 0.18 | 121 |
| 150 | 0.54 | 0.67 | 0.60 | 121 |
| 0 | 0.00 | 0.00 | 0.00 | 121 |
| 25 | 0.22 | 0.32 | 0.26 | 121 |
| 150 | 0.24 | 0.91 | 0.38 | 121 |

## Curator baseline Jaccard → overlap event (both-baselined, n=24)

| J >= t | P | R | F1 | n |
|---|---|---|---|---|

## Harmonization recommendation (data-grounded)

1. **Labels first**: only *substantive* conflicts (non-hub files) count for
   scheduling. Mechanical-hub conflicts (`VERSION`, lockfiles,
   `install-metadata.json` — post-merge-automation churn) are reported
   separately; they fire on nearly every cross-vintage pair and are owned by
   the automation, not the dispatcher.
2. **Base-vintage is the dominant cheap signal**: substantive-conflict rate
   goes 0% (skew 0) → 29% (<=25) → ~72% (26–100). Dispatching waves from a
   common fresh base (or rebasing before parallel dispatch) is worth more
   than any file-overlap heuristic at this cohort size.
3. **Issue-vs-issue collisions are the rare kernel**: only
   substantive∩shared-own-file pairs (13/121 ≈ 11%) are direct collisions;
   these are what retrieval + intent features must catch, and what the
   conflict thresholds should be tuned on.
4. **Curator file-list Jaccard is not a usable threshold signal** at this
   cohort scale (1 overlap event among 24 both-baselined pairs). Its value is
   rank-ordering only; revisit with the scaled cohort.
5. **Retrieval features** (raw vs intent-filtered overlap) are evaluated on
   the retrieval subsample — see retrieval-arm section when present.
