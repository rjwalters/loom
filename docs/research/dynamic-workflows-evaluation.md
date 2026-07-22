# Evaluating Claude Code Dynamic Workflows for Loom's in-session orchestration

> Status: **exploration deliverable** (issue #3739). This document is an evaluation
> plus a design sketch. It does **not** rewrite the production sweep/judge path, and it
> files **no** GitHub issues — recommended follow-ups are *listed* at the end for a
> human/Curator to file. See "Scope and non-goals".

## TL;DR

- **Feasibility gate: PASS (capability present).** The Dynamic Workflows primitives
  (`agent()`, `parallel()`, `pipeline()`, nested `workflow()`, a shared `budget`, typed
  `args`, JSON-Schema-validated agent outputs, journal-based resume) are **shipped in the
  installed Claude Code CLI, v2.1.206** — confirmed by direct inspection of the CLI binary
  (see "Feasibility gate"). This is *not* a hypothetical future capability.
- **But the runnable prototype + measured comparison are DEFERRED** to a follow-up. The
  reason is not that the primitives are missing — they are present — but that the *only*
  legitimate way to exercise them here (a top-level Claude Code session invoking the
  `Workflow` tool) is not available to the Builder harness that produced this document (a
  `loom-builder` Task subagent, which does not expose the `Workflow` tool and could not
  drive it without violating the #3289 one-level-deep rule). Honest precision/recall/latency
  numbers require a top-level session and a fixed PR corpus; fabricating them here would be
  worse than deferring. **No measurement numbers are invented in this document.**
- **What ships under #3739:** this evaluation + a **design sketch** of the recommended
  first prototype (multi-dimension Judge fan-out + adversarial verify), gated behind an
  off-by-default `LOOM_JUDGE_FANOUT_EXPERIMENT` flag, kept strictly separate from the
  production judge path. See `defaults/scripts/experiments/`.
- **The load-bearing conclusion — the substrate boundary:** a Dynamic Workflow is an
  **in-session, single-token** construct. It can make the *read-heavy, one-level-deep*
  parts of Loom's orchestration deterministic and testable (review fan-out, triage
  classification, proposal analysis). It **cannot** replace `loom-daemon` +
  `spawn-claude.sh`, because multi-account token rotation requires a per-task **process**
  spawn, and in-process workflow agents inherit the parent session's single OAuth token.
  **A workflow is not a daemon replacement.**

---

## Scope and non-goals

Per the issue's Curator guardrails, this deliverable is deliberately bounded:

- **Does NOT** modify, fork, or wire anything into the production orchestration
  (`defaults/.claude/commands/loom/sweep.md`, `defaults/.claude/commands/loom/judge.md`,
  `defaults/roles/judge.md`). Those were read for reference only.
- **Does NOT** run `gh issue create` or decompose #3739 into sub-issues. The final
  acceptance-criteria bullet ("follow-up issues filed for surfaces recommended to proceed")
  is satisfied by the **listed** recommendations in "Recommended follow-ups (to be filed by
  a human/Curator)" below — not by autonomous filing (which would itself risk the #3707
  issue-number race the issue flags as a hazard).
