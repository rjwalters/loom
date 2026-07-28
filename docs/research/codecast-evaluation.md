# Evaluating codecast for the Loom fleet: visibility layer vs. message-passing

**Type**: Research / fleet-architecture evaluation
**Status**: Complete (exploratory — no code change, no infrastructure stood up)
**Source issue**: [#4008](https://github.com/rjwalters/loom/issues/4008) (originally filed as
`rjwalters/safehouse#13`, moved here as a loom fleet-architecture concern)
**Subject**: [codecast-sh/codecast](https://github.com/codecast-sh/codecast) — "See, steer, and
remember every coding agent session," compared against `rjwalters/safehouse` (an
end-to-end-encrypted Matrix-room message-passing/coordination substrate for the same fleet)
**Date**: 2026-07-27 — every external fact below was retrieved this date via `gh api`; retrieval
commands are inline so a later reader can re-derive or detect drift.

> This is the second entry in `docs/research/` — following the shape of
> [`docs/notes/prometheus-comparison.md`](../notes/prometheus-comparison.md) (skeleton) plus
> [`docs/research/dynamic-workflows-evaluation.md`](dynamic-workflows-evaluation.md) ("Scope and
> non-goals" + keep/defer/reject conventions). Repo-and-docs research only: codecast was **not**
> installed, run, cloned, or added as a dependency anywhere in this diff.

## TL;DR

**Verdict: complement, with a concrete borrow list.** codecast and safehouse solve different halves
of the same fleet problem and neither subsumes the other:

- **safehouse owns coordination** — the active, encrypted, human-steerable message bus between
  agents and humans. It is the source of truth for "who told whom what, and when."
- **codecast (if ever adopted) would own passive session visibility/memory** — search, attribution,
  and a mobile/desktop dashboard over agent transcripts that already happened. It does not need to
  mediate a single message to be useful.
- **loom keeps owning orchestration.** codecast's wave/inbox model is a *push-based, in-tool*
  orchestrator that would compete with, not extend, loom's forge-native sweep/work-finder pipeline
  (see Question 2) — that surface is **not** adopted.

The fleet-architecture recommendation: **do not adopt codecast today.** It is an 11-star, seven-
month-old project (Maturity assessment, below) whose orchestration surface directly duplicates
loom's and whose default data-handling posture (plaintext transcript sync to a networked Convex
backend) is the opposite default from safehouse's server-sees-ciphertext-only invariant. The
concrete, non-platform-dependent value — `cast blame`-style attribution, semantic session search,
the iOS dashboard UX — is real and **loom-buildable independently** of ever running codecast (see
the borrow list). That value should be pursued as loom/safehouse-native follow-ups, not by taking a
dependency on a pre-adoption external platform.

## Scope and non-goals

Per the issue's Curator guardrails and Champion approval:

- **codecast was never installed, run, cloned into a worktree, or added as a dependency.** All
  reading below went through `gh api repos/codecast-sh/codecast/...` (README + `contents/docs/*`),
  never a local clone — this also keeps `bun.lock`/`convex/`/`patches/` out of the diff.
- **This deliverable does not stand up any codecast or safehouse infrastructure.** No Convex
  backend, no Bun daemon, no Matrix homeserver.
- **Follow-ups are listed, not filed** (see the closing section) — consistent with
  `docs/research/dynamic-workflows-evaluation.md`'s precedent and the #3707 concurrent-issue-
  creation hazard this repo's memory already flags.
- **The license question is answered inline, not researched** (Correction 1 pre-answered it) — see
  Question 5.
- **Expected diff shape**: exactly one new file under `docs/research/`. No `CLAUDE.md` growth, no
  `defaults/`, `.loom/`, `loom-daemon/`, or `loom-tools/` touched.

## What codecast is

Source: `gh api repos/codecast-sh/codecast/readme` (retrieved 2026-07-27); corroborated by
`docs/GETTING-STARTED.md`, `docs/SELF-HOSTING.md`, `docs/orchestration-guide.md`, and
`docs/multi-client.md` from the same repo, same retrieval date.

codecast is a **session-memory / observability + orchestration platform** for AI coding agents
(Claude Code, Codex, Cursor, Gemini today; OpenCode and pi "in active testing"). Architecture, per
`docs/GETTING-STARTED.md`:

```
~/.claude/projects/**/*.jsonl ──┐
~/.codex/history/**/*.jsonl ────┤
~/.cursor/ ─────────────────────┼──▶ CLI Daemon ──▶ Convex Backend ──▶ Web Dashboard
~/.gemini/ ─────────────────────┘   (packages/cli)  (packages/convex)  (packages/web)
```

