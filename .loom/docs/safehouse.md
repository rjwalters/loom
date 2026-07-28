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

# Phase 2 — worker-side `safehouse-mcp` injection (#3999)

Phase 1 lets the daemon *narrate*. Phase 2 gives each **worker** session a
two-way handle: when the `safehouse` block is enabled, Loom injects the
`safehouse-mcp` stdio MCP server (`rjwalters/safehouse`, tools `safehouse_send` /
`safehouse_read` / `safehouse_create_room` / `safehouse_list_rooms`; env
`SAFEHOUSED_SOCKET` + `SAFEHOUSE_PERSONA`) into the worker's MCP config, so a
Builder can ask a question in the room mid-task and read the human's answer
instead of only signalling through labels. The MCP server holds no keys — the
socket path is the only credential-adjacent value written.

## Per-worker persona: a bounded pre-registered pool (design decision)

safehoused's persona allowlist is a **static boot-time TOML array** with no
runtime registration, no glob/prefix matching, and no SIGHUP reload (see phase-1
note above). So a literal per-issue name like `loom_builder_42` **cannot** be
minted at dispatch time — safehoused would reject the `hello` for a name not in
its boot allowlist, and it cannot restart per worker.

Loom therefore assigns each worker a persona from a **bounded pool the operator
pre-registers** in safehoused's allowlist ahead of time — the same "fixed pool,
rotate per slot" shape as the token pool. Configure the pool in the `safehouse`
block and add the identical names to safehoused's `personas`:

```jsonc
"safehouse": {
  "enabled": true,
  "socket": "/run/safehoused.sock",
  "persona": "loom_daemon",                     // scalar fallback (daemon + no-pool workers)
  "workerPersonas": ["loom_builder_1",          // the pre-registered worker pool
                     "loom_builder_2",
                     "loom_builder_3",
                     "loom_builder_4"],
  "mcpCommand": "safehouse-mcp"                  // launcher for the stdio MCP server
}
```

```toml
# safehoused config — restart required after editing (allowlist read once at boot)
personas = ["loom_daemon", "loom_builder_1", "loom_builder_2", "loom_builder_3", "loom_builder_4"]
```

Each worker is assigned `workerPersonas[issue_number % pool_size]` (round-robin
by worktree slot — the issue number comes from `LOOM_SWEEP_CLAIM_OWNED`). Two
**concurrently-running** workers (distinct issue numbers) get distinct personas
whenever the pool is at least as large as the concurrency level and the numbers
do not collide mod N — so size the pool to your max concurrent workers. With **no
`workerPersonas`** configured, every worker falls back to the scalar `persona`
(workspace-wide, no per-worker attribution) — the feature degrades, never fails.

Env overrides (each wins over config): `LOOM_SAFEHOUSE_WORKER_PERSONAS`
(comma-separated pool), `LOOM_SAFEHOUSE_MCP_COMMAND`, plus the phase-1
`LOOM_SAFEHOUSE_ENABLED` / `LOOM_SAFEHOUSE_SOCKET` / `LOOM_SAFEHOUSE_PERSONA`.

## Delivery: session-scoped `--mcp-config` at spawn time

Injection happens in `spawn-claude.sh` (the mandatory agent spawn path), not by
rewriting the workspace `.mcp.json`. Concurrent sweeps **share** the workspace
root, so a per-worker persona cannot live in that shared file; instead
spawn-claude generates a **session-scoped** MCP config (persona substituted for
this worker) and passes it via `claude --mcp-config <file>`. The file lists the
`loom` server FIRST (so it is self-contained even when the session cwd has no
project `.mcp.json`) and appends `safehouse` second.

`scripts/setup-mcp.sh` (the workspace-root generator, reached inside worktrees
via the `.mcp.json` symlink `worktree.sh` creates) **also** learns to append the
`safehouse` server when enabled — but with the scalar `persona`, since it is not
per-worker. Both writers keep `loom` first and unchanged so
`claude-wrapper.sh`'s MCP pre-flight (which keys off the first server with args)
still resolves the loom entry point.

## Degradation contract (unchanged from phase 1)

- `safehouse.enabled` false/absent ⇒ **byte-for-byte no-op**: spawn-claude
  appends no `--mcp-config`, and setup-mcp emits the identical loom-only file.
- Enabled but the launch command is missing, or no socket resolves ⇒ one
  `warn`, **injection skipped**, the `loom` MCP server unaffected and the worker
  starts normally.
- Socket configured but not yet present at spawn ⇒ one `warn`, injected anyway
  (best-effort — `safehouse-mcp` connects lazily and never blocks the worker).
- A persona absent from safehoused's boot allowlist is rejected by safehoused at
  `hello` with a clear message — provision the whole pool before enabling.

## Implementation (phase 2)

- `defaults/scripts/lib/mcp-config.sh` — shared resolvers (env > config >
  default, mirroring `safehouse.rs`), the pool round-robin persona picker, and
  the `loom`-first `.mcp.json` emitter.
- `defaults/scripts/spawn-claude.sh` — per-worker injection via `--mcp-config`.
- `scripts/setup-mcp.sh` — workspace-root two-server generation when enabled.
- Tests: `defaults/scripts/tests/test-mcp-config.sh`.

> The exact `safehouse-mcp` binary/protocol lives in the external
> `rjwalters/safehouse` repo and is not verifiable from this repo, so the
> launcher is configurable (`safehouse.mcpCommand`) and a missing command
> degrades to a logged skip rather than a broken server entry.
