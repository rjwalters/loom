#!/usr/bin/env bash
# test-champion-merge-queue-step-no-fallthrough.sh - Regression for #10256 (B3).
#
# FAILURE MODE: champion-pr-merge.md Step 3 treated `unrecognized subcommand`
# from `loom-daemon forge merge-queue step` as permission to run merge-pr.sh.
# A daemon predating `step` can already be in queue mode, so that bypassed the
# queue authorization. Only an explicit DIRECT sentinel (stdout, rc 0) may fall
# through. Hermetic: extracts the shipped Step 3 block and runs it with stubs.

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../../.." && pwd)"
DOC="$REPO_ROOT/defaults/.claude/commands/loom/champion-pr-merge.md"
[[ -f "$DOC" ]] || DOC="$REPO_ROOT/.claude/commands/loom/champion-pr-merge.md"

PASS=0; FAIL=0
ok()   { PASS=$((PASS+1)); echo "  PASS: $1"; }
fail() { FAIL=$((FAIL+1)); echo "  FAIL: $1"; }

TMP="$(mktemp -d)"; trap 'rm -rf "$TMP"' EXIT
mkdir -p "$TMP/bin" "$TMP/.loom/scripts"

# Step 3 block = the bash fence containing the `merge-queue step` call.
awk '/^```bash$/{buf="";inb=1;next} /^```$/{if(inb&&buf~/merge-queue step/){printf "%s",buf;exit} inb=0} inb{buf=buf $0 "\n"}' "$DOC" > "$TMP/step3.sh"
[[ -s "$TMP/step3.sh" ]] || { echo "FAIL: could not extract Step 3 block"; exit 1; }

cat > "$TMP/bin/git" <<'S'
#!/usr/bin/env bash
exit 0
S
cat > "$TMP/bin/gh" <<'S'
#!/usr/bin/env bash
echo deadbeef
S
cat > "$TMP/.loom/scripts/merge-pr.sh" <<'S'
#!/usr/bin/env bash
echo DIRECT_MERGE_CALLED
exit 0
S
chmod +x "$TMP"/bin/* "$TMP/.loom/scripts/merge-pr.sh"

# run_case <stdout> <stderr> <rc>  -> prints the block's combined output
run_case() {
  cat > "$TMP/bin/loom-daemon" <<S
#!/usr/bin/env bash
printf '%s' "$1"
printf '%s' "$2" >&2
exit $3
S
  chmod +x "$TMP/bin/loom-daemon"
  ( cd "$TMP" && PATH="$TMP/bin:$PATH" bash "$TMP/step3.sh" 42 2>&1 )
}

out=$(run_case $'LOOM-MERGE-QUEUE-DIRECT\n' '' 0)
[[ "$out" == *DIRECT_MERGE_CALLED* ]] && ok "DIRECT sentinel, rc 0 -> direct merge runs" || fail "DIRECT sentinel should merge"

out=$(run_case '' "error: unrecognized subcommand 'step'" 2)
[[ "$out" != *DIRECT_MERGE_CALLED* && "$out" == *"rc=7"* ]] && ok "unknown verb -> no direct merge" || fail "unknown verb must not merge directly"

out=$(run_case '' 'error: LOOM-MERGE-QUEUE-DIRECT' 0)
[[ "$out" != *DIRECT_MERGE_CALLED* ]] && ok "sentinel only on stderr -> no direct merge" || fail "stderr sentinel must not merge"

out=$(run_case $'LOOM-MERGE-QUEUE-DIRECT\n' '' 2)
[[ "$out" != *DIRECT_MERGE_CALLED* ]] && ok "DIRECT with nonzero rc -> no direct merge" || fail "nonzero rc must not merge"

out=$(run_case '' '' 0)
[[ "$out" != *DIRECT_MERGE_CALLED* ]] && ok "empty output -> no direct merge" || fail "empty output must not merge"

out=$(run_case $'LOOM-MERGE-QUEUE-QUEUED\n' '' 0)
[[ "$out" != *DIRECT_MERGE_CALLED* ]] && ok "queued -> no direct merge" || fail "queued must not merge"

echo "Results: $PASS passed, $FAIL failed"
[[ $FAIL -eq 0 ]]
