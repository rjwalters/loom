---
disable-model-invocation: true
---
# Judge: Formal Review & Inline Thread Reconciliation — Exits, Dispositions, Enforcement

**Load when**: `check-review-feedback.sh` exits non-zero (10/11/12), or you are preparing an approval that must cite finding dispositions.

On-demand sub-skill of `judge.md` (split out in issue #13957): the core workflow stays in `judge.md`, which points here by the trigger above. Section references to the main workflow live in `judge.md`; existing split-outs `judge-reference.md` and `judge-ci-red.md` work the same way.

## Formal Review & Inline Thread Reconciliation (REQUIRED Before Approval, #7647)

Issue comments are not the whole review record. A PR carries **three**
independent feedback surfaces, and Loom's own approval convention (a
`gh pr comment` + label, never `gh pr review --approve`) lives on only one of
them. The gate below covers formal reviews, inline threads, **and** the
`changes-requested` verdicts posted as comments on surface 1 (#12475):

| Surface | Endpoint | What Loom writes there |
|---|---|---|
| General conversation | `/issues/<n>/comments` | Judge verdicts, Doctor/Builder notes. A `changes-requested` verdict (its `loom:verdict-sha … verdict=changes-requested` marker) from a trusted author, at any head, is gated like a formal review |
| **Formal reviews** | `/pulls/<n>/reviews` | nothing — but humans and bots submit `CHANGES_REQUESTED` here |
| **Inline review threads** | `/pulls/<n>/comments` + GraphQL `reviewThreads` | nothing — line-anchored findings and their resolution state |

**A Loom approval comment on surface 1 does not answer, supersede, or erase a
request on surface 2 or 3.** In kicad-tools#5369 a Judge approved at head
`23a0289` while an unresolved formal `CHANGES_REQUESTED` review sat at that
*same* head asking for DFM-analysis coverage; the approving pass had read the
issue comments and never `pulls/reviews`, and green CI did not exercise the
missing requirement either. Champion stopped the merge. Nothing in the
pipeline would have.

### The gate

```bash
./.loom/scripts/check-review-feedback.sh --number <PR> --head-sha "$REVIEW_HEAD_SHA"
```

It reads **all pages** of `/pulls/<n>/reviews`, plus the inline threads with
their **actual** resolution state (GraphQL `reviewThreads`, falling back to
`/pulls/<n>/comments` when GraphQL is exhausted), retains review id / state /
commit association / timestamp, and prints eval-safe `KEY=VALUE` lines
(enums, integers, SHAs and a findings-file path — never forge text; review
bodies go only to `REVIEW_FEEDBACK_FINDINGS_FILE`, which is data to read, not
to `eval`). Each trusted `changes-requested` verdict comment counts
individually (id in `REVIEW_BLOCKING_IDS`) until a later approval whose
reconciliation marker says `reconciled=yes` has cited it.

| Exit | `REVIEW_FEEDBACK_STATE` | What it means | What you must do |
|---|---|---|---|
| 0 | `CLEAR` | Complete read; nothing outstanding | Approve normally |
| 10 | `BLOCKING` | An outstanding `CHANGES_REQUESTED`, or an unresolved inline thread, **at the current head** | Reconcile each finding against the tree in writing, citing its review id — or request changes |
| 11 | `NEEDS_RECONCILIATION` | Findings anchored to an **older head**, an unrecognized review state, or resolution state that could not be established | Produce evidence of repair or an explicit disposition — a moved head is NOT a dismissal |
| 12 | `UNKNOWN` | The read itself failed / paginated incompletely | **Not** proof of "no blockers". Re-run it; never assert a state you did not read |

### Disposing of findings

**This is about reading and resolving feedback, not about changing how Loom
votes.** Do not start using `gh pr review --approve` (the self-review API
restriction at the top of this file is unchanged), and **never dismiss a
formal review** to clear the gate. Respect the forge's own state: a review
GitHub reports as `DISMISSED` is dismissed; one it reports as
`CHANGES_REQUESTED` is outstanding until the reviewer says otherwise or you
write down why it no longer applies.

When anything is outstanding, your approval must state — in the verdict
comment itself — **which findings were fixed, which were superseded, and which
remain blocking**, naming each review id. Scope disagreement ("that is the
consumer's job") is a reconciliation you must argue explicitly, never an
inferred override. If a finding genuinely still applies, the verdict is
`changes-requested`, not an approval with a caveat.

**Re-review after any `changes-requested` verdict (#12475):** your approval
must carry an explicit per-finding resolution list, one entry for every
finding in every earlier `changes-requested` verdict on the PR, whatever the
channel (comment, formal review, inline thread): either `fixed at <file:line>`
or `still open`. Any `still open` entry blocks approval, so the verdict is
`changes-requested`. A summary that restates the old claim as satisfied ("one
champion_merge per repo") without a `file:line` you opened in the tree is not
a resolution. Put the list in `--reviews-reconciled`; the gate checks that
every outstanding id is cited, the list is what makes each citation honest.

### It is enforced by the posting mechanism, not by memory

`post-verdict.sh` runs this gate itself on **every `approved` verdict** and
refuses to post (exit 3) unless the state is `CLEAR` or you supply the
disposition:

```bash
./.loom/scripts/post-verdict.sh <PR> approved "$VERDICT_SHA" \
  --body "…" \
  --reviews-reconciled "Review 5202931719 (DFM coverage) was addressed in $VERDICT_SHA — tests/test_dfm_coverage.py exercises both modules; nothing deferred."
```

The disposition text and the gate's counts are appended to the posted comment
(plus a `<!-- loom:review-reconciliation … -->` marker), so a reconciliation
is auditable on the forge rather than asserted in a transcript. A blanket
sentence that cites none of the blocking review ids is refused.

Every *read* failure fails closed. The one exception is a gate script that is
not installed at all (an install defect, which says nothing about this PR):
that degrades to the old behaviour with a stderr warning and a
`state=gate-unavailable` marker, rather than stalling every approval on every
host. If you see that marker on your own verdict, resync the installed
`.loom/scripts/` from the primary checkout and re-verify the PR's reviews by
hand before trusting the approval.

**Because every approval path in this document posts through
`post-verdict.sh`, this covers the fast paths too** — the Docs-Only Fast Path,
the conflict-only Fast-Track, the minor-PR-description-fix approval and the
trivial-fix approval all pass through the same gate without any of them
opting in. `changes-requested` verdicts are deliberately not gated: they
cannot merge anything, so gating them would only burn forge reads.
