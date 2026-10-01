# Archived one-shot Git oracle

This research command creates only isolated fixture repositories and an isolated
read-only source clone. It is not the production replay implementation. Run with
Python 3 and Git >=2.38 on AWS; see `PROTOCOL.md`. The directory argument must
contain `repo.git` (a bare Loom clone) and `manifest.json`.

Extract the following code block to `round3_oracle.py` outside a checkout, then
run `python3 round3_oracle.py <experiment-directory>`. Results and fixture bundles
are written under `<experiment-directory>/results`. A fresh directory is required
for a fresh run.

```python
"""One-shot research oracle; no scheduling actions or mutable production trees."""
import collections
import datetime as dt
import hashlib
import json
import os
from pathlib import Path
import platform
import subprocess
import sys
import time

ROOT = Path(sys.argv[1]).resolve()
REPO = ROOT / "repo.git"
INPUT = ROOT / "manifest.json"
OUT = ROOT / "results"
OUT.mkdir(exist_ok=True)
MANIFEST = json.loads(INPUT.read_text())


def cmd(args, cwd=None, timeout=120, data=None, env=None):
    p = subprocess.run(args, cwd=cwd, input=data, capture_output=True,
                       text=True, timeout=timeout, env=env)
    return p


def git(repo, *args, ok=True):
    p = cmd(["git", "-C", str(repo), *args])
    if ok and p.returncode:
        raise RuntimeError(f"git {args[0]} exited {p.returncode}: {p.stderr[:600]}")
    return p


def h(data):
    return hashlib.sha256(data.encode()).hexdigest()


def merge(repo, a, b, base=None):
    args = ["merge-tree", "--write-tree", "--messages"]
    if base is not None:
        args += ["--merge-base=" + base]
    args += [a, b]
    try:
        p = git(repo, *args, ok=False)
    except subprocess.TimeoutExpired:
        return {"label": "unknown", "reason": "timeout"}
    label = {0: "clean", 1: "textual_conflict"}.get(p.returncode, "unknown")
    paths = sorted({line.split("\t", 1)[1] for line in p.stdout.splitlines()
                    if "\t" in line and len(line.split("\t", 1)[0].split()) == 3})
    first = p.stdout.splitlines()[0] if p.stdout else None
    return {"label": label, "exit_code": p.returncode, "tree": first,
            "conflicted_paths": paths, "stdout": p.stdout, "stderr": p.stderr,
            "stdout_sha256": h(p.stdout), "stderr_sha256": h(p.stderr)}


def ancestor(repo, a, b):
    r = git(repo, "merge-base", "--is-ancestor", a, b, ok=False)
    return {0: True, 1: False}.get(r.returncode)


def fixture_repo(name):
    p = ROOT / "controls" / name
    p.mkdir(parents=True)
    git(p, "init", "-q", "-b", "main")
    git(p, "config", "user.name", "Loom experiment fixture")
    git(p, "config", "user.email", "fixture@invalid.example")
    git(p, "config", "commit.gpgSign", "false")
    return p


def commit(repo, files, name):
    for path, content in files.items():
        dest = repo / path
        dest.parent.mkdir(parents=True, exist_ok=True)
        dest.write_text(content)
    git(repo, "add", ".")
    git(repo, "commit", "-q", "-m", name)
    sha = git(repo, "rev-parse", "HEAD").stdout.strip()
    git(repo, "tag", name, sha)
    return sha


def fork(repo, sha, name):
    git(repo, "checkout", "-q", "-b", name, sha)


def control(name, repo, base, a, b, forced_base=None, semantics=None):
    native = [merge(repo, a, b), merge(repo, b, a)]
    forced = [merge(repo, a, b, forced_base or base),
              merge(repo, b, a, forced_base or base)]
    result = {"name": name, "true_base": base, "forced_base": forced_base or base,
              "heads": [a, b], "native": native, "forced": forced}
    if semantics:
        result["component_controls"] = semantics(repo, a, b, native)
    git(repo, "bundle", "create", str(OUT / (name + ".bundle")), "--all")
    return result


def controls():
    results = []
    p = fixture_repo("disjoint")
    c = commit(p, {"a.txt": "a0\n", "b.txt": "b0\n"}, "base")
    fork(p, c, "A")
    a = commit(p, {"a.txt": "a1\n"}, "A-edit")
    fork(p, c, "B")
    b = commit(p, {"b.txt": "b1\n"}, "B-edit")
    results.append(control("disjoint", p, c, a, b))

    p = fixture_repo("same-line")
    c = commit(p, {"shared.txt": "value=0\n"}, "base")
    fork(p, c, "A")
    a = commit(p, {"shared.txt": "value=1\n"}, "A-edit")
    fork(p, c, "B")
    b = commit(p, {"shared.txt": "value=2\n"}, "B-edit")
    results.append(control("same-line", p, c, a, b))

    p = fixture_repo("upstream-drift")
    c = commit(p, {"settings.txt": "value=0\n"}, "base")
    u1 = commit(p, {"settings.txt": "value=1\n"}, "upstream-1")
    fork(p, u1, "A")
    a = commit(p, {"a.txt": "only A owns this change\n"}, "A-edit")
    fork(p, u1, "later-upstream")
    u2 = commit(p, {"settings.txt": "value=2\n"}, "upstream-2")
    fork(p, c, "B")
    b = commit(p, {"b.txt": "only B owns this change\n"}, "B-edit")
    results.append(control("upstream-drift", p, c, a, b, forced_base=u2))

    p = fixture_repo("semantic-disjoint")
    check = ("from producer import BATCH\nfrom consumer import WORKERS\n"
             "assert BATCH * WORKERS <= 128, 'outstanding-item budget exceeded'\n")
    c = commit(p, {"producer.py": "BATCH = 16\n", "consumer.py": "WORKERS = 4\n",
                   "check.py": check}, "base")
    fork(p, c, "A")
    a = commit(p, {"producer.py": "BATCH = 32\n"}, "A-edit")
    fork(p, c, "B")
    b = commit(p, {"consumer.py": "WORKERS = 8\n"}, "B-edit")

    def semantic_checks(repo, a, b, native):
        out = []
        revs = [("base", c), ("A-alone", a), ("B-alone", b)]
        for i, n in enumerate(native):
            if n["label"] == "clean":
                revs.append((f"combined-orientation-{i}", n["tree"]))
        for label, rev in revs:
            dest = ROOT / "semantic-checks" / label
            dest.mkdir(parents=True)
            for path in ["producer.py", "consumer.py", "check.py"]:
                (dest / path).write_text(git(repo, "show", f"{rev}:{path}").stdout)
            p = cmd([sys.executable, "-B", "check.py"], cwd=dest)
            out.append({"component": label, "revision": rev, "exit_code": p.returncode,
                        "stdout": p.stdout, "stderr": p.stderr})
        return out

    results.append(control("semantic-disjoint", p, c, a, b, semantics=semantic_checks))
    assert all(x["label"] == "clean" for x in results[0]["native"])
    assert all(x["label"] == "textual_conflict" for x in results[1]["native"])
    assert all(x["label"] == "clean" for x in results[2]["native"])
    assert all(x["label"] == "clean" for x in results[3]["native"])
    components = results[3]["component_controls"]
    assert all(x["exit_code"] == 0 for x in components[:3])
    assert all(x["exit_code"] != 0 for x in components[3:])
    return results


def ensure_objects():
    missing = []
    for pair in MANIFEST["pairs"]:
        for pr in pair["prs"]:
            p = git(REPO, "cat-file", "-e", pr["head_sha"] + "^{commit}", ok=False)
            if p.returncode:
                missing.append(pr["pr"])
    for off in range(0, len(missing), 30):
        ns = sorted(set(missing[off:off+30]))
        args = ["git", "-C", str(REPO), "fetch", "--quiet", "origin"]
        args += [f"refs/pull/{n}/head:refs/experiment/pr-{n}" for n in ns]
        p = cmd(args, timeout=600)
        if p.returncode:
            print(f"fetch batch failed ({p.returncode}); missing objects will be unknown", flush=True)


def real_pairs():
    rows = []
    for i, pair in enumerate(MANIFEST["pairs"]):
        a, b = [p["head_sha"] for p in pair["prs"]]
        base = pair["historical_commit"]
        p = git(REPO, "merge-base", "--all", a, b, ok=False)
        bases = p.stdout.splitlines() if p.returncode == 0 else []
        rec = {"pair_id": pair["pair_id"], "declared_base": base,
               "heads": [a, b], "native_bases": bases,
               "declared_ancestor_a": ancestor(REPO, base, a),
               "declared_ancestor_b": ancestor(REPO, base, b),
               "a_ancestor_b": ancestor(REPO, a, b), "b_ancestor_a": ancestor(REPO, b, a),
               "native": [merge(REPO, a, b), merge(REPO, b, a)],
               "forced": [merge(REPO, a, b, base), merge(REPO, b, a, base)]}
        (OUT / (pair["pair_id"] + ".json")).write_text(json.dumps(rec, indent=2) + "\n")
        rows.append(rec)
        if (i+1) % 20 == 0:
            print(f"paired Git audit {i+1}/{len(MANIFEST['pairs'])}", flush=True)
    return rows


def pair_label(results):
    labels = [r["label"] for r in results]
    if "textual_conflict" in labels:
        return "textual_conflict"
    if "unknown" in labels:
        return "unknown"
    return "clean"


start = time.time()
ctrl = controls()
(OUT / "controls.json").write_text(json.dumps(ctrl, indent=2) + "\n")
ensure_objects()
rows = real_pairs()
counts = collections.Counter((pair_label(r["forced"]), pair_label(r["native"])) for r in rows)
disagreements = [r["pair_id"] for r in rows if pair_label(r["forced"]) != pair_label(r["native"])]
summary = {
    "protocol": "round3/PROTOCOL.md",
    "scope": "measurement_diagnostic_previously_inspected_sample",
    "started_at": dt.datetime.fromtimestamp(start, dt.timezone.utc).isoformat(),
    "finished_at": dt.datetime.now(dt.timezone.utc).isoformat(),
    "elapsed_seconds": round(time.time()-start, 2),
    "git_version": cmd(["git", "--version"]).stdout.strip(),
    "python_version": platform.python_version(), "machine": platform.machine(),
    "manifest_sha256": hashlib.sha256(INPUT.read_bytes()).hexdigest(),
    "driver_sha256": hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),
    "n": len(rows),
    "paired_table": [{"forced": a, "native": b, "n": n} for (a,b),n in sorted(counts.items())],
    "disagreement_pair_ids": disagreements,
    "head_containment_pairs": sum(r["a_ancestor_b"] is True or r["b_ancestor_a"] is True for r in rows),
    "declared_base_not_ancestor_of_both": sum(r["declared_ancestor_a"] is False or r["declared_ancestor_b"] is False for r in rows),
    "controls": [{"name": c["name"], "native": pair_label(c["native"]), "forced": pair_label(c["forced"])} for c in ctrl],
    "limitations": ["Native merges retain inherited history and do not identify independent-patch collision.",
                    "Previously inspected outcome-selected data cannot establish prospective accuracy.",
                    "Fixture semantics are constructed, not prevalence evidence."]
}
(OUT / "summary.json").write_text(json.dumps(summary, indent=2) + "\n")
files = sorted(p for p in OUT.iterdir() if p.is_file())
(OUT / "SHA256SUMS").write_text(''.join(hashlib.sha256(p.read_bytes()).hexdigest()+"  "+p.name+"\n" for p in files))
print(json.dumps(summary, indent=2))
```
