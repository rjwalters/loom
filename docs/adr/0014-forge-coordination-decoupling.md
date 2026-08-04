# ADR-0014: Decouple Forge API Cost From Coordination Chatter — Local Evaluation Memo, Safehouse as Accelerator Only

## Status

Accepted (design decision). Implementation is phased into follow-up issues — see
"Suggested phasing" below; this ADR does not itself change any code.

## Context

The forge (GitHub) does two jobs for Loom: it is the **durable record** (issues,
PRs, comments, merge history) and it is the **real-time coordination medium**
(who owns this issue, what changed since a role last looked). Loom implements
coordination as label mutations on a rate-limited HTTP API, so API pressure
scales with coordination chatter rather than with actual work done.

Verified 2026-08-03 (#4500): GitHub Enterprise Cloud's GraphQL uplift
(10,000 points/hr) applies only to Enterprise-owned resources, and `loom`,
`anvil`, and `safehouse` are User-owned (`rjwalters`) — the busiest repo in the
fleet gets nothing from buying capacity. The fix has to be traffic reduction,
not quota expansion.

The most wasteful traffic is **repeat passes over state that has not changed**:

- #4736 — duplicate "still blocked, no change" comments, 7x on one issue
- #4987 (merged) — a one-off, curator-only fix for exactly that pattern
- Champion re-verdicting deliberately-parked issues every cycle
- concurrent curators stacking contradictory comments on the same issue
- Judge's claim races (`loom:reviewing` POST, then back off — a wasted mutation
  pair)

The ~6 label transitions in a normal issue lifecycle are inherent, not waste.
The waste is re-evaluating something whose inputs have not changed, independently,
per role, per host.

