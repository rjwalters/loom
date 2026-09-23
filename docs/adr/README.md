# Architecture Decision Records (ADRs)

This directory contains Architecture Decision Records (ADRs) for the Loom project. ADRs document significant architectural decisions, their context, consequences, and alternatives considered.

## What is an ADR?

An Architecture Decision Record captures an important architectural decision made along with its context and consequences. It helps:
- New contributors understand "why" decisions were made
- Track the evolution of architectural thinking
- Reference specific design choices in issues and PRs
- Avoid re-litigating past decisions

## ADR Index

### Core Architecture

- [ADR-0001: Observer Pattern for State Management](0001-observer-pattern-state-management.md)
  - **Status**: Superseded (frontend removed in v0.9)
  - **Summary**: Use Observer Pattern with Map-based store for decoupled state management
  - **Key Decision**: Prefer Observer Pattern over Redux/MobX for simplicity and learning value

- [ADR-0008: tmux + Rust Daemon Architecture](0008-tmux-daemon-architecture.md)
  - **Status**: Accepted
  - **Summary**: Two-tier architecture with Rust daemon managing tmux sessions
  - **Key Decision**: Use tmux for persistence and Rust for performance over Node.js or embedded terminals

### Configuration & State

- [ADR-0003: Separate Configuration and State Files](0003-config-state-file-split.md)
  - **Status**: Accepted
  - **Summary**: Split `.loom/config.json` (user preferences) and `.loom/state.json` (runtime state)
  - **Key Decision**: Separate concerns for safer restarts and independent schema evolution

### Workflows & Coordination

- [ADR-0004: Git Worktree Paths Inside Workspace](0004-worktree-paths-inside-workspace.md)
  - **Status**: Accepted
  - **Summary**: Create all git worktrees inside `.loom/worktrees/` for sandbox compatibility
  - **Key Decision**: Sandbox-safe paths inside workspace over external directories

- [ADR-0006: Label-Based Workflow Coordination](0006-label-based-workflow-coordination.md)
  - **Status**: Accepted
  - **Summary**: Use GitHub labels as state machine for agent workflow coordination
  - **Key Decision**: Leverage GitHub labels over database, message queue, or file-based queue

- [ADR-0019: The `loom:curating` Label Claim Is Sufficient Serialization for the Dep-Recheck Post](0019-dep-recheck-post-serialization.md)
  - **Status**: Accepted
  - **Summary**: Records why Curator's dep-recheck posting sequence keeps a plain (non-atomic) `loom:curating` label claim rather than the POSIX-atomic `mkdir` lock plus under-lock re-decide of the retired `premise-recheck.sh` — the duplicate-comment incidents that motivated the lock were hours-to-weeks apart and were fully explained by hash non-determinism (since fixed by #7281/#7362/#8254), not by any claim race
  - **Key Decision**: Accept the non-atomic forge-side claim, with a falsifiable revisit trigger (duplicate same-hash re-check comments less than ~1 minute apart), over an emulated CAS that would add forge calls on a hot path under live quota pressure to prevent one self-suppressing duplicate comment

### Orchestration Architecture

- [ADR-0009: Deprecate and Delete Shepherd Brain and Python Daemon (Phase 3)](0009-shepherd-deprecation.md)
  - **Status**: Accepted
  - **Summary**: Delete `loom_tools/shepherd/` (~16.8k LOC) and `loom_tools/daemon_v2/` (~4.7k LOC); replace with spawn loop + GitHub Actions cron + `/loom:sweep`
  - **Key Decision**: Forge-as-state-machine + stateless components over persistent Python orchestration brain

- [ADR-0010: Rebuild Daemon Mode as Rust Binary with MCP-Tool Surface (v0.10.0)](0010-daemon-rebuild.md)
  - **Status**: Accepted
  - **Summary**: Extend the existing Rust `loom-daemon` binary with named sweep dispatch, a pub/sub event bus, and MCP monitoring tools instead of restoring the deleted Python brain
  - **Key Decision**: MCP-tool surface on the existing daemon binary over a restored Python brain or a shell-level `daemon.sh` wrapper

