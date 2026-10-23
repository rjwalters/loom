#!/usr/bin/env bash
# Test suite for scripts/install/provision-tools.sh (issue #12940)
#
# Usage: LOOM_TOOLS_TEST_BIN=target/debug/loom-tools ./tests/install/test-provision-tools.sh
#
# The install half (stage -> check -> publish -> rollback) runs the REAL
# `loom-tools install-binary`, so it needs a built loom-tools
# (LOOM_TOOLS_TEST_BIN, else target/{debug,release}/loom-tools; the install
# tests are skipped, loudly, when none exists). The fetch half uses a fake `gh`
# and fake loom-tools scripts: no network.
#
# Exit code 0 = all tests pass, 1 = failures detected.

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"

# shellcheck source=scripts/install/provision-tools.sh
source "$REPO_ROOT/scripts/install/provision-tools.sh"

PASS=0; FAIL=0
ok()   { echo "PASS: $1"; PASS=$((PASS + 1)); }
bad()  { echo "FAIL: $1"; FAIL=$((FAIL + 1)); }
check() { # desc, then a command
  local desc="$1"; shift
  if "$@" >/dev/null 2>&1; then ok "$desc"; else bad "$desc"; fi
}

WORK="$(mktemp -d "${TMPDIR:-/tmp}/test-provision-tools.XXXXXX")"
trap 'rm -rf "$WORK"' EXIT

REAL="${LOOM_TOOLS_TEST_BIN:-}"
if [[ -z "$REAL" ]]; then
  for c in "$REPO_ROOT/target/debug/loom-tools" "$REPO_ROOT/target/release/loom-tools" \
           "$REPO_ROOT/loom-daemon/target/debug/loom-tools"; do
    [[ -x "$c" ]] && REAL="$c" && break
  done
fi

# A stand-in `loom-tools` that delegates install-binary to the real one and
# answers --version from $1 ("fail" = exit 1; "staged-only" = only while it
# is still named like a staging file).
make_fake() {
  local path="$1" mode="$2"
  cat >"$path" <<EOF
#!/usr/bin/env bash
case "\$1" in
  install-binary) exec "$REAL" "\$@" ;;
  --version)
    case "$mode" in
      fail) exit 1 ;;
      staged-only) case "\$(basename "\$0")" in .loom-tools.loom-install.*) echo "loom-tools 1.2.3" ;; *) exit 1 ;; esac ;;
      *) echo "loom-tools $mode" ;;
    esac ;;
esac
EOF
  chmod +x "$path"
}

echo "== install: stage / check / publish / rollback =="
if [[ -z "$REAL" ]]; then
  echo "SKIP: no built loom-tools (set LOOM_TOOLS_TEST_BIN); install tests not run"
else
  # Fresh machine: no previous loom-tools, no loom-daemon involved.
  D="$WORK/fresh/bin"; make_fake "$WORK/new-ok" "1.2.3"
  provision_machine_tools "$WORK/new-ok" "$D" "1.2.3" >/dev/null 2>&1
  check "fresh install publishes loom-tools" test -x "$D/loom-tools"
  check "fresh install answers --version" "$D/loom-tools" --version
  check "no loom-daemon was needed or created" test ! -e "$D/loom-daemon"

  # Upgrade keeps the previous binary.
  D="$WORK/upgrade/bin"; mkdir -p "$D"; make_fake "$D/loom-tools" "old-1"
  provision_machine_tools "$WORK/new-ok" "$D" "1.2.3" >/dev/null 2>&1
  [[ "$("$D/loom-tools" --version)" == "loom-tools 1.2.3" ]] && ok "upgrade installs the new binary" || bad "upgrade installs the new binary"
  [[ "$("$D/loom-tools.previous" --version 2>/dev/null)" == "loom-tools old-1" ]] && ok "upgrade retains the previous binary" || bad "upgrade retains the previous binary"

  # Rollback: failing post-stage check leaves the previous binary in place.
  D="$WORK/rollback/bin"; mkdir -p "$D"; make_fake "$D/loom-tools" "old-1"; make_fake "$WORK/new-bad" "fail"
  provision_machine_tools "$WORK/new-bad" "$D" "" >/dev/null 2>&1; rc=$?
  [[ $rc -ne 0 ]] && ok "failing staged check fails the install" || bad "failing staged check fails the install"
  [[ "$("$D/loom-tools" --version)" == "loom-tools old-1" ]] && ok "failing staged check leaves the previous loom-tools in place" || bad "failing staged check leaves the previous loom-tools in place"
  check "failing staged check leaves no staging file" test -z "$(find "$D" -name '.loom-tools.loom-install.*' 2>/dev/null)"

  # A version that is not the release asked for is refused the same way.
  D="$WORK/mismatch/bin"; mkdir -p "$D"; make_fake "$D/loom-tools" "old-1"
  provision_machine_tools "$WORK/new-ok" "$D" "9.9.9" >/dev/null 2>&1; rc=$?
  [[ $rc -ne 0 && "$("$D/loom-tools" --version)" == "loom-tools old-1" ]] && ok "version mismatch is refused, previous kept" || bad "version mismatch is refused, previous kept"

  # Post-publish failure restores <dest>.previous.
  D="$WORK/restore/bin"; mkdir -p "$D"; make_fake "$D/loom-tools" "old-1"; make_fake "$WORK/new-late" "staged-only"
  provision_machine_tools "$WORK/new-late" "$D" "" >/dev/null 2>&1; rc=$?
  [[ $rc -ne 0 ]] && ok "failing post-publish check fails the install" || bad "failing post-publish check fails the install"
  [[ "$("$D/loom-tools" --version)" == "loom-tools old-1" ]] && ok "failing post-publish check restores the previous binary" || bad "failing post-publish check restores the previous binary"
