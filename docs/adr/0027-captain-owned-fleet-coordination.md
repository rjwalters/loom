# ADR-0027: Captain-Owned Fleet Coordination for the Landed Cooldown and the Fleet Streaks

## Status

**Proposed** (2026-10-17). Design only, for #12050 (reconciles #11112). No
code, deletion, deployment, access, spend or live captain change ships with
this ADR, and it selects **no storage or transport backend**.

It is written against `main` at `9f0fa9553`. Counts and line numbers below are
stamped to that commit and must be re-derived before an implementation
increment acts on them.

Partly supersedes [ADR-0025](0025-forge-lease-only-cross-host-claim.md): that
ADR's decision 2 ("no-op cooldown: per-host only") covers the #7477 window
broadcast. It was written before the three lanes below existed and is **not**
permission to drop them. See "Relationship to ADR-0025".

Operator ruling this ADR implements (#12050, 2026-10-17, attended fleet-decide
discussion 17, Q2): captain-owned fleet coordination is the long-term
direction; design comes first; current safeguards stay until a replacement and
cutover are reviewed and accepted.

## Context

ADR-0025 made the forge lease the only cross-host *claim* and let the
peer-claim channel go. Between that ruling and now, three protections were
built on top of that channel. Each answers a question a lease cannot, because
each is about **history across hosts**, not about who holds an issue now:

| Lane | Added by | What it protects | Where it lives today |
|---|---|---|---|
| Landed cooldown | #11853, #11901 | An issue left open after its closing PR merged is not redispatched, on any host, for hours (`LANDED_COOLDOWN` = 6 h, capped at 24 h by `MAX_LANDED_COOLDOWN`). | A `DispatchBackoffArmed` ad (`sweep_registry/dispatch/landed_cooldown.rs`). Receivers install `now + remaining` locally. |
| Fleet fast-fail streak | #11930, #11952, #11942 | The fast-fail park (#11854) fires on the third consecutive issue-attributable fast failure counted across hosts. | The ladder's `DispatchBackoffArmed` ad carries the sender's own streak (`peer_claims/fast_fail_tally.rs`, `sweep_registry/fast_fail_fleet.rs`). |
| Fleet no-op streak | #12024 | The no-op hold (#10156) parks an issue after `holdThreshold` consecutive no-ops on an unchanged fingerprint, counted across hosts. | A `NoopStreak` ad carrying a per-host count and a fingerprint digest (`peer_claims/noop_streak_tally.rs`, `sweep_registry/noop_cooldown/fleet_streak.rs`). |

