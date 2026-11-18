#!/usr/bin/env bash
# test-resolve-role-model.sh — the attended-subagent per-role model stub
# (issue #13956).
#
# Drives `resolve-role-model.sh` — the thin stub over the native
# `resolve-role-model` subcommand — through fake `loom-tools` / `loom-daemon`
# binaries in a scratch fixture repo with a scrubbed environment, mirroring
# test-tools-first-clean-install.sh's harness, so nothing on the host can
# answer for a binary the fixture does not provide.
#
# Pinned:
#   A. Wiring: the stub execs the `resolve-role-model` subcommand, loom-tools
#      serves it (the #13878 tools-first slice extended in #13956), and the
#      data exit code 3 ("nothing configured — omit the Task `model`
#      parameter") reaches the caller unmodified on both the tools and the
#      transitional-daemon path.
#   B. Version floor (#8484 convention): a daemon binary below the declared
#      0.19.1311 floor is REFUSED (never mistaken for an answer), and one at
#      the floor passes.
#   C. Resync keeps the setting (#13956 AC4): after a real resync-installed.sh
#      run over a synthetic repo, the synced agent copies still carry no
#      `model:` frontmatter (the #13956 ruling) and the operator's
#      `.loom/config.json` pin — which lives OUTSIDE every synced surface —
#      is byte-identical, so the config edit survives and governs.
#
# Usage: bash defaults/scripts/tests/test-resolve-role-model.sh

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SCRIPTS_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
RESYNC_SCRIPT="$SCRIPTS_DIR/resync-installed.sh"

RED='\033[0;31m'; GREEN='\033[0;32m'; NC='\033[0m'
TESTS_RUN=0; TESTS_PASSED=0; TESTS_FAILED=0
pass() { TESTS_RUN=$((TESTS_RUN + 1)); TESTS_PASSED=$((TESTS_PASSED + 1)); echo -e "${GREEN}✓${NC} $1"; }
fail() { TESTS_RUN=$((TESTS_RUN + 1)); TESTS_FAILED=$((TESTS_FAILED + 1)); echo -e "${RED}✗${NC} $1"; }
assert_eq() { if [[ "$1" == "$2" ]]; then pass "$3"; else fail "$3 (expected '$1', got '$2')"; fi; }
assert_contains() { if [[ "$2" == *"$1"* ]]; then pass "$3"; else fail "$3 (expected '$1' in [$2])"; fi; }
assert_not_contains() { if [[ "$2" != *"$1"* ]]; then pass "$3"; else fail "$3 (unexpected '$1' in [$2])"; fi; }

WORKDIR="$(mktemp -d)"
trap 'rm -rf "$WORKDIR"' EXIT

FIX="$WORKDIR/repo"
mkdir -p "$FIX/.git" "$FIX/.loom/scripts/lib" "$WORKDIR/home" "$WORKDIR/bin-tools" "$WORKDIR/bin-daemon"
cp "$SCRIPTS_DIR/resolve-role-model.sh" "$FIX/.loom/scripts/"
cp "$SCRIPTS_DIR"/lib/script-helper.sh "$SCRIPTS_DIR"/lib/locate-daemon-bin.sh "$FIX/.loom/scripts/lib/"
SYSTEM_PATH="/usr/bin:/bin"

# _fake_tools <path> — answers resolve-role-model with a canned payload.
# `argv` mode echoes its subcommand argv so the test can pin the wiring.
_fake_tools() {
    cat >"$1" <<FAKE
#!/usr/bin/env bash
case "\${1:-}" in
  --version) echo "loom-tools 0.19.1400"; exit 0 ;;
  capabilities) echo '{"payload_identity":"supports_installed=0.19.0;requires_daemon=0.19.772;invoked_files=x"}'; exit 0 ;;
  resolve-role-model) if [[ "\${2:-}" == "--argv-mode" ]]; then echo "argv: \$*"; else echo "TOOLS opus"; fi; exit 0 ;;
