# Work Plan

Prioritized roadmap of upcoming work, maintained by the Guide role.

<!-- Maintained automatically by the Guide triage agent. Manual edits are fine but may be overwritten. -->

<!-- guide:plan-body:start -->
## Operator Attention: Merge-Risk-Hold Pileup

Judge-approved PRs stuck under a `loom:operator` merge-risk hold — implementation work is done, only a human merge decision is missing.

_None._

## Urgent

Issues flagged as highest priority (`loom:urgent`).

- **#4765**: feat(champion): opt-in flag to auto-merge Dependabot dependency PRs
- **#7814**: add_worker.rs hard-codes operator identity defaults (feed egress sink URL, deny patterns) outside the #6650 scrub
- **#7815**: observability: refuse to export to reserved placeholder domains (example.com) instead of shipping the ingest key to them

## Ready

Human-approved issues ready for implementation (`loom:issue`).

- **#4765**: feat(champion): opt-in flag to auto-merge Dependabot dependency PRs
- **#6544**: provision-hooks.sh emits unquoted ${CLAUDE_PROJECT_DIR} — every hook breaks when the project path contains a space

## In Progress

Issues currently being built (`loom:building`).

- **#7318**: Guard friction: stash-scope:create-redirect recurs on bare 'git stash' inside issue worktrees despite a documented per-worktree alternative
- **#7440**: Guard telemetry: cloud-cli ASK on Auditor's own docker image cleanup (rmi) stalls headless runs
- **#7657**: Champion: close a proposal whose central premise is verified false instead of escalating it as an operator decision
- **#7705**: Version-bump commits are landing with Cargo.lock / mcp-loom/package-lock.json desynced from the bumped version
- **#7758**: Re-derive which bootstrap scripts must stay shell — fold the inventory into .loom/docs/file-size-policy.md's tier table (docs-only)
- **#7762**: Adopt a language policy: new logic in Rust, new shell only from a CI-enforced allowlist
- **#7792**: tokens_pool: empty_pool_error_enumerates_per_token_exclusion_detail flakes on the 4h/session-limit-window hour boundary
- **#7794**: Eight scripts read their own source for --help: concise usage block for the non-wrappers, interim single-read for the five loom-daemon-*.sh wrappers
- **#7795**: Guard ASK tier: which sites steer toward a safe alternative, and which are a bare 'are you sure?' that stalls headless runs
- **#7814**: add_worker.rs hard-codes operator identity defaults (feed egress sink URL, deny patterns) outside the #6650 scrub
- **#7815**: observability: refuse to export to reserved placeholder domains (example.com) instead of shipping the ingest key to them

## PRs Awaiting Review

PRs waiting on Judge (`loom:review-requested`).

- **#7899**: docs(builder-worktree): document stash-push/stash-pop clean-baseline pattern
- **#7900**: docs: correct the shell tier table with #7758's derived bootstrap inventory

## Approved (Awaiting Merge)

PRs that passed review and are queued for Champion auto-merge (`loom:pr`).

- **#7897**: chore: resync installed Loom surfaces

## Proposed

Issues carrying `loom:curated`.

- **#4496**: [Epic #4489 Phase 7] Run a multi-account Codex daemon canary and define the production-readiness gate *(curated)*
- **#6544**: provision-hooks.sh emits unquoted ${CLAUDE_PROJECT_DIR} — every hook breaks when the project path contains a space *(curated)*
- **#7318**: Guard friction: stash-scope:create-redirect recurs on bare 'git stash' inside issue worktrees despite a documented per-worktree alternative *(curated)*
- **#7440**: Guard telemetry: cloud-cli ASK on Auditor's own docker image cleanup (rmi) stalls headless runs *(curated)*
- **#7657**: Champion: close a proposal whose central premise is verified false instead of escalating it as an operator decision *(curated)*
- **#7705**: Version-bump commits are landing with Cargo.lock / mcp-loom/package-lock.json desynced from the bumped version *(curated)*
- **#7758**: Re-derive which bootstrap scripts must stay shell — fold the inventory into .loom/docs/file-size-policy.md's tier table (docs-only) *(curated)*
- **#7762**: Adopt a language policy: new logic in Rust, new shell only from a CI-enforced allowlist *(curated)*
- **#7792**: tokens_pool: empty_pool_error_enumerates_per_token_exclusion_detail flakes on the 4h/session-limit-window hour boundary *(curated)*
- **#7794**: Eight scripts read their own source for --help: concise usage block for the non-wrappers, interim single-read for the five loom-daemon-*.sh wrappers *(curated)*
- **#7795**: Guard ASK tier: which sites steer toward a safe alternative, and which are a bare 'are you sure?' that stalls headless runs *(curated)*
- **#7814**: add_worker.rs hard-codes operator identity defaults (feed egress sink URL, deny patterns) outside the #6650 scrub *(curated)*
- **#7815**: observability: refuse to export to reserved placeholder domains (example.com) instead of shipping the ingest key to them *(curated)*
- **#7819**: flaky: test-sync-labels-repo-flag.sh fails on an assertion whose needle IS in the printed output *(curated)*
- **#7895**: role-runner roster: generation high-water mark strands a peer when a late PATCH moves an observed expiry boundary into the future (#7691 follow-up) *(curated)*

## Proposed (Architect / Hermit)

- **#4167**: Proposal: first-class multi-runtime worker support (Claude Code, Codex, Amp, oh-my-pi) via a runtime adapter contract *(architect)*
- **#4196**: Proposal: safehouse room as the primary Loom operator interface (narrate → workers speak → steer → parity) *(architect)*
- **#7854**: [Epic #6896] Phase 4: migrate docker-requiring callers onto the run-job seam *(architect)*

## Epics

- **#4489**: [Epic #4167 Phase 4] Routinely deploy Codex through loom-daemon with provider-aware account management
- **#6109**: Add a runtime-neutral scientific research lifecycle with evidence-gated phase contracts
- **#6896**: Epic: Session containers — persistent Codex auth, mandatory worker containment, and a remote-execution job seam
- **#7810**: [epic] Retire shell: 10 sequential PRs, two foundational then update + agent execution

## Backlog Balance

| Tier | Count |
|------|-------|
| Operator merge-risk holds | 0 |
| Urgent | 3 |
| Ready (`loom:issue`) | 2 |
| In Progress (`loom:building`) | 11 |
| PRs awaiting review | 2 |
| Approved PRs awaiting merge | 1 |
| Curated | 15 |
| Architect / Hermit proposals | 3 |
| Active epics | 4 |
<!-- guide:plan-body:end -->