The recorded incident (#12024): one issue took **10 no-op sweeps from 6
dispatchers in 97 minutes**, and no host saw more than 2. With a per-host
threshold of 3 the hold never tripped, the item stayed promotable, and Champion
re-promoted it. Counting per host is exactly the failure the fleet lanes were
built to close, so "per-host" is not an accepted fallback (ruling).

### Why the current mechanism cannot be the end state

The ads travel on the peer-claim channel, which the #11112 direction removes.
Independently of that, the channel has properties a long-term design should not
inherit:

- **It is soft and lossy by design.** Sends are fire-and-forget
  (`send_noop_streak_ad` drops on a full queue; `peer_claims/send_health.rs`
  documents a six-day outage where the daemon still reported the channel as
  ordinary). A dropped ad is a silently lower count, i.e. unknown becomes zero.
- **Every host keeps a private, receipt-timed copy.** Break semantics are
  reconstructed from "when did the break arrive here" (`fast_fail_break_at`,
  `noop_streak_break_at`), so two hosts can disagree about what the streak is.
- **Late joiners and restarts start empty.** A host that restarts has no peer
  tally until peers re-advertise, so the fleet count is lower exactly when the
  fleet has been unstable.
- **There is no acknowledgement.** The sender cannot tell whether any peer
  counted its outcome.

### What exists to build on

- `fleet.captain` (`loom-daemon/src/fleet_captain.rs`, #8848): one host named in
  the tracked `.loom/config.json`, **assigned, not elected, fail closed**. It
  already hosts singleton jobs and reports `is_captain` in host health.
  Captain failover is explicitly alert-only (#8902), and ADR-0024 reused the
  captain as a publisher for the same reason.
- The forge lease and park record remain the cross-host *claim* and the
  durable record of a *park* (`park-record apply`, body-resident).
- Local per-host counters already exist for all three lanes and are not
  removed by this design.

## Decision

Introduce one **coordination authority**, owned by the fleet captain, that
holds the cross-host tallies for these lanes. Workers **report** outcomes to
it and **query** it; they stop deriving fleet counts from peer broadcasts.
Nothing is deleted until the cutover in section 7 is accepted.

This ADR fixes the contract. It deliberately does not fix how the authority
stores its state or how reports travel (section 8).

### 1. Captain authority and source of truth

1. The captain named by `fleet.captain` is the **sole writer** of fleet tallies
   for these lanes. Workers never write tallies; they send *reports* and the
   captain applies them. This keeps the "exactly one" property of
   `arm_singleton_job` and adds no election.
2. The **durable record is an append-only log of accepted reports** per
   `(repo, issue, lane)`. Tallies, epochs and windows are *derived* from it and
   can be rebuilt. This is the property that makes restart and failover
   tractable (section 4); where and how the log persists is the backend choice.
3. A **park** keeps its existing durable home: the forge park record and labels
   written by the host that applies it. The captain decides *when a threshold
   is reached*; the parking host (or the captain acting on a worker's behalf)
   still writes the forge record. A park therefore survives total loss of the
   authority, which matches the current `adopt_restored_parks` behaviour.
4. **Captain identity is configuration, not discovery.** A worker accepts
   tallies only from the host that `fleet.captain` names in its own tracked
   config. A config disagreement shows up as an authority mismatch (below), not
   as a split brain.

Cost model. Let `W` be fleet-wide counted outcomes per hour (landed arms,
fast-fail records, no-op records, plus their breaks) and `Q` the worker query
rate. Writes to the authority are `W`, one per accepted report, bounded by the
number of *sweeps that ended*; the #12024 incident is 10 outcomes in 97
minutes, about 6 per hour on the worst issue. Queries are batched **per repo
per tick**, not per issue: one `tally_snapshot(repo)` returns every non-zero
tally, so `Q = hosts × ticks/h`, independent of backlog size. Forge API cost of
the default path is **zero** reads and zero writes (the authority is not the
forge); only a park costs forge writes, exactly as today. If a backend choice
puts the log on the forge, each accepted report is one comment write and each
snapshot is one conditional list read, so `W` and `Q` above become the forge
bill and the backend ADR must restate them against `github.ratelimit.used`.
These are formulas with stated assumptions, not measurements; the shadow phase
(section 7) produces the measured values.

### 2. Per-lane specification

Common to all lanes:

- **Report** = `{report_id, lane, repo, issue, reporter_host, sweep_id,
  outcome_class, sequence, observed_epoch, fingerprint_digest?, window_secs?}`. No
  session content, prompts or transcripts; only identifiers the daemon already
  holds for its own bookkeeping.
- **Trusted outcome evidence.** A report is accepted only from an authenticated
  fleet host identity (same trust root as the lease and `fleet.captain`; no new
  credential). An unresolved reporter (`UNKNOWN_HOST`, #9518) is **rejected**,
  not counted, because it cannot prove it is not an echo of the captain's own
  report. The outcome class is the reporter's own *existing* classifier output;
  the authority never re-classifies, so classifiers and thresholds are
  unchanged (ruling).
- **Deduplication key.** `(lane, repo, issue, reporter_host, sweep_id)`. A
  sweep ends once, so a retry or a duplicate delivery of the same sweep's
  report is a no-op that returns the original acknowledgement.
- **Time.** Windows and expiry are measured on the **captain's clock at
  accept time**, never a reporter's wall clock. This is today's "local receipt,
  not the advertiser's clock" rule (`peer_claims/brakes.rs`), now applied once
  instead of once per receiver.
- **Epochs serialise resets.** Each `(repo, issue, lane)` has a monotonically
  increasing `epoch`. A break increments it. A report carries the epoch its
  reporter last saw.

| | Landed cooldown | Fleet fast-fail streak | Fleet no-op streak |
|---|---|---|---|
| **Evidence** | Reporter verified the issue is open while its closing PR merged (`landed_guard` / step ledger). Report names the merged PR number so the record is auditable. | An issue-attributable fast failure as classified today; environmental classes and lost lease races are reported as *transient* (count `0`), as now. | A counted no-op sweep (`note_noop_for_hold` paths) with the fingerprint digest it was counted against. |
| **Identity / scope** | `(repo, issue)` | `(repo, issue)` | `(repo, issue, fingerprint_digest)` |
| **Value kept** | `until` (captain time), extend-only | Per-host count; fleet streak = sum | Per-host count for the current fingerprint; fleet streak = sum |
| **Window** | Caller's length, capped at 24 h (`MAX_LANDED_COOLDOWN`). **The cap is enforced at the authority**; today it is enforced only by the sender, and a receiver installs `now + remaining` without one. | Entry counts while younger than the reader's ladder `max` (6 h default); hard-pruned at 24 h (`MAX_PEER_FAST_FAIL_STREAK_AGE`). | Entry kept 24 h (`MAX_PEER_NOOP_STREAK_AGE`); a fingerprint change restarts it. |
| **Reset** | Explicit clear (issue reopened after landing, or Curator keeps it, #11901); a clear never shortens a *later* arm. | Break on real progress and on operator `quarantine clear`; a non-attributable failure zeroes only the reporter's entry (`streak_break` semantics, #11952). | Break on park or release; any fingerprint change implicitly retires older-fingerprint entries. |
| **Expiry** | `until` passes. | Entry ages out. | Entry ages out. |
| **Unavailable authority** | See section 4: windows already known stay in force; new arms are recorded locally and reported on recovery. | Count is `Unknown`, not `0`. | Count is `Unknown`, not `0`. |

Concurrency, late and out-of-order events:

- **Concurrent reports** from different hosts are serialised by the captain;
  the order it accepts them in is the order of record. Sums are order
  independent, so two hosts reporting at once cannot lose a count.
- **Duplicate** report: dedup key hit, original ack returned, no change.
- **Reordered** (a report delivered after a later one from the same host):
  per-host counts are *cumulative-by-replace* as today (an ad reports a running
  total), so the higher `sequence` for `(reporter_host, lane, issue)` wins and
  a lower one is acked and ignored.
- **Reset race.** A report whose `observed_epoch` is older than the lane's
  current epoch was produced against a streak that has since been broken. It is
  **rejected with the new epoch** and is not counted. The reporter reconciles
  by resetting its own local streak to the epoch boundary (the behaviour
  `fast_fail_break_at` provides today) and counts its *next* outcome.
- **Late event after expiry or park**: accepted into the log for audit, not
  added to a closed streak; it can only start a new one.
- **Fingerprint skew** (no-op lane): a report whose digest differs from the
  tally's current digest replaces the tally only if it is *newer by captain
  accept order*; otherwise it is recorded and ignored. A peer counted against
  an older state of the issue is never summed into the current streak, which is
  today's rule.

### 3. Worker delivery and acknowledgement

- **At-least-once with idempotent apply.** A worker enqueues a report in a
  bounded durable outbox and retries until it receives an ack carrying
  `{accepted|duplicate|stale_epoch, epoch, tally_version}`. The ack is the
  worker's evidence that its outcome counted; an un-acked report is a known,
  counted gap, not a silent drop.
- **Coalescing and bound.** The outbox holds at most one pending cumulative
  report per `(lane, issue)` plus pending breaks (breaks are never coalesced
  away). On overflow the oldest non-break entry is dropped **and the overflow
  is counted and surfaced as degraded** (section 6); it is never silent.
- **Retry and backpressure.** Exponential backoff with jitter and a ceiling,
  per authority, so a recovering captain is not met by a synchronised burst
  from every worker. The captain may answer `retry_after`; a worker honours it.
- **Dispatch never blocks on the authority.** A query has a short deadline.
  A miss yields `Unknown` (section 4), and dispatch proceeds under the local
  floor. A wedged captain must not wedge the fleet (the same lesson as ADR-0025
  on failing closed).

### 4. Restart, failover, outage, stale evidence

The invariant: **an unknown count never becomes zero.** The query result is a
tri-state, `Known(n, as_of)` or `Unknown(reason)`, and each consumer states
what it does with `Unknown`.

| Situation | Behaviour |
|---|---|
| Authority reachable, answer fresh | `Known`. Decide as today. |
| Answer older than the staleness bound (`as_of`) | `Unknown(stale)`. |
| Authority unreachable / deadline missed | `Unknown(unreachable)`. |
| Worker's configured captain differs from the answering host | `Unknown(authority_mismatch)`; alert. |
| Captain restarted, log not yet replayed | `Unknown(recovering)` until replay completes. |

Consumer behaviour on `Unknown`:

- **Landed cooldown**: a window already installed locally stays in force until
  it expires (protection is not *lowered* by an outage). A new landed arm is
  applied locally at once and queued for report. Local windows are the floor.
- **Streaks**: the decision uses `max(local count, last Known fleet count)`.
  This can park later than the fleet-wide count would, never earlier, and never
  on a made-up number. The shortfall is **exposed**: each decision made under
  `Unknown` emits the degraded marker (section 6), so "this host is protecting
  per-host only right now" is visible, not implied.
- **Local counters stay.** The per-host counters that exist today are kept as
  the floor and as the replay source. They are not replaced by the authority.

Captain restart: replay the durable log, rebuild tallies and epochs, then
serve. Until then `Unknown(recovering)`.

Captain failover: stays **assigned, not elected** (the `fleet_captain.rs`
position, #8902: a late singleton is acceptable, a double one is not). Moving
the captain is a tracked-config change. The new captain (a) loads the durable
log if the backend makes it reachable, otherwise (b) asks each worker to send a
**resync report** of its own local counters and outbox, and answers
`Unknown(recovering)` until it has heard from every host or a bounded resync
window has passed. Anything not recoverable is reported as lost protection
(section 6), not rebuilt as zero. Two hosts claiming captaincy at once is
refused by the epoch/authority-mismatch rule rather than merged.

Explicit loss of protection, enumerated rather than implied. Protection is
lower than today's *healthy* peer channel when: the outbox overflowed; a
worker never heard from the captain during a streak; the log was unrecoverable
after failover. Each is a named counter and a degraded marker.

### 5. The #12024 incident shape

Replay of the recorded incident against this design: 6 hosts, 10 no-ops on one
issue in 97 minutes, no host above 2, `holdThreshold` 3.

1. Each no-op is reported with the same fingerprint digest. The first three
   accepted reports (any hosts) make the fleet sum 3, so the **third no-op,
   not the tenth, trips the hold**; at most one further sweep (one already in
   flight when the tally crossed) can begin before the park lands. That is the
   same shape as the existing fleet lane, with the difference that delivery is
   acked and a dropped ad cannot lower the count.
2. If the authority is unreachable for the whole window, each host decides on
   its local count of at most 2 and the hold does not trip: **the incident
   recurs**. The design states this plainly. It is only prevented while the
   authority is reachable, and the difference from today is that the situation
   is *visible* (`Unknown`, degraded marker, alert) instead of silent.
3. The park is applied on `loom:blocked` with a park record, never
   `loom:operator`, and the item is not re-promoted until its fingerprint
   changes (#12024 acceptance), unchanged by this ADR.

Deliberate non-claim: this ADR does not claim exclusion in an authority outage.
Exclusion between concurrent builders remains the forge lease's job and its
retained fail-open limitation (ADR-0025).

### 6. Observability

Ordinary telemetry only, following the existing policy (host id, Loom version
and full SHA on every span; no random trace ids; no session-content ingestion;
no new access):

- Counters per lane: reports sent, acked, duplicate, `stale_epoch`, retried,
  dropped-on-overflow; breaks sent.
- Gauges: outbox depth and age of oldest un-acked report; last-`Known` age per
  repo; number of tallies currently non-zero.
- Query latency histogram and the `Unknown` count by reason.
- A `coordination.source` attribute (`captain` | `local_floor` | `degraded`)
  on each span for a dispatch decision that consulted a lane, so a park or a
  dispatch can be traced to the evidence it used.
- `HostHealthRecord` already carries `is_captain`; add the degraded flag
  alongside it so the fleet dashboard shows which hosts are on the local floor.
- Alerts: authority mismatch, sustained `Unknown`, outbox overflow.

Bounded operation: tally maps are capped by entry age (24 h), per-issue
cardinality is the host count, and the outbox is bounded as in section 3.

### 7. Cutover and rollback

Existing safeguards stay until the lane's cutover is accepted. Lanes cut over
one at a time, each in four stages; each stage is reversible by configuration
alone.

1. **Shadow.** Workers still decide from peer ads exactly as today **and**
   send reports. The authority computes tallies and decisions but nothing acts
   on them. Record divergence between the ad-based and authority-based answer
   per decision. This stage also produces the measured `W` and `Q`.
2. **Compare.** Review divergence over an agreed period. Explained divergence
   (for example ads the old channel dropped) is evidence for the cutover;
   unexplained divergence blocks it.
3. **Authoritative with floor.** Decisions use the authority, with the local
   floor of section 4. Peer ads are still sent and still received, only
   ignored for the decision. Rollback is flipping the lane back to ad-based
   decisions; nothing was deleted.
4. **Ads off.** Stop sending the lane's ads. Only after all three lanes reach
   here, and #11112's removal is separately reviewed, are the peer-claim
   code paths deleted.

Order proposed: landed cooldown first (a window, no counting, simplest to
verify), then fast-fail streak, then no-op streak (the most state: fingerprint
and breaks). The wire change at each stage is additive; an older worker keeps
reading ads and is unaffected, mirroring how `NoopStreak` was introduced.

#11717 keeps its own review/conflict block and is not a prerequisite or a
dependent of this design.

### 8. Backend: deliberately not selected

The contract above needs a coordination store that can: append-and-read a log
keyed by `(repo, issue, lane)`; serialise writes from a single writer (the
captain); survive a captain restart; and be read by a replacement captain.
Candidate shapes, **listed so the choice can be made later, not chosen here**:

- State owned and held by the captain process with its own durable file, with
  workers talking to it over an authenticated channel.
- A forge-resident record (comments or a dedicated record) written by the
  captain only.
- The operator-run GitHub state mirror that ADR-0024 names, if it can accept
  captain writes.

Each needs its own review for access, cost and failure modes, and **any option
that requires a new service, credential or spend needs separate operator
approval**. The first implementation increment is a store-agnostic interface
plus an in-memory implementation for tests; the backend ADR follows.

### 9. Step-dispatch call sites

Today's peer-claim consumers inside step dispatch, and where each lands:

| Call site | Today | Disposition in this design |
|---|---|---|
| `ClaimKind::Advertise`/`Retract` with `step` (`publish_step_claim`, #11898) | Soft claim read before the label flip | **Claim, not a tally.** Under ADR-0025 the lease is the cross-host claim; this design does not move claims to the captain. Removal is #11112's, after the lanes above. |
| `PeerClaimView::claimed_issues_at` in `step_spawn/draw_claims.rs` | Draw skips items a peer advertises | Same: the draw already skips items this registry runs; the lease tie-break remains the cross-host guard. |
| `PeerClaimView::merge_in_flight_at` (Champion merge serialisation, design #11894 §6) | Best-effort per-repo merge serialisation | Stays best-effort; correctness stays with the merge-queue handoff and `merge-pr.sh --expected-head`. Open question 2. |
| `QueueDisposition::PeerClaim` in `step_dispatch/tick.rs` | Disposition for `claimed`/`claim_lost`/`peer_active`/`backstop_hold`/captain-held | The name outlives the channel; rename for the lease-only meaning in the removal increment. |
| `ClaimKind::StepCooldownArmed` (#11900, `MAX_PEER_STEP_COOLDOWN` 24 h) | Window per `(item, step)`, extend-only | A window lane like the landed cooldown, keyed by `(issue, step)`. Not one of the three lanes the ruling names. Open question 1. |

These are distinct from ADR-0025's older per-host broadcasts: those
(`NoopCooldownArmed`, pool hold, heartbeat, quarantine) were advisory
optimisations and stay dropped. The three lanes here change a *decision* (park
or not, dispatch or not for hours) and so keep protection.

### 10. Adversarial test plan (for the implementation increments)

Each is a deterministic test against the in-memory store; none needs a live
fleet.

- **Duplicate**: same report delivered twice, and the same sweep reported by
  retry after a lost ack; tally unchanged, same ack.
- **Reorder**: cumulative reports `3` then `2` from one host; count stays `3`.
- **Cross-host concurrency**: six hosts report the same issue at once; sum is
  exact, order does not matter, threshold trips on the third accepted report.
- **Reset races**: break concurrent with a report; report observed pre-break is
  rejected with the new epoch and not counted; a post-break report counts as 1.
  Break racing the park: park applied once, no re-park from stale counts.
- **Fingerprint skew**: a peer's older-fingerprint count is not summed; a
  fingerprint change restarts the streak.
- **Late event**: a report after expiry or park cannot revive a closed streak.
- **Unresolved identity**: `UNKNOWN_HOST` report contributes nothing and breaks
  nothing.
- **Landed cap**: a 30-day `window_secs` is clamped to 24 h at the authority;
  a later shorter arm does not shorten a longer one; reopen clears it.
- **Outage and recovery**: authority down mid-streak → `Unknown`, decision
  uses local floor, degraded marker emitted; on recovery the outbox drains and
  the tally matches a no-outage run. Unknown is never rendered as `0`.
- **Restart and failover**: replay rebuilds identical tallies and epochs;
  failover with an unrecoverable log answers `Unknown(recovering)` then reports
  the loss; two hosts claiming captaincy → `authority_mismatch`, no merge.
- **Outbox**: overflow drops the oldest non-break and increments the loss
  counter; a break is never dropped.
- **Lease exclusion unchanged**: the existing `lease_cross_host_tests.rs`
  cases pass untouched with the authority up, down and in shadow mode.
- **Incident replay**: the #12024 sequence (10 no-ops, 6 hosts, max 2 per
  host) parks at the third report with the authority up, and does not park
  (and says so) with the authority down.

### 11. Implementation increments (for later issues, entering at `loom:triage`)

Filed only after this design is reviewed and accepted. Each is reviewable on
its own on current `main`, none removes a safeguard before its replacement is
accepted.

1. Store-agnostic coordination interface, report/ack/epoch types, in-memory
   implementation, and the adversarial tests of section 10 against it.
2. Backend decision ADR (section 8), with access and cost review.
3. Captain-side service: log, tally derivation, epochs, restart replay.
4. Worker side: durable bounded outbox, ack handling, `Known`/`Unknown`
   query, local floor. Shadow mode for the landed cooldown.
5. Telemetry and degraded-state surfaces (section 6).
6. Per-lane compare-and-cutover for the landed cooldown, then fast-fail, then
   no-op streak (section 7), one reviewed change each.
7. Failover/resync procedure and its runbook entry.
8. After all lanes are at stage 4 and #11112 is separately reviewed: delete the
   peer-claim code, rename `QueueDisposition::PeerClaim`, retire ADR-0025's
   stale statements. This is the successor of the deletions #11717 attempted,
   replayed on a fresh branch; #11717 itself stays blocked.

## Relationship to ADR-0025

ADR-0025 decision 2 keeps the #7477 *no-op cooldown window* per host and
accepts up to N× dilution. That remains true and is unchanged here: a window
that only delays a candidate is an optimisation. The landed cooldown, the
fast-fail streak and the no-op streak were added **afterwards**, and they gate
a *decision*: a park, or hours without dispatch. The older ruling's reasoning
("rare, self-limiting, fail-open") does not carry to them, and #12024 is the
counter-example. ADR-0025 is annotated to say so. Its lease decision is
unchanged, and this design never moves claiming off the lease.

## Consequences

### Positive

- One source of truth for fleet counts. Every host sees the same tally, and a
  reset happens once, in a defined order, instead of being reconstructed per
  receiver from message arrival time.
- Delivery is acknowledged, so "my outcome counted" is knowable, and a lost
  message is a counted gap instead of a quietly lower number.
- A restarted or late-joining worker is told the current state instead of
  starting empty.
- Removes the dependence on the peer-claim channel for protections that
  matter, which unblocks #11112.
- Loss of protection is a first-class, observable state.

### Negative

- The captain becomes a soft dependency of three protections. When it is
  unavailable the fleet is on the per-host floor and the #12024 incident can
  recur (section 5). This is a stated, visible cost, not a hidden one.
- Captain failover remains manual and alert-only (#8902); recovery after a
  lost log is partial and reported as such.
- New cross-host contract (reports, acks, epochs) and a stateful component to
  operate. The backend decision is deferred and carries its own cost.
- A migration with dual paths for a period (shadow and compare), including
  the additional report traffic measured in stage 1.

## Alternatives Considered

- **Accept per-host for all three lanes** (the ADR-0025 position extended).
  Rejected by the ruling: the #12024 incident is the demonstrated cost.
- **Forge-backed equivalents per lane** (a marker comment or label per
  outcome, read by every host). A viable *backend* for section 8, not a
  different design; as a per-lane, per-host-read mechanism it multiplies reads
  by host count and has no single ordering authority, which is the weakness the
  captain removes.
- **Keep the peer-claim channel for these lanes only.** Rejected for the same
  reason as in ADR-0025: it keeps the whole channel alive for a subset, and the
  channel's lossy, unacknowledged delivery is the root problem.
- **Elect an authority by lease.** Rejected: the captain precedent
  (`fleet_captain.rs`) explicitly prefers assigned over elected, and a double
  authority is worse than a late one.
- **Decide fail-closed when the authority is unreachable** (refuse dispatch).
  Rejected: a wedged captain would wedge the fleet. The local floor with a
  visible degraded marker is chosen instead.

## Open Questions for Review

1. **Step cooldown (`StepCooldownArmed`, #11900)** is a fourth window lane the
   ruling does not list. Should it ride the same authority (cheap: it is the
   landed cooldown keyed by step) or be declared per-host? Until decided it
   keeps working as it does today.
2. **Merge serialisation** (`merge_in_flight_at`): accept best-effort lease-only
   plus the existing merge-time guards, or give it a short authority-held
   per-repo lock? This ADR assumes the former.
3. **Staleness bound** for `Known` answers and the resync window after failover:
   values are not chosen here; the shadow stage should propose them from
   measurement.
4. **Where a captain-applied park is written** when the captain acts for a
   worker: through the captain's forge identity or the worker's. Either
   preserves the park record format; the choice is about audit attribution.

## References

- Related GitHub Issues: #12050, #11112, #11717, #12024, #11930, #11942,
  #11952, #11853, #11901, #11900, #11898, #11854, #10156, #8848, #8902, #9518,
  #7477
- Related ADRs: [ADR-0025](0025-forge-lease-only-cross-host-claim.md),
  [ADR-0024](0024-cross-host-read-poller-dedupe.md),
  [ADR-0014](0014-forge-coordination-decoupling.md)
- Code read as design input (no edits):
  `loom-daemon/src/peer_claims/{steps,fast_fail_tally,noop_streak_tally,brakes,send_health}.rs`,
  `loom-daemon/src/sweep_registry/{fast_fail_fleet,noop_cooldown/fleet_streak,dispatch/landed_cooldown}.rs`,
  `loom-daemon/src/sweep_registry/step_spawn/draw_claims.rs`,
  `loom-daemon/src/step_dispatch/tick.rs`, `loom-daemon/src/fleet_captain.rs`
- Lease record format: [`defaults/docs/lease-record.md`](../../defaults/docs/lease-record.md)
