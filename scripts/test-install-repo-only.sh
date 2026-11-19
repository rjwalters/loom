#!/usr/bin/env bash
# Test suite for the installers' --repo-only / LOOM_INSTALL_REPO_ONLY=1 option
# (issue #14570): write only the target repository's files, skipping every
# machine-level provisioning step.
#
# These tests use throwaway HOME directories and local Git repositories, with
# machine writes, builds, and forge calls stubbed out:
#   1. both installers document --repo-only in --help;
#   2. install-loom.sh: every machine-level provision_* call / user-scope MCP
#      registration sits in the `else`/`elif` arm of a REPO_ONLY guard, and the
#      flag + env var both set REPO_ONLY;
#   3. install.sh: wire_quick_install_guard_hooks (extracted and run with stubs)
#      skips provision_loom_hooks under --repo-only but still asserts the
#      project-level wiring (ensure_project_hook_wiring); and unset, does both;
#   4. install.sh --dry-run preview states nothing is written outside the repo,
#      and leaves the temp HOME empty;
#   5. Full Install's MCP phase leaves the user-served bundle and source
#      config intact in repo-only mode and preserves the staged target tree.
#   6. the full installer entry stages the same consumer tree with and without
#      --repo-only when both runs use the same source commit.
#
# Usage: bash scripts/test-install-repo-only.sh

set -uo pipefail

passed=0
failed=0
pass() { echo "PASS: $1"; passed=$((passed + 1)); }
fail() { echo "FAIL: $1"; failed=$((failed + 1)); }

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
LOOM_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
FULL="$LOOM_ROOT/scripts/install-loom.sh"
QUICK="$LOOM_ROOT/install.sh"

TEST_DIR="$(mktemp -d)"
trap 'rm -rf "$TEST_DIR"' EXIT

# --- 1. --help documents the flag -------------------------------------------
full_help="$(bash "$FULL" --help 2>&1)"
quick_help="$(bash "$QUICK" --help 2>&1)"
if grep -q -- '--repo-only' <<<"$full_help"; then
  pass "install-loom.sh --help documents --repo-only"
else
  fail "install-loom.sh --help does not mention --repo-only"
fi
if grep -q -- '--repo-only' <<<"$quick_help"; then
  pass "install.sh --help documents --repo-only"
else
  fail "install.sh --help does not mention --repo-only"
fi

# --- 2. install-loom.sh: every machine-level step is gated ------------------
# Walk the file; a machine-level call is OK only while inside the else/elif arm
# of a top-level `if [[ "$REPO_ONLY" == "true" ]]` block.
gate_report="$(awk '
  /^if \[\[ "\$REPO_ONLY" == "true" \]\]; then/ { guard=1; arm="then"; next }
  guard && /^(else|elif)/ { arm="else"; next }
  guard && /^fi/ { guard=0; arm=""; next }
  /^[[:space:]]*(provision_machine_daemon|provision_loom_dispatcher|provision_loom_skills|provision_loom_hooks) / {
    n++
    if (!(guard && arm == "else")) { print "UNGATED line " NR ": " $0 }
  }
  /claude mcp add --scope user loom -- node "\$MCP_MACHINE_ENTRY"; then/ {
    m++
    if (!(guard && arm == "else")) { print "UNGATED mcp line " NR ": " $0 }
  }
  END { print "COUNT " n " " m }
' "$FULL")"
if grep -q '^UNGATED' <<<"$gate_report"; then
  fail "install-loom.sh has an ungated machine-level step: $(grep '^UNGATED' <<<"$gate_report" | head -3)"
elif grep -q '^COUNT 4 1$' <<<"$gate_report"; then
  pass "install-loom.sh gates all 4 provision_* steps and the user-scope MCP registration"
else
  fail "install-loom.sh gate scan found unexpected counts: $(grep '^COUNT' <<<"$gate_report")"
fi

skips="$(grep -c 'Skipping .*(--repo-only)' "$FULL")"
if [[ "$skips" -ge 6 ]]; then
  pass "install-loom.sh prints a skip line for each of the 6 machine-level steps ($skips found)"
