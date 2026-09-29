# ADR-0021: Consume Forge Events Through an Operator-Owned Per-Host Cursor Feed, as Prompt Pressure Over an Unchanged Polling Floor

## Status

Accepted. **Amended 2026-09-27** — see [§Amendment](#amendment-2026-09-27-the-feed-buys-down-forge-calls-measure-fix-the-cache-then-poll-less)
below, which supersedes invariant 2's "never stretches" clause and the
"Stretch the existing poll cadences" rejection under *Alternatives
considered*. The goal of the feed is now **fewer forge calls**, not only lower
latency.

## Context

[ADR-0014](0014-forge-coordination-decoupling.md) enumerated the levers for
decoupling Loom from the forge and explicitly **deferred "Lever C"** — pushing
GitHub events to daemons instead of polling — for two reasons:

1. **The fleet's hosts are untrusted.** Fanning GitHub webhook deliveries out
   to arbitrary hosts inverts the trust model: it requires each host to be
   reachable, to terminate TLS, and to be a party GitHub is configured to talk
   to. None of those are true of a laptop on a hotel network.
2. **Nothing needed push.** Every loop that mattered already polled, and
   polling was fast enough.

Two things have since changed.

**The operator now runs a perimeter.** Epic #4702 put a Cloudflare Worker +
Durable Object in front of the fleet for telemetry — operator-deployed,
Loom-agnostic, per-host HMAC keys. A second Worker of the same shape is
incremental operator work, not new architecture.

**Some loops are now slow enough to feel.** A daemon that polls only
discovers "my queued issue reached the head of the dispatch queue" or "my
in-flight PR merged" on its own cadence, which for some loops is minutes. That
is latency the fleet pays on every hand-off, all day.

The operator's own repo has since shipped the other half (`infra/loom-events`):
a dependency-free Cloudflare Worker + single Durable Object that receives
GitHub App webhook deliveries, verifies the GitHub HMAC, classifies them
against a generated repo allowlist (`org/repo` membership only — never issue
or PR numbers), and serves a **per-host cursor feed** behind per-host bearer
keys:

```text
GET /v1/hosts/{host}/events?after={cursor}&limit={page}
    -> { host_id, cursor, clamped, events[], has_more }
```

Note what this shape does to objection (1): **daemons still poll, outbound**.
No host is reachable, no host terminates TLS for GitHub, no host is known to
GitHub. The untrusted-host problem is not solved, it is **not created** — the
only party GitHub talks to is the operator's Worker, exactly as it is the only
party the telemetry backend talks to.

## Decision

**Loom ships the daemon half only: a `forge_events` module that consumes the
operator's per-host cursor feed and turns each non-empty page into one
in-process `forge.event` bus prompt. The feed is additive prompt pressure over
an unchanged polling floor. The Worker is operator infrastructure and lives
outside this repository.**
_(Amended 2026-09-27: the feed is no longer purely additive. While `healthy`
it may also gate polls under a bounded staleness cap. See the Amendment.)_

Four invariants bound everything built under this ADR. They are inherited from
ADR-0014, restated here because they are what make the decision safe:

1. **The forge is authoritative.** A feed event is a *prompt to re-query*
   through the existing rate-limited forge clients — never the state itself,
   never an input to a label / claim / merge decision. Nothing in this epic
   writes to GitHub.
2. **Polling is the correctness floor.** The feed only makes earlier
   re-checks possible. It never stretches, replaces, or disables any existing
   poll cadence. A permanently dead feed must be indistinguishable from the
   pre-webhook fleet in every correctness property; only latency may differ.
   _(Amended 2026-09-27: the floor becomes a **bounded maximum staleness**
   rather than a fixed interval — cadences may stretch while, and only while,
   the feed is `healthy`, up to a hard cap. A dead feed is still latency-only,
   bounded by one stretched interval. See the Amendment.)_
3. **Cursors are only trusted from host-matching feeds.** A feed that echoes a
   different `host_id` — or 404s for ours — yields `host_mismatch` status and
   its cursors are never applied.
4. **Keys never cross a trust boundary in the clear of the daemon.** The
   per-host key lives in a file, is re-read every poll (a rotation is a file
   swap, not a restart), is sent only as an `Authorization: Bearer` header,
   and appears in no log line and on no status surface. The Worker stores only
   `SHA256(key) → host_id`.

### Phasing

- **Phase 1 (#8765) — observe-only.** Poll loop, durable JSONL journal +
  atomic cursor, the eight-state status surface, backoff, and the
  `forge.event` publication **with no subscriber**. Zero behaviour change to
  any dispatch/claim/merge path, by construction rather than by care: there is
  nothing on the other end of the topic to change behaviour.
- **Phase 2 (#8766) — early-tick consumers**, one per loop: a work-finder
  tick, a queue-head wake, an in-flight PR re-check. Each individually
  disable-able, each degrading to the existing cadence when the feed is off.

Phase 1 exists separately so the mechanism can run against a live feed, on
real hosts, for as long as it takes to trust it — while being unable to affect
anything. The measurement ("would this have woken us earlier, and how often
was it wrong?") is available before the first behavioural change is written.

### Why a *cursor feed* and not a push socket

A cursor feed is resumable, idempotent, and bounded. A daemon that was asleep,
offline, or restarting resumes at its cursor inside the retention window; one
that is wedged simply stops advancing, which is *visible* rather than silent.
A push socket would require the daemon to be continuously connected — which is
exactly what a fleet of laptops is not — and would make "did I miss an event?"
unanswerable.

### Why the payload is a summary, never the events

`forge.event` carries routing hints only: `source`, `host_id`, `count`,
`first_seq`, `last_seq`, `types`. No repo, no issue or PR number, no title, no
actor. This is invariant 1 made structural: a subscriber that wanted forge
state would have to go ask the forge, because the prompt does not contain any.
It also means a hostile or broken Worker can, at worst, make this daemon
re-query GitHub more often than necessary — it cannot steer a decision,
because no decision reads it.

_(Amended 2026-09-27: with opt-in poll gating (`forgeEvents.pollGating`), the
payload may also carry **invalidation keys**: `repo`, and optionally kind and
number. They are still never state; they only choose what to re-query. With
gating on, the worst case also changes. A hostile or broken Worker that
stays `healthy` while under-reporting can now **delay discovery** by up to
the hard staleness cap, not only cause extra re-queries. It still cannot
steer a decision. See the Amendment.)_

## Consequences

**Good.**

- Hand-off latency on the wired loops drops from a poll interval to roughly
  the feed cadence, without touching any existing loop.
- A dead feed is a *latency* regression and nothing else — the property that
  makes this deployable to a fleet incrementally, one host at a time.
  _(Still true after the 2026-09-27 amendment. A lossy-but-`healthy` feed is
  now a bounded latency cost as well; see the Amendment.)_
- Provisioning is a key mint, not a Worker or Loom change: the Worker treats
  every daemon as "a bearer key with a feed".
- "Why is my cursor not advancing?" has a first-class answer on
  `loom-daemon status` (`disabled / misconfigured / connecting / failing /
  auth_failed / host_mismatch / backoff / healthy`) rather than being inferred
  from the absence of a log line — the #5083 lesson applied up front.

**Costs and risks.**

- A second operator-deployed Worker to run: a different GitHub App, HMAC
  secret and key table than the telemetry backend. Nothing in this repo
  carries an operator URL, App id, or key.
- A per-host credential to provision and rotate. Mitigated by re-reading the
  key file every poll (rotation without a restart) and by refusing reserved
  placeholder endpoints outright (#7815) so a sample config can never leak a
  real key to a documentation domain.
- Duplicate prompts are possible (a restart re-renders from the last durable
  cursor). Harmless by invariant 1: a duplicated prompt is a duplicated
  re-query, and the forge answers the same thing.
- Two sources of "something changed" once Phase 2 lands. Deliberately not
  reconciled: the poll is the floor and the feed is an accelerator, and the
  moment they were reconciled into one path, a dead feed would become a
  correctness problem instead of a latency one.

## Alternatives considered

**Keep polling only (the ADR-0014 status quo).** Rejected because the latency
is now the complaint, and the objection that deferred Lever C (untrusted hosts
receiving webhooks) does not apply to an outbound-poll cursor feed.

**Webhook directly to each daemon.** Rejected — this is the original
ADR-0014 objection unchanged: reachability, TLS termination and GitHub-side
configuration per untrusted host.

**Long-poll or WebSocket from the Worker.** Rejected for Phase 1: it makes
"did I miss an event?" unanswerable without a cursor anyway, and a fleet of
sleeping laptops holds no long-lived connections. The cursor feed can gain a
long-poll option later without changing any invariant above.

**Let the feed carry state and act on it directly** (e.g. apply a label change
from the event body). Rejected: it breaks invariant 1, and it makes the
operator's Worker part of Loom's correctness surface rather than part of its
latency surface.

**Stretch the existing poll cadences once the feed is live.** Rejected, and
this is the one worth stating explicitly because it is the tempting
optimisation: the moment a poll interval depends on the feed being alive, a
dead feed is a correctness bug. The polling floor stays exactly where it is.
_(Superseded 2026-09-27 by the Amendment: the objection holds only for an
**unbounded** stretch gated on nothing. A stretch gated on the existing
`healthy` status, reverting instantly on any other state, and capped at a
fixed maximum keeps a dead feed a bounded latency regression.)_

## References

- Epic: #8764 · Phase 1: #8765 · Phase 2: #8766 · bus topic: #8767
- [ADR-0014: Forge Coordination Decoupling](0014-forge-coordination-decoupling.md) (Lever C, deferred there)
- Operator reference: [`.loom/docs/forge-events.md`](../../.loom/docs/forge-events.md)
- Source: `loom-daemon/src/forge_events.rs` (+ `forge_events/tests.rs`), status
  surface in `loom-daemon/src/types/forge_events.rs` and
  `loom-daemon/src/cli/status_render/forge_events_line.rs`
- Placeholder-endpoint refusal policy: #7815
  (`observability::endpoint_policy::reserved_placeholder_host`)
- Worker half (operator-side, not in this repo): `infra/loom-events/`

## Amendment (2026-09-27): the feed buys down forge calls; measure, fix the cache, then poll less

The cursor-feed architecture above is unchanged: operator-owned Worker,
outbound per-host polling, summary-only prompts, cursors trusted only from
host-matching feeds, keys never logged. What changes is **what the feed is
for**. It was built to cut hand-off latency over an untouched polling
schedule. It is now also a mechanism for **reducing the number of forge calls
the fleet makes**. The preferred way to get there is smarter caching and
event-gated polling, not moving load between rate-limit pools.

### Motivation

The fleet is exhausting GitHub rate limits. It is not a latency problem
(#9243):

- On 2026-09-27 the fleet's App installation hit installation-wide exhaustion
  **six times in one day**. Each time, `work_finder` listings failed for
  *every* registered workspace at once, and cross-repo dispatch priority
  silently stopped meaning anything.
- **Neither pool is reliably idle.** One reading showed GraphQL draining
  (882 used in ~15 min) while REST sat idle. A later reading on the same
  installation showed REST at **0 / 6,550 remaining** with GraphQL nearly
  untouched. Moving a call from GraphQL to REST only moves the pressure
  somewhere else, so it is **not** the strategy.
- **The fleet polls the same data four times.** Four dispatching hosts each
  poll ~58 workspaces, so the same unchanged answers are fetched about four
  times.
- **The existing cache is unmeasured.** `work_finder` already lists issues
  through REST + ETag (#4428), where an unchanged answer is a free `304`. But
  ~6,990 REST calls/hr against ~232 workspace-polls per tick looks like a
  poor `304` hit rate. Candidate causes:
  - **the daemon's in-process ETag map is lost on every restart** (the
    prime suspect, given self-update and supervised restarts across ~58
    workspaces);
  - the daemon does not share ETags with `serve`, the CLI tools or agents,
    although the on-disk store from #5056 / #7275 already keys by resolved
    `owner/repo` and survives restarts;
  - on active repos the ETag really does change most ticks.

  The in-process key is `cwd|url`, but `cwd` is always the registered
  workspace root, so it fragments only if one repo is registered twice. That
  is not a meaningful cause.
- **Some callers skip the cache entirely.**
  - `pipeline_snapshot::GhPipelineSource::fetch` fires 9 GraphQL
    `issue`/`pr list --limit 500` queries per repo root, fanned out over every
    root with no concurrency bound, behind a 20 s TTL. That is ~522 requests
    per cache miss at 58 roots.
  - Agent `gh issue view` / `gh pr view` reads still reach GraphQL on every
    30 s `gh-cached` TTL miss.

The feed already knows **which repos had any event** since a host last
looked. That is exactly the information needed to *not make a call at all*,
and the original invariant 2 forbade using it for that.

### Decision

Reduce forge calls in this order. Each step is useful on its own and can be
turned off on its own. Later steps build on earlier ones but never rely on
them for correctness.

0. **Measure before building.** For each host and each caller
   (`work_finder`, `pipeline_snapshot`, `gh-cached`, role ticks, ...),
   record:
   - calls made, split into `200` vs `304`;
   - which pool each call spent (`core` / `graphql`);
   - `GET /rate_limit` readings, which are free.

   Show the results on `loom-daemon status`. Until this exists we cannot tell
   whether the fix is "make the existing cache earn its 304s" or "make fewer
   calls".

1. **Make unchanged answers free, and make the existing cache hit.** This is
   pool-agnostic: a `304` costs nothing in either pool.
   - Move the daemon's ETag cache onto the existing on-disk store
     (#5056 / #7275), which already keys by resolved `owner/repo` and
     survives restarts. Keep the daemon's own entry-point semantics
     (e.g. the #6171 404 retry).
   - **Any shared or persisted cache is keyed or partitioned by credential
     identity**, so one identity's cached answer is never served to
     another. Since #5401 a daemon may hold several per-owner credentials,
     and agents on a user `gh` credential share the same on-disk store, so
     "one gh credential per process" can no longer be assumed.
   - Route `pipeline_snapshot` through the cached listing path and bound its
     fan-out.
   - Extend the #5056 conditional-request path to `issue view N` /
     `pr view N` label/state reads.

   This step needs no feed, no Worker and no change to any invariant.

2. **Event-gated per-workspace polling (the "smarter" part).** While the feed
   is `healthy`, a workspace whose repo has had **no feed event** since its
   last successful poll is not re-polled. It is re-polled when an event
   arrives, or when its hard maximum staleness expires (below), whichever
   comes first.
   - This removes calls outright, including the 4× cross-host duplication for
     every idle repo. An idle repo is the common case across 58 workspaces.
   - Read caches (`gh-cached`, pipeline snapshots) follow the same rule: a
     cached entry for an event-free repo is held rather than expired on a
     blind TTL.
   - To make this possible, `forge.event` may additionally carry
     **invalidation keys**: `repo`, and optionally object kind and number.
     They are used only to decide *what to re-query*. This narrowly relaxes
     the "summary only" payload rule and leaves invariant 1 untouched,
     because a key can only cause a re-query, never supply state:
     - a feed that over-reports costs extra calls, bounded by the rate-limit
       breaker;
     - a feed that under-reports delays a re-poll by at most the hard cap.

   When the feed is in any state other than `healthy`, every workspace
   returns to its base cadence and base TTLs on the next tick, with no grace
   period.

   Short-lived readers such as `gh-cached` in agent processes cannot see the
   daemon's in-memory feed status. They may hold entries past their base TTL
   only on a daemon-published, freshness-stamped health signal. If that
   signal is absent or stale, they behave as though the feed were not
   `healthy`. How the signal is published is decided in the implementing
   issue.

3. **Cross-host dedup** (one host polls a repo and publishes the result for
   the others) remains the strongest lever for *active* repos, where step 2
   cannot help. It is out of scope for this ADR because it is a fleet
   coordination decision, not a feed decision. Step 2 already removes the
   duplication for idle repos without any cross-host protocol.

**Reads that gate a decision never come from a held cache.** A read that
gates a claim, a label transition or a merge uses either a conditional
request (free when unchanged, always fresh) or a plain uncached read.
- The cache and the gating exist to make *discovery* cheap ("is there
  anything for me to do?"). They never get a vote in *acting*.
- This is invariant 1 applied to the cache exactly as it already applies to
  the feed.
- Our own writes invalidate the affected entries immediately (write-through),
  as `gh-cached` already does.

### Revised invariant 2

> **Polling is the correctness floor, expressed as a bounded maximum
> staleness.** The feed may make re-checks earlier. Only while its status is
> `healthy`, it may also let event-free workspaces skip polls and let caches
> hold longer, up to fixed hard caps (default: 10× the base cadence, never
> more than 15 minutes). Any other status restores the pre-feed cadence and
> TTLs on the next tick or read.
>
> A permanently dead feed must still be indistinguishable from the
> pre-webhook fleet in every correctness property. A *silently lossy* healthy
> feed (e.g. a dropped GitHub delivery) may add at most one hard-cap interval
> of latency.

The original argument against stretching was "the moment a poll interval
depends on the feed being alive, a dead feed is a correctness bug". That
assumed an unbounded stretch. No Loom decision depends on a re-check
happening within a particular time, only on it happening eventually, so a
bounded cap keeps a feed failure a latency cost only.

The remaining risk is a feed that reports `healthy` while dropping events.
The hard caps exist to bound exactly that, and it must be measured before the
caps are raised.

### Measurement and rollout

- **Steps 0 and 1 ship unconditionally.** Step 0 comes first, so step 1 can
  show what it bought.
- **Step 2 is opt-in and default off.** It is enabled per host behind config
  (`forgeEvents.pollGating`) until the **lossy-feed rate** has been observed
  on real hosts: how often a hard-cap re-poll found a change the feed never
  reported. This is the same observe-before-act discipline as Phase 1. If
  that rate is non-zero, the caps do not go up.
- **Everything is visible on `loom-daemon status`.** Per-workspace
  gated/ungated state, the effective cap, and the time since the last poll
  are shown next to the existing feed-status line, so "why didn't my loop see
  that?" can be answered from the status surface.

### Consequences of the amendment

**Good.**
- The fleet stops paying for unchanged answers. First by making the existing
  conditional requests actually hit, then by not asking about repos where
  nothing happened.
- Both steps reduce calls rather than moving them between pools, so they help
  whichever pool is hot that hour.
- Operators without the Worker get steps 0–1 in full.

**Costs.**
- The `forge.event` payload grows invalidation keys. A feed compromise
  therefore reveals which repos (and optionally which issue numbers) changed,
  though not their content, and the Worker must emit them.
- There is one more tunable per loop: the staleness cap.
- A lossy-but-healthy feed now costs bounded latency where it previously cost
  nothing. The measurement gate above exists for that.

**Unchanged.**
- Webhooks still never reach a host.
- The Worker stays operator infrastructure outside this repo.
- Feed events still never become state.
- The cursor, key-handling and host-mismatch invariants (1, 3, 4) stand as
  written.

References for this amendment: #9243 (measurements, candidate directions),
#4428 (REST + ETag work-finder listing), #5056 (`gh-cached` conditional
listings), #4429 (rate-limit breaker).