fi

echo "== fetch: fail closed =="
ASSETS="$WORK/assets"; BIN="$WORK/fakebin"; mkdir -p "$ASSETS" "$BIN"
cat >"$BIN/gh" <<'EOF'
#!/usr/bin/env bash
# Fake `gh release download TAG -R REPO -p A -p B -D DIR --clobber`.
[[ "$1 $2" == "release download" ]] || exit 1
pats=(); dir=""; shift 3
while [[ $# -gt 0 ]]; do
  case "$1" in -p) pats+=("$2"); shift 2 ;; -D) dir="$2"; shift 2 ;; *) shift ;; esac
done
for p in "${pats[@]}"; do
  [[ -f "$FAKE_ASSETS/$p" ]] || { echo "no asset matches $p" >&2; exit 1; }
  cp "$FAKE_ASSETS/$p" "$dir/$p"
done
EOF
chmod +x "$BIN/gh"
export FAKE_ASSETS="$ASSETS"
OLDPATH="$PATH"; PATH="$BIN:$PATH"
T="x86_64-unknown-linux-gnu"; A="loom-tools-$T"

publish_asset() { # body of the fake loom-tools
  printf '%s\n' "$1" >"$ASSETS/$A"
  ( cd "$ASSETS" && { sha256sum "$A" 2>/dev/null || shasum -a 256 "$A"; } >"$A.sha256" )
}
VERIFIED="$WORK/verified-bin"; make_fake "$VERIFIED" "1.2.3" 2>/dev/null || true

# release-fetch verifies, so it prints the KEY=value contract.
publish_asset "#!/usr/bin/env bash
[[ \"\$1\" == release-fetch ]] || exit 9
[[ \"\$*\" == *'--bin loom-tools'* ]] || exit 8
echo BIN_PATH=$VERIFIED"
out="$(_pmt_fetch o/r v1.2.3 "$T" 2>/dev/null)"; rc=$?
[[ $rc -eq 0 && "$out" == "$VERIFIED" ]] && ok "verified asset yields its BIN_PATH" || bad "verified asset yields its BIN_PATH (rc=$rc out=$out)"

echo "tampered" >>"$ASSETS/$A"
_pmt_fetch o/r v1.2.3 "$T" >/dev/null 2>&1; rc=$?
[[ $rc -ne 0 ]] && ok "checksum mismatch is refused" || bad "checksum mismatch is refused"

rm -f "$ASSETS/$A"
_pmt_fetch o/r v1.2.3 "$T" >/dev/null 2>&1; rc=$?
[[ $rc -ne 0 ]] && ok "missing loom-tools asset on a tag is refused" || bad "missing loom-tools asset on a tag is refused"

publish_asset "#!/usr/bin/env bash
echo 'signature invalid' >&2; exit 1"
_pmt_fetch o/r v1.2.3 "$T" >/dev/null 2>&1; rc=$?
[[ $rc -ne 0 ]] && ok "release-fetch refusal (e.g. bad signature) is refused" || bad "release-fetch refusal (e.g. bad signature) is refused"

publish_asset "#!/usr/bin/env bash
echo BIN_PATH=/nonexistent/loom-tools"
_pmt_fetch o/r v1.2.3 "$T" >/dev/null 2>&1; rc=$?
[[ $rc -ne 0 ]] && ok "success without a usable BIN_PATH is refused" || bad "success without a usable BIN_PATH is refused"
PATH="$OLDPATH"

echo ""
echo "passed: $PASS  failed: $FAIL"
[[ $FAIL -eq 0 ]]