- **Non-invasive observation.** A per-user CLI daemon (Bun) tails each agent's own JSONL/SQLite
  history files (`docs/multi-client.md`'s per-client `watcherKind`: `jsonl-dir` or `sqlite`) and
  syncs a real-time copy to a self-hosted Convex backend. It never mediates the agent's own I/O
  path — the agent doesn't know codecast exists. This answers Question 1's "does it observe rather
  than mediate": **yes, confirmed** by the watcher-based architecture, not the README's tagline.
- **Session memory**: full-text + semantic search (`cast search`, `cast ask`), `cast blame`
  (git-blame with the author column replaced by the session that wrote each line), `cast handoff`
  (context-transfer doc generation).
- **Live inbox + steering**: a triage queue of sessions (working / idle / needs-input / errored),
  message injection into a live terminal session from web/desktop/phone, permission-prompt
  approval from a browser.
- **Plans/tasks + wave orchestration**: `cast plan decompose` → `cast plan autopilot` spawns
  implementor/reviewer/critic agents per task in tmux + git worktrees, in dependency-respecting
  waves, with drive-round polish loops (`docs/orchestration-guide.md`).
- **Clients**: web (React 19 + Vite), native macOS desktop (Electron), iOS/Android (Expo/React
  Native), a VS Code/Cursor blame extension, a vim integration.
- **Backend**: self-hosted Convex only (no managed-cloud escape hatch other than the vendor's own
  `codecast.sh` instance) — `docs/SELF-HOSTING.md` walks a ~1-hour Railway + Postgres + Caddy
  deployment.

## Maturity assessment

```
$ gh api repos/codecast-sh/codecast --jq '{stargazers_count,pushed_at,created_at,archived}'
{"stargazers_count": 11, "pushed_at": "2026-07-27T23:24:55Z", "created_at": "2025-12-10T02:44:09Z", "archived": false}

$ gh api repos/rjwalters/safehouse --jq '{stargazers_count,pushed_at,created_at,archived}'
{"stargazers_count": 0, "pushed_at": "2026-07-27T18:58:15Z", "created_at": "2026-07-27T03:17:36Z", "archived": false}
```

codecast: **11 stars**, created 2025-12-10, pushed **today** — roughly seven and a half months old,
actively developed, not abandoned. safehouse: **0 stars**, created **today** — it is a same-day-old
design-phase project of the operator's own, with no code yet (its own `docs/design.md` opens
"Status: design phase, no code").

**This bounds the verdict space.** Both projects are pre-adoption by any external-ecosystem measure.
Neither has the maturity to justify "adopt as a hard fleet dependency" — the honest comparison here
is between two young, single/small-team projects, one third-party (codecast) and one in-house
(safehouse). That asymmetry matters: safehouse is a dependency loom's own operator controls and can
reshape on demand; codecast is a dependency on someone else's roadmap, bus factor, and API
stability. Combined with the orchestration-surface duplication in Question 2, this pushes the
verdict toward **complement/borrow-ideas rather than adopt**, not toward rejecting the *problem*
codecast addresses (session visibility) — that gap in loom is real regardless of codecast's own
maturity.

## Question 1 — Integration shape

**Can codecast run alongside loom + safehouse on a fleet host, with safehouse mediating the
encrypted room (coordination, source of truth) and codecast passively capturing/searching the
resulting sessions? Where do responsibilities divide vs. collide?**

**They can run alongside each other with no structural collision, because their write paths never
touch.** codecast's daemon is a **read-only tailer** of each agent's own transcript files
(`~/.claude/projects/**/*.jsonl` etc., confirmed in `docs/GETTING-STARTED.md` and the
`watcherKind: jsonl-dir` descriptors in `docs/multi-client.md`) — it does not open a socket to the
agent, does not intercept its tool calls, and does not require the agent to call any codecast API to
be observed. safehouse's daemon (`safehoused`) is the opposite shape: agents talk to it *actively*,
over a local unix socket, to hand off a message (`rjwalters/safehouse` `docs/design.md`, §4.1 —
`send(room, envelope)` / `check(persona)`). One is a passive file-tail; the other is an active
message bus. Nothing forces them onto the same write surface.

