# Research round 3: stronger measurement, narrower conclusions

**Run:** 2026-10-01, isolated AWS directory, Git 2.43.0 / Python 3.12.3 /
Augment SDK 0.2.2. All Git controls, reconstruction, retrieval, and scoring ran
on AWS. No live scheduling policy or real PR branch was changed.

## Result in one paragraph

The old conflict instrument is demonstrably unreliable: changing only its base
selection changes **63/121** labels. Strictly isolating each PR's final net patch
qualifies **21 pairs**, all clean, while leaving **100 unknown**. A separate,
timestamp-qualified historical footprint study supplies a limited positive result:
at ten suggested files, Augment recalls **25.1%** of final changed paths versus
**10.0%** for fixed BM25 and **19.9%** for an exploratory literal-path/BM25 hybrid.
The latter reduces the apparent advantage to **5.2 percentage points**. That is
encouraging for optional code-context assistance, not evidence for serialization
thresholds or scheduling benefit. The strict primary inclusion rule qualified
only one acquisition; the 12-case figures are explicitly a finite-budget
sensitivity analysis.

## 1. What was frozen, and when

| Stage | Frozen artifact / commit | Scope |
|---|---|---|
| Git diagnostic | [PROTOCOL.md](PROTOCOL.md), `f359dfea4` | Before new oracle execution; old 121-pair pool remains exploratory. |
| Patch isolation and case selection | [PHASE2.md](PHASE2.md), `0bacd10ff` | Before reconstructing patches or acquiring new predictions. |
| Retrieval and lexical policies | [ACQUISITION.md](ACQUISITION.md), `f359bf0e6` | Before this round's provider queries; identical pinned corpus for all methods. |
| Metrics | [EVALUATION.md](EVALUATION.md), `aef30b2a3` | Before target scoring; recall@10 is the primary descriptive comparison. |
| Output-cap sensitivity | [BUDGET-NOTE.md](BUDGET-NOTE.md), `598c4bcc4` | Added after observing response-budget flags, before target scoring. Original primary result retained. |
| Stronger cheap comparator / influence | [STRESS.md](STRESS.md), `609f844b7` | Explicitly post-hoc robustness checks after observing the fixed comparison. |

These are versioned within-session protocol freezes, not an externally registered
trial. No threshold was fitted this round. Some selected issues appeared in prior
experiments; this is not a new untouched policy holdout.

## 2. Falsification: forced base versus recorded Git ancestry

Each row uses the exact same pinned PR heads under two instruments. Both
orientations were retained, with stdout/stderr and checksums.

| Previous forced-base result | Native Git clean | Native Git conflict |
|---|---:|---:|
| Clean | 43 | 9 |
| Textual conflict | 54 | 15 |

- **63 disagreements**: 54 conflict→clean and nine clean→conflict.
- **52 pairs contain one head in the other's ancestry**; 36 of those were
  called conflicts by the forced-base instrument despite native clean merges.
- The declared base is not an ancestor of both heads in **114/121** pairs.
- Native Git reports 24 conflicts, but **24 is not a corrected estimate of
  independent issue collision**: native merges still retain inherited history.

This experimentally supports the audit's warning about base-skew correlation.
The previous thresholds were responding in part to a choice of measurement
instrument, not an established pair-caused cost.

See [git-comparison.csv](git-comparison.csv) for every paired result. This is a
finite diagnostic of an already inspected cohort, not a representative study of
production conflict prevalence.

### Controlled cases

| Fixture | Native textual result | Forced-base result | Component evidence |
|---|---|---|---|
| Independent edits to different files | Clean | Clean | Textual negative control. |
| Different edits to the same line | Conflict | Conflict | Textual positive control. |
| Disjoint own edits with an inappropriate later base | Clean | Conflict | Reproduces an artificial conflict. |
| Producer/consumer edits to different files | Clean | Clean | Base, A-alone, B-alone pass; both combined orientations fail a resource-budget invariant. |

All four fixtures also passed the strict transplant procedure below, including
the positive collision fixture. Fixture repositories are archived as Git bundles.
The semantic fixture demonstrates a label distinction; it does not measure
semantic-conflict prevalence in Loom.

## 3. Exact final-net-patch reconstruction

For each PR, require a contiguous, single-parent chain of recorded own commits
ending at its pinned head. Use the first commit's parent as the patch base.
Require a unique common ancestor of the two patch bases and exact blob/mode
preimages for every changed path. Apply the net binary patch with a private Git
index, verify exact postimages, and create two synthetic commits with the same
parent. This preserves each net patch without silently importing upstream work.

| Disposition | Pairs |
|---|---:|
| Both exact reconstructions succeeded; clean merge | **21** |
| Changed-file preimages incompatible with the common source | **63 unknown** |
| One or both own-commit chains unqualified | **37 unknown** |

The 242 PR chains include 200 qualified chains and 42 unqualified chains
(41 nonlinear, one noncontiguous). Pair-level failures are counted once, so the
PR-chain and pair counts have different denominators.

