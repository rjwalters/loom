# Post-hoc cheap-hybrid and influence checks

The frozen Augment/BM25 comparison has already been observed. This extra check
is explicitly exploratory: a stronger cheap baseline places literal-path hints
first, then fills remaining positions from BM25, deduplicating before taking ten.
It has no fitted weight or threshold. Also report every leave-one-case-out
Augment-minus-BM25 recall difference; do not drop the influential case from the
main result. No provider calls or new case selection occur here.

Extract and run `python3 round3_stress.py <experiment-directory>` on AWS.

```python
"""Exploratory robustness checks after the fixed baseline comparison."""
import collections
import hashlib
import json
from pathlib import Path
import random
import statistics
import sys

ROOT=Path(sys.argv[1])
cases=json.loads((ROOT/'evaluation/cases.json').read_text())
metrics=json.loads((ROOT/'evaluation/per-issue.json').read_text())
lookup={(r['issue'],r['method'],r['k']):r for r in metrics}
rows=[]
for c in cases:
    issue=c['issue']
    lexical=json.loads((ROOT/'acquisition'/str(issue)/'baselines.json').read_text())
    target=json.loads((ROOT/'evaluation'/f'target-{issue}.json').read_text())
    truth=set(target['net_changed_paths'])
    ranked=list(dict.fromkeys(lexical['literal_paths']+[x['path'] for x in lexical['bm25']]))
    pred=set(ranked[:10]);hits=len(pred&truth)
    augment=lookup[(issue,'augment',10)]
    row={'issue':issue,'source_revision':c['source_revision'],'hybrid_top_10':ranked[:10],
         'hybrid_hits':hits,'hybrid_recall_at_10':hits/len(truth),'hybrid_precision_at_10':hits/10,
         'augment_recall_at_10':augment['recall'],'augment_precision_at_10':augment['precision_at_k'],
         'augment_minus_hybrid_recall_at_10':augment['recall']-hits/len(truth),
         'augment_minus_bm25_recall_at_10':augment['recall']-lookup[(issue,'bm25',10)]['recall']}
    rows.append(row)
groups=collections.defaultdict(list)
for r in rows:groups[r['source_revision']].append(r['augment_minus_hybrid_recall_at_10'])
rng=random.Random(20261001);g=list(groups.values());boot=[]
for _ in range(5000):
    sample=[d for group in rng.choices(g,k=len(g)) for d in group]
    boot.append(statistics.mean(sample))
boot.sort()
leave_one_out=[{'omitted_issue':r['issue'],'mean_delta':statistics.mean(x['augment_minus_bm25_recall_at_10'] for x in rows if x['issue']!=r['issue'])} for r in rows]
result={'status':'post-hoc exploratory stress check, not a new primary endpoint',
        'hybrid_policy':'literal source paths first, then BM25 ranking; deduplicate; first 10; no tuned parameter',
        'n':len(rows),'source_groups':len(groups),
        'hybrid_macro_recall_at_10':statistics.mean(r['hybrid_recall_at_10'] for r in rows),
        'hybrid_macro_precision_at_10':statistics.mean(r['hybrid_precision_at_10'] for r in rows),
        'augment_minus_hybrid_mean_recall_at_10':statistics.mean(r['augment_minus_hybrid_recall_at_10'] for r in rows),
        'descriptive_source_group_bootstrap_95':[boot[124],boot[4874]],
        'wins':sum(r['augment_minus_hybrid_recall_at_10']>0 for r in rows),
        'ties':sum(r['augment_minus_hybrid_recall_at_10']==0 for r in rows),
        'losses':sum(r['augment_minus_hybrid_recall_at_10']<0 for r in rows),
        'leave_one_case_out_augment_minus_bm25':leave_one_out,
        'per_issue':rows,'driver_sha256':hashlib.sha256(Path(__file__).read_bytes()).hexdigest()}
(ROOT/'evaluation/stress.json').write_text(json.dumps(result,indent=2)+'\n')
print(json.dumps({k:v for k,v in result.items() if k not in ('per_issue','leave_one_case_out_augment_minus_bm25')},indent=2))
print('leave-one-out BM25 gain range',min(r['mean_delta'] for r in leave_one_out),max(r['mean_delta'] for r in leave_one_out))
```
