# Work-Finder Step Dispatch: Item First, Then Role (Issue #11894)

This is the design doc for the work-finder overhaul. It replaces "list `loom:issue` and spawn a whole-lifecycle `/loom:sweep`" with **item-first step dispatch**:
1. draw a workspace;
2. draw an item from it by a seeded weighted draw;
3. advance that item **one step**, with the role its state calls for.

Support roles stop running on fixed intervals for item work, and the daemon stops spawning `/loom:sweep`.

**Status: Accepted (2026-10-14).** The operator accepted the design. Section 1 records the rulings, and section 9 records the items that stay open. This doc changes documentation only. The children listed in section 10 change the code.

This is a repo-local design doc under `docs/design/`. It is never installed into consumer repos. The format follows [`persistence-inventory.md`](persistence-inventory.md).

**Source SHA.** Every citation is against `7e6e346e1` (`main`, 2026-10-14). Paths without a prefix are under `loom-daemon/src/`. Paths under `defaults/`, `docs/` and `mcp-loom/` are written in full.

**Contents**

- [1. Fixed input: operator rulings](#1-fixed-input-operator-rulings)
- [2. Problem](#2-problem)
- [3. What already exists](#3-what-already-exists)
- [4. The model](#4-the-model)
- [5. Claims, restore and the outcome ledger](#5-claims-restore-and-the-outcome-ledger)
- [6. Champion: promotion and merge](#6-champion-promotion-and-merge)
- [7. Telemetry and gates](#7-telemetry-and-gates)
- [8. Rollout and rollback](#8-rollout-and-rollback)
- [9. Open items](#9-open-items)
- [10. Children](#10-children)
- [11. Alternatives considered](#11-alternatives-considered)
- [12. Departures from owner docs](#12-departures-from-owner-docs)

## 1. Fixed input: operator rulings

These were ruled 2026-10-13/14 and are not reopened here.

1. **Item first, then role.** Draw a repo with the existing workspace draw (#11103), then an item. If the item is being processed or is parked, pass and draw again.
2. **The item draw** is seeded and weighted: `actionable × priority × stage-age`.
   - Items carrying `loom:operator`, and items being worked on, have zero weight.
   - There is no stage term and there are no per-stage quotas.
   - Age is measured from entry into the item's current stage. Until a stage-entry time is known, creation age is used.
3. **One item per step.** The daemon stops spawning `/loom:sweep`, which stays an attended-session command.
4. **Champion makes merge decisions**, one PR per targeted step (`/loom:champion --pr N`, #11871).
5. **Champion promotion verdicts are terminal** (#11885). Each ends in one of:
   - `loom:issue`;
   - revise: remove `loom:curated` and set `loom:triage`;
   - `loom:blocked`, with a reason;
   - `loom:operator`.
6. **`loom:operator` is a no-op for every role** (#11861). Champion's self-applied holds move to `loom:blocked` plus a park record and the hold marker.
7. **GitHub's merge queue** is the end state for merges on org-owned repos (#9978, #10257). The known revocation gap is accepted. User-owned repos stay on direct merges with `--expected-head`.

## 2. Problem

The finder lists only `loom:issue` (`work_finder/forge.rs:183`) and spawns `/loom:sweep` (`sweep_registry/spawn_process.rs:122-136`). Support roles run on fixed role-runner intervals, under their own ceiling.

Measured on a multi-host fleet on 2026-10-13/14:

| Measure | Value |
|---|---|
| `loom:issue` items seen per tick | about 297 |
| ... that already had an open PR | about 190 |
| ... that were parked | about 84 |
| ... dispatched per non-halted tick | about 0.28 |
| Sweep slots busy on worker hosts | 2–3 of 12 |
| Open issues in `loom:triage` | about 900 |
| Curated issues not yet promoted | about 690 |
| Open PRs in `changes-requested` | about 150 |
| Open PRs in `loom:pr` | about 100 |

**The Builder queue was drained. The work was waiting at every other stage, and the finder never looked there.**

Part 1 (#11852–#11864) fixed the worst refusal loops, the telemetry gaps and the targeted roles. This design is Part 2.

## 3. What already exists

The design builds on these and must not duplicate them.

| Concern | Exists in | Note |
|---|---|---|
| Workspace draw | `work_finder/workspace_draw.rs` (#11103) | v1 is seeded `(minute, host)` (`:268-275`) |
| Targeted roles and outcome classes | #11868, `defaults/docs/targeted-role-invocation.md:85-137` | `verdict \| noop \| refused \| failed` plus reason tokens; `LOOM_STEP_OUTCOME` is unread (`:169-171`) |
| Targeted Champion, non-item mode | #11871, `defaults/docs/champion-targeted.md`, `role_runner/champion_mode.rs` | `autonomous.roleRunner.championMode: "non-item"` |
| Queue-depth and slot gauges | #11874 | `loom.forge.stage_items{repo,state}` (`observability/ops/stage_queue.rs:26-55`), `loom.dispatch.busy_slots{role}` (`observability/ops/busy_slots.rs`) |
| Role telemetry | #11877 | `loom.repo` and trace on role rows, Curator listing journal, `role_tick.outcome` |
| Landed cooldown, fast-fail ladder | #11875, #11872 | `sweep_registry/dispatch/landed_cooldown.rs`; `sweep_registry/fast_fail_park.rs` |
| Open-PR release hold | #11876, `4809ef7ed` | **On by default.** On a verified open linked PR a release writes no label, so the issue keeps `loom:building` while its PR is open (`ClaimRelease::HeldByOpenPr`, `sweep_registry/restore_to_ready.rs`). `LOOM_RELEASE_OPEN_PR_VETO=0` switches it off. |
| Discovery mirror seam | #11845 | `forge_mirror/discovery.rs:36-42, :215-247`: single-label listings for `WorkFinder` and `StageDwell`, with REST fallback |
| Shared ETag all-open walk | `forge_listing::list_open_issues_cached_all_as` (`:248`) | Used by `observability/ops/stage_queue.rs:115` and the intake singleton |
| Merge-queue handoff | #9978 A/B, `forge_merge_queue/` | `QUEUE_EXECUTION_ENABLED = false` (`forge_merge_queue/mod.rs:69`). Champion calls `forge merge-queue step` before every merge (#10256 B3) |

## 4. The model

### 4.1 Items and steps

An **item** is an open issue joined to its linked open PR, using the link rules of `worktree_ops/linked_pr_listing.rs`. A PR with no issue is its own item. A **step** is one role session that advances one item by one stage, in one slot, under one claim.

| Item state | Step |
|---|---|
| unlabeled or `loom:triage` | Curator |
| landed: a *closing* PR merged, issue still open (not `Part of`) | Curator keep-or-close |
| `loom:curated` | Champion promote (terminal) |
| `loom:curated` + `loom:blocked`, **dependency park only**, blocker closed | Champion re-evaluate |
| `loom:issue`, no open PR | Builder |
| linked PR `loom:review-requested` | Judge |
| linked PR `loom:changes-requested` | Doctor |
| linked PR `loom:pr` | Champion merge |
| linked PR `loom:pr` + `loom:blocked` + Champion hold marker | Champion hold re-evaluation |
| PR in GitHub's merge queue (`queued`) | none |
| `loom:operator`, `loom:frozen`, claimed, cooling down | none |

Notes on the table:
- Fast-fail and quarantine parks, and hold records, are not dependency parks.
- Revise and verdict markers are read through `comment_trust`.
- Red-main fixes keep their lane (`work_finder/main_red_fix.rs`).
- A `loom:operator-mechanical` item has weight 0 on a host without the capability (`work_finder.rs:382-395`).

### 4.2 The tick

1. **List.** Use the shared all-open walk, or the mirror when `forge.mirror` is on. Join PRs to issues, classify each item, and compute zero weights from labels and local state.
2. **Draw a workspace** with the existing workspace draw. Workspace holds, including maintain-only, keep the root out.
3. **Draw an item** with weight `actionable × priority × age`:
   - **`actionable` ∈ {0, 1}.** Set to 0 for each "none" row in §4.1, and for a Builder step in a repo whose build backoff is engaged. Red-main fixes are exempt (`work_finder/build_backoff.rs:594`). Also 0 for a step whose role's token pool is exhausted (the `record_role_tick_pool_exhausted` state): the spawn would be refused, so drawing it would only burn the redraw budget (#11896).
   - **`priority`:** red-main ×50, `loom:very-important` ×8, `loom:important` ×3. The `vi_cap` demotion (`work_finder/vi_cap.rs:1-24`) runs over the whole item list first.
   - **`age`** = `1 + min(stage_age_days, 90) / 30`.
   - **Seed:** `loom-draw/v2` = `sha256("loom-draw/v2:"+minute+":"+host+":"+tick_counter+":"+workspace+":"+draw_index)`. Draws are without replacement and replayable from `pick.decision`.
   - The weights are numeric hyperparameters in the strict block.
4. **Gate and claim** for the step's role: saturation, ramp cap, per-repo RAM/disk cap, then the claim (§5). **On a pass, draw again**, up to 8 redraws per slot and 32 per tick. Count each pass with its reason, including `claim_lost`.
5. **Spawn** `/loom:<role> <N>` (§4.4).

### 4.3 Stage-entry time

The stage-entry provider is a three-source adapter, tried in order:
1. the mirror's per-item `labels_at`. The github-events mirror serves it, but `MirrorItem` (`forge_mirror/client.rs:127-152`) does not carry it yet; #11897 adds it;
2. REST `issues/{n}/events`, with its own budget and a persisted cache;
3. `created_at`.

`pick.decision` records which source answered, as `age_source`. The fleet captain's stage-dwell cache cannot serve dispatchers (`observability/ops/stage_dwell.rs:42-49, :86, :242-247`).

### 4.4 Slots and spawn

- **Slots.** Roots with `stepDispatch: on` draw steps from `workFinder.maxConcurrent`. Other roots keep the role-runner ceiling until the two budgets merge (§10, last phase).
- **Resolvers.** A step uses the role runner's resolvers for its role (`role_runner/model_resolution.rs:71, :160`) and admits against the pool of the resolved runtime. Only Builder steps keep the per-issue experiment arm (`sweep_registry/model_dispatch.rs:64-66`).
- **Capture.** A step keeps the role-tick capture from #11877.
- **Registry.** Steps run in the sweep registry as `SweepKind::Step { item, role }` (`types.rs:945-952`). Every consumer that matches on `SweepKind` becomes exhaustive. Today about twenty sites drop any non-`Issue` kind, including occupancy (`sweep_registry/watchdog.rs`), `busy_slots.rs:25-27`, roll pause and resume, the fleet-state feed and telemetry identity.

## 5. Claims, restore and the outcome ledger

**Claims.**
- Builder: the issue lease plus `loom:building`.
- Judge and Doctor: `lease pr` plus `loom:reviewing` / `loom:treating`.
- Curator and Champion promote: a lease with a `step` field plus `loom:curating` / `loom:evaluating`.
- Champion merge: `lease pr`, with `step=champion_merge`.
- Peer claims gain an additive `(item, step)` shape (`peer_claims.rs:239-244`).
- `labels.json` `stale_after_minutes` becomes live for all five claim labels. Builder liveness stays the renewed lease.

**Restore when a step dies.** Each claim is released back to its role's ready stage:

| Released claim | Restored to |
|---|---|
| `loom:curating` | `loom:triage` |
| `loom:evaluating` | `loom:curated` |
| `loom:building` | `loom:issue` |
| `loom:reviewing` | `loom:review-requested` |
| `loom:treating` | `loom:changes-requested` |

A Builder restore with a verified open linked PR writes no label: the issue keeps `loom:building` while its PR is open, through the existing open-PR release hold (`ClaimRelease::HeldByOpenPr`). The item join then classifies the item by its PR.

**Outcome ledger.** The ledger consumes #11868's classes.

| Outcome | Effect |
|---|---|
| landed (`verdict:closed`) | Reclassify the item. No more Builder steps. |
| `noop` | Cooldown per `(item, step)` |
| fast `failed` | The #11872 ladder |
| Champion re-evaluation left the item held or blocked | Ladder from 6 h to 7 d, **stored on the forge** in the park record or hold marker |

Short cooldowns are local and broadcast, bounded at 24 h (`sweep_registry/dispatch/landed_cooldown.rs:32`).

## 6. Champion: promotion and merge

- **Promotion** is one terminal verdict per step (#11885). Promotion volume is whatever the draw selects. `tier2Cap` and the promotion slices do not apply in step mode. The capacity deferral is already gone in every mode (#11860).
- **Merge** is one PR per step. Champion runs `champion-pr-merge.md`'s criteria and then tries the merge-queue handoff. Its Step 3 calls `forge merge-queue step` before every Champion merge, batch or targeted, as of #10256 Phase B3 (`defaults/.claude/commands/loom/champion-pr-merge.md:1879`). When the handoff answers `LOOM-MERGE-QUEUE-DIRECT`, Champion uses `merge-pr.sh --expected-head`. The daemon never merges by itself.
- **Merge serialisation**:
  - Until queue mode is on, serialisation within one host comes from the PR lease and a repo-keyed mesh advertisement. Across hosts it is best-effort.
  - The correctness guards are `--expected-head` and `_head_moved_or_resync`.
  - Once queue mode is on, GitHub serialises merges per repo, and a `queued` PR has weight 0.
- **Non-item passes** (proposals, epics, Doctor-cap PRs, follow-ons, rollout checks) stay on the interval run under `championMode: "non-item"`.

## 7. Telemetry and gates

**Signals.**
- `loom.dispatch.step` span: repo, item, role, admission result, reason, draw attempt, `age_source`, resumed.
- `pick.decision`: the item-draw weight terms, `tick_counter`, and zero-weight counts by reason.
- `loom.draw.expected_wait_s`, by repo × stage.
- The baseline: `loom.forge.stage_items{repo,state}` and `loom.dispatch.busy_slots{role}`.

**Shadow comparison.** For each shadow `(item, step)`, did the live system act on it within N ticks? The comparison is bucketed by `loom.hyperparams.digest` and windowed in ticks, because mirror wakes make ticks early.

**Gate: shadow to live** (24 h):
- no unexplained live-only Builder, Judge or merge action;
- zero stale picks;
- expected wait under 2 h for review and merge items in every repo;
- ≥ 95% stage-age coverage, or the creation-age ruling recorded.

**Gate: pilot to fleet** (48 h):
- slot use of at least 70% while eligible items exist;
- fewer than 1 open-PR or landed refusal per 10 dispatches;
- no `(item, step)` run more than 3 times in 24 h without a state change;
- Judge runs per verdict ≤ 2.

## 8. Rollout and rollback

- **Flag.** A per-root `autonomous.workFinder.stepDispatch: off | shadow | on`, capped per host by `LOOM_WORK_FINDER_STEP_DISPATCH`: `off` beats everything, and `shadow` caps `on`.
- **Order:**
  1. shadow on one host;
  2. one pilot root `on` on one host, with its item roles off on every other host and Champion `non-item` there;
  3. all roots on that host;
  4. all hosts.
- **Rollback** is host env `off`, plus re-enabling the pilot root's item roles. Forge-side parks stay where they are; `release-stale-blocked` owns them.
- **Prerequisite (met).** #11861 made `loom:operator` an all-role no-op and removed the work finder's PR-lane carve-out, so no legacy path acts on held PRs that the engine passes.

## 9. Open items

- **Stage term.** If shadow shows review and merge items starving even with stage-age, should a stage term be added? Decided on shadow data, by the operator.
- **Sessions.** Should Doctor resume the author's session or its own? What is the resume TTL? Both wait on the resume-vs-fresh experiment.
- **Merge queue.** Phase C's live verification (#10257) runs after the pilot.

## 10. Children

| Issue | Scope |
|---|---|
| #11894 | This doc and the owner-doc departures (§12) |
| #11895 | Item model and state → step classifier (§4.1) |
| #11896 | Seeded weighted item draw (§4.2) |
| #11897 | All-open listing, mirror caller, stage-entry provider (§4.3) |
| #11898 | `SweepKind::Step`, step claims, per-role restore (§4.4, §5) |
| #11900 | Step-outcome ledger (§5) |
| #11906 | Step spawn (§4.4) |
| #11908 | Shadow mode and comparison (§7) |
| #11909 | Live dispatch per root (§8) |
| #11910 | Dispatch-plan `step` field |
| #11911 | Finish Sweep retirement and merge the budgets |
| #11913 | Session ledger and soft host affinity |

The fleet rollout, the dashboard consumers and the resume experiment are tracked outside this repo.

## 11. Alternatives considered

| Alternative | Verdict |
|---|---|
| **Role-first queues**: extend `demandWidth` and queue gates to every role | Keeps two budgets and per-role scans of every repo, and cannot apply one priority across stages. Its useful parts landed in Part 1. |
| **Backlog-proportional intervals** | Scales cadence, but still launches into empty repos and duplicates work across hosts |
| **Keep Sweep and admit at any stage** | Holds one slot across CI and review waits, and re-applies priority only once per lifecycle |
| **Central scheduler** assigning steps to hosts | A single owner strands work when it breaks; the lesson of `dispatcher-repo-sharding.md`. The seeded draw plus leases spreads work without an owner. |
| **Pre-filtered, stage-restricted draws** | Rejected by ruling: no quotas, no stage term |

## 12. Departures from owner docs

Each owner doc carries a short pointer to this section. The behaviour changes only when the matching child ships behind `stepDispatch`.

| Owner doc | Departure under `stepDispatch: on` |
|---|---|
| `defaults/docs/priority-model.md` | The within-workspace pick (`main_red_fix → level → created_at → number`) becomes the seeded weighted item draw (§4.2) |
| `defaults/docs/promotion-throughput.md` | Promotion is per-item and terminal. `prSlice`, `promotionSlice` and `tier2Cap` don't apply. |
| `defaults/docs/dispatch-plan.md` | Plan rows gain a `step` field, with the shared fixture changed first |
| `defaults/docs/telemetry-kind-pick-decision.md` | Work-finder rows gain step, weight terms and `age_source`. The per-role cadence volume table no longer holds for item roles. |
| `defaults/docs/daemon-reference.md` (role runner) | The item-role intervals and `demandWidth` don't apply to step roots. The two concurrency budgets merge in the last phase. |
| `defaults/docs/label-state-machine.md` | The `queued` item state. `stale_after_minutes` becomes live. Per-role restore. |
| `defaults/labels.json` (header) | `stale_after_minutes` is no longer INERT. #11898 changes the header together with the values it makes live; until then the header stays accurate. |
| `mcp-loom/src/tools/sweeps.ts` and `mcp-loom/README.md` | The sweep tools learn `SweepKind::Step` (its role and item). #11898 changes them together with the Rust type. |
| `loom-daemon/src/forge_merge_queue/mod.rs:30-32` | The comment saying no prompt invokes the handoff is stale. Champion's merge step has called it since #10256 Phase B3. Corrected in this change. |
