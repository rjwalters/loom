# Work Plan

Prioritized roadmap of upcoming work, maintained by the Guide role.

<!-- Maintained automatically by the Guide triage agent. Manual edits are fine but may be overwritten. -->

<!-- guide:plan-body:start -->
## Operator Attention: Merge-Risk-Hold Pileup

Judge-approved PRs stuck under a `loom:operator` merge-risk hold — implementation work is done, only a human merge decision is missing.

- **#7467**: fix(worktree): refuse stale-worktree reset when a live process holds it open

## Urgent

Issues flagged as highest priority (`loom:urgent`).

- **#4765**: feat(champion): opt-in flag to auto-merge Dependabot dependency PRs
- **#6646**: Sweep resync committed, rebased and bypass-pushed the primary clone's main while an operator session was active in that clone
- **#7664**: watchdog: peer-coordination escalation needs hysteresis and dedup — refiles a tracking issue on every self-recovering flap

## Ready

Human-approved issues ready for implementation (`loom:issue`).

- **#4765**: feat(champion): opt-in flag to auto-merge Dependabot dependency PRs
- **#6646**: Sweep resync committed, rebased and bypass-pushed the primary clone's main while an operator session was active in that clone
- **#6650**: .loom/config.json commits a live Matrix room id and ingest URL — intentional, or move to the private overlay tier?
- **#6969**: auto_update drain-and-restart: one relaunch waited ~4 min for the watchdog instead of launchd (KeepAlive.SuccessfulExit) — single observation
- **#7659**: Proposer roles: cite only paths that exist in the dispatched workspace; sibling checkouts named in a repo docs are context, never a citation target
- **#7664**: watchdog: peer-coordination escalation needs hysteresis and dedup — refiles a tracking issue on every self-recovering flap
- **#7666**: champion: a passing, already-decomposed epic is escalated to the operator after repeated identical stand-down passes
- **#7678**: merge-pr.sh silently exits 1 on every PR without a champion:hold-state marker (regression in #7435/bf4f1965)
- **#7694**: Builder: probe for a live sibling worktree before worktree.sh touches files (generalizing #6765)
- **#7711**: Establish a file-size policy: ratchet oversized Rust/shell files, tier the shell→Rust port
- **#7718**: Extract inline #[cfg(test)] modules to sibling files in the eight largest Rust files
- **#7726**: Restructure sweep.md under progressive disclosure

## In Progress

Issues currently being built (`loom:building`).

- **#7431**: [Epic #6896] Phase 3: Fleet-default rollout — soak criteria, flip containment default on Linux fleet hosts
- **#7710**: docs: onboarding guides instruct commands that do not exist
- **#7717**: sync-labels.sh --check crashes on macOS stock bash 3.2 (local -A / ${x,,} are bash 4+)
- **#7719**: critical: merge-pr.sh silently aborts (exit 1, no output) for any loom:pr PR with no prior Champion hold (regression in #7435)
- **#7720**: Remove nonexistent 'gh label sync' from loom.md role prompt and the guard-hooks .md/.sh pair
- **#7739**: Add tables of contents to large reference docs so partial reads reveal scope
- **#7741**: Split oversized shell test suites (the two largest files in the size ledger)
- **#7749**: resync-installed.sh uses declare -A and silently degrades on macOS bash 3.2 (exit 0, wrong dead-pin output)
- **#7756**: classify-dependency-block.sh: 'prerequisite' phrase word false-positives a merits finding as a self-clearing dependency (regression risk: infinite re-evaluation loop)
- **#7763**: ci: revert the five heavy jobs to ubuntu-latest (#6624 routing measured net-negative)

## PRs Awaiting Review

PRs waiting on Judge (`loom:review-requested`).

- **#7670**: fix(champion): stop escalating a passing, already-decomposed epic to the operator
- **#7673**: docs(roles): citation-scope rules for hermit/architect; fix stale get_random_file example
- **#7680**: fix(watchdog): dedup peer-coordination escalations across the cooldown window (#7664)
- **#7682**: fix(merge-pr): guard hold_head assignment against pipefail abort
- **#7685**: feat(resync): land resync commits conservatively, never rebase/bypass-push (#6646)
- **#7699**: fix(config): replace live safehouse room / observability endpoint with placeholders
- **#7700**: Builder: probe for a live sibling worktree before worktree.sh touches files (generalizing #6765)
- **#7707**: fix(daemon): spawn a detached post-exit verifier on the auto-update drain-and-restart path
- **#7712**: docs: replace nonexistent commands in onboarding guides
- **#7714**: feat(ci): ratchet oversized source files instead of refactor-on-touch (#7711)
- **#7723**: refactor(daemon): extract inline test modules from the eight largest Rust files (#7718)
- **#7729**: fix: replace nonexistent 'gh label sync' in loom.md role prompt and the guard-hooks pair
- **#7733**: fix: make sync-labels.sh --check work on macOS stock bash 3.2
- **#7744**: feat(docs): generate and CI-check tables of contents for large reference docs (#7739)
- **#7764**: fix: classify-dependency-block.sh 'prerequisite' false-positive on narrative co-occurrence (#7756)
- **#7766**: fix: resync-installed.sh silently degrades on macOS bash 3.2 (declare -A, exit 0)
- **#7768**: ci: revert the five heavy jobs to ubuntu-latest (#6624 routing measured net-negative)
- **#7769**: docs(runtime-adapters): document containment fleet-default soak criteria and rollback path

