#!/usr/bin/env bash
# Test suite for defaults/hooks/guard-destructive-generic.sh — cargo and perf.
#
# One slice of the former monolithic tests/hooks/test-guard-destructive.sh,
# split per #7741. Shared fixtures, assertions and catastrophic-phrase payloads
# live in tests/hooks/lib/guard-destructive-harness.sh.
#
# Usage: ./tests/hooks/test-guard-destructive-cargo-and-perf.sh

set -euo pipefail
# shellcheck source=tests/hooks/lib/guard-destructive-harness.sh
. "$(cd "$(dirname "$0")" && pwd)/lib/guard-destructive-harness.sh"

echo -e "${YELLOW}--- Cargo clean scope (guards.cargoCleanScope / LOOM_GUARD_CARGO_CLEAN) (#6684) ---${NC}"
# =========================================================================
#
# A bare `cargo clean` on a host whose `.cargo/config.toml` sets a
# `build.target-dir` SHARED outside the repo deletes every project's build
# output on that host, including an unrelated in-flight sweep's — see the
# issue's own repro (robb-studio, 2026-08-21). The four hermetic cases below
# are exactly the ones the issue's acceptance criteria list.

# Create a throwaway git repo with an optional .cargo/config.toml body and an
# optional .loom/config.json body. Echoes the repo path (becomes the guard's
# cwd / resolved REPO_ROOT). Run via command substitution, like make_sql_repo.

# Same as make_cargo_repo, but the echoed path reaches the repo through a
# SYMLINKED ancestor: the repo really lives at <tmp>/real/repo and is handed to
# the guard as <tmp>/link/repo. `git -C <cwd> rev-parse --show-toplevel` — how
# the guard resolves REPO_ROOT — returns the symlink-RESOLVED <tmp>/real/repo,
# while the guard's own $CWD (and therefore anything the .cargo config walk-up
# builds from it) keeps the <tmp>/link/repo spelling. That reproduces on ANY
# host the divergence that is the DEFAULT state of a $TMPDIR repo on macOS,
# where /var is a symlink to /private/var: the two spellings describe one
# directory but never string-match, so a purely lexical containment test reads
# a genuinely repo-local target-dir as "shared outside the repo" (#6684 review).

CARGO_NOCONFIG_REPO=$(make_cargo_repo '' '')
CARGO_SHARED_REPO=$(make_cargo_repo "$(printf '[build]\ntarget-dir = "/tmp/loom-test-shared-cargo-target-6684"\n')" '')
CARGO_LOCAL_REL_REPO=$(make_cargo_repo "$(printf '[build]\ntarget-dir = "target"\n')" '')
CARGO_OFF_REPO=$(make_cargo_repo "$(printf '[build]\ntarget-dir = "/tmp/loom-test-shared-cargo-target-6684"\n')" '{"guards":{"cargoCleanScope":false}}')
CARGO_SYMLINK_LOCAL_REPO=$(make_cargo_symlinked_repo "$(printf '[build]\ntarget-dir = "target"\n')")
CARGO_SYMLINK_SHARED_REPO=$(make_cargo_symlinked_repo "$(printf '[build]\ntarget-dir = "/tmp/loom-test-shared-cargo-target-6684"\n')")

# --- Hermetic case 1 (acceptance criteria): repo-local target -> no prompt ---
assert_allow "Cargo clean: no .cargo/config.toml at all (implicit repo-local <repo>/target) allows" \
    "cargo clean" "$CARGO_NOCONFIG_REPO"
assert_allow "Cargo clean: .cargo/config.toml with a repo-RELATIVE target-dir (resolves inside repo) allows" \
    "cargo clean" "$CARGO_LOCAL_REL_REPO"

# --- Regression (#6684 review): the SAME repo-local case, but reached through
#     a symlinked ancestor. This is the macOS default (/var -> /private/var for
#     every $TMPDIR/mktemp -d path), where the case above asked instead of
#     allowing while Linux CI stayed green — the guard's REPO_ROOT is
#     symlink-resolved by git, the config-derived target-dir is not, so a
#     lexical-only prefix comparison saw two different-looking paths for one
#     directory. Both spellings must now agree before the ask fires. ---
assert_allow "Cargo clean: repo-RELATIVE target-dir still allows when the repo is reached via a SYMLINKED path (#6684 macOS /var regression)" \
    "cargo clean" "$CARGO_SYMLINK_LOCAL_REPO"

# --- Hermetic case 2 (acceptance criteria): shared external target-dir -> ask ---
assert_ask "Cargo clean: .cargo/config.toml build.target-dir resolves OUTSIDE the repo asks" \
    "cargo clean" "$CARGO_SHARED_REPO"
