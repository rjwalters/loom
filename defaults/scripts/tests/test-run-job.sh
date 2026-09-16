#!/usr/bin/env bash
# test-run-job.sh — tests for the `run-job` seam (epic #6896 Phase 4, #7853):
# `run-job.sh` (client) + `lib/run-job-exec.sh` (executor).
#
# Style matches the sibling suites — plain bash, hand-rolled assertions. Bats
# is NOT used in this repository.
#
# Everything here runs against a FAKE docker (and, for the ssh transport, a
# fake ssh that really executes the piped executor program locally), so the
# suite is hermetic and needs no docker daemon, no ssh daemon and no network.
#
# Usage:
#   ./.loom/scripts/tests/test-run-job.sh

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SCRIPTS_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"

RUN_JOB="$SCRIPTS_DIR/run-job.sh"
EXEC_LIB="$SCRIPTS_DIR/lib/run-job-exec.sh"

if [[ ! -f "$RUN_JOB" || ! -f "$EXEC_LIB" ]]; then
    echo "SKIP: run-job seam not found (run-job.sh / lib/run-job-exec.sh)" >&2
    exit 0
fi
if ! command -v jq >/dev/null 2>&1; then
    echo "SKIP: jq is required by the run-job seam and is not installed" >&2
    exit 0
fi

RED='\033[0;31m'
GREEN='\033[0;32m'
NC='\033[0m'
TESTS_RUN=0
TESTS_PASSED=0
TESTS_FAILED=0

pass() {
    TESTS_RUN=$((TESTS_RUN + 1))
    TESTS_PASSED=$((TESTS_PASSED + 1))
    echo -e "  ${GREEN}PASS${NC}: $1"
}
fail() {
    TESTS_RUN=$((TESTS_RUN + 1))
    TESTS_FAILED=$((TESTS_FAILED + 1))
    echo -e "  ${RED}FAIL${NC}: $1"
    [[ -n "${2:-}" ]] && echo "        $2"
}
assert_eq() {
    if [[ "$1" == "$2" ]]; then pass "$3"; else fail "$3" "expected '$1', got '$2'"; fi
}
assert_contains() {
    if [[ "$1" == *"$2"* ]]; then pass "$3"; else fail "$3" "expected to contain '$2'; got: $(head -c 400 <<<"$1")"; fi
}
assert_not_contains() {
    if [[ "$1" != *"$2"* ]]; then pass "$3"; else fail "$3" "expected NOT to contain '$2'; got: $(head -c 400 <<<"$1")"; fi
}

TMP_ROOT="$(mktemp -d)"
cleanup() { rm -rf "$TMP_ROOT"; }
trap cleanup EXIT

# --- Fake docker -------------------------------------------------------------
# State lives under $FAKE_DOCKER_STATE: one file per "container". Behaviour is
# driven by FAKE_JOB_* env vars set per test case.
FAKE_BIN="$TMP_ROOT/bin"
mkdir -p "$FAKE_BIN"
cat >"$FAKE_BIN/docker" <<'FAKE_DOCKER'
#!/usr/bin/env bash
set -uo pipefail
STATE="${FAKE_DOCKER_STATE:?}"
LOG="${FAKE_DOCKER_LOG:?}"
mkdir -p "$STATE"
printf '%s\n' "$*" >>"$LOG"

