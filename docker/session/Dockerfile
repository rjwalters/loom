# syntax=docker/dockerfile:1
#
# loom-worker-session image (#6899, Epic #6896 Phase 1) — the session-capable
# layer FROM loom-worker that Phase 2's per-account persistent Codex session
# containers run. Full architecture: docs/adr/0017-session-container-architecture.md
# (two container lifetimes — this image serves the PERSISTENT one; headless
# `docker exec` dispatch; tmux as an operator-only interactive re-login
# surface, never a dispatch mechanism).
#
# What this layer adds ON TOP OF loom-worker, and nothing else:
#   1. OpenAI Codex CLI (npm package @openai/codex), version-verified at
#      build time against the runtime-adapter floor — codex >= 0.146.0, see
#      .loom/docs/runtime-adapters.md. The base image ships no Node/npm (see
#      docker/worker/README.md "What this image deliberately does NOT
#      include"), so Node is installed here too — a genuine transitive
#      dependency of "add Codex CLI" (npm is Codex's only distribution
#      channel), not toolchain scope creep.
#   2. A long-lived tmux-server entrypoint: `tini` (a real, zombie-reaping
#      init) is PID 1, and its one child starts a detached tmux session and
#      then blocks forever. `docker exec <container> codex exec …` delivers
#      headless work (ADR-0017 Decision 2); `docker exec -it <container>
#      tmux attach -t session` is the operator's interactive re-login surface
#      (e.g. `codex login` after a dead refresh chain). No daemon, no
#      supervisor, no auto-start of any agent — the container idles at
#      ~zero cost until something execs work into it.
#   3. CODEX_HOME convention: ENV CODEX_HOME=/home/loom/.codex-profile, an
#      empty, read-write directory owned by uid 1000 at build time — the
#      mount point a per-account Codex profile volume binds over at
#      `docker run`/session-start time (Phase 2). Zero secrets baked in,
#      verified by docker/session/test-image.sh's docker-history scan.
#
# FROM contract: same-version pin. `BASE_IMAGE` defaults to `:latest` for
# convenient local/manual builds, but the release workflow always passes
# --build-arg BASE_IMAGE=ghcr.io/rjwalters/loom-worker:<version> so the pair
# locksteps exactly the way loom-worker locksteps with loom-daemon. See
# docker/session/README.md for the full contract, entrypoint behavior, and
# attach/exec usage.

ARG BASE_IMAGE=ghcr.io/rjwalters/loom-worker:latest
FROM ${BASE_IMAGE}

# SHELL (bash -o pipefail -c, #6409) is inherited from the base image's own
# SHELL instruction — Docker persists it into the image config, so a failing
# `curl | sh`-shaped RUN step below fails this build immediately instead of
# silently producing a broken layer. See docker/worker/README.md "Build
# SHELL" for the mechanism this relies on.

ARG DEBIAN_FRONTEND=noninteractive

# Pinned, already-verified Codex CLI version — NOT floating `@latest`, so a
# future upstream Codex release cannot silently change this image's behavior
# out from under an unrelated rebuild. CODEX_MIN_VERSION is the
# runtime-adapter floor this build enforces regardless of which version is
# pinned above it; bump both deliberately together when the floor moves.
ARG CODEX_VERSION=0.149.1
ARG CODEX_MIN_VERSION=0.146.0

# Node.js LTS release, pinned + checksum-verified below — Codex CLI's only
# distribution channel is npm, and npm needs a Node runtime. Deliberately NOT
# `apt-get install nodejs npm`: Ubuntu 24.04's `npm` package pulls in its
# entire Debian-packaged build-from-source dependency chain (eslint, webpack,
# babel, and ~150 `node-*` library packages) to satisfy its own postinst,
# ballooning this layer by 600+ MB for tooling this image never uses. The
# official upstream tarball bundles `node` + `npm` + `npx` together with none
# of that, at a fraction of the size.
ARG NODE_VERSION=24.19.0
ARG NODE_SHA256=14b342e71204f811bde6153be8e04b62aef63c236fef92b55f9c83154b409647

USER root