Three levers were proposed (#5057):

- **Lever A** — a memo of "role X evaluated issue Y at input-hash Z" so a role
  can skip re-evaluating unchanged state.
- **Lever B** — a cache-invalidation broadcast ("repo X changed") so hosts
  invalidate on notice instead of by polling.
- **Lever C** — GitHub webhooks → the existing Cloudflare Worker → fan-out to
  daemons, replacing polling-based change *detection* with push.

Four questions had to be answered before any of this could be built. This ADR
answers them.

## Decision

### 1. Where does the memo live?

**The memo lives in daemon-local persistent state, one store per host.
Safehouse is a transport for propagating memo entries between hosts, never the
store of record, and the forge is never the store either.**

This follows the pattern the daemon already uses for `worktree_reaper.rs`,
`quarantine_reconciliation.rs`, and `claim_reconciliation.rs`: periodic,
host-owned state that a Rust daemon subsystem maintains directly, not a role
agent's job and not a forge mutation. It also follows safehouse's existing
degradation contract (`.loom/docs/safehouse.md`): safehouse is documented as a
best-effort side-channel with **zero hard dependency** — "Loom never blocks a
sweep on safehouse" — so a store that safehouse could ever be the sole holder
of would violate that contract the moment the persona is unreachable (recall
2026-08-03: safehouse was `configured, unreachable` on robb-pro).

Concretely: each daemon keeps a small local table keyed by
`(role, issue_or_pr, input_hash) -> {result, timestamp}`. When safehouse is
enabled, a host that writes a new memo entry also emits a lightweight
broadcast so peer hosts can populate their own local table without paying
their own evaluation cost — this is Lever B, riding the existing fan-out
channel and its established envelope-routing / degrade-to-`warn!` posture
(`.loom/docs/safehouse.md` § Configuration). Without safehouse, every host
still gets full single-host benefit (the #4736 shape — repeat passes *on one
host* — is fixed by the local memo alone); only the cross-host duplicate-pass
case degrades to "each host evaluates once instead of zero times," which is
strictly the pre-existing behavior, not a regression.

**Retention and invalidation.** Retention is a housekeeping concern, not a
correctness one — see decision 3. A memo entry is correctness-safe to keep
indefinitely because it self-invalidates: on read, the role recomputes the
current input-hash and compares it to the memoized one; any mismatch is
treated as "not yet evaluated," full stop. There is no proactive
invalidation message to get wrong. Housekeeping bounds the table's size with
a straightforward cap (LRU eviction past N entries, e.g. mirroring
`claim_reconciliation`'s existing aging conventions) and drops entries for
issues/PRs the daemon has independently observed as closed — reusing the same
closed-issue detection the #4088/#4123 dispatch guards already perform, not a
new mechanism.

### 2. Does the Worker become a control-plane participant (Lever C)?

**Not in this phase. Stop at Lever B.** Lever C is deferred, gated on Lever A
+ B being measured in production and shown insufficient.

The observability pipeline (`.loom/docs/observability.md`) is explicitly
one-directional today — daemon → Worker, "the read-only invariant" — and
every hop is infrastructure the operator deploys and points their own daemons
at. Lever C inverts that: GitHub → Worker → daemons makes the Worker a
control-plane participant, which requires solving webhook secret distribution
across 12 repos spanning a User account and the 2AMLogic org, a new inbound
trust boundary on a Worker that today only ever authenticates *outbound*
daemon telemetry, and replay/idempotency handling — none of which exists yet
and all of which the issue itself correctly flags as "a real architectural
decision, not an implementation detail."

Critically, Lever C's stated win — eliminating change-*detection* cost
entirely — is already substantially captured by infrastructure that exists
today: the ETag-cached REST listing (`forge_listing`, #4428) answers an
unchanged poll with a `304` at **zero** rate-limit cost, and Lever A's memo
eliminates the redundant-evaluation cost that sits *behind* a changed-listing
result. Once A and B are deployed and measured, Lever C's marginal benefit is
exactly "poll interval → near-zero latency to detect change," which is a
latency win, not a quota win — the design constraint that webhook delivery is
unguaranteed already requires keeping the ETag poll running underneath it
regardless. Building the higher-risk, higher-cost lever before measuring
whether the cheaper levers already solved the quota problem is the wrong
order of operations. If A+B measurement (AC 5, deferred to the implementation
phase) shows residual GraphQL pressure specifically attributable to poll
latency rather than redundant evaluation, Lever C becomes its own follow-up
design issue at that point — not before.

### 3. What is the input-hash?

**A content hash over the specific fields each role's decision function
actually reads — never the bare `updated_at` timestamp as the memoized key.**
`updated_at` may still be used as a free, already-fetched pre-filter at the
listing level (it rides the same ETag-cached listing Lever A's candidates come
from, at no extra cost) to decide which issues are even worth checking against
the local memo — but it is too coarse to be the hash itself. Virtually any
forge write (an unrelated comment, a label flip by a different role) bumps
`updated_at`, which would force re-evaluation on noise the role never reads,
defeating the entire purpose of the memo (this is the same "waste is
re-evaluating unchanged inputs" framing the issue opens with — a coarse hash
just reintroduces the waste one layer down).

A role-defined content hash (e.g. a truncated SHA-256 over exactly
title+body for a Curator-style content evaluation, or
title+body+labels+relevant-comment-count for a re-verdict decision) is precise
*and* free: the role already loads that content to make its decision in the
first place, so hashing it is a pure function over data already in hand, no
extra API call. This composes correctly with the two-tier flow: ETag listing
(free) → `updated_at` pre-filter (free, coarse, only decides "worth checking
the memo at all") → local memo lookup keyed by the role's own precise content
hash (free, exact) → skip only on an exact hash match.

### 4. Does this change the label protocol?

**No. Only the traffic around it changes.** The label state machine —
`.github/labels.yml`, the `loom:*` transitions each role performs — is
unaffected. This design adds an optional layer that decides *whether a role
performs its evaluation pass at all*; it never changes what a role writes to
the forge once it decides to act, and it never touches claim semantics. This
matches the standing precedent that label policy is treated as a stable
contract independent of API-cost work (see #2838 — the decision *not* to
clean up labels on close was itself cost-driven, establishing that the label
protocol and its cost profile are already handled as separate concerns).

### The guardrail, restated as a binding invariant

**Claims stay forge-authoritative, unconditionally.** The memo (Lever A) is
advisory only — it gates whether a role *evaluates*, never whether a claim is
valid. `loom:building` / `loom:issue` flips and every other claim-of-record
mutation go through the existing forge label CAS exactly as today, with no
input from the memo or from safehouse. A safehouse outage degrades
Lever B's cross-host propagation speed and nothing else — no host loses
correctness, and dispatch is never blocked (this is the existing safehouse
contract; this design adds no new dependency on it). Any event-driven path
(Lever B today, Lever C if it is ever built) retains the poll as a
correctness floor that works completely on its own — this is not a new
requirement invented here, it is the design constraint #5057 stated and this
decision does not relax it.

## Consequences

### Positive

- One general mechanism (a daemon-owned evaluation memo, keyed by role +
  input-hash) replaces the pattern of N per-role patches (#4987 was the first
  of that N) — a role does not need its own bespoke caching to stop
  re-evaluating unchanged state.
- Zero new hard dependencies: the memo works fully on a single host with no
  safehouse; safehouse enabled only improves cross-host efficiency, never
  correctness.
- No new inbound trust boundary, secret-distribution problem, or dashboard
  architecture change is taken on until Lever A+B are proven insufficient by
  measurement — avoids over-building before the cheaper fix is shown to be
  enough.
- The label protocol, the forge-authoritative claim model, and the existing
  ETag-cached listing infrastructure are all reused unchanged — no migration,
  no flag day.

### Negative

- Cross-host duplicate evaluation is only *reduced* (via optional Lever B),
  not eliminated, when safehouse is disabled or unreachable — a host with no
  safehouse connectivity still pays its own first-evaluation cost even if a
  peer host already paid it.
- Deferring Lever C means the fleet keeps paying poll-interval latency for
  change detection (bounded by the ETag/304 poll cadence) rather than
  near-real-time webhook push, until a follow-up design revisits it.
- Per-role content-hash functions must be defined and kept in sync with what
  each role's decision logic actually reads — a role that starts reading a
  new field without updating its hash function would under-invalidate (treat
  changed state as unchanged). This is a correctness responsibility each role
  owner takes on, not something the memo mechanism enforces automatically.

## Alternatives Considered

- **Store the memo directly in safehouse (a Matrix room state / synced
  table).** Rejected: violates safehouse's own "never a hard dependency"
  contract — an outage would erase every host's evaluation history, forcing
  every role to re-evaluate everything, exactly the fleet-wide cost spike the
  degradation contract exists to prevent. The 2026-08-03 safehouse outage on
  robb-pro is the concrete precedent for why this can't be the store of
  record.
- **Use `updated_at` alone as the memoized key (no content hash).** Rejected
  in decision 3 — too coarse; almost any unrelated write invalidates it,
  reintroducing the waste this design exists to remove.
- **Build Lever C now, alongside A and B.** Rejected in decision 2 — the
  webhook/Worker-as-control-plane change is the highest-cost, highest-risk
  piece and its marginal benefit over A+B is unproven until A+B are deployed
  and measured. Building it first inverts the correct order of operations.
- **Store the memo on the forge itself (a label or a pinned comment).**
  Rejected outright in the source issue — this costs exactly the mutation the
  memo exists to eliminate, and is why the issue scoped the memo to a
  non-forge resident in the first place.

## Suggested phasing (not authorized by this ADR — future issues)

1. Daemon-local evaluation memo (Lever A) as a new daemon subsystem, following
   the `claim_reconciliation.rs` / `quarantine_reconciliation.rs` pattern:
   per-role content-hash functions, local persistent store, LRU + closed-issue
   pruning.
2. Safehouse broadcast of memo writes (Lever B), reusing the existing envelope
   routing and degrade-to-`warn!` posture — additive, opt-in, no behavior
   change when safehouse is disabled.
3. Record before/after GraphQL pool consumption over a fixed window (AC 5 of
   #5057) once 1-2 are deployed on at least one host — this is an
   implementation-phase measurement, not something this design decision can
   satisfy on its own.
4. Revisit Lever C as its own design issue only if step 3's measurement shows
   residual pressure attributable to poll latency rather than redundant
   evaluation.

## References

- Related GitHub Issues: #5057 (this decision), #4500 (Enterprise GraphQL
  uplift measurement), #4736 (duplicate-comment incident), #4987 (curator
  one-off fix), #5017 (live cross-host duplicate dispatch), #4196 (safehouse
  as primary operator interface), #4702 (dashboard/observability epic),
  closed epic #4432
- Related ADRs: ADR-0006 (label-based workflow coordination — the protocol
  this decision leaves unchanged), ADR-0010 (daemon rebuild — the event bus
  and daemon-subsystem pattern this design extends)
- `.loom/docs/safehouse.md` (degradation contract, envelope routing)
- `.loom/docs/observability.md` (current one-directional daemon → Worker
  pipeline, read-only invariant)
- `.loom/docs/daemon-reference.md` § ETag-cached REST listing (`forge_listing`,
  #4428), § cross-host dispatch-collision baseline (#4085)