verb="${1:-}"; shift || true
case "$verb" in
  info) exit "${FAKE_DOCKER_INFO_RC:-0}" ;;
  run)
    name=""
    while [[ $# -gt 0 ]]; do
      case "$1" in
        --name) name="$2"; shift 2 ;;
        *) shift ;;
      esac
    done
    [[ -n "$name" ]] || { echo "fake docker: no --name" >&2; exit 125; }
    if [[ "${FAKE_DOCKER_RUN_RC:-0}" != "0" ]]; then
      echo "fake docker: run refused" >&2
      exit "${FAKE_DOCKER_RUN_RC}"
    fi
    printf 'running\n' >"$STATE/$name"
    echo "deadbeefcafe"
    ;;
  logs)
    name="${!#}"
    [[ -f "$STATE/$name" ]] || { echo "No such container: $name" >&2; exit 1; }
    [[ -n "${FAKE_JOB_STDOUT:-}" ]] && printf '%s\n' "$FAKE_JOB_STDOUT"
    [[ -n "${FAKE_JOB_STDERR:-}" ]] && printf '%s\n' "$FAKE_JOB_STDERR" >&2
    true
    ;;
  wait)
    name="${1:-}"
    [[ -f "$STATE/$name" ]] || { echo "No such container: $name" >&2; exit 1; }
    sleep "${FAKE_JOB_WAIT_SLEEP:-0}"
    printf 'exited %s\n' "${FAKE_JOB_EXIT:-0}" >"$STATE/$name"
    echo "${FAKE_JOB_EXIT:-0}"
    ;;
  inspect)
    fmt=""
    if [[ "${1:-}" == "--format" ]]; then fmt="$2"; shift 2; fi
    name="${1:-}"
    [[ -f "$STATE/$name" ]] || exit 1
    read -r status code <"$STATE/$name"
    case "$fmt" in
      *State.Status*) echo "$status" ;;
      *State.ExitCode*) echo "${code:-0}" ;;
      *) echo "{}" ;;
    esac
    ;;
  rm)
    name="${!#}"; rm -f "$STATE/$name" ;;
  stop)
    name="${!#}"; printf 'exited 143\n' >"$STATE/$name" ;;
  kill)
    name="${!#}"; printf 'exited 137\n' >"$STATE/$name" ;;
  *) echo "fake docker: unhandled verb '$verb'" >&2; exit 125 ;;
esac
FAKE_DOCKER
chmod +x "$FAKE_BIN/docker"

# --- Fake ssh ----------------------------------------------------------------
# Really executes the piped program locally with `bash -s`, so the ssh
# transport is exercised end to end (program on stdin, verb+args in argv,
# stdout/stderr/exit code relayed) without an sshd.
cat >"$FAKE_BIN/ssh" <<'FAKE_SSH'
#!/usr/bin/env bash
set -uo pipefail
printf '%s\n' "$*" >>"${FAKE_SSH_LOG:?}"
if [[ "${FAKE_SSH_FAIL:-0}" == "1" ]]; then
  echo "ssh: connect to host ${FAKE_SSH_HOST:-somewhere} port 22: Connection refused" >&2
  exit 255
fi
# Drop ssh options and the target, keep the remote command + args.
args=("$@")
i=0
while [[ $i -lt ${#args[@]} ]]; do
  case "${args[$i]}" in
    -o | -p | -i | -l | -F | -J) i=$((i + 2)) ;; # options that take a value
    -*) i=$((i + 1)) ;;
    *) break ;;
  esac
done
i=$((i + 1))   # skip the [user@]host target
remote=("${args[@]:$i}")
tee "${FAKE_SSH_STDIN_CAPTURE:-/dev/null}" | "${remote[@]}"
FAKE_SSH
chmod +x "$FAKE_BIN/ssh"

export FAKE_DOCKER_STATE="$TMP_ROOT/state"
export FAKE_DOCKER_LOG="$TMP_ROOT/docker.log"
export FAKE_SSH_LOG="$TMP_ROOT/ssh.log"
export PATH="$FAKE_BIN:$PATH"
# Pin the seam away from any host config/daemon: no config tiers, no real
# docker, no real ssh.
export LOOM_WORKSPACE="$TMP_ROOT/workspace"
mkdir -p "$LOOM_WORKSPACE/.loom"
echo '{}' >"$LOOM_WORKSPACE/.loom/config.json"
export LOOM_CONFIG_DEFAULTS_FILE=""

reset_state() {
    rm -rf "$FAKE_DOCKER_STATE"
    mkdir -p "$FAKE_DOCKER_STATE"
    : >"$FAKE_DOCKER_LOG"
    : >"$FAKE_SSH_LOG"
    unset FAKE_JOB_EXIT FAKE_JOB_STDOUT FAKE_JOB_STDERR FAKE_JOB_WAIT_SLEEP
    unset FAKE_DOCKER_RUN_RC FAKE_DOCKER_INFO_RC FAKE_SSH_FAIL
}

