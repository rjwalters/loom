# Retrospective pattern mining from Judge rejections and Doctor fixes (#5850)

**Status:** design decision. **Recommendation: park the automated mining pass; adopt the
manual-audit alternative instead.** No runtime, role, hook, script, or config change ships with
this document — the only file in its PR is this one.
**Source issue:** [#5850](https://github.com/rjwalters/loom/issues/5850) — the **adapt** verdict
(idea 4) from the evaluation in `docs/research/atomic-claude-evaluation.md`
([#5844](https://github.com/rjwalters/loom/issues/5844)).
**Measured against:** `origin/main` @ `090d15e0`, 2026-08-09, on `rjwalters/loom`. Every rate,
count, and recall number below was measured against the live forge on that date and is
**volatile by construction** — §2 gives the exact commands so any future reader can re-derive
them rather than trust them.
**Upstream reference:** [damusix/atomic-claude](https://github.com/damusix/atomic-claude) (MIT),
read-only. Nothing is vendored or ported. The upstream clone made for #5844 was not retained, so
every upstream citation here is second-hand through `docs/research/atomic-claude-evaluation.md`
§4 rather than a fresh read of the source.

---

## Answers, up front

The issue asks three questions. The short answers:

| # | Question | Answer |
|---|----------|--------|
| 1 | Mining source and cadence | **Timeline `loom:changes-requested` labeled events** as the index (not comment markers), joined to the verdict comment by a ≤120s time window; the reject→`loom:treating`→re-review→`loom:pr` label sequence as the fix record. Owner: **Auditor**, on its existing periodic tick, bounded to events since the last pass. §4–§5. |
| 2 | The write guardrail | Binding, and stated as a hard contract in §6: every proposed change to `.loom/roles/*.md`, `CLAUDE.md`, or `.github/labels.yml` from this mechanism ships as an ordinary issue → PR → Judge review. **No direct write, at any confidence, ever** — plus a bloat clause upstream has and a naive port would drop. |
| 3 | Build now, or park | **Park the automated pass. Adopt the manual checklist.** Rejections run at ~2.4/day — small enough that a periodic reader covers *100%* of them, so automation buys no coverage; and the observed patterns are, so far, non-recurring. §7 gives a measurable re-open trigger rather than an indefinite "later". |

---

## 1. Problem

Loom has no systematic way to turn its own review failures into role-prompt improvements. When a
Judge rejects a PR for a reason that will recur — an acceptance criterion silently dropped from a
PR description, a doc left stale beside the code it describes — that lesson lives in one comment
on one PR and is read by exactly one Doctor, once. Process problems become durable improvements
only when a human or an Auditor happens to notice the same thing twice and files an issue by hand.

atomic-claude's `/retrospective-learning` closes the equivalent gap for a single interactive user:
mine recent session history for correction signals, cross-reference the installed config
artifacts, and walk the findings one at a time with the human, proposing edits to skills, rules,
and `CLAUDE.md`. #5844 judged this **adapt** — the concept is right, the mechanism is not
portable.

This document decides what, if anything, Loom should build.

### What does not transfer, and why

| atomic-claude assumption | Loom reality |
|---|---|
| One long-lived interactive session per repo | One-shot dispatch per unit of work; the agent exits when the PR opens |
| `.jsonl` session transcripts accumulate locally across runs | No multi-session transcript exists to mine; each sweep's log is its own artifact |
| A live human turn to resume from (`AskUserQuestion`, Accept/Modify/Skip) | Overwhelmingly headless and unattended — there is no turn to resume from |
| Per-user state at `~/.atomic/retro-runs/*.json` tracks whether an accepted change stuck | No per-user or per-host state store exists, and adding one repeats the mistake #5844 §6 already rejected for the Realm wiki: coordination state outside the forge that no dispatched agent is guaranteed to see |

What *does* transfer is the input class. Loom's corrections are not chat corrections; they are
**Judge review verdicts and Doctor fix commits**, and unlike a chat transcript those are durable,
server-side, and addressable by any agent on any host. The rest of this document is about whether
that input is worth mining mechanically.

---

## 2. What the record actually looks like (measured, not assumed)

Every number below was measured on 2026-08-09 against `rjwalters/loom`. The commands are given so
they can be re-run; the conclusions in §7 depend on these rates, so a future reader should
re-derive rather than inherit them.

**Throughput.** 300 PRs merged between 2026-08-05T01:02Z and 2026-08-09T23:22Z — **≈61 merged
PRs/day**.

```bash
gh pr list --state merged --limit 300 --json number,mergedAt \
  --jq '{newest: .[0].mergedAt, oldest: .[-1].mergedAt, count: length}'
```

**Rejection rate.** Of the 100 most recently merged PRs, **4** carried a `loom:changes-requested`
labeled event — PRs #5716, #5730, #5794, #5799. That is **4%**, or **≈2.4 rejection events/day**,
**≈17/week**.

```bash
for pr in $(gh pr list --state merged --limit 100 --json number --jq '.[].number'); do
  gh api "repos/rjwalters/loom/issues/$pr/events" \
    --jq '[.[] | select(.event=="labeled" and .label.name=="loom:changes-requested")] | length'
done
```

**Marker recall is not 100%.** `judge.md` requires every terminal verdict comment to end with
`<!-- loom:verdict-sha sha=<sha> verdict=approved|changes-requested -->`. Only **2 of the 4**
rejection verdicts carried it (#5794, #5799 did; #5716, #5730 did not — both of those PRs carry
exactly one marker, on their later *approval*). Both misses were on 2026-08-08, the first day
after the marker convention shipped (`1187c926`, 2026-08-08T06:17Z), and both hits were the day
after, so this is **plausibly rollout lag rather than a standing defect** — but it is unverified
either way, and it is enough to disqualify the marker as a mining index. It is worth noting
separately that `verdict-staleness-guard.sh` classifies a marker-less verdict as `UNVERIFIABLE`
and deliberately *keeps* it; a persistent recall gap there would silently no-op the guard on the
affected PRs, which is a #5686 concern, not a #5850 one.

**Label events, by contrast, have 100% recall by construction** — GitHub generates them
server-side from the `gh pr edit` call, so no prompt-following behaviour is involved. All four
PRs produced an identical, fully-reconstructible sequence:

```
+loom:review-requested → +loom:reviewing → +loom:changes-requested
                       → +loom:treating → +loom:review-requested
                       → +loom:reviewing → +loom:pr
```

**The verdict comment can be joined to its label event by time.** `judge.md` chains the comment
and the label write with `&&` in every verdict path, so the gap is bounded and tiny — measured at
**2s** (#5716, 07:33:59Z → 07:34:01Z), **8s** (#5730), **1s** (#5794), and **1s** (#5799). A
"newest bot comment strictly preceding the labeled event, within 120s" join recovers the verdict
body for **4 of 4**, including both PRs the marker missed.

**Verdict comment headings are not machine-parseable.** The four rejections opened with
`❌ **Changes Requested**`, `## Judge review: changes requested` (×2), and a prose line ending
`Verdict: **changes requested**`. Any classifier over the *body* must be semantic, not textual.

**Author attribution is useless for role identification.** Every comment on all four PRs — Judge
verdicts, Doctor fix reports, stand-downs — is authored by the same `loom-fleet-dispatch[bot]`
identity. Role must be inferred from label events, never from `.user.login`.

**The findings themselves were substantive and mostly distinct.** Across the four: a data-loss
path in a stash-classification routine (#5716), a glob-pattern bug that missed root-level
`migrations/` (#5730), an acceptance criterion silently absent from both diff and PR description
(#5794), and doc drift left behind by a code change (#5799). Roughly **two of four generalise to
a process pattern** ("an AC was dropped without being marked deferred", "docs describing changed
code went stale"); the other two are code-specific findings that no role-prompt edit would have
prevented. **No pattern recurred within the sample.**

**The cycle closes fast.** Rejection → re-review took 73, 26, 8.5, and 10 minutes. Nothing here
is stuck waiting for a mechanism.

---

## 3. Decision 0 — What is being decided

Three things are separable, and conflating them is how this kind of proposal turns into scope
creep:

1. **The mining source** — what data a retrospective pass would read (§4).
2. **The mechanism** — an automated pass that classifies, dedupes, and files proposals (§5).
3. **The write policy** — what any such proposal is allowed to do to the control surface (§6).

The recommendation splits them: **adopt (1)**, **park (2)** behind a measurable trigger, and
**bind (3)** now, so that whenever (2) is revisited the guardrail is already settled rather than
re-litigated by whoever implements it.

---

## 4. Decision 1 — Mining source: label events as the index, comments as the payload

**The index is the timeline `loom:changes-requested` labeled event**, not the verdict comment and
not the `verdict-sha` marker. Measured recall: 100% vs. 50% (§2). This is not a tuning
preference — it is the difference between an index generated by GitHub from an API call and an
index generated by an agent remembering to append a string.

The full record for one rejection is assembled as:

| Field | Source | Reliability |
|---|---|---|
| That a rejection happened, and when | `issues/{n}/events`, `event=labeled`, `label.name=loom:changes-requested` | Server-generated; exact |
| The rejection's reasoning | Newest `issues/{n}/comments` entry strictly before that event, within 120s | 4/4 in the sample; bounded by `judge.md`'s `&&` chaining |
| The tree it was rendered against | `verdict=changes-requested` marker, when present | ~50% — **corroboration only, never a join key** |
| That a Doctor acted | Subsequent `+loom:treating` labeled event | Server-generated; exact |
| What the fix actually changed | Commits pushed between the `changes-requested` and the next `review-requested` event | Exact, via commit timestamps |
| That the fix was accepted | Subsequent `+loom:pr` | Server-generated; exact |

**Rejected alternatives.**

- *Marker-comment scan as the index* — what the issue sketched. Disqualified by the measured 50%
  recall: a miner that silently drops half its input produces confidently wrong frequency counts,
  which is worse than no counts at all.
- *`loom:changes-requested` as a label query* — cannot work retrospectively. Doctor removes the
  label on completion, so no merged PR still carries it; only the historical event survives.
- *Doctor commit messages* — `doctor.md`'s template is freeform ("Address review feedback" plus
  bullets) with no marker, and Doctor commits are squashed on merge. The commit *range* between
  two label events is exact; the commit *message* is not addressable.
- *A Judge-written append-only log*, mirroring `.loom/logs/guard-decisions.log`. Attractive —
  writing at event time makes reading free — but the log is host-local while the fleet is not, so
  a multi-host fleet would mine a partial record with no way to know it was partial. The forge is
  the only place every host already agrees on.

**Cost.** Because merged PRs no longer carry the label, indexing is one REST call per PR scanned:
≈61/day, ≈430 for a weekly window. Against the 5000/hr REST pool that is affordable but not free,
and it is pure overhead in the ~96% of cases where the answer is "no rejection here."

---

## 5. Decision 2 — Cadence and owner: Auditor's existing tick, bounded window

**If this is ever built, the owner is Auditor** — not a new role, and not a new cron workflow.

Loom already runs a structurally identical mechanism, and it is Auditor's: the **Guard-Decision
Telemetry Review** standing policy (`.loom/roles/auditor.md`, #3898). Its shape maps one-to-one
onto what is proposed here — mine a durable record of friction each tick, dedupe and rank by
frequency, file **one issue per distinct trigger** after a `check-duplicate.sh` pass, never
self-apply `loom:issue`, and never weaken a safety floor while refining false positives. A
rejection-pattern review is the same policy pointed at a different log. Reusing it means the
implementation cost is a role-prompt section, not new infrastructure.

**Cadence: per Auditor tick, over rejection events since the last pass.** Not "the last N PRs" —
a fixed-N window silently changes its time span as fleet throughput moves, and throughput here is
both high and variable.

**Rejected owners.**

| Candidate | Why not |
|---|---|
| A new dedicated cron role | A new role for ~17 events/week is disproportionate; `.github/workflows/loom-*.yml` and the daemon role runner would both need a new entry, for a pass that reads less data than Auditor already reads |
| Judge | Judge is the *subject* of the mining. A role proposing edits to its own prompt based on its own verdicts has no independent check in the loop |
| Guide | Owns documentation maintenance (`WORK_LOG`/`WORK_PLAN`/README), not process-quality findings; the output here is issues, which is Auditor's product |
| Champion | Merge-path role; adding a mining pass to it puts latency on the critical path |
| The daemon (Rust) | Classification is a judgement task. Only the cheap indexing half is mechanical, and that half is not the expensive half |

**On acceptance-tracking state** (the issue's open question about an equivalent to
`~/.atomic/retro-runs/*.json`): **no new state store.** The forge already is one. A filed proposal
issue is the record that a finding was raised; its closing PR is the record that the change
landed; `git log` on the role prompt is the record that it stuck. Introducing a per-host JSON to
track this would be exactly the local-coordination-state anti-pattern that #5844 §6 rejected for
the Realm wiki — invisible to every label, webhook, and PR the rest of the fleet reads from.

---

## 6. Decision 3 — The write guardrail (binding on any future implementation)

**Every change this mechanism proposes ships as an ordinary issue → PR → Judge review. There is
no direct-write path to `.loom/roles/*.md`, `CLAUDE.md`, or `.github/labels.yml`, at any
confidence score, under any configuration, ever.**

This is not a default to be relaxed by a config flag or an autonomy setting. It is the condition
under which the mechanism is permitted to exist at all.

**Tied to the specific risk upstream flags.** atomic-claude reached the same conclusion for the
same reason and enforced it in its own design: `/retrospective-learning` auto-applies **nothing**.
Findings are walked one at a time through `AskUserQuestion` with `Accept / Modify / Skip`, and a
later run audits whether an accepted change actually landed and stuck (as recorded in
`docs/research/atomic-claude-evaluation.md` §4). #5844's own initial read named the hazard
directly — *automated rewriting of role prompts needs strong guardrails* — and #5850 restates it
as an explicit non-goal. The hazard is not hypothetical and not about file permissions: **role
prompts are Loom's control surface.** An erroneous edit to `judge.md` does not break a build; it
changes how every subsequent review is conducted, silently, fleet-wide, until someone notices.
Upstream's answer is a human gate per item. Loom's equivalent gate is the review lifecycle, and it
is strictly stronger than upstream's — Curator, Champion promotion, Judge, and a human all sit
between a proposal and a merge, whereas upstream has one human answering one prompt.

Concretely, any implementation MUST:

1. **File issues, never commits.** The pass's output is `./.loom/scripts/create-issue.sh`, not an
   `Edit` of a role prompt. It enters intake at `loom:triage` (or `loom:auditor` for Champion
   evaluation) and **never self-applies `loom:issue`** — promotion ownership is Curator/Champion's
   per `.loom/roles/curator.md`.
2. **Dedupe before filing**, via `./.loom/scripts/check-duplicate.sh`, exactly as the
   guard-decision policy already requires. A mining pass that re-files the same finding every tick
   is a spam generator.
3. **Require ≥3 independent instances** before proposing any control-surface edit. Two occurrences
   is a coincidence; one is an anecdote. Every proposal must cite the specific PR numbers.
4. **Respect the bloat budget — and be allowed to propose deletions.** This clause exists because
   it is the one an obvious port would drop. Upstream's pass audits installed config for
   *staleness, bloat, and contradiction* as well as gaps; a Loom version that could only ever
   *add* a rule would monotonically grow precisely the files Loom already rations —
   `CLAUDE.md` sits at **320/320 lines** against `scripts/check-claude-md-budget.sh` (zero
   headroom, measured at `090d15e0`), `judge.md` is 2241 lines and `doctor.md` 1472. A proposal
   that adds to a prompt must say what it displaces, and "delete a rule that no longer earns its
   lines" must be an available verdict.
5. **Never propose relaxing a safety rule.** Same floor the guard-decision policy sets: refine
   false positives, never weaken a real guard, label invariant, or lifecycle gate.

**None of the above authorises anything on its own.** This document proposes no code and no role
change; it constrains a future implementation should one ever be approved.

---

## 7. Decision 4 — Build now, or park: **park, with a measurable trigger**

**Recommendation: do not build the automated mining pass now.** Adopt the manual-audit
alternative in §8 instead, and revisit against the trigger below.

Three independent reasons, in order of weight.

**7.1 At this volume, automation buys no coverage.** The whole argument for a mining pass is that
the record is too large for a human to read. It is not: ~2.4 rejection events/day, ~17/week, each
a single comment. A periodic reader can review **100% of them**. Automation would be justified if
it surfaced something a full read misses — but a full read is exactly what is on offer at this
scale. The mechanism's value is inversely proportional to how tractable the input is, and this
input is very tractable.

**7.2 The recurrence premise is unverified.** A pattern-miner is only worth building if patterns
recur. In the four-rejection sample, **no root cause appeared twice**, and only two of four were
generalisable to a process pattern at all — the other two were code-specific findings no prompt
edit would have prevented. Building a frequency-ranking mechanism before establishing that
frequencies exceed one is building for an assumption. The manual pass in §8 tests that assumption
at a fraction of the cost, and produces the exact data needed to decide.

**7.3 The self-improvement loop is not input-starved; it is already the busiest thing in the
repo.** In the seven days to 2026-08-09, **60 of 500 commits (12%) touched a role prompt** under
`defaults/.claude/commands/loom/` — ≈8.5/day, against ≈2.4 rejections/day. Role-prompt refinement
already runs at roughly **3.5× the rate of the signal this mechanism would mine**, driven by
Auditor, Hermit, Architect, and human observation. Adding a fourth proposal source to a control
surface being edited eight times a day, whose largest prompt is 2241 lines and whose `CLAUDE.md`
has zero budget headroom, has a plausible negative expected value: the marginal proposal is more
likely to add lines than to add insight.

```bash
git log --since=2026-08-02 --oneline -- defaults/.claude/commands/loom/ | wc -l   # 60
git log --since=2026-08-02 --oneline | wc -l                                      # 500
```

**Not reasons to park.** Two things that might look like blockers are not, and should not be
cited as such later: the 50% marker recall (§2) is routed around entirely by indexing on label
events, and the API cost (§4) is affordable. The case for parking is about *value*, not
feasibility — this is buildable today, it is simply not yet worth building.

### Re-open trigger

Revisit this decision when **either** condition holds — both are one command to check, so this is
a falsifiable trigger, not an indefinite deferral:

- **Volume**: `loom:changes-requested` labeled events exceed **~50/week** (≈7/day, roughly 3× the
  current rate) — the point at which reading every rejection stops fitting in an Auditor tick.
- **Recurrence**: the manual pass in §8 records **the same root cause three or more times inside
  one month**. That is direct evidence of the premise in 7.2, and it flips the decision on its
  own regardless of volume.

Until one of those fires, the honest answer is that the mechanism would be machinery in search of
a corpus.

---

## 8. The alternative that is recommended instead: a bounded rejection review

Concretely, what §7 recommends adopting — specified here so the "park" verdict is actionable
rather than a refusal. **This document does not implement it**; it is proposed as a single
follow-up issue.

**Shape:** a short standing-policy section in `.loom/roles/auditor.md`, immediately alongside the
Guard-Decision Telemetry Review it is modelled on, reusing that policy's dedupe rules, label
discipline, and safety floor verbatim rather than restating them.

**Per Auditor tick:**

1. List `loom:changes-requested` labeled events since the last pass (§4's index; bounded by time,
   not by PR count).
2. For each, read the joined verdict comment.
3. Classify it as either a **code-specific finding** (no action — the review worked) or a
   **process pattern** (a class of mistake a role prompt could prevent).
4. Keep a running tally of process patterns in the pass's own issue/comment trail — the forge is
   the state store (§5), no new file.
5. File a proposal **only** when the same pattern reaches three independent instances, subject to
   every clause of §6.

**Why this is the right size.** It costs one prompt section rather than a mechanism; it reads the
same data the automated version would; it produces the recurrence evidence §7.2 says is missing;
and if that evidence arrives, it converts directly into the automated pass's specification —
because the classification step it performs by judgement is precisely the step a miner would have
to perform by model.

**Its honest weakness**, stated so a later reviewer does not have to discover it: a
judgement-based tally across independent Auditor invocations is only as consistent as the
classification, and Loom's Auditor passes are themselves cold-start. It will under-count patterns
that a single reader would have spotted. That is acceptable at ~17 events/week, and it is the
argument that flips if the volume trigger in §7 fires.

---

## 9. Risks of the parked design (recorded for whoever revisits it)

| Risk | Why it matters | Mitigation if built |
|---|---|---|
| **Prompt bloat spiral** | The output is "add a rule"; the input is "rules were not followed". Left unchecked this grows the exact files Loom rations, and longer prompts are followed *less* reliably — the mechanism degrades the thing it is trying to fix | §6 clause 4: additions must name what they displace; deletion is a valid verdict |
| **Overfitting to one PR** | A vivid single rejection reads like a systemic pattern | §6 clause 3: ≥3 cited instances |
| **Judge self-reinforcement** | Mining Judge's verdicts to edit Judge's prompt can entrench a reviewer idiosyncrasy as policy | §5: Auditor owns it, not Judge; §6: Curator/Champion/Judge/human all gate the result |
| **Misattributed cause** | The label record shows a rejection happened, not *why the Builder erred*. A prompt edit aimed at the wrong cause is a permanent tax on every future dispatch | Require the proposal to cite the verdict text, not just the event |
| **Silent index drift** | If `judge.md` ever stops writing `loom:changes-requested` (a renamed label, a new verdict path), the miner reports zero and looks healthy | Any implementation should assert non-zero events over a window it knows had rejections |

---

## 10. What this document deliberately does not do

- Does not implement a mining pass, a script, or a role-prompt edit. The diff is this one file.
- Does not authorise any agent to edit `.loom/roles/*.md`, `CLAUDE.md`, or `.github/labels.yml`
  outside the normal review lifecycle — §6 tightens that non-goal rather than relaxing it.
- Does not file the marker-recall observation in §2 as a bug: with two misses on the day after
  rollout and two hits the day after that, the evidence is consistent with rollout lag, and
  filing on a four-event sample would be exactly the overfitting §9 warns about. A future
  measurement over a wider window can settle it against #5686's guard.
- Does not evaluate the other atomic-claude ideas (#5847–#5849, #5851 track those separately).
- Does not re-derive whether Loom's rejection *rate* is healthy. 4% is reported here as an input
  to a sizing decision, not assessed as good or bad.