esac
exit 2
FAKE
    chmod +x "$1"
}
# _fake_daemon <path> <version> — answers resolve-role-model, optionally
# exiting 3 (the "nothing configured" DATA code) so the passthrough contract
# is checkable on the daemon path too.
_fake_daemon() {
    local bin="$1" ver="$2" rc3="${3:-0}"
    cat >"$bin" <<FAKE
#!/usr/bin/env bash
case "\${1:-}" in
  --version) echo "loom-daemon $ver (commit abc, built 2026-11-17T00:00:00Z)"; exit 0 ;;
  resolve-role-model) if [[ $rc3 -eq 3 ]]; then exit 3; fi; echo "DAEMON opus"; exit 0 ;;
esac
exit 2
FAKE
    chmod +x "$bin"
}

_fake_tools "$WORKDIR/bin-tools/loom-tools"

# run_stub <args...> with extra env in $STUB_ENV (array). Echoes "<rc>|<out>".
run_stub() {
    local out rc
    out="$(env -i PATH="$SYSTEM_PATH" HOME="$WORKDIR/home" "${STUB_ENV[@]}" \
        "$FIX/.loom/scripts/resolve-role-model.sh" "$@" 2>&1 </dev/null)"; rc=$?
    echo "$rc|$out"
}

echo "A. wiring: tools-first slice, subcommand name, data-code passthrough"
STUB_ENV=(LOOM_TOOLS_BIN="$WORKDIR/bin-tools/loom-tools")
res="$(run_stub judge)"
assert_eq "0" "${res%%|*}" "resolve-role-model.sh succeeds with only loom-tools"
assert_contains "TOOLS opus" "$res" "resolve-role-model.sh ran loom-tools"
res="$(run_stub --argv-mode judge --json)"
assert_contains "argv: resolve-role-model --argv-mode judge --json" "$res" \
    "the stub execs the resolve-role-model subcommand with the caller's argv"
STUB_ENV=(LOOM_TOOLS_BIN="$WORKDIR/bin-tools/loom-tools" LOOM_TOOLS_DAEMON_FALLBACK=0)
res="$(run_stub judge)"
assert_eq "0" "${res%%|*}" "session mode keeps the tools answer (no daemon needed)"
STUB_ENV=()

echo "B. the data exit code reaches the caller unmodified"
_fake_daemon "$WORKDIR/bin-daemon/loom-daemon" 0.19.1400 3
STUB_ENV=(LOOM_DAEMON_SELF_BIN="$WORKDIR/bin-daemon/loom-daemon")
res="$(run_stub auditor)"
assert_eq "3" "${res%%|*}" "exit 3 (nothing configured) is DATA, passed through unmodified"
assert_eq "3|" "${res%%|*}|" "exit 3 carries NO output"
_fake_daemon "$WORKDIR/bin-daemon/loom-daemon" 0.19.1400 0
res="$(run_stub judge)"
assert_contains "DAEMON opus" "$res" "LOOM_DAEMON_SELF_BIN (the implementation pin) serves the stub"
STUB_ENV=()

echo "B2. tools-first order for the new subcommand"
SYSTEM_PATH="$WORKDIR/bin-daemon:/usr/bin:/bin"
res="$(run_stub judge)"
assert_contains "DAEMON opus" "$res" "no loom-tools: the transitional daemon fallback serves the stub"
cp "$WORKDIR/bin-tools/loom-tools" "$WORKDIR/bin-daemon/loom-tools"
res="$(run_stub judge)"
assert_contains "TOOLS opus" "$res" "loom-tools beats loom-daemon when both resolve"
rm -f "$WORKDIR/bin-daemon/loom-tools"
SYSTEM_PATH="/usr/bin:/bin"

echo "C. version floor: a pre-#13956 binary is refused, never mistaken for an answer"
_fake_daemon "$WORKDIR/bin-daemon/old-daemon" 0.19.1310 0   # last version WITHOUT the subcommand
STUB_ENV=(LOOM_DAEMON_SELF_BIN="$WORKDIR/bin-daemon/old-daemon")
res="$(run_stub judge)"
assert_not_contains "DAEMON opus" "$res" "a binary below the floor never answers"
assert_contains "requires-daemon" "$res" "the refusal names the floor (the actionable #8385 message)"
STUB_ENV=()
_fake_daemon "$WORKDIR/bin-daemon/floor-daemon" 0.19.1311 0 # the floor itself passes
STUB_ENV=(LOOM_DAEMON_SELF_BIN="$WORKDIR/bin-daemon/floor-daemon")
res="$(run_stub judge)"
assert_contains "DAEMON opus" "$res" "a binary at the floor is served"
STUB_ENV=()

