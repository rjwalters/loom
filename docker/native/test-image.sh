#!/usr/bin/env bash
# test-image.sh — smoke-test a built `loom-worker-native` image (#8403).
#
# Sibling to docker/worker/test-image.sh and docker/session/test-image.sh:
# takes an already-built image tag and asserts the contract
# docker/native/README.md documents — the checks a `docker build` alone
# cannot catch.
#
# Unlike docker/session/test-image.sh, this one never starts a long-lived
# container: the native image serves the EPHEMERAL lifetime, so every check
# below is a one-shot `docker run --rm`, which is also exactly how a real
# dispatch uses it.
#
# Usage:
#   docker build -f docker/worker/Dockerfile -t loom-worker:dev .
#   docker build -f docker/native/Dockerfile \
#     --build-arg BASE_IMAGE=loom-worker:dev -t loom-worker-native:dev .
#   ./docker/native/test-image.sh loom-worker-native:dev
#
# Exit 0 = every check passed. Exit 1 = at least one check failed.

set -uo pipefail

IMAGE="${1:?usage: test-image.sh <image-tag>}"
OPENCODE_VERSION="${OPENCODE_VERSION:-1.18.31}"
PI_VERSION="${PI_VERSION:-0.85.1}"
KIMI_CODE_VERSION="${KIMI_CODE_VERSION:-2.0.2}"
# @moonshot-ai/kimi-code's own engines.node floor (#8565).
KIMI_NODE_FLOOR="${KIMI_NODE_FLOOR:-22.19.0}"

FAILURES=0
fail() {
    echo "FAIL: $1" >&2
    FAILURES=$((FAILURES + 1))
}
pass() {
    echo "PASS: $1"
}
# Run one shell snippet in a throwaway container, as a real dispatch would.
in_image() {
    docker run --rm --entrypoint bash "$IMAGE" -lc "$1" 2>&1
}

echo "== Testing image: $IMAGE =="

# 1. Architecture sanity (#7388) — identical rationale to the sibling scripts:
# an emulated/wrong-arch image should fail loudly here rather than silently
# passing every functional check below.
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

# 2. All three native CLIs are present at the EXACT pinned versions
# .loom/docs/guardrail-parity-native.md records as tested. Equality, not a
# floor: the parity doc records a tested version, and "newer" is not
# "verified" (that doc's own "CLI exit zero is not acceptance evidence").
OPENCODE_ACTUAL="$(in_image 'opencode --version' | grep -oE '[0-9]+\.[0-9]+\.[0-9]+' | head -1)"
if [[ "$OPENCODE_ACTUAL" == "$OPENCODE_VERSION" ]]; then
    pass "opencode is pinned at the tested version: $OPENCODE_ACTUAL"
else
    fail "opencode reports '$OPENCODE_ACTUAL', expected the pinned $OPENCODE_VERSION (.loom/docs/guardrail-parity-native.md)"
fi

PI_ACTUAL="$(in_image 'pi --version' | grep -oE '[0-9]+\.[0-9]+\.[0-9]+' | head -1)"
if [[ "$PI_ACTUAL" == "$PI_VERSION" ]]; then
    pass "pi is pinned at the tested version: $PI_ACTUAL"
else
    fail "pi reports '$PI_ACTUAL', expected the pinned $PI_VERSION (.loom/docs/guardrail-parity-native.md)"
fi

# `kimi -V` is the version command the #8561 harness probe recorded against
# 2.0.2 (docs/experiments/kimi-harness-probe-2026-09-22.json).
KIMI_ACTUAL="$(in_image 'kimi -V' | grep -oE '[0-9]+\.[0-9]+\.[0-9]+' | head -1)"
if [[ "$KIMI_ACTUAL" == "$KIMI_CODE_VERSION" ]]; then
    pass "kimi is pinned at the tested version: $KIMI_ACTUAL"
else
    fail "kimi reports '$KIMI_ACTUAL', expected the pinned $KIMI_CODE_VERSION (.loom/docs/guardrail-parity-native.md)"
fi

# 2b. Node meets Kimi's engines.node floor. The Dockerfile asserts this at
# build time; re-asserting it here catches an image built from an older
# Dockerfile or with an overridden NODE_VERSION.
NODE_ACTUAL="$(in_image 'node --version' | tr -d 'v' | tr -d '\r')"
# `sort` consumes its whole input, so this pipeline has no early-exit consumer
# to SIGPIPE the producer under `pipefail` (scripts/check-pipefail-early-exit.sh);
# the lowest version is taken with a parameter expansion, not `head -1`.
NODE_SORTED="$(printf '%s\n%s\n' "$KIMI_NODE_FLOOR" "$NODE_ACTUAL" | sort -V)"
if [[ "${NODE_SORTED%%$'\n'*}" == "$KIMI_NODE_FLOOR" ]]; then
    pass "node $NODE_ACTUAL meets Kimi's engines.node floor ($KIMI_NODE_FLOOR)"
else
    fail "node $NODE_ACTUAL is below Kimi's engines.node floor ($KIMI_NODE_FLOOR)"
fi

