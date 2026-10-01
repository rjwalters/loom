# Archived prediction-only acquisition command

Frozen before this round's new retrieval: Augment SDK 0.2.2, two queries per
issue, 20,000-character output cap per query, 600-second per-issue acquisition
deadline. The identical corpus is used by Augment and lexical baselines: all
regular UTF-8/non-NUL files at the pinned revision, up to 1 MiB per file and a
128 MiB aggregate hard refusal. There is no file-count truncation. Corpus
exclusions are saved. BM25 uses path plus content and distinct issue-query terms.

This standalone research command reads only `phase2/frozen-inputs.json`, the
pinned Git trees, and credentials from an external owner-only file. It never
reads PR outcomes. Credential-bearing files must not be published. It records
raw retrieval, latency, versions, membership validation, and missing costs.

Extract to `round3_acquire.py` outside a checkout; run with the SDK interpreter
`python round3_acquire.py <experiment-directory>`. Exact queries and corpus
manifests are persisted before provider search; outcomes are scored later.

```python
"""Prediction-only acquisition: inputs + pinned tree, never PR outcomes."""
import collections
import datetime as dt
import hashlib
import importlib.metadata
import json
import math
import os
from pathlib import Path
import re
import subprocess
import sys
import time

ROOT = Path(sys.argv[1]).resolve()
REPO = ROOT / "repo.git"
INPUTS = json.loads((ROOT / "phase2/frozen-inputs.json").read_text())
OUT = ROOT / "acquisition"
OUT.mkdir(exist_ok=True)
MAX_FILE = 1024 * 1024
MAX_CORPUS = 128 * 1024 * 1024


def checksum(data):
    return hashlib.sha256(data).hexdigest()


def write(path, value):
    path.write_text(json.dumps(value, indent=2, ensure_ascii=False) + "\n")


def git(*args, data=None):
    p = subprocess.run(["git", "-C", str(REPO), *args], input=data,
                       capture_output=True, timeout=120)
    if p.returncode:
        raise RuntimeError("pinned source Git read failed")
    return p.stdout


def corpus(rev):
    rows = git("ls-tree", "-r", "-z", "-l", rev).split(b"\0")
    chosen, excluded = [], []
    for row in rows:
        if not row: continue
        meta, rawpath = row.split(b"\t", 1)
        mode, typ, oid, size = meta.decode().split()
        path = rawpath.decode("utf-8", errors="strict")
        if mode not in ("100644", "100755") or typ != "blob":
            excluded.append({"path": path, "reason": "not_regular_blob", "mode": mode})
        elif int(size) > MAX_FILE:
            excluded.append({"path": path, "reason": "file_size_budget", "bytes": int(size)})
        else:
            chosen.append({"path": path, "oid": oid, "mode": mode, "bytes": int(size)})
    if sum(r["bytes"] for r in chosen) > MAX_CORPUS:
        raise RuntimeError("corpus exceeds frozen byte budget; no truncation applied")
    oids = sorted({r["oid"] for r in chosen})
    raw = git("cat-file", "--batch", data=('\n'.join(oids)+'\n').encode())
    blobs = {}
    pos = 0
    for oid in oids:
        end = raw.index(b"\n", pos)
        actual, typ, size = raw[pos:end].decode().split()
        if actual != oid or typ != "blob": raise RuntimeError("blob identity mismatch")
        size = int(size); start = end+1
        blobs[oid] = raw[start:start+size]
        pos = start+size+1
    texts, entries = {}, []
    for r in sorted(chosen, key=lambda x:x["path"]):
        data = blobs[r["oid"]]
        try:
            if b"\0" in data: raise UnicodeError("NUL")
            text = data.decode("utf-8", errors="strict")
        except UnicodeError:
            excluded.append({"path": r["path"], "reason": "non_utf8_or_binary", "oid": r["oid"]})
            continue
        texts[r["path"]] = text
        entries.append({**r, "sha256": checksum(data), "lines": len(text.splitlines())})
    record = {"revision": rev, "policy": "all regular UTF-8 non-NUL blobs <=1MiB; 128MiB aggregate hard refusal; no file-count cap",
              "files": entries, "excluded": sorted(excluded,key=lambda x:x["path"])}
    digest = checksum(json.dumps(record,sort_keys=True,ensure_ascii=False).encode())
    record["index_manifest_sha256"] = digest
    return texts, record


def tokens(text):
    return re.findall(r"[a-z0-9]+", text.lower())


def baselines(case, texts):
    q = case["title"] + "\n" + case["body"]
    hints = sorted((q.find(p),p) for p in texts if p in q)
    terms = set(tokens(q))
    vectors = {p:collections.Counter(tokens(p+'\n'+s)) for p,s in texts.items()}
    lengths = {p:sum(tf.values()) for p,tf in vectors.items()}
    avg = sum(lengths.values())/len(texts) if texts else 0
    df = collections.Counter(t for tf in vectors.values() for t in tf)
    scores = []
    for path, tf in vectors.items():
        score = 0.0
        for t in terms:
            f = tf[t]
            if not f: continue
            idf = math.log(1+(len(texts)-df[t]+0.5)/(df[t]+0.5))
            score += idf * f * 2.2 / (f + 1.2*(0.25+0.75*lengths[path]/avg))
        if score > 0: scores.append((path,score))
    scores.sort(key=lambda v:(-v[1],v[0]))
    return {"literal_paths": [p for _,p in hints],
            "bm25": [{"path":p,"score":s} for p,s in scores],
            "bm25_policy": {"k1":1.2,"b":0.75,"document":"path plus content","query":"title plus body; distinct terms",
                            "tokenizer":"lowercase ASCII alphanumeric tokens; underscore splits","stop_words":[],"ties":"path lexicographic"}}


def acquire(case):
    started = time.time()
    d = OUT / str(case["issue"])
    d.mkdir(exist_ok=True)
    texts, index = corpus(case["source_revision"])
    write(d/"index-manifest.json", index)
    write(d/"input.json",case)
    lexical = baselines(case,texts)
    write(d/"baselines.json",lexical)
    prefixes = ["Find implementation locations relevant to this issue.",
                "Find tests and callers relevant to this issue."]
    queries = [{"query":p+'\n\n'+case["title"]+'\n\n'+case["body"], "max_output_length":20000} for p in prefixes]
    write(d/"queries.json",queries)
    from auggie_sdk.context import DirectContext, File
    auth = json.loads((ROOT/".augment-auth.json").read_text())
    ctx = DirectContext.create(api_key=auth["api_key"],api_url=auth["api_url"],debug=False)
    index_start = time.time()
    result = ctx.add_to_index([File(path=p,contents=s) for p,s in texts.items()])
    if set(ctx.get_indexed_paths()) != set(texts):
        raise RuntimeError("provider index membership mismatch")
    index_elapsed = time.time()-index_start
    newly = result.newly_uploaded
    already = result.already_uploaded
    def count(v): return len(v) if isinstance(v,(list,tuple,set,dict)) else v
    responses = []
    ranked = []
    invalid = []
    for qi,q in enumerate(queries):
        begin = time.time()
        try:
            raw = ctx.search(q["query"],max_output_length=q["max_output_length"])
            if not isinstance(raw,str): raise TypeError("response not text")
            (d/f"response-{qi}.txt").write_text(raw)
            paths = re.findall(r"^Path: (.+)$",raw,re.M)
            valid = []
            for path in paths:
                path = path.strip()
                if path not in texts: invalid.append({"query":qi,"path":path});continue
                if path not in valid:valid.append(path)
                if path not in ranked:ranked.append(path)
            responses.append({"query_index":qi,"status":"complete" if len(raw)<q["max_output_length"] else "at_output_limit",
                              "latency_seconds":round(time.time()-begin,3),"bytes":len(raw.encode()),
                              "sha256":checksum(raw.encode()),"validated_paths":valid})
        except Exception as e:
            responses.append({"query_index":qi,"status":"unavailable","error_type":type(e).__name__,
                              "latency_seconds":round(time.time()-begin,3)})
    complete = all(x["status"]=="complete" for x in responses)
    record = {"issue":case["issue"],"status":"complete" if complete else "partial_or_unavailable",
              "source_revision":case["source_revision"],"index_manifest_sha256":index["index_manifest_sha256"],
              "sdk_version":importlib.metadata.version("auggie-sdk"),"provider_model_version":"not_disclosed",
              "provider":"Augment DirectContext","queries":responses,"ranked_paths":ranked,
              "invalid_paths":invalid,"started_at":dt.datetime.fromtimestamp(started,dt.timezone.utc).isoformat(),
              "completed_at":dt.datetime.now(dt.timezone.utc).isoformat(),
              "index_seconds":round(index_elapsed,3),"total_seconds":round(time.time()-started,3),
              "indexed_files":len(texts),"newly_uploaded_files":count(newly),"already_uploaded_files":count(already),
              "search_calls":len(responses),"total_provider_http_calls":None,"provider_cost_usd":None,
              "cost_note":"SDK exposes neither billed query cost nor HTTP retry count; unknown, not zero",
              "ranking":"first occurrence across two frozen queries; unranked union also retained"}
    write(d/"prediction.json",record)
    print(f"issue {case['issue']}: {record['status']}, {len(texts)} indexed, {len(ranked)} files returned",flush=True)


if len(sys.argv)==3:
    issue = int(sys.argv[2])
    case = next(c for c in INPUTS["cases"] if c["issue"]==issue)
    try:
        acquire(case)
    except Exception as e:
        d=OUT/str(issue);d.mkdir(exist_ok=True)
        write(d/"error.json",{"issue":issue,"status":"unavailable","error_type":type(e).__name__})
        print(f"issue {issue}: unavailable ({type(e).__name__}; provider/credential details suppressed)",flush=True)
        sys.exit(1)
else:
    statuses=[]
    for case in INPUTS["cases"]:
        issue=case["issue"]
        try:
            p=subprocess.run([sys.executable,__file__,str(ROOT),str(issue)],timeout=600)
            statuses.append({"issue":issue,"exit":p.returncode})
        except subprocess.TimeoutExpired:
            d=OUT/str(issue);d.mkdir(exist_ok=True)
            write(d/"error.json",{"issue":issue,"status":"unavailable","reason":"600-second acquisition deadline"})
            statuses.append({"issue":issue,"exit":"timeout"})
    write(OUT/"run.json",{"cases":statuses,"driver_sha256":checksum(Path(__file__).read_bytes()),
                         "frozen_inputs_sha256":checksum((ROOT/"phase2/frozen-inputs.json").read_bytes()),
                         "completed_at":dt.datetime.now(dt.timezone.utc).isoformat()})
    print("ACQUISITION COMPLETE",flush=True)
```
