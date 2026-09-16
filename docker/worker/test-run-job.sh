#!/usr/bin/env bash
# test-run-job.sh — end-to-end proof of the `run-job` seam (epic #6896 Phase 4,
# issue #7853) driven FROM INSIDE a worker container that has no docker socket.
#
# The hermetic suite (defaults/scripts/tests/test-run-job.sh) covers the seam's
# logic against a fake docker and a fake ssh. This script covers the one thing a
# fake cannot: that the seam actually works from inside the shipped worker
# image, against a real docker daemon on the host, with real containers, real
# logs and real exit codes — and that the worker container itself never sees a
# container-runtime socket.
#
# That is epic #6896's own success criterion for this phase:
#
#   "A worker-container agent runs a docker-backed job via the run-job seam
#    with no docker socket mounted in the worker container."
#
# ## What is real here, and what is a stand-in
#
#   REAL       the worker container (the shipped loom-worker image); the client
#              (`defaults/scripts/run-job.sh`) running inside it with no docker
#              CLI and no socket; the executor (`lib/run-job-exec.sh`) running
#              on the host; the host's real docker daemon; a real job container
#              with real path-parity bind mounts; the job's real stdout,
#              stderr and exit code.
#
#   STAND-IN   only the ssh hop itself. A CI runner has no sshd that a
#              container can log into, so `LOOM_JOB_SSH_CMD` points at a
#              file-dropbox transport that relays exactly what ssh relays: the
#              executor program on stdin, the verb + args in argv, and
#              stdout/stderr/exit status back. Everything above the transport
#              — the whole seam — runs unchanged.
#
# Deliberately NOT a build step, same posture as test-image.sh and
# test-mount-contract.sh: it takes an already-built image tag and drives
# `docker run` against it.
#
# Usage:
#   docker build -f docker/worker/Dockerfile -t loom-worker:test .
#   ./docker/worker/test-run-job.sh loom-worker:test
#
# Env:
#   LOOM_TEST_DOCKER   the docker CLI invocation to use (default `docker`).
#                      May contain arguments, e.g. `LOOM_TEST_DOCKER="sudo docker"`
#                      on a host where the invoking user is not in the docker
#                      group. The executor is pointed at the same invocation.
#
# Skips CLEANLY (exit 0, not a failure) when docker is unavailable or the caller
# cannot reach the docker daemon. Exit 0 = every check passed (or skipped).
# Exit 1 = at least one check failed.

set -euo pipefail

IMAGE="${1:?usage: test-run-job.sh <image-tag>}"

DOCKER=()
read -r -a DOCKER <<<"${LOOM_TEST_DOCKER:-docker}"

# --- Skip cleanly when docker is not usable here -----------------------------
if ! command -v "${DOCKER[0]}" >/dev/null 2>&1; then
    echo "SKIP: '${DOCKER[0]}' not found on PATH — nothing to test here."
    exit 0
fi
if ! "${DOCKER[@]}" info >/dev/null 2>&1; then
    echo "SKIP: docker daemon not reachable (not running, or no permission on this host) — nothing to test here."
    exit 0
fi

FAILURES=0
fail() {
    echo "FAIL: $1" >&2
    [[ -n "${2:-}" ]] && echo "      $2" >&2
    FAILURES=$((FAILURES + 1))
}
pass() { echo "PASS: $1"; }
assert_eq() {
    if [[ "$1" == "$2" ]]; then pass "$3"; else fail "$3" "expected '$1', got '$2'"; fi
}
assert_contains() {
    if [[ "$1" == *"$2"* ]]; then pass "$3"; else fail "$3" "expected to contain '$2'; got: $(head -c 400 <<<"$1")"; fi
}
assert_not_contains() {
    if [[ "$1" != *"$2"* ]]; then pass "$3"; else fail "$3" "expected NOT to contain '$2'; got: $(head -c 400 <<<"$1")"; fi
}

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
SEAM_DIR="$REPO_ROOT/defaults/scripts"
RUN_JOB="$SEAM_DIR/run-job.sh"
EXEC_LIB="$SEAM_DIR/lib/run-job-exec.sh"
if [[ ! -f "$RUN_JOB" || ! -f "$EXEC_LIB" ]]; then
    echo "SKIP: run-job seam not present at $SEAM_DIR — nothing to test here."
    exit 0