echo ""
echo "=== 1. Job spec normalization (--print-spec) ==="
reset_state
spec="$("$RUN_JOB" --print-spec --id job-alpha --image alpine:3 \
    --mount /srv/work --mount /srv/ro:ro --workdir /srv/work \
    --env FOO=bar --cpus 2 --memory 4g --timeout 90 -- echo hello world 2>/dev/null)"
assert_eq "loom.run-job/v1" "$(jq -r .schema <<<"$spec")" "spec carries the versioned schema id"
assert_eq "job-alpha" "$(jq -r .id <<<"$spec")" "spec id honours --id"
assert_eq "alpine:3" "$(jq -r .image <<<"$spec")" "spec image honours --image"
assert_eq '["echo","hello","world"]' "$(jq -c .command <<<"$spec")" "command captured as an argv array"
assert_eq "rw" "$(jq -r '.mounts[0].mode' <<<"$spec")" "mount defaults to rw"
assert_eq "ro" "$(jq -r '.mounts[1].mode' <<<"$spec")" "':ro' suffix parsed as a read-only mount"
assert_eq "bar" "$(jq -r '.env.FOO' <<<"$spec")" "--env captured"
assert_eq "2" "$(jq -r '.limits.cpus' <<<"$spec")" "--cpus captured under limits"
assert_eq "4g" "$(jq -r '.limits.memory' <<<"$spec")" "--memory captured under limits"
assert_eq "90" "$(jq -r '.timeoutSeconds' <<<"$spec")" "--timeout captured"
assert_eq "none" "$(jq -r '.network' <<<"$spec")" "network defaults to none"

echo ""
echo "=== 2. Spec validation rejects unsafe / malformed specs (exit 78) ==="
reset_state
out="$("$RUN_JOB" --dry-run --image alpine -- true 2>&1)"
rc=$?
assert_eq "0" "$rc" "a minimal valid spec passes validation"

out="$("$RUN_JOB" --dry-run -- true 2>&1)"
assert_eq "78" "$?" "missing image is EX_CONFIG"
assert_contains "$out" "image is required" "missing image names the field"

out="$("$RUN_JOB" --dry-run --image alpine 2>&1)"
assert_eq "78" "$?" "empty command is EX_CONFIG"
assert_contains "$out" "command must not be empty" "empty command names the field"

out="$("$RUN_JOB" --dry-run --image alpine --mount relative/path -- true 2>&1)"
assert_eq "78" "$?" "relative mount path is EX_CONFIG"
assert_contains "$out" "mount path must be absolute" "relative mount rejected by parity rule"

out="$("$RUN_JOB" --dry-run --image alpine --mount /srv/../etc -- true 2>&1)"
assert_eq "78" "$?" "mount path containing '..' is EX_CONFIG"

out="$("$RUN_JOB" --dry-run --image alpine --workdir relative -- true 2>&1)"
assert_eq "78" "$?" "relative workdir is EX_CONFIG"

out="$("$RUN_JOB" --dry-run --image alpine --network host -- true 2>&1)"
assert_eq "78" "$?" "host networking is refused"
assert_contains "$out" "host networking is not offered" "host network refusal is explicit"

out="$("$RUN_JOB" --dry-run --image alpine --memory lots -- true 2>&1)"
assert_eq "78" "$?" "malformed memory limit is EX_CONFIG"

out="$("$RUN_JOB" --dry-run --image alpine --cpus many -- true 2>&1)"
assert_eq "78" "$?" "malformed cpu limit is EX_CONFIG"

echo ""
echo "=== 3. THE security criterion: no docker socket can traverse the seam ==="
reset_state
for sock in /var/run/docker.sock /run/docker.sock /home/loom/docker.sock; do
    out="$("$RUN_JOB" --dry-run --image alpine --mount "$sock" -- true 2>&1)"
    assert_eq "78" "$?" "refuses to mount $sock"
    assert_contains "$out" "host-root-equivalent" "refusal for $sock cites the security rationale"
