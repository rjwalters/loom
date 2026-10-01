# Archived independent set-metric evaluator

Run only after acquisition finishes. This reads targets from the separately
qualified own-commit chains; the predictor never reads this target file.
`EVALUATION.md` defines the frozen metrics and `BUDGET-NOTE.md` records the
finite-budget sensitivity addition before outcome scoring.

Extract the code to `round3_evaluate.py` outside a checkout and execute
`python3 round3_evaluate.py <experiment-directory>` on AWS. Six hand-enumerated
controls and hash/range invariants must pass before any metrics are written.

```python
"""Outcome evaluation after frozen acquisition; file sets only, no line claims."""
import collections
import csv
import hashlib
import json
import math
from pathlib import Path
import random
import statistics
import sys

ROOT=Path(sys.argv[1]).resolve()
OUT=ROOT/'evaluation'
OUT.mkdir(exist_ok=True)
inputs=json.loads((ROOT/'phase2/frozen-inputs.json').read_text())['cases']
patches=json.loads((ROOT/'phase2/patch-results.json').read_text())


def digest(b):return hashlib.sha256(b).hexdigest()
def save(p,v):p.write_text(json.dumps(v,indent=2,ensure_ascii=False)+'\n')


def measure(predicted,actual,k):
    pred=list(dict.fromkeys(predicted))[:k]
    truth=set(actual)
    hits=len(set(pred)&truth)
    return {'k':k,'hits':hits,'returned':len(pred),'actual':len(truth),
            'precision_at_k':hits/k,'precision_returned':hits/len(pred) if pred else None,
            'recall':hits/len(truth) if truth else None,
            'hit_paths':sorted(set(pred)&truth),'missed_paths':sorted(truth-set(pred)),
            'non_target_suggestions':sorted(set(pred)-truth)}


# Hand-enumerated external expectations, rather than expected values computed
# by a second copy of the formula. The short-result case distinguishes the two
# precision denominators explicitly.
checks=[
    ('exact',['a','b'],['a','b'],2,(2,2,1.0,1.0)),
    ('disjoint',['a'],['b'],1,(0,1,0.0,0.0)),
    ('deduplicated',['a','a','b'],['a','b'],2,(2,2,1.0,1.0)),
    ('short',['a'],['a','b'],5,(1,1,0.2,0.5)),
    ('empty',[],['a'],5,(0,0,0.0,0.0)),
    ('new-target',['old'],['old','new'],2,(1,1,0.5,0.5)),
]
for name,p,a,k,want in checks:
    m=measure(p,a,k)
    got=(m['hits'],m['returned'],m['precision_at_k'],m['recall'])
    assert got==want,(name,got,want)
    assert all(0<=m[x]<=1 for x in ('precision_at_k','recall'))
assert measure([],['a'],5)['precision_returned'] is None
assert measure(['a'],['a','b'],5)['precision_returned']==1
save(OUT/'metric-controls.json',{'checks_passed':[x[0] for x in checks],
                                'short_result_precision_returned':1,'short_result_precision_at_5':0.2,
                                'empty_result_precision_returned':None})

chains={}
pr_for_issue={}
for pair in patches:
    for pr,c in zip(pair['prs'],pair['chains']):
        chains[pr['issue']]=c
        pr_for_issue[pr['issue']]=pr

rows=[];cases=[]
for case in inputs:
    n=case['issue'];d=ROOT/'acquisition'/str(n)
    assert json.loads((d/'input.json').read_text())==case
    index=json.loads((d/'index-manifest.json').read_text())
    bare={k:v for k,v in index.items() if k!='index_manifest_sha256'}
    assert digest(json.dumps(bare,sort_keys=True,ensure_ascii=False).encode())==index['index_manifest_sha256']
    corpus={f['path'] for f in index['files']}
    baseline=json.loads((d/'baselines.json').read_text())
    c=chains[n]
    assert c['status']=='qualified_chain'
    truth=c['net_changed_paths']
    pred=json.loads((d/'prediction.json').read_text()) if (d/'prediction.json').exists() else None
    queries=json.loads((d/'queries.json').read_text())
    if pred:
        assert pred['source_revision']==case['source_revision']
        assert pred['index_manifest_sha256']==index['index_manifest_sha256']
        for q in pred['queries']:
            if 'sha256' in q:
                assert digest((d/f"response-{q['query_index']}.txt").read_bytes())==q['sha256']
    strict=bool(pred and pred['status']=='complete')
    budget=bool(pred and len(pred['queries'])==2 and all(q['status'] in ('complete','at_output_limit') for q in pred['queries']))
    methods={'literal':baseline['literal_paths'],'bm25':[r['path'] for r in baseline['bm25']]}
    if budget:methods['augment']=pred['ranked_paths']
    target={'issue':n,'pr':pr_for_issue[n]['pr'],'patch_base':c['patch_base'],'head':c['head'],
            'own_commits':c['own_commits'],'net_changed_paths':truth,
            'outside_corpus':sorted(set(truth)-corpus),'indexable_target_fraction':len(set(truth)&corpus)/len(truth)}
    save(OUT/f'target-{n}.json',target)
    cases.append({'issue':n,'source_revision':case['source_revision'],'strict_primary_available':strict,
                  'finite_budget_available':budget,'response_status':pred['status'] if pred else 'unavailable',
                  'actual_paths':len(truth),'indexable_targets':len(set(truth)&corpus),
                  'indexed_files':len(corpus),'index_exclusions':len(index['excluded']),
                  'returned_paths':len(pred['ranked_paths']) if pred else None,
                  'invalid_paths':len(pred['invalid_paths']) if pred else None,
                  'index_seconds':pred['index_seconds'] if pred else None,
                  'search_seconds':sum(q['latency_seconds'] for q in pred['queries']) if pred else None})
    for method,ranking in methods.items():
        for k in (5,10,20):
            m=measure(ranking,truth,k)
            for f in ('precision_at_k','precision_returned','recall'):
                assert m[f] is None or 0<=m[f]<=1,(n,method,k,f,m[f])
            rows.append({'issue':n,'source_revision':case['source_revision'],'method':method,
                         'strict_primary_available':strict,'finite_budget_available':budget,**m})
    expected_hits=10*len(set(truth)&corpus)/len(corpus)
    cases[-1]['uniform_random_expected_recall_at_10']=expected_hits/len(truth)
    if budget:
        all_returned=measure(pred['ranked_paths'],truth,max(1,len(pred['ranked_paths'])))
        cases[-1]['augment_all_returned_recall']=all_returned['recall']
        cases[-1]['augment_all_returned_precision']=all_returned['precision_returned']


def summary(population):
    available={c['issue'] for c in cases if c[population]}
    r=[x for x in rows if x['issue'] in available]
    result={}
    for method in ('literal','bm25','augment'):
        result[method]={}
        for k in (5,10,20):
            s=[x for x in r if x['method']==method and x['k']==k]
            if not s:result[method][str(k)]=None;continue
            result[method][str(k)]={'n':len(s),'macro_recall':statistics.mean(x['recall'] for x in s),
                'macro_precision_at_k':statistics.mean(x['precision_at_k'] for x in s),
                'macro_precision_returned':statistics.mean(x['precision_returned'] for x in s if x['precision_returned'] is not None) if any(x['precision_returned'] is not None for x in s) else None,
                'empty_rankings':sum(x['returned']==0 for x in s),
                'micro_recall':sum(x['hits'] for x in s)/sum(x['actual'] for x in s)}
    lookup={(x['issue'],x['method']):x for x in r if x['k']==10}
    deltas=[]
    for n in sorted(available):
        for baseline in ('bm25','literal'):
            a=lookup.get((n,'augment'));b=lookup.get((n,baseline))
            if a and b:deltas.append({'issue':n,'source_revision':a['source_revision'],'baseline':baseline,
                                     'delta_recall_at_10':a['recall']-b['recall']})
    comparisons={}
    for baseline in ('bm25','literal'):
        ds=[d for d in deltas if d['baseline']==baseline]
        if not ds:continue
        groups=collections.defaultdict(list)
        for d in ds:groups[d['source_revision']].append(d['delta_recall_at_10'])
        ci=None
        if len(groups)>1:
            g=list(groups.values());rng=random.Random(20261001);boot=[]
            for _ in range(5000):
                sample=[v for cluster in rng.choices(g,k=len(g)) for v in cluster]
                boot.append(statistics.mean(sample))
            boot.sort();ci=[boot[124],boot[4874]]
        comparisons[baseline]={'n':len(ds),'source_groups':len(groups),'mean_delta_recall_at_10':statistics.mean(d['delta_recall_at_10'] for d in ds),
            'descriptive_bootstrap_95':ci,'wins':sum(d['delta_recall_at_10']>0 for d in ds),
            'ties':sum(d['delta_recall_at_10']==0 for d in ds),'losses':sum(d['delta_recall_at_10']<0 for d in ds)}
    return {'n_cases':len(available),'issue_ids':sorted(available),'metrics':result,'paired_comparisons':comparisons,'per_issue_deltas':deltas}


save(OUT/'per-issue.json',rows)
save(OUT/'cases.json',cases)
cols=['issue','method','k','hits','returned','actual','precision_at_k','precision_returned','recall','strict_primary_available','finite_budget_available']
with (OUT/'per-issue.csv').open('w',newline='') as f:
    w=csv.DictWriter(f,fieldnames=cols,extrasaction='ignore');w.writeheader();w.writerows(rows)
result={'strict_primary':summary('strict_primary_available'),
        'finite_budget_sensitivity':summary('finite_budget_available'),
        'uniform_random_macro_expected_recall_at_10':statistics.mean(c['uniform_random_expected_recall_at_10'] for c in cases),
        'cost_usd':None,'cost_status':'not exposed by SDK; latency and call count retained',
        'frozen_inputs_sha256':digest((ROOT/'phase2/frozen-inputs.json').read_bytes()),
        'driver_sha256':digest(Path(__file__).read_bytes()),
        'limitations':['Sensitivity inclusion added after observing output-budget flags, before target scoring.',
                       '12 retrospective issue cases are not a scheduling-risk validation cohort.',
                       'Bootstrap grouped by source SHA only; dependence may remain.',
                       'Title/body only; comments and proposed new-file inference are omitted.']}
save(OUT/'summary.json',result)
print(json.dumps({k:v for k,v in result.items() if k not in ('strict_primary','finite_budget_sensitivity')},indent=2))
for pop in ('strict_primary','finite_budget_sensitivity'):
    print(pop,'n=',result[pop]['n_cases'])
    print('metrics at 10',json.dumps({m:v['10'] for m,v in result[pop]['metrics'].items()},indent=2))
    print('paired',json.dumps(result[pop]['paired_comparisons'],indent=2))
```
