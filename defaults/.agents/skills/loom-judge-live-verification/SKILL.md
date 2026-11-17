---
name: loom-judge-live-verification
description: "**Load when**: trigger (A): the diff drives live behavior (browser-driving, scraper, DOM-parsing code); or trigger (B): the linked issue reports an \"X was silently dropped / missed / not observed\" failure (#6883)."
---
<!-- loom-managed-skill -->
<!-- GENERATED FILE — DO NOT EDIT DIRECTLY.
     Produced by `loom-daemon generate-agent-skills` from
     defaults/.claude/commands/loom/judge-live-verification.md (the same source Claude Code
     reads as /loom:judge-live-verification via .claude/commands/loom/). This is the
     cross-vendor skill-discovery surface (.agents/skills/<name>/SKILL.md)
     read natively by Codex, Kimi Code, Mistral Vibe, and Grok — see
     runtime-adapters.md §5. To change this file, edit the source above
     and re-run the generator; CI (`loom-daemon generate-agent-skills
     --check`) fails if this file is stale. -->

# Judge: Live Verification and the Circular-Fixture Smell

**Load when**: trigger (A): the diff drives live behavior (browser-driving, scraper, DOM-parsing code); or trigger (B): the linked issue reports an "X was silently dropped / missed / not observed" failure (#6883).

On-demand sub-skill of `judge.md` (split out in issue #13957): the core workflow stays in `judge.md`, which points here by the trigger above. Section references to the main workflow live in `judge.md`; existing split-outs `judge-reference.md` and `judge-ci-red.md` work the same way.

### Live Verification and the Circular-Fixture Smell

**Scope — read this first. Two independent triggers; either one puts a PR in
scope.**

**Trigger (A) — the diff drives live behavior.** A diff that drives a real
browser, scrapes a remote page, or parses DOM/HTML the project does not itself
produce (Puppeteer/Playwright/CDP drivers, `fetch` + HTML parsers,
catalogue/listing scrapers, selector-based row readers). It does **not** apply to
ordinary UI work, to a PR that merely renders markup the repo owns, or to
anything else.

**Trigger (B) — the PR's linked issue describes a silent omission (#6883).**
Regardless of what the diff touches, this subsection also applies when the issue
the PR closes reports an "X was silently dropped / missed / not observed"
failure. Read the linked issue (`gh pr view <number> --json
closingIssuesReferences`, then `gh issue view <N> --json body`) and treat it as
this shape when its body contains one of these instruction-shaped fragments,
case-insensitively — the same "fragments, not bare words" discipline the
operator-gate scan uses (`sweep.md` → "Operator-gate advisory scan"):

`silently dropped`, `silently missed`, `silently skipped`, `silently discarded`,
`silently ignored`, `silently filtered`, `failed silently`, `fails silently`,
`nothing errored`, `no error was raised`, `never appeared`, `was not observed`,
`went unnoticed`, `failure is invisible`, `invisible failure`.

(Deliberately not the bare words `silent`, `dropped`, `missed`, or `invisible` —
each is ordinary engineering prose on its own.)

**Why the shape matters, not just the code.** For a silent-omission bug the thing
to verify is an **absence**, and an absence is exactly what a unit test cannot
distinguish from a correctly-passing filter. A fix for one rests on an assumption
about what the external source actually contains; a fixture that supplies that
source's payload *by hand* encodes the same assumption a second time and then
confirms it against itself. The fix and its test are internally consistent and
wrong in the same direction. Only contact with the real source separates them —
which is why this subsection's blocking posture, proven on trigger (A), belongs
to trigger (B) too.

**If NEITHER trigger fires**, the normal non-blocking posture above is
unchanged — do not invoke this subsection.

**The rule.** For an in-scope PR, "manually tested in browser" (or any
equivalent unverified assertion of live behavior) is **not sufficient on its
own** for approval. Approval requires the PR to demonstrate **one** of:

- **(a) A live-response fixture parsed by the real page parser.** The fixture
  is captured from an actual live response, and the **page-parsing side of any
  merge/join is produced by running the real page-parsing function**
  (`readFilmRow`-style — the same function that runs in production) over that
  captured response. It must not be synthesized by hand, or derived from the
  same saved payload that produced the expected/data side of the merge.
- **(b) A recorded live run** performed by whoever holds the shared resource /
  mutex (the debug browser, the session, the rate-limited endpoint), with the
  run's output pasted into the PR or a link to it attached. A claim that a
  live run happened, with no output and no link, does not satisfy this.

**Reading (a) under trigger (B).** For a PR in scope only via its linked issue's
failure shape, "the real page parser" generalizes to *the real production code
path that consumes the external source* — the parser, filter, or mapper that
actually runs — over a **captured real response** from that source. The invariant
is identical to trigger (A)'s: the source-side data in the fixture must have come
from the source, not from the author.

**Named review smell: "circular fixture."** A test whose external-source data
never actually comes from the external source. It has two forms, and **both are
blocking**:

- **The merge/join form** (trigger A's original shape). Both sides of a merge,
  join, or comparison built from **one** saved payload — so the parser is only
  ever checked against data that already agrees with it, and never meets the
  page's own output. It proves the two halves of the test agree; it proves
  nothing about whether the code reads the real page.
- **The fabricated-payload form** (#6883, reachable under either trigger). The
  fixture's external-source side is **hand-synthesized or invented** — written to
  match what the author believes the source returns — rather than captured from a
  real response, in a case where capturing one was available. It asserts on the
  data we *wished* the source carried. This is the form that shipped the #6883
  incident: the suite was green, the source did not carry that shape, and the
  item kept being dropped.

**Cite it by name ("circular fixture") in your evaluation comment** so the smell
stays greppable across reviews, and say which form you found.

**Trigger (A)'s posture is unchanged by (B).** A browser-driving / scraper /
DOM-parsing PR is in scope exactly as it was before #6883 — same evidence
requirement (a)/(b), same blocking treatment of a circular fixture — whether or
not its linked issue happens to use silent-omission phrasing. Trigger (B) only
**adds** an entry path; it removes nothing.

**Verify it, or block on it — do not file it as a non-blocking follow-up**
(the same imperative the Performance section applies to N-bound build code). A
"worth a live check later" note in a Judge review is exactly how a PR ships
three separate live defects at once.

**Precedent (why this is blocking).** On rjwalters/walters-family-tree, PR
#371 (a browser-driving catalogue scraper) was built and Judged **offline
only** — fixture self-tests plus a check against a saved payload — because
live verification needed a shared debug Chrome behind a mutex. Every fixture
was circular. The first live run surfaced **three** defects, each needing its
own fix round: a settle that fired before the table hydrated (0 rows read); a
hard-coded `catalogs[0]` picking the wrong catalogue while a mirrored table
double-counted rows; and a zero-padded page ID (`007664503`) that never
matched the unpadded data ID (`7664503`), so the merge silently produced
nothing. All three were reachable only by running the real parser against real
page output.

**Second precedent (trigger B, #6883).** An input filter silently dropped items
that should have reached the downstream pipeline. Nothing errored — a filtered
run and a genuinely empty one looked identical. The PR's regression test
fabricated the source payload the fix assumed; unit tests passed, Judge approved,
Champion merged and auto-closed the issue. The fix rested on a wrong assumption
about what the source returns and the item kept being dropped on the very next
run. The diff touched no browser and no DOM parser, so trigger (A) never fired —
that gap is exactly what trigger (B) closes.

**What to do when the evidence is absent.** Request changes and name which of
(a) or (b) you need. If the Builder states live verification was impossible in
their environment (see `builder.md` → "Live Verification You Cannot Perform"),
that disclosure is the honest, expected path — but it does **not** convert an
unverified live-behavior acceptance criterion into a met one. Either hold the
PR for the resource holder's recorded run, or, if the operator elects to land
it before live verification, require that a live-verification tracking issue
is filed and linked from the PR body before you approve, and say plainly in
your verdict that the live behavior is unverified.

**When live verification WAS performed, record it as evidence (#6883).** If you
executed evidence form (b) yourself, or read a recorded live run that satisfies
the linked issue's out-of-band acceptance criterion, post a comment saying what
was run and what was observed, ending with

```
<!-- loom:ac-verified sha=$VERDICT_SHA -->
```

This is a **distinct marker from `loom:verdict-sha`** and answers a different
question — not "which tree does this verdict describe" but "was the out-of-band
step actually performed, and against which tree". Champion reads it in Step 4 and
will otherwise hold the issue open after merge rather than let it close silently
(`champion-pr-merge.md` → "Out-of-Band Acceptance-Criteria Gate"). It may be
posted on the PR or on the issue; it may sit in the same comment as a verdict or
in its own. **Only stamp it for a step you personally performed or personally
read the output of** — a marker stamped on someone's unevidenced assertion
restores exactly the silent close it exists to prevent.