assert_ask_reason_matches "Cargo clean ask names the resolved shared path and the fix" \
    "cargo clean" "target-dir is shared at '/tmp/loom-test-shared-cargo-target-6684'.*cargo clean -p.*CARGO_TARGET_DIR" \
    "$CARGO_SHARED_REPO"
# Symlink-resolving the containment test must not neuter the ask: a genuinely
# shared target-dir still asks when the repo is reached via a symlinked path.
assert_ask "Cargo clean: shared external target-dir still asks when the repo is reached via a SYMLINKED path" \
    "cargo clean" "$CARGO_SYMLINK_SHARED_REPO"

# --- Hermetic case 3 (acceptance criteria): -p-scoped clean against a shared
#     dir -> no prompt, no behavior change on the common case ---
assert_allow "Cargo clean -p <pkg>: unaffected even with a shared external target-dir" \
    "cargo clean -p somepkg" "$CARGO_SHARED_REPO"
assert_allow "Cargo clean --package <pkg>: unaffected even with a shared external target-dir" \
    "cargo clean --package somepkg" "$CARGO_SHARED_REPO"

# --- Hermetic case 4 (acceptance criteria): CARGO_TARGET_DIR pointing at
#     scratch -> no prompt (an explicit override is always treated as
#     deliberate, however it resolves) ---
assert_allow "Cargo clean: same-command CARGO_TARGET_DIR=<scratch> overrides the shared config, allows" \
    "CARGO_TARGET_DIR=/tmp/loom-test-scratch-6684 cargo clean" "$CARGO_SHARED_REPO"
assert_allow_env "Cargo clean: process-env CARGO_TARGET_DIR=<scratch> overrides the shared config, allows" \
    "CARGO_TARGET_DIR=/tmp/loom-test-scratch-6684" "cargo clean" "$CARGO_SHARED_REPO"

# --- Toggle: guards.cargoCleanScope:false opts out, LOOM_GUARD_CARGO_CLEAN
#     env override wins over config either direction ---
assert_allow "Cargo clean config-off (guards.cargoCleanScope:false): shared target-dir no longer asks" \
    "cargo clean" "$CARGO_OFF_REPO"
assert_ask_env "LOOM_GUARD_CARGO_CLEAN=1 overrides config-off: shared target-dir still asks" \
    "LOOM_GUARD_CARGO_CLEAN=1" "cargo clean" "$CARGO_OFF_REPO"
assert_allow_env "LOOM_GUARD_CARGO_CLEAN=0 overrides config-on: shared target-dir no longer asks" \
    "LOOM_GUARD_CARGO_CLEAN=0" "cargo clean" "$CARGO_SHARED_REPO"

# --- Opt-out must NOT weaken unrelated guards ---
assert_deny "Cargo clean config-off: rm -rf / still blocked" \
    "rm -rf /" "$CARGO_OFF_REPO"

# Clean up temp repos created above.
for _cargo_dir in "$CARGO_NOCONFIG_REPO" "$CARGO_SHARED_REPO" "$CARGO_LOCAL_REL_REPO" "$CARGO_OFF_REPO"; do
    [[ -n "$_cargo_dir" && "$_cargo_dir" != "/" && -d "$_cargo_dir/.git" ]] && rm -rf "$_cargo_dir"
done
# The symlinked repos are <tmp-base>/link/repo — remove the whole <tmp-base>
# (removing the echoed path itself would delete through the symlink and leave
# the base behind).
for _cargo_dir in "$CARGO_SYMLINK_LOCAL_REPO" "$CARGO_SYMLINK_SHARED_REPO"; do
    _cargo_base="${_cargo_dir%/link/repo}"
    [[ -n "$_cargo_base" && "$_cargo_base" != "/" && "$_cargo_base" != "$_cargo_dir" && -d "$_cargo_base/real/repo/.git" ]] && rm -rf "$_cargo_base"
done

# =========================================================================
echo -e "${YELLOW}--- #6472: sed -n \$((...)) piped to grep -i, and nested/escaped-quoted awk '>' ---${NC}"
# =========================================================================

# Fresh fixture, not the file-global $WT_REPO -- that one is already rm -rf'd
# by the cleanup earlier in this file (see the `rm -rf "$WT_REPO" ...` block),
# so reusing it here for the deny assertions below would silently no-op
# (guard sees no resolvable repo root and allows everything).
WT_REPO_6472=$(make_wt_repo)

