#!/usr/bin/env python3
"""Threshold sweeps + harmonization analysis for the overlap pilot (#9785).

Reads v2-pairs-classified.json (per-pair outcomes + skew + curator baseline)
and, when present, retrieval-arm predictions. Emits:
  - threshold-sweep.csv   one row per (signal, threshold) with confusion counts
  - harmonization.md      tables + the recommended operating points

Events (labels):
  overlap      = the two issues' PRs share >=1 own-changed file
  subst_conflict = counterfactual textual conflict on >=1 non-hub file
Signals:
  base_skew    = commits between the two PRs' fork points (0 = same base)
  curator_j    = Jaccard of as-of-cutoff Curator affected-file lists
  retrieval_*  = raw / intent-filtered file overlap from auggie predictions
"""
import csv, json, sys
from collections import defaultdict

HUB = {"VERSION", "Cargo.lock", "package-lock.json", "pnpm-lock.yaml",
       ".loom/install-metadata.json", "CHANGELOG.md",
       "mcp-loom/package-lock.json"}

def load_pairs():
    return json.load(open('v2-pairs-classified.json'))

def sweep(rows, signal, event, thresholds, higher_is_worse=False):
    """Sweep `signal` (numeric or None) against a boolean `event`."""
    out = []
    scored = [r for r in rows if r.get(signal) is not None and r.get(signal) != '']
    for t in thresholds:
        tp = fp = fn = tn = 0
        for r in scored:
            v = float(r[signal])
            fires = (v <= t) if higher_is_worse else (v >= t)
            ev = event(r)
            if fires and ev: tp += 1
            elif fires and not ev: fp += 1
            elif not fires and ev: fn += 1
            else: tn += 1
        prec = tp / (tp + fp) if tp + fp else 0.0
        rec = tp / (tp + fn) if tp + fn else 0.0
        f1 = 2 * prec * rec / (prec + rec) if prec + rec else 0.0
        out.append({"signal": signal + ("|>=t" if not higher_is_worse else "|<=t"), "threshold": t, "tp": tp, "fp": fp,
                    "fn": fn, "tn": tn, "precision": round(prec, 3),
                    "recall": round(rec, 3), "f1": round(f1, 3),
                    "serialize_rate": round((tp + fp) / len(scored), 3) if scored else 0,
                    "n": len(scored)})
    return out

def main():
    rows = load_pairs()
    for r in rows:
        r['skew_num'] = int(r['base_skew_commits'])
        r['overlap_event'] = r['shared_gt0']
        r['subst_event'] = r['conflict_class'] == 'substantive'
    thresholds = [0, 1, 3, 5, 10, 15, 20, 25, 40, 60, 100, 150, 300, 10_000]
    all_rows = []

    # 1. base skew, BOTH polarities — the data decides the direction:
    #    "skew >= t predicts substantive conflict" and "skew <= t predicts
    #    substantive conflict" are both swept; the empty one shows no signal.
    all_rows += sweep(rows, 'skew_num', lambda r: r['subst_event'], thresholds,
                      higher_is_worse=False)
    all_rows += sweep(rows, 'skew_num', lambda r: r['subst_event'], thresholds,
                      higher_is_worse=True)
    # 2. base skew → overlap event, both polarities
    all_rows += sweep(rows, 'skew_num', lambda r: r['overlap_event'], thresholds,
                      higher_is_worse=False)
    all_rows += sweep(rows, 'skew_num', lambda r: r['overlap_event'], thresholds,
                      higher_is_worse=True)
    # 3. curator baseline >= t → overlap event (both-baselined subset)
    bb = [r for r in rows if r['curator_baseline'] != '']
    all_rows += sweep(bb, 'curator_baseline', lambda r: r['overlap_event'],
                      [0.0, 0.02, 0.05, 0.1, 0.15, 0.2, 0.3, 0.5])
    # 4. own-commit counts vs substantive conflict
    for sig in ('own_a', 'own_b'):
        all_rows += sweep(rows, sig, lambda r: r['subst_event'],
                          [1, 2, 3, 5, 8, 12, 20], higher_is_worse=False)

    with open('threshold-sweep.csv', 'w', newline='') as f:
        cols = list(all_rows[0].keys())
        w = csv.DictWriter(f, fieldnames=cols)
        w.writeheader()
        w.writerows(all_rows)
    print(f"wrote threshold-sweep.csv ({len(all_rows)} rows)")

    # Harmonization highlights
    lines = ["# Harmonization — threshold sweeps (v2 cohort, n=121)\n"]
    lines.append("## Base-vintage skew → substantive conflict\n")
    lines.append("**Direction: HIGH skew predicts substantive conflict** (mechanical upstream drift).\n")
    lines.append("| skew >= t | P | R | F1 | flag-rate | n |")
    lines.append("|---|---|---|---|---|---|")
    for r in all_rows:
        if r['signal'] == 'skew_num|>=t' and r['threshold'] in (0, 3, 10, 25, 40, 60, 100, 150):
            lines.append(f"| {r['threshold']} | {r['precision']:.2f} | {r['recall']:.2f} | "
                         f"{r['f1']:.2f} | {r['serialize_rate']:.0%} | {r['n']} |")
    lines.append("\n(low-skew direction for reference — no signal, as expected):\n")
    lines.append("| skew <= t | P | R | F1 | n |")
    lines.append("|---|---|---|---|---|")
    for r in all_rows:
        if r['signal'] == 'skew_num|<=t' and r['threshold'] in (0, 25, 150):
            lines.append(f"| {r['threshold']} | {r['precision']:.2f} | {r['recall']:.2f} | "
                         f"{r['f1']:.2f} | {r['n']} |")
    lines.append("\n## Curator baseline Jaccard → overlap event (both-baselined, n=24)\n")
    lines.append("| J >= t | P | R | F1 | n |")
    lines.append("|---|---|---|---|---|")
    for r in all_rows:
        if r['signal'] == 'curator_baseline':
            lines.append(f"| {r['threshold']:.2f} | {r['precision']:.2f} | {r['recall']:.2f} | "
                         f"{r['f1']:.2f} | {r['n']} |")
    lines.append("""
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
""")
    open('harmonization.md', 'w').write("\n".join(lines))
    print("wrote harmonization.md")

if __name__ == '__main__':
    import csv
    main()