fi

echo "== run-job seam end-to-end (client in a socket-less container, executor on the host) =="
echo "   image:    $IMAGE"
echo "   seam:     $SEAM_DIR"

# --- Scratch: the transport dropbox + the parity-mounted job workspace --------
# World-writable throughout: the client runs as the image's uid 1000 while the
# host executor runs as the invoking user, and on a CI runner those differ.
SCRATCH="$(mktemp -d "${TMPDIR:-/tmp}/loom-run-job-e2e.XXXXXX")"
# Canonicalized on purpose: the seam refuses a symlinked mount source (it would
# break the path-parity guarantee, and it is how a refused path could otherwise
# be smuggled past validation — see §4). On a host where TMPDIR sits behind a
# symlink (`/tmp -> /private/tmp` on macOS) an uncanonicalized scratch path
# would be refused, so resolve it once here rather than shipping a test that
# only passes on Linux runners.
SCRATCH="$(cd "$SCRATCH" && pwd -P)"
# Host-only, deliberately NOT mounted into any container: the docker shim the
# executor calls lives here, so nothing that can reach a container runtime is
# ever visible from inside the worker container.
HOSTDIR="$(mktemp -d "${TMPDIR:-/tmp}/loom-run-job-host.XXXXXX")"
LISTENER_PID=""
JOB_IDS=()
# shellcheck disable=SC2329  # invoked indirectly via the EXIT trap below
cleanup() {
    if [[ -n "$LISTENER_PID" ]]; then
        kill "$LISTENER_PID" 2>/dev/null || true
    fi
    local id
    for id in ${JOB_IDS[@]+"${JOB_IDS[@]}"}; do
        "${DOCKER[@]}" rm --force "loom-job-$id" >/dev/null 2>&1 || true
    done
    # Job containers write into the scratch tree as the image's uid; reset
    # ownership from that same uid before the host-side rm (same reason
    # test-mount-contract.sh does). Best-effort throughout.
    "${DOCKER[@]}" run --rm --user 0 -v "$SCRATCH:$SCRATCH" "$IMAGE" \
        chmod -R a+rwX "$SCRATCH" >/dev/null 2>&1 || true
    rm -rf "$SCRATCH" "$HOSTDIR" 2>/dev/null || true
}
trap cleanup EXIT

mkdir -p "$SCRATCH/transport" "$SCRATCH/work" "$HOSTDIR/bin"
chmod 0777 "$SCRATCH" "$SCRATCH/transport" "$SCRATCH/work"

# The executor shells out to `docker`; honour LOOM_TEST_DOCKER there too by
# giving it a one-line shim to call. Host-only by construction (see $HOSTDIR).
cat >"$HOSTDIR/bin/docker" <<EOF
#!/usr/bin/env bash
exec ${DOCKER[*]} "\$@"
EOF
chmod 0755 "$HOSTDIR/bin/docker"

# --- The transport stand-in (container side): a "ssh" that is a file dropbox --
cat >"$SCRATCH/transport/loopback-ssh" <<'LOOPBACK_SSH'
#!/usr/bin/env bash
# Stand-in for `ssh`, run INSIDE the worker container. It relays exactly what
# the real transport relays — the executor program on stdin, the verb + args in
# argv, and the executor's stdout/stderr/exit status back — over a shared
# directory instead of a TCP connection, because a CI runner has no sshd a
# container can log into. It carries no docker access of its own: the container
# still cannot reach any container runtime.
set -uo pipefail
TDIR="${LOOM_TEST_TRANSPORT_DIR:?loopback-ssh: LOOM_TEST_TRANSPORT_DIR is required}"

# argv is `[ssh options...] [user@]host bash -s -- <verb> [args...]`; everything
# after the first literal `--` is the remote command's own arguments.
rest=()
seen=0
for a in "$@"; do
    if [[ $seen -eq 1 ]]; then
        rest+=("$a")
        continue
    fi
    [[ "$a" == "--" ]] && seen=1
done
if [[ $seen -ne 1 ]]; then
    echo "loopback-ssh: malformed transport argv (no '--')" >&2
    exit 255 # what a real ssh exits on its own failures
fi