else
  fail "install-loom.sh has only $skips '--repo-only' skip lines (expected >= 6)"
fi

if grep -q 'LOOM_INSTALL_REPO_ONLY' "$FULL" && grep -q -- '--repo-only) REPO_ONLY=true' "$FULL"; then
  pass "install-loom.sh sets REPO_ONLY from both the flag and LOOM_INSTALL_REPO_ONLY"
else
  fail "install-loom.sh does not wire both the flag and the env var to REPO_ONLY"
fi

# --- 3. install.sh: hook wiring split (behavioural, with stubs) -------------
run_wire() {
  local repo_only="$1" home="$2"
  local fn
  fn="$(sed -n '/^wire_quick_install_guard_hooks() {/,/^}/p' "$QUICK")"
  [[ -n "$fn" ]] || { echo "EXTRACT_FAILED"; return 1; }
  HOME="$home" REPO_ONLY="$repo_only" bash -c '
    info() { :; }
    warning() { echo "WARN $*"; }
    provision_loom_hooks() { mkdir -p "$HOME/.claude"; echo x > "$HOME/.claude/settings.json"; echo "CALLED provision_loom_hooks"; }
    ensure_project_hook_wiring() { echo "CALLED ensure_project_hook_wiring"; }
    '"$fn"'
    wire_quick_install_guard_hooks /some/target
  '
}

H1="$TEST_DIR/home-repo-only"; mkdir -p "$H1"
out="$(run_wire true "$H1")"
if grep -q 'CALLED provision_loom_hooks' <<<"$out" || [[ -e "$H1/.claude" ]]; then
  fail "install.sh --repo-only still provisioned user-scope hooks"
else
  pass "install.sh --repo-only skips provision_loom_hooks; HOME/.claude untouched"
fi
if grep -q 'CALLED ensure_project_hook_wiring' <<<"$out"; then
  pass "install.sh --repo-only still asserts project-level hook wiring"
else
  fail "install.sh --repo-only dropped ensure_project_hook_wiring"
fi

H2="$TEST_DIR/home-default"; mkdir -p "$H2"
out="$(run_wire false "$H2")"
if grep -q 'CALLED provision_loom_hooks' <<<"$out" && grep -q 'CALLED ensure_project_hook_wiring' <<<"$out"; then
  pass "install.sh default (no flag) still provisions user-scope hooks and project wiring"
else
  fail "install.sh default behaviour changed: $out"
fi

# The quick-install daemon provisioning call sites must be gated too.
ungated_daemon="$(awk '
  /if \[\[ "\$REPO_ONLY" == "true" \]\]; then/ { guard=1; arm="then"; next }
  guard && /^[[:space:]]*else/ { arm="else"; next }
  guard && /^[[:space:]]*fi/ { guard=0; arm=""; next }
  /^[[:space:]]*provision_machine_daemon "\$LOOM_CARGO_TARGET_DIR/ {
    n++; if (!(guard && arm == "else")) print "UNGATED line " NR
  }
  END { print "COUNT " n }
' "$QUICK")"
if grep -q '^UNGATED' <<<"$ungated_daemon"; then
  fail "install.sh has an ungated provision_machine_daemon call: $ungated_daemon"
elif grep -q '^COUNT 2$' <<<"$ungated_daemon"; then
  pass "install.sh gates both quick-install provision_machine_daemon calls"
else
  fail "install.sh provision_machine_daemon gate scan unexpected: $ungated_daemon"
fi

# --- 4. install.sh preview and mode validation under --repo-only ----------------------
REPO="$TEST_DIR/target"; mkdir -p "$REPO"
git -C "$REPO" init --quiet
git -C "$REPO" config user.email t@example.com
git -C "$REPO" config user.name T
git -C "$REPO" commit -q --allow-empty -m init
H3="$TEST_DIR/home-dryrun"; mkdir -p "$H3"
plan="$(HOME="$H3" bash "$QUICK" --dry-run --quick --repo-only "$REPO" 2>&1)"; rc=$?
if [[ $rc -eq 0 ]] && grep -q 'Outside the target repository: nothing' <<<"$plan" \
   && ! grep -q 'user-level, written on this machine' <<<"$plan"; then
  pass "install.sh --dry-run --repo-only preview reports no machine-level writes"
