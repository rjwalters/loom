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

# 12. Does the ENGINE actually INVOKE the hook? (issue #8839 acceptance
# criterion 3.) Everything above proves the bridge denies when it is run and
# that the registration the CLI would read names the image-owned copy — none of
# it proves the CLI dispatches to it. A hook that is registered but never
# invoked fails open silently, and on this CLI that is not hypothetical (see the
# untrusted control below). So drive the REAL `codex exec` through the real hook
# engine, with a scripted loopback model provider standing in for the model, and
# watch what happens to a real force-push.
#
# Credential-free and hermetic: `--network none`, a synthetic profile holding
# one fake string, a local bare repo as the "remote", a provider bound to
# 127.0.0.1 that speaks a two-event Responses stream and knows nothing but the
# one command it is told to emit. No account, no token, no model call leaves
# the container (there is nowhere for it to go).
#
# Hook trust is REAL, not waived. Codex persists it only through an interactive
# TUI decision, so this section makes that decision the way an operator does —
# it runs the shipped TUI under tmux in a throwaway container and answers the
# prompt — rather than passing `--dangerously-bypass-hook-trust` or writing a
# `trusted_hash` by hand. #8839 forbids both, and neither appears anywhere in
# this script or in shipped Loom code (`spawn-codex.sh` refuses the flag,
# asserted by defaults/scripts/tests/test-provision-codex-hooks.sh). What is
# measured is therefore the production path exactly as it runs.
#
# Two profiles, provisioned and registered identically, differing ONLY in the
# answer given to that one prompt:
#
#   trusted    "Trust all and continue"          -> config.toml gains hooks.state
#   untrusted  "Continue without trusting"       -> config.toml gains no hook trust
#
# and the same two turns are driven against each:
#
#   force-push   trusted   -> BLOCKED by the engine, remote ref unmoved
#   benign write trusted   -> RUNS (the hook is consulted and allows, so the
#                             block above is a decision, not a blanket refusal)
#   force-push   untrusted -> no hook event at all, and the push LANDS
#
# That last one is the control that makes the first non-vacuous, and it is the
# executable form of the fail-open finding in
# defaults/docs/private-control-bundle.md: an untrusted hook is skipped with no
# error, no warning and no `doctor` finding. Because the two profiles differ in
# nothing else, the difference in outcome is attributable to hook trust alone.
ENGINE_DIR=$(mktemp -d)
chmod 755 "$ENGINE_DIR"
cleanup_engine() { rm -rf "$ENGINE_DIR" 2>/dev/null || true; }
trap 'cleanup; cleanup_protected; cleanup_engine' EXIT

# An always-allow hook beside Loom's, purely to capture the payload the ENGINE
# delivers. It is what turns "the bridge denied something" into "the bridge
# denied the exact event this CLI version emits" — the shape is asserted below,
# so a Codex release that renames the tool or restructures `tool_input` fails
# here instead of silently reaching a bridge that can no longer classify it.
# Both tmpfs mounts are `noexec` (Docker's default), so the recorder is invoked
# via `bash <path>` and bind-mounted from the host.
cat > "$ENGINE_DIR/recorder.sh" <<'RECORDER'
#!/usr/bin/env bash
cat > /workspace/engine-event.json
exit 0
RECORDER
chmod 644 "$ENGINE_DIR/recorder.sh"

# The TUI launcher, bind-mounted rather than inlined into the tmux command: the
# provider override is a TOML literal full of quotes that would not survive the
# `tmux new-session "<cmd>"` round-trip intact. The override only keeps the TUI
# from demanding a login; no provider is listening during the trust step and
# none is needed — the trust prompts come before any model call.
cat > "$ENGINE_DIR/engine-tui.sh" <<'ENGINE_TUI'
#!/usr/bin/env bash
export HOME=/workspace/home
export TMPDIR=/workspace/tmp
export FIXTURE_KEY=synthetic-not-a-credential
cd /workspace/repo || exit 1
exec codex \
    -c model=fixture-model \
    -c model_provider=fixture \
    -c 'model_providers.fixture={name="fixture",base_url="http://127.0.0.1:8099/v1",env_key="FIXTURE_KEY",wire_api="responses",request_max_retries=0,stream_max_retries=0}' \
    -s danger-full-access
