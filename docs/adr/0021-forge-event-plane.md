# ADR-0021: Consume Forge Events Through an Operator-Owned Per-Host Cursor Feed, as Prompt Pressure Over an Unchanged Polling Floor

## Status

Accepted

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

## Consequences

**Good.**

- Hand-off latency on the wired loops drops from a poll interval to roughly
  the feed cadence, without touching any existing loop.
- A dead feed is a *latency* regression and nothing else — the property that
  makes this deployable to a fleet incrementally, one host at a time.
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