**Division of responsibility, if both were ever run**: safehouse is the **coordination source of
truth** — the encrypted room *is* the audit log of agent-to-agent and agent-to-human handoffs
(`docs/design.md` §3: "The room is the single source of truth... even between two agents on the same
host"). codecast, if adopted, would sit **downstream and read-only**: it would index the same
agents' local transcript files (which already *contain* whatever safehouse messages an agent
composed or received, since those show up as tool calls/text in the transcript) for search and
attribution, without ever becoming a coordination channel itself. Concretely: safehouse answers "did
the research-agent tell the writer-agent to hold off," codecast (if adopted) would answer "which
session touched `src/auth.ts` and when did it mention the safehouse handoff."

**Where they would collide, if adopted carelessly**: codecast's own inbox/messaging feature
(`cast send <id> "text"` injecting into a live terminal session) is a **second, unencrypted,
Convex-mediated message-passing channel** that duplicates safehouse's job with a strictly weaker
threat model (Question 3). Running both without an explicit decision to route ALL cross-agent
messaging through safehouse and treat codecast's `cast send` as unused/disabled would create two
competing "sources of truth" for who-told-whom-what — exactly the fragmentation
`docs/design.md`'s "the room is the single source of truth" principle exists to prevent. **Verdict:
integration is structurally possible (their write paths don't collide) only if codecast's own
messaging/steering surface is treated as out-of-scope and unused, leaving codecast strictly as a
read-only observability layer over the transcripts safehouse-coordinated agents produce.**

## Question 2 — Orchestration overlap

**codecast's wave orchestration + inbox vs. loom's work-finder/sweep model — adopt, interop, or
keep separate? Does codecast's inbox duplicate or complement loom's label-driven pipeline?**

codecast's orchestration primitives (`docs/orchestration-guide.md`) are a near-exact structural
analogue of loom's, at a different layer:

| Concept | codecast | loom |
|---|---|---|
| Unit of work | `task` (`open → in_progress → in_review → done/dropped`) | issue (`loom:triage → ... → loom:issue → loom:building`, [`CLAUDE.md`](../../CLAUDE.md) label lifecycle) |
| Batch scheduling | `wave` — topological sort of the task-dependency graph, spawns all ready tasks in parallel | `loom-daemon` dispatch + GH Actions cron; sweeps are dispatched per-issue, not wave-batched by dependency graph ([`.loom/docs/daemon-reference.md`](../../.loom/docs/daemon-reference.md)) |
| Isolation | tmux session + git worktree per agent | git worktree per issue (`.loom/worktrees/issue-N`, [`CLAUDE.md`](../../CLAUDE.md)) |
| Continuous driver | `cast plan autopilot` — polls every 30s, merges completed branches, spawns next wave, retries failures up to 3x | the autonomous work finder / role runner — **opt-in, default-off** in this repo; "by default [the daemon] is not a work generator — work arrives only via `dispatch_sweep` and the cron workflows" ([`CLAUDE.md`](../../CLAUDE.md) §Daemon Mode) |
| Review gate | `reviewer` agent role, PASS/FAIL structured output | Judge role, `loom:pr` / `loom:changes-requested` labels |
| Polish loop | `drive` rounds — critic agent finds issues, becomes fix tasks, repeat | Doctor role reacting to Judge feedback; no dedicated "critic sweeps the whole codebase" role |
| Coordination substrate | Convex tables (`plans`, `tasks`) — a central, queryable database | forge labels + git — no database, state reconstructible from the forge UI ([ADR-0006](../adr/0006-label-based-workflow-coordination.md)) |

The inbox (`cast sessions`, the web triage queue) is codecast's **cross-cutting visibility surface**
over all of the above — "what's working, what's idle, what needs me" — which is genuinely something
loom's label state does not directly render as a live dashboard (labels are queryable via `gh` but
there is no push/live view).

**Verdict: keep separate for orchestration; the inbox is the one piece worth a note, but still not
worth adopting.** Three reasons:

1. **Substrate mismatch, same shape as the [Prometheus evaluation](../notes/prometheus-comparison.md)'s Q1 verdict.** codecast's plans/tasks/waves live in a **central Convex database** that must be kept in sync with reality; loom deliberately shed exactly this kind of central-state coordinator when it deleted the Python shepherd/daemon brain ([ADR-0009](../adr/0009-shepherd-deprecation.md)). Adopting codecast's orchestration would mean loom's sweep state has *two* sources of truth (the forge's labels and codecast's Convex tables) that could drift.
2. **Direction of travel makes the duplication worse, not better, over time.** codecast's own `docs/plans-roadmap.md` (retrieved 2026-07-27) describes an *actively expanding* plan/task/orchestration layer ("a parallel, stable plan/project/task structure... connective tissue between ephemeral session work and durable project state") — this is not a stable, small feature surface; it is the project's current major investment area. Adopting it now means coupling to a fast-moving target.
3. **loom's own daemon is deliberately conservative here already**, per [`CLAUDE.md`](../../CLAUDE.md): the autonomous work finder is opt-in and off by default in this repo specifically to avoid exactly the kind of uncontrolled auto-spawn that `cast plan autopilot` performs by design. Adopting codecast's autopilot loop would be a step *backward* from that operating posture, not a step alongside it.

