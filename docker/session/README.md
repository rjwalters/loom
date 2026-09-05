# `loom-worker-session` image

`ghcr.io/rjwalters/loom-worker-session:<version>` (+ `:latest`) is the
**session-capable** image layer Epic #6896 Phase 2's per-account persistent
Codex session containers run — published by `.github/workflows/release.yml`
FROM the same-version `ghcr.io/rjwalters/loom-worker:<version>` base image, so
the two version in lockstep by construction: `<version>` always matches the
loom version both images ship at.

Full architecture context: **ADR-0017**,
[`docs/adr/0017-session-container-architecture.md`](../../docs/adr/0017-session-container-architecture.md)
— read it first if the "why" below is unclear. This README documents the
"what" for someone building, running, or debugging this specific image.

## What this image adds on top of `loom-worker`, and nothing else

The base image's own `FROM` contract (Ubuntu 24.04, `loom-daemon`, Claude Code
CLI, `git`/`gh`/`jq`/`tmux`/build-essential, non-root `loom` user uid/gid
`1000`, `/workspace`) is unchanged — see
[`docker/worker/README.md`](../worker/README.md) for that contract in full.
This layer adds exactly three things:

| Addition | Detail |
|---|---|
| OpenAI Codex CLI | `@openai/codex` installed via npm, pinned to a specific version (`CODEX_VERSION` build arg), version-checked at build time against the runtime-adapter floor — `codex >= 0.146.0` per [`.loom/docs/runtime-adapters.md`](../../.loom/docs/runtime-adapters.md). A pin bump that regresses below the floor fails the build. |
| Node.js + npm | Codex CLI's only distribution channel is npm, so a Node runtime is a genuine transitive dependency — installed from the official upstream tarball (checksum-verified), not `apt-get install nodejs npm`, which pulls Ubuntu's entire Debian-packaged build-from-source dependency chain for a footprint this image never uses. |
| tmux-server entrypoint | `tini` (PID 1) → a thin entrypoint script that starts (or resumes) a detached tmux session named `session`, then blocks forever. This is what makes the container *persistent* — see "Entrypoint behavior" below. |
| `CODEX_HOME` convention | `ENV CODEX_HOME=/home/loom/.codex-profile` — an empty, read-write directory owned by uid `1000` at build time. A per-account Codex profile volume binds over this path at container-start time (Phase 2's session lifecycle); it is never baked with content. |

**Zero secrets in this image**, same guarantee as the base image — verified
by [`test-image.sh`](test-image.sh)'s `docker history` scan (extended with
Codex/`CODEX_HOME`-adjacent patterns on top of the base image's own set).

## Entrypoint behavior

```
ENTRYPOINT ["/usr/bin/tini", "--", "/home/loom/.local/bin/loom-session-entrypoint.sh"]
CMD []
```

`tini` is PID 1: a minimal, well-tested init that forwards signals
(`SIGTERM`/`SIGINT` from `docker stop`) and reaps every zombie process
reparented to it — including anything a `docker exec`'d command leaves
behind. This is the "equivalent thin init that reaps zombies" ADR-0017
Decision 2 calls for as the alternative to a hand-rolled reaper loop.

Its one child, [`entrypoint.sh`](entrypoint.sh), does exactly two things:

1. Start a detached tmux session named `session` (`$LOOM_SESSION_TMUX_NAME`
   overrides the name) if one is not already running.
2. Block forever (`exec sleep infinity`).

**No daemon, no supervisor, no auto-start of any agent.** The container
idles at ~zero cost until something execs work into it. This image never
prescribes "the container IS the worker" any more than `loom-worker` does —
it just adds a persistence boundary the base image has no reason to need.

## Two ways to interact with a running container

Both are ordinary `docker exec` — there is no separate control plane.

**Headless dispatch** (ADR-0017 Decision 2 — the normal path, what
Phase 2's session lifecycle CLI actually runs):

```bash
docker exec <container> codex exec "do the thing"
```

Exit codes, stdout/stderr, and the Codex `exec` transcript are exactly what
they would be running `codex exec` directly on a bare-metal host — nothing
about running inside this container changes `classify-error.sh`'s exit-code
handling or the runtime-adapter contract's usage accounting.

**Interactive re-login / inspection** (operator-only, rare — e.g. after a
dead Codex refresh chain requires an interactive `codex login`):

```bash
docker exec -it <container> tmux attach -t session
# ... interact, run `codex login`, etc. ...
# detach without killing the session: Ctrl-b d
```

Explicitly **not** a dispatch mechanism: nothing in the normal dispatch path
ever writes to this tmux session or scrapes its pane output. Driving work
through `tmux send-keys` was considered and rejected — see ADR-0017
Decision 2's "Rejected alternative" for the full reasoning (no real exit
code, no structured transcript, a second bespoke scraper).

## `CODEX_HOME` mount contract

```
ENV CODEX_HOME=/home/loom/.codex-profile
```

Empty at build time — a mount point, not baked content, following the same
pattern the base image already uses for `/home/loom/.loom/tokens`. A
per-account Codex profile volume (owning that account's `auth.json` refresh
chain — see ADR-0017 Decision 1) binds over this path at container-start
time:

```bash
docker run -d --name codex-session-<account> \
  -v "/path/to/codex-profiles/<account>:/home/loom/.codex-profile" \
  ghcr.io/rjwalters/loom-worker-session:<version>
```

The volume, not the image, is what makes a session container
account-specific — the image itself carries no account identity. Owning the
volume's lifecycle (creation, backup, session start/stop/status/attach
tooling) is Phase 2 scope, not this image's — shipped as `loom-daemon accounts
session start|stop|status|attach <name>` (issue #6925), layered on the
existing `loom-daemon accounts` profile store
(`loom-daemon/src/tokens_pool/session_lifecycle.rs`). Once a profile is
adopted by `session start`, it refuses further host-direct `CODEX_HOME` use
(`accounts reauth`/`status` on that profile) — the container is the sole
process allowed to touch the volume from then on.

## Building and testing locally

Like the base image, this Dockerfile expects the base image to already exist
(locally or in a registry) rather than rebuilding it as part of the same
`docker build` — `BASE_IMAGE` is a plain build arg naming whatever tag you
want to build FROM:

```bash
# From the repo root, having already built (or pulled) a loom-worker image:
docker build -f docker/worker/Dockerfile -t loom-worker:dev .