else
  fail "install.sh --dry-run --repo-only preview wrong (rc=$rc)"
fi
plan="$(HOME="$H3" LOOM_INSTALL_REPO_ONLY=1 bash "$QUICK" --dry-run --quick "$REPO" 2>&1)"
if grep -q 'Outside the target repository: nothing' <<<"$plan"; then
  pass "LOOM_INSTALL_REPO_ONLY=1 (env only) behaves like --repo-only"
else
  fail "LOOM_INSTALL_REPO_ONLY=1 did not enable repo-only behaviour"
fi
plan="$(HOME="$H3" bash "$QUICK" --dry-run --quick "$REPO" 2>&1)"
if grep -q 'user-level, written on this machine' <<<"$plan"; then
  pass "install.sh default preview still lists the machine-level writes"
else
  fail "install.sh default preview lost the machine-level section"
fi
if [[ -z "$(ls -A "$H3")" ]]; then
  pass "temp HOME is still empty after the dry-runs"
else
  fail "dry-run wrote into HOME: $(ls -A "$H3")"
fi

if combo="$(bash "$QUICK" --repo-only --tools-only --help 2>&1)" ||
   ! grep -q -- 'cannot be combined with --repo-only' <<<"$combo"; then
  fail "install.sh must reject mixing --repo-only with machine tools provisioning"
else
  pass "install.sh rejects --repo-only with --tools-only before delegation"
fi

# --- 5. Real Full Install MCP phase in isolated staged worktrees ------------
MCP_PHASE="$TEST_DIR/full-mcp-phase.sh"
cat > "$MCP_PHASE" <<'PHASE'
set -eu
header() { :; }
info() { printf '%s\n' "$*"; }
success() { info "$@"; }
warning() { info "$@"; }
PHASE
awk '
  /^CURRENT_STEP="MCP Setup"/ { active=1 }
  /^# STEP 5: Configure Branch Rulesets/ { exit }
  active { print }
' "$FULL" >> "$MCP_PHASE"

MCP_STUBS="$TEST_DIR/mcp-stubs"
mkdir -p "$MCP_STUBS"
cat > "$MCP_STUBS/node" <<'STUB'
#!/bin/bash
exit 1
STUB
cat > "$MCP_STUBS/npm" <<'STUB'
#!/bin/bash
printf '%s\n' "npm $*" >> "$MCP_CALLS"
case "$*" in
  'install --silent') ;;
  'run build') printf '%s\n' 'rebuilt bundle' > dist/index.js ;;
  *) exit 1 ;;
esac
STUB
cat > "$MCP_STUBS/claude" <<'STUB'
#!/bin/bash
printf '%s\n' "claude $*" >> "$MCP_CALLS"
STUB
chmod +x "$MCP_STUBS/node" "$MCP_STUBS/npm" "$MCP_STUBS/claude"

mcp_config='{"mcpServers":{"loom":{"command":"old"},"other":{"command":"keep"}}}'
run_full_mcp() {
  local fixture="$1" repo_only="$2"
  local source="$fixture/source" target="$fixture/target"
  mkdir -p "$source/scripts" "$source/mcp-loom/"{dist,src} "$target" || return
  cp "$LOOM_ROOT/scripts/setup-mcp.sh" "$source/scripts/setup-mcp.sh" || return
  ln -s "$source" "$fixture/machine-checkout" || return
  printf '%s\n' 'current user bundle' > "$source/mcp-loom/dist/index.js"
  touch -t 200001010000 "$source/mcp-loom/dist/index.js"
  printf '%s\n' 'new source' > "$source/mcp-loom/src/main.ts"
  printf '%s\n' "$mcp_config" > "$source/.mcp.json"
  printf '%s\n' "$mcp_config" > "$target/.mcp.json"
  (
    cd "$target" || exit
    PATH="$MCP_STUBS:$PATH" LOOM_ROOT="$source" LOOM_HOME="$fixture/machine-checkout" \
      REPO_ONLY="$repo_only" MCP_CALLS="$fixture/calls" bash "$MCP_PHASE" > "$fixture/output" 2>&1
  )
}

