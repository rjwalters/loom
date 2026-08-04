# syntax=docker/dockerfile:1
#
# loom-worker base image (#5325) — a pinned SWEEP-EXECUTION ENVIRONMENT, not a
# containerized loom-daemon.
#
# Shape decision (recorded, see docker/worker/README.md for the full
# rationale): this image does NOT run `loom-daemon` as PID 1. The daemon stays
# on the host; this image is the environment `spawn-worker.sh` dispatches a
# sweep/build-gate INTO (a `docker run` wrapping the runtime-adapter seam),
# exactly the way a bare-metal fleet worker runs one today. That sidesteps the
# #5119 restart-semantics class entirely: #5119 is about systemd's
# `KillMode=mixed` reaping in-flight sweep children when the *daemon's own*
# service cgroup is torn down on restart. There is no daemon lifecycle inside
# this container to have that problem — the daemon's systemd unit (and its
# `restart --drain` contract) is unchanged and untouched by this image.
#
# Contract for downstream `FROM` consumers (e.g. klayout-tools' EDA sim
# overlay): Ubuntu LTS + `loom-daemon` binary + Claude Code CLI + the `git`/
# `gh`/`jq`/build-essential toolchain the loom scripts assume + a non-root
# `loom` user (uid/gid 1000) + `/workspace` as the expected mount point for a
# repo checkout. Zero secrets, zero identity, zero host-specific config are
# baked in — token pools, workspace checkouts, and forge credentials all
# arrive at `docker run` time via volume mounts / env, never at build time.
# See docker/worker/README.md for the full guarantee list and mount contract.

FROM ubuntu:24.04

# The loom-daemon Linux release binary this image ships, relative to the
# BUILD CONTEXT (not this directory) — the release workflow builds this
# context as the repo root so this path lines up with the exact "dist/loom-daemon-<target>"
# convention `.github/workflows/release.yml`'s `build-daemon` job already uses
# for its own release-asset packaging step. Override for a local/manual build,
# e.g. after `cargo build --release -p loom-daemon --target x86_64-unknown-linux-gnu`:
#   mkdir -p dist && cp target/x86_64-unknown-linux-gnu/release/loom-daemon dist/loom-daemon-x86_64-unknown-linux-gnu
#   docker build -f docker/worker/Dockerfile -t loom-worker:dev .
ARG LOOM_DAEMON_BIN=dist/loom-daemon-x86_64-unknown-linux-gnu

# Non-root identity (AC: "non-root user"). Fixed uid/gid so a downstream
# overlay (or a `docker run --user`) can reason about ownership deterministically.
ARG LOOM_UID=1000
ARG LOOM_GID=1000
ARG LOOM_USER=loom

# Build-time only — deliberately ARG, not ENV, so it never leaks into the
# runtime environment of a container started from this image (a downstream
# `apt-get install` in an interactive shell should still get real prompts if
# the caller wants them).
ARG DEBIAN_FRONTEND=noninteractive

# --- Base toolchain -----------------------------------------------------
# Mirrors the fleet `add_worker.rs` "base-deps" step (build-essential,
# pkg-config, libssl-dev, libsqlite3-dev, git, curl, ca-certificates, gh) plus
# two additions the bare AMI plan does NOT install because it assumes an
# already-provisioned host: `jq` (nearly every loom script — config-resolver,
# spawn-worker.sh's runtime resolution, etc. — hard-requires it) and `tmux`
# (loom-attach.sh / loom-start.sh's terminal multiplexer). Deliberately does
# NOT install a Rust/Node/Python toolchain: this base image is language-agnostic
# by design (see README "What this image deliberately does NOT include") —
# per-repo build-gate toolchains are a downstream `FROM` layer's job.
RUN apt-get update -qq \
    && apt-get install -y --no-install-recommends \
        build-essential \
        pkg-config \
        libssl-dev \
        libsqlite3-dev \
        git \
        curl \
        ca-certificates \
        gnupg \
        jq \
        tmux \
        openssh-client \
        unzip \
        less \
    && mkdir -p -m 755 /etc/apt/keyrings \
    && curl -fsSL https://cli.github.com/packages/githubcli-archive-keyring.gpg -o /etc/apt/keyrings/githubcli-archive-keyring.gpg \
    && chmod go+r /etc/apt/keyrings/githubcli-archive-keyring.gpg \
    && echo "deb [arch=$(dpkg --print-architecture) signed-by=/etc/apt/keyrings/githubcli-archive-keyring.gpg] https://cli.github.com/packages stable main" > /etc/apt/sources.list.d/github-cli.list \
    && apt-get update -qq \
    && apt-get install -y --no-install-recommends gh \
    && apt-get clean \
    && rm -rf /var/lib/apt/lists/*

# --- loom-daemon (release binary — pinned artifact, not a source rebuild) ---
COPY ${LOOM_DAEMON_BIN} /usr/local/bin/loom-daemon
RUN chmod 0755 /usr/local/bin/loom-daemon \
    && /usr/local/bin/loom-daemon --version

# --- Non-root user --------------------------------------------------------
RUN groupadd --gid "${LOOM_GID}" "${LOOM_USER}" \
    && useradd --uid "${LOOM_UID}" --gid "${LOOM_GID}" --create-home --shell /bin/bash "${LOOM_USER}" \
    && mkdir -p /workspace \
    && chown "${LOOM_USER}:${LOOM_USER}" /workspace

USER ${LOOM_USER}
WORKDIR /home/${LOOM_USER}

# --- Claude Code CLI -------------------------------------------------------
# Same installer the fleet plan's `claude-code` step runs (add_worker.rs
# `render_claude_code`), run as the non-root user so it lands under
# ~/.local/bin exactly as it would on a bare-metal fleet worker.
RUN curl -fsSL https://claude.ai/install.sh | bash
ENV PATH="/home/${LOOM_USER}/.local/bin:${PATH}"

# --- Bootstrap seams (mount points, not baked content) --------------------
# No `VOLUME` instruction: an anonymous-volume declaration here would survive
# `docker build` in a way that's easy to mount over accidentally and is not
# needed for correctness — the consumer controls binding explicitly with
# `docker run -v`. This directory tree exists only so the intended mount
# points have correct non-root ownership before anything is bound over them:
#   /workspace              a repo checkout / worktree (bind mount)
#   /home/loom/.loom/tokens the token pool (bind mount, e.g. ~/.loom/tokens, read-only)
#   /home/loom/.loom/accounts.env  optional, bind-mounted read-only when the
#                            pool bootstraps from it inside the container
# No secret/token/credential file is created, copied, or baked in above this
# line or below it — every one of these paths is EMPTY until a `docker run`
# mounts something into it.
RUN mkdir -p /home/${LOOM_USER}/.loom/tokens

WORKDIR /workspace

# No ENTRYPOINT/CMD prescribing "the container IS the worker" (shape decision
# above): the image is a pinned shell a caller runs a command in, exactly like
# `spawn-worker.sh` invoked bare-metal. Default to an interactive shell so
# `docker run -it ghcr.io/rjwalters/loom-worker:<version>` is useful on its
# own for manual inspection; a real dispatch overrides this with the actual
# spawn command, e.g.:
#   docker run --rm -v "$PWD:/workspace" -v "$HOME/.loom/tokens:/home/loom/.loom/tokens:ro" \
#     ghcr.io/rjwalters/loom-worker:<version> \
#     .loom/scripts/spawn-worker.sh -p "/loom:sweep 123" --dangerously-skip-permissions
CMD ["/bin/bash"]