docker build -f docker/session/Dockerfile \
  --build-arg BASE_IMAGE=loom-worker:dev \
  -t loom-worker-session:dev .

./docker/session/test-image.sh loom-worker-session:dev
```

`BASE_IMAGE` defaults to `ghcr.io/rjwalters/loom-worker:latest` if omitted,
for a quick pull-and-build without a local base image.

Manual smoke check beyond what `test-image.sh` automates:

```bash
docker run -d --name session-dev loom-worker-session:dev
docker exec session-dev tmux has-session -t session   # exit 0 = live
docker exec session-dev codex --version
docker exec -it session-dev tmux attach -t session    # Ctrl-b d to detach
docker rm -f session-dev
```

## Versioning and publishing

`.github/workflows/release.yml`'s `build-session-image` job builds and
pushes `ghcr.io/rjwalters/loom-worker-session:<version>` and `:latest` on
every GitHub Release, immediately after (and depending on) the
`build-worker-image` job — `<version>` is the same `scripts/version.sh`
value the base image and every other version-bearing file in the repo share,
passed as `--build-arg BASE_IMAGE=ghcr.io/rjwalters/loom-worker:<version>` so
the pair is always built against each other's exact matching version, never
`:latest`-to-`:latest` drift.

Only `linux/amd64` is published today, mirroring `docker/worker/README.md`'s
own "Versioning and publishing" scope note — multi-arch is a reasonable
follow-up, not this image's initial scope.