The inbox's **visibility** value (a live, non-label-polling view of "what's the fleet doing right
now") is real and does not require adopting the orchestration underneath it — that is exactly the
shape of Question 4's borrow list, not an orchestration adoption.

## Question 3 — Threat-model fit

**codecast's Convex backend + optional E2E vs. safehouse's "server sees only ciphertext"
invariant. Does codecast capture plaintext agent history locally (fine) or sync it off-host
(not fine)?**

safehouse's invariant, stated in its own `docs/design.md` §2: *"The server must see **ciphertext
only**... the machine is the unit of trust."* Local plaintext capture on the host you own is
explicitly fine under this model; plaintext leaving the host to a networked, non-E2E-by-default
service is the thing the whole design exists to prevent.

codecast's default posture is the opposite, established by two of its own docs:

- `packages/shared/encryption/README.md` (retrieved 2026-07-27): "Cross-platform encryption
  utilities for **end-to-end encryption** of conversation data" — described as a utility *available*
  to the product, not a default it applies to every conversation.
- `packages/web/app/(marketing)/security/page.tsx` (retrieved 2026-07-27) is explicit that E2E is
  **opt-in and gated**: the feature list marks "End-to-end encryption" as `detail: "Enterprise
  feature"`, and the FAQ states plainly: *"Do you see my code? Only if you enable code sharing. By
  default, we sync conversation metadata (titles, timestamps, tool names) but not the actual code
  content."*

Read together with the README's own claim that `cast search`/`cast ask` work "across every
conversation your team has had" and the daemon watches history files "wherever you run them and
syncs every conversation in real time," the honest answer is: **codecast's default behavior is to
sync the conversation record (the searchable transcript prose — titles, timestamps, tool-call
summaries, and by the security page's own account, more than that unless E2E is separately turned
on) off the originating host to a networked Convex backend, in plaintext, as the baseline mode.**
Secrets are pattern-redacted before sync (`README.md` Privacy & Security section), which mitigates
credential leakage but is not the same guarantee as "server sees ciphertext only" — pattern-based
redaction is a best-effort filter, not a cryptographic boundary. Raw source-code *content* has an
additional explicit opt-in gate ("code sharing"), but the conversation transcript itself — which is
exactly the kind of data safehouse's threat model is built to protect (agent reasoning, plans,
handoff content) — syncs by default. E2E is available but positioned as an "Enterprise feature," not
the baseline.

This is not an "unclear from docs" case — the docs are explicit that plaintext-by-default,
opt-in-encryption is the model, which is the inverse of safehouse's opt-in-plaintext,
ciphertext-by-default model.

**Verdict: codecast's default threat model does not fit alongside safehouse's invariant, and this
is the single clearest reason it should not be adopted as a coordination-adjacent component without
explicit, always-on E2E configuration.** If codecast were ever run in this fleet, self-hosting the
Convex backend on a host the operator controls (per `docs/SELF-HOSTING.md`) narrows but does not
close the gap — the backend still receives plaintext by default; self-hosting only changes *whose*
server sees it, not *whether* it sees plaintext. A fleet-safe deployment would require enabling
codecast's E2E option universally and verifying (by inspecting the Convex schema/queries, out of
scope for this docs-only evaluation) that no plaintext path bypasses it — a nontrivial verification
this evaluation explicitly defers rather than assumes.

## Question 4 — Borrow list (regardless of the adopt/interop/keep-separate verdict)

Three capabilities are worth building into loom or safehouse's own surface independent of ever
running codecast as a platform:

