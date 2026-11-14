# Loom roadmap

Loom orchestrates AI coding agents through forge labels: agents claim issues,
build, review and merge, either under `loom-daemon` or in operator-directed
sessions.

Status as of 2026-11-13, `main` @ `96f505398` (version 0.19.1299).

This file gives direction and ordering. The live queue is the generated
[`WORK_PLAN.md`](../WORK_PLAN.md); completed work is in
[`WORK_LOG.md`](../WORK_LOG.md). Issue states are as read on 2026-11-13
(103 open issues, 20 open PRs).

## Where it stands

- **Three operating modes are defined; session mode has recorded gaps.**
  [`operating-modes.md`](operating-modes.md) sets the contract for fleet, local
  daemon and operator-directed sessions. It also records that several helpers
  still need the `loom-daemon` executable when no daemon runs
  (`defaults/scripts/worktree.sh`, `defaults/scripts/lib/forge-helpers.sh`).
- **`loom-tools` exists; most commands have not moved.** Epic #12708 Phases 1
  and 2 are delivered: the design is
  [`design/loom-tools-cli-boundary.md`](design/loom-tools-cli-boundary.md), the
  binary is `loom-daemon/src/bin/loom_tools.rs`, and
  `defaults/scripts/tests/test-tools-first-clean-install.sh` covers a clean
  install with no daemon. It exposes `strip-ansi`, `checkpoint`, `config`,
  `release-fetch`, `install-binary`, `resync-payload` and `capabilities`.
  `resolve-model`, `claim`, `validate-phase`, the worktree family, `forge` and
  `lease` (section 5 of the design) are still `loom-daemon` commands.
- **Discovery reads one snapshot per tick.** Under epic #12304, the children
  #12310, #12311, #12312, #12313, #12315, #12316 and #12317 are closed. Left:
  #12314 (quarantine reconciliation) and the cleanup #12320. The epic body's
  checklist still shows several of these unchecked.
