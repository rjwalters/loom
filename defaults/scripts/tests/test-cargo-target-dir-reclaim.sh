#!/usr/bin/env bash
# test-cargo-target-dir-reclaim.sh — `worktree.sh remove` reclaims a REDIRECTED
# cargo target directory (issue #7239).
#
# Covers the removal-time half of lib/cargo-target-dir.sh:
#   1. A per-worktree target dir redirected OUTSIDE the worktree is reclaimed
#      when its worktree is removed.
#   2. A target dir another LIVE worktree also resolves to is left untouched
#      (the host-optimize single-shared-target-dir convention) — and IS
#      reclaimed once that last referencing worktree is gone too.
#   3. `--dry-run` lists the reclaimable dir with its size and deletes nothing.
#   4. The default in-worktree `target/` is never reported as a reclaim (it
#      disappears with the worktree for free).
#   5. A dir a live process is sitting in is left untouched.
#   6. Paths the pass must never delete (the repo itself, the primary
#      checkout's own `target/`) are refused.
#   7. Resolution parity with the standalone scripts/cargo-target-dir.sh, so
#      the library twin can never drift from the script it mirrors.
#
# Follows the throwaway-repo harness pattern in test-worktree-remove.sh: a bare
# origin remote + a working repo, with worktree.sh + its lib/ helpers copied
# into a temp tree, then the script driven directly. Hermetic: no forge, no
# network, no cargo invocation (every case pins CARGO_TARGET_DIR or has no
# manifest at all, both of which short-circuit before `cargo metadata`).

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SCRIPTS_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
REPO_ROOT="$(cd "$SCRIPTS_DIR/../.." && pwd)"

WORKTREE_SH="$SCRIPTS_DIR/worktree.sh"
LIB_SH="$SCRIPTS_DIR/lib/cargo-target-dir.sh"
STANDALONE_SH="$REPO_ROOT/scripts/cargo-target-dir.sh"

RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
NC='\033[0m'

TESTS_RUN=0
TESTS_PASSED=0
TESTS_FAILED=0

pass() { TESTS_RUN=$((TESTS_RUN + 1)); TESTS_PASSED=$((TESTS_PASSED + 1)); echo -e "  ${GREEN}PASS${NC}: $1"; }
fail() { TESTS_RUN=$((TESTS_RUN + 1)); TESTS_FAILED=$((TESTS_FAILED + 1)); echo -e "  ${RED}FAIL${NC}: $1"; }
skip() { echo -e "  ${YELLOW}SKIP${NC}: $1"; }

# --- Throwaway repo setup ---------------------------------------------------
TMP=$(mktemp -d /tmp/loom-target-reclaim.XXXXXX)
HOLDER_PID=""
cleanup() {
    [[ -n "$HOLDER_PID" ]] && kill "$HOLDER_PID" 2>/dev/null
    rm -rf "$TMP"
}
trap 'cleanup' EXIT

git init -q -b main "$TMP/origin.git" --bare
git init -q -b main "$TMP/repo"
cd "$TMP/repo" || exit 1
git config user.email t@t
git config user.name t
git commit --allow-empty -q -m init
git remote add origin "$TMP/origin.git"
git push -q origin main