ENGINE_TUI
chmod 644 "$ENGINE_DIR/engine-tui.sh"

# Drive that TUI to the hook-trust prompt and answer it with $1. Waits on the
# prompt TEXT rather than on a sleep, so a slow container is slow rather than
# flaky, and every wait is bounded. The TUI must not be piped (`stdout is not a
# terminal`), so the pane is read back with `capture-pane`.
cat > "$ENGINE_DIR/engine-trust.sh" <<'ENGINE_TRUST'
set -u
CHOICE="$1"
export CODEX_HOME=/home/loom/.codex-profile
export HOME=/workspace/home
export TMPDIR=/workspace/tmp
mkdir -p "$HOME" "$TMPDIR" /workspace/repo || exit 90
# Codex asks about the directory it is started in, and only offers project-local
# hooks for a real working tree, so give it one.
git init -q -b main /workspace/repo || exit 91

await() {
    local want="$1" i
    for i in $(seq 1 60); do
        case "$(tmux capture-pane -p -t trust 2>/dev/null)" in
            *"$want"*) return 0 ;;
        esac
        sleep 1
    done
    echo "TRUST_TIMEOUT waiting for: $want"
    tmux capture-pane -p -t trust 2>/dev/null
    return 1
}

tmux new-session -d -s trust -x 200 -y 50 "bash /opt/loom-engine-tui.sh" || exit 92
# "Do you trust the contents of this directory?" — option 1 is preselected.
await "trust the contents of this directory" || exit 93
tmux send-keys -t trust Enter
# "Hooks can run outside the sandbox after you trust them." — 2 = trust all,
# 3 = continue without trusting.
await "Hooks can run outside the sandbox" || exit 94
tmux send-keys -t trust "$CHOICE"
sleep 1
tmux send-keys -t trust Enter
# The TUI is up once it stops showing a prompt; either way config.toml is
# written by then. Wait for the composer, not for a fixed delay.
await "Ask Codex to do anything" || exit 95
tmux kill-session -t trust 2>/dev/null
echo "TRUST_DONE"
ENGINE_TRUST
chmod 644 "$ENGINE_DIR/engine-trust.sh"