# 3. The runtime half of the pin: the CLI must not be able to update itself
# past the tested version on first launch inside a container.
for hygiene in OPENCODE_DISABLE_AUTOUPDATE KIMI_CODE_NO_AUTO_UPDATE KIMI_DISABLE_TELEMETRY; do
    if [[ "$(in_image "echo \"\${${hygiene}:-unset}\"")" == "1" ]]; then
        pass "$hygiene=1 is baked into the image"
    else
        fail "$hygiene is not 1 in the image environment"
    fi
done

# 3b. KIMI_CODE_HOME must NOT be baked in: `worker_spawn::containment` points
# it at a per-launch path, and an image-level value would hand every worker in
# a container one shared home again (#8565).
KIMI_HOME_BAKED="$(in_image 'echo "${KIMI_CODE_HOME:-unset}"')"
if [[ "$KIMI_HOME_BAKED" == "unset" ]]; then
    pass "KIMI_CODE_HOME is not baked into the image (the dispatcher relocates it per launch)"
else
    fail "KIMI_CODE_HOME is baked in as '$KIMI_HOME_BAKED' — every worker in a container would share one home"
fi

# 4. The ephemeral per-launch state root exists, is owned by uid 1000, is
# writable, and is EMPTY — it is a writable-layer location, never a mount
# point and never baked content.
# The `probe/kimi/bin` leaf is not decoration: Kimi downloads `rg`/`fd` into
# `$KIMI_CODE_HOME/bin/` on first use, so a per-launch home that cannot be
# created and written under is a broken Kimi launch, not merely untidy (#8565).
STATE_CHECK=$(in_image '
    root=/home/loom/.loom-native
    echo "OWNER_UID=$(stat -c %u "$root" 2>/dev/null)"
    mkdir -p "$root/probe/data" "$root/probe/kimi/bin" 2>/dev/null \
        && touch "$root/probe/kimi/bin/rg" 2>/dev/null && echo WRITABLE=1
')
if [[ "$STATE_CHECK" == *"OWNER_UID=1000"* && "$STATE_CHECK" == *"WRITABLE=1"* ]]; then
    pass "/home/loom/.loom-native exists, is owned by uid 1000, and is writable"
else
    fail "/home/loom/.loom-native check failed: $STATE_CHECK"
fi

STATE_CONTENTS=$(in_image 'ls -A /home/loom/.loom-native 2>/dev/null | tr "\n" " "')
if [[ -z "${STATE_CONTENTS// /}" ]]; then
    pass "/home/loom/.loom-native is empty (no baked session store or auth.json)"
else
    fail "/home/loom/.loom-native is NOT empty: $STATE_CONTENTS"
fi

# 5. Two concurrent containers from this image cannot see each other's
# ephemeral state. This is the image-level half of issue #8403's
# "disjoint session stores" criterion — the dispatcher half (pointing
# XDG_DATA_HOME et al. at a per-launch path) is covered by the Rust tests in
# loom-daemon/src/worker_spawn/containment_tests.rs.
MARKER="loom-native-isolation-$$"
docker run --rm -d --name "${MARKER}-a" --entrypoint bash "$IMAGE" \
    -lc "mkdir -p /home/loom/.loom-native/a && sleep 30" >/dev/null 2>&1
SEEN_FROM_B=$(docker run --rm --entrypoint bash "$IMAGE" \
    -lc 'ls -A /home/loom/.loom-native 2>/dev/null | tr "\n" " "' 2>&1)
docker rm -f "${MARKER}-a" >/dev/null 2>&1 || true
if [[ -z "${SEEN_FROM_B// /}" ]]; then
    pass "a second container cannot see the first container's ephemeral state"
else
    fail "ephemeral state leaked between containers: '$SEEN_FROM_B'"
fi

# 6. No secrets baked in — best-effort docker-history scan, the same pattern
# set the sibling scripts use plus native-credential shapes (a provider API
# key, or an OpenCode auth.json accidentally COPYed in).
HISTORY=$(docker history --no-trunc "$IMAGE" 2>&1 || true)
SECRET_HIT=0
for pattern in \
    CLAUDE_CODE_OAUTH_TOKEN \
    GITHUB_TOKEN \
    GH_TOKEN \
    OPENAI_API_KEY \
    ZAI_API_KEY \
    ZHIPU_API_KEY \
    KIMI_MODEL_API_KEY \
    'sk-[A-Za-z0-9]{20,}' \
    'accounts\.env' \
    '\.loom/tokens/.*\.token' \
    'auth\.json'; do
    # Here-string, not `echo … | grep -q`: under `pipefail` an early-exit
    # consumer can SIGPIPE the producer and fail the whole pipeline
    # (scripts/check-pipefail-early-exit.sh, #7060/#7771).
    if grep -qE "$pattern" <<<"$HISTORY"; then
        fail "docker history matched a secret-shaped pattern: $pattern"
        SECRET_HIT=1
    fi
done
if [[ "$SECRET_HIT" -eq 0 ]]; then
    pass "no secret-shaped strings found in docker history"
fi

# 7. Core toolchain inherited from the base image is still present (sanity
# check that this layer did not shadow or break anything), plus the Node
# runtime OpenCode needs at LAUNCH time to install its own plugin package.
for bin in git gh jq curl claude node npm opencode pi kimi; do
    if in_image "command -v $bin >/dev/null 2>&1"; then
        pass "$bin present on PATH"
    else
        fail "$bin missing from PATH"
    fi
done

# 8. The image keeps the base image's "a shell a caller runs a command in"
# shape — no ENTRYPOINT, so `docker run <image> <cmd>` runs <cmd> directly,
# which is exactly what worker_spawn::containment execs.
ENTRYPOINT_SET=$(docker inspect --format '{{json .Config.Entrypoint}}' "$IMAGE" 2>/dev/null || echo unknown)
if [[ "$ENTRYPOINT_SET" == "null" || "$ENTRYPOINT_SET" == "[]" ]]; then
    pass "no ENTRYPOINT (dispatch runs spawn-worker.sh directly)"
else
    fail "image declares an ENTRYPOINT ($ENTRYPOINT_SET) — dispatch would have to override it"
fi

echo "== $FAILURES failure(s) =="
exit $((FAILURES > 0 ? 1 : 0))