## Approved (Awaiting Merge)

PRs that passed review and are queued for Champion auto-merge (`loom:pr`).

- **#7467**: fix(worktree): refuse stale-worktree reset when a live process holds it open
- **#7733**: fix: make sync-labels.sh --check work on macOS stock bash 3.2
- **#7750**: refactor(tests): split the 8.9k-line guard-destructive suite into 11 subject suites (#7741)

## Proposed

Issues carrying `loom:curated`.

- **#4136**: measure: every sweep phase re-reads the repo from scratch — quantify the duplicated-read cost *(curated)*
- **#4496**: [Epic #4489 Phase 7] Run a multi-account Codex daemon canary and define the production-readiness gate *(curated)*
- **#6544**: provision-hooks.sh emits unquoted ${CLAUDE_PROJECT_DIR} — every hook breaks when the project path contains a space *(curated)*
- **#6565**: Dogfood config: loom-repo curator starved 3d — runtime=codex admitted with no codex model configured (#5028 skip, DEBUG-silent) *(curated)*
- **#6646**: Sweep resync committed, rebased and bypass-pushed the primary clone's main while an operator session was active in that clone *(curated)*
- **#6650**: .loom/config.json commits a live Matrix room id and ingest URL — intentional, or move to the private overlay tier? *(curated)*
- **#6704**: Roster-driven role-runner shard assignment: reassign a dead host's slice within a bounded window (follow-up to #6374's static ring) *(curated)*
- **#6969**: auto_update drain-and-restart: one relaunch waited ~4 min for the watchdog instead of launchd (KeepAlive.SuccessfulExit) — single observation *(curated)*
- **#7431**: [Epic #6896] Phase 3: Fleet-default rollout — soak criteria, flip containment default on Linux fleet hosts *(curated)*
- **#7463**: Dispatch-time worktree prep can reset/clean a worktree while orphaned processes from a prior interrupted session are still writing into it *(curated)*
- **#7657**: Champion: close a proposal whose central premise is verified false instead of escalating it as an operator decision *(curated)*
- **#7659**: Proposer roles: cite only paths that exist in the dispatched workspace; sibling checkouts named in a repo docs are context, never a citation target *(curated)*
- **#7664**: watchdog: peer-coordination escalation needs hysteresis and dedup — refiles a tracking issue on every self-recovering flap *(curated)*
- **#7666**: champion: a passing, already-decomposed epic is escalated to the operator after repeated identical stand-down passes *(curated)*
- **#7678**: merge-pr.sh silently exits 1 on every PR without a champion:hold-state marker (regression in #7435/bf4f1965) *(curated)*
- **#7694**: Builder: probe for a live sibling worktree before worktree.sh touches files (generalizing #6765) *(curated)*
- **#7705**: Version-bump commits are landing with Cargo.lock / mcp-loom/package-lock.json desynced from the bumped version *(curated)*
- **#7711**: Establish a file-size policy: ratchet oversized Rust/shell files, tier the shell→Rust port *(curated)*
- **#7716**: [tracking] Budget agent-facing markdown by tokens *(curated)*
- **#7718**: Extract inline #[cfg(test)] modules to sibling files in the eight largest Rust files *(curated)*
- **#7726**: Restructure sweep.md under progressive disclosure *(curated)*
- **#7739**: Add tables of contents to large reference docs so partial reads reveal scope *(curated)*
- **#7741**: Split oversized shell test suites (the two largest files in the size ledger) *(curated)*
- **#7756**: classify-dependency-block.sh: 'prerequisite' phrase word false-positives a merits finding as a self-clearing dependency (regression risk: infinite re-evaluation loop) *(curated)*

## Proposed (Architect / Hermit)

- **#3979**: Architecture: elastic compute — expand sweep parallelism onto cloud worker hosts when local CPU saturates *(architect)*
- **#4167**: Proposal: first-class multi-runtime worker support (Claude Code, Codex, Amp, oh-my-pi) via a runtime adapter contract *(architect)*
- **#4196**: Proposal: safehouse room as the primary Loom operator interface (narrate → workers speak → steer → parity) *(architect)*

## Epics

- **#4489**: [Epic #4167 Phase 4] Routinely deploy Codex through loom-daemon with provider-aware account management
- **#6109**: Add a runtime-neutral scientific research lifecycle with evidence-gated phase contracts
- **#6896**: Epic: Session containers — persistent Codex auth, mandatory worker containment, and a remote-execution job seam

## Backlog Balance

| Tier | Count |
|------|-------|
| Operator merge-risk holds | 1 |
| Urgent | 3 |
| Ready (`loom:issue`) | 12 |
| In Progress (`loom:building`) | 10 |
| PRs awaiting review | 18 |
| Approved PRs awaiting merge | 3 |
| Curated | 24 |
| Architect / Hermit proposals | 3 |
| Active epics | 3 |
<!-- guide:plan-body:end -->
