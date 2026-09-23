# ADR-0021: The Forge Event Plane — GitHub App Webhook → Operator-Owned Fan-Out Worker → `loom-daemon` Reactions (Lever C, Shipped Conservatively)

## Status

Accepted (operator decision 2026-09-23: "use GitHub webhooks instead of pull where it makes the interactions with GitHub cleaner; the webhook server is hosted on the operator's Cloudflare account"). Supersedes **only** ADR-0014's decision 2 (the deferral of Lever C); ADR-0014's decisions 1, 3 and 4, its claim-authority model, and its measurement-first discipline carry forward unchanged.

## Context

ADR-0014 established that the forge's GraphQL/REST quota is spent on **repeat
evaluation of unchanged state** — N hosts × M repos × a full-repo LIST every
`listEverySecs` — and recorded three levers to reduce the work quota actually
serves: **Lever A** the daemon-local evaluation memo (deferred, not yet
shipped), **Lever B** safehouse as a best-effort broadcast accelerator
(deferred, no issue filed), and **Lever C** "GitHub webhooks → existing
Cloudflare Worker → fan-out to daemons" — explicitly parked for *after* A/B
were measured and shown insufficient (decision 2), because the one question it
forced open had no cheap answer: does a webhook-fed Worker become a participant
in the fleet's control plane?

Three things have since changed.

1. **The operator asked for it directly.** On 2026-09-23 the operator stated
   the goal: GitHub interactions should be push-driven ("webhooks instead of
   pull") where push is cleaner, with the webhook server hosted on the
   operator's Cloudflare account (already running: `bot-issues.2amlogic.com`,
   `dashboard.2amlogic.com`). A parked lever with an operator request is no
   longer parked.
2. **The instance the question was costing has now been paid for and proven.**
   The `bot-issues` Worker (`infra/bot-issues/` in the 2am repo) has run in
   production since 2026-09-17: signed GitHub App webhook intake, HMAC
   verification, Durable Object queue with dedup on `X-GitHub-Delivery`,
   cursor-fed per-broker event feeds, bearer key auth per broker, a 15-minute
   redelivery sweep against GitHub's no-retry-on-5xx behavior, per-event
   drop/receipt/latency logging. This is precisely the Lever C transport,
   already debugged against GitHub's real webhook failure modes.
