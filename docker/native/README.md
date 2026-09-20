# `loom-worker-native` image

`ghcr.io/rjwalters/loom-worker-native:<version>` (+ `:latest`) is the image
that **per-sweep ephemeral containers for native-harness (Pi / OpenCode)
sweeps** run in — published by `.github/workflows/release.yml` FROM the
same-version `ghcr.io/rjwalters/loom-worker:<version>` base image, so the two
version in lockstep by construction, exactly like
[`loom-worker-session`](../session/README.md) does.

Filed as issue #8403 (epic #6896 Phase 3). Full architecture context:
**ADR-0017**,
[`docs/adr/0017-session-container-architecture.md`](../../docs/adr/0017-session-container-architecture.md).

## Why a third image, and not `loom-worker-session`

ADR-0017 specifies **two container lifetimes**, not two CLIs:

| Lifetime | Image | Runtime | Why that lifetime |
|---|---|---|---|
| Per-account, **persistent** | `loom-worker-session` | Codex | `CODEX_HOME/auth.json` is a mutable OAuth **refresh chain**. A refresh rotates the stored credential, so exactly one process may own it; a second concurrent owner invalidates the first. Persistence is what gives that chain an owner. |
| Per-sweep, **ephemeral** | `loom-worker` (Claude), **`loom-worker-native`** (Pi/OpenCode) | Claude, Pi, OpenCode | Stateless credential. Nothing on the container's filesystem needs to survive the sweep, so the container is created fresh and destroyed at the end. |

A native harness on an API-key subscription is squarely in the second row:
**an API key has no refresh chain**, so there is nothing for a persistent
container to own. Reusing the session shape would buy no correctness and cost
the two properties the ephemeral shape gives for free — a guaranteed-clean
filesystem per sweep, and a hard teardown at sweep end. The key is injected as
container **env** at `docker run` time and exists nowhere on the container's
filesystem. See [`.loom/docs/runtime-adapters.md`](../../.loom/docs/runtime-adapters.md)
§ "Session containers" for the same reasoning in the runtime-adapter contract's
own terms.

So this image differs from `loom-worker-session` in **lifetime and shape**, not
merely in which CLI it installs — which is why it is a sibling layer rather
than another CLI bolted into that one.

## What this image adds on top of `loom-worker`, and nothing else

The base image's `FROM` contract (Ubuntu 24.04, `loom-daemon`, Claude Code CLI,
`git`/`gh`/`jq`/`tmux`/build-essential, non-root `loom` user uid/gid `1000`,
`/workspace`) is unchanged — see [`docker/worker/README.md`](../worker/README.md).
This layer adds exactly four things:

| Addition | Detail |
|---|---|
| Node.js + npm | npm is the only distribution channel for both native CLIs, and OpenCode additionally installs its own `@opencode-ai/plugin` package into its config directory **at launch time**, so npm must be present at run time too. Installed from the official upstream tarball (checksum-verified), identical pins to `docker/session/Dockerfile`. |
| OpenCode CLI | `opencode-ai`, pinned to `OPENCODE_VERSION` (default `1.18.31`). |
| Pi CLI | `@earendil-works/pi-coding-agent`, pinned to `PI_VERSION` (default `0.85.1`). |
| `OPENCODE_DISABLE_AUTOUPDATE=1` | The runtime half of the pin — a pinned install is not enough if the CLI can update *itself* past the tested version on first launch. |

**The pins are equality-checked at build time, not floor-checked.**
`1.18.31` / `0.85.1` are the exact versions
[`.loom/docs/guardrail-parity-native.md`](../../.loom/docs/guardrail-parity-native.md)
and its [verification receipt](../../.loom/docs/native-runtime-verification-2026-09-19.md)
record as *tested*. A version floor would be the wrong check here: the parity
doc records a tested version, and "newer" is not "verified" — that doc's own
"CLI exit zero is not acceptance evidence" applies to the CLI's own version
just as much as to a sweep's outcome. Bump the pin, the parity doc, and a fresh
canary run together; never one without the others.

**No ENTRYPOINT.** The base image's "a pinned shell a caller runs a command in"
shape is exactly right for an ephemeral dispatch: `worker_spawn::containment`
runs `docker run … <image> <workspace>/.loom/scripts/spawn-worker.sh <args>`
directly. There is no tmux server, no `tini`, no daemon, and no session
persistence of any kind in this image.

**Zero secrets in this image**, same guarantee as the base image — verified by
[`test-image.sh`](test-image.sh)'s `docker history` scan, extended with
native-credential shapes (`ZAI_API_KEY`, `ZHIPU_API_KEY`, a stray `auth.json`)
on top of the base set.

## The ephemeral state root

```
/home/loom/.loom-native/      # empty at build time, owned by uid 1000
```

`loom_daemon::worker_spawn::containment` points every one of
`XDG_DATA_HOME`, `XDG_CONFIG_HOME`, `XDG_CACHE_HOME`, `XDG_STATE_HOME`,
`OPENCODE_CONFIG_DIR`, and `LOOM_NATIVE_TOOLS_DIR` at
`/home/loom/.loom-native/<per-launch-id>/…` inside this tree. That is the fix
for the concrete problem #8403 opens with: uncontained, `XDG_DATA_HOME` is not
relocated per launch, so N concurrent native workers on one host share **one**
`~/.local/share/opencode` session store and **one** `auth.json`, and a
`/connect`-style login by one worker is visible to all.

Contained, two concurrent workers are disjoint twice over — different
containers *and* different paths within them. The per-launch id is not
redundant belt-and-braces: it makes the disjointness inspectable (`docker
exec <id> ls /home/loom/.loom-native`) rather than merely implied by the
container boundary.

This directory is a **writable-layer location, never a mount point**. Nothing
is bind-mounted over it, and nothing in it survives the container.

## Build

```bash
# Build the base image first (the FROM target), then this layer:
cargo build --release -p loom-daemon --target x86_64-unknown-linux-gnu
mkdir -p dist && cp target/x86_64-unknown-linux-gnu/release/loom-daemon dist/loom-daemon-linux-amd64
docker build -f docker/worker/Dockerfile -t loom-worker:dev .
docker build -f docker/native/Dockerfile --build-arg BASE_IMAGE=loom-worker:dev \
  -t loom-worker-native:dev .
./docker/native/test-image.sh loom-worker-native:dev
```

The release workflow does the same thing with
`--build-arg BASE_IMAGE=ghcr.io/rjwalters/loom-worker:<version>`, then pushes
multi-arch (`linux/amd64,linux/arm64`).

## Run

You do not normally run this image by hand — enable native containment and let
the dispatcher do it:

```json
{
  "runtimes": {
    "containment": {
      "native": "ephemeral"
    }
  }
}
```

```bash
LOOM_RUNTIME=opencode loom-daemon spawn-worker -- --profile zai-flash \
  --log .loom/logs/native.log -p '/loom:sweep 123'
```

The dispatcher writes a `# LOOM_DISPATCH_MODE mode=container image=<image>
cpus=<v|none> memory=<v|none> containment=native-ephemeral` marker to that log
before it execs `docker run`, and labels the container
`loom.containment=native-ephemeral` — so `docker ps --filter
label=loom.containment=native-ephemeral` and `loom-daemon status`'s `CTR`
column both name the shape. Details:
[`.loom/docs/runtime-adapters.md`](../../.loom/docs/runtime-adapters.md)
§ "Native-harness ephemeral containment".