# Narrowed shape-3 repro (#6472): a `sed -n` (no `-i`) print-only range whose
# script argument contains a `$((...))` arithmetic expansion, piped to a
# LATER pipeline segment carrying an `-i`-prefixed flag (`grep -i`/`grep
# -iE`), was misread as ONE un-split sed segment: the later segment's `-i`
# flag leaked into the sed segment's own toks[] (setting has_i=1 with no real
# `-i` anywhere in the sed invocation itself), and the literal `|` that
# should have separated the two commands -- having failed to split -- was
# then scanned as a phantom write-target argument, denying with target `|`.
#
# Root cause: qsplit() (#3755) decides a quoted span carrying a `$(` (which
# `$((...))` contains, being `$(` followed by a second `(`) must keep its
# separators ACTIVE (so a smuggled `"$(a|halt)"` still splits), but then
# resumed the top-level quote-detection loop character by character with NO
# "still inside this span" state. When that loop naturally reached the
# span's own real closing quote, that byte was misread as the OPENING of a
# BRAND NEW span -- one that runs to the NEXT unrelated same-type quote
# character later in the command (here, the grep pattern's own quote) and
# copies everything in between, including the real `|`, as if it were inert
# quoted data. Fixed by having qsplit() walk directly to the
# already-located real closing quote and emit it literally, without ever
# re-entering the top of the loop for that byte.
assert_allow "write-confinement (#6472): sed -n with \$((...)) arithmetic range piped to grep -i allows (was denied to target '|')" \
    'L=$(grep -n "Loom daemon starting" ~/.loom/daemon.log | tail -1 | cut -d: -f1); sed -n "$((L-60)),$((L+40))p" ~/.loom/daemon.log | grep -iE "role_runner" | cut -c1-300 | head -12' "$WT_REPO_6472"
assert_allow "write-confinement (#6472): minimal sed -n \$((...)) piped to grep -iE allows" \
    'sed -n "$((L-60)),$((L+40))p" ~/.loom/daemon.log | grep -iE "role_runner"' "$WT_REPO_6472"
assert_allow "write-confinement (#6472 regression): plain sed -n \"1,5p\" piped to grep -i still allows (no \$((...)) involved -- already allowed pre-fix, guards against a fix narrowing too far)" \
    'sed -n "1,5p" ~/.loom/daemon.log | grep -i pattern' "$WT_REPO_6472"

# Nested/escaped-quoting awk repro (#6472): an awk double-quoted `>`/`<`
# comparison reached through an ADDITIONAL layer of escaped quoting (a
# single-quoted Python string, itself inside a double-quoted `python3 -c`
# argument, containing a backslash-escaped `\"`) was misread by mask_gt()'s
# quote-state tracking: a bare `"`/`'"'"'` byte toggles quote mode
# unconditionally, including one that is backslash-escaped and therefore, in
# real shell semantics, still just literal DATA inside the still-open outer
# double-quoted span. The escaped `\"` flipped mode back to "unquoted"
# mid-string, exposing the awk program's internal `>` as a live redirect
# operator and denying with a quoted-operand fragment as the write target.
# Fixed by tracking an `esc` flag in mask_gt() so a backslash-escaped byte
# (unquoted or double-quoted context only -- never single-quoted, where a
# backslash has no escaping power in real bash) can never toggle quote mode.
#
# NOTE: the issue'"'"'s own two simpler repro shapes (a bare double-quoted awk
# `>` comparison, with or without ssh-wrapping, and the same with `awk -v`
# variables) do NOT reproduce as literally written -- confirmed allow both
# before and after this fix (curator verified this during triage; not
# re-asserted here as a "deny that must become allow" since it was never a
# deny to begin with). Only the nested/escaped-quoting shape below denied.
# Built as separate variables (rather than inlined directly in the
# assert_allow argument list) to avoid the fragile '"'"'-inside-'"'"'
# nesting that previously caused the description's inert prose `>` to be
# misread as a live shell redirection operator, writing a stray file into
# the repo root on every test run (#7328). The description uses $'...'
# ANSI-C quoting (\' -> a literal single quote, no quote-breaking needed);
# the command string's quoting is unchanged from before this fix.
_desc_6472_nested_gt=$'write-confinement (#6472): nested/escaped-quoted awk \'>\' comparison via python3 -c allows (was denied with a quoted-operand write target)'
_cmd_6472_nested_gt='python3 -c "import subprocess; subprocess.run('"'"'awk \"\$1 > \"x\"\"'"'"')"'
assert_allow "$_desc_6472_nested_gt" "$_cmd_6472_nested_gt" "$WT_REPO_6472"
unset _desc_6472_nested_gt _cmd_6472_nested_gt

