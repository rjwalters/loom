#!/usr/bin/env bash
# test-image.sh — smoke-test a built `loom-worker` image (#5325).
#
# Deliberately NOT a build step: it takes an already-built image tag and
# asserts the contract docker/worker/README.md documents — the checks a
# `docker build` alone cannot catch (a Dockerfile with no `RUN false` still
# builds "successfully" even if the resulting image is missing a binary on
# PATH, runs as root, or has a stray secret baked in).
#
# Usage:
#   docker build -f docker/worker/Dockerfile -t loom-worker:test .
#   ./docker/worker/test-image.sh loom-worker:test
#
# Exit 0 = every check passed. Exit 1 = at least one check failed (each
# failure is printed with which assertion broke, not just a generic diff).

set -euo pipefail

IMAGE="${1:?usage: test-image.sh <image-tag>}"

FAILURES=0
fail() {
    echo "FAIL: $1" >&2
    FAILURES=$((FAILURES + 1))
}
pass() {
    echo "PASS: $1"
}

run() {
    # Run a command inside the image as the image's default user, capturing
    # stdout+stderr together so a failure's own diagnostic is visible in CI logs.
    docker run --rm "$IMAGE" bash -lc "$1"
}

echo "== Testing image: $IMAGE =="

# 1. Architecture sanity (#7388): when this script runs on the SAME host that
# built/loaded the image (the common local `docker build && ./test-image.sh`
# loop, with no `--platform` override), the image's reported architecture
# MUST match the host's own — otherwise an accidentally-emulated image (QEMU,
# a stale `--platform` flag, a misconfigured buildx builder) passes every
# other check here while being the wrong arch for this machine. Maps
# `uname -m`'s naming (x86_64/aarch64) to Docker's own (amd64/arm64) so a
# native run always compares like for like; CI (a native ubuntu-latest amd64
# runner testing its own native amd64 build) is a native run in this same
# sense, so this assertion applies there unconditionally too — the arm64 leg
# of a multi-arch release build is intentionally never smoke-tested this way
# in CI (no native arm64 runner), see docker/worker/README.md.
HOST_ARCH_RAW="$(uname -m)"
case "$HOST_ARCH_RAW" in
    x86_64|amd64) HOST_ARCH="amd64" ;;
    aarch64|arm64) HOST_ARCH="arm64" ;;
    *) HOST_ARCH="$HOST_ARCH_RAW" ;;
esac
IMAGE_ARCH="$(docker inspect --format '{{.Architecture}}' "$IMAGE" 2>/dev/null || echo unknown)"
if [[ "$IMAGE_ARCH" == "$HOST_ARCH" ]]; then
    pass "image architecture ($IMAGE_ARCH) matches host ($HOST_ARCH_RAW)"
else
    fail "image architecture ($IMAGE_ARCH) does not match host ($HOST_ARCH_RAW -> $HOST_ARCH) — built under emulation, or with the wrong --platform?"
fi

# 2. loom-daemon is on PATH and runs.
if OUT=$(run "loom-daemon --version" 2>&1); then
    pass "loom-daemon --version: $OUT"
else
    fail "loom-daemon --version did not exit 0"
fi

# 3. Claude Code CLI is on PATH.
if run "command -v claude >/dev/null 2>&1"; then
    pass "claude CLI present on PATH"
else
    fail "claude CLI not found on PATH"
fi

# 4. Core toolchain the loom scripts assume is present.
for bin in git gh jq tmux curl; do
    if run "command -v $bin >/dev/null 2>&1"; then
        pass "$bin present on PATH"
    else
        fail "$bin missing from PATH"
    fi
done

# 5. Runs as a non-root user by default.
ACTUAL_USER=$(run "id -un")
ACTUAL_UID=$(run "id -u")
if [[ "$ACTUAL_USER" != "root" && "$ACTUAL_UID" != "0" ]]; then
    pass "default user is non-root ($ACTUAL_USER, uid=$ACTUAL_UID)"
else
    fail "default user is root (expected a non-root default user)"
fi

# 6. /workspace exists, is the default cwd, and is writable by the default user.
WORKDIR_CHECK=$(run 'pwd && touch /workspace/.loom-test-write && rm -f /workspace/.loom-test-write && echo WRITABLE')
if [[ "$WORKDIR_CHECK" == *"/workspace"* && "$WORKDIR_CHECK" == *"WRITABLE"* ]]; then
    pass "/workspace is the default cwd and writable by the default user"
else
    fail "/workspace is not the default cwd or not writable: $WORKDIR_CHECK"
fi

# 7. No secrets baked in. This is a best-effort scan, not a proof: it greps
# the image's history/env for the token/credential env vars and file paths
# loom's own token-pool and forge-auth mechanisms use, so a regression that
# accidentally bakes a real secret into a layer fails loudly here instead of
# shipping silently.
HISTORY=$(docker history --no-trunc "$IMAGE" 2>&1 || true)
SECRET_HIT=0
for pattern in CLAUDE_CODE_OAUTH_TOKEN GITHUB_TOKEN GH_TOKEN 'accounts\.env' '\.loom/tokens/.*\.token'; do
    if echo "$HISTORY" | grep -qE "$pattern"; then
        fail "docker history matched a secret-shaped pattern: $pattern"
        SECRET_HIT=1
    fi
done
if [[ "$SECRET_HIT" -eq 0 ]]; then
    pass "no secret-shaped strings found in docker history"
fi

# Token pool dir exists but is empty (mount point, not baked content).
TOKENS_CONTENTS=$(run 'ls -A /home/*/.loom/tokens 2>/dev/null || true')
if [[ -z "$TOKENS_CONTENTS" ]]; then
    pass "token pool mount point is empty (no baked tokens)"
else
    fail "token pool mount point is NOT empty: $TOKENS_CONTENTS"
fi

echo "== $FAILURES failure(s) =="
exit $((FAILURES > 0 ? 1 : 0))