# Provision one profile with the image's own sealed provisioner through the same
# endpoint the daemon drives before it creates a session (so the registration
# under test is the production one, not one this script wrote), add the payload
# recorder, then make the hook-trust decision $2 in the shipped TUI.
engine_profile() {
    local name="$1" choice="$2" dir="$ENGINE_DIR/$1" out
    mkdir -p "$dir"
    chmod 777 "$dir"
    printf 'synthetic-not-a-credential\n' > "$dir/auth.json"
    chmod 666 "$dir/auth.json"

    out=$(docker run --rm --network none --user "$PROBE_USER" --read-only \
        --cap-drop ALL --security-opt no-new-privileges \
        --tmpfs /tmp:rw,nosuid,nodev,size=64m \
        --mount "type=bind,src=$dir,dst=/home/loom/.codex-profile" \
        --env CODEX_HOME=/home/loom/.codex-profile \
        --entrypoint loom-daemon "$IMAGE" private-workspace provision-controls 2>&1)
    if ! echo "$out" | jq -e '.status == "ready"' >/dev/null 2>&1; then
        fail "could not provision the '$name' engine-probe profile: $out"
        return 1
    fi

    # Merged from INSIDE a container as uid 1000: the provisioner wrote
    # hooks.json 0600 as the profile's owner, which is not whoever runs this
    # script. Truncate in place rather than replacing the file, so ownership
    # and mode survive. Registered BEFORE the trust step, so "trust all"
    # covers the recorder too.
    if ! out=$(docker run --rm --network none --user "$PROBE_USER" --read-only \
        --cap-drop ALL --security-opt no-new-privileges \
        --tmpfs /tmp:rw,nosuid,nodev,size=64m \
        --mount "type=bind,src=$dir,dst=/home/loom/.codex-profile" \
        --env CODEX_HOME=/home/loom/.codex-profile \
        --entrypoint bash "$IMAGE" -lc 'jq '"'"'.hooks.PreToolUse += [{"matcher":"*","hooks":[{"type":"command","command":"bash /opt/loom-engine-recorder.sh","timeout":30}]}]'"'"' "$CODEX_HOME/hooks.json" > /tmp/hooks.json && cat /tmp/hooks.json > "$CODEX_HOME/hooks.json"' 2>&1); then
        fail "could not register the payload recorder in the '$name' profile: $out"
        return 1
    fi

    # The profile is READ-WRITE here on purpose: this is the operator step that
    # happens before a session exists, and it is the only writer of hook trust.
    if ! out=$(docker run --rm --network none --user "$PROBE_USER" --read-only \
        --cap-drop ALL --security-opt no-new-privileges \
        --tmpfs /tmp:rw,nosuid,nodev,size=128m \
        --tmpfs /workspace:rw,size=128m,mode=1777 \
        --mount "type=bind,src=$dir,dst=/home/loom/.codex-profile" \
        --mount "type=bind,src=$ENGINE_DIR/engine-tui.sh,dst=/opt/loom-engine-tui.sh,readonly" \
        --mount "type=bind,src=$ENGINE_DIR/engine-trust.sh,dst=/opt/loom-engine-trust.sh,readonly" \
        --env CODEX_HOME=/home/loom/.codex-profile \
        --entrypoint bash "$IMAGE" -lc "bash /opt/loom-engine-trust.sh $choice" 2>&1); then
        fail "could not drive the shipped TUI to the hook-trust prompt for '$name': $out"
        return 1
    fi
    return 0
}

engine_profile trusted 2
engine_profile untrusted 3

# Read a profile's trust state from INSIDE a container as uid 1000, never from
# the host. The provisioner writes `config.toml` 0600 owned by uid 1000, and
# whoever runs this script is only that uid by coincidence — it is on a
# developer host where the login user happens to be uid 1000, and is NOT on a
# GitHub Actions runner (uid 1001). A host-side `grep` therefore reads nothing
# there and reports "no trust established" for a profile that is perfectly
# trusted, which is exactly how the first version of this assertion failed in
# CI while passing locally. Everything else in this section already crosses the
# boundary through a container, so this does too.
engine_trust_state() {
    docker run --rm --network none --user "$PROBE_USER" --read-only \
        --cap-drop ALL --security-opt no-new-privileges \
        --mount "type=bind,src=$ENGINE_DIR/$1,dst=/home/loom/.codex-profile,readonly" \
        --entrypoint bash "$IMAGE" -lc \
        'if grep -q trusted_hash /home/loom/.codex-profile/config.toml 2>/dev/null; then
             echo TRUSTED
         else
             echo "UNTRUSTED config.toml=[$(cat /home/loom/.codex-profile/config.toml 2>&1)]"
         fi' 2>&1
}

# The one difference between the two profiles, asserted rather than assumed —
# without this, a trust step that silently did nothing would make the whole
# section a comparison of two identical sessions.
TRUSTED_STATE=$(engine_trust_state trusted)
UNTRUSTED_STATE=$(engine_trust_state untrusted)
if [[ "$TRUSTED_STATE" == "TRUSTED" && "$UNTRUSTED_STATE" == UNTRUSTED* ]]; then
    pass "the shipped TUI persisted real hook trust in one profile and not the other"
else
    fail "hook trust was not established exactly once by the TUI: trusted=[$TRUSTED_STATE] untrusted=[$UNTRUSTED_STATE]"
fi

# The probe body itself. Bind-mounted read-only rather than passed as a `-lc`
# string: it carries a nested heredoc (the provider) and would not survive the
# quoting round-trip intact.
cat > "$ENGINE_DIR/engine-probe.sh" <<'ENGINE_PROBE'
set -u
CONTROL_ROOT=/opt/loom/private-control
export CODEX_HOME=/home/loom/.codex-profile
export HOME=/workspace/home
export TMPDIR=/workspace/tmp
mkdir -p "$HOME" "$TMPDIR" || exit 90

