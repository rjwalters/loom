# The `loom:operator` state — "a human is needed"

Loom's coordination substrate is labels (see `CLAUDE.md` § "Label-Based
Workflow") — every pipeline transition (`loom:triage` → `loom:curated` →
`loom:issue` → `loom:building`, `loom:review-requested` → `loom:pr`, etc.) is
a label change on an issue or PR. Before `loom:operator` existed, one state
was the exception: "the engine has stopped and a human is the only way
forward." Champion's merge-risk hold expressed that state as an HTML comment
marker (`<!-- champion:merge-risk-hold -->`) buried inside a PR comment —
invisible to `gh pr list`, the dashboard, or any label-filtered query. See
[#5502](https://github.com/rjwalters/loom/issues/5502) for the incident that
prompted this (four Judge-approved PRs sat held-but-invisible for up to 126
hours).

`loom:operator` moves that state onto the label substrate, where every other
pipeline state already lives.

## Definition

> `loom:operator`: the engine will not work this item further; a human is the
> only transition out.

## Relationship to `loom:blocked`, `loom:operator-only`, and `loom:needs-capability`

Four labels now sit in similar territory. They are **not** consolidated into
one — each answers a different question, and the differences are load-bearing
enough to keep separate (see `.github/labels.yml` inline comments, next to
each definition, for the terse version of this same table):

| Label | Question it answers | Does sweep/shepherd skip it? |
|---|---|---|
| `loom:blocked` | Waiting on a dependency, but still automatable once that clears | No |
| `loom:operator-only` | Requires human action or ruling *outside* automation entirely (credentials, infra, hardware, an owner-gated decision) | **Yes** — sweep/shepherd skip it |
| `loom:needs-capability` | Blocked on a missing tool/agent capability — not an operator-by-right decision, but automation genuinely cannot proceed without the capability existing first (#5817) | **Yes** — sweep/shepherd skip it, identically to `loom:operator-only` today |
| `loom:operator` | The engine has stopped on this specific artifact and a human must act, but the item stays live in its normal queue so the engine's own release conditions can still fire | **No** — stays in the normal re-evaluation queue |

The distinguishing property of `loom:operator` is that it is **re-evaluable**:
unlike `loom:operator-only`, applying it must never cause sweep/shepherd
dispatch to skip the item. That is what makes it safe to apply to a PR that
still needs to pass through its normal Champion tick — the hold that put the
label on can also be the mechanism that takes it back off, without a human
having to remember to remove it.

## Entry points

| Role | Trigger | Status |
|---|---|---|
| Champion (PR merge) | Posts a merge-risk hold (`champion:merge-risk-hold`) because a safety axis is red | **Wired** — `defaults/.claude/commands/loom/champion-pr-merge.md`, "Hold behavior" |
| Builder / Doctor | Encounters work that needs credentials, infra, or a policy ruling outside automation (today's `loom:operator-only` use case) | Not yet wired — follow-up work |
| Judge | A review surfaces a question only a human can answer | Not yet wired — follow-up work |
| Human | Applies the label directly to any issue or PR | Always available (labels are always human-writable) |

**Scope note**: this first pass (#5502) wires only the Champion merge-risk
hold entry point end-to-end. `curator.md`, `builder.md`, `doctor.md`,
`judge.md`, `champion.md`, `champion-common.md`, `champion-issue-promo.md`,
`champion-reference.md`, `loom.md`, `sweep.md`, and `watch.md` all reference
`loom:operator-only` and/or `loom:blocked` today; none of them assume that set
is exhaustive in a way that required editing for this PR, but none of them
have been migrated to *use* `loom:operator` yet either. Extending
`loom:operator` to the Builder/Doctor/Judge entry points above is explicitly
out of scope here — file a follow-up issue per entry point once the Champion
wiring has run in production.

## Exit rule

`loom:operator` is cleared when the artifact the engine judged **materially
changes** — never merely because a role re-read the same artifact and changed
its mind. For the Champion hold, this reuses the *existing* release precheck
(`champion-pr-merge.md`, "Sticky holds" / criterion #2), which already
computes exactly this distinction for the hold marker itself. `loom:operator`
does not add a second, independent state-tracking mechanism — it piggybacks
on the same four precheck outcomes:

| Precheck outcome | `loom:operator` |
|---|---|
| Never held (`PRIOR_HOLD=false`) | Never applied |
| Held, no release signal yet | Stays applied (label add is idempotent — re-asserted, not re-added, each tick the hold stands) |
| Held, released by `loom:auto-merge-ok` override | Removed in the same pass as the reversal comment |
| Held, released by an explicit operator-comment, a new push (head SHA changed), or a new Judge review | Removed in the same pass as the reversal comment |

A human can also clear `loom:operator` directly at any time by removing the
label — the automated exit rule above is the *default* path, not the only
one.

## Current implementation

Only the Champion merge-risk-hold entry/exit pair is wired today:

- **Entry** — `defaults/.claude/commands/loom/champion-pr-merge.md`, criterion
  #2's "Hold behavior" block (`gh pr edit ... --add-label loom:operator`,
  posted alongside the `champion:merge-risk-hold` marker).
- **Exit** — the same file's Step 2 ("Add Pre-Merge Comment"), gated on the
  non-empty `$HOLD_REVERSAL_BLOCK` built by the release precheck (`gh pr edit
  ... --remove-label loom:operator`, posted alongside the
  `champion:merge-risk-hold-cleared` marker).

Both reuse the single release precheck at `champion-pr-merge.md` ("Sticky
holds — a hold does NOT clear on a re-read alone") rather than re-deriving
release state independently.

**One consumer honors the hold without ever setting it (#5686)**: the
stale-verdict machinery (`defaults/scripts/verdict-staleness-guard.sh` and
`loom-daemon`'s `reconcile_pr_verdicts`) clears a review verdict whose head SHA
has moved — but **not** on a PR carrying `loom:operator`, `loom:operator-only`,
or `loom:blocked`. Re-queueing such a PR for review would silently un-park it,
which is precisely the transition only a human may make. It still reports the
verdict as stale, so the PR is not merged either; it simply stays exactly where
the operator left it.

## `loom:operator-only` sub-kinds (#5671)

`loom:operator-only` was a single label carrying at least four distinct
meanings — blocked on infrastructure that does not exist yet, mechanical
(host/credential access, no judgement required), a genuine operator decision,
or simply mislabelled as the cautious default — with no way to tell them apart
without reading the issue. A fleet-wide sample found 96 open
`loom:operator-only` issues, only 1 of which named its blocker in a
machine-readable way. That makes triage a reading exercise instead of a label
query, and the pile grows monotonically because nothing can mechanically
distinguish "waiting for something that will resolve itself" from "a human
must rule on this."

**Resolution of the open design question below** (previously "TBD" — see the
now-superseded bullet this section replaces): `loom:operator-only` remains the
distinct, permanent gating label — it is **not** subsumed by `loom:operator` +
a separate skip-dispatch signal. The two labels answer different questions
(see the table above: one causes sweep/shepherd to skip the item entirely, the
other keeps it in the normal re-evaluation queue) and collapsing them would
lose that distinction. Instead, `loom:operator-only` is refined **in place**
by three sub-kind labels applied *alongside* it:

| Sub-label | Meaning | Self-clearing? |
|---|---|---|
| `loom:operator-blocked` | Waiting on a named issue, PR, or piece of infrastructure that does not exist yet — the condition is transient and expected to clear once that lands | Yes — a future pass can safely re-evaluate once the named blocker closes/merges |
| `loom:operator-mechanical` | Needs host or admin access, a credential, or another mechanical action — no judgement required | No (needs the action to happen) |
| `loom:operator-decision` | Genuine operator judgement — a ruling, a trade-off, a "which side ships first" call — is needed | No (needs a human ruling) |

**Rules for any role applying `loom:operator-only`:**

1. **Always apply exactly one sub-label alongside it**, in the same command
   (e.g. `--add-label "loom:operator-only,loom:operator-decision"`) — never
   the base label alone. This is additive: every existing filter/skip/query
   keyed on the base label (sweep pre-flight, `warn-operator-gated.sh`,
   Champion's promotion-queue exclusions, Doctor/Curator's queue exclusions)
   is unaffected, because the base label is never removed or replaced.
2. **`loom:operator-decision` is the safe default** when the kind is not
   obvious — it is the one meaning that is always safe to over-apply (a human
   is never wrong to look at a genuine-judgement item, even if it later turns
   out to have been mechanical or self-clearing). Never leave a
   `loom:operator-only` application without a sub-label rather than guess
   wrong; when genuinely unsure, default to `loom:operator-decision`.
3. **When the sub-kind is `loom:operator-blocked`, name the blocker in
   machine-readable form**, not only in prose: include a `Blocked by #N` /
   `Depends on #N` / `Requires #N` line in the same comment (same phrasing
   `detect-dependency-cycle.sh` and `warn-operator-gated.sh` already parse via
   regex — see their headers). A backtick-quoted issue reference alone (e.g.
   `` `owner/repo#123` `` in prose) does not satisfy this — the phrase itself
   must be present so a future automated pass can extract it without an LLM
   read.
4. **No backfill.** Existing plain `loom:operator-only` issues are not
   required to gain a sub-label retroactively — no code path may assume every
   `loom:operator-only` issue already carries one. The value is in the intake
   rate, not a one-time migration.

**Where this is wired today** — every role that can apply the label (#5819):

| Role | Site | Sub-kind it applies |
|---|---|---|
| Champion | Unrevised-proposal N=2 escalation (`champion-issue-promo.md`), epic-complete-unpromoted escalation (`champion-common.md`) | `loom:operator-blocked` when the recurring finding is itself a live, open dependency; `loom:operator-decision` otherwise |
| Champion | Dependency-cycle detector (`detect-dependency-cycle.sh`, invoked from `champion-issue-promo.md` and `champion-pr-merge.md`), capped-PR close recommendation (`champion-pr-merge.md`) | `loom:operator-decision` — matching their own rationale ("breaking a cycle is a human decision" / "the approach itself is not viable") |
| Curator | "Applying `loom:operator-only`" (`curator.md`) — routing an issue that encodes a still-pending human decision instead of closing it | Caller's choice; `loom:operator-decision` default |
| Builder | "Applying `loom:operator-only`" (`builder.md`) — parking a claimed issue that turns out to need a human; `builder-complexity.md` additionally states that a *size* finding is `loom:blocked`, never this label | Caller's choice; `loom:operator-decision` default |
| Judge | "Applying `loom:operator-only`" (`judge.md`) — an issue surfaced during review, or a PR raising a question only a human can answer | Caller's choice; `loom:operator-decision` default |
| Doctor | "Applying `loom:operator-only`" (`doctor.md`) — the rare case a Doctor session parks a PR it cannot fix without host/credential access (Doctor otherwise only *filters* on the label) | Caller's choice; `loom:operator-mechanical` is the typical Doctor case |

See #5664 for the incident that motivated distinguishing the transient
(`loom:operator-blocked`) case from a genuine decision in Champion's escalation
path, and #5819 for the fleet-wide measurement (2 of 78 operator-only issues
across the five busiest repos carried a sub-kind) that motivated wiring the
remaining four roles. The prompt-side convention is enforced mechanically by
`defaults/scripts/tests/test-operator-only-subkind.sh`, which fails CI on any
`--add-label` in a role prompt, doc, or script that applies `loom:operator-only`
without a sub-kind in the same argument.

## `loom:needs-capability` — a narrower claim than `loom:operator-only` (#5817)

A fleet-wide census (2AMLogic/2am#184) found `loom:operator-only` carrying at
least two very different populations under one label: issues that are
genuinely **operator-by-right** (disclosure flips, spending, legal, tier
grants, fleet membership — a human must rule regardless of tooling), and
issues that are simply **unbuilt capability wearing an operator label** —
work automation cannot yet do because a tool or agent capability does not
exist, not because a human's judgement is required. Mixing the two makes the
label unreliable for triage: "this needs a human ruling" and "this needs
someone to build the missing tool first" call for entirely different next
steps, but both looked identical on the forge.

`loom:needs-capability` splits the second population out:

> `loom:needs-capability`: blocked on a missing tool/agent capability, not an
> operator-by-right decision; the filed capability-request issue must be
> linked (e.g. `Depends on #N` / `Requires #N`, the same machine-readable
> convention `loom:operator-blocked` uses above) so a future pass can tell
> when the capability lands.

**Skip parity, by design.** `loom:needs-capability` skips `/loom:sweep`
identically to `loom:operator-only` today — same hard-skip row in the `all`
sentinel's "Aggressive candidate taxonomy" table (`sweep.md`), same skip
condition in Mode C's C0 pre-flight, same dependency-declared check in
`warn-operator-gated.sh` (a candidate that depends on either label is flagged
the same way). Nothing about *routing* differs yet — only the label's
*meaning* is narrower, and the description now records which capability
request must land before a human should reconsider it. This issue (#5817) is
deliberately scoped to the split only; **which label to apply when**, a
required "why" comment, and the bidirectional routing convention (should a
landed capability request automatically clear the label?) are follow-up work
(2AMLogic/2am#184's remaining asks, tracked as #5818).

**Additive only.** No existing `loom:operator-only` issue is retagged as part
of introducing this label — 2AMLogic/2am#184 explicitly rejected retrofitting
the existing backlog ("retrofitting 120 issues is not proposed; apply going
forward"). The value is in the intake rate for newly filed/curated issues,
the same "no backfill" principle the operator-only sub-kinds above already
follow.

## Follow-up work

- Wire `loom:operator` into Builder/Doctor's credential-or-policy stop path
  (today's `loom:operator-only` usage). **Still open** — #5819 wired the
  *sub-kind requirement* into those paths, but they still route to
  `loom:operator-only` (skip-dispatch), not to the re-evaluable
  `loom:operator`.
- Wire `loom:operator` into Judge's unanswerable-question path. **Still open**,
  same distinction as above.
- Build the actual self-healing re-evaluation pass that `loom:operator-blocked`
  makes possible (re-check the named blocker, un-escalate when it clears) —
  tracked separately in #5664; this document only defines the label the
  self-healing pass keys off.
- Decide and document the `loom:needs-capability` vs. `loom:operator-only`
  routing convention (2AMLogic/2am#184's remaining asks) — tracked as #5818.