staging="$(mktemp -d "$TDIR/staging-XXXXXX")" || exit 255
cat >"$staging/program"
printf '%s\n' ${rest[@]+"${rest[@]}"} >"$staging/argv"
chmod -R a+rwX "$staging"
req="$TDIR/req-$(basename "$staging")"
mv "$staging" "$req"
: >"$req/ready"
chmod a+rw "$req/ready"

# Liveness heartbeat. A real ssh connection dropping (because the client was
# killed, or the whole worker container was torn down) delivers SIGHUP to the
# remote program; this dropbox has to emulate that or the executor would keep
# streaming a job whose caller is long gone. The heartbeat dies with this
# container, and the host side HUPs the executor once it goes stale.
(
    while :; do
        touch "$req/alive" 2>/dev/null || exit 0
        sleep 0.5
    done
) &
heartbeat=$!
# shellcheck disable=SC2329  # invoked indirectly via the EXIT trap below
stop_heartbeat() { kill "$heartbeat" 2>/dev/null; }
trap stop_heartbeat EXIT

waited=0
while [[ ! -f "$req/rc" ]]; do
    sleep 0.2
    waited=$((waited + 1))
    if [[ $waited -gt 1500 ]]; then # 300s
        echo "loopback-ssh: executor did not answer within 300s" >&2
        exit 255
    fi
done
cat "$req/out"
cat "$req/err" >&2
exit "$(cat "$req/rc")"
LOOPBACK_SSH
chmod 0755 "$SCRATCH/transport/loopback-ssh"

# --- The transport stand-in (host side): run the executor program for real ----
cat >"$HOSTDIR/bin/executor-listener" <<'LISTENER'
#!/usr/bin/env bash
# Host side of the dropbox transport. For each request it runs the piped
# executor program with the requested verb — a real `bash lib/run-job-exec.sh
# run <spec>` against the host's real docker daemon — and relays its
# stdout/stderr/exit status back into the request directory.
set -uo pipefail
TDIR="${1:?}"

mtime() { stat -c %Y "$1" 2>/dev/null || stat -f %m "$1" 2>/dev/null || echo 0; }

serve() {
    local req="$1"
    local argv=()
    local line
    while IFS= read -r line; do argv+=("$line"); done <"$req/argv"

    # PIPE-backed stdio, deliberately — not a plain `>"$req/out"` redirect.
    #
    # A real ssh hop hands the executor PIPES for stdout/stderr, and a pipe is
    # what makes a leaked background child on the executor observable: it holds
    # the descriptor open, the reader never sees EOF, and the caller blocks.
    # Redirecting straight to FILES made this suite blind to exactly that class
    # of bug (#7875: an orphaned watchdog `sleep` stalled the client for the
    # whole `--timeout`), because a held descriptor on a regular file blocks
    # nobody. The relays below drain each pipe into the file the client polls
    # for, and `rc` is published only after BOTH relays have seen EOF — so a
    # descriptor leak now shows up here as it would over a real ssh hop.
    rm -f "$req/out.pipe" "$req/err.pipe"
    mkfifo "$req/out.pipe" "$req/err.pipe"
    cat "$req/out.pipe" >"$req/out" &
    local out_relay=$!
    cat "$req/err.pipe" >"$req/err" &
    local err_relay=$!

    bash "$req/program" ${argv[@]+"${argv[@]}"} >"$req/out.pipe" 2>"$req/err.pipe" &
    local prog=$! hupped=0 now=0 seen=0
    while kill -0 "$prog" 2>/dev/null; do
        # Stand in for ssh's own connection-drop SIGHUP: if the client's
        # heartbeat goes stale (its container died), hang the executor up. It
        # must DETACH, not kill the job — that is the property under test.
        if [[ $hupped -eq 0 && -f "$req/alive" ]]; then
            now="$(date +%s)"
            seen="$(mtime "$req/alive")"
            if ((now - seen > 4)); then
                kill -HUP "$prog" 2>/dev/null
                hupped=1
            fi
        fi
        sleep 0.5
    done
    wait "$prog"
    local rc=$?
    # EOF on both relays means nothing on the executor side still holds the
    # stream open. Only then is the captured output complete and `rc` honest.
    wait "$out_relay" 2>/dev/null || true
    wait "$err_relay" 2>/dev/null || true
    rm -f "$req/out.pipe" "$req/err.pipe"
    printf '%s\n' "$rc" >"$req/rc.part"
    mv "$req/rc.part" "$req/rc" # atomic: the client polls for rc
}