3. **The daemon half has a clean, cheap Rust seam.** The daemon already
   carries an outbound-only `reqwest` client for the observability exporter
   (#4705), a `Generic` event-bus topic class for non-taxonomy events, and a
   repeated off-by-default config pattern (`env > config > default`, disabled
   ⇒ zero syscalls). An event *client* is a small module; it needs no new
   dependency and no new daemon surface beyond a status section.

What Lever C does that ADR-0014's two levers cannot: it removes the **blind
spot between polls**. The memo (A) makes repeat *evaluation* free once the
state is known; the safehouse (B) makes *invalidation* fast among daemons.
Neither knows that state changed in the first place — only the forge does, and
today the forge tells no one. A webhook-fed event plane turns "I may be stale"
into "I am stale now," which is the actual gap in the ADR-0014 model: its
corollary ("repeat LISTs are just a cache for state that could instead be
event-driven") describes a design where *something* pushes. Loom has never
had that something. This ADR builds it.

### The question ADR-0014 could not answer, answered

> (d) does a webhook-fed W become a participant in the fleet's control plane?

**Yes, in a bounded form that the ADR-0014 invariants already permit, and no
in the form they forbid.** The Worker is a *member of the event plane*: it is
trusted to receive signed forge events and hand them to daemons. It is **not**
a member of the *control plane*: it never claims, labels, closes, merges, or
decides; daemons never act on its data except as a *prompt to re-verify*
against the forge. A webhook event is a trigger, never a claim. Under those
two sentences, every ADR-0014 invariant survives contact with the new
component:

- Claims remain forge-authoritative (forge claims still run; nothing here
  writes a label).
- The polling floor remains: every existing loop keeps running at its existing
  cadence. Events only make some of those loops run *sooner*.
- A hostile, lost, or duplicated event can only cause a daemon to *check*
  sooner. It cannot make a daemon claim, or be wrong, or be late past its poll
  deadline. Worst case of total event-plane failure: the fleet behaves
  identically to today.

This is the same shape the ADR already accepted for Lever B ("safehouse
suggestion is a *cache invalidation hint*, never the memo itself"), with the
invalidation hint arriving from the forge itself instead of from a peer.

### What the fleet today actually re-evaluates every poll

Measured against the codebase (the loops a webhook would shortcut):

| Loop (module) | Cadence | Cost per pass | What an event removes |
|---|---|---|---|
| Work-finder LIST, per workspace (`work_finder.rs`) | `listEverySecs` (default 30 s) | one GraphQL listIssuesOrPullRequests per repo | the ~29/30 passes between a real change and its next poll |
| Merge-reconciliation polls (`epic_supervisor.rs`) | `checkIntervalSecs` (default 60 s) | `gh api` + REST list per active epic workspace | the wait up to one interval after a PR closes/merges |
| Post-merge claim reconciliation (`claim_reconciliation.rs`) | `intervalSec` (default 15 min) | full `gh api` label audit + patch/delete per workspace | the up-to-15-min blind spot on foreign label edits |
| Watch queries (`watch_registry.rs`) | `intervalSecs` default 60 s | one GraphQL per watch query | the up-to-60-s detection lag for watched changes |
| Forge listing / cached list (`forge_listing.rs`, `forge_cached_list.rs`) | on-demand + refresh | REST/GraphQL list with ETag where available | re-verification after pushes to watched branches/tag pushes |

The event plane does **not** touch the role-prompt's own per-sweep GraphQL
(spawn path, per ADR-0014 decision 3 that cost belongs to the LLM and the
memo's job) or the curator's enrichment refreshes. The v1 reaction set is the
five loops above: *prompt a re-check*, nothing else.

## Decision

### D1 — Topology: one operator-owned fan-out Worker per fleet, not one per daemon

```
                    (HMAC-SHA256, app webhook)
GitHub ─── signed webhooks (push / issues / issue_comment /
              pull_request / pull_request_review /
              pull_request_review_comment / check_run /
              workflow_run / merge_group) ──────┐
                                                ▼
                     Cloudflare Worker (operator-owned; per-fleet instance,
                     e.g. infra/loom-events/ on 2am)
                       · verifies X-Hub-Signature-256 (rejects before any I/O)
                       · classifies + allowlists (owner + repo, from the fleet
                         manifest)   · dedups on X-GitHub-Delivery (DO SQLite)
                       · assigns a monotonic seq per fleet (DO SQLite)
                       · 15-min redelivery sweep vs /app/hook/deliveries
                                                │
                     one durable, replayable stream (GET, cursor-fed)
                                                │
              ┌───────────────┬────────────────┼────────────────┬───────────────┐
              ▼               ▼                ▼                ▼               ▼
         daemon A       daemon B           daemon C         daemon D        (dashboard
         fetch loop     …                  …                …               / future
         every N s      (N ≈ 10 s)                                 consumers)
              │
              ├─ durable cursor (survives restarts)
              ├─ local journal (~/.loom/forge-events/journal.jsonl, rotated)
              ├─ Event::Generic("forge.event") on the in-host bus
              └─ (Phase 2) early-tick prompts: work-finder / merge-reconcile /
                 claim-reconcile / watch queries / listing refresh
```

- **One Worker service per fleet** (four hosts ⇒ one Worker), because the
  event stream is fleet-scoped (a `push` to `loom` is relevant to every host
  that lists `loom`). Per-host *consumption* (auth key, cursor, cursor-bearing
  feed entries, status) is a property of the fetch, not of the service. This
  reuses the `bot-issues` architecture's cost model: idle monitoring costs one
  fetch per host per interval — a `GET` with cursor that returns 200/`[]` —
  not one round trip per event.
- **The App's webhooks are the source.** Loom fleets using the GitHub App
  identity (#4430) get webhooks for free: an App's configured webhooks fire
  for every repo in its installation, signed with one secret. Fleets
  authenticating via personal tokens/PATs can stand up an equivalent Web
  *App* (or org App) the same way; the Worker is source-agnostic as long as
  the signature scheme is the standard `X-Hub-Signature-256` HMAC. Loom itself
  ships **the daemon half only** (D3); the Worker is operator infrastructure
  (see "What Loom ships" below) — Loom remains deploy-your-own and never
  hard-codes an operator hostname.
- **Durable, replayable, per-host cursors.** The stream is a cursor feed
  (monotonic `seq`, `after=` query, ack-free cursor returned in the response),
  not a push channel. Rationale: daemons sit behind NAT/tailnet with no public
  inbound; outbound HTTPS is the only transport every host shares (same
  constraint that shaped the observability exporter, #4705); a cursor makes a
  slow/restarted daemon *replay-safe by construction* (fetch from persisted
  cursor, dedup is idempotent on the daemon side too, via seq); and it keeps
  the Worker's queue semantics on a proven substrate (`bot-issues`' Durable
  Object). Push (WebSocket fan-out, or direct Worker→daemon POSTs) is
  rejected for this phase — see Alternatives.

### D2 — Transport contract (Worker API)

Versioned, additive, HTTP + JSON only. The daemon half of Loom codes to this
contract; the reference Worker implementation (2am `infra/loom-events/`)
implements it.

- `POST /webhook/github` — GitHub only. Unauthenticated by bearer; the HMAC
  signature **is** the authentication (verified in constant time before any
  parsing or storage). Rejections: non-POST 405; oversized body 413; bad or
  missing `X-Hub-Signature-256` 401; delivery targeting another App
  (`X-GitHub-Hook-Installation-Target-ID` mismatch) 403; non-2xx here is what
  the redelivery sweep later repairs.
- `GET /v1/healthz` — liveness; unauthenticated (no state disclosed).
- `GET /v1/hosts/{host_id}/events?after={seq}&limit={n}` — the feed.
  Auth: `Authorization: Bearer <event-key>`. The key maps to exactly one
  `host_id` (SHA-256 of the key ⇒ host, like `bot-issues` broker keys); a
  correct key presented against a *different* host's path is a 404
  (attribution without revealing existence); an unknown key is a 401.
  Response: `{ "host_id", "after", "cursor": <last-seq-in-page or after>,
  "events": [ { "seq", "received_at", "event", "action", "repo",
  "owner", "actor", "number", "url", "detail" } ], "has_more" }`. `limit`
  bounded server-side (default 100).
- `POST /v1/hosts/{host_id}/test-event` — operator self-test; signs a
  synthetic event through the **real** pipeline (verify → classify → DO →
  queue → feed) with a daily quota, so "the pipeline works" is an answer from
  the pipeline itself, not a shortcut around it.
- `GET /v1/status` — operator view: receipt counts, filter-reason histogram
  (why a delivery was not queued), per-host consumption (acked-through,
  unread), recent deliveries, redelivery-sweep log. Auth: any valid host key
  (fleet-scoped status, no secrets).
- Retention: deliveries 7 d (receipt + dedup answerability), events 30 d
  (bounded replay horizon). A daemon's cursor older than retention is
  clamped; the daemon treats the clamp as "replay unavailable — resume from
  now and rely on the polling floor" (see D5).

Event vocabulary v1 (superset of what the daemon reacts to in Phase 2;
unimplemented reactions simply have no handler — the event still lands in the
journal): `push` (branch refs only; tag pushes are a `detail` fact),
`issues` (opened/closed/reopened), `issue_comment` (created), `pull_request`
(opened/closed/reopened/ready_for_review, with `merged`, `base`, `head`),
`pull_request_review` (submitted), `pull_request_review_comment` (created),
`check_run` (completed), `workflow_run` (completed), `merge_group` (merged),
`release` (published). Classification is a pure function of `(event,
payload)` — no I/O, testable in isolation, and the *single* home of every
drop rule (owner mismatch, repo not allowlisted, action ignored) so "was it
received, and why wasn't it queued" always has one answer.

### D3 — The daemon half: `forge_events` module in `loom-daemon` (Rust, not shell)

A new `loom-daemon/src/forge_events.rs` module (opt-in subsystem in the
established `idle_exit`/`observability` shape: configured, spawned, status,
off ≡ inert):

- **Config** — `forgeEvents` block in `.loom/config.json` (and
  `.loom-local/local.json`), `env > config > default`:
  `enabled` (default **false**), `endpoint` (https base URL of the Worker),
  `eventKeyFile` (default `$HOME/.loom/forge-events/key` — key in a file,
  never inline, mirroring `observability.ingestKeyFile`), `pollIntervalSecs`
  (default 10), `pageSize` (default 100). `LOOM_FORGE_EVENTS_*` env
  overrides follow the existing convention. `defaults/config.json` ships the
  block with `enabled: false`, so a fresh install is byte-identical in
  behavior to today.
- **Degradation ladder** (the whole subsystem fails *toward* today's
  behavior, one rung at a time):
  1. `enabled=false` (or block absent) ⇒ `spawn_task` returns `None`: no
     client, no fetches, no files, zero syscalls. The daemon is
     byte-identical to a non-event daemon.
  2. Enabled but under-configured (no endpoint, or endpoint is a reserved
     placeholder host, or no readable key file) ⇒ warn once at startup, log
     misconfigured state, no network. (Placeholder refusal reuses the same
     guard the observability exporter got in #7815, so a copy-pasted sample
     endpoint cannot become a data-exfil or key-leak channel.)
  3. Configured, endpoint down ✱ ⇒ the *poll* cadence stretches from 10 s
     to a 5-minute ceiling after `BACKOFF_AFTER_ERRORS` (3) consecutive
     failures of any class, and holds at the ceiling while the streak
     continues (a stretch with a floor, not an increment — no halfway
     rungs); `consecutive_errors` counted in status; `last_error` is the
     sanitized error (path/host/status, never the key).
     Polling of the forge by the loops continues; nothing notices a
     difference.
  4. Configured, 401/403 (key mismatch revoked/rotated) ⇒ same backoff, plus
     a `warn!` surfacing the auth failure specifically; still no polling
     impact. A 404 on `/events` with a valid-shape key is reported as
     host-not-provisioned and backs off identically.
  5. Configured, working, but cursor older than retention ⇒ clamp to `after=0`
     response, log once per occurrence-class, resume from now.
- **What each fetch does** (observe-only in Phase 1): read the page
  (`?after=cursor`), append every event to the local journal
  (`~/.loom/forge-events/journal.jsonl` under the subsystem's one state
  directory, capped at ~10 MiB with a single rotation to `journal.1` — it
  is a diagnostic aid, not state of record), persist the new cursor
  atomically (`~/.loom/forge-events/state.json`: exactly `cursor` +
  `updated_at`; the live counters are on the in-process status surface and
  reset to zero on restart by construction — the journal is what survives),
  publish one `event_bus::Event::Generic { topic: "forge.event", payload }`
  per non-empty page (payload: `{source, host_id, count, first_seq,
  last_seq, types}` — `types` is the sorted, de-duplicated event-type list
  Phase 2 consumers will route on. The payload is a *summary*, never a copy
  of events, and it never feeds a decision; the journal is the copy). `Generic` is used deliberately: the frozen taxonomy
  (ADR-0009/#1847; `docs/event-bus`) is for daemon-originated state, and
  forge-pushed events are neither; introducing a `ForgeEvent` variant would
  break the topic freeze for no current consumer.
- **Reactions (Phase 2 of this ADR, deliberately not in Phase 1):** per-event
  *early-tick prompts* to the five loops in the table above, implemented as
  per-loop wake tokens (a `tokio::sync::Notify`-style seam per loop, firing
  the loop's existing tick code early — the tick then re-verifies against the
  forge exactly as it would on its timer; the event data is *not* fed to the
  loop's decision). The invariant stated in ADR-0014 and restated here: **a
  wake is a prompt, not a truth.** Every code path an event can trigger is a
  path the timer already triggers; the event can add verification cost (a
  tick that found no change ≈ one ETag-304 or one unchanged list), never
  correctness.
- **Status/health surface:** a `forge_events` section on the daemon status
  (human line and `--json` `forge_events` field): `state` (`disabled |
  misconfigured | connecting | failing | auth_failed | host_mismatch |
  backoff | healthy`), `host_id`, `endpoint`, `cursor` (the durable one),
  `last_success_at`, `last_error` (sanitized: path/host/status, never the
  key), `consecutive_errors`, `events_journaled` (since process start),
  `poll_interval_secs`. Eight states: the error *classes* (`failing`,
  `auth_failed`, `host_mismatch`) answer "why is my cursor not advancing",
  while `backoff` is the cadence-stretch promotion of any class (the class
  stays readable in `last_error`) — and the field is always populated from a
  daemon of this vintage: `disabled` is a real answer, and `null` in JSON
  means "daemon older than ADR-0021".

### D4 — Security model

- **HMAC at the door.** The Worker refuses to *read* more than the signature
  needs (constant-time `crypto.subtle` verify; 401 before any storage or
  parsing beyond the raw body). The secret is a Worker secret rotated through
  `configure-webhook.sh` (Worker-first ordering: the Worker learns the new
  secret before GitHub signs with it — no 401 window; the sweep mops any
  slip).
- **The allowlist is policy, not trust.** The Worker re-checks `owner` and
  the repo allowlist *even though* the App is installed only on fleet repos
  (defense in depth against an operator mis-install; the `bot-issues`
  pattern). Repo allowlist is generated from the fleet manifest (`repos.yml`,
  owner-qualified slugs) by a script — one derived file, never hand-edited.
- **Per-host keys.** One key per daemon host (`loom-events_<host_id>_<32-bytes>`),
  only its SHA-256 in the Worker (`HOST_KEYS_JSON`, like bot-issues
  `BOT_KEYS_JSON`). The key is a file on the host (D3), so it never appears
  in repo, config, or logs. Revocation = drop the SHA from the JSON and
  re-`wrangler secret put`; the daemon's next 401 surfaces it in its status.
  Credential model matches the precedent the fleet already trusts
  (`credential.md`: keys minted and delivered outside the repo; the Worker
  can't read secrets back — revocation is the read path for rotation).
- **The stream is a read surface only.** Feeds return *normalized* events
  (actor, number, title-clipped, urls, trimmed detail per event type), never
  raw webhook bodies, so issue/PR *bodies* — the one data class in a webhook
  payload that can carry secrets-adjacent content (pasted tokens are the
  documented incident class, #4656-class) — are clipped to excerpt length
  (600 chars) and, in v1, only present where a human would need them to triage
  (issue title + excerpt). The journal persists what the daemon received,
  not more.
- **No inbound to daemons.** The daemon never opens a listening socket for
  this; the tailnet/NAT posture is unchanged. This is the property that makes
  the Worker+cursor design the only one in Alternatives that needed no
  change to the host security posture at all.
- **Host identity.** The `host_id` in the path is provisioned (mint script
  prints it; the daemon's config `hostId` must match the key's), and the
  daemon cross-checks the echoed `host_id` in the feed response against its
  configured value — mismatch ⇒ `host_mismatch` status + backoff, never
  continued fetching (same shape as the observability `host_id` echo check,
  #4830).

### D5 — Invariants carried from ADR-0014 (unchanged, re-verified)

1. **Claims are forge-authoritative.** No code in this design writes a label,
   closes an issue, or claims work based on an event. The only writer is the
   forge, as today.
2. **Polling is the correctness floor.** Every cadence table row stays
   running. The event plane's total failure mode is "the fleet polls exactly
   as it does today." This is asserted as a testable property in the Phase 2
   acceptance criteria (kill the Worker ⇒ no behavioral delta beyond latency).
3. **Events are advisory.** Reactions are early *ticks* reusing existing
   decision code with existing forge-verified inputs. An event's content is
   never compared, hashed into a memo, or trusted past the point of
   deciding "check now."
4. **The memo (Lever A) and the safehouse (Lever B) are orthogonal.** When
   A lands, the event plane becomes its invalidation signal *into* the memo
   (an event for repo R ⇒ memo entries for R re-validated), not a
   replacement for it. When B lands, the two are redundant for host-to-host
   invalidation *within* a shared safehouse, but the event plane stays the
   only component that knows about forge changes the daemons haven't seen —
   B invalidates on *peer* knowledge, C invalidates on *source* truth.

### D6 — Measurement discipline (this lever is accepted the same way A/B would have been)

Phase 2 ships with an acceptance criterion, measured with the
`.loom/docs/observability.md` backend, that fails the lever if it didn't
deliver:

- **Detection latency** (forge change ⇒ first daemon reaction) p50/p99:
  pre-rollout (poll-bound: ~up to one poll interval) vs post-rollout
  (event-bound: ~one poll interval of the *fetch* loop, i.e. ≤10 s +
  delivery). The bound is a known number on both sides — no "faster" vibes.
- **Polling traffic delta:** per-host GraphQL+REST core call rate for the
  loops in the table, post-rollout. Expectation: *little or no reduction* in
  **calls** for v1 (early ticks add ticks; they don't remove the floor) —
  the win is detection latency and (in Phase 3, with Lever A) the *cost per
  poll* via memo invalidation. The ACR therefore measures both, and the
  decision gate for Phase 3 (memo integration) is the per-poll cost curve,
  not the call count.
- **False-wake rate:** wakes that re-verified and found nothing (expected
  high in bursty windows, bounded by event volume; a `warn!` floor if it
  grows past where it was estimated, because that is a sign the allowlist is
  too wide, not a reason to drop events).

## Consequences

- **Positive.** (1) The four-eyes-between-polls blind spot closes for the
  five listed loops: a merged PR is seen in ≤ ~10 s + delivery instead of ≤
  60–900 s. (2) The polling budget becomes *honest* — it is no longer doing
  reaction work, only coverage work, which is what ADR-0014 always said it
  should be once events existed. (3) A proven transport (the bot-issues
  Worker's failure modes are already known and handled: 5xx no-retry, replay,
  secret rotation, queue durability) — Lever C's engineering risk drops from
  "build delivery infrastructure" to "write one Rust module and one Worker
  that reuses an existing design." (4) Operator-hosted: the fleet's forge
  interaction path stops requiring each host to reach GitHub with its own
  identity *for reaction* (polling identities remain, unchanged). (5) The
  event bus gains a `forge.event` topic class → any future subsystem (e.g. a
  terminal-side "your epic merged" notification) subscribes for free.
- **Negative / accepted.** (1) One more always-on surface per fleet (Worker +
  DO) whose failure needs monitoring — mitigated by the `status` section and
  the sweep's own log line. (2) Dashboard/broker-style key material on every
  fleet host — mitigated by file-based keys + SHA-only server side. (3)
  Early-tick wakes add forge traffic in bursty windows (a PR with 10 review
  comments ⇒ up to 10 early LISTs) — the price of responsiveness, bounded by
  the allowlist and by the fact that each unmodified re-check is typically an
  ETag-304; measured in D6. (4) The `forgeEvents` block and
  `LOOM_FORGE_EVENTS_*` env surface are new config surface — additive,
  off-by-default, and the daemon is byte-identical when off (the test must
  prove it, not assert it).
- **What does NOT change:** label protocol (ADR-0006), claim authority
  (ADR-0014), the daemon's zero-inbound posture, the event-bus taxonomy,
  `GH_TOKEN`/App-identity polling paths, and every loop's cadence table.

## What Loom ships vs what the fleet operator ships

- **Loom (this repo, public):** ADR-0021; `forge_events.rs` in `loom-daemon`
  (client, journal, cursor, status, reactions); `forgeEvents` in
  `defaults/config.json` (disabled); `.loom/docs/` section; status/health
  output; tests (config precedence, degradation ladder with a stubbed
  endpoint, journal rotation, replay idempotency, wake-token safety,
  "off ≡ inert" property test).
- **Fleet operator (e.g. 2am):** the Worker (reference implementation in
  `infra/loom-events/`, modeled on `infra/bot-issues/`), its secrets and
  domains, per-host key minting, fleet-manifest allowlist generation, App
  webhook configuration (`configure-webhook.sh`), and the fleet-side
  rollout order (canary host ⇒ fleet). The Worker is *operator*
  infrastructure by construction: different fleets point daemons at
  different endpoints; Loom codes to the contract (D2), never to a
  deployment.

## Alternatives Considered

- **Direct GitHub webhook → each daemon (no Worker).** Rejected: daemons have
  no public inbound (NAT/tailnet); would need per-daemon tunnel/relay state
  and per-host secrets on the forge side (N webhook configs per fleet — the
  exact scaling the ADR-0014 table shows is already painful). The Worker
  collapses N inbound paths into one and gives dedup/replay for free.
- **GitHub Events API / polling the event stream.**Rejected: that is
  polling again, with a worse envelope (no installation scoping, coarser
  granularity, per-request budget) — the ADR-0014 framing applies to it
  verbatim.
- **Worker → daemon push (POST to daemon, or WebSocket).** Rejected for v1:
  requires per-daemon inbound addressing (tailnet DNS per host — a security-
  surface change on every host for a latency win the cursor already delivers
  at ≤10 s) and a second auth surface on the daemon. The cursor feed is the
  same information, one direction, zero new host posture. Push may be revisited
  if Phase 2 measurement shows 10 s fetch-latency is the binding constraint
  (it won't be — delivery latency dominates).
- **Daemon-to-daemon gossip (Lever B) as the mechanism.** Not an alternative,
  a complement (D5.4): B cannot know about changes no daemon has seen yet.
- **One Worker per daemon host.** Rejected: the stream is fleet-scoped;
  per-host Workers would N× the webhook surface (N App webhook configs) for
  no isolation benefit the keys don't already provide.
- **Reusing `bot-issues` Worker for bot-webhook fan-out.** Rejected: different
  App (bot-issues' App ≠ `loom-fleet-dispatch`), different secret domain
  (bot payloads are external-human content; fleet events are
  fleet-internal), different allowlist, different retention; coupling two
  operator services in one Worker makes one incident the other's incident.
  Sharing the *pattern* (this is a sibling, not a fork) is the reuse the
  house rules ask for.
- **Letting events drive claims (the original Lever C question, unbounded
  form).** Rejected — this is the ADR-0014 decision-2 question, and D1/D5
  are the answer: events may prompt, never decide. A webhook that could
  trigger a claim would be a second forge and every conflict-resolution
  cost of #6522-class incidents would reappear on a non-authoritative
  surface.

## References

- ADR-0014 (decisions 2 and 4; corollary on LISTs-as-cache; the
  claim-authority model this ADR inherits)
- ADR-0008 (the event bus; `Generic` topic class rationale), ADR-0009
  (taxonomy freeze)
- #4705 / epic #4702 (observability: the outbound-only `reqwest` client,
  host-id echo check #4830, file-held keys, FLAGS-OFF posture this module
  mirrors)
- #7815 (reserved-placeholder host refusal, reused)
- #4430 (GitHub App identity — the App whose webhooks feed this plane)
- 2am `infra/bot-issues/` (production reference implementation of the
  transport: HMAC verify, DO queue, dedup, cursor feed, keys, sweep)
- `.loom/docs/credential-storage.md` (keys: mint outside repo, SHA-only
  server, rotation), `.loom/docs/observability.md` (status surface and
  endpoint policy), `watch-design-notes.md` (watch queries the event plane
  accelerates)