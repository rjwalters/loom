#!/usr/bin/env bash
# Tests per-scope MemoryMax sizing from the observed per-repo peak (#11094):
# lib/memory-budget.sh `loom_mem_scope_limit_mb` and spawn-claude.sh's
# systemd-run `-p MemoryMax=` wiring (history-gated, env-disable-able).
# Split from test-spawn-claude.sh, which is frozen by the file-size ratchet.
set -uo pipefail

SCRIPTS_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PASS=0
FAIL=0
ok() { PASS=$((PASS + 1)); echo "  ok: $1"; }
bad() { FAIL=$((FAIL + 1)); echo "  FAIL: $1"; }
eq() { if [[ "$2" == "$3" ]]; then ok "$1"; else bad "$1 (got '$2', want '$3')"; fi; }

T="$(mktemp -d)"
trap 'rm -rf "$T"' EXIT
GIB=$((1024 * 1024 * 1024))

# shellcheck source=../lib/memory-budget.sh
source "$SCRIPTS_DIR/lib/memory-budget.sh"
echo "loom_mem_scope_limit_mb (#11094)"
export LOOM_RAM_PEAKS_PATH="$T/peaks.json"
unset LOOM_SWEEP_MEMORY_MAX LOOM_SWEEP_MEMORY_MAX_PCT LOOM_SWEEP_MEMORY_MAX_MIN_MB
cat >"$LOOM_RAM_PEAKS_PATH" <<JSON
{"repos": {"big": [$((3 * GIB)), $((13 * GIB)), $((5 * GIB))], "small": [$((1 * GIB))], "empty": []}, "inflight": {}}
JSON
eq "no history for repo -> no limit" "$(loom_mem_scope_limit_mb nosuch 30000)" ""
eq "empty history -> no limit" "$(loom_mem_scope_limit_mb empty 30000)" ""
eq "high-water 13 GiB x200% = 26624 MiB" "$(loom_mem_scope_limit_mb big 65536)" "26624"
eq "small peak is raised to the 4096 MiB floor" "$(loom_mem_scope_limit_mb small 30000)" "4096"
eq "limit >= host RAM is pointless -> none" "$(loom_mem_scope_limit_mb big 20000)" ""
eq "PCT override" "$(LOOM_SWEEP_MEMORY_MAX_PCT=150 loom_mem_scope_limit_mb big 65536)" "19968"
eq "=0 disables" "$(LOOM_SWEEP_MEMORY_MAX=0 loom_mem_scope_limit_mb big 65536)" ""
eq "=off disables" "$(LOOM_SWEEP_MEMORY_MAX=off loom_mem_scope_limit_mb big 65536)" ""
eq "explicit MiB without history" "$(LOOM_SWEEP_MEMORY_MAX=8000 loom_mem_scope_limit_mb nosuch 30000)" "8000"
echo "garbage" >"$LOOM_RAM_PEAKS_PATH"
eq "corrupt file -> no limit" "$(loom_mem_scope_limit_mb big 65536)" ""
mv "$LOOM_RAM_PEAKS_PATH" "$T/peaks.moved"
eq "missing file -> no limit" "$(loom_mem_scope_limit_mb big 65536)" ""

echo "spawn-claude systemd scope MemoryMax (#11094)"
WS="$T/big"
mkdir -p "$WS/.loom" "$T/bin"
ln -s "$SCRIPTS_DIR" "$WS/.loom/scripts"
echo '{"runtimes": {"containment": {"enabled": false}}}' >"$WS/.loom/config.json"
printf '#!/usr/bin/env bash\necho "stub-claude ran"\n' >"$T/bin/claude"
printf '#!/usr/bin/env bash\necho 8\n' >"$T/bin/nproc"
printf '#!/usr/bin/env bash\nexit 0\n' >"$T/bin/systemctl"
SYSTEMD_RUN_LOG="$T/systemd-run.log"
cat >"$T/bin/systemd-run" <<STUB
#!/usr/bin/env bash
echo "\$*" >>"$SYSTEMD_RUN_LOG"
[[ -n "\${STUB_REJECT_MEMORY:-}" && "\$*" == *MemoryMax* && "\$*" == *"-- true" ]] && exit 1
while [[ \$# -gt 0 && "\$1" != "--" ]]; do shift; done
shift
exec "\$@"
STUB
chmod +x "$T/bin/"*
cat >"$T/peaks.json" <<JSON
{"repos": {"big": [$((2 * GIB))]}, "inflight": {}}
JSON

run() {
    : >"$SYSTEMD_RUN_LOG"
    env -u LOOM_SWEEP_CPU_QUOTA -u LOOM_SWEEP_MEMORY_MAX "$@" LOOM_WORKSPACE="$WS" LOOM_DAEMON_BIN=/bin/false \
        LOOM_SPAWN_NO_EXPORT=1 CLAUDE_CODE_OAUTH_TOKEN=fake-caller-token LOOM_SWEEP_INFLIGHT_SWEEPS=1 \
        LOOM_SYSTEMD_FORCE=1 LOOM_RAM_PEAKS_PATH="$T/peaks.json" PATH="$T/bin:$PATH" \
        "$SCRIPTS_DIR/spawn-claude.sh" -p ping 2>&1
}

OUT="$(run)"
FINAL="$(tail -n1 "$SYSTEMD_RUN_LOG")"
if [[ "$FINAL" == *"-p MemoryMax=4096M"* && "$FINAL" == *"-p OOMPolicy=continue"* && "$FINAL" == *"CPUQuota="* ]]; then
    ok "history -> real scope carries MemoryMax=4096M with OOMPolicy=continue"
else bad "history -> real scope carries MemoryMax: $FINAL"; fi
if [[ "$OUT" == *"stub-claude ran"* ]]; then ok "stub claude still runs"; else bad "stub claude still runs"; fi

run LOOM_SWEEP_MEMORY_MAX=0 >/dev/null
if grep -q MemoryMax "$SYSTEMD_RUN_LOG"; then bad "LOOM_SWEEP_MEMORY_MAX=0 disables"; else ok "LOOM_SWEEP_MEMORY_MAX=0 disables"; fi

OUT="$(run STUB_REJECT_MEMORY=1)"
FINAL="$(tail -n1 "$SYSTEMD_RUN_LOG")"
if [[ "$FINAL" != *MemoryMax* && "$FINAL" == *CPUQuota* && "$OUT" == *"stub-claude ran"* ]]; then
    ok "rejected MemoryMax probe degrades to no limit, CPU quota kept"
else bad "rejected MemoryMax probe degrades: $FINAL"; fi

mv "$T/peaks.json" "$T/peaks.moved2"
run >/dev/null
if grep -q MemoryMax "$SYSTEMD_RUN_LOG"; then bad "no history -> no MemoryMax"; else ok "no history -> no MemoryMax"; fi

echo "passed=$PASS failed=$FAIL"
[[ $FAIL -eq 0 ]]