# True-positive baselines (#6472): both fixes above must not loosen the
# fail-closed floor -- sed -i, tee, cp, mv, and a bare '>' redirect into the
# main checkout must all still deny exactly as before (#4921/#6172), and the
# #3755 anti-smuggling floor (a real separator hidden inside a quoted `$(...)`
# command substitution) must still be caught.
assert_deny "write-confinement (#6472 control): sed -i on main-checkout path still denies" \
    "sed -i 's/a/b/' $WT_REPO_6472/f" "$WT_REPO_6472"
assert_deny "write-confinement (#6472 control): tee to main-checkout path still denies" \
    "echo x | tee $WT_REPO_6472/f" "$WT_REPO_6472"
assert_deny "write-confinement (#6472 control): cp destination in main checkout still denies" \
    "cp /tmp/a.sh $WT_REPO_6472/defaults/hooks/f.sh" "$WT_REPO_6472"
assert_deny "write-confinement (#6472 control): mv destination in main checkout still denies" \
    "mv /tmp/a.sh $WT_REPO_6472/defaults/hooks/f.sh" "$WT_REPO_6472"
assert_deny "write-confinement (#6472 control): bare '>' redirect to main-checkout path still denies" \
    "echo x > $WT_REPO_6472/defaults/hooks/f.sh" "$WT_REPO_6472"
assert_deny "#6472 control: smuggled \$(x|halt ) command substitution inside a quoted grep -E pattern still denies (qsplit's #3755 anti-smuggling floor)" \
    'grep -E "$(x|halt )" file'
rm -rf "$WT_REPO_6472"

echo ""

# =========================================================================
echo -e "${YELLOW}--- Performance check ---${NC}"
# =========================================================================

# NOTE (#3687): `git status` is now a read-only FAST-PATH command — with the
# default toggle ON it exits after one bash-builtin structural test + one lazy
# jq config read, skipping the ~37-fork deny/ask gauntlet and the git rev-parse
# entirely. This benchmark command should therefore be dramatically cheaper than
# the historical full-path average (~179ms measured pre-#3687 → ~1 jq read).
# Export LOOM_GUARD_READONLY_FASTPATH=0 to benchmark the full-path cost instead.
#
# The measured average is dominated by 10 sequential guard process spawns
# (shell + jq/python3 interpreter startup), which is a function of machine
# load rather than guard-logic complexity. A hard cap therefore flakes under
# contention, so by default this row is INFORMATIONAL: it always prints the
# measured average but never increments FAIL.
#
# Env vars:
#   LOOM_GUARD_PERF_MAX_MS  - threshold in ms for the printed comparison
#                             (default 200).
#   LOOM_GUARD_PERF_STRICT  - set to 1/true to restore a hard gate: when the
#                             average meets/exceeds LOOM_GUARD_PERF_MAX_MS the
#                             suite fails (FAIL++/exit 1). Intended only for
#                             runs on a deliberately quiescent machine.
PERF_MAX_MS="${LOOM_GUARD_PERF_MAX_MS:-200}"
TOTAL=$((TOTAL + 1))
START=$(date +%s%N 2>/dev/null || python3 -c "import time; print(int(time.time()*1e9))")
for i in $(seq 1 10); do
    make_input "git status" "$REPO_ROOT" | "$GUARD" >/dev/null 2>&1
done
END=$(date +%s%N 2>/dev/null || python3 -c "import time; print(int(time.time()*1e9))")
ELAPSED_MS=$(( (END - START) / 1000000 ))
AVG_MS=$((ELAPSED_MS / 10))

if [[ $AVG_MS -lt $PERF_MAX_MS ]]; then
    PASS=$((PASS + 1))
    echo -e "  ${GREEN}PASS${NC}: Average execution time: ${AVG_MS}ms (< ${PERF_MAX_MS}ms threshold)"
elif [[ "${LOOM_GUARD_PERF_STRICT:-}" == "1" || "${LOOM_GUARD_PERF_STRICT:-}" == "true" ]]; then
    FAIL=$((FAIL + 1))
    echo -e "  ${RED}FAIL${NC}: Average execution time: ${AVG_MS}ms (>= ${PERF_MAX_MS}ms threshold, LOOM_GUARD_PERF_STRICT)"
else
    PASS=$((PASS + 1))
    echo -e "  ${YELLOW}INFO${NC}: Average execution time: ${AVG_MS}ms (>= ${PERF_MAX_MS}ms threshold; informational only, set LOOM_GUARD_PERF_STRICT=1 to gate)"
fi

echo ""

# =========================================================================
# Summary
# =========================================================================

print_summary