- **`merge-pr` is half-way to its minimal contract.** Tracker #12063: slice 1
  (#12122) and slice 2 (#12287) landed; `defaults/scripts/merge-pr.sh` is 1,615
  lines against 3,114 when the plan was written. Slices 3 to 7 have no issue.
  Merge-queue mode (#9978) is closed as completed.
- **Fleet version floor.** Epic #10698 is open; its acceptance check #12418 is
  blocked on live receipts.
- **Dead-code removal is the current steady stream.** Merged on 2026-11-13:
  #13823, #13843, #13852 and #13842 (part of #13827).

## Now

Can start today. Claimed items (`loom:building`: #13800, #13814, #13822,
#13841, #13848, #13850, #13851, #13857, #13860) are not repeated.

| Item | Done when | Issue |
| --- | --- | --- |
| Route every read through `GhInvocation` so none lands on the writer | no raw `run_gh` read remains outside the routing layer; a test fails if one is added | #13744 (`loom:important`; still carries `loom:curating`) |
| Report a writer rate limit as a rate limit | a lease claim refused on writer budget names the rate limit, and the finder stops drawing steps it must refuse; both covered by tests | #13746 (still `loom:curating`) |
| Stop the Guide backfill writing another repo's history | a backfill run against a fixture with two repos writes only the current repo's entries; the docs-only fast path rejects the fixture's bad diff | #13760 (still `loom:curating`) |
| Export peer-coordination verdict and degraded duration on `host.health` | both attributes appear on the emitted record, asserted by a unit test | #12167 |
| One env-truthy parser | the 47 inline copies call `env_flag::parse_truthy`; `git grep` for the inline pattern in `loom-daemon/src` returns none | #13859 |
| One PID liveness probe | the seven private `kill(pid, 0)` probes call the shared `is_pid_alive`; a test pins the `EPERM` answer | #13862 |
| Finish removing file-size ratchet comments | `git grep -i 'file-size ratchet'` returns no stale citation of the gate #12099 deleted (181 matching lines remain after #13842) | #13827 |
| Success-path run for orchestrator branch isolation | `test-pr-worktree-isolation.sh` runs a real success path and asserts the branch | #13854 (unlabelled) |
| Champion Step 0.5 reads children from a markdown table | a tracking epic whose children are table rows stands down; fixture test added | #13874 (unlabelled) |
| `loom-tools resolve-model` | `loom-tools resolve-model` matches `loom-daemon resolve-model` on output and exit codes 0, 2 and 3, with no daemon installed | none (seed 1); part of #12708 Phase 3 |
| Bring `docs/README.md` up to date | its tables list every directory and top-level file under `docs/`, and the ADR range matches `docs/adr/` | none (seed 2) |

## Next

Unblocked once a named item lands.

- **`loom-tools claim`, then `validate-phase`.** After `resolve-model`: shared
  CLI edits are serialized (design section 6.4). Done when each command passes
  the same exit-code assertions as its `loom-daemon` form. Epic #12708.
- **`loom-tools` worktree, `forge` and `lease` families, then the role, skill
  and wrapper switch-over.** Order is fixed by #12708: reads before writes,
  with `merge-pr` coordinated through #8191. Done when the Phase 3 children are
  closed and a session-mode run uses no legacy-daemon fallback.
- **`merge-pr` slice 3 (post-merge extras move into the daemon).** Needs
  re-scoping against current `main` before it is filed: the plan folded #10150
  into it, and #10150 has since closed as not planned. Done when the slice
  issue exists under #12063 with its own acceptance criteria. #10838 closes
  with it.
- **Model-scoped exhaustion marks and a one-rung model fallback.** #13872 and
  #13873 are in `loom:triage`; buildable after curation.
- **Close out discovery.** #12320 deletes the per-call `forge_mirror` path once
  every worker reads the worklist; #12314 first.
- **Retired hold-label readers.** #12248, #12249, #12250 (the three parts of
  #11843), in that order.

## Later

- **`loom-tools` Phase 4: retire migrated wrappers.** First: have all
  consumers moved, and what is the compatibility window?
- **`merge-pr` slices 4 to 6 and the single Rust subcommand (#8191).** First:
  does slice 3 land as planned or get reduced?
- **Cut forge API usage (epic #10332).** First: what does usage measure now
  that discovery reads one snapshot?
- **Daemon memory on Linux (#10420).** First: how much of the baseline is
  `ci_telemetry`, per `notes/ci-telemetry-memory-attribution-11114.md`?
- **Remove the chat integration (#11112).** First: rebuild or replace the
  conflicting PR #11717? #13071 depends on the answer.
- **Additional harnesses and seats (#4489, #11287, #11283).** First: live
  receipts (#12978, #11308, #12395).
- **Stacking conflicting PRs (#12757).** Frozen; needs a decision to thaw.

## Blocked and waiting

47 open issues carry `loom:blocked` and 23 carry `loom:operator`.

- **Approved PRs held for a merge decision:** #12969, #13582, #13802, #13816,
  #13830, #13847, #13858. Three of them address in-progress issues (#13800,
  #13814, #13822).
- **Review-requested PRs under a hold:** #12631, #12637 (for #12604), #12639
  (for #12638). Their issues are labelled ready but will not be drawn while
  the PR is parked.
- **Parked PRs needing rework or a ruling:** #11305 (for #11107), #11717,
  #12000, #12381, #12491.
- **Removal proposals awaiting approval:** #13776, #13818, #13820, #13829,
  #13831, #13832, #13833, #13861, #13867, #13871.
- **Ratification lane:** #13734, #13736, #13762, #13763.
- **CI telemetry owner discovery:** #12909, #13640.
- **Operator-held findings:** #12993, #13306, #13378, #13521, #13538, #13668.
- **Live verification that needs a running fleet or a manual run:** #8434,
  #8698, #12408, #12418, #13078, #13590, #13661, #13845.
- **Outside this repo:** the mirror side of #12304; `merge-pr` slice 7 (merge
  queue) waits on the org move recorded in #12063.

## Not planned

- A hand-maintained queue here: `WORK_PLAN.md` stays the queue of record.
- New hand-curated `CHANGELOG.md` entries (policy note at the top of that
  file); release notes are generated.
- New refresh, retry, re-date or re-wait paths in `merge-pr` (#12063).
- A version pin or maximum for the fleet floor; the floor is fix-forward only
  (#10698).
- Extracting a new crate for `loom-tools`, or a token-vending service for
  local modes (#12708).
