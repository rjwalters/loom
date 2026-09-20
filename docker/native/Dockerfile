# syntax=docker/dockerfile:1
#
# loom-worker-native image (#8403, Epic #6896 Phase 3) — the native-harness
# layer FROM loom-worker that per-sweep EPHEMERAL containers for Pi/OpenCode
# sweeps run. Full architecture: docs/adr/0017-session-container-architecture.md.
#
# Which of the two container lifetimes is this? The EPHEMERAL one, the same
# lifetime `spawn-claude.sh`'s containerized dispatch uses (#7429/#7430) — one
# fresh container per sweep, torn down at the end. It is deliberately NOT the
# per-account PERSISTENT session shape `docker/session/` serves for Codex.
# That shape exists because `CODEX_HOME/auth.json` is a mutable OAuth refresh
# chain that needs exactly one owning process; an API-key subscription has no
# refresh chain to own, so the key is injected as container ENV at start and
# exists nowhere on the container's filesystem. See
# .loom/docs/runtime-adapters.md § Session containers for that reasoning in
# full — this image's whole reason to be separate from docker/session/ is that
# it is a DIFFERENT lifetime, not a different CLI.
#
# What this layer adds ON TOP OF loom-worker, and nothing else:
#   1. Node.js LTS (tarball, checksum-verified) — npm is the only distribution
#      channel for both native CLIs, and OpenCode additionally installs its own
#      `@opencode-ai/plugin` package into its config directory at launch time,
#      so npm has to be present at RUN time as well as build time. The base
#      image ships no Node by design (docker/worker/README.md "What this image
#      deliberately does NOT include"); this is a genuine transitive dependency
#      of "add the native CLIs", the same judgement docker/session/Dockerfile
#      already made for Codex.
#   2. The two native CLIs PINNED at the versions .loom/docs/
#      guardrail-parity-native.md records as tested (see the verification
#      receipt native-runtime-verification-2026-09-19.md): OpenCode 1.18.31 and
#      Pi 0.85.1. Pinned, never `@latest`: a floating tag would let an
#      unrelated rebuild silently ship a version nothing has been verified
#      against, and "CLI exit zero is not acceptance evidence" (the parity
#      doc's own words) means a drifted version would not announce itself.
#   3. `OPENCODE_DISABLE_AUTOUPDATE=1` — the runtime half of the same
#      guarantee. Pinning the install is not enough if the CLI can update
#      ITSELF past the tested version on first launch inside the container.
#
# What it deliberately does NOT add: no ENTRYPOINT, no tmux server, no daemon,
# no CODEX_HOME, and no session persistence of any kind. The base image's
# `CMD ["/bin/bash"]` shape is exactly right here — a real dispatch overrides
# it with `<workspace>/.loom/scripts/spawn-worker.sh …`, which is what
# `loom_daemon::worker_spawn::containment` execs. Zero secrets, zero identity,
# zero host config are baked in, verified by docker/native/test-image.sh.
#
# FROM contract: same-version pin, identical to docker/session/'s.
# `BASE_IMAGE` defaults to `:latest` for convenient local builds; the release
# workflow always passes
# --build-arg BASE_IMAGE=ghcr.io/rjwalters/loom-worker:<version>.

ARG BASE_IMAGE=ghcr.io/rjwalters/loom-worker:latest
FROM ${BASE_IMAGE}

# SHELL (bash -o pipefail -c, #6409) is inherited from the base image's own
# SHELL instruction, so a failing `curl | sh`-shaped RUN below fails this build
# at the step that broke. See docker/worker/README.md "Build SHELL".

ARG DEBIAN_FRONTEND=noninteractive

# Pinned, already-verified native CLI versions — the exact pair
# .loom/docs/native-runtime-verification-2026-09-19.md ran its guarded live
# edit canaries against. Bump both the pin and the parity doc together, with a
# fresh canary run; never one without the other.
ARG OPENCODE_VERSION=1.18.31
ARG PI_VERSION=0.85.1

# Node.js LTS, pinned + checksum-verified. Deliberately NOT `apt-get install
# nodejs npm`: Ubuntu 24.04's `npm` package drags in ~150 `node-*` build
# dependencies (600+ MB) this image never uses. Same ARGs, same mapping and the
# same published SHASUMS256.txt entries docker/session/Dockerfile pins — the
# two layers install identical Node builds on purpose, so a future bump is one
# decision applied twice rather than two independent drifting pins.
ARG TARGETARCH
ARG NODE_VERSION=24.19.0
ARG NODE_SHA256_AMD64=14b342e71204f811bde6153be8e04b62aef63c236fef92b55f9c83154b409647
ARG NODE_SHA256_ARM64=01443c1e1a29e531ccad5a46fefa6df490d2189c49f7955904aecdbb0fe86fdc