done
out="$("$RUN_JOB" --dry-run --image alpine --mount /run/podman/podman.sock -- true 2>&1)"
assert_eq "78" "$?" "refuses a podman socket mount too"
out="$("$RUN_JOB" --dry-run --image alpine --mount /proc -- true 2>&1)"
assert_eq "78" "$?" "refuses a /proc mount"

# The executor re-validates independently: a client that skips its own
# pre-flight (or lies) still cannot talk the executor into a socket mount.
evil="$(jq -nc '{schema:"loom.run-job/v1", id:"job-evil", image:"alpine",
                 command:["true"], workdir:"", network:"none",
                 mounts:[{path:"/var/run/docker.sock", mode:"rw"}],
                 env:{}, limits:{cpus:"",memory:""}, timeoutSeconds:0}')"
out="$(bash "$EXEC_LIB" run "$(printf '%s' "$evil" | base64 | tr -d '\n')" 2>&1)"
assert_eq "78" "$?" "executor independently rejects a socket-mount spec"
assert_contains "$out" "refusing docker/container-runtime socket mount" "executor-side refusal is explicit"
assert_not_contains "$(cat "$FAKE_DOCKER_LOG")" "docker.sock" "no docker run was issued for the rejected spec"

# And the generated argv never contains a socket mount or a privilege escalation.
argv="$("$RUN_JOB" --dry-run --image alpine --mount /srv/work -- true 2>/dev/null)"
assert_not_contains "$argv" "docker.sock" "generated argv contains no docker socket"
assert_not_contains "$argv" "--privileged" "generated argv is never privileged"
assert_not_contains "$argv" "--cap-add" "generated argv adds no capabilities"

echo ""
echo "=== 4. --dry-run prints the real docker argv (parity mounts, limits) ==="
reset_state
argv="$("$RUN_JOB" --dry-run --id job-argv --image alpine:3 \
    --mount /srv/work --mount /srv/ro:ro --workdir /srv/work \
    --env FOO=bar --cpus 2 --memory 4g -- bash -lc 'echo hi' 2>/dev/null)"
assert_contains "$argv" "--detach" "job container is started detached (restart safety)"
assert_not_contains "$argv" "--rm" "job container is NOT --rm (exit code must survive a restart)"
assert_contains "$argv" "loom-job-job-argv" "container name is derived from the job id"
assert_contains "$argv" "loom.job.id=job-argv" "container carries a loom.job.id label"
assert_contains "$argv" "/srv/work:/srv/work" "rw mount uses an identical-path (parity) bind"
assert_contains "$argv" "/srv/ro:/srv/ro:ro" "ro mount uses an identical-path (parity) bind, read-only"
assert_contains "$argv" "FOO=bar" "env var forwarded"
assert_contains "$(tr '\n' ' ' <<<"$argv")" "--cpus 2" "--cpus forwarded to docker"
assert_contains "$(tr '\n' ' ' <<<"$argv")" "--memory 4g" "--memory forwarded to docker"
assert_contains "$(tr '\n' ' ' <<<"$argv")" "--network none" "--network forwarded to docker"
assert_contains "$(tr '\n' ' ' <<<"$argv")" "-w /srv/work" "workdir forwarded to docker"
assert_contains "$argv" "alpine:3" "image appears in the argv"

