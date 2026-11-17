---
disable-model-invocation: true
---
# Judge: Verdict-Time Branch Handling & Verdict Marker Background

**Load when**: a Verdict-Time CAS Recheck branch fires (claim lost, raced, tree moved) and you need the stand-down template, or you want the Verdict SHA Marker's background (incident, lenient reader spellings, backstops, the `ac-verified` distinction). The marker **rule** itself is always-read: `judge.md` → "Verdict SHA Marker".

On-demand sub-skill of `judge.md` (split out in issue #13957): the core workflow stays in `judge.md`, which points here by the trigger above. Section references to the main workflow live in `judge.md`; existing split-outs `judge-reference.md` and `judge-ci-red.md` work the same way.

## Standing-down notes (Verdict-Time CAS Recheck branches)

**Standing down** (claim lost or raced):

```bash
gh pr comment $N --body "Standing down: re-checked labels immediately before writing my verdict and found <loom:reviewing removed | loom:pr or loom:changes-requested already present> — another Judge's verdict raced mine. Discarding my review; not writing any label."
```

**Standing down** (tree moved — do NOT stamp a verdict marker on this note; it
is not a verdict):

```bash
gh pr comment $N --body "Standing down: the PR head moved from \`$REVIEW_HEAD_SHA\` to \`$VERDICT_SHA\` while I was reviewing. A review verdict is a statement about a specific tree, so I am discarding mine rather than labelling a tree I did not evaluate. Leaving the PR on \`loom:review-requested\` for a fresh pass."
gh pr edit $N --remove-label "loom:reviewing"
```

This shrinks the race window from the full review duration (minutes) to the
gap between the recheck and the write (seconds). It is not a new mechanism —
it is the existing Stale Claim Check re-run one more time, at the point that
actually matters (the write), not just at claim entry. Doctor applies the
analogous recheck (label state, not just head SHA) immediately before its own
completion write — see `doctor.md`'s "Verdict-Time CAS Recheck".

### Verdict SHA Marker: background

The rule (post every verdict through `post-verdict.sh`, on every verdict-label
write, never on a non-verdict comment) is in `judge.md` → "Verdict SHA
Marker". This section is the why behind it. Before #5686 a verdict *label*
survived the head SHA moving, because nothing recorded which tree it covered.

**Incident this closes (rjwalters/repo#192, 2026-08-08)**: a correct
`loom:changes-requested` survived a rebase+force-push that made CI green, and
nothing re-queued the PR until an operator cleared the label by hand. The
inverse is the direction to fear: a `loom:pr` approval that survives a
force-push lets Champion auto-merge a tree **nobody approved**.

**Canonical spelling**: the marker's `verdict=` token is `approved` or
`changes-requested`; `post-verdict.sh` accepts and writes only those. Readers
(the staleness guard, `extract_latest_verdict_sha`) are lenient and also
tolerate `approve` / `request-changes` in a hand-posted marker (#12104) — never
emit those spellings yourself.

**Routing through `post-verdict.sh` closes the *typo/omission* failure mode,
not the *bypass* one (#6382).** The marker was once compliance-by-memory and
was dropped on roughly one verdict in four (#6319) — a compliance *rate*, not
a stale prompt. Once posted through `post-verdict.sh` the marker is an
argument, not prose, so there is no "forgot to append it" outcome. What
remains possible is deviating from this document and calling raw `gh pr
comment` (or a bare `gh pr edit --add-label "loom:pr"`) instead; the
pre-approval checklist in `judge.md` exists to catch that. Two backstops catch what
gets through: the Stale-Verdict Sweep in `judge.md` (guard `--clear --anchor`) and
`loom-daemon`'s `reconcile_pr_verdicts` tick. A markerless **approval** is
re-queued (`loom:pr` → `loom:review-requested`), **never anchored** (#9258);
only a markerless rejection is anchored at the head *as of that run*.

**Neither backstop knows which tree you reviewed**, so a markerless approval
is **lost work that HARD-STALLS the PR** (#12136): it never merges until
re-posted through `post-verdict.sh`, the only path that lands an approval.
Bypassing it is never a shortcut. Use it.

**Not to be confused with `<!-- loom:ac-verified sha=... -->` (#6883).** That is
a separate marker with separate rules: it asserts that an **out-of-band
acceptance-criteria step was actually performed** against a tree, it is read by
Champion's Step 4 close gate rather than by the verdict-staleness machinery, and
whoever performed it (Builder, operator, Judge) posts it — counted only from a
trusted author (#9548). It
is neither a substitute for nor a component of a verdict marker — a comment may
carry one, the other, or both. See "Live Verification and the Circular-Fixture
Smell" for when a Judge stamps it.
