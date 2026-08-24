# ADR-0017: Session-Container Architecture — Two Lifetimes, Headless-Exec Dispatch, Remote-Execution Nested Compute, Fleet-Default Rollout

## Status

Accepted

## Context

Codex has no long-lived credential equivalent to `CLAUDE_CODE_OAUTH_TOKEN`. Its
auth is a mutable `CODEX_HOME/auth.json` refresh chain — treated as opaque by
`spawn-codex.sh` and `loom-daemon accounts` — and when the refresh chain dies
the only recovery is an **interactive** `codex login`. This is a structural
blocker for routine multi-account Codex dispatch: epic #4489 (multi-account
Codex canary, stalled `loom:operator-only`) cannot proceed because headless
workers cannot re-authenticate themselves, and no host-level mechanism today
serializes concurrent `auth.json` refreshes from multiple dispatched workers
sharing one account.

Separately, the fleet has a recurring host-saturation incident class
(#5979, #4903): unbounded concurrent sweeps on one host compete for CPU/mem
with no per-sweep resource ceiling, and workers occasionally need
docker-backed workloads (Lean builds, SPICE simulations, build-gate
toolchains) with no clean way to grant that without handing an agent a
docker socket — which is host-root-equivalent.

Epic #6896 records the fix as **containers as the session-persistence and
containment boundary** for worker runtimes. The operator made four decisions
on 2026-08-24 that this ADR records durably so later phases (#6898 mount
contract, #6899 image layer, and epics Phase 2–4) implement against a
settled design instead of re-litigating it. This ADR is Phase 1 of epic
#6896 and is itself a docs-only change — no behavior changes ship here.

## Decision

Four decisions, each with its own context/consequences/rejected-alternative,
following the shape of [ADR-0012](0012-runtime-adapter-contract.md).

### 1. Two container lifetimes, one dispatch seam

**Context.** Loom's two admitted worker runtimes have fundamentally different
auth shapes: Claude Code's `CLAUDE_CODE_OAUTH_TOKEN` is a long-lived, stateless
bearer credential rotated from a flat token-file pool (`.loom/tokens/`) — any
worker can pick up any token, use it, and hand it back. Codex's `auth.json` is
a *mutable, refreshing* credential chain scoped to one `CODEX_HOME` — two
processes touching it concurrently can clobber each other's refresh, and a
dead chain needs an interactive fix, not a token swap.

**Decision.** Two container lifetimes, chosen per-runtime by that auth shape,
both dispatched through the *existing* `spawn-worker.sh` → `spawn-<runtime>.sh`
seam (`.loom/docs/runtime-adapters.md`) with **no new dispatch path**:

- **Per-account persistent session containers** for runtimes with mutable
  interactive auth (Codex today). One long-lived container per Codex account
  owns that account's `CODEX_HOME` volume. The container *is* the
  auth-persistence boundary: one owner process serializes every refresh
  against that volume (no multi-process `auth.json` clobber race), and the
  container's own lifecycle is independent of any one host session, so it
  survives daemon restarts, SSH disconnects, and host reboots (module the
  drain interaction specified in Decision 4 below).
- **Per-sweep ephemeral containers** for stateless-auth runtimes (Claude
  today). No auth to persist, so no reason to persist the container either —
  it is created for one sweep and torn down when the sweep finishes, carrying
  cgroup CPU/mem limits as its containment payload (epic #6896 Phase 3).

Both lifetimes are runtime-adapter concerns, not dispatch-seam concerns: the
seam that resolves `LOOM_RUNTIME` → `spawn-<runtime>.sh` is unchanged: a
runtime's own adapter internally decides whether "spawn" means "start a fresh
container" (ephemeral) or "exec into an existing one, starting it if absent"
(persistent). The seam's seven contract points (spawn, model mapping, error
classification, usage accounting, instruction format, permission/sandbox
mapping, capability declaration — [ADR-0012](0012-runtime-adapter-contract.md))
are unaffected in shape; only the *spawn* point's internals gain a
containerization option.

**Consequences.**

*Positive*: one Codex account owner eliminates the `auth.json` clobber class
entirely, by construction, rather than by locking discipline layered on top.
Claude's ephemeral containers need no auth-persistence design at all — they
inherit the flat token-pool model unchanged, mounted read-only exactly as
`docker/worker/README.md`'s bootstrap-seam table already documents. Neither
lifetime requires a new dispatch mechanism, so every existing seam property
(runtime resolution precedence, observability markers, exit-code semantics)
continues to hold.

*Negative*: two lifetimes is two operational models an operator must
understand, not one — a Codex session container needs lifecycle commands
(epic #6896 Phase 2: `loom-daemon accounts session start|stop|status|attach`)
that a Claude ephemeral container never needs. A session-managed Codex profile
must also refuse host-direct `CODEX_HOME` use once adopted (Phase 2's
ownership rule), which is a behavior change for any operator workflow that
assumed direct host access to that profile.

**Rejected alternative: one lifetime for everything.** Force every runtime
through ephemeral per-sweep containers, and solve Codex auth persistence with
an external volume + a file lock instead of a container boundary. Rejected:
a file lock only prevents *concurrent* writers; it does nothing for the
actual failure mode (a dead refresh chain needing `codex login`), so the
interactive-recovery problem — the entire reason this epic exists — would
still be unsolved, and the "which process currently owns this volume" question
would need to be reinvented as a separate coordination mechanism instead of
being answered for free by "the container that's still running is the owner."
Conversely, forcing Claude through *persistent* containers for a runtime with
no state to persist would just add teardown/idle-lifecycle overhead for zero
benefit — persistence is opt-in, chosen only where the auth shape demands it.

### 2. Dispatch is headless `docker exec`; tmux is operator-only

**Context.** A session container needs an interactive surface for the one
operation that genuinely requires a human: `codex login` after a refresh
chain dies. It is tempting to route *all* dispatch through that same
interactive surface, since it already exists.

**Decision.** Work enters a session container as a headless command:
`docker exec <session> codex exec …`, preserving exit codes,
`classify-error.sh`'s `codex` provider table, transcripts, and the
observability markers ([contract point 1](../../.loom/docs/runtime-adapters.md#1-spawn))
`spawn-codex.sh` already emits, unchanged. The container also runs a tmux
server, but that server exists **solely** for interactive re-login (`codex
login`) and inspection — an operator `attach`es to it by hand; nothing in the
dispatch path ever writes to it.

Explicitly rejected: **driving the interactive TUI via `tmux send-keys`** as
the dispatch mechanism (i.e., typing `codex`'s interactive prompt into a tmux
pane and scraping the pane output for the response). This would forfeit:

- **Error classification.** `classify-error.sh`'s exit-code-first ordering
  needs an actual process exit code; a tmux pane has no such signal — only
  screen-scraped text, which is exactly the "confident nonsense" failure mode
  the runtime-adapter contract explicitly warns against.
- **Usage accounting.** Per-turn token/model attribution comes from the
  `codex exec` transcript; a TUI session run through tmux either has no
  equivalent structured output or would need a second, bespoke scraper to
  reconstruct it.
- **The dispatch seam's own contract.** `spawn-codex.sh`'s Spawn contract
  (args passthrough, prompt delivery, model tier env, missing-pool/
  runtime-missing failure codes) is defined against a headless child process,
  not a terminal multiplexer pane; layering tmux underneath it would require
  either two divergent spawn paths (headless for Claude, TUI-driven for
  session-mode Codex) or reworking the contract itself for one runtime's
  session mode — both reintroduce exactly the per-runtime special-casing
  [ADR-0012](0012-runtime-adapter-contract.md) exists to prevent.

**Consequences.**

*Positive*: dispatch through a session container is byte-for-byte the same
`docker exec … codex exec …` shape as any other headless invocation, so
everything downstream of exit code + transcript (health classification, the
account pool, sweep dispatch, Judge/Doctor review of a session-dispatched
sweep) needs zero awareness that the runtime happens to live inside a
long-lived container rather than a fresh process.

*Negative*: the tmux server is a second thing to keep running and secure
correctly — it is an attack surface (a shell inside the container) that a
pure headless-exec design would not have, and it sits idle 99.9% of the time,
existing only for the rare re-auth path. This is an accepted cost: without
*some* interactive surface, the epic's whole premise (headless workers cannot
self-recover from a dead refresh chain) has no recovery mechanism at all.

**Rejected alternative: no interactive surface at all — replace with a
periodic pre-emptive re-auth job.** Rejected because `codex login`
fundamentally requires a human in the loop (OAuth device-code / browser flow)
— there is no headless substitute for the moment the actual refresh token
dies, so *some* human-reachable surface is required. tmux was chosen over a
bespoke re-auth CLI because it is already in the base image's core toolchain
(`docker/worker/README.md`'s guaranteed-tools table lists `tmux`) and gives an
operator a real shell for inspection beyond just `codex login`, at no
additional image cost.

### 3. Nested compute is remote execution — never docker-in-docker, never docker.sock passthrough

**Context.** Agents dispatched inside a worker container sometimes need to run
docker-backed workloads themselves (Lean builds, SPICE simulations, build-gate
toolchains that shell out to `docker build`/`docker run`). The container
running the agent is not itself a docker host, so something has to bridge
that gap.

**Decision.** Worker containers get **no docker socket** — none mounted, none
reachable. Docker-requiring workloads route through the epic's Phase 4
`run-job` seam to a host-level executor: a job spec (image, command, mounts
under the mount contract #6898 defines, resource limits) sent to an executor
that is a real docker host, with the loopback/same-host case as the mandatory
degenerate deployment (so a single-host install is self-sufficient with no
external executor dependency) and a right-sized remote host as the general
case.

**Rejected alternative: docker.sock passthrough.** Mount the host's
`/var/run/docker.sock` into the worker container so the agent can `docker
run` directly. Rejected on a single, non-negotiable security ground: **a
mounted docker socket is host-root-equivalent** — any process that can talk
to it can launch a privileged container, bind-mount the host filesystem into
it, and read/write anything the host user can. Handing that to an
LLM-directed agent inside a containment boundary whose entire purpose is
limiting blast radius defeats the boundary. It was also rejected on a
narrower functional ground: docker.sock passthrough makes the *host's*
docker daemon do the work, so per-sweep resource limits (epic #6896 Phase 3)
would need to be enforced a second time at the nested-container level with no
natural place to hook the accounting — the same containment gap the fleet's
saturation incidents (#5979, #4903) are already caused by.

**Rejected alternative: docker-in-docker (a nested dockerd inside the worker
container, `--privileged` or `docker:dind`).** Rejected for the same
root-equivalence problem one layer down — a `--privileged` container is
itself effectively host-root — plus additional operational cost (dind's own
storage-driver quirks, image-cache duplication per worker container instead
of a shared host cache, and a second daemon lifecycle to supervise inside a
container this ADR's Decision 1 already deliberately keeps daemon-free, per
`docker/worker/README.md`'s shape decision that `loom-daemon` stays on the
host, not PID 1 in a container).

**Consequences.**

*Positive*: the security story is simple and auditable — "no worker
container ever holds a credential that can escape its own cgroup" is true by
construction, with no residual-gap footnote the way docker.sock passthrough
would need one. The `run-job` seam is also the concrete consumer open
proposal #3979 (elastic compute) has been waiting for: #3979's *placement*
question (which host runs a given job) now has a real caller asking it,
rather than a proposal with no consumer. Elastic *autoscaling* of executor
hosts explicitly stays in #3979's scope, not this epic's — this ADR only
settles that the seam exists and is socket-free, not how executor capacity
scales.

*Negative*: a down or unreachable remote executor blocks every docker-
requiring job with no local fallback beyond the mandatory loopback/same-host
executor — this is accepted because that same-host executor is required to
exist for exactly this reason (a single-host install must remain
self-sufficient). Every docker-requiring caller (build-gate, sim/build
wrappers) must be migrated onto the seam rather than shelling out to `docker`
directly, which is real, sequenced follow-up work (epic #6896 Phase 4) — this
ADR settles the shape, not the migration.

### 4. Rollout posture

**Context.** Containment changes host behavior for every sweep that runs
under it — resource ceilings, filesystem visibility, and (per Decision 1) an
auth-persistence boundary that did not exist before. It cannot ship as a
silent default; it needs a posture that both proves itself and never
regresses installs that have no reason to want it.

**Decision.**

- **Containment becomes the *default* on Linux fleet hosts after a defined
  soak period** (the soak criteria and the mechanical default-flip are epic
  #6896 Phase 3 scope, not this ADR's — this ADR records only that
  fleet-default-after-soak, not opt-in-forever, is the settled target).
  Bare installs and macOS/operator hosts keep bare-metal dispatch,
  config-selectable — consistent with `docker/worker/README.md`'s existing
  macOS-bind-mount-performance rationale (VirtioFS is slow for cargo-scale
  builds) for why macOS stays bare-metal-first indefinitely if needed.
- **The daemon stays on the host; the container is the execution
  environment** — this reaffirms, unchanged, the shape decision
  `docker/worker/README.md` already recorded from #5325: `loom-daemon` is
  never PID 1 inside a worker container, precisely because that would fork
  the restart-safety contract this ADR's next section specifies. Containment
  adds *what a sweep or session runs inside*; it does not move *where the
  daemon runs*.
- **The #5119 restart-safety/drain contract must hold for per-sweep
  containers exactly as it holds for the bare-metal cgroup case it was
  written for** — specified normatively below, since this is the one place a
  container boundary is genuinely a new failure surface, not a reaffirmation
  of an existing one.

#### The #5119 drain interaction, specified

Today, #5119 governs what happens to a **bare-metal** sweep child (a process
in the daemon's own systemd cgroup, or a launchd-reparented orphan) across a
daemon restart. A per-sweep *container* is a new kind of child with its own
teardown mechanics, and this ADR settles how the existing contract extends to
it rather than leaving a gap for Phase 3 to discover live.

**`loom-daemon restart --drain` (the scheduled drain-and-restart primitive,
#4090/#5119).** No change to the primitive's semantics; per-sweep containers
are admitted into the *same* in-flight accounting `--drain` already polls to
zero before rolling:

- A drain sets the daemon-global drain flag, which — exactly as it already
  does for bare-metal dispatch — stops the work finder, epic supervisor, and
  role runner from **starting** new work, and (per #5340) refuses new
  `DispatchSweep` requests too. A containerized sweep that is already running
  is an *in-flight sweep* like any other: the drain supervisor's cross-root
  polling loop counts it, and the restart does not proceed until that count
  reaches zero (or `--force-after-timeout` cancels it explicitly).
  Concretely, "cancel" for a containerized sweep means the daemon runs the
  same `cancel_sweep` path it already runs for a bare-metal child, and the
  container's own teardown (`docker stop`/`docker rm`) is invoked as part of
  that cancellation, not left to leak as an orphaned container after the
  cancel returns.
- Because the drain waits for the container's sweep to *finish* (or be
  explicitly cancelled) before the daemon exits, the daemon's own restart —
  whether the host is launchd or systemd — never races the container's
  lifecycle: the container is stopped and removed by the completing (or
  cancelled) sweep itself, before the daemon process that dispatched it goes
  away. No new drain-phase or event-bus topic is needed; the container is
  additional *plumbing* inside "how a sweep runs", not a new category the
  drain state machine has to reason about.

**A hard stop — SIGKILL of the daemon, a `docker restart`/host reboot that
tears down containers directly, or any other non-drained teardown.** This is
where a containerized sweep's failure mode **differs from, and is strictly
better isolated than, the bare-metal case**:

- On the **systemd bare-metal path**, #5119's finding is that a plain
  stop/restart SIGKILLs in-flight sweep children because they live *inside
  the daemon's own service cgroup* — the daemon's stop job reaps them as a
  side effect of reaping itself, with no boundary between "the daemon
  process" and "the work it dispatched."
- A **per-sweep container is its own cgroup, managed by the container
  runtime, not the daemon's systemd unit** — a hard-killed daemon process
  does **not**, by itself, stop a running container the way a hard-killed
  bare-metal sweep dies with its parent's cgroup. The container keeps running
  after the daemon that dispatched it is gone, exactly as a launchd-orphaned
  bare-metal sweep keeps running today (the "sweeps survive by design"
  launchd behavior in `.loom/docs/daemon-reference.md`) — except now this
  property holds on **systemd too**, because the container boundary, not the
  supervisor's process-reparenting behavior, is what decouples the child's
  lifetime from the daemon's.
- The container becomes an **orphan** relative to the relaunched daemon's
  in-memory registry — identical in kind to the pre-#4090 bare-metal orphan
  problem `daemon-reference.md`'s "Amended by #4090" note already describes.
  `SweepRegistry::reconstruct` must be extended (Phase 3 implementation
  scope, not this ADR) to recognize a live, docker-labeled sweep container on
  startup the same way it re-admits a live-lock-owning bare-metal process
  today, so a hard-stop-and-restart does not silently duplicate work by
  re-dispatching an issue whose container is still running.
- **A `docker restart`/host reboot that tears the container down directly**
  (as opposed to a daemon-side hard stop that merely orphans it) genuinely
  kills the sweep's work, the same as any other unplanned host-level
  destructive event — this is not a new gap containment introduces; it is
  the same "an operator or the platform destroyed the machine" case that
  already applies to bare-metal work on host reboot.

**Net specification**: a per-sweep container converts what is, on systemd
bare-metal today, a **hard failure mode (guaranteed SIGKILL on any non-drained
stop, #5119's headline finding)** into a **soft failure mode (orphaned-but-
surviving, on both supervisors)** — strictly improving the failure surface
`--drain` exists to avoid, while `--drain` itself remains the *recommended*
path on both supervisors because it also avoids the registry-reconciliation
work an orphan requires. This is a Phase 3 implementation obligation (the
`SweepRegistry::reconstruct` extension above), not something this ADR
implements; it is specified here so the drain contract's coverage is settled
before Phase 3 starts building against it.

**Consequences.**

*Positive*: the rollout posture is conservative by construction (soak before
default, bare-metal always available where containment does not fit), and
the drain interaction is a net safety *improvement* over the bare-metal
systemd case it extends, not a new risk the epic introduces. `docker/worker/
README.md`'s existing shape decision needs zero revision — this ADR is fully
consistent with it, and cites it as the reason the daemon-as-PID-1
alternative was already rejected before this epic existed.

*Negative*: "soak before default" and the `SweepRegistry::reconstruct`
container-recognition extension are both real, sequenced engineering work
this ADR does not itself complete — a reader following this ADR into Phase 3
should expect to build the reconstruction logic, not find it already done.
Two rollout postures (Linux fleet default vs. everywhere-else opt-in) is also
a second configuration axis operators must reason about, layered on top of
the existing `runtimes.default`/`runtimes.roles` axis this ADR does not
change.

**Rejected alternative: containment as a permanent opt-in, never a default.**
Simpler to reason about (every host's behavior is exactly what its config
says, forever) but does not solve the actual incident class (#5979, #4903):
an opt-in feature that operators must remember to turn on per-host does not
protect a host whose operator has not yet turned it on, which is precisely
the host most likely to be under-provisioned and saturate. The soak-then-
default posture accepts the extra rollout complexity because the alternative
leaves the saturation incident class unaddressed by default on exactly the
hosts most likely to hit it.

## Consequences

Summarized across all four decisions (see each decision's own
Positive/Negative above for the full detail):

### Positive

- Codex's structural auth-persistence blocker (epic #4489) gets a real fix —
  one owning process per account, with a documented interactive-recovery
  path — instead of remaining permanently stalled.
- The fleet's host-saturation incident class (#5979, #4903) gets a
  containment mechanism with real resource ceilings, rolled out
  conservatively (soak, then default, Linux-fleet-scoped).
- Nested compute gets a clean security story (no docker socket in any worker
  container) and, as a side effect, gives #3979's elastic-compute proposal
  its first concrete consumer.
- Every decision routes through existing seams (`spawn-worker.sh`, the
  runtime-adapter contract, `docker/worker/README.md`'s shape decision, the
  #5119 drain primitive) rather than inventing new ones — this ADR settles
  *how* those seams extend, not a parallel architecture beside them.
- The systemd drain gap #5119 documents for bare-metal sweeps is *narrowed*,
  not widened, by per-sweep containers (soft-orphan instead of hard-SIGKILL
  on a non-drained stop).

### Negative

- Two container lifetimes, a new lifecycle CLI (`accounts session …`), a new
  `run-job` seam, and a new rollout-posture axis are all real, ongoing
  operational surface this epic adds — this ADR does not reduce Loom's
  operational complexity, it grows it in exchange for solving the auth and
  saturation problems.
- `SweepRegistry::reconstruct`'s container-recognition extension (specified
  above, not implemented here) is a concrete piece of Phase 3 engineering a
  reader must not assume already exists.
- Claude-only bare installs must continue to behave identically to today —
  this is a hard constraint every later phase inherits from this ADR, not
  optional scope.

## Alternatives Considered

Each decision area's rejected alternative is recorded inline above (single
lifetime for everything; TUI-driven dispatch via `tmux send-keys`; docker.sock
passthrough; docker-in-docker; permanent opt-in rollout). At the whole-epic
level, one further alternative was considered and rejected:

**Do nothing — leave epic #4489 stalled and accept the saturation incident
class as a recurring operational cost.** Rejected because both problems are
structural, not incidental: Codex's `auth.json` refresh chain has no
headless recovery path by design (an OpenAI CLI property, not a Loom gap
Loom can close without a persistence boundary), and unbounded concurrent
sweep dispatch on a single host has already produced multiple dated
incidents (#5979, #4903) with no ceiling in sight as token pools and queue
depth both grow. A session/containment boundary is the mechanism this ADR
commits to instead of continuing to absorb both costs indefinitely.

## References

- Epic **#6896** — Session containers: persistent Codex auth, mandatory
  worker containment, and a remote-execution job seam (the parent epic this
  ADR's Phase 1 belongs to).
- **#4489** — multi-account Codex daemon deployment, stalled on the
  auth-persistence gap this ADR's Decision 1 resolves.
- **#3979** — elastic compute (executor *placement*); this ADR's Decision 3
  supplies the concrete `run-job` consumer #3979 has been waiting for, while
  autoscaling of executor hosts stays in #3979's own scope.
- **#5119** — `loom-daemon restart` post-restart relaunch verification +
  self-heal, and the systemd drain-vs-hard-stop contract this ADR's
  Decision 4 extends to per-sweep containers. See
  [`.loom/docs/daemon-reference.md`](../../.loom/docs/daemon-reference.md)
  §"Supervisor difference — on systemd, a plain stop/restart KILLS sweeps
  (#5119)" and §"Scheduled drain-and-restart (`--drain`, #4090)".
- **#6898** — Container mount contract: path parity, worktree-correctness
  test, secrets and build-cache placement (Phase 1 sibling issue; the
  filesystem contract every mount referenced in this ADR's decisions is
  specified against).
- **#6899** — `loom-worker-session` image layer (Phase 1 sibling issue; the
  image the session containers in Decision 1 and 2 run).
- **#5325** / [`docker/worker/README.md`](../../docker/worker/README.md) —
  the shipped `loom-worker` base image and its shape decision (daemon stays
  on host; container is the execution environment), reaffirmed unchanged by
  this ADR's Decision 4.
- **#5979, #4903** — the host-saturation incident class this ADR's Decision 4
  containment posture addresses.
- Related ADRs: [ADR-0012](0012-runtime-adapter-contract.md) (the runtime
  adapter contract both container lifetimes dispatch through unchanged),
  [ADR-0010](0010-daemon-rebuild.md) (the Rust daemon whose restart/drain
  primitives Decision 4 extends).
- Contract specification: [`.loom/docs/runtime-adapters.md`](../../.loom/docs/runtime-adapters.md)
