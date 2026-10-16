# Work-Finder Step Dispatch: Item First, Then Role (Issue #11894)

This is the design doc for the work-finder overhaul. It replaces "list `loom:issue` and spawn a whole-lifecycle `/loom:sweep`" with **item-first step dispatch**:
1. draw a workspace;
2. draw an item from it by a seeded weighted draw;
3. advance that item **one step**, with the role its state calls for.

Support roles stop running on fixed intervals for item work, and the daemon stops spawning `/loom:sweep`.

**Status: Live (2026-10-14).** The operator accepted the design, then ruled it ships with no flag, no shadow mode and no pilot: the step tick is the work finder's only item dispatch path since #11909, rolled out fleet-wide and fixed forward. Section 1 records the rulings, and section 9 the items that stay open. The children listed in section 10 changed the code.

This is a repo-local design doc under `docs/design/`. It is never installed into consumer repos. The format follows [`persistence-inventory.md`](persistence-inventory.md).

**Source SHA.** Sections 2 and 3 describe the code before the cutover and cite `7e6e346e1` (`main`, 2026-10-14). Every other citation is against `24815f814` (`main`, 2026-10-14), or names a module and symbol, which do not drift. Paths without a prefix are under `loom-daemon/src/`. Paths under `defaults/`, `docs/` and `mcp-loom/` are written in full.

**Contents**

