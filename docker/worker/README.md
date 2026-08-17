# `loom-worker` base image

`ghcr.io/rjwalters/loom-worker:<version>` (+ `:latest`) is a pinned OCI base
image for Loom fleet workers, published by `.github/workflows/release.yml`
from the same tag as the `loom-daemon` release binaries — `<version>` always
matches the loom version those binaries ship at, so the image and the daemon
binary it contains version in lockstep by construction.

## Shape decision: sweep-execution environment, not daemon-as-PID-1

Filed in #5325, this is the recorded answer to the question the issue asked
to settle first.

**The container is not the worker. `loom-daemon` stays on the host.** This
image is the pinned environment a sweep or build-gate *executes inside* —
the runtime-adapter seam (`spawn-worker.sh` → `spawn-<runtime>.sh`,
[`.loom/docs/runtime-adapters.md`](../../.loom/docs/runtime-adapters.md))
already treats "how the worker CLI is launched" as swappable; this image
makes "what environment it launches into" swappable and reproducible the
same way. A dispatcher (bare-metal today; `docker run` wrapping this image
tomorrow) invokes `spawn-worker.sh` the same way either way — the seam needed
no change to admit this.

The alternative shape — `loom-daemon` running as PID 1 inside the container,
foreground, no systemd — was rejected for one concrete, already-documented
reason: **it forks the fleet's restart-safety contract (#5119)**.