echo ""
echo "=== 5. Local executor: exit-code + log passthrough (success) ==="
reset_state
export FAKE_JOB_EXIT=0
export FAKE_JOB_STDOUT="job stdout line"
export FAKE_JOB_STDERR="job stderr line"
outfile="$TMP_ROOT/out.5"
errfile="$TMP_ROOT/err.5"
"$RUN_JOB" --executor local --id job-ok --image alpine -- true >"$outfile" 2>"$errfile"
rc=$?
assert_eq "0" "$rc" "successful job exits 0"
assert_contains "$(cat "$outfile")" "job stdout line" "job stdout passed through to the caller's stdout"
assert_contains "$(cat "$errfile")" "job stderr line" "job stderr passed through to the caller's stderr"
assert_contains "$(cat "$errfile")" "# LOOM_RUN_JOB_EXIT id=job-ok code=0" "exit sentinel emitted"
assert_contains "$(cat "$FAKE_DOCKER_LOG")" "rm --force loom-job-job-ok" "container removed once its exit code was read"

echo ""
echo "=== 6. Local executor: exit-code passthrough (failure) ==="
reset_state
export FAKE_JOB_EXIT=42
export FAKE_JOB_STDERR="boom"
errfile="$TMP_ROOT/err.6"
"$RUN_JOB" --executor local --id job-fail --image alpine -- false 2>"$errfile"
assert_eq "42" "$?" "failing job's exit code is passed through verbatim"
assert_contains "$(cat "$errfile")" "boom" "failing job's stderr is passed through"

reset_state
export FAKE_JOB_EXIT=255
"$RUN_JOB" --executor local --id job-255 --image alpine -- false >/dev/null 2>&1
assert_eq "255" "$?" "a job exiting 255 is reported as 255 (not confused with an ssh failure)"

echo ""
echo "=== 7. SSH executor: transport shape + end-to-end passthrough ==="
reset_state
export FAKE_JOB_EXIT=7
export FAKE_JOB_STDOUT="remote stdout"
export FAKE_SSH_STDIN_CAPTURE="$TMP_ROOT/ssh-stdin"
outfile="$TMP_ROOT/out.7"
errfile="$TMP_ROOT/err.7"
LOOM_JOB_EXECUTOR_HOST=executor.example LOOM_JOB_EXECUTOR_USER=loom \
    "$RUN_JOB" --executor ssh --id job-ssh --image alpine -- true >"$outfile" 2>"$errfile"
assert_eq "7" "$?" "ssh executor passes the remote job's exit code through"
assert_contains "$(cat "$outfile")" "remote stdout" "ssh executor passes remote stdout through"
assert_contains "$(cat "$FAKE_SSH_LOG")" "loom@executor.example" "ssh target honours host/user resolution"
assert_contains "$(cat "$FAKE_SSH_LOG")" "BatchMode=yes" "default ssh options are applied"
assert_contains "$(cat "$FAKE_SSH_LOG")" "bash -s --" "executor program is fed to a remote 'bash -s'"
assert_contains "$(cat "$FAKE_SSH_LOG")" "run " "verb + base64 spec travel in argv"
assert_contains "$(cat "$FAKE_SSH_STDIN_CAPTURE")" "LOOM_RUN_JOB_EXIT" "the executor program itself is piped on stdin (no remote Loom install needed)"
unset FAKE_SSH_STDIN_CAPTURE

echo ""
echo "=== 8. Transport failure is EX_UNAVAILABLE, never a fake job exit code ==="
reset_state
export FAKE_SSH_FAIL=1
errfile="$TMP_ROOT/err.8"
LOOM_JOB_EXECUTOR_HOST=down.example "$RUN_JOB" --executor ssh --id job-down --image alpine -- true 2>"$errfile"
assert_eq "69" "$?" "unreachable executor exits 69 (EX_UNAVAILABLE), not 255"
assert_contains "$(cat "$errfile")" "is unreachable" "transport failure is named as such"
assert_contains "$(cat "$errfile")" "did NOT run" "transport failure makes clear the job never ran"
unset FAKE_SSH_FAIL

echo ""
echo "=== 9. Restart safety: a drain signal detaches, it never kills the job ==="
reset_state
export FAKE_JOB_EXIT=5
export FAKE_JOB_WAIT_SLEEP=30
errfile="$TMP_ROOT/err.9"
spec="$(jq -nc '{schema:"loom.run-job/v1", id:"job-drain", image:"alpine",
                 command:["sleep","300"], workdir:"", network:"none", mounts:[],
                 env:{}, limits:{cpus:"",memory:""}, timeoutSeconds:0}')"