MODE="$1"
FORCE_PUSH='git push --force origin HEAD:main'

# A disposable bare repo as the "remote", with `main` as the protected ref the
# guard polices. Local on purpose: the force-push must be able to LAND in the
# untrusted control for the trusted block to mean anything, and nothing here may
# reach a real forge.
git init -q --bare /workspace/remote.git || exit 91
git init -q -b main /workspace/repo || exit 91
cd /workspace/repo || exit 91
git config user.email fixture@example.invalid
git config user.name Fixture
git config commit.gpgsign false
git remote add origin /workspace/remote.git
printf 'base\n' > file
git add file
git commit -qm base
git push -q origin main
# Rewrite local history so the force-push genuinely MOVES the remote ref when
# it is allowed to run — a push that would be a no-op proves nothing.
printf 'rewritten\n' > file
git add file
git commit -q --amend -m rewritten
echo "${MODE}_BASE_REF $(git -C /workspace/remote.git rev-parse main)"

# The model, scripted: one exec_command call carrying $FIXTURE_COMMAND, then a
# final message. Loopback only; the container has no network at all.
cat > "$TMPDIR/provider.js" <<'PROVIDER'
const http = require('http');
const command = process.env.FIXTURE_COMMAND;
const usage = {input_tokens: 1, output_tokens: 1, total_tokens: 2};
function sse(res, events) {
  res.writeHead(200, {'Content-Type': 'text/event-stream'});
  for (const event of events) {
    res.write('event: ' + event.type + '\ndata: ' + JSON.stringify(event) + '\n\n');
  }
  res.end();
}
http.createServer((req, res) => {
  let body = '';
  req.on('data', (chunk) => { body += chunk; });
  req.on('end', () => {
    if (!body.includes('function_call_output')) {
      sse(res, [
        {type: 'response.created', response: {id: 'r1', status: 'in_progress'}},
        {type: 'response.output_item.done', output_index: 0, item: {
          type: 'function_call', id: 'fc1', call_id: 'call_1',
          name: 'exec_command', arguments: JSON.stringify({cmd: command})}},
        {type: 'response.completed', response: {id: 'r1', status: 'completed', output: [], usage}},
      ]);
    } else {
      sse(res, [
        {type: 'response.created', response: {id: 'r2', status: 'in_progress'}},
        {type: 'response.output_item.done', output_index: 0, item: {
          type: 'message', id: 'm2', role: 'assistant', status: 'completed',
          content: [{type: 'output_text', text: 'FIXTURE-TURN-DONE'}]}},
        {type: 'response.completed', response: {id: 'r2', status: 'completed', output: [], usage}},
      ]);
    }
  });
}).listen(8099, '127.0.0.1');
PROVIDER

# The forced policy comes from the sealed manifest, never restated here, so
# this probe cannot drift away from what the daemon actually applies.
mapfile -t FORCED < <(jq -r '.policy | to_entries[] | "\(.key)=\(.value)"' "$CONTROL_ROOT/manifest.json")
export FIXTURE_KEY=synthetic-not-a-credential

# The provider is selected entirely through `-c` overrides, so `config.toml`
# stays exactly what the sealed provisioner and the operator's trust decision
# left there (and is bound read-only anyway). `danger-full-access` because THIS
# CONTAINER is the sandbox: Codex's own sandbox needs unprivileged user
# namespaces, which a `--cap-drop ALL` container does not have. The property
# under test is the hook, not the sandbox. No trust flag is passed — the whole
# point is that trust is already real in this profile.
turn() {
    local command="$1"
    FIXTURE_COMMAND="$command" node "$TMPDIR/provider.js" &
    local provider=$! i
    for i in 1 2 3 4 5 6 7 8 9 10; do
        (exec 3<>/dev/tcp/127.0.0.1/8099) 2>/dev/null && break
        sleep 1
    done
    env "${FORCED[@]}" codex exec \
        -c model=fixture-model \
        -c model_provider=fixture \
        -c 'model_providers.fixture={name="fixture",base_url="http://127.0.0.1:8099/v1",env_key="FIXTURE_KEY",wire_api="responses",request_max_retries=0,stream_max_retries=0}' \
        -C /workspace/repo -s danger-full-access "run the requested command" </dev/null 2>&1
    kill "$provider" 2>/dev/null
    wait "$provider" 2>/dev/null
}

