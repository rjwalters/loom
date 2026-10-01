# Latency vs. Reliability: What Loom Optimizes For

*Loom's gates are priced in human-attention units. When attention is free, they are a bargain. When attention is abundant, they are a tax.*

---

## The Thing Loom Is Actually For

Read the README quickly and you will come away with the wrong idea. "AI agents claim
issues, implement features, review PRs, and merge code" sounds like a productivity
pitch — a machine for getting code written faster than you could write it yourself.

That is not the design target. Loom optimizes for **unattended correctness over long
horizons**. The question it is built to answer is not "how fast can I get a working
thing?" but "can I leave this running for eight hours, or a weekend, or a week, and
find a `main` branch I still trust?"

Those are different problems, and they want different machines. Time-to-first-working-thing
rewards skipping steps, because a human is standing right there to notice when a
skipped step mattered. Unattended correctness rewards the opposite: every step that
can catch a defect *before* it propagates is worth taking, because there is nobody
around to catch it afterward.

Loom picks the second. Deliberately, and at every layer.

---

## Gates Are Priced in Human-Attention Units

Here is the load-bearing idea, and everything else in this essay follows from it.

Each gate in the Loom pipeline buys a guarantee, and each guarantee is **priced in the
human attention it replaces**. A Judge pass is not inherently valuable; it is valuable
exactly insofar as no human was going to read that diff. A Curator pass is not
inherently valuable; it is valuable exactly insofar as no human was going to notice
that the issue was underspecified before a Builder spent forty minutes implementing the
wrong thing.

So the real cost calculation for any gate is:

> **cost** = the latency it adds
> **benefit** = the defect it catches × the cost of that defect going unnoticed

The second term is the one that moves. In unattended operation, a bad merge at 3am
doesn't cost ten minutes — it costs every sweep that starts against the broken `main`
for the rest of the night, each one dispatching an agent that burns tokens
investigating a failure that isn't its own. Ten minutes of Judge review against
multiple hours of poisoned downstream work is not a close call.

Flip the attention term to zero cost — put a human in front of the screen, watching —
and the same arithmetic produces the opposite answer. The defect still gets caught; it
gets caught in seconds, by the human, for free. The gate is now buying insurance
against a risk that is already fully covered. It is pure latency with nothing bought in
return.

This is why "Loom felt slow" and "Loom is working as designed" are both true
statements, and why they are not in tension.

---

## What Each Gate Buys

None of these exist because thoroughness is a virtue. Each one exists because a
specific thing goes wrong when a pipeline runs unsupervised.

**Curator** — *exists because an underspecified issue costs a full Builder cycle
overnight.* A human reading a vague issue asks a clarifying question. An agent reading a
vague issue confidently implements one of the several things it might have meant, and
you find out at review time, hours later, with a PR that has to be thrown away.

**Judge** — *exists because nobody is reading the diff.* The review gate is not a
formality layered on top of a human review that was going to happen anyway. In
unattended operation it **is** the review. Remove it, and code reaches `main` having been
read by exactly one party: the agent that wrote it.

**Doctor** — *exists because a failing PR that nobody fixes becomes a permanent
blockage.* Interactively, a failing check is a prompt — you look at it and fix it.
Unattended, it is a dead end: the Builder has exited, the sweep has moved on, and the
PR sits there accumulating merge conflicts until someone intervenes. Doctor is the
someone.

**Champion merge-risk holds** — *exist because the expensive failure mode is not a bad
PR, it is a bad `main`.* Individual PRs are cheap to revert. A `main` branch that has
been broken for six hours, with a dozen sweeps having branched from it, is not. The
hold trades merge latency on one PR for the integrity of the base that everything else
builds on.

**`buildGate`** — *exists because "the agent said it worked" is not evidence.* It runs
the build and tests before a PR is opened, so a broken branch never enters the review
pipeline and never spends a Judge cycle discovering what a compiler would have said in
thirty seconds. See [`build-gate.md`](../../defaults/docs/build-gate.md).

