# Forge event plane: GitHub webhooks → daemon prompt feed

> ADR-0021, Phase 1 (observe-only). This page is the operator map: what the
> daemon does, how it is turned on, and how to read its status. The design
> argument lives in [`docs/adr/0021-forge-event-plane.md`](../../docs/adr/0021-forge-event-plane.md);
> this repo contains **only the daemon half** — the webhook worker is
> operator infrastructure (an operator-run sibling of the observability
> backend), so nothing here hard-codes an operator URL, app id, or key.

## The pipeline, in one picture

```
GitHub (App-level webhook, the operator's fleet-dispatch App)
  delivery: HMAC-signed JSON, only allowlisted org repos
        ▼
Operator's webhook worker (Cloudflare Worker + Durable Object)
  verify HMAC → classify by repo allowlist → HMAC per-host keys
        │  one queue per host behind a cursor (seq > after, bounded page)
        │  GET /v1/hosts/{host}/events?after={cursor}&limit={page}
        ▼
loom-daemon (per host)  —  `forge_events` module, off by default
  poll the feed → journal every page (durable JSONL)
               → status snapshot (cursor / state / last error)
               → one `forge.event` in-process bus prompt per non-empty page
```

Loom ships the daemon half only. The worker is deployed by the fleet
operator (different App, different HMAC secret, different per-host keys than
any other worker the same operator may run), exactly as
[§4 of the observability reference](observability.md) is: infrastructure
**you** deploy and point your own daemons at. A Loom deployment with no
worker is a deployment where the phase-1 consumer simply reports
`disabled`/`misconfigured` — nothing else changes.

## Invariants (ADR-0014, restated because they bound everything here)

1. **The forge is authoritative.** A feed event is a *prompt to re-query*
   issue/PR state through the existing rate-limited clients — never the
   state itself, never an input to the label/claim/merge pipeline. Nothing
   in this module writes to GitHub.
2. **Polling is the correction floor.** The feed only makes earlier re-checks
   possible; it never stretches, replaces, or disables any existing poll
   cadence. If the feed is dead forever, fleet behavior is exactly the
   pre-webhook behavior.
3. **Phase 1 is observe-only by construction.** There is no consumer of the
   `forge.event` prompt yet; the observable effect is the journal plus the
   status surface. The early-tick consumers (work-finder tick, queue-head
   wake, in-flight PR watch) are Phase 2, each behind its own issue.

## 1. Enable the feed consumer on a daemon

Add the `forgeEvents` block to the host's `.loom/config.json`:

```json
{
  "forgeEvents": {
    "enabled": true,
    "endpoint": "https://<your-worker>.<domain>/",
    "hostId": "<this-host's-id>"
  }
}
```

Precedence is **env > config > default** (same rule every daemon subsystem
follows — `config_resolver.rs`):

| Config key | Env override | Default |
|---|---|---|
| `enabled` | `LOOM_FORGE_EVENTS_ENABLED` | `false` |
| `endpoint` | `LOOM_FORGE_EVENTS_ENDPOINT` | — (required when enabled) |
| `hostId` | `LOOM_FORGE_EVENTS_HOST_ID` | — (required when enabled) |
| `eventKeyFile` | `LOOM_FORGE_EVENTS_KEY_FILE` | `~/.loom/forge-events/key` |
| `pollIntervalSecs` | `LOOM_FORGE_EVENTS_POLL_INTERVAL_SECS` | `10` |
| `pageSize` | `LOOM_FORGE_EVENTS_PAGE_SIZE` | `100` |

**`hostId` is an operator-provisioned identity, not something a daemon may
synthesize**: the worker keys its feeds on it, and a wrong identity is
reported as `host_mismatch`, under which **no cursor from the feed is
trusted** (see §3). Every daemon of this vintage reports a `forge_events`
status unconditionally — including `disabled` — so a watch loop can assert
the state instead of guessing from silence.

**`eventKeyFile` is secrets-only and never in committed config.** The key is
read from the file on **every poll** (a rotation is a file swap, never a
daemon restart), sent only as the `Authorization: Bearer <key>` header on
feed requests, and never logged or placed on the status surface. The default
path needs no config value; a host that must use another path sets
`eventKeyFile` in its gitignored local tier or via the env override. The
worker's host-key table maps `SHA256(key)` to the host id — the raw key never
exists server-side (same discipline as the observability ingest keys, §1 of
[the observability reference](observability.md) applies word for word).