rm -f /workspace/engine-event.json
echo "${MODE}_FORCE_BEGIN"
turn "$FORCE_PUSH"
echo "${MODE}_FORCE_END"
echo "${MODE}_REF_AFTER_FORCE $(git -C /workspace/remote.git rev-parse main)"
echo "${MODE}_FORCE_EVENT $(test -s /workspace/engine-event.json && echo PRESENT || echo ABSENT)"
echo "${MODE}_EVENT_JSON $(tr -d '\n' < /workspace/engine-event.json 2>/dev/null)"

rm -f /workspace/engine-event.json /workspace/repo/allowed.txt
echo "${MODE}_BENIGN_BEGIN"
turn 'printf allowed > /workspace/repo/allowed.txt'
echo "${MODE}_BENIGN_END"
echo "${MODE}_BENIGN_MARKER $(cat /workspace/repo/allowed.txt 2>/dev/null || echo ABSENT)"
ENGINE_PROBE
chmod 644 "$ENGINE_DIR/engine-probe.sh"

# Run the identical probe against one profile. The control files are bound
# READ-ONLY exactly as a session binds them, which makes this also the only
# place that proves the real CLI can still run with them frozen (it writes its
# session state into the profile DIRECTORY around them).
engine_run() {
    local name="$1" mounts control
    mounts=()
    for control in hooks.json config.toml loom-codex-hooks.json; do
        mounts+=(--mount "type=bind,src=$ENGINE_DIR/$name/$control,dst=/home/loom/.codex-profile/$control,readonly")
    done
    docker run --rm --network none --user "$PROBE_USER" --read-only \
        --cap-drop ALL --security-opt no-new-privileges \
        --tmpfs /tmp:rw,nosuid,nodev,size=256m \
        --tmpfs /workspace:rw,size=256m,mode=1777 \
        --mount "type=bind,src=$ENGINE_DIR/$name,dst=/home/loom/.codex-profile" \
        "${mounts[@]}" \
        --mount "type=bind,src=$ENGINE_DIR/recorder.sh,dst=/opt/loom-engine-recorder.sh,readonly" \
        --mount "type=bind,src=$ENGINE_DIR/engine-probe.sh,dst=/opt/loom-engine-probe.sh,readonly" \
        --env CODEX_HOME=/home/loom/.codex-profile \
        --entrypoint bash "$IMAGE" -lc "bash /opt/loom-engine-probe.sh $2" 2>&1
}

TRUSTED_OUT=$(engine_run trusted TRUSTED)
UNTRUSTED_OUT=$(engine_run untrusted UNTRUSTED)

# `$1` is the whole probe output, `$2` the line key. Read with bash's own
# parameter expansion rather than `grep -m1 | cut`: under `set -o pipefail` an
# early-exit consumer can close the pipe while the producer is still writing,
# which reports the pipeline as failed (scripts/check-pipefail-early-exit.sh).
# The haystack is framed with newlines so the first and last lines of the probe
# output match the same patterns as any interior line.
engine_line() {
    local hay=$'\n'"$1" rest
    rest="${hay#*$'\n'"$2" }"
    [[ "$rest" == "$hay" ]] && return 1
    printf '%s' "${rest%%$'\n'*}"
}
engine_turn() {
    local hay=$'\n'"$1"$'\n' rest
    rest="${hay#*$'\n'"$2"$'\n'}"
    [[ "$rest" == "$hay" ]] && return 1
    printf '%s' "${rest%%$'\n'"$3"$'\n'*}"
}