while :; do
    for req in "$TDIR"/req-*; do
        [[ -d "$req" && -f "$req/ready" && ! -f "$req/claimed" ]] || continue
        : >"$req/claimed"
        serve "$req"
    done
    sleep 0.2
done
LISTENER
chmod 0755 "$HOSTDIR/bin/executor-listener"

LOOM_RUN_JOB_DOCKER="$HOSTDIR/bin/docker" \
    "$HOSTDIR/bin/executor-listener" "$SCRATCH/transport" &
LISTENER_PID=$!

# --- How the client container is launched ------------------------------------
# NOTE the mount list: the seam scripts (read-only) and the scratch tree. There
# is NO -v of /var/run/docker.sock, and there is no --privileged — that absence
# is the entire point, and section 1 below asserts it from inside.
CLIENT_ARGS=(
    run --rm
    -v "$SEAM_DIR:$SEAM_DIR:ro"
    -v "$SCRATCH:$SCRATCH"
    -e "LOOM_TEST_TRANSPORT_DIR=$SCRATCH/transport"
    -e "LOOM_JOB_SSH_CMD=$SCRATCH/transport/loopback-ssh"
    -e "LOOM_JOB_EXECUTOR_HOST=executor.loopback"
    -e "LOOM_WORKSPACE=$SCRATCH/work"
    -w "$SCRATCH/work"
)

echo ""
echo "-- 1. The worker container has no container-runtime socket --"
probe_out=""
probe_rc=0
probe_out="$("${DOCKER[@]}" "${CLIENT_ARGS[@]}" "$IMAGE" bash -lc '
  rc=0
  for s in /var/run/docker.sock /run/docker.sock /var/run/containerd/containerd.sock /run/podman/podman.sock; do
    if [ -e "$s" ]; then echo "PRESENT:$s"; rc=1; fi
  done
  if grep -qE "(docker|containerd|podman|crio)\.sock" /proc/mounts 2>/dev/null; then echo "MOUNTED-RUNTIME-SOCKET"; rc=1; fi
  if command -v docker >/dev/null 2>&1 && docker info >/dev/null 2>&1; then echo "DAEMON-REACHABLE"; rc=1; fi
  echo OK
  exit $rc' 2>&1)" || probe_rc=$?
assert_eq "0" "$probe_rc" "no container-runtime socket is present or reachable inside the worker container"
assert_contains "$probe_out" "OK" "the socket probe ran"
assert_not_contains "$probe_out" "PRESENT:" "no runtime socket file exists in the worker container"
assert_not_contains "$probe_out" "MOUNTED-RUNTIME-SOCKET" "no runtime socket is bind-mounted into the worker container"
assert_not_contains "$probe_out" "DAEMON-REACHABLE" "no container runtime is reachable from the worker container"

echo ""
echo "-- 2. Success: a real job runs on the host, logs and exit code come back --"
JOB_OK="e2e-ok-$$"
JOB_IDS+=("$JOB_OK")
OUT_FILE="$SCRATCH/client-ok.out"
ERR_FILE="$SCRATCH/client-ok.err"
rc=0
ok_start=$SECONDS
"${DOCKER[@]}" "${CLIENT_ARGS[@]}" "$IMAGE" \
    "$RUN_JOB" --id "$JOB_OK" --image "$IMAGE" \
    --mount "$SCRATCH/work" --workdir "$SCRATCH/work" \
    --cpus 1 --memory 1g --timeout 300 \
    -- bash -lc "echo JOB_STDOUT_MARKER; echo JOB_STDERR_MARKER >&2; id -u > '$SCRATCH/work/artifact.txt'; exit 0" \
    >"$OUT_FILE" 2>"$ERR_FILE" || rc=$?
ok_elapsed=$((SECONDS - ok_start))
ok_out="$(cat "$OUT_FILE")"
ok_err="$(cat "$ERR_FILE")"
assert_eq "0" "$rc" "a successful job exits 0 through the seam"
# The job above is instantaneous but carries `--timeout 300`. The client must
# return when the JOB finishes, not when the timeout expires (#7875) — the
# pipe-backed transport above is what makes a stranded watchdog visible here.
if ((ok_elapsed < 120)); then
    pass "the client returns when the job does (${ok_elapsed}s), not when its --timeout 300 does"