bash "$EXEC_LIB" run "$(printf '%s' "$spec" | base64 | tr -d '\n')" >/dev/null 2>"$errfile" &
exec_pid=$!
# Wait for the container to exist, then send the drain signal.
for _ in $(seq 1 50); do
    [[ -f "$FAKE_DOCKER_STATE/loom-job-job-drain" ]] && break
    sleep 0.1
done
kill -TERM "$exec_pid" 2>/dev/null
wait "$exec_pid"
rc=$?
assert_eq "75" "$rc" "a drain SIGTERM detaches (EX_TEMPFAIL), it does not fail the job"
assert_contains "$(cat "$errfile")" "# LOOM_RUN_JOB_DETACHED id=job-drain" "detach is announced machine-readably"
docker_log="$(cat "$FAKE_DOCKER_LOG")"
assert_not_contains "$docker_log" "kill loom-job-job-drain" "teardown never SIGKILLs the in-flight job"
assert_not_contains "$docker_log" "stop " "teardown never even stops the in-flight job"
assert_not_contains "$docker_log" "rm --force loom-job-job-drain" "teardown never removes the in-flight container"
if [[ -f "$FAKE_DOCKER_STATE/loom-job-job-drain" ]]; then
    pass "the job container is still in flight after the drain"
else
    fail "the job container is still in flight after the drain" "container state file is gone"
fi

echo ""
echo "=== 10. Reattach after a restart recovers logs AND the exit code ==="
export FAKE_JOB_WAIT_SLEEP=0
export FAKE_JOB_STDOUT="output survived the restart"
outfile="$TMP_ROOT/out.10"
errfile="$TMP_ROOT/err.10"
"$RUN_JOB" --executor local attach job-drain >"$outfile" 2>"$errfile"
assert_eq "5" "$?" "reattach returns the in-flight job's real exit code"
assert_contains "$(cat "$outfile")" "output survived the restart" "reattach replays the job's output from its first line"
assert_contains "$(cat "$errfile")" "# LOOM_RUN_JOB_EXIT id=job-drain code=5" "reattach emits the exit sentinel"

echo ""
echo "=== 11. status / cancel verbs ==="
reset_state
printf 'running\n' >"$FAKE_DOCKER_STATE/loom-job-job-st"
out="$("$RUN_JOB" --executor local status job-st 2>/dev/null)"
assert_contains "$out" "state=running" "status reports a running job"
out="$("$RUN_JOB" --executor local status job-absent 2>/dev/null)"
assert_contains "$out" "state=absent" "status reports an unknown job as absent"

errfile="$TMP_ROOT/err.11"
"$RUN_JOB" --executor local cancel job-st 2>"$errfile"
assert_eq "0" "$?" "cancel of a live job succeeds"
assert_contains "$(cat "$errfile")" "# LOOM_RUN_JOB_CANCELLED id=job-st" "cancel is announced"
docker_log="$(cat "$FAKE_DOCKER_LOG")"
assert_contains "$docker_log" "stop --time" "cancel stops the job GRACEFULLY (SIGTERM + grace)"
assert_not_contains "$docker_log" "kill loom-job-job-st" "cancel never uses docker kill"

echo ""
echo "=== 12. Executor resolution: auto picks local when docker is reachable ==="
reset_state
export FAKE_JOB_EXIT=0
errfile="$TMP_ROOT/err.12"
"$RUN_JOB" --id job-auto --image alpine -- true >/dev/null 2>"$errfile"
assert_eq "0" "$?" "auto-resolved executor runs the job"
assert_contains "$(cat "$errfile")" "mode=local" "auto resolves to the local executor when docker is reachable"
assert_contains "$(cat "$errfile")" "auto:local" "the resolution path is logged"