TRUSTED_BASE=$(engine_line "$TRUSTED_OUT" TRUSTED_BASE_REF)
UNTRUSTED_BASE=$(engine_line "$UNTRUSTED_OUT" UNTRUSTED_BASE_REF)
if [[ -z "$TRUSTED_BASE" || -z "$UNTRUSTED_BASE" ]]; then
    fail "the engine probe produced no output to assert on: trusted=[$TRUSTED_OUT] untrusted=[$UNTRUSTED_OUT]"
fi

# The claim this section exists to make: the real engine, with real operator
# trust and no waiver of any kind, dispatched to the image-owned bridge and
# honored its deny.
TRUSTED_FORCE=$(engine_turn "$TRUSTED_OUT" TRUSTED_FORCE_BEGIN TRUSTED_FORCE_END)
if [[ "$TRUSTED_FORCE" == *"Command blocked by PreToolUse hook"* \
    && "$TRUSTED_FORCE" == *"force operation targets protected branch"* ]]; then
    pass "the real Codex CLI, with real hook trust and no waiver, invoked the image-owned bridge and blocked the force-push"
else
    fail "the real CLI did not block a force-push through the trusted registered hook: $TRUSTED_FORCE"
fi
if [[ -n "$TRUSTED_BASE" && "$(engine_line "$TRUSTED_OUT" TRUSTED_REF_AFTER_FORCE)" == "$TRUSTED_BASE" ]]; then
    pass "the disposable remote's protected ref never moved while the hook was live"
else
    fail "the protected ref moved despite the hook: $TRUSTED_BASE -> $(engine_line "$TRUSTED_OUT" TRUSTED_REF_AFTER_FORCE)"
fi

# The payload the engine actually delivered. `shell`/argv was the 0.146.0
# shape; THIS is what 0.149.1 emits, and the bridge's tool classifier and
# command extractor must both keep handling it.
ENGINE_EVENT=$(engine_line "$TRUSTED_OUT" TRUSTED_EVENT_JSON)
if echo "$ENGINE_EVENT" | jq -e '.hook_event_name == "PreToolUse"
        and .tool_name == "Bash"
        and (.tool_input.command | type) == "string"
        and (.tool_input.command | test("push --force"))' >/dev/null 2>&1; then
    pass "engine-delivered pre_tool_use payload matches the shape the bridge classifies"
else
    fail "the engine delivered an unexpected pre_tool_use payload (update guard-codex-bridge.sh and defaults/docs/private-control-bundle.md): $ENGINE_EVENT"
fi

# An allowed call still runs, so the block above is a decision and not a
# blanket refusal of every tool call.
if [[ "$(engine_line "$TRUSTED_OUT" TRUSTED_BENIGN_MARKER)" == "allowed" ]]; then
    pass "a benign tool call is allowed through the same live hook and executes"
else
    fail "the hook blocked a benign tool call: $(engine_turn "$TRUSTED_OUT" TRUSTED_BENIGN_BEGIN TRUSTED_BENIGN_END)"
fi

# The control. Same image, same profile contents, same probe — only the answer
# to the hook-trust prompt differs. An UNTRUSTED hook is skipped with no error,
# no warning and no doctor finding, and the force-push lands. That is why Loom
# proves hook readiness itself before dispatching a mutable role, and it is what
# makes the trusted result above a real result rather than a vacuous one.
if [[ "$(engine_line "$UNTRUSTED_OUT" UNTRUSTED_FORCE_EVENT)" == "ABSENT" \
    && -n "$UNTRUSTED_BASE" \
    && "$(engine_line "$UNTRUSTED_OUT" UNTRUSTED_REF_AFTER_FORCE)" != "$UNTRUSTED_BASE" ]]; then
    pass "without persisted hook trust the identical session runs unhooked and the escalation reproduces"
else
    fail "expected an untrusted hook to be skipped and the force-push to land, so the checks above are non-vacuous: $(engine_turn "$UNTRUSTED_OUT" UNTRUSTED_FORCE_BEGIN UNTRUSTED_FORCE_END)"
fi

echo "== $FAILURES failure(s) =="
exit $((FAILURES > 0 ? 1 : 0))
