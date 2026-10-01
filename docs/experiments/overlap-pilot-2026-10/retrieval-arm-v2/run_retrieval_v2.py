
import json, os, re, subprocess, sys, hashlib, datetime

DRIVER = "/Users/user1249/.loom/bin/auggie_driver.py"
REPO = "/Users/user1249/dev/loom"
OUT = "/tmp/opencode/pilot/predictions-v2"
STATE = "/tmp/opencode/pilot/auggie-states-v2"
os.makedirs(OUT, exist_ok=True); os.makedirs(STATE, exist_ok=True)
manifest = json.load(open("/tmp/opencode/pilot/manifest-retrieval.json"))

def query_plan(title, body):
    topic = " ".join(f"{title} {body}".split())
    return [f"{prefix}: {topic}" for prefix in (
        "implementation sites for", "callers and consumers of",
        "tests covering", "configuration and registration of")]

def sha256(s):
    import hashlib
    return hashlib.sha256(s.encode()).hexdigest()

for pair in manifest["pairs"]:
    rev, pid = pair["historical_commit"], pair["pair_id"]
    state = f"{STATE}/{pid}.json"
    if not os.path.exists(state):
        r = subprocess.run([DRIVER, "ensure-index", "--state", state,
                            "--git-repo", REPO, "--git-rev", rev, "--max-files", "3000"],
                           capture_output=True, text=True)
        if r.returncode != 0:
            print(f"IDX FAIL {pid}: {r.stderr[-200:]}", flush=True); continue
        print(f"idx {pid}: {r.stdout.strip()[:100]}", flush=True)
    for side, tag in ((0, "a"), (1, "b")):
        snap = pair["issues"][side]
        issue = snap["issue"]
        out_path = f"{OUT}/{issue}.json"
        if os.path.exists(out_path):
            continue
        files_map, order = {}, []
        for q in query_plan(snap["title"], snap["body"]):
            r = subprocess.run([DRIVER, "search", "--state", state, "--query", q],
                               capture_output=True, text=True)
            if r.returncode != 0:
                print(f"SRCH FAIL {issue}: {r.stderr[-150:]}", flush=True); continue
            try:
                payload = json.loads(r.stdout)
            except Exception:
                print(f"PARSE FAIL {issue}", flush=True); continue
            for c in payload.get("chunks", []):
                p = c["path"]
                if p not in files_map:
                    files_map[p] = {"path": p, "start": None, "end": None, "qs": set()}
                    order.append(p)
                e = files_map[p]; e["qs"].add(1)
                if c.get("start") is not None:
                    e["start"] = c["start"] if e["start"] is None else min(e["start"], c["start"])
                    e["end"] = c["end"] if e["end"] is None else max(e["end"], c["end"])
        curator = set()
        for m2 in re.finditer(r'(?im)^#{2,3}\s+Affected Files\s*$(.*?)(?=^#{2,3}\s|\Z)', snap["body"] or "", re.M | re.S):
            curator |= set(re.findall(r'`([^`\n]+)`', m2.group(1)))
        pred = {
            "artifact_version": 1, "issue": issue,
            "issue_content_hash": sha256(f"{snap['title']}\n{snap['body']}"),
            "source_revision": rev, "query_policy_version": "qp-v1",
            "provenance": {"provider": "augment", "model": "direct-context",
                           "index_version": f"auggie-state:{pid}"},
            "retrieved_at": datetime.datetime.now(datetime.timezone.utc).isoformat(),
            "status": {"kind": "present", "files": [
                {"path": f["path"],
                 "intervals": ([{"start": f["start"], "end": f["end"]}] if f["start"] is not None else []),
                 "symbols": [],
                 "intent": "edit" if f["path"] in curator else "context"} for f in files_map.values()]},
        }
        json.dump(pred, open(out_path, "w"), indent=1)
        print(f"issue {issue}: {len(files_map)} files", flush=True)
print("RETRIEVAL DONE")
