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

The property #11112 asks for is **two hosts on a shared backlog never build
the same issue at once.** The cooldowns are optimisations on top of whatever
exclusion the claim provides. They do not provide it. The peer-claim channel
did not provide it either: it was a soft advisory read before the label flip,
and it never took part in the lease tie-break below. So removing it neither
weakens nor strengthens that tie-break.

This ADR does **not** claim that #11112's unconditional criterion is fully
enforced. It records the narrower property the forge lease does give and the
fail-open cases where it does not (see "Retained fail-open limitation").

## Decision

1. **The forge lease is the cross-host claim.** A dispatcher writes
   `<!-- loom:lease host=… sweep=… -->` (`lease ensure`, `write_lease_comment`)
   and then runs the claim-then-verify-order tie-break
   (`SweepRegistry::resolve_lease_order`, #6287, with #6816/#6951/#6994
   retries and the #9453 leaseless-label leg). No side channel is consulted.
   **When a dispatcher can read the forge and sees both its own lease and an
   earlier peer lease inside its bounded read-back and confirmation window**
   (a few retries of 300 ms and 500 ms, about 2 s in total), it yields before
   it spawns a builder or touches a worktree. The earliest lease in the
   forge's id order wins.
   `loom-daemon/src/sweep_registry/lease_cross_host_tests.rs` pins both what
   this gives and what it does not. It runs two registries under two
   `LOOM_HOST_ID`s with no shared local state and no peer-claim publisher.
   With both leases visible, exactly one host proceeds and flipping the forge
   order flips the winner. A dispatch-level case runs the two hosts one after
   the other on one shared forge comment list and shows only the first-leased
   host reaching builder spawn. That is the visible case. It is not a test of
   concurrent publication. The same file
   also asserts the fail-open cases below, where both hosts proceed.
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
  tries once and then arms its own cooldown. Each of those dispatches goes
  through the same lease tie-break, so they are exclusive under the
  visibility assumptions in Decision 1 and subject to the same fail-open
  limitation below. No-op releases are rare and the cost limits itself.
- A host learns about a peer's claim only from the forge, so it pays one lease
  read-back per dispatch. That read already happens today. The peer view only
  ever added an early back-off on top of it.

### Retained fail-open limitation

`resolve_lease_order` only adds a refusal when it has positive evidence of an
earlier peer lease. It never makes one up from a read it cannot verify. This
ADR keeps that policy as it is. As a result, **two hosts can both proceed**
in these cases, and the tests above assert that they do:

- **Exhausted read-back.** Every lease read fails (rate limit, outage) for
  the whole retry budget. The dispatcher proceeds (`guards.rs`, the
  `read_lease_comments` → `None` branch). Pinned by
  `exhausted_read_back_lets_both_hosts_proceed` and
  `resolve_lease_order_proceeds_when_the_read_fails`.
- **Own lease not visible.** The dispatcher never sees its own lease within
  the retry budget, because its write failed or did not propagate. It
  proceeds. Pinned by `own_lease_never_visible_lets_both_hosts_proceed` and
  `resolve_lease_order_proceeds_when_its_own_comment_is_not_found`.
- **Peer invisible past the window.** The confirmation window is bounded. If
  a peer's earlier lease is still invisible to a host when that host's
  window closes, the host proceeds. The peer later sees both leases, finds
  itself earliest, and proceeds too. Pinned by
  `staggered_visibility_both_proceed_inside_the_invisible_window` and
  `dispatch_with_a_peer_lease_never_visible_spawns_on_both_hosts`.

So the forge lease alone gives exclusion **only when each dispatcher can read
the forge and sees the competing lease inside its window**. It does not give
unconditional exclusion. Removing the Matrix peer-claim channel does not
change this, because that channel was never part of the tie-break. Fixing the
limitation would mean changing the fail-open policy (for example, failing
closed or re-checking later in the lifecycle). That trades the residual race
for a risk of wedging dispatch, so it needs its own decision and is out of
scope here.

**Status of #11112's "never build at once" criterion:** still open. This
increment verifies only the narrower property above. It does not enforce the
unconditional criterion.

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
