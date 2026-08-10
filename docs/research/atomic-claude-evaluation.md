# Evaluating damusix/atomic-claude for ideas worth bringing into Loom

**Type**: Research / comparison evaluation
**Status**: Complete (research/documentation only — no runtime, role, hook, or config change)
**Source issue**: [#5844](https://github.com/rjwalters/loom/issues/5844)
**Subject**: [damusix/atomic-claude](https://github.com/damusix/atomic-claude) — MIT-licensed,
self-described as stable. A Claude Code configuration system built around a persistent repo
wiki, a tree-sitter code graph, and a maker/checker subagent loop.
**Date**: 2026-08-09. Source cloned read-only to a scratch location
(`git clone --depth 1 https://github.com/damusix/atomic-claude.git`, not committed anywhere in
this repo) and read directly — every citation below is a real file path in that clone, not the
README's marketing copy.

## TL;DR

Loom and atomic-claude solve overlapping problems from opposite directions. **Loom is
forge-centric**: GitHub/Gitea labels are the coordination state, a Rust daemon dispatches work
across git worktrees, and roles are the unit of specialization — strong at *multi-agent fleet
orchestration* across many short-lived, cold-start agents. **atomic-claude is repo-centric**: a
persistent wiki + symbol graph is the coordination state, and workflow progresses through
semantic slash commands inside one long-lived interactive session — strong at *per-repo context
durability and code comprehension* for a single human driving one Claude Code session.

That architectural difference is the throughline of every verdict below. The **wiki/dirty-marking
idea is the one clean fit** — it targets exactly the gap Loom already admits to (every agent
re-derives the codebase from `CLAUDE.md` + grep, which is why `CLAUDE.md` itself is
budget-rationed). Everything else either (a) needs real adaptation before it fits Loom's
cold-start-agent, forge-as-state model, or (b) is already substantially covered by something Loom
built for a different reason (the daemon event bus, the PR-level Builder→Judge split), or (c)
depends on a single persistent interactive session that Loom's architecture deliberately does not
have.

| # | Idea | Verdict | Follow-up issue |
|---|------|---------|------------------|
| 1 | Persistent repo wiki + dirty-marking | **Adopt** | [#5847](https://github.com/rjwalters/loom/issues/5847) |
| 2 | Code graph via tree-sitter / blast-radius MCP | **Adapt** | [#5848](https://github.com/rjwalters/loom/issues/5848) |
| 3 | Maker/checker test-first split | **Adapt** | [#5849](https://github.com/rjwalters/loom/issues/5849) |
| 4 | Self-sharpening config (`/retrospective-learning`) | **Adapt** | [#5850](https://github.com/rjwalters/loom/issues/5850) |
| 5 | Inter-session bus | **Skip** | — |
| 6 | "Realm" multi-repo workspace | **Adapt** | [#5851](https://github.com/rjwalters/loom/issues/5851) |
| 7 | Persistent REPLs | **Skip** | — |
| 8 | Incremental adoption ladder | **Skip** | — |

No code, role, hook, or config changes ship in this PR. Every **adopt**/**adapt** verdict below
links to a separate `loom:architect`-labeled issue proposing a concrete Loom-shaped design — this
document does not implement anything.

## Scope and non-goals

- atomic-claude was cloned **read-only** to `/tmp/atomic-claude-eval` to read its actual source
  (commands, agents, the Go `atomic` CLI internals, design/spec docs) — not installed, run, or
  added as a dependency anywhere in this repo. The clone is not committed; nothing under
  `atomic/`, `commands/`, `agents/`, etc. from that repo appears in this diff.
- This is a comparison and adoption-triage exercise, not a full architecture review of either
  project. Verdicts are informed by reading the cited files, not by running atomic-claude's
  `atomic` binary or its Claude Code commands.
- **Expected diff shape**: exactly this one new file under `docs/research/`. All Loom-shaped
  design work for adopted/adapted ideas is deferred to the five linked follow-up issues.
- License: atomic-claude is MIT (`LICENSE` in its repo, copyright Danilo Alonso). Nothing here
  copies its code — every idea below is described conceptually and grounded by file-path
  citation, not by porting text or source. If a future follow-up issue ends up copying literal
  code or prose (rather than reimplementing a concept), that follow-up must carry MIT attribution
  per the license's terms; none of the five filed here propose doing so.

## What atomic-claude is (from reading the source, not just the README)

atomic-claude installs a bundle of Claude Code artifacts (`commands/`, `agents/`, `skills/`,
`rules/`, `output-styles/`) plus a companion Go CLI binary (`atomic`, source under `atomic/`) that
does the deterministic heavy lifting the LLM shouldn't (hashing, scanning, tree-sitter parsing, a
Unix-socket daemon for inter-session messaging, a symbol-graph SQLite store). The Claude-facing
surface is a set of slash commands (`/setup-wiki`, `/refresh-wiki`, `/atomic-plan`,
`/subagent-implementation`, `/retrospective-learning`, `/autopilot`, …) that orchestrate
fresh-context subagents (`agents/atomic-implementer.md`, `atomic-reviewer.md`,
`atomic-investigator.md`, `atomic-wiki-inferrer.md`, `atomic-strategist.md`). The whole system is
designed around **one interactive Claude Code session per repo, run repeatedly by the same human**
— nothing in it assumes concurrent, independently-dispatched, cold-start agents the way every Loom
sweep does.

## 1. Persistent repo wiki + dirty-marking — **Adopt**

**What it does.** `/setup-wiki` (`commands/setup-wiki.md`) audits a repo for conventions
(`.gitignore` entries, `docs/` layout, a `CLAUDE.md`) and proposes only what's missing, never
overwriting. `/refresh-wiki` (`commands/refresh-wiki.md`) runs the actual pipeline: a
**deterministic scan** (`docs/wiki/scan.md`, `atomic wiki scan`) captures raw facts, then the
`atomic-wiki-inferrer` agent (`agents/atomic-wiki-inferrer.md`) synthesizes a compact router
(`docs/wiki/index.md`) plus domain-split detail files, wired into `CLAUDE.md` via an `@-ref`
(`@docs/wiki/index.md`) so it auto-loads every session. Staleness is tracked by comparing a
recorded `reflects_rev` fingerprint against current HEAD (`atomic wiki stale`,
`commands/refresh-wiki.md` Step 5) and a `.dirty` marker file that is **only cleared on a fully
clean refresh** (`refresh-wiki.md` Step 12: `rm -f <resolved-root>/wiki/.dirty`) — a partial or
aborted run leaves it set, so the staleness nudge keeps firing until a clean pass completes. The
router deliberately excludes the large raw scan from the auto-loaded `@-ref` ("`docs/wiki/scan.md`
is NOT — it can be thousands of lines on large repos and would blow up context" —
`setup-wiki.md` line 272) — only the compact index is force-loaded.

**Why this is the highest-value idea here.** This targets exactly the gap Loom's own `CLAUDE.md`
names as a cost center: "This file is the operating core... the CI budget check
(`scripts/check-claude-md-budget.sh`) enforces this so every agent's context does not regrow
unchecked." That sentence describes rationing a scarce, **hand-maintained** resource. A
generated, dirty-marked digest is a structurally different kind of artifact — its size can grow
with the repo without anyone manually trimming prose, because a machine wrote it and a machine
can re-derive it. Every Loom role (Builder, Judge, Curator, Doctor, Hermit) currently re-derives
"what does this repo look like" from `CLAUDE.md` + grep on every dispatch; a maintained digest of
build/test commands, directory→domain map, and cross-cutting notes would shrink that cold-start
tax across the whole fleet, not just one session.

**What does NOT transfer directly.** atomic-claude's dirty-marking and session-start nudge assume
a session that can *notice* it's stale and *run* `/refresh-wiki` itself — Loom has no equivalent
single session; every dispatch is an independent, ephemeral worktree that may never come back. The
follow-up issue (below) has to redesign the trigger and ownership: a periodic role pass committing
the regenerated digest via a normal PR, not a session-start hook.

**Follow-up**: [#5847](https://github.com/rjwalters/loom/issues/5847) — design a generated,
dirty-marked repo knowledge digest, forge-committed rather than session-local.

## 2. Code graph via tree-sitter / blast-radius MCP — **Adapt**

**What it does.** A pure-Go, single-static-binary code-intelligence engine
(`docs/design/code-intel-engine.md`) parses the repo with tree-sitter (via `wazero`/WASM, no cgo)
across 19+ languages (`atomic/internal/codeintel/extraction/languages/*.go` — `go.go`,
`python.go`, `typescript.go`, `rust.go`, `ruby.go`, … plus standalone extractors for Vue/Svelte/
Liquid/embedded SQL under `extraction/standalone/`), stores symbols/edges in SQLite
(`atomic/internal/codeintel/db/`, `graph/graph.go`), and resolves references including
framework-specific synthesized edges (`atomic/internal/codeintel/resolution/frameworks/*.go` —
Spring, Rails-style Ruby, Node, Python, Elixir, Rust). It's exposed two ways: an MCP server
(`atomic/internal/codeintel/mcp/server.go`, `docs/guides/code-intel-mcp.md`) for the interactive
session, and direct CLI verbs (`atomic code callers|callees|impact|explore|search`) that
subagents shell out to instead (`agents/atomic-implementer.md` and `atomic-reviewer.md` both carry
an identical `## Code-intel index` section instructing exactly this). The degradation contract is
explicit and repeated verbatim in every consuming agent: "Before querying, confirm the path is
live... On any failure — binary absent, DB missing, query error — fall back silently to
sg/grep/heuristics. Never print an error about the index being unavailable; never block because it
is missing."

**Why adapt, not adopt.** The engine itself is not a small utility — it's effectively its own
product: a full extraction pipeline for 19+ languages, a resolution pipeline with per-framework
synthesis, a realm-federation layer (see idea 6), and an MCP server, all reproducing "the data
model and tuned constants" of an even larger reference implementation
(`docs/design/code-intel-engine.md`: "Broad-parity extraction: all 19 tree-sitter languages + the
standalone extractors"). Building or vendoring that is disproportionate to Loom's actual need,
which is narrower and different: Loom's own codebase is Rust + TypeScript + shell + a lot of
Markdown, and the consumers would be two support roles (Judge asking "what does this diff's blast
radius actually touch?", Hermit asking "are there real callers of this symbol?") — not every
Builder dispatch on every language a Loom-managed repo might contain. The value (a real symbol
graph beating grep for "what calls this?") is real; the implementation shape should not be a
ported clone of a 19-language Go engine.

**Follow-up**: [#5848](https://github.com/rjwalters/loom/issues/5848) — survey lighter-weight
alternatives (`ast-grep`, language-server call hierarchies, a narrowly-scoped Rust+TS-only
tree-sitter query) before recommending any build-vs-integrate path, preserving the same
non-negotiable graceful-degradation contract atomic-claude enforces.
**Outcome**: that survey is complete — see
[`docs/design/code-intel-lite.md`](../design/code-intel-lite.md). It recommends **against** any
graph or index (a whole-repo `ripgrep` scan already costs ~11 ms, and a Rust+TS graph would be
blind to ~62% of the files referencing a given Rust symbol in this repo), in favour of a stateless
whole-repo reference-evidence helper — narrowing the "adapt" verdict here to "adapt the
degradation contract and the question, not the engine."

## 3. Maker/checker, test-first split — **Adapt**

**What it does.** This is Anthropic's evaluator-optimizer pattern, applied inside a single unit of
work rather than across a whole PR. `agents/atomic-implementer.md` has an explicit TDD step:
"For new behavior: write failing test first, run it, confirm it fails for the right reason (not a
syntax error). Implement. Run again, confirm green. For bug fixes: write a test that reproduces
the bug (fails on current code), then fix, then confirm green." (`atomic-implementer.md`,
Workflow step 3). `agents/atomic-reviewer.md` then independently **re-runs** the claimed signals
rather than trusting them: "`tests: ✓` — run tests yourself, confirm... If implementer's claim
doesn't match reality → `🔴 bug: claimed tests pass but npm test reports M failures.`"
(`atomic-reviewer.md` code-mode workflow step 4). The orchestrator loop that drives this
(`commands/subagent-implementation.md`) adds a stuck-fix escalation (two consecutive
`CHANGES_REQUESTED` rounds on the same signal → surface `/pressure-test` / `atomic-strategist`
options rather than loop blindly) and a 6-iteration soft-stop.

**What Loom already covers.** The cross-context half of this pattern is already Loom's structure:
Builder implements, Judge reviews in a genuinely separate, fresh-context dispatch, gated on the PR
(`CLAUDE.md` §"Sweep Lifecycle": `Builder → Judge → Doctor (if needed) → Merge`, "all stages of the
lifecycle must be executed in order"). That is functionally the same evaluator-optimizer shape
atomic-claude runs *within* one implementer/reviewer loop — Loom just runs it at the PR
granularity instead.

**What's genuinely missing.** The narrower, missing piece is the **in-Builder** test-first
discipline plus a checkable signal for it. Loom's `builder.md` has only a general "test your
changes" guideline, no requirement to write the test before the fix, and no verifiable trace of
whether that happened. atomic-claude's reviewer treats an unverified test claim as a hard bug
(🔴), not a style note — that's the part worth adapting, not the whole implement→review loop
machinery (which would duplicate the PR-level Builder→Judge cycle Loom already has).

**Follow-up**: [#5849](https://github.com/rjwalters/loom/issues/5849) — a concrete, checkable
test-first signal Judge or `buildGate` can verify (e.g. a required PR-body line, or a commit-order
check), scoped to the in-Builder discipline only.

## 4. Self-sharpening config (`/retrospective-learning`) — **Adapt**

**What it does.** `commands/retrospective-learning.md` is a large orchestrated audit: it mines the
last 5 `.jsonl` session-history files plus the live conversation for correction/frustration
signals ("Corrections: 'no', 'don't', 'stop'... Explicit feedback: 'improve', 'better', 'should'"
— Step 2b), cross-references installed config artifacts (skills, rules, `CLAUDE.md`, memory) for
staleness/bloat/contradiction, categorizes findings into a 13-tier priority scheme, and walks them
**one at a time** via `AskUserQuestion` with `Accept / Modify / Skip` — nothing is auto-applied
(Step 6-7). A persisted run log (`~/.atomic/retro-runs/<ts>.json`) later checks whether an accepted
change actually landed and stuck (Step 2c, "prior-retro audit").

**Why adapt, and why the interactive flow specifically does not transfer.** The entire mechanism
assumes one user having one long-running, correctable conversation — the "Modify flow" section is
explicit about this: "`AskUserQuestion` does not collect free-text... The only way to get the
user's replacement wording is to end the assistant turn and resume from their next message." Loom
sweeps are overwhelmingly headless and unattended; there is no live human turn to resume from in
most invocations, and no single multi-session `.jsonl` transcript to mine the way
`history-brief.md` does (Loom sessions are one-shot per dispatch). The concept worth keeping is
narrower: **mine Loom's own durable, forge-hosted record of recurring corrections** — Judge's
`loom:changes-requested` review comments across PRs, and Doctor's fix-commit patterns — as the
input, instead of chat transcripts.

**The non-negotiable guardrail carries over unchanged.** atomic-claude's own "initial read" in the
source issue flagged this correctly: automated rewriting of role prompts needs strong guardrails.
atomic-claude's answer is per-item human confirmation before any write. Loom's answer has to be
the same idea translated to its own review surface: every proposed change ships as a normal,
Judge/human-reviewed PR — never a direct edit to `.loom/roles/*.md`, `CLAUDE.md`, or
`.github/labels.yml` outside the standard lifecycle.

**Follow-up**: [#5850](https://github.com/rjwalters/loom/issues/5850) — a mining-source and
cadence proposal (which role, which forge data, how often), with the human/Judge-reviewed-PR
guardrail as a hard requirement.

## 5. Inter-session bus — **Skip** (largely covered; also a genuine architecture conflict)

**What it does.** `atomic bus` (design: `docs/design/atomic-bus.md`; implementation:
`atomic/internal/bus/{protocol,daemon,room,client,identity}.go`) is a single per-user Unix-domain-
socket daemon at `~/.atomic/bus.sock` that lets concurrent Claude Code sessions on **one machine**
message each other over named rooms, with addressed-vs-FYI messages (a `to` field distinguishes
"act on this" from "note and don't act," the whole loop-prevention mechanism per
`docs/reference/bus.md`), and a human-enforceable `halt` that makes agent `send` calls fail with a
distinct exit code until resumed. The design doc is explicit about why it needs a daemon rather
than files: "`halt` must bind, not merely inform... Advisory halt is a request an agent can ignore
— and the agent that most needs halting is the one looping" (`atomic-bus.md`, Recommendation).

**Why skip.** Two independent reasons, not just one:

1. **Largely already covered.** Loom already has a cross-agent coordination layer built for a
   different reason but serving the same need: the daemon's own event bus + MCP tools
   (`mcp__loom__subscribe_to_events`, etc. — `.loom/docs/daemon-reference.md`), and the optional
   `safehouse_send`/`safehouse_read` fleet-comms surface (`builder.md` §"Fleet-Comms Etiquette")
   that Loom roles already use for cross-agent notes. Room-scoped addressing and a human `halt`
   are real refinements atomic-bus has that Loom's event bus doesn't, but they'd be incremental
   improvements to an existing surface, not a new one.
2. **Architecture conflict.** atomic-bus fundamentally assumes long-lived, co-located sessions on
   one host that stay up to `recv` on a socket. Loom's Builder/Judge/etc. are dispatched,
   short-lived, and worktree-isolated by design (`CLAUDE.md` §"Git Worktree Workflow") — an agent
   that finishes its PR and exits has nothing left to `recv` with. A persistent-daemon messaging
   layer is a better fit for atomic-claude's single-host, always-on session model than for Loom's
   ephemeral-per-issue one.

No follow-up filed — this is a case where the negative finding is the useful output, per the
source issue's own framing.

## 6. "Realm" multi-repo workspace — **Adapt**

**What it does.** A Realm is a directory holding multiple member repos plus a compiled `wiki/`
layer: per-repo summaries for members without their own wiki, cross-repo "concern" documents, and
knowledge pages synthesized from capture buckets (`research/`, `raw/`, etc.) — see the tree
example in `README.md` and the federation design in `docs/design/code-intel-realm.md` /
`atomic/internal/codeintel/realm/{config.go,resolver.go,seed.go}`. Code-intel queries fan out
across every member repo's own independent index when run from the realm root
(`code-intel-realm.md`: "Federation... N independent per-repo dbs; verbs fan out").

**Why adapt, not adopt as-is.** The Realm's `wiki/` is itself a second, locally-compiled
coordination-state store living entirely outside any forge — exactly the shape Loom's architecture
deliberately avoids (`CLAUDE.md`: "GitHub/Gitea labels are the coordination state"). A literal port
would introduce a state artifact invisible to every label, webhook, and PR Loom's other roles read
from, with no natural mechanism to keep it current the way forge state is (a `.dirty` marker on a
local file nobody's dispatched agent is ever guaranteed to see). Loom already notes "relevant to
fleet hosts running several Loom repos" as the plausible motivation, and that motivation is real —
this host runs multiple Loom-managed repos with no shared view across them today — but the
existing candidate homes for that view are forge/daemon-native, not a new local file store:
`loom-daemon serve` (the opt-in fleet dashboard, `.loom/docs/daemon-reference.md` §"Fleet
dashboard") and the `observability` block (`.loom/docs/observability.md`) already aggregate
per-repo daemon state to a backend.

**Follow-up**: [#5851](https://github.com/rjwalters/loom/issues/5851) — first resolve which
problem this actually is (a dashboard rollup across repos vs. cross-repo agent context — likely
only the former is in scope near-term), then design it on top of `loom-daemon serve`/
`observability` rather than a new local wiki-style store.
**Outcome**: that resolution is complete — see
[`docs/design/fleet-cross-repo-summary.md`](../design/fleet-cross-repo-summary.md). It scopes to
the dashboard-rollup framing, and finds — checked against this host's own running daemon, not just
documentation — that the "no shared view across them today" premise above no longer holds: the
existing multi-repo daemon (`loom-daemon workspace add` + `status`/`serve`) already reports
per-repo active-sweep counts and forge queue depth across every registered repo, live, with zero
new state. Recommendation: build nothing new for this framing; cross-repo agent context stays
out of scope, gated on the wiki-digest proposal (#5847) shipping first.

## 7. Persistent REPLs — **Skip**

**What it does.** `atomic repl` (design: `docs/design/atomic-repl.md`; implementation:
`atomic/internal/repl/{client,spawn,protocol}.go`, harnesses at
`atomic/internal/repl/harness/{python_harness.py,node_harness.js}`) gives an agent a named,
persistent Python/Node interpreter session that survives across separate Bash tool calls — solving
the problem that "Claude Code's Bash tool resets shell and interpreter state on every invocation
(only the working directory persists)" (`atomic-repl.md`, Problem). Each session is a detached,
self-serving harness process bound to a Unix socket, self-reaping after an idle window.

**Why skip.** This targets a task shape — "load a dataset once, iteratively query it across many
tool calls" — that doesn't match how Loom's own agents actually work: Builder/Judge/Doctor/etc.
mostly run git operations, forge API calls (`gh`), and file edits, not iterative interpreter-state
exploration. Loom's own tooling notes describe the identical Bash-statelessness constraint this
agent runs under ("The working directory persists between commands, but shell state does not")
without ever needing a persistent-interpreter workaround for it — the actual task shapes don't
require one. Adding a persistent-daemon-backed REPL surface would also sit awkwardly against
Loom's worktree-lifecycle model: a REPL session tied to a worktree that `loom-clean` or the
daemon's reaper subsequently removes would need explicit teardown, another background process for
the token-pool/host-sleep concerns Loom's docs already track to worry about. Low value, real new
surface — not worth it.

## 8. Incremental adoption ladder — **Skip** (already Loom's own pattern; no gap)

**What it is.** atomic-claude frames itself as opt-in, layer by layer: "reply formatting → wikis →
full autopilot, each layer opt-in" (`README.md` feature table, "Incremental adoption"), concretely
walked in `docs/guides/getting-started.md` (Step 1: output style, Step 2: `/setup-wiki` +
`/refresh-wiki`, Step 3: either the in-the-loop `/atomic-plan → /subagent-implementation → /commit`
path or the hands-off `/autopilot`).

**Why skip as a distinct proposal.** This isn't a feature to borrow — it's a design *philosophy*
Loom already has, independently derived, and documents explicitly. `CLAUDE.md`'s own tier table
(Tier 0 single-task roles → Tier 1 `/loom:sweep` single-issue lifecycle → Tier 2 continuous daemon
dispatch → Tier 3 human oversight) is the identical "start small, layer up, everything stays
addressable on its own" shape, plus the `.loom/config.json` `buildGate`/`autonomous`/`runtimes`
blocks that are individually opt-in and default-conservative. There's no concrete artifact here to
adapt and no gap to close — the honest verdict is that Loom already does this, and the value of
writing that down is only to confirm the pattern rather than propose a new one.

## What this evaluation deliberately did not do

- Did not install, run, or add the `atomic` binary or any atomic-claude command/skill/agent to
  this repo or any Loom-managed repo.
- Did not benchmark atomic-claude's code-intel engine, wiki inference quality, or bus latency —
  verdicts above are architectural-fit judgments from reading the source, not measured
  comparisons.
- Did not evaluate atomic-claude's SQL/dbt lineage extraction, its `atomic serve` web frontend, or
  its Docker evaluation harness (`docs/guides/evaluations.md`) — out of scope for the 8 named
  concepts.