- [1. Fixed input: operator rulings](#1-fixed-input-operator-rulings)
- [2. Problem](#2-problem)
- [3. What already exists](#3-what-already-exists)
- [4. The model](#4-the-model)
- [5. Claims, restore and the outcome ledger](#5-claims-restore-and-the-outcome-ledger)
- [6. Champion: promotion and merge](#6-champion-promotion-and-merge)
- [7. Telemetry and alerts](#7-telemetry-and-alerts)
- [8. Rollout and rollback](#8-rollout-and-rollback)
- [9. Open items](#9-open-items)
- [10. Children](#10-children)
- [11. Alternatives considered](#11-alternatives-considered)
- [12. Departures from owner docs](#12-departures-from-owner-docs)

## 1. Fixed input: operator rulings

These were ruled 2026-10-13/14 and are not reopened here.

1. **Item first, then role.** Draw one item from a single pool of every actionable item across the host's repos (#12185; this was a repo draw with the workspace draw (#11103), then an item in it, until the operator changed it to cut claim collisions on small repos: 27 repos with at most 10 actionable items got 31% of step draws and 185 of 264 `claim_lost` refusals in a 3-hour window). If the item is being processed or is parked, pass and draw again.
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
8. **No flag, no old code, no shadow mode, no smoke check** (2026-10-14). The cutover replaces the tick in one revertable merge and ships fleet-wide; problems are fixed forward. Rollback is reverting that merge.
9. **The role runner runs non-item roles only.** Curator, Judge and Doctor no longer run on intervals; Champion runs `--non-item` only.

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

The classifier (`work_finder/step_item.rs`, `classify`) checks these rows in order; the first that matches decides.

| Item state | Step |
|---|---|
| `loom:frozen`, `loom:operator`, `external`, or a claim label (on the PR or any open issue it links) | none |
| PR in GitHub's merge queue (`queued`) | none |
| the issue's open PR is another item's (`shared_pr`) | none |
| PR `loom:sequenced` | none |
| linked PR `loom:pr` + `loom:blocked` + Champion hold marker | Champion hold re-evaluation |
| linked PR `loom:changes-requested` + `loom:blocked` + hold marker, Doctor cycle not capped | Doctor |
| linked PR `loom:review-requested` + `loom:blocked` + hold marker | Judge |
| any other blocked PR | none |
| linked PR `loom:changes-requested` | Doctor |
| linked PR `loom:review-requested` | Judge |
| linked PR `loom:pr` | Champion merge |
| PR with no workflow label | none |
| issue `loom:epic` | none (Champion's non-item run) |
| `loom:architect` / `loom:hermit` / `loom:auditor` proposal, not curated or promoted | none (Champion's non-item run) |
| `loom:issue` + `loom:blocked`, capability-only park, every capability held | Builder (capability lane) |
| the same, a capability not held | none |
| `loom:curated` (not `loom:issue`) + `loom:blocked`, **dependency park only** | Champion re-evaluate (it re-checks the blockers on every draw, #12078) |
| any other blocked issue | none |
| landed: a *closing* PR merged, issue still open (not `Part of`) | Curator keep-or-close |
| `loom:decision-malformed` | Curator |
| `loom:issue`, no open PR | Builder |
| `loom:curated` | Champion promote (terminal) |
| unlabeled or `loom:triage` | Curator |

Cooling down is not a row: the outcome ledger's cooldowns are draw state, applied as `actionable` = 0 (§4.2).

Notes on the table:
- Fast-fail and quarantine parks, and hold records, are not dependency parks.
- Revise and verdict markers are read through `comment_trust`.
- Red-main fixes keep their lane (`work_finder/main_red_fix.rs`).
- A capability-parked item has weight 0 on a host without the capability (`capability_missing`).

### 4.2 The tick

1. **List.** Use the shared all-open walk, or the mirror when `forge.mirror` is on. Join PRs to issues, classify each item, and compute zero weights from labels and local state.
2. **Pool the workspaces.** There is no workspace draw (#12185). Workspace holds, including maintain-only, keep the root out of the pool; a repo with `w_repo` 0 has weight 0.
3. **Draw an item** from the one pool (#12185), with weight `w_repo × iw × S_r^(α−1) × (1 + in_flight_repo)^(−β)`, where `iw = actionable × priority × age` is the item weight and:
   - `w_repo` is the repo's roster weight (`workspaces.json`, from the fleet store); `S_r` is the sum of `iw` over the repo's items still in the pool; `in_flight_repo` is the repo's items this host runs plus the live claims peers advertise, counted at the start of the tick.
   - **α** (`dispatch.backlogExponent`, 0–1, default 0) and **β** (`dispatch.busyExponent`, ≥ 0, default 0) are float hyperparameters in the strict block, hot-applied. α = 0 gives `w_repo × iw / S_r`, the earlier two-stage draw's distribution (a repo's share is its `w_repo`); α = 1 gives `w_repo × iw`, a flat pool where a repo's share grows with its backlog; β > 0 draws a repo with work in flight less. (The first draft of #12185 wrote `n_items^(α−1)`; that only equals the two-stage draw when every item of a repo weighs the same, so the term is `S_r`. `n_items` is still recorded.) The repo-level `dispatch.weightBonus*` points are retired: with one pool the item priority factors do their job.
   - The terms are real numbers: the weights are computed in floating point, scaled so the heaviest is `2^40`, and rounded to integers (a positive weight stays at least 1), so the roll and the pick are integer and the probabilities are `w / Σw` of the scaled weights. Draws are without replacement and `S_r` is recomputed over the items still in the pool at each draw.
   - Knobs, formula and effects: [`defaults/docs/hyperparameters.md`](../../defaults/docs/hyperparameters.md#the-item-draw-12185).
   - **`actionable` ∈ {0, 1}.** Set to 0 for each "none" row in §4.1, and for a Builder step in a repo whose build backoff is engaged. Red-main fixes are exempt (`work_finder/build_backoff.rs:594`). Also 0 for a step whose role's token pool is exhausted (the `record_role_tick_pool_exhausted` state): the spawn would be refused, so drawing it would only burn the redraw budget (#11896).
   - **`priority`:** red-main ×50, `loom:very-important` ×8, `loom:important` ×3. The `vi_cap` demotion (`work_finder/vi_cap.rs:1-24`) runs over the whole item list first.
   - **`age`** = `1 + min(stage_age_days, 90) / 30`.
   - **Seed:** `loom-draw/v3` = `sha256("loom-draw/v3:"+minute+":"+host+":"+tick_counter+":"+draw_index)` (`v2` also keyed the workspace). Draws are without replacement and replayable from `pick.decision`, which logs the exponents, each repo's `w_repo`, `in_flight` and `n_items`, the starting pool (in pool order, with item weights), the repos closed mid-tick (an owner whose claim cannot be verified, #12260), and every draw's roll and picked index.
   - The weights are numeric hyperparameters in the strict block (`w_repo` excepted: it stays in the roster).
4. **Gate and claim** for the step's role: the pool gate, then the claim (§5). There is no capacity gate beyond free slots: #12406 removed the saturation brake, the ramp cap and the per-repo disk cap (the RAM term went in #12330). **On a pass, draw again**: at most 8 draws fill one slot and at most 32 passes are taken in one tick (`step_dispatch/tick.rs`, `MAX_DRAWS_PER_SLOT`, `MAX_PASSES_PER_TICK`). Each pass is counted by its reason, including `claim_lost`, on `pick.decision` (`refused_<reason>` beside the weight-0 counts) and, for the pool gate and the claim, in the outcome ledger.
   - Before the draw, an item this host runs (a live sweep or step) or a peer advertises a live claim on is a `claimed` pass, and while a peer's Champion merge is in flight in the repo every other merge step there is too (`SweepRegistry::draw_claims`, `PeerClaimView::merge_in_flight_at`). The classifier's `claimed` row reads labels only, and a non-Builder step labels its item late.
5. **Spawn** `/loom:<role> <N>` (§4.4).

### 4.3 Stage-entry time

The stage-entry provider is a three-source adapter, tried in order:
1. the mirror's per-item `labels_at`. The github-events mirror serves it, but `MirrorItem` (`forge_mirror/client.rs:127-152`) does not carry it yet; #11897 adds it;
2. REST `issues/{n}/events`, with its own budget (each page one read) and a persisted cache. GitHub lists events oldest first, so the read takes the label's latest `labeled` event from the end: page 1 for the `Link` header, then back from the last page (#11924). A cached time is re-read when the item's `updated_at` has moved since, so a removal and re-application between two listings is not served the earlier time;
3. `created_at`.

`pick.decision` records which source answered, as `age_source`. The fleet captain's stage-dwell cache cannot serve dispatchers (`observability/ops/stage_dwell.rs:42-49, :86, :242-247`).

### 4.4 Slots and spawn

- **Slots.** Steps draw from `workFinder.maxConcurrent` on every root: the tick fills every one of its `maxConcurrent - occupancy` free slots (#12406). The role runner keeps its own ceiling for the non-item roles until the two budgets merge (#11911).
- **The tick** is `step_dispatch::driver::StepDriver::tick`, called from the work-finder loop in place of the `loom:issue` finder. Per root it lists and classifies (`step_dispatch::gather`), weighs with the registry's live state (`step_cooldowns`, the Champion ladder's due times, `pool_exhausted_roles`), and runs `step_dispatch::tick::run`. The seed's `workspace` input is the forge slug `pick.decision` names as `repo`.
- **Resolvers.** A step uses the role runner's resolvers for its role (`role_runner/model_resolution.rs`, `resolve_role_runner_model` and `resolve_role_runner_effort`) and admits against the pool of the resolved runtime. Only Builder steps keep the per-issue experiment arm (`sweep_registry/model_dispatch.rs`).
- **The lock.** The spawn (`SweepRegistry::spawn_step_unlocked`) releases the registry lock around the runtime admission's private-clone containment (#8787), a non-Builder step's pre-claim and the account-selection poll, as `private_dispatch::prepare` does for sweeps.
- **Capture.** A step keeps the role-tick capture from #11877.
- **Registry.** Steps run in the sweep registry as `SweepKind::Step { item, role }` (`types.rs`). Every consumer that matches on `SweepKind` is exhaustive (#11898).
- **The end.** A step's end is queued (`sweep_registry/step_spawn/ended.rs`); the next tick takes the post-state its claim release read before writing (or reads the item once when nothing was released), classifies it with the session's outcome line and records it in the ledger (`step_dispatch::step_end`). A failed read is `failed` / `probe_failed`, never a default. A refusal's cooldown (the pool gate, a claim race, a failed launch) stays local to the host; every other window is broadcast.
- **Listing failures.** A failed item listing falls back to the repo's last good one for up to 10 minutes; a failed open-PR listing draws no Builder step in the repo that tick, since a PR linked only by its branch would be invisible.
- **The role runner** runs Guide, Auditor, Hermit, Architect and Champion `--non-item`. Curator, Judge and Doctor named in `roles` or `onIdle` are ignored with a warning; `championMode` is retired (`role_runner/champion_mode.rs`). Epic `BuildChildren` dispatches nothing: the children are Builder steps.

## 5. Claims, restore and the outcome ledger

**Claims.**
- Builder: the daemon takes the issue lease and flips `loom:issue` to `loom:building` before the spawn.
- Every other step: the daemon takes **only** the role-keyed lease (`lease pr claim --role <role> --step <step>`); the prompt applies its claim label itself (`loom:reviewing`, `loom:treating`, `loom:curating`, `loom:evaluating`; a Champion merge has none).
- Champion merge: `lease pr`, with `step=champion_merge`.
- Peer claims gain an additive `(item, step)` shape (`peer_claims.rs:239-244`).
- `labels.json` `stale_after_minutes` is live for all five claim labels. Builder liveness stays the renewed lease. Curating, evaluating and reviewing are 45 minutes, longer than the 30-minute step timeout, so a running step's claim never reads as stale; treating is 60.
- **Timeouts.** Every non-Builder step has the role runner's 30-minute timeout and a Builder step the sweep's; the reaper kills a step past it and releases its claim (`sweep_registry/step_spawn/timeout.rs`).

**Restore when a step dies.** Each claim is released back to its role's ready stage:

| Released claim | Restored to |
|---|---|
| `loom:curating` | `loom:triage` |
| `loom:evaluating` | `loom:curated` |
| `loom:building` | `loom:issue` |
| `loom:reviewing` | `loom:review-requested` |
| `loom:treating` | `loom:changes-requested` |

A Builder restore with a verified open linked PR writes no label: the issue keeps `loom:building` while its PR is open, through the existing open-PR release hold (`ClaimRelease::HeldByOpenPr`). A step's restore keeps that hold even when `LOOM_RELEASE_OPEN_PR_VETO=0` switches the hold off for sweeps. The item join then classifies the item by its PR.

**Post-state.** The ledger's post-state comes from the one read the claim release takes, before its restore write; a read after the write would always see the claim gone. A step whose claim was not released (another claimant holds the item, a merge step) is read once at the next tick (`step_dispatch::step_end`).

No restore runs when the item is held elsewhere. For a Builder step that means `loom:building` re-applied more than 60 s after the step started. For every other step it means either of two things. The first is that the role's earliest live lease belongs to another sweep. The second is that the claim label was applied more than 60 s after the step started and outside the step's own lease (#11969): after that lease was released, yielded or aged out, or when the step has no lease. That second case is an attended session that claims by label alone, such as Doctor or the Curator path. A label applied while the step's own lease was live belongs to the step, because the role prompt applies it after the daemon's lease.

A step that wrote its verdict and died still holding its claim only loses the claim. Its ready label is not restored. Champion promote's verdict labels are `loom:issue` (`promoted`) and `loom:triage` (`revise`, #11969).

**Outcome ledger.** The ledger consumes #11868's classes.

| Outcome | Effect |
|---|---|
| landed (`verdict:closed`) | Reclassify the item. No more Builder steps. |
| `noop` | Cooldown per `(item, step)` |
| fast `failed` | Builder: the #11872 ladder. Every other step (Curator, Champion promote, Judge, Doctor, Champion merge): a ledger-local doubling window capped at 24 h, never the issue quarantine, whose release restores `loom:issue` (#11969) |
| Champion re-evaluation left the item held or blocked | Ladder from 6 h to 7 d, **stored on the forge**: a `<!-- loom:reeval step=… rung=… next=… -->` line in the item's body, beside its park record or under its hold marker |
| `refused`, `pool_gate` or `claimed` | A short cooldown per `(item, step)`, in force before the next tick |
| `refused`, `claim_lost` | Redraw; counted. A second in a row on the same `(item, step)` arms the refusal cooldown, doubling to 24 h (#11969) |
| any other `refused` | Reclassify the item |

**Bounds on repeats.** The ledger's repeat cap allows at most 3 runs of one `(item, step)` in 24 h without a state change, counting only steps that worked the item: a verdict, a `noop` other than `ci_pending`, and a `failed` (#11900). The outcomes it excludes have their own brakes. `ci_pending` gets the 1 h noop window. `pool_gate` and `claimed` get the 10 min refusal cooldown. A state refusal costs nothing the first time, but a second identical one in a row arms the refusal cooldown, doubling to 24 h. A `claim_lost` redraw never ran, but a second one in a row on the pair arms the same doubling refusal cooldown. The cap's run history is per host and in memory, so across N hosts the fleet-wide bound is about 2N + 1 runs. Short cooldowns are local and broadcast (`step_cooldown_armed`), bounded at 24 h on both ends (`step_dispatch/outcome_ledger.rs` `MAX_SHORT_COOLDOWN`, which the landed cooldown shares). The draw reads them, and the forge ladder's due times, as part of `actionable` (#11900).

## 6. Champion: promotion and merge

- **Promotion** is one terminal verdict per step (#11885). Promotion volume is whatever the draw selects. `tier2Cap` and the promotion slices do not apply in step mode. The capacity deferral is already gone in every mode (#11860).
- **Merge** is one PR per step. Champion runs `champion-pr-merge.md`'s criteria and then tries the merge-queue handoff. Its Step 3 calls `forge merge-queue step` before every Champion merge, batch or targeted, as of #10256 Phase B3 (`defaults/.claude/commands/loom/champion-pr-merge.md:1879`). When the handoff answers `LOOM-MERGE-QUEUE-DIRECT`, Champion uses `merge-pr.sh --expected-head`. The daemon never merges by itself.
- **Merge serialisation**:
  - Until queue mode is on, serialisation within one host comes from the PR lease and a repo-keyed mesh advertisement. Across hosts it is best-effort.
  - The correctness guards are `--expected-head` and `_head_moved_or_resync`.
  - Once queue mode is on, GitHub serialises merges per repo, and a `queued` PR has weight 0.
- **Non-item passes** (proposals, epics, Doctor-cap PRs, follow-ons, rollout checks) stay on the interval run, which is always `/loom:champion --non-item` (#11909).

## 7. Telemetry and alerts

**Signals.**
- `loom.dispatch.step` span: repo, item, role, admission result, reason, draw attempt, `age_source`, resumed, and (#12185) the drawn item's draw weight, the pool's total weight at that draw, and its repo's `n_items` and `in_flight_repo` (`loom.step.weight`, `loom.step.total_weight`, `loom.step.repo_items`, `loom.step.in_flight_repo`; `observability/ops/dispatch_step.rs`). One per spawn attempt, spawned or refused.
- `pick.decision`'s `item_draw`: the `loom-draw/v3` seed inputs, the two exponents, each repo's `w_repo`, `in_flight` and `n_items`, the weight-0 counts by reason, the starting pool and every draw, enough to replay the tick (`item_draw::replay_record`), and the repos in the pool when a `WeightOverflow` stopped the draws. Work-finder rows carry their step as `stage` and in the dispatch plan's `step` field.
- `loom.step.actionable{repo}`: the drawable depth per repo, each tick.
- The baseline: `loom.forge.stage_items{repo,state}`, sampled per repo by the captain's stage-dwell sampler, and `loom.dispatch.busy_slots{role}`. Steps count under `role="sweep"` (one slot each) and are broken down by step role as `busy_slots{role=<role>, pool="step"}`, never added to the total.

**Alerts** (`defaults/observability/signoz/alerts/`, shipped with the cutover so a fix-forward rollout is watched from its first minute):

| Rule | Fires when |
|---|---|
| `step-dispatch-stalled.json` | a repo has drawable items (`loom.step.actionable` > 0) for 15 min, a host dispatching for it has a free slot, and no `loom.dispatch.step` was spawned for the repo |
| `step-draw-pass-rate.json` | pool-gate plus claim refusals are above 80% of a host's `loom.dispatch.step` attempts for 30 min |
| `step-slots-idle.json` | a repo had work waiting for 30 min while the busiest host dispatching for it stayed below 50% of `maxConcurrent` |
| `role-runner-item-tick.json` | a Curator, Judge or Doctor `role_tick.outcome` that is not a step (`loom.role_tick.source` != `step`) appears: a pre-cutover daemon or a surviving interval loop |

The stalled and idle rules key on `loom.step.actionable{repo}`, which every dispatching host emits per listed repo each tick: the items with weight > 0 after the live claims. `loom.forge.stage_items` would count parked, held, operator, frozen and epic work the tick never draws, and it is the captain's gauge, not the dispatcher's.

**Holds.** A held workspace is not listed, so it has no queue rows on the tick (only a red-main repo that admits its fixes is listed). A listed red repo keeps every step that adds no code to `main` (Curator, Champion promote, re-evaluate and hold re-evaluate, Judge, Doctor) for any item, so a fix PR can be reviewed and repaired; Builder and `ChampionMerge` run only for a red-main fix, so Champion merges a fix PR only when its issue carries the `<!-- loom:main-red-fix -->` marker.

## 8. Rollout and rollback

- **No flag, no shadow, no pilot** (ruling 8). The cutover merge replaces the tick; the release that carries it goes to every host.
- **Fix forward.** The alerts in §7 are the watch.
- **Rollback** is reverting the cutover merge, which restores the `loom:issue` finder and the item-role intervals. Forge-side parks stay where they are until a human, Guide or Champion lifts them (the release pass was removed, #11301).
- **Prerequisite (met).** #11861 made `loom:operator` an all-role no-op and removed the work finder's PR-lane carve-out, so no other path acts on held PRs that the engine passes.

## 9. Open items

- **Stage term.** None today (ruling 2): the weight is `actionable × priority × stage-age`. Whether one is ever added is decided on production data, by the operator; the `step-slots-idle` alert and `pick.decision` show whether a stage starves.
- **Sessions.** Implemented, **off by default** (#11913): `step_dispatch/session_ledger.rs` is a host-local ledger (`.loom/step-sessions.json`) of `(item, role) → (host, session_id, ended_at, context_size)`, written when a step ends (`step_end::settle`) and read at dispatch (`step_spawn::step_session_resume`). A Judge resumes only a Judge, never an author's session. A Builder retry resumes its own, and a Doctor its own last session, else the Builder's. A resume needs the same host, a record younger than the TTL (default 1 h), a transcript under the context cap (default 2 MiB) and a reachable session store; otherwise the step runs fresh and the ledger logs a written handoff. Affinity is a preference: `affinity_wait` is nonzero only inside the grace window (default 10 min) for a record another host holds, and never gates eligibility. Enable with `LOOM_STEP_SESSION_RESUME=1` or `autonomous.stepSessionResume.enabled` (`graceSecs`, `ttlSecs`, `contextCapBytes` tune it). Resumed steps set `loom.step.resumed=true`. Still open, and waiting on the resume-vs-fresh experiment (tracked outside this repo): whether to flip the default, and the final TTL and cap values. Only Claude sessions are ledgered; the Doctor→Builder fallback matches records by item number, so it applies once a PR step and its issue share one.
- **Merge queue.** Phase C's live verification (#10257) runs on the live fleet.

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
| #11908 | Shadow mode and comparison: closed, superseded by #11909, which carries its telemetry |
| #11909 | The cutover: the step tick as the only item dispatch path, the alerts (§4.4, §7, §8) |
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

Each owner doc carries a short pointer to this section. Since #11909 every departure is the rule in force.

| Owner doc | Departure |
|---|---|
| `defaults/docs/priority-model.md` | The within-workspace pick (`main_red_fix → level → created_at → number`) is the seeded weighted item draw (§4.2) |
| `defaults/docs/promotion-throughput.md` | Promotion is a per-item, terminal Champion step. `prSlice`, `promotionSlice` and `tier2Cap` don't drive it. |
| `defaults/docs/dispatch-plan.md` | Every plan row carries its `step`; the fleet merge folds by `repo#issue` + `step`, pinned by the shared fixture |
| `defaults/docs/telemetry-kind-pick-decision.md` | Work-finder records carry `item_draw` (step, weight terms, `age_source`). The per-role cadence volume table does not hold for item roles. |
| `defaults/docs/daemon-reference.md` (role runner) | The item roles do not run on intervals and `demandWidth` doesn't apply to them; Champion runs `--non-item`. The two concurrency budgets merge in #11911. |
| `defaults/docs/label-state-machine.md` | The `queued` item state. `stale_after_minutes` is live (curating, evaluating and reviewing 45 min). Per-role restore. |
| `defaults/labels.json` (header) | `stale_after_minutes` is live for the five claim labels (#11898). |
| `mcp-loom/src/tools/sweeps.ts` and `mcp-loom/README.md` | The sweep tools know `SweepKind::Step` (its role and item), since #11898. |
| `loom-daemon/src/forge_merge_queue/mod.rs` | Champion's merge step calls the merge-queue handoff (since #10256 Phase B3); the module comment says so. |
