# Archived patch-isolation and qualification command

Run on the isolated AWS directory from `ORACLE.md` after adding authoritative
`issues.json` (GraphQL number/title/body/createdAt/lastEditedAt, with observation
time). This diagnostic writes only private index files and unreachable synthetic
objects in the isolated clone. It does not modify real branches.

Extract the code to `round3_patch_study.py` outside a checkout and run
`python3 round3_patch_study.py <experiment-directory>`. See `PHASE2.md` for the
frozen endpoint, eligibility, limitations, and selection rule.

```python
"""Strict patch transplant and temporal qualification; no fitted policy."""
import collections
import datetime as dt
import hashlib
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile

ROOT = Path(sys.argv[1]).resolve()
REPO = ROOT / "repo.git"
OUT = ROOT / "phase2"
OUT.mkdir(exist_ok=True)
MANIFEST = json.loads((ROOT / "manifest.json").read_text())
METADATA = json.loads((ROOT / "issues.json").read_text())
parse = lambda s: dt.datetime.fromisoformat(s.replace("Z", "+00:00"))


def git(*args, data=None, env=None, ok=True):
    p = subprocess.run(["git", "--literal-pathspecs", "-C", str(REPO), *args],
                       input=data, capture_output=True, text=True, timeout=120, env=env)
    if ok and p.returncode:
        raise RuntimeError(f"git {args[0]} ({p.returncode}): {p.stderr[:400]}")
    return p


def sha(s):
    return hashlib.sha256(s.encode()).hexdigest()


def ancestry(a, b):
    return {0: True, 1: False}.get(git("merge-base", "--is-ancestor", a, b, ok=False).returncode)


def chain(pr):
    own = pr.get("own_commits") or []
    if not own or own[-1] != pr["head_sha"]:
        return {"status": "unknown", "reason": "head_not_end_of_own_chain"}
    parent = None
    dates = []
    for i, c in enumerate(own):
        p = git("show", "-s", "--format=%P%n%aI%n%cI", c, ok=False)
        lines = p.stdout.splitlines()
        if p.returncode or len(lines) != 3:
            return {"status": "unknown", "reason": "commit_metadata_unavailable"}
        parents = lines[0].split()
        if len(parents) != 1:
            return {"status": "unknown", "reason": "nonlinear_own_commit"}
        if i and parents[0] != own[i-1]:
            return {"status": "unknown", "reason": "own_commits_not_contiguous"}
        if not i:
            parent = parents[0]
        dates.append({"commit": c, "author": lines[1], "committer": lines[2]})
    files = git("diff", "--no-ext-diff", "--no-textconv", "--no-renames",
                "--name-only", "-z", parent, pr["head_sha"]).stdout.split("\0")
    files = sorted(f for f in files if f)
    if not files:
        return {"status": "unknown", "reason": "empty_net_patch"}
    return {"status": "qualified_chain", "patch_base": parent, "head": pr["head_sha"],
            "own_commits": own, "commit_dates": dates, "net_changed_paths": files}


entry_cache = {}
def entry(rev, path):
    key = (rev, path)
    if key not in entry_cache:
        text = git("ls-tree", "-z", rev, "--", path).stdout
        entry_cache[key] = text.split("\t", 1)[0] if text else None
    return entry_cache[key]


def transplant(c, common, name):
    mismatches = [p for p in c["net_changed_paths"]
                  if entry(common, p) != entry(c["patch_base"], p)]
    if mismatches:
        return {"status": "unknown", "reason": "preimage_differs_at_common_base",
                "paths": mismatches}
    patch = git("diff", "--binary", "--full-index", "--no-ext-diff", "--no-textconv",
                "--no-renames", c["patch_base"], c["head"]).stdout
    with tempfile.TemporaryDirectory(prefix="private-index-", dir=ROOT) as t:
        env = {**os.environ, "GIT_INDEX_FILE": str(Path(t) / "index")}
        git("read-tree", common, env=env)
        p = git("apply", "--cached", "--whitespace=nowarn", "-", data=patch, env=env, ok=False)
        if p.returncode:
            return {"status": "unknown", "reason": "patch_application_failed",
                    "stderr": p.stderr, "patch_sha256": sha(patch)}
        tree = git("write-tree", env=env).stdout.strip()
    bad_post = [p for p in c["net_changed_paths"] if entry(tree, p) != entry(c["head"], p)]
    if bad_post:
        return {"status": "unknown", "reason": "postimage_mismatch", "paths": bad_post}
    env = {**os.environ,
           "GIT_AUTHOR_NAME": "Loom research reconstruction", "GIT_COMMITTER_NAME": "Loom research reconstruction",
           "GIT_AUTHOR_EMAIL": "fixture@invalid.example", "GIT_COMMITTER_EMAIL": "fixture@invalid.example",
           "GIT_AUTHOR_DATE": "2026-10-01T00:00:00Z", "GIT_COMMITTER_DATE": "2026-10-01T00:00:00Z"}
    synthetic = git("commit-tree", tree, "-p", common, "-m", name, env=env).stdout.strip()
    return {"status": "reconstructed", "tree": tree, "commit": synthetic,
            "patch_sha256": sha(patch), "preimages_exact": True, "postimages_exact": True}


def merge(a, b):
    p = git("merge-tree", "--write-tree", "--messages", a, b, ok=False)
    return {"label": {0: "clean", 1: "textual_conflict"}.get(p.returncode, "unknown"),
            "exit_code": p.returncode, "stdout": p.stdout, "stderr": p.stderr}


chains = {}
records = []
for i, pair in enumerate(MANIFEST["pairs"]):
    sides = []
    for pr in pair["prs"]:
        if pr["pr"] not in chains:
            chains[pr["pr"]] = chain(pr)
        sides.append(chains[pr["pr"]])
    r = {"pair_id": pair["pair_id"], "prs": pair["prs"], "chains": sides,
         "label": "unknown"}
    if any(s["status"] != "qualified_chain" for s in sides):
        r["reason"] = "one_or_both_own_chains_unqualified"
    else:
        bases = git("merge-base", "--all", sides[0]["patch_base"], sides[1]["patch_base"]).stdout.splitlines()
        if len(bases) != 1:
            r["reason"] = "no_unique_common_base"
        else:
            r["common_base"] = bases[0]
            tx = [transplant(s, bases[0], pair["pair_id"] + f" side-{i}") for i,s in enumerate(sides)]
            r["reconstructions"] = tx
            if any(x["status"] != "reconstructed" for x in tx):
                r["reason"] = "one_or_both_patches_not_exactly_reconstructable"
            else:
                r["merge_orientations"] = [merge(tx[0]["commit"], tx[1]["commit"]),
                                           merge(tx[1]["commit"], tx[0]["commit"])]
                labels = [v["label"] for v in r["merge_orientations"]]
                r["label"] = "textual_conflict" if "textual_conflict" in labels else ("unknown" if "unknown" in labels else "clean")
    records.append(r)
    if (i+1) % 30 == 0: print(f"patch isolation {i+1}/{len(MANIFEST['pairs'])}", flush=True)

(OUT / "patch-results.json").write_text(json.dumps(records, indent=2) + "\n")
qualification = []
eligible_inputs = []
for pair in MANIFEST["pairs"]:
    for snap in pair["issues"]:
        pr = next(p for p in pair["prs"] if p["issue"] == snap["issue"])
        now = METADATA["issues"].get(str(snap["issue"]))
        c = chains[pr["pr"]]
        reasons = []
        if not now: reasons.append("authoritative_issue_unavailable")
        elif now["lastEditedAt"] is not None: reasons.append("issue_edited")
        if now and (now["title"] != snap["title"] or now["body"] != snap["body"]):
            reasons.append("saved_content_does_not_equal_authoritative")
        if c["status"] != "qualified_chain": reasons.append(c["reason"])
        source = None
        if not reasons:
            cutoff = parse(now["createdAt"])
            if any(parse(d[k]) <= cutoff for d in c["commit_dates"] for k in ("author", "committer")):
                reasons.append("implementation_timestamp_not_after_issue_creation")
            source = git("rev-list", "--first-parent", "-1", "--before="+now["createdAt"], "main").stdout.strip()
            dates = git("show", "-s", "--format=%aI%n%cI", source).stdout.splitlines()
            if any(parse(d)>cutoff for d in dates): reasons.append("source_timestamp_after_cutoff")
            if ancestry(source, c["patch_base"]) is not True: reasons.append("source_not_ancestor_of_patch_base")
            if ancestry(c["head"], source) is not False: reasons.append("implementation_already_in_source_history")
        qualification.append({"issue": snap["issue"], "pr": pr["pr"], "status": "excluded" if reasons else "qualified",
                              "reasons": reasons, "source_revision": source,
                              "lastEditedAt": now and now["lastEditedAt"], "createdAt": now and now["createdAt"]})
        if not reasons:
            eligible_inputs.append({"issue": snap["issue"], "title": now["title"], "body": now["body"],
                                    "cutoff": now["createdAt"], "source_revision": source,
                                    "issue_content_sha256": sha(json.dumps({"title": now["title"], "body": now["body"]},sort_keys=True)),
                                    "comments": [], "comments_coverage": "omitted_by_title_body_only_protocol",
                                    "selection_key": sha(f"round3-footprint-v1:{snap['issue']}")})

eligible_inputs.sort(key=lambda v:v["selection_key"])
selected = eligible_inputs[:12]
(OUT / "qualification.json").write_text(json.dumps(qualification, indent=2)+"\n")
(OUT / "frozen-inputs.json").write_text(json.dumps({"policy":"round3-footprint-v1", "selection":"smallest 12 seeded issue-ID hashes among qualified cases", "cases":selected},indent=2)+"\n")

summary = {
    "endpoint": "textual collision of exactly transplanted final net patches",
    "n_pairs": len(records), "labels":dict(collections.Counter(r["label"] for r in records)),
    "unknown_reasons":dict(collections.Counter(r.get("reason") for r in records if r["label"]=="unknown")),
    "own_chain_status":dict(collections.Counter(r["status"] for r in chains.values())),
    "qualified_title_body_cases":len(eligible_inputs), "excluded_title_body_cases":sum(r["status"]=="excluded" for r in qualification),
    "exclusion_reasons":dict(collections.Counter(reason for r in qualification for reason in r["reasons"])),
    "selected_issue_ids":[r["issue"] for r in selected],
    "frozen_inputs_sha256":hashlib.sha256((OUT/"frozen-inputs.json").read_bytes()).hexdigest(),
    "observed_issue_metadata_at":METADATA["observed_at"],
    "source_main_tip":git("rev-parse","main").stdout.strip(),
    "driver_sha256":hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),
    "limitations":["Final net patches are not original pre-repair patches.","Exact preimage requirement excludes difficult cases; unknown is not clean.","Qualified historical input metadata cannot exclude provider pretraining leakage or unobserved work."]
}
(OUT / "summary.json").write_text(json.dumps(summary,indent=2)+"\n")
print(json.dumps(summary,indent=2))
```
