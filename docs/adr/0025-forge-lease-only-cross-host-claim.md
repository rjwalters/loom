# ADR-0025: The Forge Lease Is the Only Cross-Host Claim; Dispatch Cooldowns Stay Per-Host

## Status

Accepted (2026-10-10). Recorded by slice 1 of #11112, the removal of the
safehouse (Matrix) integration. The code that this ADR retires (the
peer-claim channel and the #7477 cooldown broadcast) is deleted in a later
slice of the same issue. This ADR stands on its own and does not depend on any
safehouse document, since those are being deleted too.

Partly supersedes [ADR-0014](0014-forge-coordination-decoupling.md)
where that ADR treats safehouse as a coordination accelerator.

## Context

The operator decided in #11112 that Loom coordinates through forge records and
labels only. That removes the safehouse room, and with it two kinds of
cross-host signal that went through it:

1. **Peer claims** (#4028, #6157 Layer 3). A host advertised a soft claim in
   the room so other hosts backed off before the non-atomic `loom:building`
   label flip.
2. **Advisory broadcasts.**
   - **#7477, no-op cooldown / dispatch backoff.** When a sweep released
     without a PR, the host armed a local cooldown and also published it
     (`SweepRegistry::publish_peer_cooldown_claim`). Consumers
     (`noop_cooldown_issues`, `noop_cooldown_dispatch_block`, #9928) take the
     union of the local map and the peer view. With safehouse off, which is
     the default, the publisher is `None` and the broadcast already does
     nothing (#9186).
   - **#9936, quarantine broadcast.** This was never implemented.
     `sweep_registry/quarantine.rs` has no publish or consume path. It was
     deferred and nothing was built.

The one property that must survive is correctness: **two hosts on a shared
backlog never build the same issue at once.** The cooldowns are optimisations
on top of that property. They do not provide it.

## Decision

1. **The forge lease is the cross-host claim.** A dispatcher writes
   `<!-- loom:lease host=… sweep=… -->` (`lease ensure`, `write_lease_comment`)
   and then runs the claim-then-verify-order tie-break
   (`SweepRegistry::resolve_lease_order`, #6287, with #6816/#6951/#6994
   retries and the #9453 leaseless-label leg). The earliest lease in the
   forge's id order wins. Every other dispatcher yields before it spawns a
   builder or touches a worktree. No side channel is consulted.
   `loom-daemon/src/sweep_registry/lease_cross_host_tests.rs` pins this. It
   runs two registries under two `LOOM_HOST_ID`s with no shared local state and
   no peer-claim publisher. Given one shared forge lease list, exactly one host
   proceeds, and flipping the forge order flips the winner.
2. **No-op cooldown (#7477): per-host only.** The cooldown broadcast is
   removed with the peer-claim code in a later slice of #11112. Each host keeps
   its own local cooldown. No forge marker replaces the broadcast.
3. **Quarantine broadcast (#9936): dropped.** It was never built, so nothing
   is removed. #9936 is closed as not planned and points here.
4. **Pool-hold (#8001) and heartbeat broadcasts** go with the peer-claim
   channel for the same reason. The local pre-flight already stops each host
   on its next tick.

## Consequences

### Positive

- One claim mechanism, readable by every host and by a human looking at the
  issue. No Matrix daemon, socket or room is needed to run a fleet.
- The "peer-claim coordination is DEGRADED" alert class goes away, because
  the thing it monitored goes away.
- No new forge writes. A forge-visible cooldown marker would add a comment or
  label write on every no-op release.

### Negative

- **Up to N× dilution of the no-op cooldown.** On an N-host fleet, a no-op
  candidate can be dispatched up to N times per cooldown window: each host
  tries once and then arms its own cooldown. This costs extra work, not
  correctness, because the lease still makes each of those dispatches
  exclusive. No-op releases are rare and the cost limits itself.
- A host learns about a peer's claim only from the forge, so it pays one lease
  read-back per dispatch. That read already happens today. The peer view only
  ever added an early back-off on top of it.

## Alternatives Considered

- **A forge-visible cooldown marker** (comment or label per no-op release).
  Rejected for now. It would add API traffic and a new marker to parse and
  expire, all to optimise an advisory, fail-open path. The label-based
  `decline_cooldown.rs` lane already shows the forge-visible pattern for the
  cases that need it, and it stays as it is.
- **Keeping the Matrix channel only for cooldowns.** Rejected. It keeps the
  whole safehouse dependency alive for an optimisation, which goes against the
  #11112 decision.

**Revisit trigger:** if N× re-dispatch of the same no-op candidate is observed
as real churn (#7466/#7468-style flapping), file a follow-up for a cooldown
keyed on the forge lease comment. Do not bring back a side channel.

## References

- Related GitHub Issues: #11112, #7477, #9936, #9928, #9186, #8001, #6287,
  #6816, #6951, #6994, #9453, #4028, #6157
- Related ADRs: [ADR-0014](0014-forge-coordination-decoupling.md),
  [ADR-0006](0006-label-based-workflow-coordination.md)
- Lease record format: [`defaults/docs/lease-record.md`](../../defaults/docs/lease-record.md)