> On a `systemd --user`-supervised host, sweep/role children run **inside the
> daemon's own service cgroup**, so a plain stop/restart SIGKILLs them
> (`KillMode=mixed`) unless the daemon does an explicit `restart --drain`
> first (see [`daemon-reference.md`](../../.loom/docs/daemon-reference.md)
> around "Supervisor difference — on systemd, a plain stop/restart KILLS
> sweeps (#5119)"). `fleet add-worker --safehouse`'s systemd-unit assumptions
> (`daemon-unit` step: `Restart=on-success`, `LOOM_DAEMON_SUPERVISOR=systemd`,
> the `--drain` contract) are built entirely around that boundary.

A daemon-as-PID-1 container has **no cgroup boundary "for free" the way the
host unit does** — a container runtime's own restart/kill semantics
(`docker restart`, a Kubernetes pod eviction, …) would need an equivalent
explicit "finish in-flight sweeps before the container is torn down" step
re-implemented from scratch, with no existing `--drain` primitive to reuse
and a genuinely different failure surface (SIGTERM timing, PID-1 zombie
reaping, no `systemctl --user is-active` equivalent to detect the
supervisor). None of that problem exists here: the daemon's systemd unit,
its `restart --drain` contract, and #5119's fix are completely untouched by
this image, because this image never runs the daemon at all.

**What this shape does not solve** (by design — tracked separately): making
the worker *host* itself a reproducible artifact (the AMI, not the
container) remains the 2AM umbrella repo's job. This image is one input to
that host, not a replacement for it.

## What this image guarantees (the `FROM` contract)

A downstream image (e.g. klayout-tools' EDA sim overlay) that does
`FROM ghcr.io/rjwalters/loom-worker:<version>` can rely on:

| Guarantee | Detail |
|---|---|
| Base OS | Ubuntu 24.04 LTS, pinned (not `ubuntu:latest`) |
| `loom-daemon` | The release binary for this exact version, on `PATH` at `/usr/local/bin/loom-daemon`, smoke-tested at build time (`loom-daemon --version`) |
| Claude Code CLI | Installed via the same `curl -fsSL https://claude.ai/install.sh \| bash` installer the fleet `add_worker` plan uses (with `--retry 5 --retry-all-errors` for transient upstream 4xx/5xx), on `PATH` at `/home/loom/.local/bin/claude`, verified at build time |
| Core toolchain | `git`, `gh`, `jq`, `tmux`, `curl`, `ca-certificates`, `openssh-client`, plus the C toolchain (`build-essential`, `pkg-config`, `libssl-dev`, `libsqlite3-dev`) the fleet's `base-deps` step installs |
| Default user | Non-root `loom` (uid/gid `1000`), `HOME=/home/loom` |
| Default `WORKDIR` | `/workspace` — the expected repo-checkout mount point |
| Build `SHELL` | `/bin/bash -o pipefail -c` (#6409). Docker persists `SHELL` into the image config, so a downstream `FROM` layer's own `RUN` steps inherit it: a failing `curl … \| sh` there fails the build instead of silently producing a broken layer. Override per-stage with your own `SHELL` instruction if you need `/bin/sh -c` back. Runtime is unaffected (the exec-form `CMD` and any `docker run` command do not go through `SHELL`). |
| Secrets | **Zero.** No token, credential, PAT, or account file is copied, generated, or referenced anywhere in the build. Verified by `docker/worker/test-image.sh`'s `docker history` scan. |

## What this image deliberately does NOT include

- **No language/build toolchain beyond the C basics above** — no Rust, Node,
  Python, or domain-specific compiler/simulator. Per-repo build-gate
  toolchains are a downstream layer's job (this is the "generic worker
  mechanism" the issue's owner decision describes; domain toolchains build
  `FROM` this image, they do not live in it).
- **No `loom-daemon` lifecycle.** No systemd, no supervisor, no `ENTRYPOINT`
  that starts the daemon. See the shape decision above.
- **No secrets, tokens, or identity of any kind.**

## Bootstrap seams (mounts, not baked content)

Everything host-specific arrives at `docker run` time:

| Path | Contents | How it arrives |
|---|---|---|
| `/workspace` | A repo checkout / git worktree | Bind mount, e.g. `-v "$PWD:/workspace"` |
| `/home/loom/.loom/tokens` | The token pool | Bind mount, read-only, e.g. `-v "$HOME/.loom/tokens:/home/loom/.loom/tokens:ro"` |
| `gh`/git forge auth | A PAT or `gh auth login` state | Bind mount `~/.config/gh`, or `GH_TOKEN`/`GITHUB_TOKEN` env at `docker run` |

Example dispatch, mirroring what a bare-metal fleet worker's `spawn-worker.sh`
invocation already does:

```bash
docker run --rm \
  -v "$PWD:/workspace" \
  -v "$HOME/.loom/tokens:/home/loom/.loom/tokens:ro" \
  -e CLAUDE_CODE_OAUTH_TOKEN \
  ghcr.io/rjwalters/loom-worker:<version> \
  .loom/scripts/spawn-worker.sh -p "/loom:sweep 123" --dangerously-skip-permissions
```

## Building and testing locally

The Dockerfile expects a pre-built Linux `loom-daemon` release binary in the
build context (the same `dist/loom-daemon-<target>` layout
`.github/workflows/release.yml`'s `build-daemon` job already produces) rather
than rebuilding it from source — this keeps the image build fast and makes
the image ship the *exact, already-tested* release artifact instead of a
second, divergent build of the same commit.

```bash
# From the repo root:
cargo build --release -p loom-daemon --target x86_64-unknown-linux-gnu
mkdir -p dist
cp target/x86_64-unknown-linux-gnu/release/loom-daemon dist/loom-daemon-x86_64-unknown-linux-gnu

docker build -f docker/worker/Dockerfile -t loom-worker:dev .
./docker/worker/test-image.sh loom-worker:dev
```

## Versioning and publishing

`.github/workflows/release.yml` builds and pushes
`ghcr.io/rjwalters/loom-worker:<version>` and `:latest` on every GitHub
Release, using the `x86_64-unknown-linux-gnu` binary its own `build-daemon`
job already built and checksummed for that release — `<version>` is read
from `scripts/version.sh` at the released commit, so it is always exactly
the loom version the image's `loom-daemon` binary reports via `--version`.

Only `linux/amd64` is published today (the platform every observed fleet
worker host runs, including the loom-worker-2 provisioning incident this
issue was filed from). Multi-arch (`linux/arm64`, matching the
`aarch64-unknown-linux-gnu` binary the release workflow already builds) is a
reasonable follow-up but is out of scope here — it needs a buildx/QEMU cross
-build leg this change does not add.