reset_state
export FAKE_DOCKER_INFO_RC=1
export FAKE_SSH_FAIL=1
errfile="$TMP_ROOT/err.12b"
LOOM_JOB_EXECUTOR_HOST=host.example "$RUN_JOB" --id job-auto2 --image alpine -- true >/dev/null 2>"$errfile"
assert_eq "69" "$?" "auto falls back to ssh when no docker daemon is reachable here"
assert_contains "$(cat "$errfile")" "auto:ssh" "auto resolves to ssh (the loopback/remote executor) with no local docker"
unset FAKE_DOCKER_INFO_RC FAKE_SSH_FAIL

echo ""
echo "=== 13. Config-driven executor resolution ==="
reset_state
cat >"$LOOM_WORKSPACE/.loom/config.json" <<'CFG'
{
  "jobs": {
    "executor": {
      "mode": "ssh",
      "host": "cfg.example",
      "user": "cfguser",
      "sshOptions": ["-o", "BatchMode=yes", "-p", "2222"]
    }
  }
}
CFG
export FAKE_JOB_EXIT=0
"$RUN_JOB" --id job-cfg --image alpine -- true >/dev/null 2>"$TMP_ROOT/err.13"
assert_eq "0" "$?" "config-selected ssh executor runs the job"
assert_contains "$(cat "$FAKE_SSH_LOG")" "cfguser@cfg.example" "host/user come from jobs.executor config"
assert_contains "$(cat "$FAKE_SSH_LOG")" "-p 2222" "jobs.executor.sshOptions are applied"
assert_contains "$(cat "$TMP_ROOT/err.13")" "config (jobs.executor.mode)" "the config source is logged"
echo '{}' >"$LOOM_WORKSPACE/.loom/config.json"

echo ""
echo "=== 14. A spec file is accepted verbatim (--spec) ==="
reset_state
export FAKE_JOB_EXIT=0
cat >"$TMP_ROOT/job.json" <<'SPECFILE'
{
  "schema": "loom.run-job/v1",
  "id": "job-fromfile",
  "image": "ghcr.io/rjwalters/loom-worker:latest",
  "command": ["bash", "-lc", "cargo build --release"],
  "workdir": "/home/loom/workspaces/loom",
  "mounts": [{"path": "/home/loom/workspaces/loom", "mode": "rw"}],
  "env": {"CARGO_TARGET_DIR": "/home/loom/workspaces/loom/target"},
  "limits": {"cpus": "4", "memory": "8g"},
  "timeoutSeconds": 1800
}
SPECFILE
argv="$("$RUN_JOB" --spec "$TMP_ROOT/job.json" --dry-run 2>/dev/null)"
assert_contains "$argv" "loom-job-job-fromfile" "spec file id is used"
assert_contains "$argv" "/home/loom/workspaces/loom:/home/loom/workspaces/loom" "spec file mount is parity-bound"
assert_contains "$(tr '\n' ' ' <<<"$argv")" "--memory 8g" "spec file limits are applied"
"$RUN_JOB" --spec - --executor local </dev/null >/dev/null 2>&1
assert_eq "78" "$?" "an empty spec on stdin is EX_CONFIG"

echo ""
echo "=== 15. No shipped Loom script mounts a container-runtime socket ==="
# AC: "No docker socket is mounted into any worker container as part of this
# seam." Structural check, not a promise: grep every shipped script for a
# `-v …docker.sock` bind.
offenders=""
while IFS= read -r f; do
    if grep -nE '^[^#]*-v[[:space:]]*[^[:space:]]*(docker|containerd|podman|crio)\.sock' "$f" >/dev/null 2>&1; then
        offenders+="$f "
    fi
done < <(find "$SCRIPTS_DIR" -name '*.sh' -type f)
assert_eq "" "$offenders" "no shipped script binds a container-runtime socket into a container"

echo ""
echo "======================================"
echo "Tests run:    $TESTS_RUN"
echo -e "Tests passed: ${GREEN}${TESTS_PASSED}${NC}"
if ((TESTS_FAILED > 0)); then
    echo -e "Tests failed: ${RED}${TESTS_FAILED}${NC}"
    exit 1
fi
echo "All tests passed."
exit 0
