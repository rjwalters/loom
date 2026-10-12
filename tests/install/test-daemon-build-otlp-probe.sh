#!/usr/bin/env bash
# Test suite for the post-build OTLP probe in scripts/daemon-build.sh and its
# exit-code mapping in scripts/install-loom.sh (issue #10700).
#
# Usage: ./tests/install/test-daemon-build-otlp-probe.sh
#
# daemon-build.sh runs `<built binary> telemetry-capabilities --require-otlp`
# after the build and exits 4 — distinct from 1 (compile failure) and 3
# (binary missing) — when the probe fails. install-loom.sh maps 4 to its own
# error message. A stub `cargo` on PATH writes a FAKE loom-daemon whose probe
# answer is settable, so no real compile is needed.
#
# install-loom.sh runs its whole installation flow when sourced, so the
# status-mapping block (from the daemon-build.sh call to its `esac`) is
# extracted with awk and evaluated against a stub daemon-build.sh, the same
# extraction approach as test-daemon-build-shortcut.sh.
#
# Exit code 0 = all tests pass, 1 = failures detected.

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
DAEMON_BUILD="$REPO_ROOT/scripts/daemon-build.sh"
INSTALL_LOOM="$REPO_ROOT/scripts/install-loom.sh"

PASS=0
FAIL=0
TOTAL=0

assert_eq() {
  local desc="$1" expected="$2" actual="$3"
  TOTAL=$((TOTAL + 1))
  if [[ "$expected" == "$actual" ]]; then
    echo "PASS: $desc"
    PASS=$((PASS + 1))
  else
    echo "FAIL: $desc"
    echo "  expected: '$expected'"
    echo "  actual:   '$actual'"
    FAIL=$((FAIL + 1))
  fi
}

WORKDIR="$(mktemp -d)"
trap 'rm -rf "$WORKDIR"' EXIT

# ---------- stub cargo: builds a fake loom-daemon with a settable probe ----------
STUB_BIN="$WORKDIR/bin"
mkdir -p "$STUB_BIN"
cat > "$STUB_BIN/cargo" <<'STUB_CARGO_EOF'
#!/usr/bin/env bash
set -euo pipefail
if [[ "${1:-}" == "metadata" ]]; then
  printf '{"target_directory":"%s"}\n' "$STUB_TARGET_DIR"
  exit 0
fi
if [[ "${1:-}" == "build" ]]; then
  mkdir -p "$STUB_TARGET_DIR/release"
  cat > "$STUB_TARGET_DIR/release/loom-daemon" <<EOF
#!/usr/bin/env bash
if [[ "\$1" == "telemetry-capabilities" && "\$2" == "--require-otlp" ]]; then
  echo '{"otlp":$STUB_OTLP,"otlp_protocol":"http/json"}'
  exit $STUB_PROBE_RC
fi
exit 64
EOF
  chmod +x "$STUB_TARGET_DIR/release/loom-daemon"
  exit 0
fi
echo "stub cargo: unrecognized invocation: $*" >&2
exit 64
STUB_CARGO_EOF
chmod +x "$STUB_BIN/cargo"

run_build() {
  local target="$1" otlp="$2" probe_rc="$3"
  STUB_TARGET_DIR="$target" STUB_OTLP="$otlp" STUB_PROBE_RC="$probe_rc" \
    env -u CARGO_TARGET_DIR PATH="$STUB_BIN:$PATH" "$DAEMON_BUILD" \
    >"$WORKDIR/out" 2>"$WORKDIR/err"
}

# ---------- test 1: probe passes -> exit 0, stdout is ONLY the target dir ----------
T1="$WORKDIR/target-ok"
rc=0
run_build "$T1" true 0 || rc=$?
assert_eq "probe passes: daemon-build.sh exits 0" "0" "$rc"
assert_eq "probe passes: stdout is only the target dir (probe JSON went to stderr)" \
  "$T1" "$(cat "$WORKDIR/out")"
assert_eq "probe passes: probe JSON is on stderr" "1" \
  "$(grep -c '"otlp":true' "$WORKDIR/err")"
assert_eq "probe passes: the arch-specific copy is produced" "1" \
  "$( [[ -x "$T1/release/loom-daemon-aarch64-apple-darwin" ]] && echo 1 || echo 0 )"

# ---------- test 2: probe fails -> distinct exit 4, nothing handed on ----------
T2="$WORKDIR/target-no-otlp"
rc=0
run_build "$T2" false 1 || rc=$?
assert_eq "probe fails: daemon-build.sh exits 4 (distinct from 1 and 3)" "4" "$rc"
assert_eq "probe fails: stdout is empty (no target dir for the caller to use)" "" \
  "$(cat "$WORKDIR/out")"
assert_eq "probe fails: the message names the OTLP probe" "1" \
  "$(grep -c 'cannot export OTLP' "$WORKDIR/err")"
assert_eq "probe fails: not reported as a compile failure" "0" \
  "$(grep -c 'Failed to build loom-daemon' "$WORKDIR/err")"
assert_eq "probe fails: no arch-specific copy is produced" "0" \
  "$( [[ -e "$T2/release/loom-daemon-aarch64-apple-darwin" ]] && echo 1 || echo 0 )"

# ---------- install-loom.sh maps each daemon-build.sh status ----------
MAPPING="$(awk '/^TARGET_DIR=\$\("\$LOOM_ROOT\/scripts\/daemon-build.sh"\)/{f=1} f{print} f&&/^esac$/{exit}' "$INSTALL_LOOM")"
assert_eq "install-loom.sh status-mapping block was extracted" "1" \
  "$( [[ "$MAPPING" == *esac* ]] && echo 1 || echo 0 )"

# Run the extracted block against a fake LOOM_ROOT whose daemon-build.sh exits
# with $1; print what `error` was called with (or OK).
map_status() {
  local root="$WORKDIR/fake-root-$1"
  mkdir -p "$root/scripts"
  printf '#!/usr/bin/env bash\necho /fake/target\nexit %s\n' "$1" > "$root/scripts/daemon-build.sh"
  chmod +x "$root/scripts/daemon-build.sh"
  (
    set -e
    # Both are consumed by the eval'd block, which shellcheck cannot see.
    # shellcheck disable=SC2317
    error() { echo "ERROR: $*"; exit 1; }
    # shellcheck disable=SC2034
    LOOM_ROOT="$root"
    eval "$MAPPING"
    echo "OK $TARGET_DIR"
  )
}

assert_eq "install-loom.sh: status 0 proceeds with the target dir" "OK /fake/target" \
  "$(map_status 0)"
OUT4="$(map_status 4)"
assert_eq "install-loom.sh: status 4 gets its own OTLP message" "1" \
  "$( [[ "$OUT4" == ERROR:*"cannot export OTLP"* ]] && echo 1 || echo 0 )"
OUT3="$(map_status 3)"
assert_eq "install-loom.sh: status 3 keeps the binary-not-found message" "1" \
  "$( [[ "$OUT3" == ERROR:*"binary was not found"* ]] && echo 1 || echo 0 )"
assert_eq "install-loom.sh: status 1 is a build failure" "ERROR: Failed to build loom-daemon" \
  "$(map_status 1)"

echo ""
echo "-----------------------------------------"
echo "Total: $TOTAL  Passed: $PASS  Failed: $FAIL"
if [[ "$FAIL" -gt 0 ]]; then
  exit 1
fi
exit 0
