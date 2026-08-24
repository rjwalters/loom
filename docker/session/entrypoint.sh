#!/bin/bash
# loom-session-entrypoint.sh (#6899) — the persistent-container entrypoint
# ADR-0017 Decision 2 describes. Runs as `tini`'s direct child (tini is PID 1,
# see docker/session/Dockerfile's ENTRYPOINT): starts (or resumes) the
# long-lived tmux session that makes this container persistent, then blocks
# forever.
#
# Dispatch surfaces this makes possible:
#   docker exec <container> codex exec …          headless work, ANY time
#   docker exec -it <container> tmux attach -t "$LOOM_SESSION_TMUX_NAME"
#                                                   operator interactive
#                                                   re-login (e.g. after
#                                                   `codex login` is needed)
#
# This script never starts an agent, a daemon, or a supervisor — it starts
# exactly one tmux server and then idles. Zombie reaping for anything
# `docker exec`'d into the container is tini's job (PID 1), not this
# script's — orphaned processes reparent to tini automatically at the
# kernel level regardless of what this script does.
set -euo pipefail

SESSION_NAME="${LOOM_SESSION_TMUX_NAME:-session}"

if ! tmux has-session -t "${SESSION_NAME}" 2>/dev/null; then
    tmux new-session -d -s "${SESSION_NAME}"
fi

# Block indefinitely. tini forwards SIGTERM/SIGINT (e.g. from `docker stop`)
# to this process, which terminates `sleep` and lets the container exit
# cleanly; tmux's own server process is independent of this script's exit
# and is torn down with the container.
exec sleep infinity
