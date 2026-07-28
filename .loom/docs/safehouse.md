# Safehouse fleet-comms narration (phase 1, #3997)

Loom coordinates through forge labels — that is unchanged and remains the sole
source of truth. **Safehouse** ([`rjwalters/safehouse`](https://github.com/rjwalters/safehouse))
adds an optional, additive **narration** side-channel: an end-to-end-encrypted
Matrix room a human watches in Element to follow a multi-host agent fleet in
real time, instead of polling `gh` or tailing daemon logs.

Phase 1 **narration** is daemon-side and emit-only: the `loom-daemon` subscribes
its existing in-process event bus and narrates sweep-lifecycle transitions into
the room, adding no new event topics and no new publish call sites. On top of
that, **peer-claim coordination (#4028) makes the room bidirectional** — a
dedicated read task consumes inbound peer advertisements so daemons on a shared
backlog back off before the non-atomic `loom:building` label flip would let them
race. See [Peer-claim coordination](#peer-claim-coordination-cross-host-soft-claim-4028)
below.

> **Out of scope** (tracked separately): per-worker personas (`loom_builder_42`)
> and `SAFEHOUSE_PERSONA` forwarding to workers → **#3999**; inbound **human**
> steering (reading `@`-mentions back to agents) → follow-up (it reuses the same
> inbound read task #4028 adds); cloud-host provisioning of `safehoused` →
> **#3998**; carrying the judge verdict value in an event payload (needs a
> frozen-taxonomy amendment) → follow-up; the **atomic cross-host claim
> authority** (a real CAS behind the soft claim) → Phase 2 of #4028.

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
   per **repo-qualified** issue (`<repo>_<issue>`, issue #4201) so two managed
   repos' identically-numbered issues never collide into one thread.

This static, operator-provisioned model is a phase-1 constraint. Per-issue
personas (`loom_builder_42`) are blocked upstream until safehoused grows prefix
support and are tracked in #3999 — do not attempt to register personas at
dispatch time; there is no such path.

## What gets narrated

The sink maps the **existing frozen event taxonomy** (`event_bus.rs`,
`types.rs`) to envelope-v1 messages. All are broadcast (`to: "*"`).

### Repo qualification (issue #4201)

The daemon manages multiple workspaces (loom, vibesql, anvil, kicad-tools, …)
behind a **single shared event bus** (`workspace_pool.rs`), so a bare issue
number is not unique across them — loom `#4201` and vibesql `#4201` would
otherwise thread into the *same* Matrix room thread. Every narrated event's
`task_id` and body prefix are therefore **repo-qualified**:

- **Convention**: the repo name is the **basename of the workspace-root
  filesystem path** stamped onto the event's `repo` field by
  `SweepRegistry::emit_event` (Issue #3929's pattern) — e.g.
  `/Users/x/GitHub/vibesql` → `vibesql`. This is a path-derived directory name,
  not a forge `owner/repo` slug: it needs no network call, and the daemon's
  workspace registry already guarantees at most one managed registry per path.
- **`task_id`**: `<repo>_<issue>` (e.g. `vibesql_6173`), with any character
  outside the `[A-Za-z0-9_]` charset (`build_send_request` enforces this)
  folded to `_` — so `kicad-tools` becomes `kicad_tools`.
- **Body prefix**: `<repo>#<issue>` (e.g. `vibesql#6173`), used verbatim since
  the body is free text with no charset restriction.
- **Fallback**: an event with no `repo` known (a synthetic/test event, or one
  from an era before this field existed) narrates with the pre-#4201
  unqualified form — bare `<issue>` for `task_id`, bare `#<issue>` for the body
  prefix — rather than erroring.

`SweepGlobalDispatch` needed a small additive amendment (`repo: Option<String>`)
to carry this — it was the one sweep-scoped event that had not yet been stamped
with `repo`, unlike `SweepPhase`/`SweepBlocker`/`SweepExited`/`SweepCrashed`.

### Body grammar (issue #4201)

Every narrated body follows `<repo>#<issue> · <phase/status> [· <detail>] [—
<commentary>]` — informal by design (there is no single rigid 4-field parse),
but consistently repo-qualified and consistently favoring one line of
actionable detail over the previous terse `issue #N …` phrasing:

| Bus event | Envelope `type` | Body |
|---|---|---|
| `SweepGlobalDispatch` (Issue) | `task` | `<repo>#N · dispatch` — the sink best-effort appends ` — "<issue title>"` (see below) |
| `SweepPhase` | `task` | `<repo>#N · <phase>` (+ ` · PR #M open` when present) |
| `SweepBlocker` | `handoff` | `<repo>#N · BLOCKED — <reason>` (a human must act) |
| `SweepExited` (exit 0) | `ack` | `<repo>#N · done ✓ · <dur>` (e.g. `6m55s`) |
| `SweepExited` (exit ≠ 0) | `ack` | `<repo>#N · failed ✗ · exit <code>[ (decoded)] · <dur>` — exit `78` decodes to `(EX_CONFIG: token pool)`; every other code prints raw (no attempt at a full sysexits table) |
| `SweepCrashed` | `handoff` | `<repo>#N · crashed ✗ at <checkpoint_phase> — resumable (checkpoint kept)` |

**Dispatch-line title (AC3)**: the operator's highest-value ask was seeing the
issue title on the dispatch line (the single most common message in the room —
33 of 60 messages in the first night's history were bare dispatch roots). The
payload-amendment route (threading the title through `SweepGlobalDispatch`)
was judged too heavy for this bug-fix issue, unlike the small `repo` amendment
above (which fixes an actual collision bug). Instead the **sink** fetches it at
narration time — one `gh issue view --json title --jq .title` in the event's
workspace root, bounded by a 5s timeout, with a 10-minute cache keyed by
`(workspace_root, issue)` so a re-dispatch of the same issue (e.g. after a
Doctor cycle) does not re-shell to `gh`. Every failure (missing `gh`, no
network, unauthenticated, timeout) degrades to narrating the dispatch line
**without** a title — this never blocks narration or the sweep itself.

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
  demultiplexes by skipping any line with an `event` key. The **narration**
  connection is emit-only and discards inbound pushes; the **peer-claim
  coordination** connection (#4028) instead routes each inbound `event` line to a
  handler (see below).

## Implementation

- `loom-daemon/src/safehouse.rs` — config resolver, envelope-v1 client, the
  event→envelope mapping, the reconnecting bus-subscriber narration sink, and
  (#4028) the peer-claim coordination task + `InboundEventSink`.
- `loom-daemon/src/peer_claims.rs` — the pure, socket-free peer-claim view
  (TTL expiry, self-claim recognition, retraction, `ClaimAd` parse/serialize).
- `loom-daemon/src/workspace_pool.rs` — `start_safehouse_narration()` and
  `start_peer_coordination()` subscribe/attach the shared `Arc<EventBus>` /
  `PeerClaimView` and spawn on the daemon runtime; both no-ops when disabled.

## Peer-claim coordination: cross-host soft claim (#4028)

On a multi-host deployment the only cross-host claim signal is the forge label,
whose `loom:issue → loom:building` flip is **not** compare-and-swap
(`SweepRegistry::flip_label_to_building` is an unconditional
`--remove-label`/`--add-label`) — two hosts can both read `loom:issue` and
dispatch before either flip propagates, producing duplicate sweeps. Peer-claim
coordination shrinks that window:

- **Advertise.** At the dispatch decision point — right after the local claim
  lock, **before** the label flip — the daemon publishes a claim advertisement
  over the room: issue number, [repo slug](#), host identity, PID, and a
  wall-clock timestamp, carried as a **`task`** envelope (the `type` enum is
  closed and owned by the safehouse repo, so a claim rides `task` with the bare
  issue number as `task_id` — **no fifth type is invented**) whose `body` is the
  structured JSON payload (marked `loom_claim`).
- **Consume.** A **dedicated inbound read task** — separate from the narration
  sink — drains the socket continuously via `select!`, so an **idle** daemon that
  emits no narration still observes peer claims promptly (the narration
  connection only reads while it is emitting). Each inbound claim is folded into a
  shared `PeerClaimView`.
- **Back off.** The work-finder skips any issue with a live peer claim, counted
  under its **own** distinct `peer-claim-skip` reason on the per-tick summary line
  (never folded into #4085's collision count).
- **TTL.** Every peer claim expires after **`safehouse.peerClaimTtlSecs`
  (default 120s = 2× the 60s work-finder interval)**, so a crashed peer cannot
  permanently starve an issue. The TTL clock is the **local receipt `Instant`**,
  never the advertiser's wall clock (clock skew is not comparable across hosts).
  A peer also **retracts** its claim early when its sweep exits/crashes (a
  `retract`-kind ad emitted from the reaper), freeing the issue before the TTL.
- **Host identity.** loom's single, explicit host-identity concept is
  `sweep_registry::host_identity()` (`LOOM_HOST_ID` > `$HOSTNAME` > the `hostname`
  binary > `unknown-host`) — derived, not a new config block, and stable across
  restarts. safehoused stamps the socket `from` from the *persona* (all daemons
  share `loom_daemon`), which cannot distinguish hosts, so the identity travels in
  the claim body and is what powers self-claim recognition: **a daemon never backs
  off on its own advertisement.**
- **Event taxonomy.** The internal pub/sub topic taxonomy is frozen; peer claims
  add **no new bus topic** — they travel entirely over the safehouse room.

### Soft claim, NOT a mutex (the load-bearing caveat)

A room broadcast is eventually consistent, so this is a **fast backoff, not a
lock**: two hosts advertising near-simultaneously still race. Advertisement
*shrinks* the collision window; it does not close it. The atomic authority for
the final claim — a real cross-host CAS (e.g. a `git push` to a claim ref) — is
**Phase 2 of #4028**, deliberately out of scope here.

### Fail-open (never a liveness dependency)

Coordination is best-effort end to end: an unreachable/refusing/timing-out
`safehoused` socket, a malformed inbound envelope, or a full outbound channel is
logged (once) and **dispatch proceeds normally**. The outbound advertisement is a
bounded, non-blocking `try_send` off the dispatch path; a `Full`/`Closed` channel
drops the ad. `safehouse.enabled` false/absent is a **byte-for-byte no-op**: no
view, no channel, no coordination task, no socket.

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
