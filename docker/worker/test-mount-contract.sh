#!/usr/bin/env bash
# test-mount-contract.sh — proves the worktree-correctness property of
# docker/worker/MOUNT-CONTRACT.md § "Path parity" (#6898).
#
# Path parity (the host workspace root mounted at the identical absolute path
# inside the container) is load-bearing for git worktrees: `.git` pointer
# files on both sides of a `git worktree add` relationship store ABSOLUTE
# paths. This script proves that concretely against a scratch git repo,
# rather than just asserting it in prose:
#
#   POSITIVE — with a parity mount (host path == container path), a
#   container can run `git status` cleanly inside a `git worktree add`-created
#   worktree, commit there, and the host sees the resulting commit.
#
#   NEGATIVE — the identical worktree, bind-mounted at a DIFFERENT
#   (non-parity) container path, fails `git status` — proving the positive
#   case actually exercises path parity rather than passing by accident.
#
# Deliberately NOT a build step, same posture as test-image.sh: it takes an
# already-built image tag and drives `docker run` against it.
#
# Usage:
#   docker build -f docker/worker/Dockerfile -t loom-worker:test .
#   ./docker/worker/test-mount-contract.sh loom-worker:test
#
# Skips CLEANLY (exit 0, not a failure) when docker is unavailable or the
# caller cannot reach the docker daemon — this suite is meant to run
# wherever docker is available (including CI's worker-image-smoke leg) but
# must never fail a docker-less host/dev machine.
#
# Exit 0 = every check passed (or skipped due to no docker). Exit 1 = at
# least one check failed.

set -euo pipefail

IMAGE="${1:?usage: test-mount-contract.sh <image-tag>}"

# --- Skip cleanly when docker is not usable here ---------------------------
if ! command -v docker >/dev/null 2>&1; then
    echo "SKIP: docker CLI not found on PATH — nothing to test here."
    exit 0
fi
if ! docker info >/dev/null 2>&1; then
    echo "SKIP: docker daemon not reachable (not running, or no permission on this host) — nothing to test here."
    exit 0
fi

FAILURES=0
fail() {
    echo "FAIL: $1" >&2
    FAILURES=$((FAILURES + 1))
}
pass() {
    echo "PASS: $1"
}

echo "== Testing mount contract (path parity) against image: $IMAGE =="

# --- Scratch git repo + worktree, all under one absolute host path ---------
# mktemp -d already returns an absolute, literal path, so every path derived
# below is unambiguous — no unexpanded-variable path escapes the sandbox this
# script itself runs in.
SCRATCH="$(mktemp -d "${TMPDIR:-/tmp}/loom-mount-contract-test.XXXXXX")"
# shellcheck disable=SC2329  # invoked indirectly via the EXIT trap below
cleanup() {
    # The container commit above (run as image uid 1000) creates new
    # .git/objects/ directories that postdate the o+rwx chmod below and
    # inherit git's default (non-o+w) permissions — the host user cannot
    # unlink files inside them. Reset permissions from the same uid that
    # created them (root inside the container) before the host-side rm;
    # every step here is best-effort so a leftover-permissions edge case
    # never fails the whole test run via the EXIT trap under `set -e`.
    docker run --rm --user 0 -v "$SCRATCH:$SCRATCH" "$IMAGE" chmod -R a+rwX "$SCRATCH" >/dev/null 2>&1 || true
    chmod -R u+rwx "$SCRATCH" 2>/dev/null || true
    rm -rf "$SCRATCH" 2>/dev/null || true
}
trap cleanup EXIT

MAIN_REPO="$SCRATCH/main-repo"
WORKTREE="$SCRATCH/main-repo-worktree"

git -C "$SCRATCH" init -q "$MAIN_REPO"
git -C "$MAIN_REPO" config user.email "mount-contract-test@loom.local"
git -C "$MAIN_REPO" config user.name "Mount Contract Test"
# Avoid mode-bit noise from the chmod below showing up as a spurious diff.
git -C "$MAIN_REPO" config core.fileMode false
echo "seed" > "$MAIN_REPO/seed.txt"
git -C "$MAIN_REPO" add seed.txt
git -C "$MAIN_REPO" commit -q -m "seed commit"
git -C "$MAIN_REPO" worktree add -q -b mount-contract-test-branch "$WORKTREE"

# The image's default user is uid/gid 1000 (`loom`) — the CI runner's own uid
# is generally NOT 1000, so make the scratch tree writable by any uid rather
# than chown-ing to a hardcoded 1000 (this test is about path parity, not
# uid/gid mapping, which MOUNT-CONTRACT.md § "uid/gid mapping" covers
# separately and is not exercised here).
chmod -R o+rwx "$SCRATCH"

GIT_IDENTITY=(-c user.email=mount-contract-test@loom.local -c user.name="Mount Contract Test")
SAFE_DIR=(-c safe.directory=*)

# --- POSITIVE: parity mount (host path == container path) ------------------
echo "-- positive case: parity mount --"
if OUT=$(docker run --rm \
    -v "$SCRATCH:$SCRATCH" \
    -w "$WORKTREE" \
    "$IMAGE" \
    git "${SAFE_DIR[@]}" status 2>&1); then
    pass "git status succeeds inside a worktree under a parity mount"
else
    fail "git status failed under a parity mount: $OUT"
fi

if OUT=$(docker run --rm \
    -v "$SCRATCH:$SCRATCH" \
    -w "$WORKTREE" \
    "$IMAGE" \
    git "${SAFE_DIR[@]}" "${GIT_IDENTITY[@]}" commit --allow-empty -m "container commit under parity mount" 2>&1); then
    pass "container can commit inside a worktree under a parity mount"
else
    fail "container commit failed under a parity mount: $OUT"
fi

HOST_LOG="$(git -C "$WORKTREE" log -1 --format=%s 2>&1 || true)"
if [[ "$HOST_LOG" == "container commit under parity mount" ]]; then
    pass "host sees the container's commit in the worktree history"
else
    fail "host does not see the container's commit (git log -1: '$HOST_LOG')"
fi

# --- NEGATIVE: same worktree, mounted at a non-parity path ------------------
echo "-- negative case: non-parity mount --"
NONPARITY_PATH="/loom-mount-contract-nonparity"
if OUT=$(docker run --rm \
    -v "$SCRATCH:$NONPARITY_PATH" \
    -w "$NONPARITY_PATH/main-repo-worktree" \
    "$IMAGE" \
    git "${SAFE_DIR[@]}" status 2>&1); then
    fail "git status unexpectedly SUCCEEDED under a non-parity mount (expected failure — the worktree's absolute-path .git pointer should not resolve): $OUT"
else
    pass "git status fails as expected under a non-parity mount (proves the positive case exercises real path parity)"
fi

echo "== $FAILURES failure(s) =="
exit $((FAILURES > 0 ? 1 : 0))
