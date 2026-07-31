# Survey: stablyai/orca — orchestration ideas applicable to Loom

**Type**: Research / idea-extraction survey (no code change, no infrastructure adopted)
**Source issue**: [#4775](https://github.com/rjwalters/loom/issues/4775)
**Subject**: [stablyai/orca](https://github.com/stablyai/orca) — an "AI Agent Orchestrator" /
Application Development Environment (Electron + TypeScript) that runs many CLI coding agents
(Claude Code, Codex, Cursor, etc.) in parallel git worktrees, with human-in-the-loop review.
**Date**: 2026-07-31

## Verification note (read this before the verdicts)

This session did not have live network/browsing access to `github.com/stablyai/orca` — no
`gh api repos/stablyai/orca/...` calls, no README fetch, no source read. Every factual claim
about Orca's behavior below is taken **verbatim from the six candidate-idea descriptions in
issue #4775's body**, which the issue itself describes as "already a reasonably detailed
summary of Orca's feature set." Nothing about Orca's implementation, file layout, or internals
is invented beyond what the issue states. Where a verdict depends on a specific claim (e.g.
"50+ terminal-compatible agents"), that claim is attributed to the issue body, not to firsthand
verification of the Orca repo. This is the same posture `docs/research/codecast-evaluation.md`
and `docs/research/dynamic-workflows-evaluation.md` took toward their subjects, adapted for the
case where even the `gh api` read-only route was unavailable this session.

## Scope and non-goals

Per the issue's own framing:

- **Not proposing to adopt Orca's architecture.** Orca is an interactive Electron ADE; Loom is
  a headless CLI + daemon with the forge as shared state. The ask is idea extraction, not
  convergence.
- **UI features are out of scope** — WebGL terminals, Design Mode click-to-inject, embedded
  Chromium. None of the six ideas below are UI-adoption proposals.
- **This survey does not itself change any code.** Its deliverable is this document plus
  scoped follow-up issues for every adopt/adapt verdict.

## Summary table

| # | Idea | Verdict | Follow-up |
|---|------|---------|-----------|
| 1 | Fan-out / best-of-N dispatch | **Adapt** | [#4776](https://github.com/rjwalters/loom/issues/4776) |
| 2 | Diff annotation routed back to agents | **Adapt** | [#4777](https://github.com/rjwalters/loom/issues/4777) |
| 3 | Remote/SSH worktrees | **Reject** | — |
| 4 | Mobile/remote monitoring and steering | **Reject** | — |
| 5 | Broad runtime support (50+ terminal-compatible agents) | **Adapt** | [#4780](https://github.com/rjwalters/loom/issues/4780) |
| 6 | CLI scripting surface (`orca worktree create`, `snapshot`, …) | **Adapt** | [#4778](https://github.com/rjwalters/loom/issues/4778) |

## Idea 1 — Fan-out / best-of-N dispatch

**Orca's claim (per issue body):** fan one prompt across five agents, each in its own isolated
worktree, compare the results, and merge the winner.

**Verdict: Adapt.**

Loom dispatches exactly one Builder per issue today, and that default should stay — but the
underlying primitive (worktree-isolated parallel attempts, judged by an independent reviewer)
already has a real, if narrow, precedent inside this repo on the **Judge** side, not the
Builder side: `docs/research/judge-fanout-measurement-runbook.md` and
`defaults/scripts/experiments/judge-fanout-*` implement an off-by-default
(`LOOM_JUDGE_FANOUT_EXPERIMENT=1`) fan-out of PR review across several reviewer perspectives,
with an adversarial-verify-then-reduce step, explicitly kept off the production sweep path.
That precedent means "fan out N independent attempts, judge picks a winner" is not a foreign
concept being imported wholesale from Orca — it is a pattern Loom has already built once, on
the review side, and never extended to the generation side. Extending it to Builder (N
worktrees generating N candidate PRs for one issue, Judge picks one, the rest are discarded) is
a natural, bounded adaptation: reuse `worktree.sh` isolation, reuse Judge's existing PR-review
role, and gate the whole thing behind an explicit opt-in (a sweep flag or label an operator
applies to issues that have already bounced through Doctor), not an automatic heuristic. The
real cost is token-pool interaction — N attempts consume N times the concurrency/token budget
of a normal sweep, which needs explicit accounting against `autonomous.maxConcurrent` /
`autonomous.perTokenConcurrency` (`.loom/docs/token-pool.md`) so a fan-out sweep doesn't starve
the rest of the fleet. That accounting, plus a measurement step (does N=3 actually beat N=1 on
historically-hard issues, mirroring the judge-fanout runbook's own "don't fabricate numbers,
measure first" discipline) is the right first increment, not a dispatch-level rewrite.

## Idea 2 — Diff annotation routed back to agents

**Orca's claim (per issue body):** comment on a generated diff and have the feedback flow to
the agent; the issue asks whether Orca's finer-grained per-hunk feedback loop suggests
improvements to how Doctor consumes review comments.

**Verdict: Adapt.**

Checking Loom's own Doctor → feedback path turned up a real, concrete gap rather than a
speculative one. `.loom/roles/doctor.md` reads PR feedback via
`gh api repos/{owner}/{repo}/issues/{N}/comments` and repeated `gh pr view <pr> --comments`
calls — both of these are GitHub's **top-level PR comment** surface. GitHub's separate
**inline/per-line review comment** endpoint (`pulls/{N}/comments`, the surface that produces
`#discussion_r<id>` URLs anchored to a specific diff hunk) is never fetched anywhere in Doctor's
role. In practice this gap is invisible for Loom's own Judge → Doctor loop, because CLAUDE.md
already mandates Judge post feedback via `gh pr comment` (a top-level comment, not `gh pr
review`) specifically to dodge GitHub's self-review API block — so Judge's own feedback always
lands where Doctor already looks. But a **human** reviewer who leaves an inline comment on a
specific hunk of a Loom-authored PR would have that feedback silently missed by Doctor today.
This is exactly the class of finer-grained, per-hunk feedback loop Orca's feature description
gestures at, and it maps to a small, additive, well-scoped fix (fetch one more GitHub API
endpoint) rather than a structural change — filed as #4777.

## Idea 3 — Remote/SSH worktrees

**Orca's claim (per issue body):** execution on a VPS with auto-reconnection, relevant to fleet
scaling beyond one host.

**Verdict: Reject.**

Loom already solves "run agents on more than one host" without needing a central controller to
SSH into workers, because the forge (labels + git) is the coordination layer, not a
process-management layer. Each host runs its own independent `loom-daemon`; hosts never need to
reach each other directly, because they coordinate exclusively through the shared forge state
(ADR-0006, forge-as-state) — the same architectural bet that motivated deleting the Python
shepherd's central-brain coordinator (ADR-0009). Orca's SSH-remote-worktree model exists because
Orca *is* the central controller — a single desktop app that must reach out to remote machines
to drive agents on them. Adopting that shape into Loom would mean building a control plane Loom
deliberately does not have and does not need: any host that can talk to the forge can already
participate in the fleet with zero SSH wiring. The genuine "auto-reconnection" resilience
concern Orca's description names is a real problem, but Loom's answer to it is host-local
(`loom-daemon`'s own restart/health primitives, `.loom/docs/machine-dispatcher.md`'s
`loom restart` drain-and-roll), not a cross-host SSH session that needs reconnecting in the
first place. Nothing here is worth building.

## Idea 4 — Mobile/remote monitoring and steering

**Orca's claim (per issue body):** a companion app is explicitly out of scope per the issue's
own non-goals, but the underlying idea of a lightweight remote view of running agents overlaps
with the opt-in fleet dashboard (`loom-daemon serve`).

**Verdict: Reject** (not a gap — already covered by tracked, in-progress work).

Two pieces of existing Loom infrastructure already target exactly this need, and duplicating
either with a new follow-up issue would fragment ownership rather than add value: (1) the
opt-in `loom-daemon serve` fleet dashboard (`.loom/docs/daemon-reference.md` §Fleet dashboard)
is already the "lightweight remote view of running agents" surface the issue describes, with a
documented `--peers` flag for multihost fan-out; and (2) the codecast evaluation
(`docs/research/codecast-evaluation.md`, Question 4) already worked through the same
"mobile visibility on a phone" idea in more depth than this issue asks for, and its
recommendation — build it on top of the safehouse comms layer (already rolled out and now the
PRIMARY Loom interface, per this repo's project memory) rather than a bespoke native app — is
the answer to Orca's mobile idea too, since both point at the same underlying gap (no live push
view). This survey deliberately does not re-litigate that prior evaluation or file a redundant
issue; anyone extending the dashboard's field set should read the codecast evaluation's borrow
list first, not this entry.

## Idea 5 — Broad runtime support (per issue body: "they claim 50+ terminal-compatible agents")

**Orca's claim (per issue body):** 50+ terminal-compatible agents; the issue asks whether this
implies anything Loom's own runtime adapters (`.loom/docs/runtime-adapters.md`) are missing.

**Verdict: Adapt** (the mechanism, not the breadth-first posture).

Loom's seven-point runtime adapter contract is deliberately stricter than "runs in a terminal" —
every admitted runtime needs a guardrail-parity doc with an explicit residual-gap section and a
CI smoke leg before it is trusted with Builder/Doctor (worktree-mutating) work. That rigor is
correct and should not be diluted; it is why Loom has exactly two admitted runtimes (Claude Code
tier-1, Codex tier-2) against Orca's claimed 50+. But the contract's existing capability-manifest
mechanism (`defaults/runtimes/<name>.json`, tri-state `yes|no|partial`, fails closed on
`"partial"`) already has everything needed to admit a much lower-trust **tier-3** class for
**read-only roles only** (Judge, Curator, Guide, Auditor) without weakening what Builder/Doctor
require: a runtime that declares `worktreeIsolation: "no"` is refused for any role requiring it,
by the exact same mechanism that already keeps Codex closed to Builder today. That gives Loom a
path to some of Orca's breadth — quickly wiring a new CLI for observation/review work — without
touching the trust bar for worktree-mutating roles. Filed as #4780, scoped to the mechanism (one
generic spawn contract + the capability gate) rather than a roster of specific CLI integrations.

## Idea 6 — CLI scripting surface (`orca worktree create`, `snapshot`, …)

**Orca's claim (per issue body):** an `orca worktree create` / `snapshot` CLI surface, to be
compared against `worktree.sh` / `loom-daemon` for ergonomics gaps.

**Verdict: Adapt** (one concrete gap found, not a general redesign).

`worktree.sh`'s existing verb set is broad and, on most axes, already ahead of what the issue
describes for Orca: `<issue-number>` (create), `remove` (sentinel-gated, dirty-guard-protected),
`--sparse`/`--full` (cone-mode checkout), `--check`, `--json` (machine-readable output), and
`--return-to`. The one gap that survives the comparison is Orca's `snapshot` verb: Loom has no
first-class way to capture a worktree's in-progress WIP as a standalone artifact. Today the only
general tool for that is `git stash` — and this repo's own project memory already documents a
real bug class from exactly that gap: `git stash` is **repo-global across worktrees**, so
concurrent builders stashing around the same time can pop each other's WIP
(`project_stash_cross_worktree_contamination.md`). A `worktree.sh snapshot <issue-number>`
verb that writes a **patch file** (not a stash-list entry) closes this gap by construction — a
patch file has no shared mutable state to collide on, unlike the stash list. This also composes
with the existing `check-main-clean.sh --quarantine` recovery flow, which already produces a
"replay this diff inside your worktree" artifact in a similar shape. Filed as #4778.

## What was explicitly out of scope for this survey

Per the issue's own non-goals, none of the following were evaluated as adoption candidates:
Orca's Electron/WebGL terminal UI, Design Mode click-to-inject, embedded Chromium, or its
overall architecture as a target for convergence. Loom remains a headless CLI + daemon with the
forge as shared state; nothing in this survey proposes changing that.

## References

- Source issue: [#4775](https://github.com/rjwalters/loom/issues/4775).
- Follow-ups filed from this survey: [#4776](https://github.com/rjwalters/loom/issues/4776)
  (best-of-N Builder fan-out experiment), [#4777](https://github.com/rjwalters/loom/issues/4777)
  (Doctor: ingest inline PR review comments), [#4778](https://github.com/rjwalters/loom/issues/4778)
  (`worktree.sh snapshot`), [#4780](https://github.com/rjwalters/loom/issues/4780) (tier-3 generic
  passthrough runtime adapter).
- Prior art consulted for existing Loom surfaces referenced above:
  `docs/research/judge-fanout-measurement-runbook.md`,
  `docs/research/codecast-evaluation.md`, `.loom/docs/runtime-adapters.md`,
  `.loom/docs/daemon-reference.md` (§Fleet dashboard, §`dispatch_sweep`), `.loom/docs/token-pool.md`,
  `.loom/docs/machine-dispatcher.md`, `.loom/roles/doctor.md`, `.loom/scripts/worktree.sh`,
  [ADR-0006](../../docs/adr/0006-label-based-workflow-coordination.md) (forge-as-state),
  [ADR-0009](../../docs/adr/0009-shepherd-deprecation.md) (central-brain deletion).
- Structural precedent followed: `docs/research/codecast-evaluation.md`'s
  verdict/rationale/borrow-list shape, adapted to this issue's per-idea adopt/adapt/reject format.