**"CI is dumb and reliable, on purpose"** — *exists because a clever CI that is
occasionally wrong is worse than a slow CI that is always right.* When a human is
watching, an ambiguous CI result is a minor annoyance to be squinted at. When no human
is watching, it is a coin flip that something downstream will act on as though it were a
fact. The full reasoning, and the incidents that produced it, are in
[`ci-principles.md`](../../defaults/docs/ci-principles.md).

Read that list as a set of answers to "what went wrong overnight?" Because that is what
it is. Every gate is a scar.

---

## The Regime Where the Trade Is Bad

The trade is bad — genuinely, not apologetically — when all three of these hold:

1. **The deadline is short.** Hours, not days. Latency is the binding constraint, and
   anything that adds latency is spending the only resource you cannot replace.
2. **The artifact is disposable.** It will be demoed, judged, and abandoned. Nothing
   will be built on top of it next month. Technical debt has no time to come due.
3. **A human is watching continuously.** Defects surface in seconds because someone is
   looking at the screen when they happen.

Satisfy all three and every gate inverts. The latency is real; the guarantee is
redundant. You are paying full price for insurance on a risk you have already
self-covered by sitting there.

Note that this is a conjunction, not a menu. A short deadline on an artifact that will
be maintained for years does not put you in this regime — it just means you are in a
hurry, which is the normal condition of software development and not a license to
remove review. Disposability and continuous attention are what actually change the math.

---

## The Recipe for That Regime

If you are in that regime, do not use Loom in anger, and do not try to tune it into
something it isn't. Use a different method, stated here without hedging:

- **Work in live agent sessions, not dispatched sweeps.** A sweep is a bet that you will
  not be present to supervise; its entire structure is built around that bet. If you
  *are* present, pay attention directly. Keep the agent in a conversation where you see
  each step and can redirect mid-stream.
- **Write fewer tests.** Tests are a message to a future maintainer who does not exist
  here. Write the one or two that pin down something you cannot eyeball, and skip the
  rest.
- **Merge rapidly, skip the review round-trip.** You are the review, and you are
  reviewing continuously. A separate asynchronous review pass adds a round-trip and
  catches what you already saw.
- **Fix problems by over-patching, not by diagnosing root cause.** Root-cause analysis
  is an investment in not hitting the same bug again in three months. There is no three
  months. Patch the symptom, patch it broadly, move on.

Every one of those four is terrible practice in a codebase with a future. Every one of
them is correct when the artifact's lifetime is measured in hours. The difference is not
discipline or rigor — it is that the debt these shortcuts incur is always paid later,
and "later" is a thing that only exists for artifacts that survive. Shortcuts are not
cheating when there is no future self to cheat.

The symmetry is worth sitting with: the hackathon recipe is wrong everywhere else for
exactly the same reason Loom is wrong at the hackathon. Both are correct tools, each
mispriced in the other's regime.

---

## The Grounding Case: 2026-09-27

We used Loom at a hackathon on 2026-09-27, and it was the wrong tool.

Nothing malfunctioned. Curator enriched issues. Judge reviewed PRs. Champion held
merges it had been told to hold. Every gate did precisely the job it was designed to do,
and every gate was latency we could not afford. We were not fighting a bug; we were
fighting the design, by applying it in the one regime where its central trade does not
pay.

The useful output of that day was not a defect report. It was a **regime boundary** —
evidence that Loom's trade has an edge, located roughly where continuous human attention
begins. We had been operating as though the design were universally good rather than
specifically good, and the hackathon is where that assumption met a counterexample.

What was actually missing was this document. Nothing in the repository said "this tool
optimizes for unattended correctness, so if you have a human watching and four hours,
use something else." That silence is what turned a predictable mismatch into a surprise.
A surprise you can write down stops being a surprise and becomes a decision rule.

---

## Not a Promise of a Fast Mode

This essay is not a roadmap. There is no low-gate "fast path" being announced here, and
the absence of one is not an oversight awaiting a fix.

A reduced-gate mode might be worth building someday — the argument above sketches what
it would have to be for. But an honest statement of what a tool is for is more useful
right now than a vague commitment to someday being for something else. The gates are the
product. They are what makes it safe to dispatch work you will not watch, which is the
whole proposition.

Use Loom when you want to stop watching. Use something else when you cannot stop
watching anyway.