mkdir -p .loom/scripts/lib
cp "$WORKTREE_SH" .loom/scripts/worktree.sh
if [[ -d "$SCRIPTS_DIR/lib" ]]; then
    cp -R "$SCRIPTS_DIR"/lib/* .loom/scripts/lib/ 2>/dev/null || true
fi
chmod +x .loom/scripts/worktree.sh

REPO="$TMP/repo"

make_worktree() {
    local n="$1"
    ( cd "$REPO" && ./.loom/scripts/worktree.sh "$n" ) >/dev/null 2>&1
}

# A target dir with something in it, so `du` reports a non-zero size.
make_target_dir() {
    local dir="$1"
    mkdir -p "$dir/debug"
    head -c 4096 /dev/zero > "$dir/debug/artifact.bin" 2>/dev/null || echo "artifact" > "$dir/debug/artifact.bin"
}

# --- Test 1: a redirected, exclusive target dir is reclaimed -----------------
echo "Test 1: removing a worktree reclaims its redirected (external) cargo target dir"
EXT1="$TMP/ext-target-201"
make_target_dir "$EXT1"
CARGO_TARGET_DIR="$EXT1" make_worktree 201
if [[ -d "$REPO/.loom/worktrees/issue-201" ]]; then
    pass "precondition: worktree issue-201 created"
else
    fail "precondition: worktree issue-201 was not created"
fi
if ( cd "$REPO" && CARGO_TARGET_DIR="$EXT1" ./.loom/scripts/worktree.sh remove 201 ) >/tmp/tr-out1.$$ 2>&1; then
    if [[ ! -d "$EXT1" ]]; then
        pass "external target dir reclaimed with the worktree"
    else
        fail "external target dir survived the removal (see /tmp/tr-out1.$$)"
    fi
    if grep -q "Reclaimed redirected cargo target dir" /tmp/tr-out1.$$; then
        pass "removal reports what it reclaimed"
    else
        fail "removal did not report the reclaim"
    fi
else
    fail "remove exited non-zero (see /tmp/tr-out1.$$)"
fi

# --- Test 2: a target dir shared with another live worktree is untouched -----
echo ""
echo "Test 2: a target dir another LIVE worktree resolves to is never deleted"
SHARED="$TMP/ext-target-shared"
make_target_dir "$SHARED"
CARGO_TARGET_DIR="$SHARED" make_worktree 202
CARGO_TARGET_DIR="$SHARED" make_worktree 203
# The harness repo has no Cargo.toml, so the primary checkout is (correctly)
# not counted as a referent. Give issue-203 one, so it is the ONE live
# worktree that shares this target dir — the exact host-optimize shape.
echo 'the sibling that is still building here' > "$REPO/.loom/worktrees/issue-203/Cargo.toml"
if ( cd "$REPO" && CARGO_TARGET_DIR="$SHARED" ./.loom/scripts/worktree.sh remove 202 ) >/tmp/tr-out2.$$ 2>&1; then
    if [[ -d "$SHARED" && -f "$SHARED/debug/artifact.bin" ]]; then
        pass "shared target dir (and its contents) survived removal of one sharer"
    else
        fail "shared target dir was deleted out from under a live worktree (see /tmp/tr-out2.$$)"
    fi
    if grep -q "still used by" /tmp/tr-out2.$$; then
        pass "removal explains that the target dir is still in use"
    else
        fail "removal did not explain why the target dir was kept"
    fi
else
    fail "remove 202 exited non-zero (see /tmp/tr-out2.$$)"
fi

echo ""
echo "Test 2b: the same dir IS reclaimed once the last referencing worktree goes"
if ( cd "$REPO" && CARGO_TARGET_DIR="$SHARED" ./.loom/scripts/worktree.sh remove 203 --force ) >/tmp/tr-out2b.$$ 2>&1; then
    if [[ ! -d "$SHARED" ]]; then
        pass "target dir reclaimed after the last sharer was removed"
    else
        fail "target dir survived after every referencing worktree was removed (see /tmp/tr-out2b.$$)"
    fi
else
    fail "remove 203 --force exited non-zero (see /tmp/tr-out2b.$$)"
fi

# --- Test 3: --dry-run reports with a size and deletes nothing ---------------
echo ""
echo "Test 3: --dry-run lists the reclaimable target dir with its size, deletes nothing"
EXT3="$TMP/ext-target-204"
make_target_dir "$EXT3"
CARGO_TARGET_DIR="$EXT3" make_worktree 204
if ( cd "$REPO" && CARGO_TARGET_DIR="$EXT3" ./.loom/scripts/worktree.sh remove 204 --dry-run ) >/tmp/tr-out3.$$ 2>&1; then
    if [[ -d "$EXT3" ]]; then
        pass "--dry-run left the target dir on disk"
    else
        fail "--dry-run DELETED the target dir"
    fi
    if [[ -d "$REPO/.loom/worktrees/issue-204" ]]; then
        pass "--dry-run left the worktree on disk"
    else
        fail "--dry-run removed the worktree"
    fi
    if grep -q "Would reclaim redirected cargo target dir: $EXT3" /tmp/tr-out3.$$; then
        pass "--dry-run names the reclaimable dir"
    else
        fail "--dry-run did not name the reclaimable dir (see /tmp/tr-out3.$$)"
    fi
    if grep -Eq "Would reclaim redirected cargo target dir: .*\([0-9.]+[BKMGT]?\)" /tmp/tr-out3.$$; then
        pass "--dry-run reports a size for the reclaimable dir"
    else
        fail "--dry-run did not report a size (see /tmp/tr-out3.$$)"
    fi
else
    fail "remove --dry-run exited non-zero (see /tmp/tr-out3.$$)"
fi
# --json must still be a single parseable document, now carrying the plan.
if ( cd "$REPO" && CARGO_TARGET_DIR="$EXT3" ./.loom/scripts/worktree.sh remove 204 --dry-run --json ) >/tmp/tr-out3b.$$ 2>/dev/null; then
    if [[ "$(grep -c . /tmp/tr-out3b.$$)" == "1" ]] && grep -q '"dryRun": true' /tmp/tr-out3b.$$ && \
       grep -q "\"targetDirStatus\": \"would-reclaim\"" /tmp/tr-out3b.$$; then
        pass "--dry-run --json emits one document reporting the target-dir plan"
    else
        fail "--dry-run --json output unexpected (see /tmp/tr-out3b.$$)"
    fi
else
    fail "remove --dry-run --json exited non-zero (see /tmp/tr-out3b.$$)"
fi

# --- Test 4: the default in-worktree target/ is not a "reclaim" --------------
echo ""
echo "Test 4: an un-redirected in-worktree target/ is not reported as a reclaim"
make_worktree 205
make_target_dir "$REPO/.loom/worktrees/issue-205/target"
if ( cd "$REPO" && env -u CARGO_TARGET_DIR ./.loom/scripts/worktree.sh remove 205 --force ) >/tmp/tr-out4.$$ 2>&1; then
    if ! grep -q "redirected cargo target dir" /tmp/tr-out4.$$; then
        pass "no redirected-target-dir reporting for the default layout"
    else
        fail "reported a redirect for a plain in-worktree target/ (see /tmp/tr-out4.$$)"
    fi
    if [[ ! -d "$REPO/.loom/worktrees/issue-205" ]]; then
        pass "worktree (and its in-tree target/) removed as before"
    else
        fail "worktree was not removed"
    fi
else
    fail "remove 205 exited non-zero (see /tmp/tr-out4.$$)"
fi

# --- Test 5: a dir a live process is using is left alone --------------------
echo ""
echo "Test 5: a target dir with a live process inside it is never deleted"
EXT5="$TMP/ext-target-206"
make_target_dir "$EXT5"
CARGO_TARGET_DIR="$EXT5" make_worktree 206
( cd "$EXT5" && exec sleep 120 ) &
HOLDER_PID=$!
sleep 0.3
if ( cd "$REPO" && CARGO_TARGET_DIR="$EXT5" ./.loom/scripts/worktree.sh remove 206 ) >/tmp/tr-out5.$$ 2>&1; then
    if [[ -d "$EXT5" ]]; then
        pass "target dir backing a live process survived"
    else
        fail "target dir was deleted while a process was using it (see /tmp/tr-out5.$$)"
    fi
    if grep -q "still using it" /tmp/tr-out5.$$; then
        pass "removal names the live-process hold as the reason"
    else
        # /proc and lsof are both fail-open by design; on a host where neither
        # can answer this gate cannot fire, so report rather than hard-fail.
        if [[ -d /proc ]] || command -v lsof >/dev/null 2>&1; then
            fail "no live-process explanation emitted (see /tmp/tr-out5.$$)"
        else
            skip "neither /proc nor lsof available — process gate cannot be exercised"
        fi
    fi
else
    fail "remove 206 exited non-zero (see /tmp/tr-out5.$$)"
fi
kill "$HOLDER_PID" 2>/dev/null
HOLDER_PID=""

# --- Test 6: never-delete paths are refused ---------------------------------
echo ""
echo "Test 6: the primary checkout's own target/ is refused, not reclaimed"
make_target_dir "$REPO/target"
CARGO_TARGET_DIR="$REPO/target" make_worktree 207
if ( cd "$REPO" && CARGO_TARGET_DIR="$REPO/target" ./.loom/scripts/worktree.sh remove 207 ) >/tmp/tr-out6.$$ 2>&1; then
    if [[ -d "$REPO/target" ]]; then
        pass "the primary checkout's own target/ was not deleted"
    else
        fail "the primary checkout's own target/ was deleted (see /tmp/tr-out6.$$)"
    fi
    if grep -q "Refusing to reclaim" /tmp/tr-out6.$$; then
        pass "refusal is explained"
    else
        fail "no refusal explanation emitted (see /tmp/tr-out6.$$)"
    fi
else
    fail "remove 207 exited non-zero (see /tmp/tr-out6.$$)"
fi

echo ""
echo "Test 6b: a target dir containing the repository itself is refused"
CARGO_TARGET_DIR="$TMP" make_worktree 208
if ( cd "$REPO" && CARGO_TARGET_DIR="$TMP" ./.loom/scripts/worktree.sh remove 208 ) >/tmp/tr-out6b.$$ 2>&1; then
    if [[ -d "$REPO" ]]; then
        pass "an ancestor of the repository was not deleted"
    else
        fail "CATASTROPHIC: the repository's parent was deleted"
    fi
    if grep -q "Refusing to reclaim" /tmp/tr-out6b.$$; then
        pass "ancestor refusal is explained"
    else
        fail "no refusal explanation for the ancestor case (see /tmp/tr-out6b.$$)"
    fi
else
    fail "remove 208 exited non-zero (see /tmp/tr-out6b.$$)"
fi

# --- Test 7: resolver parity with scripts/cargo-target-dir.sh ---------------
echo ""
echo "Test 7: the library resolver agrees with scripts/cargo-target-dir.sh"
if [[ ! -x "$STANDALONE_SH" ]]; then
    skip "scripts/cargo-target-dir.sh not present (consumer-repo checkout)"
else
    # shellcheck source=../lib/cargo-target-dir.sh
    source "$LIB_SH"
    PARITY_ROOT="$TMP/parity-root"
    mkdir -p "$PARITY_ROOT"
    parity_case() {
        local label="$1" env_value="$2"
        local lib_says script_says
        if [[ -n "$env_value" ]]; then
            lib_says="$(CARGO_TARGET_DIR="$env_value" loom_resolve_cargo_target_dir "$PARITY_ROOT")"
            script_says="$(CARGO_TARGET_DIR="$env_value" "$STANDALONE_SH" "$PARITY_ROOT" 2>/dev/null)"
        else
            lib_says="$(env -u CARGO_TARGET_DIR bash -c "source '$LIB_SH'; loom_resolve_cargo_target_dir '$PARITY_ROOT'")"
            script_says="$(env -u CARGO_TARGET_DIR "$STANDALONE_SH" "$PARITY_ROOT" 2>/dev/null)"
        fi
        if [[ "$lib_says" == "$script_says" && -n "$lib_says" ]]; then
            pass "parity ($label): both resolve to '$lib_says'"
        else
            fail "parity ($label): lib says '$lib_says', script says '$script_says'"
        fi
    }
    parity_case "absolute CARGO_TARGET_DIR" "$TMP/abs-target"
    parity_case "relative CARGO_TARGET_DIR" "rel-target"
    # No manifest under PARITY_ROOT, so both fall through to `<root>/target`
    # (cargo metadata fails there) without a network call.
    parity_case "no redirect configured" ""
fi

# --- Test 8: merge-pr.sh's post-merge cleanup reclaims too -------------------
# Post-merge cleanup is the removal path most worktrees actually take, so the
# leak lives here more than anywhere else. Drives the REAL `_remove_loom_worktree`
# body extracted from the live source (the no-drift pattern from
# test-merge-pr-dirty-worktree-guard.sh), not a reimplementation.
echo ""
echo "Test 8: merge-pr.sh's post-merge worktree cleanup reclaims a redirected target dir"
MERGE_PR="$SCRIPTS_DIR/merge-pr.sh"
if [[ ! -f "$MERGE_PR" ]]; then
    skip "merge-pr.sh not found"
else
    extract_fn() {
        local name="$1" file="$2"
        awk -v fn="$name" '
          $0 ~ "^"fn"\\(\\) \\{" { grab=1 }
          grab { print }
          grab && /^}/ { exit }
        ' "$file"
    }
    info()    { echo "INFO: $*"; }
    warning() { echo "WARN: $*"; }
    success() { echo "OK: $*"; }
    error()   { echo "ERROR: $*" >&2; return 1; }
    loom_record_worktree_removal() { :; }

    # shellcheck source=../lib/cargo-target-dir.sh
    source "$LIB_SH"
    eval "$(extract_fn _primary_worktree_path "$MERGE_PR")"
    eval "$(extract_fn _worktree_branch_for "$MERGE_PR")"
    eval "$(extract_fn _worktree_branch_fully_captured "$MERGE_PR")"
    eval "$(extract_fn _maybe_delete_local_branch "$MERGE_PR")"
    eval "$(extract_fn _mp_report_target_dir_reclaim "$MERGE_PR")"
    eval "$(extract_fn _remove_loom_worktree "$MERGE_PR")"

    MP_REPO="$TMP/mp-repo"
    git init -q -b main "$MP_REPO"
    git -C "$MP_REPO" config user.email t@t
    git -C "$MP_REPO" config user.name t
    echo hello > "$MP_REPO/README.md"
    git -C "$MP_REPO" add -A
    git -C "$MP_REPO" commit -q -m init
    REPO_ROOT="$MP_REPO"

    MP_WT="$TMP/mp-wt-301"
    git -C "$MP_REPO" worktree add -q -b feature/issue-301 "$MP_WT" >/dev/null 2>&1
    touch "$MP_WT/.loom-managed"
    MP_EXT="$TMP/ext-target-301"
    make_target_dir "$MP_EXT"

    mp_out="$(CARGO_TARGET_DIR="$MP_EXT" _remove_loom_worktree "$MP_WT" 2>&1)"
    if [[ ! -d "$MP_WT" ]]; then
        pass "merge-pr cleanup removed the worktree"
    else
        fail "merge-pr cleanup did not remove the worktree: $mp_out"
    fi
    if [[ ! -d "$MP_EXT" ]]; then
        pass "merge-pr cleanup reclaimed the redirected target dir"
    else
        fail "merge-pr cleanup left the redirected target dir behind: $mp_out"
    fi
    if [[ "$mp_out" == *"Reclaimed redirected cargo target dir"* ]]; then
        pass "merge-pr cleanup reports the reclaim"
    else
        fail "merge-pr cleanup did not report the reclaim: $mp_out"
    fi

    echo ""
    echo "Test 8b: merge-pr.sh's cleanup never deletes a target dir a live worktree shares"
    MP_SHARED="$TMP/ext-target-shared-302"
    make_target_dir "$MP_SHARED"
    MP_WT2="$TMP/mp-wt-302"
    MP_WT3="$TMP/mp-wt-303"
    git -C "$MP_REPO" worktree add -q -b feature/issue-302 "$MP_WT2" >/dev/null 2>&1
    git -C "$MP_REPO" worktree add -q -b feature/issue-303 "$MP_WT3" >/dev/null 2>&1
    touch "$MP_WT2/.loom-managed" "$MP_WT3/.loom-managed"
    echo 'the sibling still building here' > "$MP_WT3/Cargo.toml"
    mp_out2="$(CARGO_TARGET_DIR="$MP_SHARED" _remove_loom_worktree "$MP_WT2" 2>&1)"
    if [[ -f "$MP_SHARED/debug/artifact.bin" ]]; then
        pass "shared target dir survived merge-pr cleanup of one sharer"
    else
        fail "merge-pr cleanup deleted a shared target dir: $mp_out2"
    fi
    if [[ "$mp_out2" == *"still used by"* ]]; then
        pass "merge-pr cleanup explains why it kept the dir"
    else
        fail "merge-pr cleanup gave no explanation: $mp_out2"
    fi
fi

# --- Summary ----------------------------------------------------------------
echo ""
echo "Tests run: $TESTS_RUN, Passed: $TESTS_PASSED, Failed: $TESTS_FAILED"
[[ $TESTS_FAILED -eq 0 ]] || exit 1
