# Fleet Captain Dispatch: Workers Ask, the Captain Assigns (Issue #12447)

This is the Phase 0 spec for **captain-assigned dispatch**. Today every fleet host gathers every repo and draws its own items, and the forge lease decides who wins. In fleet mode a worker asks the fleet captain before it claims (Phase 1), and then for its next assignments from **one fleet queue** (Phase 2). The forge lease comment stays the **only** record of who works an item. The captain keeps a soft table that stops hosts from racing for the same items, and becomes the home for the fleet-wide signals the peer-claim channel used to carry.

**Status: Draft for operator review (2026-10-16).** Nothing here is built. The Phase 1 contract (section 5) is fixed and its issues are filed (section 13.5). Phases 2 and 3 are proposals: merging this doc is not approval to build them, and their issues are filed only after the operator reviews it. A design review (GO-WITH-CHANGES) is folded in; see the [review log](#18-review-log).

This is a repo-local design doc under `docs/design/`. It is never installed into consumer repos. The format follows [`work-finder-step-dispatch.md`](work-finder-step-dispatch.md), whose step tick this design keeps.

**Source SHA.** Citations are against `b8e70ef28` (`main`, 2026-10-16), or name a module and symbol, which do not drift. Paths without a prefix are under `loom-daemon/src/`. Paths under `defaults/` and `docs/` are written in full.

**Contents**

- [1. Fixed input: operator direction](#1-fixed-input-operator-direction)
- [2. Problem](#2-problem)
- [3. What already exists](#3-what-already-exists)
- [4. The model](#4-the-model)
- [5. Protocol](#5-protocol)
- [6. Leases, TTLs and the forge record](#6-leases-ttls-and-the-forge-record)
- [7. Scheduling policy](#7-scheduling-policy)
- [8. What the peer channel carried, and where it goes](#8-what-the-peer-channel-carried-and-where-it-goes)
- [9. Fail-open](#9-fail-open)
- [10. The fleet queue: change stream, refresh, staleness](#10-the-fleet-queue-change-stream-refresh-staleness)
- [11. Security](#11-security)
- [12. Telemetry](#12-telemetry)
- [13. Rollout, phases and follow-up issues](#13-rollout-phases-and-follow-up-issues)
- [14. Relation to #12304](#14-relation-to-12304)
- [15. Failure matrix and edge cases](#15-failure-matrix-and-edge-cases)
- [16. Scale](#16-scale)
- [17. Alternatives considered and open questions](#17-alternatives-considered-and-open-questions)
- [18. Review log](#18-review-log)

## 1. Fixed input: operator direction

Given 2026-10-16 and not reopened here.

1. **Fleet mode.** A worker's tick calls `next(host, free_capacity, constraints)`. The captain returns assignments, each with a repo, item, step, assignment id and TTL. Eligibility comes from the fleet config; the worker reports its live capacity.
2. **One fleet queue** in the captain, with a full refresh about every 10 minutes (configurable in the fleet config). Its source is #12304's mirror, not another gather. Mirror events update and invalidate entries between refreshes.
3. **Staleness is the worker's to catch.** The worker's existing per-item pre-check stays. A stale entry is refused, reported back and dropped.
4. **Forge lease comments stay authoritative.** A captain assignment is advisory, with a TTL, and is re-assigned on expiry.
5. **Fail-open.** If the captain is unreachable or slow, the worker dispatches on its own, with forge leases still authoritative. A captain outage never stops dispatch.
6. **Auth is the tailnet**, with `whois` caller identity and no per-host credentials.
7. **Packaging.** A `loom-daemon` subcommand on the captain host, rolled by the version floor, configured from the fleet config.
8. **Telemetry** over OTLP: assignments, refusals, fallbacks, queue age and refresh cost.
9. **Keep it simple.** The only capacity cap is `maxConcurrent`. No knob the design does not need.

## 2. Problem

Measured on the operator's fleet, 2026-10-16 (six dispatching hosts at the time of the claim measurement; the fleet has **eight** dispatchers counting two operator workstations, and the design is sized for that):

| Measure | Value |
|---|---|
| Step draws that ended `claim_lost` | 30–43% |
| Ticks on 0.19.1172 that stopped on `pass_limit` | most, with slots still empty |
| CPU per agent session | about 1% (sessions are I/O-bound) |
| A 16-vCPU host at 20–30 sessions | 15–25% CPU, plenty of RAM free |
| Per-host gather, concurrency 8 | about 20 s of a tick |
| A tick with many spawns | 100–140 s |

**Every host draws from the same pool, so they draw the same items.** The step draw (`step_dispatch/tick.rs`, `loom-draw/v3`) seeds per host, but the weights are the same everywhere, so the heavy items come up on every host at once.

**This baseline is a degraded state, not the steady one.** Before the draw, `sweep_registry/step_spawn/draw_claims.rs` removes every item a peer advertises on the peer-claim channel (`peer_claims.rs`). That channel has carried nothing since early October, so the 30–43% is advisory dedup switched off. Reviving it would only narrow the race again: it is eventually consistent by its own module doc. And it carried more than claims (section 8); those signals are also dark today.

**The forge budget drain is the lost claim, not discovery.** With #12304 each host reads one ETagged mirror worklist per tick at zero forge quota, so discovery is already off the forge budget. A lost race is not: a step pre-claim (`sweep_registry/step_spawn/lease.rs`, the `lease pr claim` verb) reads the lease comments, posts a lease, re-reads to confirm order (`resolve_lease_order`) and posts a yield, all on the forge owner's **writer** installation; a Builder loss also flips the label twice. On the measured fleet, losses ran at roughly 850 in three hours, about **1,500 writer calls an hour spent on races**. The budget goal is therefore **zero writer calls on a lost race**, and Phase 1 delivers it.

**The pass budget runs out before the slots fill.** A loss is free only up to `MAX_FREE_CLAIM_LOST_PER_TICK`, then it spends `MAX_PASSES_PER_TICK`. #12435 (0.19.1172) made losses cheap passes; #12448 (0.19.1173) is the further stopgap (free losses 16 → 128, passes 32 → 48). Neither stops the race.

**Hosts could run far more sessions.** At about 1% CPU per session a host is limited by how fast its tick finds work, not by compute. The queue must be able to feed about **100 concurrent sessions per host**.

## 3. What already exists

The design builds on these and must not duplicate them.

| Concern | Exists in | Note |
|---|---|---|
| The captain | `fleet_captain.rs`, `cli/fleet_captain_cmd.rs` (#8848, #8901) | A **placement gate**, not a service. `fleet.captain: "<host id>"` names one host; `resolve_gate` returns `Armed` / `Refused` / `NoCaptainDeclared`, fail-closed, exact match on `sweep_registry::host_identity`. `arm_singleton_job` records in-daemon jobs. **It listens on nothing.** No election, no failover (#8902 is an alert, not failover). |
| Hand-rolled HTTP servers | `serve.rs` + `cli/serve_cmd.rs`, `worker_spawn/egress_proxy/server.rs`, `observability/otlp/relay/server.rs` | No HTTP framework. `serve::validate_bind` defaults to loopback, needs an explicit opt-in otherwise, and refuses a wildcard bind. The relay authenticates before reading a body and caps connections. |
| The step tick | `step_dispatch/tick.rs` (#11909, #12298, #12410) | Rounds of draws, one per repo per round, gates, `StepSpawner::spawn_batch`. Stops are a closed set (`TickStop`, `loom.dispatch.stop`). |
| The draw and its weights | `step_dispatch/item_draw.rs` (#11896, #12185) | `iw = actionable × priority × age`; pool weight `w_repo × iw × S_r^(α−1) × (1 + in_flight_repo)^(−β)`; seeded, without replacement, replayable from `pick.decision` (`observability/pick_decision.rs`). Very-important cap: `work_finder/vi_cap.rs`. |
| Classification | `work_finder/step_item.rs` | Item state → step or pass. |
| Outcome ledger | `step_dispatch/outcome_ledger.rs` (#11900) | `(item, step)` → cooldown / repeat cap from the step's outcome class. Per host today. |
| Session ledger | `step_dispatch/session_ledger.rs` (#11913) | Soft host affinity: a later step on an item may resume the session that ran there. |
| Discovery | `discovery/` (#12306, #12311) | One `TickSnapshot` per tick. Mirror tier: one ETagged `GET /v1/mirror/worklist`; gateway and `gh` tiers capped at the backstop pace (`discovery/fallback.rs`). Never serves claim, lease, label or merge reads. |
| Mirror change stream | `forge_mirror/wake.rs` (#10892) | Pulls `GET /v1/changes` with a persisted per-repo cursor; each change becomes a wake prompt. |
| Gate-read invariant | `forge_events/poll_gate.rs`, invariants 1–3 | An event is a prompt, not a truth; polling is the floor; reads that gate a decision are never held. |
| Forge call purpose | `gh_invocation/purpose.rs` (#12310) | `loom.forge.purpose` = discovery \| gate \| write on every forge call. The measure for this design. |
| Issue lease | `defaults/docs/lease-record.md`, `defaults/docs/lease-renewal.md` | `<!-- loom:lease host=… sweep=… -->`; liveness is the comment's forge `updated_at`; TTL 15 min (`claim_reconciliation::resolve_lease_ttl_minutes`), renewed about every 5 min. **Four writers**: daemon dispatch, in-session sweeps, `worktree.sh`, hand claims. |
| Step (PR-phase) lease | `sweep_registry/step_spawn/lease.rs` (#11906, #9401) | `<!-- loom:pr-lease role=… host=… sweep=… step=… -->`: pre-claim, renew while the session lives, reap at step end. |
| Claim order | `sweep_registry/guards.rs` (`resolve_lease_order`, `LEASE_ORDER_LOOKBACK_SECS = 90`), `sweep_registry/lease_episode.rs` (#6287, #8840) | Earliest forge comment id wins; a renewed lease within the TTL still counts; a loser yields before touching a worktree. |
| Per-item pre-checks | `sweep_registry/preflip_labels.rs`, `sweep_registry/landed_guard.rs`, `work_finder::landed_prefilter`, the step resolvers | The live label read, the closed-issue read (`already_closed` / `already_landed`), the landed pre-filter, the role's own head check. |
| Peer channel | `peer_claims.rs` and `peer_claims/` (`steps.rs`, `brakes.rs`, `noop_streak_tally.rs`, `fast_fail_tally.rs`), `sweep_registry/pool_hold_broadcast.rs`, `sweep_registry/heartbeat_broadcast.rs`, `SweepRegistry::readvertise_peer_claims` | Claims, step claims, merge-in-flight, brakes, tallies, heartbeat. Silent since early October. |
| Host fit | `host_affinity.rs` (#7456), `capability.rs` (#6893, #11861) | `loom:host:<id>` / `<!-- loom:requires-host=… -->`; `<!-- loom:capability=… -->`. |
| Fleet config | `fleet_store/` (`compiled.rs`, `roster.rs`, `render.rs`, `state.rs`, `floor.rs`) | `fleet.json`: one fleet-wide roster with per-repo `weight` and `fleet: true \| maintain`, `firewall` exclusion, per-host config tiers, run state, `loom_min_version`. Fails closed. No per-host repo list in the roster today. |

## 4. The model

Two things live in the captain, and both are **soft**: lost on restart, rebuilt from the workers and the mirror, never persisted.

- **The assignment table.** One row per `(repo, item)` some host is about to work or works: host, step, assignment id, state (`granted`, `claimed`, `held`), and a deadline on the captain's **monotonic clock**. It is built from the captain's own grants and from every worker's live set. It is the same table in every phase: Phase 1 fills it from `claim` calls, Phase 2 from `next`.
- **The fleet queue** (Phase 2). The classified, weighted pool of every dispatchable item, derived from the mirror. It is a cache of the forge's state, refused freely by workers whose live pre-check disagrees.

Plus the **fleet ledger**: the per-`(item, step)` outcome state the per-host outcome ledger and the peer brakes kept (section 8), so cooldowns and streaks count fleet-wide.

What the captain is **not**: an owner of work, a forge writer, or a second lease record. The forge lease comment is the only ownership record. A grant says "nobody else in fleet mode will try this right now"; it does not stop an attended session, a hand claim or a fallback worker, and the forge decides those races as today.

## 5. Protocol

**The Phase 1 contract below is fixed** (2026-10-16). It is stated verbatim in the Phase 1 issues #12449 (server) and #12450 (client), which build against it in parallel; this section must match them, and any change goes through those issues first. Section 5.6 (`next`) is a Phase 2 proposal for review.

### 5.1 Transport and identity

- **HTTP/1.1 JSON over the tailnet.** The server binds the captain's tailnet address on a configurable port, with a hand-rolled listener in the style of `serve.rs` and the OTLP relay: identity checked before the body is read, a size cap, a connection cap, short head timeouts, strict parsing, never a panic on a malformed request. No new heavy dependency.
- **Identity.** The server resolves the caller with the Tailscale LocalAPI `whois` (remote address → tailnet node), then maps the node to a fleet `host_id` through an **explicit optional `tailnet_node` field on each host record** in the fleet config (node name → `host_id`; the `dispatchers` list stays plain host ids). It never assumes the node name equals the `host_id`; they diverge for operator machines and re-imaged hosts. A node not in that map is **refused with `403`**. The body's `host_id` must match the mapped one, or the request is refused. Identity resolution sits behind a trait, so tests inject identities (section 11).
- **All endpoints are `POST`** with JSON bodies, and **every response carries `server_epoch`**, which changes on every restart, so a worker can tell its table entries were rebuilt.

| Endpoint | Phase | Purpose |
|---|---|---|
| `POST /v1/heartbeat` | 1 | The host's live set, free slots and runnable roles. The server rebuilds its table from these. |
| `POST /v1/claim` | 1 | "May I claim this item I drew?" Grant or refuse. |
| `POST /v1/refuse` | 1 | The worker's pre-check or forge claim refused a granted item. |
| `POST /v1/release` | 1 | The step ended. |
| `POST /v1/next` | 2 (proposal) | Up to the host's free slots in assignments from the fleet queue. |

### 5.2 `heartbeat`

```json
{"host_id": "<fleet host id>",
 "live": [{"repo": "owner/name", "item": 123, "step": "judge", "assignment_id": "a-…"}],
 "free_slots": 12,
 "roles_ok": ["builder", "judge", "doctor", "curator", "champion"]}
```

→ `{"server_epoch": "…", "grace": false}`

- A worker sends it **every 30 s**, riding the existing reaper heartbeat (the cadence `SweepRegistry::readvertise_peer_claims` runs on), so a host with a long tick still renews.
- **`live`** is every step the host runs, including ones the captain never granted (fallback, attended). `assignment_id` is present only for granted ones. This is how the table is rebuilt after a restart.
- **`free_slots`** is the worker's own count. The server enforces a host's live assignments ≤ `min(free_slots + live, maxConcurrent)`, with `maxConcurrent` read from the fleet config, so a wrong report cannot hoard.
- **`roles_ok`** is host-local state the captain cannot know: a role whose token pool is exhausted, or behind the load brake or a disk back-off, is left out. Static fit comes from the fleet config (section 7).

### 5.3 `claim`

`{"host_id", "repo", "item", "step"}` → one of:

- `{"granted": true, "assignment_id": "a-…", "ttl_s": 1200}`: run today's pre-claim and forge lease path, unchanged. A **duplicate claim from the same host for the same `(item, step)`** is an idempotent re-grant: the same `assignment_id`, with the TTL refreshed. A claim for a **different** step on an item the host already holds is a new assignment: it goes through the full policy (the per-repo Champion merge lock, `roles_ok`, the cap), and only a grant replaces the old row, under a new `assignment_id`.
- `{"granted": false, "reason": "held" | "grace" | "not_eligible" | "cap", "held_by": "<host id>"?, "retry_after_s": 60}`: for `held`, `not_eligible` and `cap`, a **free pass with zero forge calls**; the item leaves this tick's pool. `grace` is the exception (below).

| Reason | Meaning |
|---|---|
| `held` | another live assignment holds the `(repo, item)`, or another host holds the repo's Champion merge step; `held_by` names the host when known |
| `grace` | the server is in its start-up grace window (section 15.1). The worker treats it as **fail-open for that one call**: it takes today's local forge-lease path for that claim. It does **not** count toward the fallback hysteresis (section 9), since it is expected after every roll. |
| `not_eligible` | in Phase 1, the step's role is not in the host's `roles_ok`, and nothing else; Phase 2 adds the per-host fit of section 7.2 |
| `cap` | the host is at its cap (5.2) |

### 5.4 `refuse` and `release`

- `{"host_id", "assignment_id", "reason"}` on `/v1/refuse`: the worker's live pre-check or its forge claim refused a granted item. `reason` is the existing refusal token. **`claim_lost` holds the item for the forge lease TTL** (someone outside fleet mode owns it, section 6); a stale reason (`already_closed`, `already_landed`, `claimed`, `head_moved`, …) **drops** it.
- `{"host_id", "assignment_id", "outcome"}` on `/v1/release`: the step ended. `outcome` is the step's outcome (#11868's class, `verdict | noop | refused | failed`), the input Phase 2's fleet ledger folds.
- **No-progress hold (#12541, a Phase 1 hotfix ahead of the Phase 2 ledger).** A `release` whose outcome is not progress (anything but `verdict`) holds its `(repo, item, step)`: every host's next `claim` for it is refused `cooldown` (span reason `no_progress_cooldown`; a worker records it as `captain_cooldown`, distinct from `captain_held`, another host's live claim) until the hold ends. It escalates per consecutive no-progress release of the triple, 30 min, 2 h, 6 h, then 24 h (`fleet-dispatch serve --no-progress-cooldown-secs`), and a `verdict` release of any step on the item clears every hold on it. A Champion step's class is its `CHAMPION-STEP` line folded (`outcome_ledger::champion_outcome`). A step that ended waiting on CI (Judge `noop` `ci_pending`, Champion `requeued`) releases as `ci_wait` (#12551): a flat 5 min hold that neither climbs the ladder nor clears other holds, so the item's next step is drawn within minutes of CI finishing. Only a refusal by the hold itself counts as `no_progress_cooldown`. Soft state: a captain restart forgets the holds.

**A row ends only by `refuse`, `release` or its TTL.** A row missing from a host's `live` set is **not** released early: a claim granted between one heartbeat and the spawn is legitimately absent from that heartbeat, and ending it on absence would let a second host be granted the same item. A lost `refuse` / `release` or a crashed host therefore costs one TTL (section 6).

### 5.5 Failures, from the worker's side

Unreachable, a timeout (short, about 2 s) or a `5xx` is a **failure** and counts toward fallback (section 9). A `403` (the host is not in the node map, or its `host_id` does not match) also fails open and counts toward the hysteresis, and the worker logs it at `ERROR`, so a mapping mistake is loud rather than a silent permanent fallback. A `claim` refusal is an answer, never a failure; a `grace` refusal sends that one claim down today's forge-lease path without counting as a failure. Nothing is retried within the tick.

### 5.6 `next` (Phase 2 proposal)

`{"host_id", "free_slots", "roles_ok"}` (the heartbeat's host state) → `{"server_epoch", "queue_age_s", "assignments": [{"assignment_id", "repo", "item", "step", "ttl_s", "seed"}]}`.

- An **empty** list is an answer: nothing fits this host now. The worker does **not** draw locally on an empty answer; that would bring the race back exactly when work is scarce.
- `seed` (`loom-draw/v3` minute, tick and draw index) lets `pick.decision` replay the draw from the captain's record.
- Each assignment goes through the same path as a drawn item: gates, live pre-check, pre-claim, `spawn_batch`. An assignment is a candidate, never a reason to skip a check. `refuse` and `release` close it as in Phase 1.

## 6. Leases, TTLs and the forge record

| | Captain row | Forge lease |
|---|---|---|
| Where | captain memory | an issue or PR comment |
| Says | "host H is assigned item I" | "host H's sweep S works item I" |
| Ends | `release`, `refuse`, or TTL; never on absence from `live` | release or reap, or TTL |
| TTL | `resolve_lease_ttl_minutes() + 5` min (20 by default) from the last heartbeat that renewed it | 15 min (`LOOM_LEASE_TTL_MINUTES`) from the last renewal |
| Clock | captain's monotonic clock | forge `updated_at` |
| Authoritative | no | **yes** |

**The captain never re-assigns before the forge lease could have lapsed.** The row TTL is the forge TTL plus a margin: `claim_reconciliation::resolve_lease_ttl_minutes() + 5` minutes, 20 at today's default. A host that dies after posting its forge lease leaves a renewed lease that out-ranks any new claimant in claim-then-verify-order (`lease_episode.rs` leg 2) until it ages out; re-assigning sooner only buys a wasted pre-claim. The TTL is derived from the forge TTL, not a new knob.

**Renewal is the heartbeat.** Every heartbeat that lists an assignment in `live` renews its row. A row ends early only by `release` or `refuse`; a row the host stops listing simply stops being renewed and ends by TTL (section 5.4 gives the race this avoids).

**`claim_lost` after a grant is a first-class outcome.** It means someone outside fleet mode (a fallback worker, an attended session, `worktree.sh`, a hand claim) won the forge order. The captain marks the row `held` for the forge TTL and assigns it to nobody; the forge lease's owner has it. A `held` row ends early if the mirror shows the item leaving its stage.

**Times never come from workers.** Deadlines run on the captain's monotonic clock from the moment it receives a call; worker clocks are never compared (the peer channel's "TTL against local receipt" rule, moved to one place).

**No new marker.** The forge lease comment is unchanged. The assignment id travels in telemetry only.

## 7. Scheduling policy

The captain runs **the same classifier and the same draw** a worker runs, over the fleet's items instead of one host's. It adds no second policy.

### 7.1 The pool

The captain classifies every item of every dispatchable roster repo (`fleet: true`; a `fleet: maintain` repo is registered but never dispatched into, and a `firewall` repo is never in the roster; `fleet_store/roster.rs`) with `work_finder::step_item::classify`, applies the very-important cap (`work_finder/vi_cap.rs`), and weighs with `item_draw::pool_weights`. Priority, stage age, the roster `weight`, α and β are unchanged. `in_flight_repo` becomes the fleet-wide count from the assignment table, exact rather than reconstructed from peer adverts. An item's weight is 0 while the fleet ledger has its `(item, step)` cooling down.

### 7.2 Per-host fit

A draw for a host is restricted to items that host fits:

- **the repo**: the roster is fleet-wide today (every host registers every desired repo), so every dispatchable repo, unless the host's tier of the fleet config holds it;
- **affinity and capability**: a host-affinity constraint (`host_affinity.rs`) names it, and a capability item's markers (`capability.rs`) are ones its fleet config grants;
- **runtime and toolchain**: a step needs a runtime or toolchain the host's fleet config says it has. Today host affinity markers carry toolchain needs; adding host-tier fields for this is a Phase 2 question, not a Phase 1 need;
- **the role**: the step's role is in the host's `roles_ok`;
- **not taken**: no row for the item in the table.

### 7.3 Exact fleet-wide rules

- **One Champion merge per repo**, fleet-wide. The step-dispatch design's §6 serialisation was best-effort across hosts; the table makes it exact.
- **One step per item.** No two rows for one item.
- **The `(item, step)` ledger** (section 8): cooldowns, repeat caps and streaks count every host's outcomes.

There is no new per-repo or per-host quota. **Exact per-repo concurrency was considered and declined**: the operator removed per-repo caps, and `maxConcurrent` is the only cap. The busy term β keeps doing what it does, now on exact counts.

### 7.4 Rounds, fairness and starvation

- A `next` fills up to `free_slots` in rounds, at most one item per repo per round, as the tick's rounds do (#12298). A host asking for 100 slots gets them spread over the repos it fits.
- Every repo with `weight > 0` and an actionable item keeps a non-zero share, and its age term grows until it is drawn. A low-priority repo is drawn less, never starved.
- Calls are answered in arrival order and each takes at most its own free capacity. A host with more free slots gets more work; there is no per-host quota.
- An item only some hosts fit enters only those hosts' draws; it does not shrink anyone else's share.

### 7.5 Session affinity

From `release` the captain knows which host last ran each `(item, role)` (the session ledger's key, #11913). When that host calls and the item is eligible, the item is offered to it first. It is a preference, not a reservation: another host draws it if the first does not call within its normal tick. Lost on restart, which only costs a resume.

### 7.6 Who calls

A host is in fleet mode when its fleet config tier names the captain (section 13.4). Operator workstations opt out by not being configured; they keep the local path and the forge lease, and their claims show up in the table through `claim_lost` and the mirror.

## 8. What the peer channel carried, and where it goes

The peer channel was more than claims. Each of its signals either moves to the captain or is stated to die; none is left without a home by accident.

| Signal | Today | Under captain dispatch |
|---|---|---|
| Claim / step claim ads (`Advertise`/`Retract`, `peer_claims/steps.rs`) | the draw's peer exclusion (`draw_claims.rs`) | **The assignment table.** |
| Merge in flight (`peer_claims/steps.rs`) | best-effort one merge per repo | **The table**, exact (7.3). |
| Step cooldown (`StepCooldownArmed`, #11900) | per-host ledger + broadcast | **The fleet ledger**, fed by `release`. |
| No-op cooldown (`NoopCooldownArmed`, #7477) and no-op streak (`noop_streak_tally.rs`, #12024) | broadcast window + per-host tallies | **The fleet ledger**: one streak per item, counted from every host's `release`. |
| Dispatch backoff (`DispatchBackoffArmed`) and fast-fail streak (`fast_fail_tally.rs`, `fast_fail_fleet.rs`, #11930) | broadcast window + summed count | **The fleet ledger.** The park itself stays a forge write by the worker, as today. |
| PR-less retry tally (`PrlessReleaseArmed`, #9292) | per-host count, summed | **The fleet ledger.** |
| Pool-exhaustion hold (`PoolHoldArmed`/`Cleared`, `pool_hold_broadcast.rs`, #8001) | broadcast per token pool | **Host state.** A host leaves the role out of `roles_ok`. A pool shared by several hosts is reported by each; no fleet copy needed. |
| Completion narration (`ClaimKind::Completed`, #6352) | stops two hosts narrating the same public-feed completion | **Not a dispatch signal; not moved.** It is dark with the channel today. This design does not host it; Phase 3 deletes only the channel's dispatch use, and its owner decides its home. |
| Issue-filing lock (`ClaimKind::FilingLock` / `FilingUnlock`, #6714) | fleet-wide hold on `gh issue create` bursts | **Not a dispatch signal; not moved.** Dark with the channel today. A fleet-wide lock fits the captain naturally, but hosting it is out of scope here; its owner decides. |
| Heartbeat (`heartbeat_broadcast.rs`, #8736) | liveness ad | **Dies** for dispatch: every captain call is a heartbeat. Other consumers of host liveness keep their telemetry source. |

Workers keep their local copies of these for the fallback path, so a fallback tick still honours its own host's cooldowns. In Phase 3 the broadcasts are deleted; the local ledgers stay.

## 9. Fail-open

**In Phase 1 the fallback is today's local tick with a reduced draw budget.** Phase 1 changes only the claim, so the worker's own discovery and draw are still running every tick and there is nothing else to fall back to.

**From Phase 2 on**, the fallback must not need the per-host gather that Phase 3 retires, so it is **the #12304 discovery chain at its capped backstop pace** (`discovery/fallback.rs`), which stays for standalone installs anyway, feeding the same local draw.

1. **A `grace` refusal is not a failure.** After a captain start, `grace` sends that one claim down today's local forge-lease path, and the hysteresis counter is untouched: a roll is routine and must not push hosts into fallback or stall dispatch for the window.
2. **Enter on 3 consecutive failures** (section 5.5), not on one. A single failed call claims as today for that item, and the worker tries the captain again on the next one.
3. **In fallback**, each tick:
   - discovers as the worker does today (Phase 1), or through the worker's own chain at the backstop pace (Phase 2 on);
   - runs the local draw with a **reduced budget**: one round (`spawn_concurrency` items at most) per tick and no free `claim_lost` allowance, so a captain blip does not restart an eight-way race at full rate;
   - honours its local ledgers and every forge gate, as today;
   - keeps sending heartbeats, which are its probes.
4. **Leave** after **5 minutes of successful probes**; **any failure resets the 5-minute timer**, so a flapping captain does not flip modes every tick.
5. **Alert** when the fleet's fallback share passes 5% of ticks over an hour. Fail-open is safe for correctness, which is exactly why it can hide a broken captain.

The counts (3 failures, 5 min, one round) are constants, not knobs, until telemetry shows they are wrong.

**Split-brain.** While a worker is in fallback, the captain may assign an item that worker draws locally. Both run the pre-claim; the forge lease decides; the later one yields. If the captain's host lost, it `refuse`s with `claim_lost` and the row goes `held` (section 6). Nothing new is needed for correctness; the refusals counter's `reason=claim_lost` counts it.

**A bad release.** The captain rolls on the same version floor as the workers. A release that breaks the captain sends workers to fallback, which is the existing tick code; a release that breaks the tick breaks today's dispatch too, with or without a captain.

## 10. The fleet queue: change stream, refresh, staleness

### 10.1 The change stream drives it

The captain follows the mirror's `/v1/changes` stream, the pull and cursor `forge_mirror/wake.rs` already implements. A change to a repo re-reads that repo's worklist rows and re-classifies just those items. Per `poll_gate.rs` invariant 1, an event is a prompt, not a truth: it triggers a re-read, never an edit from the event's contents. For the captain this needs #12311 and #12319 as **library code it can host** (the `discovery` chain and the wake pull over a fleet-wide repo set), not only as a per-host loop.

### 10.2 The full refresh is the backstop

Every `queueRefreshSecs` (default 600) the captain re-reads the whole worklist (ETag-conditional; a `304` only advances the age). This is invariant 2's correctness floor: a lossy change stream costs at most one refresh of staleness. The worklist is keyed "every repo the caller's key may read", so the captain needs a mirror key that spans the fleet's repos.

The per-item facts the gather still reads outside the worklist (hold markers, CI at Judge heads, landed verdicts, stage entry; `step_dispatch/gather.rs`) are read **once per change or refresh by the captain**, under the gather's existing budgets, instead of once per host per tick. As #12317 moves them into the worklist, the captain's queue needs no forge read.

### 10.3 Staleness: live pre-check, refuse, drop

The costly stale transitions, and what catches each:

| Transition | Cost if missed | Caught by |
|---|---|---|
| Item closed or merged | a wasted session start | the closed-issue read (`landed_guard.rs`) |
| Stage label moved | the wrong step | the live label read (`preflip_labels.rs`) |
| PR head moved | a review of the wrong head | Judge and Champion's own `head-moved` refusal |
| Builder on an issue whose PR has landed | a duplicate PR | `landed_prefilter` at spawn, kept |

The pre-check stays a **live** read of labels and state (invariant 3), never served from the queue. On a refusal:

1. The worker `refuse`s with the existing token.
2. The captain drops the entry and **tombstones** `(repo, item, updated_at)`: a refresh showing the same `updated_at` does not re-admit it, a newer one does. A stale mirror cannot feed the same refused item back every refresh.
3. The worker's mirror recheck report (#12318) still goes to the mirror, so the mirror corrects itself.

## 11. Security

- **Tailnet only.** The listener binds the captain's tailnet address, never a wildcard (the `serve::validate_bind` rule).
- **An access-policy grant.** The tailnet's access policy admits the fleet's dispatching hosts to the captain's port and nothing else, the same pattern the fleet already uses for its telemetry reader.
- **Identity is `whois` plus an explicit roster map.** For each connection the captain asks its local tailnet daemon who the peer is (the Tailscale LocalAPI `whois` for the remote address; the captain process needs read access to the tailnet daemon's local socket). It maps the returned node to a `host_id` through the optional `tailnet_node` field on each host record in the fleet config (the `dispatchers` list stays plain host ids), and never by assuming node name == `host_id` (they diverge for operator machines and re-imaged hosts). A node not in the map is refused `403` even if the access policy lets it connect, and the mapped `host_id` must equal the body's. Nothing in the daemon crate does this today (`git grep whois` finds nothing); it is new, small and local.
- **No new credential.** No token, key or JWT is issued for this. Nothing to rotate or leak. A compromised worker already holds a forge writer credential, so the captain adds no blast radius; server-side caps (5.2) and per-host fit (7.2) stop it from hoarding assignments.
- **The captain holds no forge write.** It reads the mirror and, until #12317, the gather's reads. It never posts a label, comment or merge. Every bad assignment it could hand out still meets the worker's live pre-check and the forge lease.

## 12. Telemetry

All signals go out over OTLP with the daemon's existing exporter, with `loom.repo` / `loom.issue` where an item is involved.

| Signal | Where | Meaning |
|---|---|---|
| `loom.fleet_dispatch.call` span, one per call | worker and captain | `host`, endpoint, latency, granted or refused, `reason`, `server_epoch` (Phase 2 adds `queue_age_s` and queue size) |
| `loom.assignment.id` attribute | the existing `loom.dispatch.step` span | joins assign → pre-check → claim → outcome |
| `claim_lost` after a grant (**derived**, not a new signal) | captain | the existing refusals counter with `reason=claim_lost`, which only a `refuse` after a grant can carry; the split-brain and attended-claim measure |
| `loom.fleet_dispatch.refusals` counter | captain | refusals by reason: `claim` refusals and `refuse` tokens |
| `loom.fleet_dispatch.expired` counter | captain | assignments that hit their TTL |
| `loom.fleet_dispatch.fallback_ticks` counter | worker | fallback ticks per host (the entry reason only if #12450 records one on it; no separate signal) |
| `loom.fleet_dispatch.queue_refresh` span (Phase 2) | captain | full or change-driven, bytes, `304` or not, items, forge reads by `loom.forge.purpose` |
| writer calls per spawned step | derived | `loom.forge.purpose = write` per step spawn; the Phase 1 headline |
| `loom.dispatch.stop` distribution, occupancy at cap | worker (existing) | the tick SLIs this design must move |
| `pick.decision` | captain (Phase 2) | the existing record, written where the draw now happens |

## 13. Rollout, phases and follow-up issues

### 13.1 Phase 1: the claims table (the minimal slice; contract fixed, issues filed)

- **Server** (#12449): `loom-daemon fleet-dispatch serve` on the captain host, in a new `fleet_dispatch/` module: `proto.rs` (the section 5 serde types, shared with the client), `server.rs`, `table.rs` (the assignment table and its policy, pure, tested with an injected monotonic clock) and `identity.rs` (the identity trait and a LocalAPI `whois` implementation). The host roster comes from the fleet store's `fleet.json` dispatchers, and #12449's `fleet_store` loader reads the tailnet **node** (`tailnet_node`) from each dispatcher's host record (node name → `host_id`, section 11). The join from dispatcher to host record is by host name: loom-ui resolves a dispatcher by name or `host_id`, while the loader resolves by name only, which fails safe (an unmatched dispatcher is unresolved, `403`). It is its own subcommand because `fleet-captain <JOB_NAME>` takes its first argument as a job name; it should run only on the declared captain (the `fleet_captain` gate is the existing way to say so).
- **Client** (#12450): `fleet_dispatch/client.rs` on `reqwest` (already a dependency) with short timeouts, hooked into the step claim path before the forge pre-claim (`sweep_registry/step_spawn/lease.rs`). A refusal is a free pass with **zero forge calls**; a grant runs today's lease path unchanged and carries `assignment_id` to the `loom.dispatch.step` span and to `release` / `refuse`. The heartbeat rides the 30 s reaper heartbeat. The fallback state machine (section 9) is a pure function. With the flag off, behaviour is byte-identical to today.
- **Deployment wiring** (the deployment-wiring issue in the operator's infra repo): the tailnet access-policy grant, the captain's service unit, the `tailnet_node` field on the host records of the captain and the trial workers, and the per-host flag on **two workers**; the rest are the control group.
- The table is the one `next` uses in Phase 2.

**Exit, on the flagged hosts against the control group:** `claim_lost` under 5% of step draws; no `pass_limit` stops with free slots; occupancy at `maxConcurrent`; writer calls per spawned step down; no ownership incident against attended sessions or hand claims.

### 13.2 Phase 2: the captain assigns from the fleet queue

The queue (section 10), the fleet ledger (section 8) and `next`. Configured workers call `next` each tick; `claim` remains for fallback and older workers. Roll out host by host.

**Exit:** over a week, `claim_lost` near 0 on captain-sourced ticks; few `pass_limit` / `slot_draws` stops with free slots; worker discovery at the backstop pace only; a host able to hold about 100 sessions reaches it when the backlog allows.

### 13.3 Phase 3: retire

When fallback is rare (proposed: under 1% of ticks fleet-wide over 14 days, and no captain outage longer than one refresh in that window), delete in fleet mode: the per-tick gather on workers, the peer-channel broadcasts (section 8), `draw_claims.rs`'s peer half, and the #12448 free-`claim_lost` allowance. Kept forever: the forge lease and claim-then-verify-order, the backstop-paced discovery chain (fallback and standalone), and the local ledgers.

### 13.4 Config

Phase 1 adds two per-host keys, live-reloadable and set from the fleet config's host tier:

- `fleetDispatch.enabled` (bool): this host uses the captain. Off by default and on every standalone install.
- `fleetDispatch.url`: the captain's tailnet URL.

Setting them on two hosts is the Phase 1 A/B; setting them in the fleet defaults puts every host in fleet mode. Phase 2 proposes one more, `fleetDispatch.queueRefreshSecs` (the full-refresh period, default 600), read by the captain.

### 13.5 Follow-up issues

**Phase 1 (filed 2026-10-16, built against the section 5 contract):**

| Issue | Scope |
|---|---|
| A: #12449 | The captain server, `loom-daemon fleet-dispatch serve`: claim arbiter, soft state, roster auth. Reads the host records' `tailnet_node` field in the `fleet_store` loader. |
| B: #12450 | The worker client: `claim` before the forge pre-claim, `heartbeat`/`refuse`/`release`, fail-open with hysteresis and a reduced budget. |
| C: the deployment-wiring issue in the operator's infra repo | The tailnet access-policy grant, the captain unit, the host records' `tailnet_node` field in the fleet config, the per-host flag on two workers. |

**Phases 2 and 3 (proposals; filed only after the operator reviews this doc):**

| Phase | Proposed issue | Scope |
|---|---|---|
| 2 | Discovery and wake as hostable library code | #12311 / #12319 over a fleet-wide repo set; a fleet-spanning mirror key. |
| 2 | Fleet queue | Change-stream updates, full-refresh backstop, captain-side classify and weigh, tombstones. |
| 2 | Fleet ledger | Cooldowns, streaks and tallies from `release` (section 8). |
| 2 | `next` and worker fleet mode | Per-host fit, rounds, session affinity, `fleetDispatch.queueRefreshSecs`. |
| 3 | Retire per-tick discovery in fleet mode | Workers keep the backstop-paced chain only. |
| 3 | Retire the peer channel's dispatch use | Delete the broadcasts, the draw-claims peer half, the #12448 allowance. |

## 14. Relation to #12304

One design, not two. #12304 makes discovery one mirror read per tick **per host**; this design makes the dispatch decision once **for the fleet**, in the captain, on top of what #12304 builds.

| #12304 part | Under captain dispatch |
|---|---|
| Mirror worklist endpoint, freshness by continuity, `/v1/changes` | **Stays.** The queue's source and its update stream. |
| Discovery interface and mirror backend (#12311) | **Stays**, as library code the captain hosts. Workers in fleet mode stop reading it **per tick** and read it at the backstop pace for fallback and the non-dispatch readers. |
| Direct backend (#12312) | **Stays**: standalone installs, and the fallback tiers. |
| Snapshot consumers: `queue_blocked`, quarantine, claim reconciliation (#12313–#12315) | **Stay on the worker's snapshot.** They are per-host reconcilers, not draws. |
| Gather per-item reads into the snapshot (#12317) | **Stays**; it takes the captain's queue to zero forge reads. |
| Claim refusal recheck (#12318) | **Stays**: the worker still tells the mirror. |
| Wakes on change (#12319) | **Moves to the captain** in fleet mode, as hostable library code. Workers keep it for fallback and standalone. |
| Per-host per-tick worklist read | **Replaced** by the captain's queue in fleet mode. |

The GitHub saving is #12304's (discovery) plus this design's (no writer calls on a lost race). What the captain saves on top of #12304 is mirror load (one reader instead of N per tick) and the gather's remaining per-item forge reads until #12317 lands.

## 15. Failure matrix and edge cases

| Case | What happens |
|---|---|
| Captain down | Workers enter fallback after 3 failures (section 9); dispatch continues at the reduced budget. |
| Captain slow | Each call has a 2 s timeout; a slow call is a failure; 3 in a row is fallback. |
| Captain partitioned from some hosts | Those hosts fall back; the others keep fleet mode; the forge lease settles overlaps, and the captain marks losses `held`. |
| Split brain (captain assigns, fallback host draws) | Forge lease decides; the loser yields; counted as refusals with `reason=claim_lost`. |
| Attended or hand claim | Same as split brain: `claim_lost`, row `held` for the forge TTL. |
| Clock skew | None to handle: captain deadlines are monotonic on the captain; forge liveness uses forge `updated_at`; no worker timestamp is compared. |
| Captain restart or roll | New `server_epoch` and a grace window; 15.1. |
| Worker dies holding an assignment | 15.2. |
| Stale entry, item already closed | 15.3. |
| Fleet config change | Picked up on the captain's next fleet-store reload; applies from the next draw. |
| Worker on an older version | Draws locally, a permanent fallback host; the version floor removes it. |

### 15.1 Captain restart mid-assignment

**Nothing is persisted.** On start the captain takes a new `server_epoch` and opens a **grace window**: 90 s by default, ending early once every **fleet-mode** host (`fleetDispatch.enabled`, not every roster host) has heartbeated. During it `claim` is answered `{"granted": false, "reason": "grace", "retry_after_s": …}` (and, in Phase 2, `next` returns nothing), while heartbeats and their `live` sets are accepted, so the table is rebuilt from the workers before anything is granted. The worker takes today's local forge-lease path for each `grace`-refused claim, without counting it toward fallback, so a captain roll is never a 90 s dispatch stall; during the window the forge lease settles races as it does today. In Phase 2 the captain also reads the worklist and builds the queue during the window.

Grants outstanding at the crash are lost. Each was either claimed (the host's next `live` set puts it back in the table), refused, or never tried. A new grant of an item someone claimed in the gap ends in that host's pre-claim yielding. Persisting the table would add a second record that can disagree with the forge after a crash.

### 15.2 A worker dies holding an assignment

Its heartbeats stop. Its rows live out their TTL (`resolve_lease_ttl_minutes() + 5`, 20 min by default), which is longer than its forge lease's 15 min, so when the item returns to the queue the dead lease has aged out of claim-then-verify-order and the next claimant wins. Existing reclamation (`claim_reconciliation.rs`) restores labels on the forge side on the same TTL. If it died before posting a forge lease, the cost is the same 20 min of an idle item: the price of never re-assigning before a lease could have lapsed.

### 15.3 A stale entry whose item is already closed

The worker's closed-issue read refuses it with `already_closed` (or `already_landed`) before any forge write. The worker `refuse`s; the captain drops and tombstones the entry; the change stream (or the next refresh) brings the closed state and the item leaves the queue. Cost: one live read the worker would have made anyway.

## 16. Scale

- **Request rate.** Eight dispatchers, each one `next` or `claim` burst per tick plus a heartbeat every 30 s: a few requests a second at most. Each call is a filtered weighted draw over a few thousand items. The captain is not a bottleneck at any plausible fleet size.
- **Feeding 100 sessions per host.** A host's free slots are filled in one `next`, in rounds, not one draw per tick pass. Sessions take minutes, so a 100-slot host frees a few slots a minute and asks on its next tick.
- **The captain's own forge use** is bounded by the change rate and the refresh, not by host count or requests: the change stream and ETag-conditional worklist reads (no forge quota), plus, until #12317, one budgeted gather's worth of per-item reads per change or refresh. It makes no claim, lease, label or merge call.
- **Workers' forge use** falls to the reads and writes of the steps they actually start. A refused `claim` costs zero forge calls.
- **Memory.** One snapshot plus small tables; a few MB.

## 17. Alternatives considered and open questions

### 17.1 Alternatives

- **Revive the peer-claim channel.** Eventually consistent, so it only narrows the race, and it has gone dark once. Rejected.
- **Per-host credentials or a JWT.** The tailnet identifies the host; a token adds rotation and leak surface for no gain. Rejected.
- **Shard repos across hosts.** Removes the race but strands work when a host is down or full, and fights per-host fit.
- **Persist captain state.** A second record of who works what that can disagree with the forge. Soft state rebuilt in one warm-up is enough.
- **The captain posts the forge claim.** It would need writer credentials and would claim for a host that may never start the step.
- **Full 10-minute refresh as the only source.** Up to 10 minutes of wrong steps and wasted starts; the change stream is already there.

### 17.2 Open questions for the operator

1. **Runtime and toolchain fit.** Are host affinity markers enough for Phase 2, or should the host tier gain explicit runtime/toolchain fields?
2. **Phase 3 threshold.** Is under 1% fallback ticks over 14 days the right gate?
3. **Fallback constants.** 3 failures in, 5 minutes of successful probes out, one round per tick, a 5% fallback-share alert: acceptable as starting constants?
4. **Heartbeat consumers.** Does anything besides dispatch read the peer heartbeat (#8736), and does it need a replacement?
5. **Non-dispatch peer signals.** Completion narration (#6352) and the issue-filing lock (#6714) are dark with the peer channel and not moved by this design. Should the captain host the filing lock later?

## 18. Review log

| Date | Reviewer | Point | Resolution |
|---|---|---|---|
| 2026-10-16 | Design review (Fable), verdict GO-WITH-CHANGES | The forge-budget win was misattributed: #12304 already makes discovery quota-free; the drain is lost claims. | §2 restates the goal as zero writer calls on a lost race; Phase 1 delivers it; measured with `loom.forge.purpose` (§12). |
| 2026-10-16 | same | The baseline is the dead peer channel, which also carried cooldowns, merge serialisation, pool holds, tallies, brakes. | §2 caveat; §8 gives every signal a home or says it dies. |
| 2026-10-16 | same | Fail-open contradicted Phase 3 (local draw needs the gather). | §9: fallback is #12304's chain at backstop pace, with hysteresis, a reduced budget and a fallback-share alert. |
| 2026-10-16 | same | An assignment is not ownership. | §4, §5.5 (`refuse`), §6: `claim_lost` after a grant holds the item for the forge TTL; row TTL = forge TTL + margin. |
| 2026-10-16 | same | Captain state must be soft, on a monotonic clock, with a grace window. | §4, §6, §15.1. |
| 2026-10-16 | same | The change stream should drive the queue, the full refresh as backstop; keep the live pre-check and `landed_prefilter`. | §10. |
| 2026-10-16 | same | Host-local constraints, server-side caps, exact merge serialisation, a fleet `(item, step)` ledger, session affinity, eight dispatchers. | §5.2, §7, §8, §2. |
| 2026-10-16 | same | Auth: `whois` plus roster mapping plus an access-policy grant; no JWT. | §11. |
| 2026-10-16 | same | Nits: stopgap versions; a fleet-spanning worklist key; #12311/#12319 as hostable library code. | §2, §10.1–10.2, §13.5, §14. |
| 2026-10-16 | same | Minimal first slice: `fleet-dispatch serve`, two flagged workers, `claim` before the pre-claim, exit criteria. | §13.1 is that slice. |
| 2026-10-16 | Phase 1 build session, by agreement | Open question: should a `grace` refusal stall the claim? | Settled: `grace` is fail-open for that one call (today's forge-lease path) and does not count toward hysteresis. §5.3, §5.5, §9, §15.1. |
| 2026-10-16 | same | Open question: is the tailnet node name the `host_id`? | Settled: no. Each dispatcher entry carries an explicit node field; `whois` → node → `host_id`; unmapped nodes get `403`. §5.1, §11. |
| 2026-10-16 | Judge (changes requested at `c0af4bcf`) | §5.4 and §6 disagreed on a row missing from `live`. | One rule: rows end only by `refuse`, `release` or TTL; absence never ends a row (it races a grant made between heartbeat and spawn). §5.4, §6. |
| 2026-10-16 | same | §9 described the Phase 3 fallback. | Phase 1 fallback is today's local tick with a reduced budget; backstop-pace discovery is Phase 2 on. §9. |
| 2026-10-16 | same | `claim_lost_after_grant` and a fallback entry reason were not in #12449's contract. | Both are derived: refusals with `reason=claim_lost`; the entry reason only if #12450 records one on the fallback counter. §9, §12, §15. |
| 2026-10-16 | same | §8 used `roles_runnable` and missed `Completed` (#6352) and `FilingLock`/`FilingUnlock` (#6714). | Renamed to `roles_ok`; rows added (not dispatch signals, not moved). §8, §17.2. |
| 2026-10-16 | same | The node field's owners were unstated. | #12449's `fleet_store` loader reads it; the deployment-wiring change adds it to the fleet config. §13.1, §13.5. |
| 2026-10-16 | Phase 1 build | The node field was specified on dispatcher entries, but the dispatcher list is plain host ids. | Moved to an optional `tailnet_node` field on host records (the `dispatchers` list is unchanged), matching #12449. §5.1, §11, §13. |
| 2026-10-16 | same (recommended, all taken) | TTL value; grace end; `403`; duplicate claims; Phase 1 `not_eligible`; fallback exit timer; per-repo concurrency. | TTL = `resolve_lease_ttl_minutes() + 5` min; grace ends once every fleet-mode host has heartbeated; `403` fails open, logs `ERROR`, counts toward hysteresis; a same-host duplicate claim is an idempotent re-grant; Phase 1 `not_eligible` is `roles_ok` only; any failure resets the 5-min exit timer; exact per-repo concurrency considered and declined. §5.3, §5.5, §6, §7.3, §9, §15.1. |
| 2026-10-16 | Judge (changes requested at `d8b31aa3`, on #12465) | A claim from a row's own host for a different step was re-granted, rewriting the row's step and skipping the merge lock and `roles_ok`. | Only the same `(item, step)` is re-granted; a different step runs the full policy and, if granted, replaces the row under a new id. §5.3. |
| 2026-10-16 | Operator (on #12465) | Where the node field lives: `dispatch.dispatchers` objects would break the store's schema, render step, dashboard and shell readers, which take strings. | `dispatch.dispatchers` stays host-name strings; the node is `hosts[].tailnet_node` on the host record, joined by name. A dispatcher with no record or node is unresolved (`403`, startup warning). §5.1, §11, §13.1. |
