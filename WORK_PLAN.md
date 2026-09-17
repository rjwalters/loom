# Work Plan

Prioritized roadmap of upcoming work, maintained by the Guide role.

<!-- Maintained automatically by the Guide triage agent. Manual edits are fine but may be overwritten. -->

<!-- guide:plan-body:start -->
## Operator Attention: Merge-Risk-Hold Pileup

Judge-approved PRs stuck under a `loom:operator` merge-risk hold — implementation work is done, only a human merge decision is missing.

- **#7978**: fix(guards): port sky130-modexp mkdir/qsplit-continuation guard-hook fail-open fixes
- **#8019**: fix(guard): #7970 heredoc variable-capture masking has no $NAME read check (ask tier)

## Urgent

Issues flagged as highest priority (`loom:urgent`).

- **#4765**: feat(champion): opt-in flag to auto-merge Dependabot dependency PRs
- **#6544**: provision-hooks.sh emits unquoted ${CLAUDE_PROJECT_DIR} — every hook breaks when the project path contains a space
- **#7971**: Builder/Doctor/Judge file side findings with no duplicate check — three agents filed the same bug in four minutes

## Ready

Human-approved issues ready for implementation (`loom:issue`).

- **#4765**: feat(champion): opt-in flag to auto-merge Dependabot dependency PRs
- **#6544**: provision-hooks.sh emits unquoted ${CLAUDE_PROJECT_DIR} — every hook breaks when the project path contains a space
- **#7971**: Builder/Doctor/Judge file side findings with no duplicate check — three agents filed the same bug in four minutes
- **#7986**: Guard: same-command mktemp safe-path denies the routine `VAR=$(cd "$VAR" && pwd -P)` realpath-canonicalization reassignment
- **#7993**: Migrate sweep_md_doc_lint.rs off prose-existence assertions to extract-and-execute tests (split from #7979)
- **#8005**: Generalise #7818's credential-staging guard from two hardcoded gh-config paths to the credential-bearing class (.loom/tokens/, accounts.env, claude-config/)
- **#8006**: Two #7818 test assertions are insensitive to the fix they name (vacuous ls-tree check; group 2 executes a hardcoded pathspec, not the emitted one)
- **#8010**: Stacked-parent reconciliation is not reliably reachable after #7982's pin-and-warn downgrade

## In Progress

Issues currently being built (`loom:building`).

- **#7964**: auto_update: fall back to the mirrored ~/.local/share/loom-daemon/defaults copy when --resolve-json produces no JSON
- **#7972**: work_finder re-claims #7893 in a loop: 14 claims, 55 label events, 4 hours, zero PRs
- **#7974**: loom-daemon: bind the IPC socket and start the heartbeat before the synchronous startup claim-reconciliation pass
- **#7990**: loom-daemon status/health: surface the #7708 host-level token-pool exhaustion hold (the deferred AC4)
- **#7994**: Migrate sweep_md_stage_minus_one_doc_lint.rs off prose-existence assertions (split from #7979)
- **#7995**: guard: deny in-place writes to installed Loom files in a consumer repo (needs a repo-identity discriminator first)
- **#8001**: [Part of #7708] Broadcast the pool-exhaustion hold to peers via the peer-claim room
- **#8004**: land-resync-commit.sh treats an ALREADY-TRACKED credential path the same as an untracked one — the one state the #7818 incident left behind
- **#8026**: peer_coordination DEGRADED can be a false positive during a fleet-wide dispatch lull (advertise is dispatch-gated, not periodic)
- **#8032**: Characterize claude-wrapper.sh's retry policy before porting it: 1,675 lines, 0 dedicated tests

## PRs Awaiting Review

PRs waiting on Judge (`loom:review-requested`).

- **#8014**: fix(create-issue): add a duplicate backstop at the single filing call site
- **#8016**: fix(guard): admit the exact mktemp-then-canonicalize chain in both same-command fast paths
- **#8024**: test(loom-daemon): migrate sweep_md_doc_lint.rs off code-fence literal pins
- **#8033**: fix(security): hard-stop land-resync-commit.sh on an ALREADY-TRACKED credential path (#8004)
- **#8036**: test: characterize claude-wrapper.sh's retry policy before porting it (#8032)