repo_only_mcp="$TEST_DIR/mcp-repo-only"
default_mcp="$TEST_DIR/mcp-default"
if run_full_mcp "$repo_only_mcp" true && [[ ! -e "$repo_only_mcp/calls" ]] &&
   [[ "$(cat "$repo_only_mcp/machine-checkout/mcp-loom/dist/index.js")" == 'current user bundle' ]] &&
   [[ "$(cat "$repo_only_mcp/source/.mcp.json")" == "$mcp_config" ]]; then
  pass "Full Install --repo-only leaves the user-served MCP bundle and source config untouched"
else
  fail "Full Install --repo-only touched machine MCP state or source configuration"
fi
if run_full_mcp "$default_mcp" false &&
   grep -Fxq 'npm run build' "$default_mcp/calls" &&
   grep -Fq 'claude mcp add --scope user loom ' "$default_mcp/calls" &&
   [[ "$(cat "$default_mcp/machine-checkout/mcp-loom/dist/index.js")" == 'rebuilt bundle' ]] &&
   ! grep -q '"loom"' "$default_mcp/source/.mcp.json"; then
  pass "default Full Install still refreshes and registers the machine MCP bundle"
else
  fail "default Full Install MCP behavior changed"
fi
if diff -r "$repo_only_mcp/target" "$default_mcp/target"; then
  pass "Full Install MCP phase preserves identical staged target files in both modes"
else
  fail "Full Install changed staged target file content between modes"
fi

# --- 6. Full installer entry with local staging and inert boundaries ---------
FULL_FIXTURE="$(cd "$TEST_DIR" && pwd -P)/full-stage"
STAGE_PATH=.loom/worktrees/install-fixture

prepare_full_stage() (
  local root="$1" source="$1/source" name function_name verifier
  mkdir -p "$root/bin" "$root/build/release" "$root/home" || return
  export HOME="$root/home" GIT_CONFIG_NOSYSTEM=1 GIT_CONFIG_GLOBAL=/dev/null GIT_ALLOW_PROTOCOL=file
  git clone --quiet --shared --no-checkout -- "$LOOM_ROOT" "$source" || return
  git -C "$source" remote remove origin || return
  git -C "$source" checkout --quiet -B main "$(git -C "$LOOM_ROOT" rev-parse HEAD)" || return

  # Stock macOS /bin/bash is below Loom's Bash 4 floor. Run the unmodified
  # verifier body with the fixture's PATH Bash when the system Bash is older.
  if [[ "$(/bin/bash -c 'echo "${BASH_VERSINFO[0]}"')" -lt 4 ]]; then
    verifier="$source/defaults/scripts/verify-install.sh"
    sed '1s|^#!/bin/bash$|#!/usr/bin/env bash|' "$verifier" > "$root/verifier" || return
    cat "$root/verifier" > "$verifier" || return
  fi

  # The two targets use identical starting commits and independent worktrees.
  git init --quiet -b main "$root/seed" || return
  printf '%s\n' '# Consumer repository' > "$root/seed/README.md"
  printf '%s\n' "$mcp_config" > "$root/seed/.mcp.json"
  printf '%s\n' '.loom/worktrees/' '.loom/loom-source-path' > "$root/seed/.gitignore"
  git -C "$root/seed" add -A || return
  git -C "$root/seed" -c user.name=Fixture -c user.email=fixture@example.invalid \
    -c commit.gpgsign=false commit --quiet -m seed || return

  ln -s "$(command -v bash)" "$root/bin/bash" || return
  ln -s "$(command -v jq)" "$root/bin/jq" || return
  cat > "$root/bin/shim" <<'SHIM' || return
#!/usr/bin/env bash
set -eu
case "${0##*/}:$*" in
  'node:--version') echo v22.0.0 ;;
  'node:-pe '*) jq -r .version "$FIXTURE_SOURCE/package.json" ;;
  'cargo:--version') echo 'cargo 1.90.0' ;;
  'pnpm:--version') echo 10.15.1 ;;
  'gh:pr list '*|'gh:label create '*) : ;;
  'claude:mcp remove --scope user loom') : ;;
  'claude:mcp add --scope user loom -- node '*) echo claude-register >> "$FIXTURE_MACHINE" ;;
  'date:+%Y-%m-%d') echo 2026-01-01 ;;
  'date:-u +%Y-%m-%dT%H:%M:%SZ') echo 2026-01-01T00:00:00Z ;;
  'date:'*) exec /bin/date "$@" ;;
  *) printf 'Blocked external call: %s\n' "${0##*/}" >&2; exit 1 ;;