- **Stays one level deep (#3289)** and read-only with respect to other work's labels/PRs.
- The prototype is a **review-quality experiment**, never a production judge run: it merges
  nothing, applies no `loom:pr` / `loom:changes-requested` transitions, and creates no
  issues.

---

## Feasibility gate

The issue's Curator notes require confirming the primitives actually exist in the
currently-installed `claude` before designing against them, with two sanctioned outcomes
(available → full scope; not available → reduced v1 = evaluation + design sketch + explicit
deferral). The reality found here is a **third, sharper** case, documented honestly below.

### What was checked

Installed CLI: **`claude` v2.1.206** (`/opt/homebrew/Caskroom/claude-code/2.1.206/claude`,
a single compiled binary). The binary's embedded strings were inspected for the workflow
runtime and its documented tool surface. The following were **positively confirmed present**
(quotations are from the binary's own embedded tool documentation and runtime code):

| Primitive | Confirmed signature / behavior (from CLI 2.1.206) |
|---|---|
| `agent(prompt, opts?)` | "spawn a subagent. Without schema, returns its final text as a string. With schema (a JSON Schema), the subagent is forced to call a StructuredOutput tool and `agent()` returns the validated object — no parsing needed. Returns `null` if the user skips the agent mid-run or the subagent dies on a terminal API error after retries (filter with `.filter(Boolean)`)." `opts` includes `label`, `phase`, `schema`, `model`, `effort` (`'low'|'medium'|'high'|'xhigh'|'max'`), `isolation: 'worktree'`, `agentType`. |
| `parallel(thunks)` | "run tasks concurrently. This is a BARRIER: awaits all thunks before returning." A thunk that throws resolves to an error sentinel rather than rejecting the batch. |
| `pipeline(items, stage1, stage2, …)` | "run each item through all stages independently, NO barrier between stages … Wall-clock = slowest single-item chain, not sum-of-slowest-per-stage." Each stage callback receives `(prevResult, originalItem, index)`. |
| `workflow(name \| {scriptPath})` | Invokes a named/bundled child workflow — **and enforces one-level nesting itself**: calling `workflow()` from inside a child workflow throws *"workflow() cannot be called from within a child workflow — nesting is limited to one level. Inline the inner script or call its agents directly."* |
| `budget` | `{total: number\|null, spent(): number, remaining(): number}` — "a HARD ceiling, not advisory: once `spent()` reaches `total`, further `agent()` calls throw." The pool is **shared** across the main loop and all workflows, not per-workflow. |
| `args` | Typed workflow input, passed verbatim as a real JSON value. |
| Schema-validated output | Passing `opts.schema` (a JSON Schema) forces a `StructuredOutput` tool call and returns the validated object. |
| Native resume | Journal-backed: `Workflow({scriptPath, resumeFromRunId: "…"})`; the tool "automatically persists its script to a file under the session directory and returns the path in the tool result." |
| Discovery | Workflow `.js` files with a meta header (`name`, `description`, `whenToUse`, `phases`) are discovered from `workflows/` directories under user/project settings. |

Two findings that **correct or sharpen the issue's assumptions**:

1. **There is no dedicated `verify()` primitive.** The workflow VM binds exactly four
   control-flow globals: `agent`, `parallel`, `pipeline`, `workflow`. The "`verify()` /
   adversarial pass" the issue lists as a first-class primitive is, in the shipped API, a
   **pattern composed from `agent()` + a schema** (an adversarial reviewer agent that takes
   the raw findings + the diff and returns a typed, filtered set) — not a built-in. The
   design sketch reflects this: verify is an `agent()` call, not a `verify()` call.
2. **`agent()` exposes `effort` directly.** The issue's Risks section assumes the #3705
   effort-plumbing degradation ("`@effort` rungs stay degraded unless the workflow spawns a
   process") carries over. It does **not** fully: an in-process `agent()` call accepts
   `opts.effort` (`'low'…'max'`), so the effort dimension is *recoverable* inside a
   workflow in a way it is not through the bare Task tool. What a workflow still **cannot**
   do is multi-account **model/token rotation** — that remains a process-spawn concern (see
   substrate boundary). So #3705 is *partially mitigated* for the in-session path, not
   fixed at the substrate level.

### Feasibility outcome for #3739

- **Capability: available.** The primitives are shipped and usable from a top-level Claude
  Code session in this environment.
- **Runnable prototype in this PR: deferred — honestly.** The Builder that produced this
  artifact is a Task subagent. It does not have the `Workflow` tool, and even if it did,
  invoking it would spawn workflow agents at a *second* nesting level (subagent →
  workflow-agent), which is exactly the #3289 stall the issue forbids. A truthful
  measured comparison also needs a fixed corpus of already-merged/rejected PRs run under a
  top-level session with token accounting — out of a single subagent's reach.
- **Therefore this PR ships the sanctioned reduced-v1 shape** (evaluation + design sketch +
  explicit deferral), but for a more precise reason than "primitives absent": *primitives
  present, harness cannot honestly exercise them one level deeper.* The runnable prototype
  and its measurement are a named follow-up below.

---

## Capability → need mapping

How each shipped primitive maps to a concrete pain point in Loom's current prose-driven
orchestration (the "control flow lives in Markdown the model re-executes every run"
problem):

| Dynamic Workflow capability | Loom need it addresses | Today (prose) | With a workflow |
|---|---|---|---|
| `pipeline()` sequential/independent composition | Per-issue lifecycle: curator → approve-gate → builder → judge → doctor-loop → merge | `sweep.md` prose the orchestrator LLM must execute correctly every invocation | Deterministic stage graph; per-item progress without a barrier |
| `parallel()` fan-out (barrier) | Builder fan-out within a wave; multi-dimension review; multi-domain proposal analysis | Serialized, or "defer parallel to a future issue" | One concurrent barrier'd batch, bounded by the harness concurrency cap |
| Schema-validated `agent()` output | PR-number extraction; Judge verdict parsing | `sed -n 's/.*pr_number.*'` over checkpoint JSON; verdicts parsed from free-text comments + labels | Typed object, no regex, no free-text parsing |
| `agent()` + schema as an adversarial "verify" | Drop findings the diff doesn't support (fan-out-then-verify, as the repo's `deep-research` skill already ships) | Single Opus pass emits unverified nits | A second typed agent filters low-support findings |
| Journal-based resume | Mid-phase-death recovery | Hand-rolled `sweep-checkpoint.sh` phase enum (`.loom/sweep-checkpoint/issue-N.json`) | Native resume — **but** interop with the existing checkpoint schema must be preserved (see Risks) |
| `budget` (shared hard ceiling) | Cap large fan-outs; bound the doctor→judge loop | Attempt counter the model tracks by hand across kills | `while (budget.remaining() > X)`; provably bounded loop |
| Ordinary JS arithmetic | Model-precedence chain; escalation ladder `ladder[min(attempt-1, len-1)]`; doctor-cycle cap; routing tables | Arithmetic-in-prose the model must get right every time | Plain, unit-testable code |

The through-line: Loom already hand-rolls a large amount of exactly this orchestration in
prose (most visibly the ~1600-line `sweep.md`). Every piece of arithmetic or routing that
lives in Markdown is a surface the model can drift on. Workflows move the *deterministic*
parts into deterministic code, leaving the model to do what only the model can (read a
diff, judge a design), which is the right division of labor.

---

## The substrate boundary (the load-bearing conclusion)

This is the single most important output of the evaluation, because it is where the
enthusiasm has to stop.

```
┌─────────────────────────────────────────────────────────────────────┐
│  IN-SESSION, SINGLE-TOKEN  (a Dynamic Workflow fits here)            │
│  ── read-heavy, one-level-deep fan-out ──                           │
│                                                                     │
│   • Multi-dimension Judge review of ONE PR (parallel + verify)      │
│   • Whole-backlog triage classification (pure-JS router)            │
│   • Architect/Hermit read-only analysis fan-out (writes serialized) │
│   • Model-cost experiment record-keeping (typed outputs)            │
│                                                                     │
│   Concurrency ceiling = harness cap min(16, cores-2); Loom's        │
│   subagent wave band is [3,6] (#3693). ONE OAuth token for all.     │
└─────────────────────────────────────────────────────────────────────┘
                    ▲  a workflow lives INSIDE one sweep child (Tier 1)
                    │
════════════════════╪══════════ SUBSTRATE BOUNDARY ═══════════════════════
                    │  crossing it requires a PROCESS spawn, not a subagent
                    ▼
┌─────────────────────────────────────────────────────────────────────┐
│  DETACHED-PROCESS, MULTI-ACCOUNT  (only loom-daemon fits here)       │
│  ── account-balanced building, up to ~10 concurrent ──             │
│                                                                     │
│   • spawn-claude.sh selects a token, exec's `claude` with           │
│     CLAUDE_CODE_OAUTH_TOKEN exported → per-child account rotation    │
│   • loom-daemon: registry, 30s reaper, tokio event bus (6 topics)   │
│   • Rate-limit recovery by rotating to a fresh account               │
│                                                                     │
│   A workflow's in-process agent() inherits the parent's ONE token — │
│   it CANNOT provide account-balanced concurrency. NOT a replacement.│
└─────────────────────────────────────────────────────────────────────┘
```

Why the boundary is hard, not soft:

- **Token rotation is a process-spawn property.** `spawn-claude.sh` picks a token from
  `.loom/tokens/` and `exec`s `claude` with `CLAUDE_CODE_OAUTH_TOKEN` set. An in-process
  workflow `agent()` runs in the *same* process and inherits the *same* single token —
  identically to a Task subagent. So a workflow tops out at the single-token subagent
  concurrency band (`[3,6]`, #3693), never the daemon's account-balanced ~10.
- **Two orchestration substrates is a real cost.** `loom-daemon` (Rust: registry, reaper,
  tokio broadcast bus, 6 frozen topics) is the load-bearing Tier-2 backend. A JS workflow
  runtime is a *second* dispatch substrate; letting it grow registry/reaper/dispatch logic
  would duplicate the daemon and risk the "do not write `.loom/daemon-state.json`"
  coexistence rule. The clean fit is a workflow **inside a single sweep child** (Tier 1),
  observed by the daemon through the existing checkpoint — never a daemon replacement.
- **Checkpoint interop is mandatory.** `.loom/sweep-checkpoint/issue-N.json` (#3373) is
  shared between the sweep skill and the daemon reaper/resume path. A workflow's native
  journal resume must stay **bidirectionally compatible** with that checkpoint, or the
  daemon can no longer observe/resume a workflow-driven sweep.

**Rule of thumb for any future adoption:** put a workflow *inside* a sweep child to make
that child's read-heavy fan-out deterministic; never let a workflow become the thing that
dispatches sweep children across accounts. That job stays on `loom-daemon` +
`spawn-claude.sh`.

---

## Keep / defer / reject — the five candidate surfaces

Each verdict is tied to the specific risk(s) that gate it: **nesting** (#3289), **token
rotation** (single-token substrate), **#3707 filing race**, **checkpoint interop** (#3373),
**effort degradation** (#3705).

| # | Surface | Verdict | Gating risk(s) | One-paragraph rationale |
|---|---|---|---|---|
| 1 | **Judge phase** — multi-dimension review fan-out + adversarial verify (`defaults/roles/judge.md`, `sweep.md` step 5) | **KEEP** (prototype first) | nesting ✔ satisfied; token ✔ single-token-friendly; #3707 n/a (no filing); checkpoint n/a (no sweep-state write); effort ✔ recoverable via `agent(opts.effort)` | The best first target and the one this PR sketches. It is read-only, single-token-friendly, exactly one level deep (dispatch dimension reviewers directly — never a sub-workflow), creates no issues, and produces a clean typed verdict that would feed the doctor-cycle loop without label+comment string parsing. Fan-out across correctness / security-credential-surface / test-coverage / perf-simplification, then a typed adversarial `agent()` that drops findings the diff doesn't support, then a schema-validated reduce to approve/changes-requested. It exercises `parallel()` + schema + the verify pattern with zero exposure to the daemon, the filing race, or checkpoint interop. It **preserves** the No-Fable-Judge invariant and sequential-within-wave merge ordering: only the review of *one* PR is parallelized. |
| 2 | **Doctor→Judge escalation loop** — bounded rejection loop (`sweep.md` steps 6/C1b, `defaults/roles/doctor.md`) | **DEFER** (after #1 lands) | checkpoint interop (primary); nesting; effort | This is the most arithmetic-heavy prose in the skill — attempt counter across kills, ladder indexing `ladder[min(attempt-1,len-1)]`, the single-use distinct-defect grace cycle, "refusal must not eat the cap". A `budget`-bounded loop with a typed verdict is a genuinely good fit and would make the loop provably bounded and resume-safe. It is deferred, not kept, purely because it **writes sweep state**: the loop's resume must stay bidirectionally compatible with `.loom/sweep-checkpoint/issue-N.json` (#3373), which the daemon reaper also reads. That interop contract has to be designed and tested before this can be trusted, and it should build on the typed-verdict shape proven by #1. Effort rungs are recoverable via `agent(opts.effort)`, but model-tier rotation on escalation still needs care (single-token substrate). |
| 3 | **Whole-backlog triage** — `/sweep all` routing classifier (`defaults/roles/guide.md`) | **DEFER** (strong, independent) | token rotation (for the *build* legs); #3707 (if it fans out issue-creating classes); nesting | The routing taxonomy (loom:issue→build, loom:curated→promote, uncurated→curate, stale loom:building→reclaim, loom:blocked→probe, loom:epic→fan-out, has-open-PR→judge/merge, loom:operator-only→skip) is already fully deterministic prose — an ideal pure-JS classifier that yields schema-checked routing, a reproducible dry-run plan, and a hard budget, while keeping the mandatory confirmation gate and the loom:operator-only hard exclusion. It is deferred rather than kept because the *classifier* is in-session-safe but the *building* legs it routes to must still cross the substrate boundary to `loom-daemon` for multi-account concurrency, and any class that fans out issue creation (epic fan-out) hits #3707. Right shape: workflow classifies + plans (in-session), daemon builds (multi-account). Sequence it after #1 proves the typed-output ergonomics. |
| 4 | **Architect / Hermit proposal generation** — fan-out reads, serialize writes (`defaults/roles/architect.md`, `hermit.md`, +#3707) | **DEFER** (blocked on #3707) | #3707 filing race (primary); nesting | A workflow resolves the #3707 tension *structurally*: `parallel()` the read-only analysis agents (architecture, dedup, test-gaps, deps, security, simplification), then a **single serialized** reduce that files via `gh issue create` one at a time behind a filing lock — "fan out reads, serialize writes" becomes an enforced control-flow boundary instead of a documented convention. This is architecturally attractive, but it is deferred because it is *gated on* #3707's resolution: the whole value proposition is disciplining concurrent issue creation, and #3739 itself is explicitly forbidden from filing issues, so this cannot be prototyped end-to-end here. Keep it queued behind #3707 and behind the typed-output patterns from #1. |
| 5 | **Model-cost A/B experiment (#3725)** — fold arm assignment + per-phase record into the workflow (`defaults/scripts/sweep-experiment.sh`, `sweep-model-stats.jsonl`) | **REJECT** (for now) | checkpoint interop; token fidelity | The #3725 machinery already lives in Python (`sweep_experiment`) precisely so arm assignment is byte-for-byte deterministic and resume-safe, and its cost signal is recovered at harvest time from per-subagent transcripts — a path that a workflow's typed output does **not** improve and might *degrade* (the workflow does not surface per-subagent `usage` at the Task boundary any better than today). Folding arm/record-keeping into a workflow would re-implement working, tested, resume-safe code with no measurable win and new checkpoint-interop surface. Reject unless/until a workflow is *already* driving the sweep for other reasons (#2), at which point the records "fall out of control flow" for free — revisit only then. |

Summary: **1 keep (prototype), 3 defer (sequenced), 1 reject.** Nothing lands in production
under #3739.

---

## The design sketch (recommended prototype #1)

A runnable version is deferred (see Feasibility gate). The design is captured as an
**inert, off-by-default** artifact so a future top-level session can pick it up:

- `defaults/scripts/experiments/judge-fanout-workflow.js` — the workflow script itself,
  written against the confirmed CLI 2.1.206 API (`parallel()` of dimension reviewers →
  adversarial `agent()`-with-schema verify → typed reduce). It is a **design sketch**: it
  is not wired into `sweep.md`/`judge.md`, it lives outside any auto-discovered `workflows/`
  directory, and it is documented as read-only / one-level-deep / creates-no-issues.
- `defaults/scripts/experiments/judge-fanout-experiment.sh` — an off-by-default gate/runner.
  Without `LOOM_JUDGE_FANOUT_EXPERIMENT=1` it is a **no-op** that prints how to run the
  experiment and exits 0 (this is the smoke test: flag off ⇒ nothing happens). With the
  flag set it syntax-checks the sketch and explains the top-level-session invocation path —
  it deliberately does **not** attempt to dispatch a live judge run from a subagent context.
- `defaults/scripts/experiments/README.md` — what these files are, the flag contract, the
  precedent they follow (`LOOM_MODEL_EXPERIMENT` / `sweep.modelExperiment`, off by default,
  loud banner), and the explicit deferral.

### Flag contract (follows the `LOOM_MODEL_EXPERIMENT` precedent)

- **Off by default.** No env var, no behavior. Byte-for-byte unchanged production path.
- `LOOM_JUDGE_FANOUT_EXPERIMENT=1` opts in to the *experiment tooling* only (syntax-check +
  guidance). It never touches production judge code, never applies labels, never merges.
- The flag is an in-session experiment switch, exactly like `sweep.modelExperiment`'s
  `observe`/`experiment` modes are off-by-default and loudly banner'd.

### Sketch shape (pseudocode, mirrors the shipped `.js`)

```js
// meta: name, description, whenToUse — discovered only if placed under a workflows/ dir.
// args: { pr: <number>, diff: <string>, dimensions?: [...] }
const DIMENSIONS = [
  "correctness", "security-credential-surface", "test-coverage", "perf-simplification",
];

// 1. FAN OUT — one reviewer per dimension, exactly one level deep (direct agent() calls,
//    never a nested workflow()). Barrier: parallel() awaits all.
const rawFindings = await parallel(
  DIMENSIONS.map((dim) => () =>
    agent(reviewPrompt(dim, args.diff), {
      label: `review:${dim}`,
      phase: "review",
      effort: dim === "correctness" ? "high" : "medium", // #3705 recoverable in-session
      schema: FINDINGS_SCHEMA, // typed: [{severity, dimension, claim, evidenceLine}]
    }),
  ),
).then((r) => r.filter(Boolean).flat()); // .filter(Boolean): agent() may return null

// 2. ADVERSARIAL VERIFY — NOT a verify() primitive; an agent() that drops unsupported
//    findings. Typed in, typed out.
const verified = await agent(verifyPrompt(rawFindings, args.diff), {
  label: "adversarial-verify",
  phase: "verify",
  effort: "high",
  schema: VERIFIED_FINDINGS_SCHEMA, // [{...finding, diffSupported: boolean, reason}]
}).then((v) => (v ?? []).filter((f) => f.diffSupported));

// 3. TYPED REDUCE — plain JS, no free-text/label parsing. Read-only: returns a verdict,
//    applies NOTHING to the PR.
return {
  pr: args.pr,
  verdict: verified.some((f) => f.severity === "blocker") ? "changes-requested" : "approve",
  findings: verified,
  dimensionsCovered: DIMENSIONS,
}; // caller decides what to do — the workflow itself merges/labels nothing.
```

Invariants the sketch preserves by construction: **one level deep** (no `workflow()` call
inside; dimension reviewers are direct `agent()` calls); **read-only** (returns a verdict
object, performs no label/PR/merge action); **no issue creation**; **single-token-safe**
(all `agent()` calls share the session token — no rotation claimed).

---

## What is deferred (explicitly)

1. **The runnable prototype.** Place `judge-fanout-workflow.js` under a discovered
   `workflows/` directory (or invoke via `Workflow({scriptPath})`) from a **top-level**
   Claude Code session — not a subagent — so its `agent()`/`parallel()` calls are the first
   nesting level (#3289-safe).
2. **The measured comparison.** Against a fixed set of 3–5 already-merged/rejected PRs,
   measure precision (unverified-nit rate), recall (dimensions caught), latency, and token
   cost versus today's single-pass Judge. Document methodology **and raw results** (not just
   conclusions). This document intentionally contains **no** such numbers — none have been
   measured, and none are fabricated.

---

## Recommended follow-ups (to be filed by a human/Curator — NOT filed here)

Listed with enough detail to file later. Filing is a human/Curator decision (autonomous
filing here would risk the #3707 race the issue itself flags).

1. **Prototype + measure the Judge fan-out workflow (KEEP surface #1).** Promote
   `judge-fanout-workflow.js` from sketch to runnable behind `LOOM_JUDGE_FANOUT_EXPERIMENT`;
   run it from a top-level session against a 3–5 PR corpus; record precision/recall/latency/
   cost vs. the single-pass Judge. Acceptance: a measured table + a keep/kill call. Depends
   on: nothing (self-contained, read-only). *This is the direct continuation of #3739.*
2. **Design the checkpoint-interop contract for a workflow-driven Doctor→Judge loop (DEFER
   surface #2).** Specify how a workflow's journal resume maps onto
   `.loom/sweep-checkpoint/issue-N.json` (#3373) so the daemon reaper can still observe/
   resume it. Depends on: #1's typed-verdict shape.
3. **Prototype the `/sweep all` triage classifier as a pure-JS workflow (DEFER surface
   #3).** Classifier + dry-run plan in-session; building legs still route to `loom-daemon`.
   Must keep the confirmation gate and the loom:operator-only exclusion. Depends on: #1;
   coordinate with #3707 for any epic-fan-out class.
4. **After #3707 resolves, prototype fan-out-reads/serialize-writes for Architect/Hermit
   (DEFER surface #4).** The serialized-`gh issue create`-behind-a-lock reduce is the whole
   point; blocked on #3707. Depends on: #3707.
5. **(Only if #2 lands) revisit folding the #3725 model-cost record-keeping into the
   workflow (REJECT-for-now surface #5).** Revisit *only* once a workflow already drives the
   sweep; otherwise leave the tested Python path alone.

---

## References (read-only)

- `defaults/.claude/commands/loom/sweep.md` — the ~1600-line prose orchestrator (read for
  the lifecycle, escalation ladder, doctor-cycle cap, checkpoint convention).
- `defaults/roles/judge.md`, `defaults/.claude/commands/loom/judge.md` — Judge dimensions
  and the No-Fable-Judge invariant (design reference for surface #1).
- `CLAUDE.md` → "Model-Cost Experiment" / "Model Selection Strategy" — the
  `LOOM_MODEL_EXPERIMENT` / `sweep.modelExperiment` off-by-default flag precedent this
  prototype's flag follows, and the escalation-ladder / doctor-cycle-cap arithmetic that
  DEFER surface #2 would move into code.
- `defaults/scripts/sweep-experiment.sh` — the existing Python-backed experiment stub
  (context for REJECT surface #5).
- CLI **v2.1.206** binary — source of the confirmed primitive signatures in "Feasibility
  gate".