- **`cast blame`-style session attribution.** codecast's blame view answers "which agent session
  wrote this line" by joining git blame output against session records (`README.md` Editor
  Integrations section: "a drop-in `git blame` replacement whose author column shows the codecast
  session that wrote each line"). loom already has the raw material for a much lighter version:
  every Builder/Doctor commit is created inside a labeled issue's worktree and (per `CLAUDE.md`'s
  builder workflow) closes a specific issue number. **What loom would build**: a small script
  (`.loom/scripts/blame-issue.sh` or a `loom-tools` subcommand) that runs `git blame -e <path>` or
  `git log --follow -S<pattern>`, cross-references each commit's `Closes #N` trailer, and prints the
  originating issue/PR per line or hunk — no session-transcript indexing required, since the
  git history + issue trailer already carries the attribution. **Where it attaches**: `loom-tools`
  CLI (`loom-tools/src/loom_tools/`), as a read-only reporting command; no daemon or database change.
- **Semantic session search.** codecast indexes every session transcript for `cast search`/`cast
  ask` via vector embeddings (Voyage/OpenAI, per `docs/SELF-HOSTING.md`'s "Embeddings (Semantic
  Search)" section). loom's closest analogue today is `.loom/logs/loom-{role}-issue-{N}.log` per-sweep
  transcripts, which are grep-able but not semantically searchable and are not aggregated across
  sweeps. **What loom would build**: an opt-in, off-by-default indexer that embeds completed sweep
  transcripts (or just their final summaries/PR descriptions, to bound cost and avoid re-litigating
  the threat-model question above) into a local vector store, with a `loom-tools search "auth bug"`
  command. **Where it attaches**: a new `loom-tools` subcommand reading `.loom/logs/` and forge PR
  bodies; explicitly **local-only** (no networked backend) to avoid reproducing codecast's Question
  3 problem inside loom itself.