esac
SHIM
  chmod +x "$root/bin/shim" || return
  for name in node cargo pnpm gh claude date npm curl wget ssh; do
    ln -s shim "$root/bin/$name" || return
  done

  for name in daemon dispatcher skills hooks; do
    case "$name" in
      daemon) function_name=provision_machine_daemon ;;
      *) function_name="provision_loom_$name" ;;
    esac
    printf '%s() { echo %s >> "$FIXTURE_MACHINE"; }\n' "$function_name" "$name" \
      > "$source/scripts/install/provision-$name.sh" || return
  done
  cat > "$source/scripts/setup-mcp.sh" <<'SHIM' || return
#!/usr/bin/env bash
echo setup-mcp >> "$FIXTURE_MACHINE"
SHIM
  cat > "$source/scripts/daemon-build.sh" <<'SHIM' || return
#!/usr/bin/env bash
printf '%s\n' "$FIXTURE_BUILD"
SHIM
  cat > "$root/build/release/loom-daemon" <<'SHIM' || return
#!/usr/bin/env bash
set -euo pipefail
case "$*" in
  '--version') echo 'loom-daemon fixture'; exit ;;
  'forge merge-config --help'|'forge merge-config --verbose') exit ;;
  "init --defaults $FIXTURE_SOURCE/defaults .") ;;
  *) exit 1 ;;
esac
test "$PWD" = "$FIXTURE_TARGET/$FIXTURE_STAGE"
defaults="$3"
# Inert init scaffold, as in test-installer.sh. The actual shell installer
# still writes configs, source/install metadata, merge attributes and checksums.
mkdir -p .loom
cp -RL "$defaults/"{roles,scripts,runtimes,docs,config.json,pricing.json} .loom/
cp -RL "$defaults/.loom/." .loom/
cp -RL "$defaults/"{.claude,.github,loom.sh,package.json} .
cp "$defaults/.loom-README.md" .loom/README.md
printf '%s\n' '<!-- BEGIN LOOM ORCHESTRATION -->' '# Loom guide' '<!-- END LOOM ORCHESTRATION -->' > CLAUDE.md
SHIM
  cat > "$source/scripts/install/validate-target.sh" <<'SHIM' || return
#!/usr/bin/env bash
[[ "$*" == "$FIXTURE_TARGET" ]]
SHIM
  cat > "$source/scripts/install/create-worktree.sh" <<'SHIM' || return
#!/usr/bin/env bash
set -eu
[[ "$*" == "$FIXTURE_TARGET" ]]
git -C "$1" worktree add --quiet -b install-fixture "$FIXTURE_STAGE" HEAD
printf '%s|install-fixture|main\n' "$FIXTURE_STAGE"
SHIM
  cat > "$source/scripts/install/setup-repository-settings.sh" <<'SHIM' || return
