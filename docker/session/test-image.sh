#!/usr/bin/env bash
# test-image.sh — smoke-test a built `loom-worker-session` image (#6899).
#
# Sibling to docker/worker/test-image.sh: takes an already-built image tag
# and asserts the contract docker/session/README.md documents — the checks a
# `docker build` alone cannot catch. Unlike the base image's test script,
# this one actually STARTS a container (the image's whole point is to stay
# running detached) and drives it with `docker exec`, mirroring exactly how
# a real dispatch would use it.
#
# Usage:
#   docker build -f docker/worker/Dockerfile -t loom-worker:dev .
#   docker build -f docker/session/Dockerfile \
#     --build-arg BASE_IMAGE=loom-worker:dev -t loom-worker-session:dev .
#   ./docker/session/test-image.sh loom-worker-session:dev
#
# Exit 0 = every check passed. Exit 1 = at least one check failed (each
# failure is printed with which assertion broke, not just a generic diff).

set -uo pipefail

IMAGE="${1:?usage: test-image.sh <image-tag>}"
CODEX_MIN_VERSION="${CODEX_MIN_VERSION:-0.146.0}"
TMUX_SESSION_NAME="${LOOM_SESSION_TMUX_NAME:-session}"

FAILURES=0
fail() {
    echo "FAIL: $1" >&2
    FAILURES=$((FAILURES + 1))
}
pass() {
    echo "PASS: $1"
}

echo "== Testing image: $IMAGE =="

# 1. Architecture sanity (#7388): when this script runs on the SAME host that
# built/loaded the image (the common local `docker build && ./test-image.sh`
# loop, with no `--platform` override), the image's reported architecture
# MUST match the host's own — an emulated/wrong-arch image should fail here
# loudly rather than silently passing every functional check below. Maps
# `uname -m`'s naming (x86_64/aarch64) to Docker's own (amd64/arm64); see
# docker/worker/test-image.sh's sibling check for the identical rationale,
# including why this still applies unconditionally to CI's native amd64 leg.
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

# 2. codex CLI is present and meets the runtime-adapter floor
# (.loom/docs/runtime-adapters.md). Run as a one-shot container — this does
# NOT need the persistent session to be up, so it runs before the container
# under test is started below.
CODEX_VERSION_OUT=$(docker run --rm --entrypoint bash "$IMAGE" -lc "codex --version" 2>&1)
CODEX_ACTUAL=$(echo "$CODEX_VERSION_OUT" | grep -oE '[0-9]+\.[0-9]+\.[0-9]+' | head -1 || true)
if [[ -n "$CODEX_ACTUAL" ]]; then
    LOWEST=$(printf '%s\n%s\n' "$CODEX_MIN_VERSION" "$CODEX_ACTUAL" | sort -V | head -1)
    if [[ "$LOWEST" == "$CODEX_MIN_VERSION" ]]; then
        pass "codex --version meets the $CODEX_MIN_VERSION floor: $CODEX_ACTUAL"
    else
        fail "codex $CODEX_ACTUAL is below the $CODEX_MIN_VERSION floor (.loom/docs/runtime-adapters.md)"
    fi
else
    fail "codex --version produced no parseable version: $CODEX_VERSION_OUT"
fi

# 3. Start the container the way a real session container runs: detached,
# no command override — the image's own ENTRYPOINT (tini + the tmux-server
# entrypoint script) is the whole point under test from here on.
CONTAINER_NAME="loom-session-test-$$"
cleanup() {
    docker rm -f "$CONTAINER_NAME" >/dev/null 2>&1 || true
}
trap cleanup EXIT

if ! docker run -d --name "$CONTAINER_NAME" "$IMAGE" >/dev/null; then
    fail "docker run -d did not start the container at all"
    echo "== $FAILURES failure(s) =="
    exit 1
fi

# 4. tmux session comes up (poll briefly — entrypoint startup is not
# instantaneous) and the container stays running detached (no auto-exit).
TMUX_UP=0
for _ in $(seq 1 20); do
    if docker exec "$CONTAINER_NAME" tmux has-session -t "$TMUX_SESSION_NAME" 2>/dev/null; then
        TMUX_UP=1
        break
    fi
    sleep 0.5
done
if [[ "$TMUX_UP" -eq 1 ]]; then
    pass "tmux session '$TMUX_SESSION_NAME' is live inside the running container"
else
    fail "tmux session '$TMUX_SESSION_NAME' never came up inside the container"
