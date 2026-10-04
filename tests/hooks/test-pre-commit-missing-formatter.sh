#!/bin/sh
# Tests .githooks/pre-commit: a missing or failing cargo/pnpm formatter must
# stop the commit with a clear message, never silently skip (#10123).
set -u
HOOK="$(cd "$(dirname "$0")/../.." && pwd)/.githooks/pre-commit"
TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT
EXTRA_ENV=; PASS=0; FAIL=0; N=0
ok() { PASS=$((PASS + 1)); echo "PASS: $1"; }
bad() { FAIL=$((FAIL + 1)); echo "FAIL: $1"; }

BIN="$TMP/bin"; mkdir -p "$BIN"
for t in git xargs grep sed cat env; do
  p=$(command -v "$t") && ln -s "$p" "$BIN/$t"
done
STUBS="$TMP/stubs"; mkdir -p "$STUBS"
printf '#!/bin/sh\nexit 0\n' >"$STUBS/ok"; chmod +x "$STUBS/ok"
printf '#!/bin/sh\nexit 1\n' >"$STUBS/fail"; chmod +x "$STUBS/fail"
# loom-daemon stub whose secret-scan is unavailable (hook warns and allows)
printf '#!/bin/sh\nexit 1\n' >"$STUBS/noscan"; chmod +x "$STUBS/noscan"

# new_repo <file...>: fresh repo with the given files staged
new_repo() {
  N=$((N + 1)); R="$TMP/repo.$N"; mkdir -p "$R"
  (
    cd "$R" && git init -q . && git config user.email t@t && git config user.name t
    for f in "$@"; do mkdir -p "$(dirname "$f")"; echo x >"$f"; git add "$f"; done
  )
}
# run_hook <extra-bin-dir-or-empty> [stub-for-tool...]; sets RC, ERR
run_hook() {
  d="$TMP/path.$N"; mkdir -p "$d"; cp -P "$BIN"/* "$d"/
    shift
  for pair in "$@"; do ln -s "$STUBS/${pair#*=}" "$d/${pair%%=*}"; done
  ERR=$(cd "$R" && env PATH="$d" LOOM_DAEMON_SELF_BIN="$STUBS/noscan" \
    $EXTRA_ENV /bin/sh "$HOOK" 2>&1 >/dev/null); RC=$?
}

new_repo foo.rs
run_hook "" 
if { [ "$RC" -ne 0 ] && echo "$ERR" | grep -q cargo; }; then ok "missing cargo stops commit"; else bad "missing cargo (rc=$RC: $ERR)"; fi

new_repo src/a/x.ts
run_hook ""
if { [ "$RC" -ne 0 ] && echo "$ERR" | grep -q pnpm; }; then ok "missing pnpm stops commit"; else bad "missing pnpm (rc=$RC: $ERR)"; fi

new_repo foo.rs
run_hook "" cargo=fail
if [ "$RC" -ne 0 ]; then ok "failing cargo stops commit"; else bad "failing cargo (rc=$RC)"; fi

new_repo src/a/x.ts
run_hook "" pnpm=fail
if [ "$RC" -ne 0 ]; then ok "failing pnpm stops commit"; else bad "failing pnpm (rc=$RC)"; fi

new_repo foo.rs src/a/x.ts
run_hook "" cargo=ok pnpm=ok
if [ "$RC" -eq 0 ]; then ok "working formatters pass"; else bad "working formatters (rc=$RC: $ERR)"; fi

new_repo README.md
run_hook ""
if [ "$RC" -eq 0 ]; then ok "no staged code files: no tool check"; else bad "nothing relevant staged (rc=$RC: $ERR)"; fi

new_repo foo.rs
EXTRA_ENV="LOOM_PRECOMMIT_ALLOW_MISSING_FORMATTER=1"; run_hook ""; EXTRA_ENV=
if { [ "$RC" -eq 0 ] && echo "$ERR" | grep -q WARNING; }; then ok "opt-out downgrades to warning"; else bad "opt-out (rc=$RC: $ERR)"; fi

echo "Passed: $PASS Failed: $FAIL"
[ "$FAIL" -eq 0 ]