#!/usr/bin/env bash
[[ "$*" == "$FIXTURE_TARGET" ]]
SHIM
  cat > "$source/scripts/install/create-pr.sh" <<'SHIM' || return
#!/usr/bin/env bash
set -eu
[[ "$*" == "$FIXTURE_TARGET/$FIXTURE_STAGE main" ]]
git -C "$1" add -A
git -C "$1" write-tree > "$FIXTURE_TREE"
printf '%s\n' 'https://example.invalid/fixture/target/pull/1|manual'
SHIM
  chmod +x "$root/build/release/loom-daemon" "$source/scripts/daemon-build.sh" \
    "$source/scripts/setup-mcp.sh" "$source/scripts/install/"{validate-target,create-worktree,setup-repository-settings,create-pr}.sh
)

run_full_stage() (
  local mode="$1" root="$FULL_FIXTURE" target="$FULL_FIXTURE/$1/target"
  shift
  export HOME="$root/$mode/home" GIT_CONFIG_NOSYSTEM=1 GIT_CONFIG_GLOBAL=/dev/null GIT_ALLOW_PROTOCOL=file
  mkdir -p "$HOME" || return
  git clone --quiet --local -- "$root/seed" "$target" || return
  git -C "$target" remote set-url origin https://example.invalid/fixture/target.git || return
  env -i HOME="$HOME" PATH="$root/bin:/usr/bin:/bin" LC_ALL=C \
    GIT_CONFIG_NOSYSTEM=1 GIT_CONFIG_GLOBAL=/dev/null GIT_ALLOW_PROTOCOL=file \
    FIXTURE_SOURCE="$root/source" FIXTURE_BUILD="$root/build" \
    FIXTURE_TARGET="$target" FIXTURE_STAGE="$STAGE_PATH" \
    FIXTURE_MACHINE="$root/$mode/machine" FIXTURE_TREE="$root/$mode/git-tree" \
    bash "$root/source/scripts/install-loom.sh" --yes --no-dogfood "$@" "$target" \
      < /dev/null > "$root/$mode/output" 2>&1 || {
        cat "$root/$mode/output" >&2
        return 1
      }
  if grep -q 'Installation Complete!' "$root/$mode/output" &&
    git -C "$target/$STAGE_PATH" diff --cached --name-only > "$root/$mode/staged" &&
    grep -Fxq '.loom/install-metadata.json' "$root/$mode/staged" &&
    grep -Fxq '.gitattributes' "$root/$mode/staged" &&
    test -s "$target/$STAGE_PATH/.loom/manifest.json"; then
    return 0
  fi
  printf 'Missing completed stage for %s\n' "$mode" >&2
  cat "$root/$mode/output" >&2
  return 1
)

default_stage="$FULL_FIXTURE/default/target/$STAGE_PATH"
repo_only_stage="$FULL_FIXTURE/repo-only/target/$STAGE_PATH"
if prepare_full_stage "$FULL_FIXTURE" && run_full_stage default && run_full_stage repo-only --repo-only; then
  if [[ ! -s "$FULL_FIXTURE/repo-only/machine" ]] &&
     [[ "$(grep -c 'Skipping .*(--repo-only)' "$FULL_FIXTURE/repo-only/output")" -eq 6 ]] &&
     cmp -s "$FULL_FIXTURE/default/machine" <(printf '%s\n' daemon dispatcher skills hooks setup-mcp claude-register); then
    pass "Full installer entry exercises default machine steps and skips all six in repo-only mode"
  else
    fail "Full installer entry did not exercise the two provisioning modes"
  fi
  if diff -r -x .git "$default_stage" "$repo_only_stage" &&
     cmp -s "$FULL_FIXTURE/default/git-tree" "$FULL_FIXTURE/repo-only/git-tree"; then
    pass "Full installer entry produces identical staged target trees from the same source"
  else
    fail "Full installer entry staged target trees differ between modes"
  fi
else
  fail "Full installer entry did not stage a complete target in both modes"
fi

echo ""
echo "passed=$passed failed=$failed"
[[ "$failed" -eq 0 ]]