# --- Node.js (tarball, checksum-verified) + tini ---------------------------
# tini: a minimal, well-tested init (~750 lines of C) that becomes this
# image's PID 1 below — it forwards signals and reaps every zombie
# reparented to it (including anything a `docker exec`'d command leaves
# behind), which is the "equivalent thin init that reaps zombies" ADR-0017
# Decision 2 calls for as an alternative to a bespoke reaper loop.
RUN apt-get update -qq \
    && apt-get install -y --no-install-recommends tini \
    && apt-get clean \
    && rm -rf /var/lib/apt/lists/* \
    && curl -fsSL --retry 5 --retry-all-errors -o /tmp/node.tar.xz \
         "https://nodejs.org/dist/v${NODE_VERSION}/node-v${NODE_VERSION}-linux-x64.tar.xz" \
    && echo "${NODE_SHA256}  /tmp/node.tar.xz" | sha256sum -c - \
    && tar -xJf /tmp/node.tar.xz -C /usr/local --strip-components=1 \
    && rm -f /tmp/node.tar.xz \
    && node --version && npm --version

USER loom
WORKDIR /home/loom

# npm's default global prefix (/usr/local, owned by root above) would make a
# `-g` install fail as a non-root user; a user-owned prefix keeps the same
# "no root-owned runtime artifact" posture the base image's Claude Code CLI
# install already follows.
ENV NPM_CONFIG_PREFIX=/home/loom/.npm-global
ENV PATH="/home/loom/.npm-global/bin:${PATH}"

# --- OpenAI Codex CLI -------------------------------------------------
# Installs the pinned version, then asserts BOTH that the binary runs at all
# and that its reported version meets the runtime-adapter floor — a pin bump
# that accidentally regresses below the floor fails this build loudly rather
# than shipping an image `spawn-codex.sh` would reject at dispatch time.
# `npm cache clean` + removing ~/.npm: the CLI's own installed package
# (native platform binary bundled as an optionalDependency) is a genuine
# ~300MB, but npm additionally leaves a ~130MB download cache behind in
# ~/.npm that serves no purpose in a built image — dropped in this same
# layer so it never lands in a shipped layer at all.
RUN npm install -g "@openai/codex@${CODEX_VERSION}" \
    && CODEX_VERSION_OUTPUT="$(codex --version)" \
    && echo "Installed: ${CODEX_VERSION_OUTPUT}" \
    && CODEX_ACTUAL="$(echo "${CODEX_VERSION_OUTPUT}" | grep -oE '[0-9]+\.[0-9]+\.[0-9]+' | head -1)" \
    && LOWEST="$(printf '%s\n%s\n' "${CODEX_MIN_VERSION}" "${CODEX_ACTUAL}" | sort -V | head -1)" \
    && npm cache clean --force \
    && rm -rf /home/loom/.npm \
    && if [ "${LOWEST}" != "${CODEX_MIN_VERSION}" ]; then \
         echo "FATAL: codex ${CODEX_ACTUAL} is below the ${CODEX_MIN_VERSION} floor (.loom/docs/runtime-adapters.md)" >&2; \
         exit 1; \
       fi

# --- CODEX_HOME convention --------------------------------------------
# Empty at build time (mount point, not baked content) — a per-account
# Codex profile volume binds over this path at container-start time
# (Phase 2). Created by the `loom` user so it is owned by uid 1000 with no
# extra chown step.
ENV CODEX_HOME=/home/loom/.codex-profile
RUN mkdir -p "${CODEX_HOME}"

# --- tmux-server entrypoint (Decision 2) --------------------------------
COPY --chown=loom:loom docker/session/entrypoint.sh /home/loom/.local/bin/loom-session-entrypoint.sh
RUN chmod 0755 /home/loom/.local/bin/loom-session-entrypoint.sh

WORKDIR /home/loom

# tini is PID 1 (signal forwarding + zombie reaping); its one child is the
# entrypoint script, which starts the detached tmux session and then blocks
# forever. No CMD — this image is not the "run a shell for inspection"
# shape the base image's own CMD ["/bin/bash"] is; it is always the
# persistent session server, so any inherited CMD is cleared below.
ENTRYPOINT ["/usr/bin/tini", "--", "/home/loom/.local/bin/loom-session-entrypoint.sh"]
CMD []
