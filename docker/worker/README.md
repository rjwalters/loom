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
  or domain-specific compiler/simulator. (`python3` is present as a bare
  interpreter only, because 2am's managed `gh` launcher is a Python 3 script
  (#9987); a policy-governed dispatch refuses an image without it.) Per-repo build-gate
  toolchains are a downstream layer's job (this is the "generic worker
  mechanism" the issue's owner decision describes; domain toolchains build
  `FROM` this image, they do not live in it).
- **No `loom-daemon` lifecycle.** No systemd, no supervisor, no `ENTRYPOINT`
  that starts the daemon. See the shape decision above.
- **No secrets, tokens, or identity of any kind.**

## Bootstrap seams (mounts, not baked content)

Everything host-specific arrives at `docker run` time. This table is a quick
summary for standalone use (a one-off container against an ad-hoc checkout);
[**`MOUNT-CONTRACT.md`**](MOUNT-CONTRACT.md) is the normative contract —
covering path parity (load-bearing for git worktrees), the full secrets-mount
policy, uid/gid mapping, and build-cache placement — that Loom-managed
dispatch (fleet workers, epic #6896's session containers) MUST follow. Where
the two differ, `MOUNT-CONTRACT.md` wins.

| Path | Contents | How it arrives |
|---|---|---|
| `/workspace` (standalone) or the parity-mounted workspace root (Loom-managed dispatch — see `MOUNT-CONTRACT.md` § "Path parity") | A repo checkout / git worktree | Bind mount, e.g. `-v "$PWD:/workspace"` (standalone) or `-v "<host-abs-path>:<same-abs-path>"` (parity) |
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

## Forge egress boundary (`enforcement.api = required`, #9989)

Under a machine/env forge-egress policy with `enforcement.api = required`, a
Loom-dispatched worker container must not be able to reach the GitHub API
directly — only through the managed `gh` launcher and the policy's
`github.apiOrigin`. Loom owns this boundary; it installs **no host firewall
rules** (bare-metal host policy is 2am#1931).

**Mechanism (Docker): a netns-holding sidecar.** `--add-host` only changes name
resolution, and a `NET_ADMIN` worker could delete its own rules. So before the
worker starts, Loom (`loom-daemon forge egress container-network`, called from
`spawn-claude.sh`'s `container-args` decision point; the native path calls the
same code from `containment.rs`) starts a short-lived sidecar from the worker
image (override: `LOOM_EGRESS_SIDECAR_IMAGE`; it needs `iptables`, `ip6tables`,
`getent`, `awk` — this image installs `iptables`) as `--user 0:0` (iptables
needs uid 0 even with `NET_ADMIN`; the image's non-root `USER` would be refused)
with `NET_ADMIN`/`NET_RAW`
only, and the worker, which keeps the image user, joins its network namespace with `--network
container:<sidecar>`, holding no network capability. The sidecar:

- resolves `api.github.com` and `uploads.github.com` over **IPv4 and IPv6**
  and atomically replaces a dedicated `OUTPUT` chain (`iptables-restore
  --noflush`): TLS-SNI string match for the blocked hosts (skipped, never
  fatal, if the kernel lacks `xt_string`), `ACCEPT` for the addresses of
  `github.apiOrigin` and `github.com` (git transport), `REJECT --reject-with
  tcp-reset` for the blocked hosts' addresses;
- **re-resolves every 30 s** (the CDN rotates addresses; a launch-time snapshot
  is not a boundary) and swaps the chain atomically;
- is removed by a host-side reaper when the worker's `docker run` client exits
  (label `loom.egress-sidecar=<name>` finds any stray: `docker ps -a --filter
  label=loom.egress-sidecar`).

Because the worker shares the sidecar's namespace, docker refuses
`--add-host`/`--hostname`/`-p`/`--dns` on the worker; the host-gateway mapping
the credential proxy needs is applied to the sidecar instead.

**Negative canary and abort path.** Before the agent starts, Loom runs
`enforcement.negativeCanary` (default `curl -sS --max-time 5
https://api.github.com/zen`) in a throwaway, capability-free container joined
to the same namespace. Only env/machine-owned policies may name a canary. It
is classified through the one C1 classifier (`forge_egress::checks::assert_runtime`):

| Canary | Finding | Under `required` |
|---|---|---|
| request refused/reset **and** the allowed gateway answers a positive probe | none: logged `runtime.verified` (from the canary, never config) | worker starts |
| request succeeds | `runtime.bypass-open` | spawn aborted (exit 78), sidecar removed |
| cannot run (no `curl`, timeout, DNS/TLS/usage error), gateway unreachable, rules not installed (incl. blocked hosts that do not resolve), or a `required` policy that can no longer be read | `runtime.unverifiable` | spawn aborted (exit 78) |

With no policy, or `observe`, none of this runs and the `docker run` argv is
byte-identical to before. A bare-metal host keeps `runtime.unverifiable` in
`loom-daemon forge egress doctor` (exit 2) until 2am's host policy sets
`enforcement.runtimeEgress` and a `negativeCanary`; Loom never claims host
enforcement it did not prove. Tests: `cargo test -p loom-daemon egress_policy`;
the real-Docker matrix (`docker_boundary_blocks_api_and_keeps_git_transport`)
skips without Docker and runs when `LOOM_EGRESS_DOCKER_TEST_IMAGE` names an
image with `iptables` and `curl`.

## Building and testing locally

The Dockerfile expects a pre-built Linux `loom-daemon` release binary in the
build context, staged as `dist/loom-daemon-linux-<arch>` (`amd64`/`arm64` —
Docker's own `TARGETARCH` naming, which the `LOOM_DAEMON_BIN` build-arg
defaults against) rather than rebuilding it from source — this keeps the
image build fast and makes the image ship the *exact, already-tested*
release artifact instead of a second, divergent build of the same commit.
`.github/workflows/release.yml`'s `build-daemon` job produces the underlying
binaries under Rust target-triple names
(`loom-daemon-x86_64-unknown-linux-gnu` /
`loom-daemon-aarch64-unknown-linux-gnu`); the release workflow itself stages
them under the `linux-<arch>` naming before invoking `docker build`/`buildx
build` — a local build does the same staging step by hand:

```bash
# From the repo root — substitute aarch64-unknown-linux-gnu/linux-arm64 on
# an Apple Silicon / arm64 host:
cargo build --release -p loom-daemon --target x86_64-unknown-linux-gnu
mkdir -p dist
cp target/x86_64-unknown-linux-gnu/release/loom-daemon dist/loom-daemon-linux-amd64

docker build -f docker/worker/Dockerfile -t loom-worker:dev .
./docker/worker/test-image.sh loom-worker:dev
```

### apt resilience and `APT_MIRROR` (#10822)

The image writes `/etc/apt/apt.conf.d/80-loom-retries` (`Acquire::Retries
"5"`, 30 s http/https timeouts) before its first `apt-get`, so one stalled
fetch is retried instead of hanging the build. The file stays in the
published image, where it only affects a downstream layer's own `apt-get`.

| Build arg | Default | Effect |
|-----------|---------|--------|
| `APT_MIRROR` | empty (upstream `archive.ubuntu.com`) | When set, rewrites the `archive.ubuntu.com` URIs in `/etc/apt/sources.list.d/ubuntu.sources` to this mirror. `security.ubuntu.com` and `cli.github.com` are untouched. Release builds do not set it, so the published image keeps upstream sources. |

CI (`.github/workflows/ci.yml` job `worker-base-image`, and `ci-daily.yml`)
passes the GitHub runners' Azure mirror:

```bash
docker build -f docker/worker/Dockerfile \
  --build-arg APT_MIRROR=http://azure.archive.ubuntu.com/ubuntu \
  -t loom-worker:dev .
```

In `ci.yml` the base image is built once per run by `worker-base-image`, with
a registry layer cache at `ghcr.io/rjwalters/loom-worker:buildcache` (read on
every run, written only from pushes to `main`); the three image smokes load
that build instead of rebuilding it. `:buildcache` is a BuildKit cache
manifest, not a runnable image -- do not `docker pull` it.

## Versioning and publishing

`.github/workflows/release.yml` builds and pushes a multi-arch
(`linux/amd64` + `linux/arm64`) manifest for
`ghcr.io/rjwalters/loom-worker:<version>` and `:latest` on every GitHub
Release, using the `x86_64-unknown-linux-gnu` and `aarch64-unknown-linux-gnu`
binaries its own `build-daemon` job already built and checksummed for that
release — `<version>` is read from `scripts/version.sh` at the released
commit, so it is always exactly the loom version the image's `loom-daemon`
binary reports via `--version`.

Both `linux/amd64` and `linux/arm64` are published under the same tags
(verify post-release with `docker manifest inspect
ghcr.io/rjwalters/loom-worker:<version>`). CI's own smoke test
(`docker/worker/test-image.sh`) only runs natively against the `linux/amd64`
leg — there is no native arm64 GitHub Actions runner in this repo, so the
`linux/arm64` leg is built and pushed (via QEMU emulation) without its own CI
smoke test; verify an arm64 build/run manually on an Apple Silicon or other
arm64 host if you need to validate that leg end to end.