- **The iOS client UX.** codecast's mobile app (`README.md` Mobile App section: session browsing,
  swipe-to-pin, push notifications, inbox parity with the web) demonstrates that fleet visibility on
  a phone is valuable independent of any orchestration engine underneath it. **What loom would
  build**: this is the one item that is *not* a small loom-tools patch — it needs a live-view server
  loom does not currently have. The realistic path is **not** a bespoke loom iOS app, but composing
  it on top of the already-approved safehouse comms layer (#3997, phase 1 emit-only): once agents
  emit sweep-state events into a safehouse room, any Matrix client (Element X, already cross-
  platform including iOS, per safehouse's own `README.md` architecture diagram) gets a
  "fleet-visibility-on-a-phone" experience for free, encrypted, without building or maintaining a
  native app. **Where it attaches**: downstream of #3997/#3998/#3999 (the safehouse fleet-comms
  phases), not a standalone loom project.

## Question 5 — License

**MIT (codecast) ↔ Apache-2.0 (safehouse) ↔ loom's own license — any interop concern?**

```
$ git show origin/main:LICENSE | head -2
MIT License

$ gh api repos/codecast-sh/codecast --jq '.license.spdx_id'
MIT

$ gh api repos/rjwalters/safehouse --jq '.license.spdx_id'
Apache-2.0
```

**Verdict: no blocker in any direction.** loom↔codecast is MIT↔MIT. MIT code is freely consumable by
an Apache-2.0 project. The only asymmetry worth naming: Apache-2.0 carries an express patent grant
that MIT does not, so the direction that would need a second look is *borrowing safehouse
(Apache-2.0) code into codecast (MIT)* — and that direction is not proposed by anything in this
evaluation or the borrow list above, all of which describes loom building its own small,
independent implementations rather than vendoring codecast or safehouse code either way.

## What loom (+ safehouse) already does better

A fair evaluation names where the fleet's existing bets pay off, not just where codecast has
features loom lacks:

- **Zero additional infrastructure to operate.** loom's coordination substrate is forge labels + git
  worktrees ([ADR-0006](../adr/0006-label-based-workflow-coordination.md)); safehouse's is a Matrix
  homeserver the operator already controls. Neither requires standing up a self-hosted Convex
  deployment (Postgres + Caddy + serverless functions, per `docs/SELF-HOSTING.md`'s ~1-hour,
  4-Railway-service setup) just to get fleet visibility.
- **Threat model matches the fleet's actual trust boundary.** safehouse's ciphertext-only server
  invariant is the correct default for a system whose whole point is agent-to-human handoffs over a
  network; codecast's plaintext-by-default sync (Question 3) would be a regression if adopted
  as-is.
- **Human-approval gate already exists.** loom's `loom:issue` label is an explicit human-approval
  point before autonomous work begins ([`CLAUDE.md`](../../CLAUDE.md)); codecast's `autopilot` is
  designed to run end-to-end without an equivalent gate once a plan is decomposed.
- **Deliberately conservative autonomy posture.** The autonomous work finder / epic supervisor /
  role runner in loom's own daemon are opt-in and default-off specifically to avoid uncontrolled
  auto-dispatch — the same failure mode `cast plan autopilot`'s always-on wave loop invites by
  design.

The honest counter-argument: loom really does lack a live, cross-sweep, human-friendly *dashboard*
(the daemon's MCP surface is queryable, not glanceable) and has no semantic memory across past
sweeps. Both are real gaps codecast's existence makes visible — they are just better addressed by
the narrow, local-first borrow-list items above than by adopting the platform.

## Recommended follow-ups (to be filed by a human/Curator — NOT filed here)

Per the issue's Correction 4 and this repo's #3707 concurrent-issue-creation hazard, these are
**listed only**; `gh issue list --search "codecast"` was not used to file anything during this run.

1. **Build the git-blame/issue-attribution reporting command** (Question 4, item 1). Small,
   self-contained, no new infrastructure. Good first `loom-tools` addition.
2. **Design a local-only, opt-in semantic search over `.loom/logs/` and PR history** (Question 4,
   item 2). Explicitly scope it local-first from day one to avoid re-opening the Question 3 problem
   inside loom's own tooling.
3. **Revisit the iOS/mobile-visibility idea only after #3997/#3998/#3999 (safehouse fleet comms)
   land** (Question 4, item 3). Not a standalone project; downstream of the coordination layer
   settling first.
4. **If safehouse ever adds its own session-search or attribution surface**, re-check this
   evaluation's Question 1 division of responsibility — the "codecast is read-only, safehouse is
   the source of truth" split assumed here could invert if safehouse grows the memory surface
   itself, at which point codecast becomes fully redundant rather than complementary.
5. **No action needed on codecast itself.** This evaluation does not recommend re-visiting codecast
   adoption on any fixed timer; a re-evaluation is only warranted if its maturity profile changes
   materially (e.g., an independent self-hosted deployment story matures, or E2E becomes the
   default rather than an "Enterprise feature").

## References

- Subject: [codecast-sh/codecast](https://github.com/codecast-sh/codecast) —
  [README](https://github.com/codecast-sh/codecast/blob/main/README.md),
  [`docs/orchestration-guide.md`](https://github.com/codecast-sh/codecast/blob/main/docs/orchestration-guide.md),
  [`docs/SELF-HOSTING.md`](https://github.com/codecast-sh/codecast/blob/main/docs/SELF-HOSTING.md),
  [`docs/GETTING-STARTED.md`](https://github.com/codecast-sh/codecast/blob/main/docs/GETTING-STARTED.md),
  [`docs/multi-client.md`](https://github.com/codecast-sh/codecast/blob/main/docs/multi-client.md),
  [`docs/plans-roadmap.md`](https://github.com/codecast-sh/codecast/blob/main/docs/plans-roadmap.md),
  [`packages/shared/encryption/README.md`](https://github.com/codecast-sh/codecast/blob/main/packages/shared/encryption/README.md),
  [`packages/web/app/(marketing)/security/page.tsx`](https://github.com/codecast-sh/codecast/blob/main/packages/web/app/(marketing)/security/page.tsx) —
  all retrieved 2026-07-27 via `gh api repos/codecast-sh/codecast/...`, not a local clone.
- Comparison subject: [rjwalters/safehouse](https://github.com/rjwalters/safehouse) —
  [README](https://github.com/rjwalters/safehouse/blob/main/README.md),
  `docs/design.md` (§2 Threat model, §4.1 the daemon) — retrieved 2026-07-27 via `gh api
  repos/rjwalters/safehouse/...`.
- Loom prior art: [ADR-0006](../adr/0006-label-based-workflow-coordination.md) (forge-as-state),
  [ADR-0009](../adr/0009-shepherd-deprecation.md) (central-brain deletion),
  [`CLAUDE.md`](../../CLAUDE.md) (label lifecycle, daemon operating posture),
  [`.loom/docs/daemon-reference.md`](../../.loom/docs/daemon-reference.md) (dispatch/work-finder
  model).
- Structural precedents followed: [`docs/notes/prometheus-comparison.md`](../notes/prometheus-comparison.md),
  [`docs/research/dynamic-workflows-evaluation.md`](dynamic-workflows-evaluation.md).
- Origin: issue [#4008](https://github.com/rjwalters/loom/issues/4008), Champion approvals citing
  the adjacent decision #3997 (safehouse fleet comms, phase 1 emit-only).