else
    fail "the client returns when the job does, not when its --timeout 300 does" \
        "took ${ok_elapsed}s — the executor is stranding a watchdog on the stderr pipe"
fi
assert_contains "$ok_out" "JOB_STDOUT_MARKER" "the job's stdout reached the caller's stdout"
assert_contains "$ok_err" "JOB_STDERR_MARKER" "the job's stderr reached the caller's stderr"
assert_contains "$ok_err" "# LOOM_RUN_JOB_EXIT id=$JOB_OK code=0" "the authoritative exit sentinel came back"
# With no docker CLI and no socket in the container, `auto` must land on the
# loopback executor — the mandatory baseline for a single-host install.
assert_contains "$ok_err" "mode=ssh" "the client used the loopback/ssh executor"
if [[ -f "$SCRATCH/work/artifact.txt" ]]; then
    pass "the job really ran on the host's docker (artifact visible through the parity mount)"
else
    fail "the job really ran on the host's docker (artifact visible through the parity mount)" "no $SCRATCH/work/artifact.txt"
fi
leftover="$("${DOCKER[@]}" ps -a --filter "name=loom-job-$JOB_OK" --format '{{.Names}}' 2>/dev/null || true)"
assert_eq "" "$leftover" "the job container is removed once its exit code has been read"

echo ""
echo "-- 3. Failure: a non-zero exit code is passed through verbatim --"
JOB_FAIL="e2e-fail-$$"
JOB_IDS+=("$JOB_FAIL")
ERR_FILE="$SCRATCH/client-fail.err"
rc=0
"${DOCKER[@]}" "${CLIENT_ARGS[@]}" "$IMAGE" \
    "$RUN_JOB" --id "$JOB_FAIL" --image "$IMAGE" \
    --mount "$SCRATCH/work" --workdir "$SCRATCH/work" \
    -- bash -lc 'echo FAILING_JOB_STDERR >&2; exit 42' \
    >/dev/null 2>"$ERR_FILE" || rc=$?
fail_err="$(cat "$ERR_FILE")"
assert_eq "42" "$rc" "a failing job's exit code is passed through verbatim"
assert_contains "$fail_err" "FAILING_JOB_STDERR" "a failing job's stderr is passed through"
assert_contains "$fail_err" "# LOOM_RUN_JOB_EXIT id=$JOB_FAIL code=42" "the failure sentinel carries the job's real code"

echo ""
echo "-- 4. A socket mount cannot traverse the seam, even from inside --"
JOB_EVIL="e2e-evil-$$"
JOB_IDS+=("$JOB_EVIL")
rc=0
evil_out="$("${DOCKER[@]}" "${CLIENT_ARGS[@]}" "$IMAGE" \
    "$RUN_JOB" --id "$JOB_EVIL" --image "$IMAGE" \
    --mount /var/run/docker.sock -- bash -lc 'true' 2>&1)" || rc=$?
assert_eq "78" "$rc" "a job requesting a docker-socket mount is rejected (EX_CONFIG)"
assert_contains "$evil_out" "host-root-equivalent" "the refusal cites the security rationale"
created="$("${DOCKER[@]}" ps -a --filter "name=loom-job-$JOB_EVIL" --format '{{.Names}}' 2>/dev/null || true)"
assert_eq "" "$created" "no container was created for the rejected job"

# ...and the refusal cannot be walked around with a symlink (#7875). This is
# the adversary ADR-0017 Decision 3 is actually about: an agent inside the
# worker container plants the link in an rw parity mount it already holds, and
# the link's own name matches no refusal pattern. Docker resolves a bind's
# SOURCE on the executor host, so without resolution the real socket would land
# in the job container.
JOB_SYM="e2e-symlink-$$"
JOB_IDS+=("$JOB_SYM")
rc=0
sym_out="$("${DOCKER[@]}" "${CLIENT_ARGS[@]}" "$IMAGE" bash -lc "
  ln -sf /var/run/docker.sock '$SCRATCH/work/innocent.sock'
  '$RUN_JOB' --id '$JOB_SYM' --image '$IMAGE' --mount '$SCRATCH/work/innocent.sock' -- true" 2>&1)" || rc=$?
