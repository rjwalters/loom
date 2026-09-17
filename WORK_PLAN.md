# Work Plan

Prioritized roadmap of upcoming work, maintained by the Guide role.

<!-- Maintained automatically by the Guide triage agent. Manual edits are fine but may be overwritten. -->

<!-- guide:plan-body:start -->
## Operator Attention: Merge-Risk-Hold Pileup

Judge-approved PRs stuck under a `loom:operator` merge-risk hold — implementation work is done, only a human merge decision is missing.

- **#7969**: feat(daemon): port dep-recheck-fingerprint.sh; keep its 104 assertions as the proof (#7961)
- **#7981**: fix(guards): stop asking to approve --force-with-lease on feature branches (#7980)
- **#7989**: feat(daemon): closed ChatOps command enum + sender allowlist + confirm-nonce for inbound safehouse steering
- **#7998**: fix(merge-pr): pin the parent ref and warn instead of blocking stacked merges (#7982)

## Urgent

Issues flagged as highest priority (`loom:urgent`).

- **#4765**: feat(champion): opt-in flag to auto-merge Dependabot dependency PRs
- **#6544**: provision-hooks.sh emits unquoted ${CLAUDE_PROJECT_DIR} — every hook breaks when the project path contains a space
- **#7877**: classify-dependency-block.sh: _DEP_REF_WINDOW=60 too narrow, misclassifies a genuine dependency finding as merits-finding

## Ready

Human-approved issues ready for implementation (`loom:issue`).

- **#4765**: feat(champion): opt-in flag to auto-merge Dependabot dependency PRs
- **#6544**: provision-hooks.sh emits unquoted ${CLAUDE_PROJECT_DIR} — every hook breaks when the project path contains a space
- **#7708**: work_finder: dispatching into a pool with zero usable accounts produces a 4-host re-dispatch storm — 228 token-selection deaths in 4h, 39 lease comments on one issue; needs a sweep pre-flight + host-level exhaustion hold
- **#7923**: Index guard: distinguish inert read-tree documentation from executable shell wrappers
- **#7945**: Port sky130-modexp's mkdir/qsplit-continuation guard-hook fail-open fixes (#116/#121) to defaults/
- **#7971**: Builder/Doctor/Judge file side findings with no duplicate check — three agents filed the same bug in four minutes
- **#7972**: work_finder re-claims #7893 in a loop: 14 claims, 55 label events, 4 hours, zero PRs
- **#7977**: [epic #7810 PR 5] Resolve release artifacts natively; auto_update.rs stops shelling out to --resolve-json
- **#7986**: Guard: same-command mktemp safe-path denies the routine `VAR=$(cd "$VAR" && pwd -P)` realpath-canonicalization reassignment
- **#7994**: Migrate sweep_md_stage_minus_one_doc_lint.rs off prose-existence assertions (split from #7979)

## In Progress

Issues currently being built (`loom:building`).

- **#7877**: classify-dependency-block.sh: _DEP_REF_WINDOW=60 too narrow, misclassifies a genuine dependency finding as merits-finding
- **#7915**: test-isolation: pr_set_dispatch_exports_no_lease_renewal_marker fails when run from inside a sweep (ambient LOOM_SWEEP_LEASE_RENEW_DISPATCHED leaks into the child)
- **#7935**: SweepRegistry entry survives its child's death when the pid is recycled — blocks `restart --drain` and every auto_update roll
- **#7970**: guard: #7355 heredoc variable-capture masking has no $NAME read check (ask tier)
- **#7974**: loom-daemon: bind the IPC socket and start the heartbeat before the synchronous startup claim-reconciliation pass
- **#7993**: Migrate sweep_md_doc_lint.rs off prose-existence assertions to extract-and-execute tests (split from #7979)

## PRs Awaiting Review

PRs waiting on Judge (`loom:review-requested`).

- **#8014**: fix(create-issue): add a duplicate backstop at the single filing call site
- **#8016**: fix(guard): admit the exact mktemp-then-canonicalize chain in both same-command fast paths

## Approved (Awaiting Merge)

PRs that passed review and are queued for Champion auto-merge (`loom:pr`).

- **#7969**: feat(daemon): port dep-recheck-fingerprint.sh; keep its 104 assertions as the proof (#7961)
- **#7981**: fix(guards): stop asking to approve --force-with-lease on feature branches (#7980)
- **#7989**: feat(daemon): closed ChatOps command enum + sender allowlist + confirm-nonce for inbound safehouse steering
- **#7998**: fix(merge-pr): pin the parent ref and warn instead of blocking stacked merges (#7982)

