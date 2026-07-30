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
> inbound read task #4028 adds); carrying the judge verdict value in an event
> payload (needs a frozen-taxonomy amendment) → follow-up; the **atomic
> cross-host claim authority** (a real CAS behind the soft claim) → Phase 2 of
> #4028. Cloud-host provisioning of `safehoused` (formerly tracked here as
> **#3998**) has landed — see
> [Fleet provisioning: cloud workers](#fleet-provisioning-cloud-workers-fleet-add-worker---safehouse-3998)
> below.

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

## Connection status: not configured / unreachable / connected (#4345)

Before #4345, `safehouse.enabled` false/absent, enabled-but-unreachable, and
enabled-and-connected all looked identical to an operator — silence. Two
surfaces now report the live state:

- **`loom-daemon status`** (and `status --json`) prints a `Safehouse:` line
  (a `safehouse` object in JSON) with one of three states, self-reported by
  the daemon's own live connection — never a second, status-time connection
  attempt (a CLI-side probe could not know "room joined" the way the daemon's
  own connection can):
  - `not configured` — no `safehouse` block, `enabled: false`/absent, or
    enabled with no socket path resolving at all (nothing to even try). No
    connection has been attempted.
  - `configured, unreachable` — enabled, a socket path resolved, but the most
    recent connect attempt failed, was refused, or dropped. The resolved
    socket path is included.
  - `connected` — the most recent connect attempt completed the `hello`
    handshake. The configured room name is included when one was configured
    (`safehouse.room` unset is valid only when safehoused joined exactly one
    room, resolved server-side — the daemon is never told that resolved name,
    so the line omits it rather than guessing).
- **`loom-daemon-start.sh`** prints a cheaper, **static**, pre-connect check at
  start time (`ok`/`warn` colored, one line): it runs *before* the daemon
  connects, so it can only distinguish "not configured" from "configured" —
  proving "connected" needs the daemon's own live socket, which is what
  `loom-daemon status` is for. Concretely: no `safehouse` block/disabled ⇒
  `not configured`; enabled with no socket resolving ⇒ `configured,
  unreachable`; enabled with a socket path that exists as a socket on disk ⇒
  `configured (socket present)`; enabled with a socket path that does not
  exist yet ⇒ `configured, unreachable` (the path is included either way).

Implementation: `loom-daemon/src/safehouse.rs`'s `SafehouseState` is a shared
`Arc<Mutex<..>>` cell (the same injection shape [`PeerClaimView`] already
uses) updated by both the narration sink ([`run_sink`]) and the peer-claim
coordination task ([`run_coordination`]) on every connect/disconnect
transition; `workspace_pool.rs`'s `WorkspacePool` owns one cell per daemon and
`ipc.rs`'s `build_daemon_status` reads it into a new optional
`DaemonStatusReport.safehouse` field (`#[serde(default)]`, so an older
daemon's wire payload — missing the field entirely — still parses).
`loom-daemon-start.sh`'s static check reuses the same env>config>default
resolvers `lib/mcp-config.sh` already defines for the safehouse-mcp worker
injection (phase 2, below) rather than re-deriving them.

## New-host onboarding (#4345, #4346)

The path from a fresh interactive host (no `safehoused`, no `safehouse` config
block anywhere) to `loom-daemon status` reading `connected`. Step 3 registers
`safehoused` as a **supervised service** (launchd LaunchAgent on macOS,
`systemd --user` on Linux) via `safehoused-service.sh` (#4346); running it by
hand is still documented as the debug fallback.

1. **Bot account + credentials.** Provision (or reuse) a Matrix account for
   the `loom_daemon` persona in the target safehouse deployment — this is an
   operator-side step in the external `rjwalters/safehouse` repo/deployment,
   not something this repo automates. Note the account's credentials and the
   room the fleet uses.
2. **Build/install `safehoused`.** Build from the `rjwalters/safehouse`
   checkout per that repo's own instructions. Confirm the `personas` allowlist
   in its config includes `loom_daemon` (or whichever persona you assign this
   host, per "Operator setup" above) — the allowlist is boot-time and
   restart-only, no hot reload.
3. **Register `safehoused` as a supervised service.** Use
   [`safehoused-service.sh`](#supervised-service-wrapper-safehoused-servicesh-4346) —
   it renders and installs a launchd LaunchAgent (macOS) or `systemd --user`
   unit (Linux) that starts `safehoused` at login and keeps it up
   (`KeepAlive`/`Restart=always`, so it survives a crash or reboot), mirroring
   `loom-daemon-start.sh`'s own supervised-service pattern:
   ```bash
   # Preview the service definition first (no side effects):
   ./.loom/scripts/cli/safehoused-service.sh --print-plist   # macOS
   ./.loom/scripts/cli/safehoused-service.sh --print-unit    # Linux
   # Then install + start (point --bin at your built safehoused; the socket is
   # resolved from the same safehouse.socket / $SAFEHOUSED_SOCKET chain the
   # daemon uses, or pass --socket explicitly):
   ./.loom/scripts/cli/safehoused-service.sh install --bin "$(command -v safehoused)"
   ```
   On a headless Linux host, run `loginctl enable-linger "$USER"` once so the
   `systemd --user` unit survives a reboot. **Fallback (debug only):** start it
   by hand under any supervisor — `nohup safehoused &`, a tmux pane, a personal
   plist. Either way, note the socket path safehoused binds (its own config
   controls this) for the next step.
4. **Socket env or config.** Either export `SAFEHOUSED_SOCKET=<path>` (the
   convention safehoused's own clients read) machine-wide, or set
   `safehouse.socket` explicitly in this host's `.loom/config.json` — see
   [Socket resolution](#configuration) above for the full precedence.
5. **Enable the `safehouse` config block** in `.loom/config.json` (per
   workspace, since it lives in the per-repo config tier) or export
   `LOOM_SAFEHOUSE_ENABLED=1` machine-wide:
   ```jsonc
   "safehouse": {
     "enabled": true,
     "socket": null,      // omit to rely on $SAFEHOUSED_SOCKET
     "room": null,         // omit only if safehoused joined exactly one room
     "persona": "loom_daemon"
   }
   ```
6. **Start/restart `loom-daemon`** (`loom-daemon-start.sh`). Its startup
   banner prints the static `Safehouse:` line described above — confirm it
   reads `configured (socket present ...)`, not `not configured` or
   `configured, unreachable`, before moving on.
7. **Verify with `loom-daemon status`.** Give it a few seconds for the
   narration sink / peer-coordination task to complete their first connect
   (the sink connects lazily on the first narrated bus event; the
   peer-coordination task connects eagerly at daemon startup, so it is
   usually first to show `connected`). Confirm the `Safehouse:` line reads
   `connected` with the expected room, then run a sweep and confirm the room
   shows the `loom-daemon → everyone · task` dispatch line.

If the line sticks at `configured, unreachable`: confirm `safehoused` is
actually running and bound to the exact path `loom-daemon status` reports,
that the persona is in safehoused's allowlist (a rejected `hello` also
degrades to `unreachable` — check the daemon log for `safehoused rejected
persona`), and that the daemon process can reach the socket path (permissions,
same-host, no stale socket file from a crashed prior run).

### Supervised service wrapper: `safehoused-service.sh` (#4346)

`defaults/scripts/cli/safehoused-service.sh` (installed as
`.loom/scripts/cli/safehoused-service.sh`) registers `safehoused` as a
supervised service so it starts at login and comes back after a crash or
reboot — the interactive-host counterpart to the cloud-host provisioning path
(#3998). It mirrors `loom-daemon-start.sh`'s supervised-service pattern
(launchd LaunchAgent on macOS / `systemd --user` on Linux) including the
`--print-plist` / `--print-unit` preview modes.

| Command | Effect |
|---|---|
| `--print-plist` / `--print-unit` | Print the launchd plist / systemd unit that *would* be installed, no side effects (any platform). |
| `install` | Render + install + enable + start the service. |
| `uninstall` | Stop + disable + remove the service definition. |
| `status` | Report whether the supervised service is loaded / running. |

Parameters (precedence **flag > env > config > default**): `--bin`
(`SAFEHOUSED_BIN`, else `command -v safehoused`); `--exec "<argv>"`
(`SAFEHOUSED_EXEC`) for a full ExecStart override when safehoused needs flags;
`--socket` (else the shared `safehouse.socket` → `$LOOM_SAFEHOUSE_SOCKET` →
`$SAFEHOUSED_SOCKET` chain the daemon resolves); `--config`
(`SAFEHOUSED_CONFIG`); `--log` (default `~/.loom/logs/safehoused.log`);
`--label` / `--unit` for the launchd label / systemd unit name.

**Supervision policy differs from `loom-daemon`'s on purpose.** `loom-daemon`
uses `KeepAlive:{SuccessfulExit:true}` / `Restart=on-success` because it has a
clean-exit restart *primitive* (exit 0 == intentional relaunch, the
`RestartDaemon` path). `safehoused` has no such primitive — it is a persistent
connection daemon that should simply stay up — so the wrapper renders
`KeepAlive=true` (launchd) / `Restart=always` + `RestartSec=5` (systemd).

**Ownership decision (recorded here per #4346's acceptance criteria):** this
wrapper is deliberately **safehoused-agnostic** and lives in *this* repo, while
the **authoritative** service definition (safehoused's real argv, config
schema, and key-backup / steady-state teardown semantics) is owned by the
external `rjwalters/safehouse` repo. loom does not vendor safehoused's binary
invocation — that would rot the moment the external repo changes it — so the
wrapper only supervises an operator-supplied binary and bakes a minimal,
non-secret environment (`SAFEHOUSED_SOCKET` / `SAFEHOUSED_CONFIG` when
provided; never a forwarded token). If the safehouse repo ships its own service
files, point the runbook at those and treat this generator as the fallback.

## Fleet provisioning: cloud workers (`fleet add-worker --safehouse`, #3998)

The onboarding runbook above is for an interactive host an operator sets up by
hand. `loom-daemon fleet add-worker <ssh-host> --repo <owner/name> --safehouse
<inputs>` (epic #4340, `loom-daemon/src/fleet/add_worker.rs`) is the same
onboarding **encoded as an ordered, idempotent plan** that a cloud worker's
spin-up runs unattended over SSH — no cloud-init fragment, no cloud CLI, no
Tailscale API call from loom itself (epic #4340's boundary: a VM comes from
`repo:remote`, loom only consumes "a reachable box + an SSH alias").

### What the plan does

With `--safehouse`, `fleet add-worker` appends seven steps after the plain
worker's bootstrap (each following the same check/apply contract as the rest
of the plan, so a re-run against an already-provisioned host reports every one
`AlreadyDone`):

1. **`safehouse-tailscale-install`** — installs the `tailscale` apt package.
2. **`safehouse-tailscale-join`** — `tailscale up --auth-key=file:<path>` with
   the operator-minted key (below). No `--advertise-tags`: the tag is baked
   into the key server-side.
3. **`safehouse-build`** — `cargo build --release -p safehoused` from a fresh
   `rjwalters/safehouse` checkout.
4. **`safehouse-config`** — writes `~/.loom/safehoused/config.toml` (`0600`):
   homeserver URL, the per-host Matrix account, fresh store/recovery
   passphrases, and the persona allowlist. **Must precede step 6** — the
   allowlist is boot-time-only (no reload), and the plan's step order enforces
   this (asserted in `add_worker.rs`'s tests).
5. **`safehouse-room-invite`** — joins the fleet room via
   [safehouse#39](https://github.com/rjwalters/safehouse/issues/39)'s
   daemon-side `invite` op — never raw CS-API temporary devices. loom does not
   vendor this invocation (owned by the external repo); override it with
   `--safehouse-invite-exec "<argv>"` if it changes upstream.
6. **`safehouse-supervise`** — installs `safehoused` under `systemd --user` via
   [`safehoused-service.sh`](#supervised-service-wrapper-safehoused-servicesh-4346)
   (the same script the interactive runbook uses) and enables lingering.
7. **`safehouse-daemon-restart`** — wires `LOOM_SAFEHOUSE_ENABLED` /
   `_SOCKET` / `_ROOM` into the worker's own `loom-daemon` systemd unit and
   restarts it — env-only, per #3997's decision (no worker-side
   `.loom/config.json` edit).

**Without `--safehouse`, behavior is byte-for-byte unchanged**: a single
`safehouse` skip-with-notice entry, a plain worker, zero safehouse
provisioning.

### Inputs the operator must mint

Every secret travels the same way `AddWorkerConfig`'s existing `--pat-file` /
`--accounts-env` do: read locally at preflight, transferred to the worker only
over **ssh stdin**, landing only in `0600` files. None of these ever appear on
a command line, in the rendered `--dry-run` plan text, in a daemon log at any
level, or in the fleet registry.

| Flag | Contents | Notes |
|---|---|---|
| `--safehouse-tailnet-auth-key-file PATH` | A Tailscale auth key | **Operator-minted, ephemeral + `tag:loom-worker`** — loom never calls the Tailscale API. Ephemeral means a dead VM auto-deregisters from the tailnet with no fleet-roster bookkeeping. |
| `--safehouse-secrets-file PATH` | `KEY=VALUE` lines: `SAFEHOUSE_MATRIX_USER_ID`, `SAFEHOUSE_MATRIX_PASSWORD`, `SAFEHOUSE_STORE_PASSPHRASE`, `SAFEHOUSE_RECOVERY_PASSPHRASE` | The Matrix account is **operator-created** on the homeserver (the [safehouse#25 verified sequence](#operator-setup-provisioning-the-persona-requires-a-safehoused-restart)) — `fleet add-worker` never needs homeserver admin credentials, only the resulting account. Passphrases are freshly generated per host. |
| `--safehouse-homeserver-url URL` | Not secret | Must resolve inside the tailnet. |
| `--safehouse-room ROOM` | Not secret | The fleet room this host joins. |
| `--safehouse-persona NAME` (repeatable) | Not secret | Mirrors the studio host's allowlist (#3999) — at least one required. |
| `--safehouse-repo-url URL` | Not secret | Defaults to `rjwalters/safehouse`. |
| `--safehouse-invite-exec "ARGV"` | Not secret | Overrides the default `safehoused invite --config <path>` if safehouse#39's CLI surface changes upstream. |

`--safehouse` with any of the first five omitted fails **preflight** — before
any SSH connection — with a message naming exactly which flag is missing (no
half-joined host).

### Required tailnet ACL

The auth key's `tag:loom-worker` is expected to carry an ACL restricting
workers to reaching only the homeserver's `443`, not the rest of the tailnet.
loom documents this requirement; it does not manage the tailnet ACL itself
(epic #4340's boundary — no Tailscale API call from this repo).

### Teardown: `fleet drain`'s flush verification

`fleet drain <ssh-host>`'s `flush-safehouse` phase (`loom-daemon/src/fleet/drain.rs`)
is the spin-up's counterpart: it stops the worker's `safehoused` unit over SSH
(`systemctl --user stop safehoused`) — a supervised stop **is** the flush,
since safehoused's SIGTERM/ctrl-c shutdown path calls
`client.encryption().backups().wait_for_steady_state()` and prints
`"safehoused: room-key backup flushed; bye"` before exiting — then verifies via
the journal line, falling back to the unit's `ExecMainStatus` when the journal
has rotated. The verdict maps onto drain's existing contract:

- `safehouse.enabled == false` — `Skipped`, no room keys in play, exit `0`.
- Flush verified — `Changed`, eligible for "safe to power off", exit `0`.
- Flush **not** verified (nonzero remote exit, or the host was unreachable) —
  `Unverified`; the drain still completes (workspace/roster cleanup proceed —
  loom never refuses to retire a box over this), but the report withholds
  "safe to power off" and exits `3` so an operator/monitor treats it as a flag,
  not a clean success.

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
| `SweepExited` **whose PR merged** (#4426) | `completion` | `<repo>#N · merged ✓ · PR #M · <dur>` — emitted *in addition to* the `ack`, carrying the `completion-v1` `meta` (see below) |

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

### Completion envelopes → the public fleet feed (#4426)

safehoused's egress subsystem mirrors well-formed **`completion`** envelopes out
of allowlisted rooms — redacted and delay-buffered — to a `sink_url`; that is
what feeds the public fleet feed on 2amlogic.com. Loom is the producer:

- **Emit point**: the narration sink, on `SweepExited`. Exit status alone proves
  nothing, so the sink checks **forge truth** (`gh pr list --head
  feature/issue-N --state merged`, in the event's workspace root, 10s timeout)
  and emits the `completion` only when that issue's PR actually merged — the
  `ack` still goes out either way. Chosen over having the sweep child publish a
  post-merge phase event because it is daemon-only (no skill edit), has the
  sweep timing to hand, and verifies rather than trusts.
- **`meta` (`completion-v1`)**: `{schema, agent, repo, ref, result, started_at,
  completed_at}` required, plus optional `issue`/`tokens` (envelope-v1 preserves
  unknown `meta` keys, so no schema rev is needed for extensions). `body` stays
  required human prose — a room reader sees a sentence, `meta` is the machine
  view.
- **`repo` is the forge `owner/repo` slug** (`gh repo view --json
  nameWithOwner`, cached per workspace for the daemon's lifetime), deliberately
  **not** the path-basename convention above: the feed links `ref` (the PR URL)
  and displays the forge identity. `tokens` is omitted rather than guessed — no
  cheap token source is wired to the sink.
- **Timestamps** come from the reaper's clock (`started_at = exit − duration_sec`,
  `completed_at = exit`), so the pair is always self-consistent.
- **`result: "failure"` is out of scope for v1**: `completion-v1` requires a
  `ref`, and a sweep with no merged PR has no meaningful one (an open PR is
  unfinished, not failed, and is usually resumed). The wire support exists
  (`CompletionResult::Failure`) for a follow-up that identifies a genuinely
  terminal negative outcome.
- **At most one per merge**, deduped on `(workspace, issue)` for the daemon's
  lifetime, so a resumed sweep's second `SweepExited` does not double-post.
  Downstream ingest is additionally idempotent on `event_id`.
- **Strict client-side construction.** safehoused **silently degrades a
  malformed `meta` to `chat`** — the event then vanishes from the feed with no
  error anywhere — so `build_send_request` refuses to send a `completion` unless
  `validate_completion_meta` accepts it (all required fields present and
  non-empty, `schema == "completion-v1"`, `agent` a valid persona, `repo` an
  `owner/repo` slug, `ref` an absolute http(s) URL, `result` ∈
  {`success`,`failure`}, both timestamps RFC3339 with `completed_at >=
  started_at`). Nothing here relies on server-side validation.
- **Same degradation contract**: a failing/absent/slow `gh`, an unreachable
  safehoused, or a rejected envelope drops the completion silently and never
  affects the sweep.

## Wire protocol (envelope v1)

- `AF_UNIX`, **newline-delimited JSON**, one object per line, bidirectional.
- Mandatory first request: `{"id":0,"op":"hello","persona":"<name>"}`.
- `send` carries `to`/`type`/`body` and optional `task_id`/`room`/`meta`. `type`
  is a closed enum `{chat,task,handoff,ack,completion}` owned by the safehouse
  repo (loom invents no members); `task_id` must be `[A-Za-z0-9_]`; `meta` is
  valid **only** on a `completion`, which in turn **requires** it (see above) —
  all validated before sending. The daemon **stamps `from`** from the socket
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
  Also (#4345) owns the `SharedSafehouseState` cell and exposes
  `safehouse_status()` for `ipc::build_daemon_status`.
- `loom-daemon/src/types.rs` — `DaemonStatusReport.safehouse` /
  `SafehouseStatus` (#4345), the wire shape for the connection-state line.
- `loom-daemon/src/ipc.rs` / `loom-daemon/src/main.rs` — (#4345)
  `build_daemon_status` reads the pool's `safehouse_status()`; `main.rs`
  renders the `Safehouse:` human line and the `safehouse` JSON object.
- `defaults/scripts/cli/loom-daemon-start.sh` — (#4345) the static
  pre-connect `Safehouse:` line, via `lib/mcp-config.sh`'s existing
  `loom_mcp_safehouse_enabled`/`loom_mcp_safehouse_socket` resolvers.
- Tests: `safehouse.rs`'s `mod tests` (state-cell + wire-rendering cases),
  `workspace_pool.rs`'s `mod tests` (pool wiring), `ipc.rs`'s
  `test_build_daemon_status_reports_halt_and_in_flight` (report field),
  `defaults/scripts/tests/test-loom-daemon-start.sh` (start-wrapper line).

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
