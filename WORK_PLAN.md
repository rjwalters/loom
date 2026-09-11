# Work Plan

Prioritized roadmap of upcoming work, maintained by the Guide role.

<!-- Maintained automatically by the Guide triage agent. Manual edits are fine but may be overwritten. -->

<!-- guide:plan-body:start -->
## Operator Attention: Merge-Risk-Hold Pileup

Judge-approved PRs stuck under a `loom:operator` merge-risk hold — implementation work is done, only a human merge decision is missing.

- **#7425**: fix(guard): mask only the live-span lines of an unquoted heredoc body (#7421)
- **#7435**: feat(merge-pr): hard-block merge when loom:pr label is absent
- **#7436**: fix(guards): recognize git-registered worktrees nested under the main checkout (#7415)
- **#7438**: fix(tokens): re-probe monitor ranking rows frozen past their own reset (#7420)
- **#7444**: feat(spawn-claude): per-sweep container resource limits + containment observability
- **#7467**: fix(worktree): refuse stale-worktree reset when a live process holds it open

## Urgent

Issues flagged as highest priority (`loom:urgent`).

- **#7487**: Clippy fails on macOS: is_directory_or_descendant unused outside Linux-only find_processes_proc
- **#7488**: macOS worktree-safety: find_processes_lsof discards valid results when lsof exits 1 on an FD-only match

## Ready

Human-approved issues ready for implementation (`loom:issue`).

- **#7487**: Clippy fails on macOS: is_directory_or_descendant unused outside Linux-only find_processes_proc
- **#7488**: macOS worktree-safety: find_processes_lsof discards valid results when lsof exits 1 on an FD-only match

## In Progress

Issues currently being built (`loom:building`).

- **#7477**: Dispatch flapping: #7466/#7468 repeatedly claim-then-release between loom:issue and loom:building

## PRs Awaiting Review

PRs waiting on Judge (`loom:review-requested`).

- **#7489**: fix: gate is_directory_or_descendant to target_os=linux

## Approved (Awaiting Merge)

PRs that passed review and are queued for Champion auto-merge (`loom:pr`).

- **#7425**: fix(guard): mask only the live-span lines of an unquoted heredoc body (#7421)
- **#7435**: feat(merge-pr): hard-block merge when loom:pr label is absent
- **#7436**: fix(guards): recognize git-registered worktrees nested under the main checkout (#7415)
- **#7438**: fix(tokens): re-probe monitor ranking rows frozen past their own reset (#7420)
- **#7444**: feat(spawn-claude): per-sweep container resource limits + containment observability
- **#7467**: fix(worktree): refuse stale-worktree reset when a live process holds it open

## Proposed

Issues carrying `loom:curated`.

- **#4136**: measure: every sweep phase re-reads the repo from scratch — quantify the duplicated-read cost *(curated)*
- **#4496**: [Epic #4489 Phase 7] Run a multi-account Codex daemon canary and define the production-readiness gate *(curated)*
- **#5512**: Quarantine stashes accumulate with no lifecycle — 37 across one fleet, oldest 9 days, all referencing closed issues *(curated)*
- **#5660**: Vendored guard-destructive-generic.sh has drifted ~2,200 lines ahead of its upstream, and the single-marker capability probe makes partial reconciliation unsafe *(curated)*
- **#6245**: Guard ask-pattern false positive: printenv of an account-label env var denied by credential-exposure TOKEN pattern, blocks headless runs *(curated)*
- **#6544**: provision-hooks.sh emits unquoted ${CLAUDE_PROJECT_DIR} — every hook breaks when the project path contains a space *(curated)*
- **#6565**: Dogfood config: loom-repo curator starved 3d — runtime=codex admitted with no codex model configured (#5028 skip, DEBUG-silent) *(curated)*
- **#6646**: Sweep resync committed, rebased and bypass-pushed the primary clone's main while an operator session was active in that clone *(curated)*
- **#6650**: .loom/config.json commits a live Matrix room id and ingest URL — intentional, or move to the private overlay tier? *(curated)*
- **#6704**: Roster-driven role-runner shard assignment: reassign a dead host's slice within a bounded window (follow-up to #6374's static ring) *(curated)*
- **#6969**: auto_update drain-and-restart: one relaunch waited ~4 min for the watchdog instead of launchd (KeepAlive.SuccessfulExit) — single observation *(curated)*
- **#7356**: Guard friction: worktree-write-confinement-unresolved-var denies mktemp/tmp-scoped writes (44/126 = top guard-decision volume) *(curated)*
- **#7359**: merge=ours driver on .loom/install-metadata.json can silently drop non-loom_version field edits during rebase, uncaught by version-check-gate.sh *(curated)*
- **#7415**: Worktree-isolation guard blocks cp/mv into a registered worktree nested under the main checkout (.claude/worktrees/<name>) *(curated)*
- **#7419**: merge-pr.sh should refuse a PR that is not loom:pr unless explicitly overridden *(curated)*
- **#7420**: tokens check --ranking shows revoked accounts as exhausted with a reset date in the past instead of re-probing to auth-dead *(curated)*
- **#7421**: Guard false positive: worktree-write-confinement denies heredoc 'cat > /tmp/... <<EOF' scratch writes (Champion digest maintenance, 133 hits, top pattern) *(curated)*
- **#7430**: [Epic #6896] Phase 3: Per-sweep resource limits and containment observability *(curated)*
- **#7463**: Dispatch-time worktree prep can reset/clean a worktree while orphaned processes from a prior interrupted session are still writing into it *(curated)*
- **#7477**: Dispatch flapping: #7466/#7468 repeatedly claim-then-release between loom:issue and loom:building *(curated)*
- **#7481**: daemon: tmux health check logs 🚨 ERROR every tick when the loom tmux server is merely idle (no live sweeps) *(curated)*
- **#7487**: Clippy fails on macOS: is_directory_or_descendant unused outside Linux-only find_processes_proc *(curated)*

## Proposed (Architect / Hermit)

- **#3979**: Architecture: elastic compute — expand sweep parallelism onto cloud worker hosts when local CPU saturates *(architect)*
- **#4167**: Proposal: first-class multi-runtime worker support (Claude Code, Codex, Amp, oh-my-pi) via a runtime adapter contract *(architect)*
- **#4196**: Proposal: safehouse room as the primary Loom operator interface (narrate → workers speak → steer → parity) *(architect)*
- **#7431**: [Epic #6896] Phase 3: Fleet-default rollout — soak criteria, flip containment default on Linux fleet hosts *(architect)*

## Epics

- **#4489**: [Epic #4167 Phase 4] Routinely deploy Codex through loom-daemon with provider-aware account management
- **#6109**: Add a runtime-neutral scientific research lifecycle with evidence-gated phase contracts
- **#6896**: Epic: Session containers — persistent Codex auth, mandatory worker containment, and a remote-execution job seam

## Backlog Balance

| Tier | Count |
|------|-------|
| Operator merge-risk holds | 6 |
| Urgent | 2 |
| Ready (`loom:issue`) | 2 |
| In Progress (`loom:building`) | 1 |
| PRs awaiting review | 1 |
| Approved PRs awaiting merge | 6 |
| Curated | 22 |
| Architect / Hermit proposals | 4 |
| Active epics | 3 |
<!-- guide:plan-body:end -->