echo "D. resync keeps the setting (#13956 AC4: the setting lives outside defaults/)"
REPO="$WORKDIR/resync-repo"
mkdir -p "$REPO/defaults/.claude/agents" "$REPO/defaults/scripts/lib" \
         "$REPO/.claude/agents" "$REPO/.loom/scripts/lib" "$REPO/.loom"
# A shipped agent WITHOUT model: frontmatter (the #13956 contract)…
printf -- '---\nname: loom-judge\ndescription: judge agent\ntools: Read, Bash\n---\nbody\n' \
    > "$REPO/defaults/.claude/agents/loom-judge.md"
# …whose installed copy drifted (a model: pin that MUST be wiped by the sync).
printf -- '---\nname: loom-judge\ndescription: judge agent\ntools: Read, Bash\nmodel: opus\n---\nbody\n' \
    > "$REPO/.claude/agents/loom-judge.md"
cp "$SCRIPTS_DIR/resolve-role-model.sh" "$REPO/defaults/scripts/"
cp "$SCRIPTS_DIR"/lib/script-helper.sh "$SCRIPTS_DIR"/lib/locate-daemon-bin.sh "$REPO/defaults/scripts/lib/"
# The operator's setting, OUTSIDE every synced surface:
cat > "$REPO/.loom/config.json" <<'JSON'
{
  "terminals": [
    { "id": "judge", "roleConfig": { "roleFile": "judge.md", "model": "opus" } }
  ]
}
JSON
printf '{\n  "version": "9.9.9"\n}\n' > "$REPO/package.json"
printf '{\n  "loom_version": "0.0.0",\n  "loom_commit": "old",\n  "install_date": "2020-01-01",\n  "loom_source": "%s",\n  "installed_files": []\n}\n' \
    "$REPO" > "$REPO/.loom/install-metadata.json"
git -C "$REPO" init -q
git -C "$REPO" add -A >/dev/null 2>&1
git -C "$REPO" commit -qm "chore: install Loom v0.0.0" >/dev/null 2>&1

CONFIG_BEFORE="$(cat "$REPO/.loom/config.json")"
# shellcheck disable=SC2034  # OUT captured-and-dropped: failure surfaces via RC
OUT="$(cd "$REPO" && bash "$RESYNC_SCRIPT" 2>&1)"; RC=$?

assert_eq "0" "$RC" "resync-installed.sh applies cleanly over the fixture"

# The synced agent copy matches defaults — the drift pin is GONE after resync.
if grep -q '^model:' "$REPO/.claude/agents/loom-judge.md"; then
    fail "the synced agent copy carries no model: frontmatter after resync"
else
    pass "the synced agent copy carries no model: frontmatter after resync"
fi
assert_eq "$(cat "$REPO/defaults/.claude/agents/loom-judge.md")" \
    "$(cat "$REPO/.claude/agents/loom-judge.md")" "resync made the installed agent byte-identical to defaults"

# The setting itself is untouched: config is INSTALL_TIME_ONLY, never synced.
assert_eq "$CONFIG_BEFORE" \
    "$(cat "$REPO/.loom/config.json")" "config is byte-identical after the resync"
if grep -q '"model": "opus"' "$REPO/.loom/config.json"; then
    pass "the roleConfig.model pin survived the resync verbatim"
else
    fail "the roleConfig.model pin survived the resync verbatim"
fi

# And the lookup itself survived the resync into the installed tree.
assert_eq "$(cat "$REPO/defaults/scripts/resolve-role-model.sh")" \
    "$(cat "$REPO/.loom/scripts/resolve-role-model.sh")" \
    "resolve-role-model.sh is synced into the installed scripts surface"

echo
echo "Passed: $TESTS_PASSED / $TESTS_RUN"
if [[ $TESTS_FAILED -gt 0 ]]; then
    echo -e "${RED}FAILED: $TESTS_FAILED${NC}"
    exit 1
fi
exit 0