Five of the 21 qualified pairs had previously been forced-base “conflicts.”
Two had native-history conflicts but clean isolated patches. This reinforces
the need to separate inherited-history integration from own-patch collision.

**There are zero real positive collisions in the reconstructable subset.**
That cannot be converted into a low-risk claim: strict reconstruction can exclude
precisely the difficult cases. The endpoint is final net patches, not unavailable
original pre-repair heads. It still cannot train or calibrate a conflict policy.

## 4. A narrower input-qualified retrieval experiment

The next question is deliberately simpler: which final changed paths can be
found from an issue's unedited title/body and code predating that issue?

### Eligibility and data provenance

- Re-read authoritative title/body, creation time, and `lastEditedAt` for all
  242 issues. Archive that response and observation time.
- Require no recorded issue edits and equality with the saved title/body.
- Require a qualified own-commit chain; issue creation must precede every own
  commit's author and committer time.
- Choose a first-parent main revision dated before issue creation, require its
  ancestry of the patch base, and reject ancestry containing the final PR head.
- **61 issues qualified; 181 were excluded.** Reasons overlap: 168 edited issues,
  41 nonlinear chains, three incompatible source ancestors, one noncontiguous chain.
- Select **12** using the smallest SHA-256 values of
  `round3-footprint-v1:<issue>`, without inspecting changed-file counts or
  retrieval performance. They cover **ten distinct source revisions**.

See [eligibility.csv](eligibility.csv) and
[input-provenance.csv](input-provenance.csv). Eligibility relies on forge metadata
and Git timestamps, not archived proof of when every commit was publicly visible.
Comments are omitted by design. Earlier unobserved implementation and provider
pretraining knowledge remain limitations. “Timestamp-qualified” is more precise
than claiming a fully proven historical information boundary.

### Equal corpus, inspectable provider evidence

All methods receive the same pinned regular-text corpus: 1,985–2,836 files per
case, with complete path/blob/content hashes and explicit exclusions. There is
no 3,000-file truncation. Inputs exceed neither the frozen per-file nor aggregate
byte caps. Provider index membership matches the supplied corpus exactly.

- Two fixed queries per issue, 20,000-character output budget each.
- **24/24 successful responses; zero out-of-corpus returned paths.**
- Numbered snippet verification finds **8,622 exact source lines** and **14
  truncated prefixes**, all at response ends; no unexplained line mismatch.
- Exact queries and raw responses are retained. Provider model internals and
  dollar cost are not exposed; both remain unknown.
- Summed measured index time: **53.2 seconds**; search time: **64.8 seconds**.
  These are observed service timings with existing provider blob reuse, not
  cold-start guarantees or measured savings in a scheduler.

### The output-budget complication is part of the result

Eighteen queries reached the output limit. Under the frozen strict completeness
rule, **only issue #9591 qualifies for the primary analysis**. Augment and BM25
tie there at recall@10 = 1/3. This primary comparison is inconclusive.

The following 12-case table instead evaluates **what was actually returned
within the fixed budget**, as declared in [BUDGET-NOTE.md](BUDGET-NOTE.md) before
target scoring. It is a post-protocol sensitivity analysis. Capped responses are
not relabeled complete, and unreturned evidence is not declared absent.

### Finite-budget sensitivity results at ten suggestions

The target includes all net changed paths, including new/deleted paths and both
names in renames. Paths absent from the index stay in the recall denominator.
Precision@10 is hits/10; a short result list does not receive a smaller denominator.

| Method | Macro recall@10 | Macro precision@10 | Status |
|---|---:|---:|---|
| Literal issue-path hints | 11.9% | 7.5% | Frozen baseline; six empty lists. |
| BM25, k1=1.2 / b=0.75 | 10.0% | 14.2% | Frozen baseline. |
| Literal hints first, then BM25 | **19.9%** | 16.7% | Post-hoc stronger cheap comparator, no fitted parameter. |
| Augment, first appearance across two queries | **25.1%** | **25.0%** | Finite-budget sensitivity. |

Compared with BM25, Augment wins on seven issues, ties on four, loses on one.
The mean recall difference is +15.1 points. However, the single-file issue
#9467 contributes heavily: leave-one-case-out differences range from +7.3 to
+16.9 points. Micro recall is 18.0% versus BM25's 10.2%, a smaller +7.8-point gain.

Compared with the cheap hybrid, Augment wins on **five**, ties on **six**, loses
on **one**, with a mean gain of **+5.2 points**. The descriptive source-group
bootstrap range is **+1.1 to +10.3 points**. This is not a confirmatory significance
result: it has 12 selected historical cases, ten partial dependence groups, a
post-protocol coverage analysis, and a post-hoc comparator. It warrants replication.

Only 72.3% of target paths are indexable on average. Issue #9434 has none of
its seven eventual changed paths in the old corpus. Returning existing code
cannot by itself constitute a complete plan for new files.

### Per-issue inspection: hits among ten suggestions