assert_eq "78" "$rc" "a symlink pointing at the docker socket is rejected too (EX_CONFIG)"
assert_contains "$sym_out" "is a symlink to" "the symlink refusal names the real target it resolves to"
created="$("${DOCKER[@]}" ps -a --filter "name=loom-job-$JOB_SYM" --format '{{.Names}}' 2>/dev/null || true)"
assert_eq "" "$created" "no container was created for the smuggled socket mount"
rm -f "$SCRATCH/work/innocent.sock" 2>/dev/null || true

echo ""
echo "-- 5. An unreachable executor is EX_UNAVAILABLE, never a fake job result --"
rc=0
down_out="$("${DOCKER[@]}" "${CLIENT_ARGS[@]}" \
    -e "LOOM_JOB_SSH_CMD=$SCRATCH/transport/no-such-transport" \
    "$IMAGE" \
    "$RUN_JOB" --id "e2e-down-$$" --image "$IMAGE" -- bash -lc 'true' 2>&1)" || rc=$?
assert_eq "69" "$rc" "an unreachable executor exits 69 (EX_UNAVAILABLE)"
assert_contains "$down_out" "did NOT run" "the client says plainly that the job never ran"

echo ""
echo "-- 6. Restart safety: killing the client does not kill the job --"
# The client is what a daemon restart would take down. Start a long job, kill
# the client container, then prove the job is still running on the host and its
# exit code is still recoverable by `attach` (#5119 drain semantics).
JOB_DRAIN="e2e-drain-$$"
JOB_IDS+=("$JOB_DRAIN")
CLIENT_NAME="loom-run-job-client-$$"
"${DOCKER[@]}" run --rm --name "$CLIENT_NAME" \
    -v "$SEAM_DIR:$SEAM_DIR:ro" -v "$SCRATCH:$SCRATCH" \
    -e "LOOM_TEST_TRANSPORT_DIR=$SCRATCH/transport" \
    -e "LOOM_JOB_SSH_CMD=$SCRATCH/transport/loopback-ssh" \
    -e "LOOM_WORKSPACE=$SCRATCH/work" -w "$SCRATCH/work" \
    "$IMAGE" \
    "$RUN_JOB" --id "$JOB_DRAIN" --image "$IMAGE" \
    --mount "$SCRATCH/work" --workdir "$SCRATCH/work" \
    -- bash -lc 'sleep 10; echo DRAINED_JOB_FINISHED; exit 17' \
    >/dev/null 2>&1 &
CLIENT_BG=$!

job_started=0
for _ in $(seq 1 100); do
    if [[ -n "$("${DOCKER[@]}" ps --filter "name=loom-job-$JOB_DRAIN" --format '{{.Names}}' 2>/dev/null || true)" ]]; then
        job_started=1
        break
    fi
    sleep 0.2
done
assert_eq "1" "$job_started" "the job container started on the host"

"${DOCKER[@]}" kill "$CLIENT_NAME" >/dev/null 2>&1 || true
wait "$CLIENT_BG" 2>/dev/null || true
still_running="$("${DOCKER[@]}" ps --filter "name=loom-job-$JOB_DRAIN" --format '{{.Names}}' 2>/dev/null || true)"
assert_contains "$still_running" "loom-job-$JOB_DRAIN" "the job survives the client's death (teardown is not cancellation)"

ERR_FILE="$SCRATCH/client-attach.err"
OUT_FILE="$SCRATCH/client-attach.out"
rc=0
"${DOCKER[@]}" "${CLIENT_ARGS[@]}" "$IMAGE" \
    "$RUN_JOB" attach "$JOB_DRAIN" >"$OUT_FILE" 2>"$ERR_FILE" || rc=$?
assert_eq "17" "$rc" "reattaching after the client's death recovers the job's real exit code"
assert_contains "$(cat "$OUT_FILE")" "DRAINED_JOB_FINISHED" "reattach replays the job's output from its first line"
assert_contains "$(cat "$ERR_FILE")" "# LOOM_RUN_JOB_EXIT id=$JOB_DRAIN code=17" "reattach emits the exit sentinel"

echo ""
if ((FAILURES > 0)); then
    echo "== $FAILURES check(s) FAILED =="
    exit 1
fi
echo "== all run-job end-to-end checks passed =="
exit 0