## Proposed

Issues carrying `loom:curated`.

- **#4496**: [Epic #4489 Phase 7] Run a multi-account Codex daemon canary and define the production-readiness gate *(curated)*
- **#6544**: provision-hooks.sh emits unquoted ${CLAUDE_PROJECT_DIR} — every hook breaks when the project path contains a space *(curated)*
- **#7708**: work_finder: dispatching into a pool with zero usable accounts produces a 4-host re-dispatch storm — 228 token-selection deaths in 4h, 39 lease comments on one issue; needs a sweep pre-flight + host-level exhaustion hold *(curated)*
- **#7855**: robb-pro daemon stopped heartbeating and answering IPC for 25 min while still logging; the watchdog CONFIRMED the wedge twice and never restarted it *(curated)*
- **#7864**: Port 2AMLogic/sky130-modexp#117's resync-installed.sh local-divergence protection to defaults/ *(curated)*
- **#7877**: classify-dependency-block.sh: _DEP_REF_WINDOW=60 too narrow, misclassifies a genuine dependency finding as merits-finding *(curated)*
- **#7893**: [#4196 Phase 3a] Daemon ChatOps command enum + allowlisted senders + confirm-nonce for inbound safehouse steering *(curated)*
- **#7915**: test-isolation: pr_set_dispatch_exports_no_lease_renewal_marker fails when run from inside a sweep (ambient LOOM_SWEEP_LEASE_RENEW_DISPATCHED leaks into the child) *(curated)*
- **#7923**: Index guard: distinguish inert read-tree documentation from executable shell wrappers *(curated)*
- **#7935**: SweepRegistry entry survives its child's death when the pid is recycled — blocks `restart --drain` and every auto_update roll *(curated)*
- **#7945**: Port sky130-modexp's mkdir/qsplit-continuation guard-hook fail-open fixes (#116/#121) to defaults/ *(curated)*
- **#7970**: guard: #7355 heredoc variable-capture masking has no $NAME read check (ask tier) *(curated)*
- **#7971**: Builder/Doctor/Judge file side findings with no duplicate check — three agents filed the same bug in four minutes *(curated)*
- **#7972**: work_finder re-claims #7893 in a loop: 14 claims, 55 label events, 4 hours, zero PRs *(curated)*
- **#7974**: loom-daemon: bind the IPC socket and start the heartbeat before the synchronous startup claim-reconciliation pass *(curated)*
- **#7977**: [epic #7810 PR 5] Resolve release artifacts natively; auto_update.rs stops shelling out to --resolve-json *(curated)*
- **#7982**: merge-pr.sh should pin the parent ref and warn, instead of blocking every stacked-PR merge *(curated)*
- **#7986**: Guard: same-command mktemp safe-path denies the routine `VAR=$(cd "$VAR" && pwd -P)` realpath-canonicalization reassignment *(curated)*
- **#7993**: Migrate sweep_md_doc_lint.rs off prose-existence assertions to extract-and-execute tests (split from #7979) *(curated)*
- **#7994**: Migrate sweep_md_stage_minus_one_doc_lint.rs off prose-existence assertions (split from #7979) *(curated)*

## Proposed (Architect / Hermit)

- **#4167**: Proposal: first-class multi-runtime worker support (Claude Code, Codex, Amp, oh-my-pi) via a runtime adapter contract *(architect)*
- **#4196**: Proposal: safehouse room as the primary Loom operator interface (narrate → workers speak → steer → parity) *(architect)*

## Epics

- **#4489**: [Epic #4167 Phase 4] Routinely deploy Codex through loom-daemon with provider-aware account management
- **#6109**: Add a runtime-neutral scientific research lifecycle with evidence-gated phase contracts
- **#6896**: Epic: Session containers — persistent Codex auth, mandatory worker containment, and a remote-execution job seam
- **#7810**: [epic] Retire shell: 10 sequential PRs, two foundational then update + agent execution

## Backlog Balance

| Tier | Count |
|------|-------|
| Operator merge-risk holds | 4 |
| Urgent | 3 |
| Ready (`loom:issue`) | 10 |
| In Progress (`loom:building`) | 6 |
| PRs awaiting review | 2 |
| Approved PRs awaiting merge | 4 |
| Curated | 20 |
| Architect / Hermit proposals | 2 |
| Active epics | 4 |
<!-- guide:plan-body:end -->