## 2. What the daemon does per poll

One `GET {endpoint}/v1/hosts/{hostId}/events?after={cursor}&limit={page}`,
bounded 10 s timeout, bounded 256 KiB response (a body over the cap is
**refused, never truncated** — a truncated page would desync the cursor):

1. **Journal first** — each event is appended to
   `~/.loom/forge-events/journal.jsonl` (one JSON line per event, capped at
   ~10 MiB, rotated to `journal.1`). A crash between journal and cursor
   re-renders the same page next poll; the journal is an audit tail, never a
   correctness dependency.
2. **Cursor next** — the feed's `cursor` is persisted atomically to
   `~/.loom/forge-events/state.json` and the in-memory cursor advances **only
   after the durable write succeeds** (a lost advance risks a permanent
   event gap). A cursor that goes backwards is refused: the feed must never
   replay silently.
3. **Prompt last** — a non-empty page publishes **one** `forge.event`
   `Event::Generic` on the in-process event bus (per-page, not per-event —
   the bus topic taxonomy stays dedup-disciplined while any future consumer
   may be one-shot per topic). The payload is routing hints only:
   `source: "forge-event-feed"`, `host_id`, `count`, `first_seq`/`last_seq`,
   `types` (sorted, de-duplicated event types). Phase 1 has no subscriber.

An **empty page is a success** — a quiet forge is not an error.

## 3. Reading the status

```
Forge events:  OK — cursor=41218 as host_id=studio-host, 392 event(s) journaled, last success 2026-12-30T02:14:05Z
```

The same facts machine-readable:

```bash
loom-daemon status --json | jq -e '.forge_events.state == "healthy"'
```

| `state` | Meaning |
|---|---|
| `disabled` | Off by config — the deliberately silent case |
| `misconfigured` | `enabled: true` but a spawn-time check failed: `endpoint`/`hostId` did not resolve at any tier, the endpoint is a reserved placeholder, or the event key is unreadable — a config error, never a guess |
| `connecting` | Spawned; first poll not yet completed |
| `failing` | Repeated fetch failures of an uncategorised kind (transport, 5xx, malformed body) — `last_error` carries the class |
| `auth_failed` | Feed answered 401/403 — key mismatch or the worker's key table lacks this host; self-heals on rotation (the key file is re-read every poll) |
| `host_mismatch` | Feed answered 404, or echoed a different host id — the identity is wrong and **its cursors are not trusted** |
| `backoff` | Streak ≥ 3 of *any* class — cadence stretched 10 s → 5 min until the next success (a dead endpoint is not worth ten-second error spam); `last_error` keeps the class |
| `healthy` | Last poll succeeded (an empty page is a success) |

Every error class self-heals on the next success without a restart
(`misconfigured` excepted — that one is a spawn-time answer; fix the named
piece and restart).

## 4. What this is NOT

- Not a polling replacement, and not a trigger into any pipeline.
- Not a second GitHub channel: all state writes still go through the
  existing rate-limited forge clients; the feed only tells the daemon
  *sooner* that it should re-check.
- Not coupled to any operator's deployment: endpoint, host id, and key are
  all per-deployment values; the worker's repo allowlist and its HMAC
  secret live in the operator's infrastructure, not this repo.

## Map of detail

| Doc / source | Covers |
|---|---|
| [`docs/adr/0021-forge-event-plane.md`](../../docs/adr/0021-forge-event-plane.md) | Design decision, phase plan, invariants |
| `loom-daemon/src/forge_events.rs` (+ `forge_events/tests.rs`) | Config resolution, feed client, journal/cursor, status — source of truth |
| `loom-daemon/src/status_render.rs` (`render_forge_events_line`) | Status surface rendering |
| Operator's worker repo (e.g. `infra/loom-events/`) | Deploy + runbook for the Worker half: HMAC, allowlist, host keys, self-test |
| [ADR-0014](../../docs/adr/0014-forge-coordination-decoupling.md) | The decoupling this plane extends (Lever C, previously deferred) |