USER root

RUN case "${TARGETARCH}" in \
         amd64) NODE_ARCH=x64; NODE_SHA256="${NODE_SHA256_AMD64}" ;; \
         arm64) NODE_ARCH=arm64; NODE_SHA256="${NODE_SHA256_ARM64}" ;; \
         *) echo "FATAL: unsupported TARGETARCH '${TARGETARCH}' — no Node.js tarball mapping" >&2; exit 1 ;; \
       esac \
    && curl -fsSL --retry 5 --retry-all-errors -o /tmp/node.tar.xz \
         "https://nodejs.org/dist/v${NODE_VERSION}/node-v${NODE_VERSION}-linux-${NODE_ARCH}.tar.xz" \
    && echo "${NODE_SHA256}  /tmp/node.tar.xz" | sha256sum -c - \
    && tar -xJf /tmp/node.tar.xz -C /usr/local --strip-components=1 \
    && rm -f /tmp/node.tar.xz \
    && node --version && npm --version

USER loom
WORKDIR /home/loom

# npm's default global prefix (/usr/local, root-owned above) would make a `-g`
# install fail as a non-root user; a user-owned prefix keeps the same "no
# root-owned runtime artifact" posture the base image's Claude Code CLI install
# already follows.
ENV NPM_CONFIG_PREFIX=/home/loom/.npm-global
ENV PATH="/home/loom/.npm-global/bin:${PATH}"

# --- Native CLIs (pinned) --------------------------------------------------
# Each install asserts the binary runs AND reports the exact pinned version, so
# a pin that silently resolved to something else fails THIS layer rather than
# shipping an image whose behavior nothing has verified. Unlike the Codex
# layer's floor check, this is an EQUALITY check: the parity doc records a
# tested version, not a minimum, and "newer" is not "verified".
RUN npm install -g "opencode-ai@${OPENCODE_VERSION}" "@earendil-works/pi-coding-agent@${PI_VERSION}" \
    && npm cache clean --force \
    && rm -rf /home/loom/.npm \
    && OPENCODE_ACTUAL="$(opencode --version 2>&1 | grep -oE '[0-9]+\.[0-9]+\.[0-9]+' | head -1)" \
    && PI_ACTUAL="$(pi --version 2>&1 | grep -oE '[0-9]+\.[0-9]+\.[0-9]+' | head -1)" \
    && echo "Installed: opencode ${OPENCODE_ACTUAL}, pi ${PI_ACTUAL}" \
    && if [ "${OPENCODE_ACTUAL}" != "${OPENCODE_VERSION}" ]; then \
         echo "FATAL: opencode ${OPENCODE_ACTUAL} != pinned ${OPENCODE_VERSION} (.loom/docs/guardrail-parity-native.md)" >&2; exit 1; \
       fi \
    && if [ "${PI_ACTUAL}" != "${PI_VERSION}" ]; then \
         echo "FATAL: pi ${PI_ACTUAL} != pinned ${PI_VERSION} (.loom/docs/guardrail-parity-native.md)" >&2; exit 1; \
       fi

# --- Never self-update past the tested version -----------------------------
# The runtime half of the pin above. Baked into the image (not only passed at
# `docker run`) so an interactive `docker run -it …` inspection session has the
# same guarantee a dispatched sweep does.
ENV OPENCODE_DISABLE_AUTOUPDATE=1

# --- Ephemeral per-launch state root ---------------------------------------
# `worker_spawn::containment` points XDG_DATA_HOME / XDG_CONFIG_HOME /
# XDG_CACHE_HOME / XDG_STATE_HOME / OPENCODE_CONFIG_DIR / LOOM_NATIVE_TOOLS_DIR
# at `<this dir>/<per-launch id>/…`. Created here so the tree has correct
# non-root ownership before anything writes into it, and EMPTY — it is a
# writable-layer location, never a mount point and never baked content. This is
# what makes two concurrent native workers' session stores and `auth.json`
# files disjoint: different containers AND different paths within them.
RUN mkdir -p /home/loom/.loom-native

WORKDIR /workspace

# No ENTRYPOINT and the base image's CMD inherited unchanged: this image is a
# pinned shell a caller runs a sweep in, exactly like loom-worker itself.