## Approved (Awaiting Merge)

PRs that passed review and are queued for Champion auto-merge (`loom:pr`).

- **#7978**: fix(guards): port sky130-modexp mkdir/qsplit-continuation guard-hook fail-open fixes
- **#8019**: fix(guard): #7970 heredoc variable-capture masking has no $NAME read check (ask tier)

## Proposed

Issues carrying `loom:curated`.

- **#4496**: [Epic #4489 Phase 7] Run a multi-account Codex daemon canary and define the production-readiness gate *(curated)*
- **#6544**: provision-hooks.sh emits unquoted ${CLAUDE_PROJECT_DIR} — every hook breaks when the project path contains a space *(curated)*
- **#7855**: robb-pro daemon stopped heartbeating and answering IPC for 25 min while still logging; the watchdog CONFIRMED the wedge twice and never restarted it *(curated)*
- **#7864**: Port 2AMLogic/sky130-modexp#117's resync-installed.sh local-divergence protection to defaults/ *(curated)*
- **#7945**: Port sky130-modexp's mkdir/qsplit-continuation guard-hook fail-open fixes (#116/#121) to defaults/ *(curated)*
- **#7964**: auto_update: fall back to the mirrored ~/.local/share/loom-daemon/defaults copy when --resolve-json produces no JSON *(curated)*
- **#7970**: guard: #7355 heredoc variable-capture masking has no $NAME read check (ask tier) *(curated)*
- **#7971**: Builder/Doctor/Judge file side findings with no duplicate check — three agents filed the same bug in four minutes *(curated)*
- **#7972**: work_finder re-claims #7893 in a loop: 14 claims, 55 label events, 4 hours, zero PRs *(curated)*
- **#7974**: loom-daemon: bind the IPC socket and start the heartbeat before the synchronous startup claim-reconciliation pass *(curated)*
- **#7986**: Guard: same-command mktemp safe-path denies the routine `VAR=$(cd "$VAR" && pwd -P)` realpath-canonicalization reassignment *(curated)*
- **#7990**: loom-daemon status/health: surface the #7708 host-level token-pool exhaustion hold (the deferred AC4) *(curated)*
- **#7993**: Migrate sweep_md_doc_lint.rs off prose-existence assertions to extract-and-execute tests (split from #7979) *(curated)*
- **#7994**: Migrate sweep_md_stage_minus_one_doc_lint.rs off prose-existence assertions (split from #7979) *(curated)*
- **#7995**: guard: deny in-place writes to installed Loom files in a consumer repo (needs a repo-identity discriminator first) *(curated)*
- **#8001**: [Part of #7708] Broadcast the pool-exhaustion hold to peers via the peer-claim room *(curated)*
- **#8004**: land-resync-commit.sh treats an ALREADY-TRACKED credential path the same as an untracked one — the one state the #7818 incident left behind *(curated)*
- **#8005**: Generalise #7818's credential-staging guard from two hardcoded gh-config paths to the credential-bearing class (.loom/tokens/, accounts.env, claude-config/) *(curated)*
- **#8006**: Two #7818 test assertions are insensitive to the fix they name (vacuous ls-tree check; group 2 executes a hardcoded pathspec, not the emitted one) *(curated)*
- **#8010**: Stacked-parent reconciliation is not reliably reachable after #7982's pin-and-warn downgrade *(curated)*
- **#8026**: peer_coordination DEGRADED can be a false positive during a fleet-wide dispatch lull (advertise is dispatch-gated, not periodic) *(curated)*
- **#8028**: [epic #7810 PR 6a] Port the artifact fetch + verification; split Phase 6 into slices *(curated)*
- **#8037**: Port claude-wrapper.sh's six retry classifiers, with #8032's 44 assertions as the proof *(curated)*

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
| Operator merge-risk holds | 2 |
| Urgent | 3 |
| Ready (`loom:issue`) | 8 |
| In Progress (`loom:building`) | 10 |
| PRs awaiting review | 5 |
| Approved PRs awaiting merge | 2 |
| Curated | 23 |
| Architect / Hermit proposals | 2 |
| Active epics | 4 |
<!-- guide:plan-body:end -->