fi

RUNNING=$(docker inspect -f '{{.State.Running}}' "$CONTAINER_NAME" 2>/dev/null || echo false)
if [[ "$RUNNING" == "true" ]]; then
    pass "container stays running detached (no auto-exit)"
else
    fail "container is not running (expected a persistent, still-running container): status=$RUNNING"
fi

# 5. `docker exec` round-trips a REAL exit code, not just 0.
if docker exec "$CONTAINER_NAME" true; then
    pass "docker exec true -> exit 0"
else
    fail "docker exec true did not exit 0"
fi

docker exec "$CONTAINER_NAME" false
FALSE_EXIT=$?
if [[ "$FALSE_EXIT" -eq 1 ]]; then
    pass "docker exec false -> exit 1 (real exit code round-tripped)"
else
    fail "docker exec false exited $FALSE_EXIT, expected 1"
fi

docker exec "$CONTAINER_NAME" bash -c 'exit 42'
ARBITRARY_EXIT=$?
if [[ "$ARBITRARY_EXIT" -eq 42 ]]; then
    pass "docker exec bash -c 'exit 42' -> exit 42 (arbitrary exit code round-tripped)"
else
    fail "docker exec bash -c 'exit 42' exited $ARBITRARY_EXIT, expected 42"
fi

# 6. CODEX_HOME convention: env is set, owned by uid 1000, writable, and
# empty (mount point, not baked content — no profile/secret baked in).
CODEX_HOME_CHECK=$(docker exec "$CONTAINER_NAME" bash -lc '
    echo "HOME_PATH=$CODEX_HOME"
    echo "OWNER_UID=$(stat -c %u "$CODEX_HOME" 2>/dev/null)"
    touch "$CODEX_HOME/.loom-test-write" 2>/dev/null && rm -f "$CODEX_HOME/.loom-test-write" && echo WRITABLE=1
' 2>&1)
if [[ "$CODEX_HOME_CHECK" == *"HOME_PATH=/home/loom/.codex-profile"* \
    && "$CODEX_HOME_CHECK" == *"OWNER_UID=1000"* \
    && "$CODEX_HOME_CHECK" == *"WRITABLE=1"* ]]; then
    pass "CODEX_HOME is set, owned by uid 1000, and writable"
else
    fail "CODEX_HOME check failed: $CODEX_HOME_CHECK"
fi

CODEX_HOME_CONTENTS=$(docker exec "$CONTAINER_NAME" bash -lc 'ls -A "$CODEX_HOME" 2>/dev/null || true')
if [[ -z "$CODEX_HOME_CONTENTS" ]]; then
    pass "CODEX_HOME mount point is empty (no baked profile/secrets)"
else
    fail "CODEX_HOME mount point is NOT empty: $CODEX_HOME_CONTENTS"
fi

# 6b. git trusts bind-mounted repositories regardless of their apparent owner
# (issue #8518): Docker Desktop presents a bind mount as root-owned while the
# image runs as uid 1000, and without safe.directory git reports "dubious
# ownership", Codex sees "not a repository", and dispatch is refused.
# Simulated here with a root-owned repo inside the container itself.
SAFE_DIR_CHECK=$(docker exec -u root "$CONTAINER_NAME" bash -lc '
    mkdir -p /tmp/root-owned-repo && cd /tmp/root-owned-repo && git init -q . 2>/dev/null && chmod -R a+rX /tmp/root-owned-repo
    su -s /bin/bash loom -c "cd /tmp/root-owned-repo && git rev-parse --is-inside-work-tree" 2>&1
' 2>&1 | tail -1)
if [[ "$SAFE_DIR_CHECK" == "true" ]]; then
    pass "git (as uid 1000) treats a root-owned repository as a work tree (safe.directory=*)"
else
    fail "git as uid 1000 refused a root-owned repository: $SAFE_DIR_CHECK"
fi

# 7. No secrets baked in. Best-effort docker-history scan, same pattern set
# docker/worker/test-image.sh uses, plus Codex/CODEX_HOME-adjacent patterns
# specific to this image (an OpenAI API key shape, a baked auth.json, or
# contents accidentally COPYed into the CODEX_HOME mount point).
HISTORY=$(docker history --no-trunc "$IMAGE" 2>&1 || true)
SECRET_HIT=0
for pattern in \
    CLAUDE_CODE_OAUTH_TOKEN \
    GITHUB_TOKEN \
    GH_TOKEN \
    OPENAI_API_KEY \
    'sk-[A-Za-z0-9]{20,}' \
    'accounts\.env' \
    '\.loom/tokens/.*\.token' \
    'auth\.json' \
    '\.codex-profile/.+'; do
    if echo "$HISTORY" | grep -qE "$pattern"; then
        fail "docker history matched a secret-shaped pattern: $pattern"
        SECRET_HIT=1
    fi
done
if [[ "$SECRET_HIT" -eq 0 ]]; then
    pass "no secret-shaped strings found in docker history"
fi

# 8. Core toolchain inherited from the base image is still present (sanity
# check that this layer did not accidentally shadow/break anything).
for bin in git gh jq tmux curl claude codex node npm; do
    if docker exec "$CONTAINER_NAME" bash -lc "command -v $bin >/dev/null 2>&1"; then
        pass "$bin present on PATH inside the running container"
    else
        fail "$bin missing from PATH inside the running container"
    fi
done

# 9. Private-session control bundle (issue #8839) — the image-owned half of the
# `loom-private-control-v1` boundary. This is the only place the claims about
# it can be made against the REAL shipped artifacts: the real guard scripts, the
# real hook wire protocol, and the real pinned Codex CLI. The Rust fixtures in
# loom-daemon/tests/private_workspace_docker prove the host/lease binding on a
# synthetic image; they cannot prove what this image actually ships.
# Full contract, evidence and rollback: defaults/docs/private-control-bundle.md
CONTROL_ROOT=/opt/loom/private-control

CONTROL_MANIFEST=$(docker exec "$CONTAINER_NAME" cat "$CONTROL_ROOT/manifest.json" 2>&1)
if echo "$CONTROL_MANIFEST" | jq -e '.protocol == "loom-private-control-v1" and .control_version == 2' >/dev/null 2>&1; then
    pass "control bundle ships a sealed loom-private-control-v1 manifest"
else
    fail "control bundle manifest missing or not loom-private-control-v1: $CONTROL_MANIFEST"
fi

# The registration must name the IMAGE-OWNED bridge. A registration pointing
# into the worker's own clone is the #8839 escalation: one `rm` disables it.
EXPECTED_REGISTRATION="$CONTROL_ROOT/hooks/guard-codex-bridge.sh --project-root /workspace/repo --loom-hook-version 1"
ACTUAL_REGISTRATION=$(echo "$CONTROL_MANIFEST" | jq -r '.registration // ""' 2>/dev/null)
if [[ "$ACTUAL_REGISTRATION" == "$EXPECTED_REGISTRATION" ]]; then
    pass "managed hook registration names the image-owned bridge"
else
    fail "registration is '$ACTUAL_REGISTRATION', expected '$EXPECTED_REGISTRATION'"
fi

# The manifest records the CLI observed at seal time, not a build argument —
# so this assertion is what makes "0.149.1 is what the bundle was sealed
# against" a fact about the image rather than a claim about the Dockerfile.
SEALED_CLI=$(echo "$CONTROL_MANIFEST" | jq -r '.codex_cli // ""' 2>/dev/null)
OBSERVED_CLI=$(docker exec "$CONTAINER_NAME" bash -lc 'codex --version' 2>&1 | tr -d '\r')
if [[ -n "$SEALED_CLI" && "$SEALED_CLI" == "$OBSERVED_CLI" ]]; then
    pass "control bundle was sealed against the Codex CLI this image ships: $SEALED_CLI"
else
    fail "sealed codex_cli '$SEALED_CLI' does not match the installed '$OBSERVED_CLI'"
fi

# The worker's own view of its boundary. `observe` re-derives every sealed
# digest and PROVES non-writability by attempting real writes, so its verdict
# is evidence, not a restatement of the image's file modes.
#
# THIS container is deliberately not a session: it has no account profile bound
# at all, let alone the read-only per-file binds a real session carries, so the
# only correct answer here is `profile-mutable`. The image alone cannot make the
# control-file claim — that half is mount topology the HOST establishes — and an
# image that answered `ready` without it would be claiming protection it does
# not have. The protected shape is exercised immediately below.
CONTROL_REPORT=$(docker exec "$CONTAINER_NAME" loom-daemon private-workspace control 2>&1)
if echo "$CONTROL_REPORT" | jq -e '.status == "profile-mutable"' >/dev/null 2>&1; then
    pass "control boundary refuses a container whose profile controls are not mount-protected"
else
    fail "a container with no protected account profile did not report profile-mutable: $CONTROL_REPORT"
fi

# ...and the protected shape, against this same shipped image: a synthetic
# profile whose three control files are bound READ-ONLY over their own paths,
# exactly as private_workspace::docker::create binds them. `ready` here means
# the real image's own boundary code accepted a real mount topology, and the
# probe then proves the kernel-level property that topology exists for — a
# worker cannot write, delete, rename away or rename over the hook registration
# it is policed by, while `auth.json` beside it stays refreshable.
# These probes run as the session's own uid 1000, exactly as production does,
# NOT as whoever invokes this script: an account profile is only accessible to
# its owner, so a probe pinned to the caller's uid passes on a developer box
# that happens to be uid 1000 and reports `profile-inaccessible` on any host
# where it is not (a GitHub runner, for one). The synthetic throwaway profile is
# therefore made reachable by MODE rather than by ownership — it holds one fake
# string and never a credential, and directory permissions are not the property
# under test here (mount topology is, and that is uid- and mode-independent).
# The production shape — a 0700 profile with 0600 auth.json, owned by and
# reached as uid 1000 — is proven separately by the Rust Docker fixtures, which
# CI runs under `setpriv --reuid=1000`.
PROBE_USER="1000:1000"
PROTECTED_PROFILE=$(mktemp -d)
chmod 777 "$PROTECTED_PROFILE"
cleanup_protected() { rm -rf "$PROTECTED_PROFILE" 2>/dev/null || true; }
trap 'cleanup; cleanup_protected' EXIT
printf 'synthetic-not-a-credential\n' > "$PROTECTED_PROFILE/auth.json"
chmod 666 "$PROTECTED_PROFILE/auth.json"
# Provisioned by the image's OWN sealed provisioner, through the same
# `provision-controls` endpoint the daemon drives before it creates a session.
PROVISION_OUT=$(docker run --rm --network none --user "$PROBE_USER" --read-only \
    --cap-drop ALL --security-opt no-new-privileges \
    --tmpfs /tmp:rw,nosuid,nodev,size=64m \
    --mount "type=bind,src=$PROTECTED_PROFILE,dst=/home/loom/.codex-profile" \
    --env CODEX_HOME=/home/loom/.codex-profile \
    --entrypoint loom-daemon "$IMAGE" private-workspace provision-controls 2>&1)
if echo "$PROVISION_OUT" | jq -e '.status == "ready"' >/dev/null 2>&1; then
    pass "the image's sealed provisioner establishes the profile's control files"
else
    fail "provision-controls did not report ready against a fresh profile: $PROVISION_OUT"
fi

PROTECTED_MOUNTS=()
for CONTROL_FILE in hooks.json config.toml loom-codex-hooks.json; do
    PROTECTED_MOUNTS+=(--mount "type=bind,src=$PROTECTED_PROFILE/$CONTROL_FILE,dst=/home/loom/.codex-profile/$CONTROL_FILE,readonly")
done
PROTECTED_REPORT=$(docker run --rm --network none --user "$PROBE_USER" --read-only \
    --cap-drop ALL --security-opt no-new-privileges \
    --tmpfs /tmp:rw,nosuid,nodev,size=64m \
    --mount "type=bind,src=$PROTECTED_PROFILE,dst=/home/loom/.codex-profile" \
    "${PROTECTED_MOUNTS[@]}" \
    --env CODEX_HOME=/home/loom/.codex-profile \
    --entrypoint loom-daemon "$IMAGE" private-workspace control 2>&1)
if echo "$PROTECTED_REPORT" | jq -e '.status == "ready" and (.identity | test("^[0-9a-f]{64}$"))' >/dev/null 2>&1; then
    pass "control boundary is ready and bound once the profile controls are bound read-only"
else
    fail "the protected shape is not ready inside the shipped image: $PROTECTED_REPORT"
fi

# The kernel-level property itself: on a read-only mount point a write is
# EROFS and an unlink or rename is EBUSY, while the directory around it stays
# writable. `set -e` is deliberately absent — each attempt is asserted to FAIL.
FREEZE_PROBE='
escalated() { echo "ESCALATED: $1"; exit 1; }
for control in hooks.json config.toml loom-codex-hooks.json; do
    path="$CODEX_HOME/$control"
    printf attack > "$path" 2>/dev/null && escalated "wrote $control"
    rm -f "$path" 2>/dev/null && escalated "removed $control"
    mv "$path" "$path.stolen" 2>/dev/null && escalated "renamed $control away"
    printf attack > "$CODEX_HOME/decoy" || escalated "profile directory is not writable"
    mv "$CODEX_HOME/decoy" "$path" 2>/dev/null && escalated "renamed over $control"
    rm -f "$CODEX_HOME/decoy"
done
printf refreshed > "$CODEX_HOME/auth.json.tmp" && mv "$CODEX_HOME/auth.json.tmp" "$CODEX_HOME/auth.json" \
    || escalated "the canonical atomic auth refresh stopped working"
echo FROZEN
'
FREEZE_OUT=$(docker run --rm --network none --user "$PROBE_USER" --read-only \
    --cap-drop ALL --security-opt no-new-privileges \
    --tmpfs /tmp:rw,nosuid,nodev,size=64m \
    --mount "type=bind,src=$PROTECTED_PROFILE,dst=/home/loom/.codex-profile" \
    "${PROTECTED_MOUNTS[@]}" \
    --env CODEX_HOME=/home/loom/.codex-profile \
    --entrypoint bash "$IMAGE" -lc "$FREEZE_PROBE" 2>&1)
if [[ "$FREEZE_OUT" == *FROZEN* ]]; then
    pass "uid 1000 cannot write, delete, rename away or rename over any profile control file"
else
    fail "a profile control file was reachable from inside the session: $FREEZE_OUT"
fi

if docker exec "$CONTAINER_NAME" bash -lc "
    ! (printf x > $CONTROL_ROOT/hooks/guard-codex-bridge.sh) 2>/dev/null \
    && ! (rm -f $CONTROL_ROOT/hooks/guard-codex-bridge.sh) 2>/dev/null \
    && ! (printf x > $CONTROL_ROOT/attack) 2>/dev/null \
    && ! (printf x > $CONTROL_ROOT/scripts/attack) 2>/dev/null"; then
    pass "uid 1000 cannot write, replace or delete anything in the control bundle"
else
    fail "the control bundle is writable by the session user — the boundary proves nothing"
fi

# `guard-destructive.sh` in the bundle must BE the vendored generic guard. The
# dispatcher form prefers $LOOM_PROJECT_ROOT/.claude/skills/repo/hooks/…, a
# worker-writable path — "switch to another guard" is one of the routes this
# boundary exists to close.
if docker exec "$CONTAINER_NAME" bash -lc "cmp -s $CONTROL_ROOT/hooks/guard-destructive.sh $CONTROL_ROOT/hooks/guard-destructive-generic.sh"; then
    pass "bundled guard-destructive.sh is the vendored generic guard, not the dispatcher"
else
    fail "bundled guard-destructive.sh differs from guard-destructive-generic.sh"
fi

# 10. The #8839 regression itself, against the real guard code in this image and
# a real Codex `pre_tool_use` event. Run twice, and BOTH halves matter: with the
# worker's own `.loom/config.json` as the effective policy the force-push is
# allowed (an allow is silence on this wire) — that is the reported escalation,
# and without it this check would prove nothing; with the manifest's forced
# policy applied to the hook's process the same call fails closed.
PROBE_SCRIPT='
set -u
root=/tmp/loom-control-probe
rm -rf "$root" && mkdir -p "$root/.loom" || exit 90
git init -q -b main "$root" >/dev/null 2>&1 || exit 91
printf "%s" "{\"guards\":{\"forceScope\":\"off\"}}" > "$root/.loom/config.json" || exit 92
event="{\"hook_event_name\":\"PreToolUse\",\"tool_name\":\"shell\",\"tool_input\":{\"command\":[\"bash\",\"-lc\",\"git push --force origin HEAD:main\"],\"workdir\":\"$root\"},\"cwd\":\"$root\",\"session_id\":\"00000000-0000-0000-0000-00000000c0de\",\"tool_use_id\":\"probe\",\"turn_id\":\"probe\",\"model\":\"probe\",\"permission_mode\":\"auto\",\"transcript_path\":\"/dev/null\"}"
bridge=CONTROL_ROOT_PLACEHOLDER/hooks/guard-codex-bridge.sh
# The forced policy is read FROM THE SEALED MANIFEST, never restated here, so
# this check cannot drift away from what the daemon actually applies.
mapfile -t forced < <(jq -r ".policy | to_entries[] | \"\(.key)=\(.value)\"" CONTROL_ROOT_PLACEHOLDER/manifest.json)
echo "--- WORKER_POLICY"
printf "%s" "$event" | bash "$bridge" --project-root "$root" --loom-hook-version 1
echo
echo "--- FORCED_POLICY"
printf "%s" "$event" | env "${forced[@]}" bash "$bridge" --project-root "$root" --loom-hook-version 1
echo
echo "--- END"
'
PROBE_OUT=$(docker exec "$CONTAINER_NAME" bash -lc "${PROBE_SCRIPT//CONTROL_ROOT_PLACEHOLDER/$CONTROL_ROOT}" 2>&1)
WORKER_DECISION=$(echo "$PROBE_OUT" | sed -n '/--- WORKER_POLICY/,/--- FORCED_POLICY/p' | sed '1d;$d')
FORCED_DECISION=$(echo "$PROBE_OUT" | sed -n '/--- FORCED_POLICY/,/--- END/p' | sed '1d;$d')

if [[ -z "${WORKER_DECISION//[[:space:]]/}" ]]; then
    pass "regression reproduced: worker-controlled guards.forceScope:off allows the force-push"
else
    fail "the #8839 escalation did not reproduce, so the check below proves nothing: $PROBE_OUT"
fi

if echo "$FORCED_DECISION" | jq -e '.hookSpecificOutput.hookEventName == "PreToolUse"
        and .hookSpecificOutput.permissionDecision == "deny"
        and (.hookSpecificOutput.permissionDecisionReason | length) > 0' >/dev/null 2>&1; then
    pass "regression closed: the manifest's forced policy denies the same force-push"
else
    fail "force-push over a protected ref was not denied under the forced policy: $PROBE_OUT"
fi

# The deny must be expressible on THIS CLI's wire. Every key below is one the
# shipped binary explicitly refuses (see the strings assertion next); emitting
# any of them would turn a denial into a hook error, i.e. fail open.
if echo "$FORCED_DECISION" | jq -e 'has("decision") or has("continue") or has("stopReason")
        or has("suppressOutput") or (.hookSpecificOutput | has("updatedInput"))' >/dev/null 2>&1; then
    fail "the deny payload carries a field this Codex engine refuses: $FORCED_DECISION"
else
    pass "deny payload is the deny-only shape this Codex engine accepts"
fi

# 11. The bridge pins its tested wire schema at 0.146.0 while this image pins a
# newer CLI. "Newer than the floor" is not evidence, so assert the refusal set
# the pin was derived from is still present in the binary this image ships — a
# Codex bump that changes the wire fails HERE rather than in production.
# `command -v codex` is npm's JS shim, and the package ships several large
# vendored binaries (ripgrep, the code-mode host) beside the CLI itself — so
# select the vendored `bin/codex` explicitly rather than "the first big file".
CODEX_BIN=$(docker exec "$CONTAINER_NAME" bash -lc 'find /home/loom/.npm-global/lib/node_modules/@openai -type f -path "*/vendor/*/bin/codex" 2>/dev/null | head -1' | tr -d '\r')
if [[ -z "$CODEX_BIN" ]]; then
    fail "could not locate the shipped Codex binary to assert its hook wire contract"
else
    MISSING_WIRE=""
    for marker in \
        'PreToolUse hook returned unsupported permissionDecision:allow' \
        'PreToolUse hook returned unsupported permissionDecision:ask' \
        'PreToolUse hook returned unsupported decision:approve' \
        'PreToolUse hook returned unsupported continue:false' \
        'PreToolUse hook returned unsupported stopReason' \
        'PreToolUse hook returned unsupported suppressOutput' \
        'PreToolUse hook returned permissionDecision:deny without a non-empty permissionDecisionReason' \
        'hooks.state."'; do
        if ! docker exec "$CONTAINER_NAME" grep -qaF "$marker" "$CODEX_BIN"; then
            MISSING_WIRE="$MISSING_WIRE
  - $marker"
        fi
    done
    if [[ -z "$MISSING_WIRE" ]]; then
        pass "shipped Codex CLI still carries the pre_tool_use wire contract the bridge pins at $CODEX_MIN_VERSION"
    else
        fail "shipped Codex CLI no longer carries these pinned hook-protocol markers (re-establish the evidence in defaults/docs/private-control-bundle.md before bumping CODEX_VERSION):$MISSING_WIRE"
    fi
fi

echo "== $FAILURES failure(s) =="
exit $((FAILURES > 0 ? 1 : 0))
