#!/usr/bin/env bash
# Exercise the pool selector and supervised session-exec transport using synthetic
# accounts. Only Docker and the provider CLI are stand-ins.
set -euo pipefail
scripts="$(cd "$(dirname "$0")/.." && pwd)"
source "$scripts/tests/lib/require-daemon-bin.sh"
loom_test_require_daemon_bin --self-only --path "$scripts" "session-exec protocol"
daemon="$LOOM_DAEMON_SELF_BIN"
scratch="$(mktemp -d)"
trap 'rm -rf "$scratch"' EXIT
cd "$scratch"
export LOOM_WORKSPACE="$PWD/repo" LOOM_SHARED_TOKENS_DIR="$PWD/tokens" LOOM_CLAUDE_SEAT_ROOT="$PWD/seats"
export HOME="$PWD/operator" LOOM_SWEEP_NICE=0 LOOM_SWEEP_CPU_QUOTA=0 LOOM_SWEEP_CONTAINERIZED=0
export LOOM_DAEMON_BIN="$daemon" LOOM_TOKEN_SPREAD_TOP_N=1
unset CLAUDE_CODE_OAUTH_TOKEN LOOM_TOKEN_NAME LOOM_SPAWN_NO_EXPORT LOOM_SPAWN_CONTAINERIZED LOOM_ROLE LOOM_SESSION_STDERR_FILE
mkdir -p "$LOOM_WORKSPACE/.loom" "$HOME" bin
mkdir -m 700 "$LOOM_SHARED_TOKENS_DIR"
ln -s "$scripts" "$LOOM_WORKSPACE/.loom/scripts"
for seat in alpha beta; do
    mkdir -p "seats/$seat/home"
    touch "seats/$seat/.session-managed.json"
    (umask 077; printf 'fixture-oauth-%s' "$seat" > "tokens/$seat.token")
done

cat > bin/docker <<'PY'
#!/usr/bin/env python3
import json, os, sys
from pathlib import Path

args = sys.argv[1:]
root = Path(os.environ['LOOM_CLAUDE_SEAT_ROOT'])
seats = {'alpha': 'a'*64, 'beta': 'b'*64}
if any('fixture-oauth-' in arg for arg in args):
    sys.exit('OAuth credential appeared in Docker argv')
if args[0] == 'inspect':
    name = args[-1].removeprefix('loom-claude-session-')
    seat = next(k for k, v in seats.items() if name in (k, v))
    home = str(root / ('alpha' if (root / seat / 'wrong-home').exists() else seat) / 'home')
    state = {'Id': seats[seat], 'Image': 'sha256:'+'c'*64,
             'State': {'Running': not (root / seat / 'stopped').exists()},
             'Config': {'Image': 'operator/session:test', 'Labels': {
                 'loom.session-runtime': 'claude', 'loom.containment': 'claude-session',
                 'loom.session-posture': 'container-boundary-v1'}},
             'HostConfig': {'Privileged': False, 'CapDrop': ['ALL'], 'SecurityOpt': ['no-new-privileges']},
             'Mounts': [{'Type':'bind','Source': home,'Destination':'/home/loom/.claude','RW':True},
                        {'Type':'bind','Source':os.getcwd(),'Destination':os.getcwd(),'RW':True}]}
    if (root / seat / 'pool-mounted').exists():
        state['Mounts'].append({'Type': 'bind', 'Source': os.environ['LOOM_SHARED_TOKENS_DIR'],
                                'Destination': '/extra', 'RW': False})
    print(json.dumps([state]))
    sys.exit(0)
assert args.pop(0) == 'exec'
env = {'PATH': os.environ['PATH']}
while args[0].startswith('-'):
    opt = args.pop(0)
    if opt == '--workdir':
        os.chdir(args.pop(0))
    elif opt in ('--env', '-e'):
        key, sep, val = args.pop(0).partition('=')
        env[key] = val if sep else os.environ[key]
    else:
        assert opt == '-i', opt
target = args.pop(0)
seat = next(k for k, v in seats.items() if target in (v, 'loom-claude-session-'+k))
home = str(root / seat / 'home')
# Like the real image: HOME is the user's home, the mounted seat home is only
# the Claude config dir (/home/loom/.claude), never HOME itself.
env.update(HOME=str(root.parent / 'image-home' / seat), CLAUDE_CONFIG_DIR=home)
os.makedirs(env['HOME'], exist_ok=True)
os.execve(os.environ['LOOM_DAEMON_SELF_BIN'], args, env)
PY
cat > bin/claude <<'CLI'
#!/usr/bin/env bash
set -eu
if [[ "$1" = auth && "$2" = status ]]; then echo '{"loggedIn":true}'; exit 0; fi
[[ "$1" = -p && "$2" = "/loom:builder 42" ]]
[[ "$CLAUDE_CODE_OAUTH_TOKEN" = "fixture-oauth-$LOOM_TOKEN_NAME" ]]
[[ -f source.txt && "$(cat source.txt)" = before ]]
printf 'after\n' > result.txt
if [[ -n "${CLAUDE_CONFIG_DIR:-}" ]]; then
    [[ -z "${LOOM_SHARED_TOKENS_DIR:-}" ]]
    printf '%s\n' "$2" >> "$CLAUDE_CONFIG_DIR/transcript"
fi
if [[ -f fail ]]; then
    echo '401 Invalid bearer token' >&2
    exit 42