| Issue | Actual changed paths | Augment | BM25 | Literal |
|---|---:|---:|---:|---:|
| #9467 | 1 | 1 | 0 | 1 |
| #8919 | 17 | 5 | 1 | 0 |
| #9038 | 18 | 5 | 3 | 0 |
| #9105 | 30 | 8 | 5 | 5 |
| #9591 | 3 | 1 | 1 | 0 |
| #9683 | 14 | 0 | 0 | 0 |
| #9133 | 18 | 3 | 2 | 1 |
| #9442 | 16 | 1 | 1 | 0 |
| #8757 | 15 | 2 | 1 | 1 |
| #9440 | 21 | 1 | 2 | 0 |
| #8758 | 7 | 3 | 1 | 1 |
| #9434 | 7 | 0 | 0 | 0 |

[per-issue.csv](per-issue.csv) includes all frozen k=5/10/20 results and explicit
denominators. Detailed hit/miss paths, the hybrid check, and bootstrap inputs are
in [summary.json](summary.json) and the evidence archive.

## 5. Which reviewer objections this round addresses

| Objection | New evidence | Still unresolved |
|---|---|---|
| Measurement validity | Independent Git control, 121 paired comparisons, exact patch pre/postimage checks, positive/negative fixtures | 100 unknown real patch-pair outcomes; original heads and semantic labels missing. |
| Temporal inputs | 61 qualified title/body cases; 12 frozen pre-issue source indexes; actual returned text checked against blobs | Earlier unseen work, comment requirements, historical publication evidence, provider memorization. |
| Weak baselines | Same-corpus BM25 and literal hints; stronger cheap-hybrid stress check | Properly tuned cheap baselines on an independent development cohort, second repository. |
| Statistics | Explicit denominators, primary/sensitivity separation, paired cases, source-group bootstrap, influence analysis | Small selected retrospective set; no prospective policy holdout or power-qualified action trial. |
| Reproducibility | Raw inputs/responses, corpus hashes, source commands, fixture bundles, code/version pins | Fresh provider output may differ; some Git objects may eventually need archival beyond forge refs. |
| Scheduling utility | No new unsupported intervention claim | Net benefit of synchronization, serialization, or consolidation has not been measured. |

## 6. Updated recommendation

**The proposal is now stronger as an optional context-retrieval aid and weaker
as a threshold-driven scheduler.**

1. Retain the cheap literal/BM25 hybrid as a serious comparator. Its recall nearly
   doubles fixed BM25 here and materially reduces the claimed Augment increment.
2. Carry Augment forward as an optional, cached shadow feature. The +5.2-point
   exploratory footprint increment may be useful, but dollar cost and delivery
   benefit remain unmeasured.
3. Retire forced-base raw-head labels and old numeric scheduling thresholds.
   Qualified reconstruction must preserve unknowns; do not train on the 100
   reconstruction failures as either clean or conflicting.
4. The next decisive data should be prospective dispatch-window snapshots on a
   common observed base, followed by original pre-repair PR heads and recorded
   integration effort. Pre-register finite-budget retrieval semantics correctly.
5. Replicate the cheap-hybrid comparison on new groups before adding complex intent
   or hub-weight machinery. Evaluate the eventual scheduling action separately.

This round establishes concrete measurement failures and a modest retrieval
benefit under a narrow exploratory endpoint. It does **not** establish a calibrated
conflict probability, a production serialization threshold, or certainty about
the benefit of changing scheduling.

## 7. Inspect and reproduce

- [Evidence archive](evidence.tar.gz): **245 files**, 4,513,797 compressed bytes;
  SHA-256 `b0943906890953f3c50538a73151739484ad2812877daffd0c44c7301ce3fc79`.
- [EVIDENCE-SHA256SUMS](EVIDENCE-SHA256SUMS): hashes for every archive member.
- [ORACLE.md](ORACLE.md), [PATCH-STUDY.md](PATCH-STUDY.md),
  [ACQUISITION.md](ACQUISITION.md), [EVALUATOR.md](EVALUATOR.md), and
  [STRESS.md](STRESS.md): exact standalone research commands, archived before
  execution where indicated. These do not repair or validate the production CLI.

Extract the archive into an external research directory. It contains the original
121-pair manifest, authoritative metadata snapshot, fixture Git bundles, all
per-pair oracle results, qualified inputs, raw retrieval and index manifests,
target paths, and evaluation files. No credentials or provider session file are
included. Check archive member hashes with `sha256sum -c EVIDENCE-SHA256SUMS`
from the extraction root (copy the checksum file there first).

Offline scoring needs neither a provider account nor a Git checkout:

```text
python3 round3_evaluate.py <extraction-root>
python3 round3_stress.py <extraction-root>
```

Credential-free rescoring was run on AWS: **20 evaluation files were
byte-identical**, and six hand-enumerated metric controls plus [0,1] invariants
passed. Original-case source and PR object availability is needed to re-derive
Git outcomes; pin main to the recorded `source_main_tip` when repeating historical
selection. Fresh retrieval requires the documented external credential setup and
the recorded SDK; it is not promised to return identical rankings.
