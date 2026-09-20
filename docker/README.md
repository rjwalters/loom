# `docker/`

This directory holds the Docker build contexts for Loom's worker images. Shape
decision, common to all of them: **`loom-daemon` stays on the host** — every
image published from here is a pinned sweep-execution *environment* a worker
runs inside, never a containerized daemon.

| Directory | Image | What it is |
|---|---|---|
| [`worker/`](worker/README.md) | `ghcr.io/rjwalters/loom-worker` | The base image. Ubuntu LTS + `loom-daemon` + Claude Code CLI + the `git`/`gh`/`jq` toolchain the loom scripts assume + a non-root `loom` user. Everything else here is a `FROM` layer on it. Read this README first for the full `FROM` contract, bootstrap seams, and the mount contract. |
| [`session/`](session/README.md) | `ghcr.io/rjwalters/loom-worker-session` | The **persistent** per-account session layer (Codex): adds the Codex CLI and a tmux-server entrypoint. Serves ADR-0017's session-container lifetime, for a runtime whose credential is a mutable OAuth refresh chain needing one owning process. |
| [`native/`](native/README.md) | `ghcr.io/rjwalters/loom-worker-native` | The **ephemeral** per-sweep native-harness layer (Pi / OpenCode): adds the two CLIs, pinned to the versions the guardrail-parity doc records as tested. Serves ADR-0017's ephemeral lifetime, for API-key credentials that have no refresh chain to own. |

The two overlay layers differ by **container lifetime**, not by which CLI they
install — see [`native/README.md`](native/README.md) § "Why a third image, and
not `loom-worker-session`" if that distinction is not obvious.

All three are published by `.github/workflows/release.yml`, version-locked to
each other and to the `loom-daemon` binary they ship.