- [ADR-0013: Retire the Python `loom-tools` Package — One Rust Binary Plus Bash](0013-loom-tools-python-retirement.md)
  - **Status**: Accepted
  - **Summary**: Delete the ~31.8k-line Python `loom_tools` package over four phases (epic #4081), moving its load-bearing functionality into `loom-daemon` subcommands while every shell entry point keeps its name and flags; motivated by #4079, where a stale editable pip install's frozen console scripts shadowed the Rust binary on PATH
  - **Key Decision**: One commit-stamped Rust artifact plus bash over a maintained pip install, with a byte-compatible on-disk state contract holding across every phase so no cutover needed a flag day; `loom-search` carved out of the deletion (opt-in, no native port, no test would have caught its removal)

- [ADR-0014: Decouple Forge API Cost From Coordination Chatter — Local Evaluation Memo, Safehouse as Accelerator Only](0014-forge-coordination-decoupling.md)
  - **Status**: Accepted (design decision; implementation phased into follow-up issues)
  - **Summary**: The forge's GraphQL/REST quota is spent on repeat evaluation of unchanged state, not on the inherent label transitions of a normal issue lifecycle; answers the four open questions from #5057 on where an "already evaluated" memo lives, whether a webhook-fed Worker becomes a control-plane participant, what the memo's input-hash should be, and whether the label protocol changes
  - **Key Decision**: A daemon-local evaluation memo (per-role content hash, not `updated_at`) is the store of record, with safehouse as an optional best-effort cross-host broadcast accelerator — never the store itself; claims stay forge-authoritative and the label protocol is untouched; defer webhook/Worker-as-control-plane (Lever C) until the local memo + safehouse broadcast are measured and shown insufficient

- [ADR-0015: In-Builder Test-First Checkpoint — PR-Body Signal, Advisory on Absence, Blocking on Contradiction](0015-builder-test-first-checkpoint.md)
  - **Status**: Accepted
  - **Summary**: Adapts damusix/atomic-claude's maker/checker TDD split (#5844/#5849) into a required `TDD:` line in the PR body's Test Plan section, checkable by Judge against the diff
  - **Key Decision**: A PR-body prose line, not a commit-order check (Loom's squash-merge workflow makes commit order frequently unobservable); Judge notes an absent line or a plausible `TDD: no` reason advisory-only, but treats a `TDD: yes` claim contradicted by the diff as blocking — the same class of finding as any other inaccurate PR claim; not a `buildGate` extension, since classifying plausibility requires judgment `buildGate` is designed to exclude

- [ADR-0016: Write-Target Confinement Design — Bounded Tokenization Plus Reused Same-Command Literal Declaration, No Control-Flow Inference](0016-write-target-confinement-approach.md)
  - **Status**: Accepted (design decision; implementation deferred to Phase 2, epic #6172)
  - **Summary**: Chooses the redesign approach for the worktree-isolation guard's variable-rooted write-target analysis after PR #5397's `for`-loop carve-out produced three sequential, independently-confirmed bypasses; also root-causes and documents a live, previously unreported unsound `sed`-argument false-negative found during the investigation
  - **Key Decision**: Retire all control-flow-scoped value inference (loops/conditionals/branches) permanently — that category, not any one implementation of it, produced #5397's bypasses. Keep bounded per-idiom argument-position tokenization (restricted Option A), and reuse the already-shipped, already-fail-closed same-command literal-assignment resolver (`record_assign()`/`resolve_var()`, #4881) as the one sanctioned declaration mechanism (Option B) — it can only remove *ambiguity*, never weaken the containment verdict, so a declaration can never grant an allow beyond what a literal path would already get. #6123/#6110's residual friction (already-known main-checkout targets) is explicitly out of scope: this design changes nothing about targets that are already resolved

- [ADR-0018: Rust Owns Behavior; Shell Exists Only to Reach It](0018-rust-owns-behavior-shell-reaches-it.md)
  - **Status**: Accepted (operator ruling 2026-09-16; no implementation ships with the ADR — downstream cluster is epic #7810, #7758, #7762, #7794)
  - **Summary**: States the responsibility boundary for the ~45%-done, previously undocumented migration of `defaults/scripts/` behind `loom-daemon`: shell may only locate the executable, report if it is missing, and `exec` it; everything that interprets arguments, inspects state, decides, or handles failure lives in Rust. Records the evidence the ruling rested on (a 8,976-line guard hook on the hot path of every Bash call, 49 of 101 commits to it in the code-vs-quoted-data class, seven bash-3.2 breakages in a day) and the author's caveat that porting removes interpreter-compatibility and quoting defect classes but does not fix concurrency or wall-clock flakiness
  - **Key Decision**: Responsibility, not line count or grep, is the enforcement axis — finished wrappers are held to a mechanically checkable template, shared bootstrap helpers are reviewed separately, and legacy implementations stay a tracked backlog; the execution boundary must be lossless (spawn failure, exit code, signal, invalid UTF-8 all preserved) and shim↔binary skew is guarded by a local `loom-daemon supports <capability>` probe shipped before any wrapper switches; sequencing is execution contract → compatibility contract → one complete migration → broader migration, proven through existing call sites rather than a standalone framework

- [ADR-0021: The Forge Event Plane — GitHub App Webhook → Operator-Owned Fan-Out Worker → `loom-daemon` Reactions (Lever C, Shipped Conservatively)](0021-forge-event-plane.md)
  - **Status**: Accepted (operator decision 2026-09-23; supersedes only ADR-0014's Lever C deferral)
  - **Summary**: Ships ADR-0014's parked Lever C after the operator asked for push-driven GitHub interactions: a GitHub App webhook feeds an operator-owned Cloudflare Worker (HMAC-verified, allowlisted, deduped, cursor-fed, redelivery-swept — the proven `bot-issues` pattern as a sibling), and a new opt-in `forge_events` module in `loom-daemon` (Rust, off-by-default) plays the stream back to each host behind a durable per-host cursor, journaling events and prompting early re-check ticks on the existing poll loops
  - **Key Decision**: The Worker becomes a member of the *event* plane but never of the *control* plane — an event may only prompt a daemon to re-verify against the forge (a wake is a prompt, not a truth); claims stay forge-authoritative, every polling cadence stays the correctness floor, and total event-plane failure behaves byte-identically to today; Loom ships the daemon half and the transport contract, the Worker stays operator infrastructure

### CI Infrastructure

- [ADR-0011: CI Runner Platform — Speedup Ceiling and Decision](0011-ci-runner-platform.md)
  - **Status**: Accepted
  - **Summary**: Measured that compile is only ~10% of `ci.yml`'s critical path while one serial-locked `loom-daemon` test binary is ~67%, and that its ~127s serial-lock floor is not reducible by adding cores
  - **Key Decision**: Reject new CI hardware on speed grounds alone; default to Graviton (arch parity) if a runner is ever provisioned (#4057) while treating full macOS parity as a separate, independently-costed decision; prioritize de-serializing `#[serial]` tests over buying cores

### Worker Runtime

- [ADR-0012: Multi-Runtime Worker Support via a Single Runtime Adapter Contract](0012-runtime-adapter-contract.md)
  - **Status**: Accepted
  - **Summary**: Support multiple CLI agent runtimes (Claude Code, Codex, Amp, oh-my-pi) through one seven-point adapter contract instead of per-runtime parallel scripts; collaborate via upstream PRs from the gpeyton/loom fork, not cherry-picks
  - **Key Decision**: A single runtime adapter contract (`defaults/docs/runtime-adapters.md`) with Claude Code as adapter #1/default/tier-1 (zero regression) and non-Claude runtimes tier-2 (CI-gated) over parallel per-runtime special-casing

- [ADR-0017: Session-Container Architecture — Two Lifetimes, Headless-Exec Dispatch, Remote-Execution Nested Compute, Fleet-Default Rollout](0017-session-container-architecture.md)
  - **Status**: Accepted
  - **Summary**: Records the settled architecture for epic #6896 (Session containers): two container lifetimes behind one dispatch seam, headless `docker exec` dispatch with tmux as an operator-only re-auth surface, docker-requiring nested compute routed through a remote-execution `run-job` seam (never docker-in-docker, never docker.sock passthrough), and a soak-then-fleet-default rollout posture — plus the #5119 drain-vs-hard-stop contract's extension to per-sweep containers
  - **Key Decision**: Per-account persistent session containers for mutable-auth runtimes (Codex) and per-sweep ephemeral containers for stateless-auth runtimes (Claude), both dispatched through the existing `spawn-worker.sh` seam with no new dispatch path; nested docker workloads never get a docker socket, routing instead through a host-level `run-job` executor; containment becomes the Linux-fleet default after a soak period while bare-metal stays available config-selectably

- [ADR-0020: Govern a Shared Metered API Key With a Provider-Side Spend Ceiling, Not Fleet Aggregation](0020-fleet-metered-spend-ceiling.md)
  - **Status**: Accepted
  - **Summary**: Records the #8556 decision for a metered OpenAI-compatible key shared across every fleet host, where no per-host mechanism can bound aggregate spend (N hosts × a local ceiling of K is not a spend cap): a provider-side hard ceiling that *fails the launch* is the primary mechanism, observability-backend aggregation is a scoped fallback promoted only if a chosen provider offers alerting rather than a hard stop, and tap-attributed usage accounting — `(runtime, credential source)` — lands first because both options consume it and it answers the backstop-vs-subscription spend question on its own
  - **Key Decision**: The authoritative mechanism is the only one that can be authoritative (the provider refuses the request) over a fleet-side counter that is eventually consistent by construction, couples dispatch to backend uptime when fail-closed, and defeats its own purpose when fail-open; if the fallback is ever built it fails closed on the **metered tap only**, with a bounded staleness window rather than indefinite trust in a last-known-good counter

## Creating a New ADR

When making a significant architectural decision:

1. **Copy the template**:
   ```bash
   cp template.md NNNN-short-title.md
   ```

2. **Number sequentially**: Use the next available number (e.g., 0009)

3. **Fill in all sections**:
   - **Context**: What problem are we solving?
   - **Decision**: What did we decide?
   - **Consequences**: What are the tradeoffs?
   - **Alternatives**: What else did we consider and why reject it?

4. **Update this README**: Add your ADR to the index above

5. **Reference in code**: Link to ADR in relevant files using comments

## ADR Status

- **Proposed**: Under discussion, not yet accepted
- **Accepted**: Decision approved and implemented
- **Deprecated**: No longer recommended, but not yet superseded
- **Superseded**: Replaced by a newer ADR (link to replacement)

## Format

ADRs use a lightweight format:
- Markdown for easy reading and version control
- Numbered sequentially for stable references
- Grouped by topic in this index for discoverability

See [template.md](template.md) for the full ADR template.

## References

- Michael Nygard's ADR: http://thinkrelevance.com/blog/2011/11/15/documenting-architecture-decisions
- GitHub ADR Organization: https://adr.github.io/