fi
echo 'LOOM_STEP_OUTCOME role=builder target=42 class=verdict reason=pr_opened'
CLI
# The wrapper probes API reachability and caches auth status. Keep both local.
printf '#!/bin/sh\nexit 0\n' > bin/curl
chmod +x bin/*
export PATH="$scratch/bin:$PATH"
cd "$LOOM_WORKSPACE"
echo before > source.txt
run() {
    local status=0
    local expected=$1
    shift
    rm -f result.txt
    env -i PATH="$PATH" HOME="$HOME" TMPDIR="${TMPDIR:-/tmp}" LOOM_WORKSPACE="$LOOM_WORKSPACE" \
        LOOM_DAEMON_SELF_BIN="$daemon" LOOM_DAEMON_BIN="$daemon" LOOM_TOKEN_SPREAD_TOP_N=1 \
        LOOM_SWEEP_CPU_QUOTA=0 LOOM_SWEEP_NICE=0 LOOM_SWEEP_CONTAINERIZED="$LOOM_SWEEP_CONTAINERIZED" \
        LOOM_SWEEP_CREDENTIAL_PROXY="${LOOM_SWEEP_CREDENTIAL_PROXY:-0}" \
        LOOM_SHARED_TOKENS_DIR="$LOOM_SHARED_TOKENS_DIR" LOOM_CLAUDE_SEAT_ROOT="$LOOM_CLAUDE_SEAT_ROOT" \
        LOOM_TOKEN_NAME="${LOOM_TOKEN_NAME:-}" CLAUDE_CODE_OAUTH_TOKEN="${CLAUDE_CODE_OAUTH_TOKEN:-}" \
        "$scripts/spawn-claude.sh" "$@" -p "/loom:builder 42" >out 2>log || status=$?
    [[ "$status" = "$expected" ]] || { cat log >&2; echo "expected exit $expected, got $status" >&2; exit 1; }
}
# Capture the same representative command through the host adapter.
LOOM_CLAUDE_SEAT_ROOT='' run 0
cp out expected
check_seat() {
    run 0
    cmp out expected
    grep -qx after result.txt
    grep -q "containment=claude-session.*container_id=$2.*image_digest=sha256:" log
    # Explicit scheduling opt-outs hold inside the seat too.
    ! grep -q 're-exec at nice' log
    grep -qx '/loom:builder 42' "$LOOM_CLAUDE_SEAT_ROOT/$1/home/transcript"
}
printf 'beta|available\nalpha|available\n' > "$LOOM_SHARED_TOKENS_DIR/.ranking"
check_seat beta bbbbbbbb
rm "$LOOM_SHARED_TOKENS_DIR/.ranking"
echo alpha > "$LOOM_SHARED_TOKENS_DIR/.allowlist"
LOOM_SWEEP_CONTAINERIZED=1 check_seat alpha aaaaaaaa
rm "$LOOM_SHARED_TOKENS_DIR/.allowlist"
printf 'alpha|exhausted\nbeta|available\n' > "$LOOM_SHARED_TOKENS_DIR/.ranking"
check_seat beta bbbbbbbb
touch fail
run 42
source "$scripts/lib/classify-error.sh"
[[ "$(classify_error "$(cat log)" 42 claude)" = TOKEN_EXPIRED ]]
run 42 --use-wrapper
grep -q '401 Invalid bearer token' out
# The host, not the seat, records the dead login: the seat's pool is unreachable.
grep -q '^[^ ]* beta auth-dead' "$LOOM_SHARED_TOKENS_DIR/.bad_tokens"
[[ ! -e "$LOOM_CLAUDE_SEAT_ROOT/beta/home/.loom-account-failure" ]]
rm fail
# Both accounts rank available; the next dispatch skips the marked account.
printf 'beta|available\nalpha|available\n' > "$LOOM_SHARED_TOKENS_DIR/.ranking"
run 0
grep -q 'name=alpha' log
rm "$LOOM_SHARED_TOKENS_DIR/.bad_tokens" "$LOOM_SHARED_TOKENS_DIR/.ranking"
touch "$LOOM_CLAUDE_SEAT_ROOT/beta/wrong-home"
run 78
grep -q 'registered home' log
rm "$LOOM_CLAUDE_SEAT_ROOT/beta/wrong-home"
touch "$LOOM_CLAUDE_SEAT_ROOT/beta/pool-mounted"
run 78
grep -q 'overlapping seat state or the token pool' log
rm "$LOOM_CLAUDE_SEAT_ROOT/beta/pool-mounted"
CLAUDE_CODE_OAUTH_TOKEN=fixture-oauth-alpha LOOM_TOKEN_NAME=beta run 78
grep -q 'does not match' log
LOOM_SWEEP_CONTAINERIZED=1 LOOM_SWEEP_CREDENTIAL_PROXY=1 run 78
grep -q 'credential-proxied dispatch' log
touch "$LOOM_CLAUDE_SEAT_ROOT/beta/stopped"
run 78
grep -q 'category=SESSION_DOWN' log
# A mixed pool keeps an unadopted account on its configured transport.
rm "$LOOM_CLAUDE_SEAT_ROOT/beta/.session-managed.json"
run 0
grep -q 'mode=bare-metal' log
# Unadopted account on a seat-bearing host: the wrapper marks it bad directly
# (host branch of record_host_account_failure, no seat record involved).
rm -f "$LOOM_SHARED_TOKENS_DIR/.bad_tokens"
touch fail
run 42 --use-wrapper
grep -q '^[^ ]* [a-z]* auth-dead' "$LOOM_SHARED_TOKENS_DIR/.bad_tokens"
rm fail "$LOOM_SHARED_TOKENS_DIR/.bad_tokens"
rm "$LOOM_SHARED_TOKENS_DIR/"*.token
run 78
grep -q 'Token selection failed' log
if grep -r -F 'fixture-oauth-' "$LOOM_CLAUDE_SEAT_ROOT" out log; then exit 1; fi
echo 'PASS Claude seat dispatch, selection, transcript isolation, classifier and exit status'
