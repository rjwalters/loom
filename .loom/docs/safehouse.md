# Safehouse fleet-comms narration (phase 1, #3997)

Loom coordinates through forge labels — that is unchanged and remains the sole
source of truth. **Safehouse** ([`rjwalters/safehouse`](https://github.com/rjwalters/safehouse))
adds an optional, additive **narration** side-channel: an end-to-end-encrypted
Matrix room a human watches in Element to follow a multi-host agent fleet in
real time, instead of polling `gh` or tailing daemon logs.

Phase 1 is **daemon-side, emit-only**. The `loom-daemon` subscribes its existing
in-process event bus and narrates sweep-lifecycle transitions into the room. It
adds no new event topics and no new publish call sites.

> **Out of scope for phase 1** (tracked separately): per-worker personas
> (`loom_builder_42`) and `SAFEHOUSE_PERSONA` forwarding to workers → **#3999**;
> inbound human steering (reading `@`-mentions back to agents) → follow-up;
> cloud-host provisioning of `safehoused` → **#3998**; carrying the judge verdict
> value in an event payload (needs a frozen-taxonomy amendment) → follow-up.

## The degradation contract (read this first)

Safehouse is a **best-effort side-channel with no hard dependency** — it mirrors
the claude-monitor optional-integration pattern. **Loom never blocks a sweep on
safehouse.** Concretely:

- `safehouse.enabled` false/absent ⇒ **byte-for-byte no-op**: the daemon does
  not subscribe to the bus and makes zero socket syscalls.
- Enabled but the socket is missing/refused, the persona is rejected, or the
  peer restarts mid-run ⇒ every failure degrades to a single `warn!` (one per
  outage, not per event) and the sweep proceeds unaffected. The sink reconnects
  lazily with capped exponential backoff; dropped narration is never retried
  into a hot loop and never fails a sweep.

## Configuration

An optional `safehouse` block in `.loom/config.json` (shipped in
`defaults/config.json`), resolved with precedence **env > config >
default(disabled)**:

```jsonc
"safehouse": {
  "enabled": false,       // default off — additive, opt-in
  "socket": null,         // default: $SAFEHOUSED_SOCKET
  "room": null,           // omit only if safehoused joined exactly one room
  "persona": "loom_daemon"
}
```

Env overrides (each wins over config for that key):

| Env var | Overrides |
|---|---|
| `LOOM_SAFEHOUSE_ENABLED` | `enabled` (`1`/`true`/`yes`/`on` ⇒ on; `0`/`false`/`no`/`off`/`""` ⇒ off) |
| `LOOM_SAFEHOUSE_SOCKET` | `socket` |
| `LOOM_SAFEHOUSE_ROOM` | `room` |
| `LOOM_SAFEHOUSE_PERSONA` | `persona` |

**Socket resolution**: configured `socket` → `$LOOM_SAFEHOUSE_SOCKET` →
`$SAFEHOUSED_SOCKET`. If none resolves, narration logs one `warn!` and stays off.

New template keys reach existing consumer configs via the installer deep-merge
(template is the base, existing values win) — no migration needed. The tier
ownership of the block is noted in
[`docs/design/config-resolution-tiers.md`](../../docs/design/config-resolution-tiers.md).

## Operator setup: provisioning the persona (requires a safehoused restart)

`safehoused` reads its `personas` allowlist **once at boot** — a plain TOML
array with **no runtime registration, no glob/prefix matching, and no SIGHUP
reload**. So the persona Loom narrates as must be added to safehoused's config
and **safehoused must be restarted** before it will accept the connection:

1. Add the persona to safehoused's config (default `loom_daemon`):
   ```toml
   personas = ["loom_daemon"]
   ```
2. **Restart `safehoused`** — the allowlist is not hot-reloaded.
3. Enable Loom narration (`safehouse.enabled = true`, or
   `LOOM_SAFEHOUSE_ENABLED=1`) and ensure the socket path resolves.
4. Run a sweep; the room shows `loom-daemon → everyone · task` lines, threaded
   per issue.

This static, operator-provisioned model is a phase-1 constraint. Per-issue
personas (`loom_builder_42`) are blocked upstream until safehoused grows prefix
support and are tracked in #3999 — do not attempt to register personas at
dispatch time; there is no such path.

## What gets narrated

The sink maps the **existing frozen event taxonomy** (`event_bus.rs`,
`types.rs`) to envelope-v1 messages. All are broadcast (`to: "*"`) and threaded
by the bare issue number (`task_id`):

| Bus event | Envelope `type` | Body |
|---|---|---|
| `SweepGlobalDispatch` (Issue) | `task` | `sweep dispatched: issue #N` |
| `SweepPhase` | `task` | `issue #N → <phase>` (+ `(PR #M)` when present) |
| `SweepBlocker` | `handoff` | `issue #N blocked: <reason>` (a human must act) |
| `SweepExited` | `ack` | `issue #N complete (exit <code>, <dur>s)` |
| `SweepCrashed` | `handoff` | `issue #N crashed at <checkpoint_phase>` |

`SweepGlobalCompleted` is intentionally **not** narrated: it carries only a
`sweep_id` (no issue number), and `SweepExited` already emits the completion
`ack` with richer data — narrating both would double-post per completion.

## Wire protocol (envelope v1)

- `AF_UNIX`, **newline-delimited JSON**, one object per line, bidirectional.
- Mandatory first request: `{"id":0,"op":"hello","persona":"<name>"}`.
- `send` carries `to`/`type`/`body` and optional `task_id`/`room`. `type` is a
  closed enum `{chat,task,handoff,ack}`; `task_id` must be `[A-Za-z0-9_]` (both
  validated before sending). The daemon **stamps `from`** from the socket
  identity — the client never sends one (no impersonation).
- Replies echo the request `id`. **Async push lines are interleaved on the same
  connection, carry an `event` key, and have no `id`** — the client
  demultiplexes by skipping any line with an `event` key. Phase 1 is emit-only,
  so pushes are read off the wire and discarded.

## Implementation

- `loom-daemon/src/safehouse.rs` — config resolver, envelope-v1 client, the
  event→envelope mapping, and the reconnecting bus-subscriber sink.
- `loom-daemon/src/workspace_pool.rs` — `start_safehouse_narration()` subscribes
  the shared `Arc<EventBus>` (the single place it is owned) and spawns the sink
  on the daemon runtime; a no-op when disabled.
