#!/usr/bin/env bash
# Test suite for .loom/hooks/post-worktree.sh's main-workspace-binary lookup
# (issue #6013).
#
# Usage: ./tests/hooks/test-post-worktree-target-dir.sh
#
# .loom/hooks/post-worktree.sh copies a pre-built loom-daemon binary from the
# main workspace into a freshly created worktree instead of rebuilding it
# (the fix for #2291's cargo-lock rebuild-storm contention). It used to look
# for that binary at a hardcoded `<main-workspace>/target/release/loom-daemon`
# path, which is wrong on a host whose `~/.cargo/config.toml` sets
# `build.target-dir` (or the `CARGO_TARGET_DIR` env var) to redirect Cargo's
# build output elsewhere (issue #5922) -- on such a host the hardcoded path
# was *always* "missing", so every worktree fell through to a full
# `cargo build --release -p loom-daemon`, reintroducing #2291.
#
# This suite drives the real hook script (there is no `defaults/hooks/`
# counterpart to keep in sync -- this hook is a repo-local dogfooding
# artifact, not part of the template shipped to consumer repos) against
# throwaway `git worktree add` fixtures, covering:
#   1. Default layout (no redirect) -- pre-existing #2291 behavior.
#   2. An absolute CARGO_TARGET_DIR shared verbatim across the main workspace
#      and every worktree (the exact scenario in #6013 -- e.g. a target dir
#      redirected to a separate disk).
#   3. A relative CARGO_TARGET_DIR, which resolves to a different directory
#      per workspace root (main vs. worktree), exercising the actual copy.
#   4. Fallback when scripts/cargo-target-dir.sh itself is missing (partial
#      checkout) -- must degrade to the exact pre-#6013 hardcoded assumption.
#
# No real `cargo build` is ever invoked: every case pre-seeds a fake
# executable "binary" at the resolved location and asserts the hook's fast
# copy path is taken, not the rebuild fallback.
#
# Exit code 0 = all tests pass, 1 = failures detected.

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
HOOK="$REPO_ROOT/.loom/hooks/post-worktree.sh"
CARGO_TARGET_DIR_SCRIPT_SRC="$REPO_ROOT/scripts/cargo-target-dir.sh"

RED='\033[0;31m'
GREEN='\033[0;32m'
NC='\033[0m'

PASS=0
FAIL=0
TOTAL=0

pass() { TOTAL=$((TOTAL + 1)); PASS=$((PASS + 1)); echo -e "  ${GREEN}PASS${NC}: $1"; }
fail() { TOTAL=$((TOTAL + 1)); FAIL=$((FAIL + 1)); echo -e "  ${RED}FAIL${NC}: $1"; }

if [[ ! -x "$HOOK" ]]; then
    echo -e "${RED}FATAL${NC}: hook not found or not executable at $HOOK"
    exit 1
fi

if [[ ! -x "$CARGO_TARGET_DIR_SCRIPT_SRC" ]]; then
    echo -e "${RED}FATAL${NC}: scripts/cargo-target-dir.sh not found or not executable"
    exit 1
fi

WORKDIR="$(mktemp -d)"
trap 'rm -rf "$WORKDIR"' EXIT

# Isolate every cargo invocation from the HOST's own ~/.cargo/config.toml --
# a host that itself has a build.target-dir redirect (like the one #6013
# reports) would otherwise make the "default layout" case non-deterministic.
export CARGO_HOME="$WORKDIR/cargo-home"
mkdir -p "$CARGO_HOME"

# Build a fake loom-daemon binary at $1 with distinguishable content at $2.
make_fake_bin() {
    local path="$1" marker="$2"
    mkdir -p "$(dirname "$path")"
    cat > "$path" <<EOF
#!/usr/bin/env bash
echo "$marker"
EOF
    chmod +x "$path"
}

# Set up a minimal single-package "loom-daemon" crate at $1, with
# scripts/cargo-target-dir.sh vendored alongside it (unless SKIP_HELPER=1) and
# defaults/scripts/lib/cargo-target-dir.sh vendored too (unless SKIP_LIB=1 --
# the hook sources the lib for the #8458 per-worktree helpers and falls back to
# degraded twins without it; both paths are exercised below).
make_main_workspace() {
    local root="$1" skip_helper="${2:-0}" skip_lib="${3:-${2:-0}}"
    if [[ "$skip_lib" != "1" ]]; then
        mkdir -p "$root/defaults/scripts/lib"
        cp "$REPO_ROOT/defaults/scripts/lib/cargo-target-dir.sh" "$root/defaults/scripts/lib/cargo-target-dir.sh"
    fi
    mkdir -p "$root/src"
    cat > "$root/Cargo.toml" <<'EOF'
[package]
name = "loom-daemon"
version = "0.1.0"
edition = "2021"
EOF
    echo 'fn main() {}' > "$root/src/main.rs"
    if [[ "$skip_helper" != "1" ]]; then
        mkdir -p "$root/scripts"
        cp "$CARGO_TARGET_DIR_SCRIPT_SRC" "$root/scripts/cargo-target-dir.sh"
        chmod +x "$root/scripts/cargo-target-dir.sh"
    fi
    git -C "$root" init -q
    git -C "$root" config user.email "test@example.com"
    git -C "$root" config user.name "Test"
    git -C "$root" add -A
    git -C "$root" commit -q -m "initial commit"
}

# Create a worktree of $1 (main workspace) at $2, on a throwaway branch.
add_worktree() {
    local main="$1" worktree="$2" branch="$3"
    git -C "$main" worktree add -q -b "$branch" "$worktree" >/dev/null 2>&1
}

# Run the hook exactly the way worktree.sh invokes it: cd into the worktree
# first, pass the absolute worktree path as $1.
run_hook() {
    local worktree="$1"
    ( cd "$worktree" && "$HOOK" "$worktree" "test-branch" "1" )
}

# ==========================================================================
# Test 1: default layout (no redirect) -- pre-existing #2291 behavior
# ==========================================================================
MAIN1="$WORKDIR/main1"
WT1="$WORKDIR/wt1"
make_main_workspace "$MAIN1"
add_worktree "$MAIN1" "$WT1" "t1"
make_fake_bin "$MAIN1/target/release/loom-daemon" "main1-binary"

OUT1="$( unset CARGO_TARGET_DIR; run_hook "$WT1" )"
RC1=$?

if [[ $RC1 -eq 0 ]]; then
    pass "default layout: hook exits 0"
else
    fail "default layout: hook exited $RC1"
fi

if [[ -x "$WT1/target/release/loom-daemon" ]]; then
    pass "default layout: binary copied to worktree's target/release/"
else
    fail "default layout: no binary at $WT1/target/release/loom-daemon"
fi

if [[ "$("$WT1/target/release/loom-daemon" 2>/dev/null)" == "main1-binary" ]]; then
    pass "default layout: copied binary matches the main workspace's"
else
    fail "default layout: copied binary content mismatch"
fi

if [[ "$OUT1" == *"copied from main workspace"* ]]; then
    pass "default layout: hook reports the fast-path copy, not a rebuild"
else
    fail "default layout: hook output did not mention the copy: $OUT1"
fi

# ==========================================================================
# Test 2: absolute CARGO_TARGET_DIR shared verbatim across main + worktree
# (the exact scenario reported in #6013)
# ==========================================================================
MAIN2="$WORKDIR/main2"
WT2="$WORKDIR/wt2"
REDIR2="$WORKDIR/redirected-shared-2"
make_main_workspace "$MAIN2"
add_worktree "$MAIN2" "$WT2" "t2"
make_fake_bin "$REDIR2/release/loom-daemon" "main2-binary"

OUT2="$( CARGO_TARGET_DIR="$REDIR2" run_hook "$WT2" )"
RC2=$?

if [[ $RC2 -eq 0 ]]; then
    pass "redirected (absolute, shared): hook exits 0"
else
    fail "redirected (absolute, shared): hook exited $RC2"
fi

# An absolute CARGO_TARGET_DIR resolves to the SAME directory regardless of
# workspace root, so the worktree's own resolved binary already exists --
# no copy needed, and (critically) no fall-through to a full rebuild.
if [[ "$OUT2" == *"already exists"* || "$OUT2" == *"copied from main workspace"* ]]; then
    pass "redirected (absolute, shared): resolved via the redirected dir, no rebuild"
else
    fail "redirected (absolute, shared): fell through to a rebuild: $OUT2"
fi

if [[ ! -e "$WT2/target/release/loom-daemon" ]]; then
    pass "redirected (absolute, shared): the old hardcoded <root>/target path was never touched"
else
    fail "redirected (absolute, shared): unexpectedly wrote to the hardcoded default path"
fi

# ==========================================================================
# Test 3: relative CARGO_TARGET_DIR -- resolves per-workspace-root, so main
# and worktree land in genuinely different directories, exercising the copy.
# ==========================================================================
MAIN3="$WORKDIR/main3"
WT3="$WORKDIR/wt3"
make_main_workspace "$MAIN3"
add_worktree "$MAIN3" "$WT3" "t3"
make_fake_bin "$MAIN3/cargo-out/release/loom-daemon" "main3-binary"

OUT3="$( CARGO_TARGET_DIR="cargo-out" run_hook "$WT3" )"
RC3=$?

if [[ $RC3 -eq 0 ]]; then
    pass "redirected (relative, per-root): hook exits 0"
else
    fail "redirected (relative, per-root): hook exited $RC3: $OUT3"
fi

if [[ -x "$WT3/cargo-out/release/loom-daemon" ]]; then
    pass "redirected (relative, per-root): binary copied to the worktree's own resolved target dir"
else
    fail "redirected (relative, per-root): no binary at $WT3/cargo-out/release/loom-daemon"
fi

if [[ "$("$WT3/cargo-out/release/loom-daemon" 2>/dev/null)" == "main3-binary" ]]; then
    pass "redirected (relative, per-root): copied binary matches the main workspace's"
else
    fail "redirected (relative, per-root): copied binary content mismatch"
fi

if [[ ! -e "$WT3/target/release/loom-daemon" ]]; then
    pass "redirected (relative, per-root): the old hardcoded <root>/target path was never touched"
else
    fail "redirected (relative, per-root): unexpectedly wrote to the hardcoded default path"
fi

# ==========================================================================
# Test 4: scripts/cargo-target-dir.sh missing (partial checkout) -- must
# degrade to the exact pre-#6013 hardcoded <root>/target assumption.
# ==========================================================================
MAIN4="$WORKDIR/main4"
WT4="$WORKDIR/wt4"
make_main_workspace "$MAIN4" 1 1  # no scripts/cargo-target-dir.sh, no lib/
add_worktree "$MAIN4" "$WT4" "t4"
make_fake_bin "$MAIN4/target/release/loom-daemon" "main4-binary"

OUT4="$( CARGO_TARGET_DIR="$WORKDIR/should-be-ignored-4" run_hook "$WT4" )"
RC4=$?

if [[ $RC4 -eq 0 ]]; then
    pass "no helper script: hook exits 0"
else
    fail "no helper script: hook exited $RC4: $OUT4"
fi

if [[ -x "$WT4/target/release/loom-daemon" ]]; then
    pass "no helper script: falls back to hardcoded <root>/target and still copies"
else
    fail "no helper script: no binary at $WT4/target/release/loom-daemon (fallback broken)"
fi

# ==========================================================================
# Test 5: the per-worktree target-dir scheme (issue #8458) -- THE #6013/#6014
# REGRESSION TEST for it.
#
# Under that scheme the SOURCE and the DESTINATION are resolved by different
# rules, and the sweep's ambient CARGO_TARGET_DIR names the DESTINATION:
#
#   CARGO_TARGET_DIR=<shared>/wt/issue-N       (exported by spawn-claude.sh)
#   worktree's own target dir  = that same path (the marker / the env var)
#   main workspace's target dir = <shared>      (~/.cargo/config.toml)
#
# Resolving the MAIN workspace through that ambient value -- which is what env
# beats config means -- reports the pre-built binary "missing" on EVERY worktree
# creation and falls through to a full `cargo build --release`. That is exactly
# #6013's rebuild storm, and it is the single most likely way #8458 goes wrong.
# So: assert the copy happens, assert it lands in the per-worktree dir, and
# assert the rebuild path is NOT taken.
#
# 5a drives it via LOOM_WORKTREE_CARGO_TARGET_DIR (what worktree.sh exports);
# 5b drives it via the `.loom-cargo-target-dir` marker alone (the hook invoked
# without worktree.sh in the loop).
# ==========================================================================
MAIN5="$WORKDIR/main5"
WT5="$WORKDIR/wt5"
SHARED5="$WORKDIR/shared-target-5"
make_main_workspace "$MAIN5"
add_worktree "$MAIN5" "$WT5" "t5"
# The host redirects EVERY checkout to one shared root (the #8453 shape), and
# the pre-built binary lives there, where a build in the main workspace put it.
mkdir -p "$CARGO_HOME"
printf '[build]\ntarget-dir = "%s"\n' "$SHARED5" > "$CARGO_HOME/config.toml"
make_fake_bin "$SHARED5/release/loom-daemon" "main5-binary"
PERWT5="$SHARED5/wt/wt5"

OUT5="$( LOOM_WORKTREE_CARGO_TARGET_DIR="$PERWT5" CARGO_TARGET_DIR="$PERWT5" run_hook "$WT5" )"
RC5=$?

if [[ $RC5 -eq 0 ]]; then
    pass "per-worktree (#8458): hook exits 0"
else
    fail "per-worktree (#8458): hook exited $RC5: $OUT5"
fi

if [[ "$OUT5" == *"copied from main workspace"* ]]; then
    pass "per-worktree (#8458): FAST PATH taken -- copied, not rebuilt (#6013/#6014)"
else
    fail "per-worktree (#8458): fell through to a rebuild -- #6013/#6014 REGRESSION: $OUT5"
fi

if [[ -x "$PERWT5/release/loom-daemon" ]]; then
    pass "per-worktree (#8458): binary copied INTO the per-worktree target dir"
else
    fail "per-worktree (#8458): no binary at $PERWT5/release/loom-daemon"
fi

if [[ "$("$PERWT5/release/loom-daemon" 2>/dev/null)" == "main5-binary" ]]; then
    pass "per-worktree (#8458): the copied binary is the main workspace's"
else
    fail "per-worktree (#8458): copied binary content mismatch"
fi

if [[ ! -e "$WT5/target/release/loom-daemon" ]]; then
    pass "per-worktree (#8458): the hardcoded <root>/target path was never touched"
else
    fail "per-worktree (#8458): unexpectedly wrote to the hardcoded default path"
fi

# 5b: the marker alone, with no LOOM_WORKTREE_CARGO_TARGET_DIR in the env.
MAIN5B="$WORKDIR/main5b"
WT5B="$WORKDIR/wt5b"
make_main_workspace "$MAIN5B"
add_worktree "$MAIN5B" "$WT5B" "t5b"
make_fake_bin "$SHARED5/release/loom-daemon" "main5b-binary"
PERWT5B="$SHARED5/wt/wt5b"
printf '%s\n' "$PERWT5B" > "$WT5B/.loom-cargo-target-dir"

OUT5B="$( CARGO_TARGET_DIR="$PERWT5B" run_hook "$WT5B" )"

if [[ "$OUT5B" == *"copied from main workspace"* && -x "$PERWT5B/release/loom-daemon" ]]; then
    pass "per-worktree (#8458): the marker alone drives the fast path too"
else
    fail "per-worktree (#8458): marker-only path did not take the fast path: $OUT5B"
fi

# 5c: a genuinely SESSION-GLOBAL CARGO_TARGET_DIR (no per-worktree shape) must
# still be honored for the main workspace -- stripping it unconditionally would
# be its own #6013 regression, for the operator who redirects all builds.
MAIN5C="$WORKDIR/main5c"
WT5C="$WORKDIR/wt5c"
GLOBAL5C="$WORKDIR/session-global-5c"
make_main_workspace "$MAIN5C"
add_worktree "$MAIN5C" "$WT5C" "t5c"
make_fake_bin "$GLOBAL5C/release/loom-daemon" "main5c-binary"

OUT5C="$( CARGO_TARGET_DIR="$GLOBAL5C" run_hook "$WT5C" )"

if [[ "$OUT5C" == *"already exists"* || "$OUT5C" == *"copied from main workspace"* ]]; then
    pass "session-global CARGO_TARGET_DIR: still honored for the main workspace, no rebuild"
else
    fail "session-global CARGO_TARGET_DIR: fell through to a rebuild: $OUT5C"
fi

# 5d: lib/cargo-target-dir.sh ABSENT (partial checkout / pre-#8458 install) but
# the ambient per-worktree CARGO_TARGET_DIR still present. The hook's degraded
# twins must keep the fast path working -- skipping the strip here would
# reintroduce #6013/#6014 for exactly the installs least able to notice.
MAIN5D="$WORKDIR/main5d"
WT5D="$WORKDIR/wt5d"
make_main_workspace "$MAIN5D" 0 1   # standalone helper yes, lib no
add_worktree "$MAIN5D" "$WT5D" "t5d"
make_fake_bin "$SHARED5/release/loom-daemon" "main5d-binary"
PERWT5D="$SHARED5/wt/wt5d"

OUT5D="$( CARGO_TARGET_DIR="$PERWT5D" run_hook "$WT5D" )"

if [[ "$OUT5D" == *"copied from main workspace"* && -x "$PERWT5D/release/loom-daemon" ]]; then
    pass "per-worktree (#8458) without the lib: degraded twins keep the fast path"
else
    fail "per-worktree (#8458) without the lib: fell through to a rebuild: $OUT5D"
fi

rm -f "$CARGO_HOME/config.toml"

# ==========================================================================
# Summary
# ==========================================================================
echo ""
echo "========================================="
echo -e "  Total:  $TOTAL"
echo -e "  ${GREEN}Passed${NC}: $PASS"
echo -e "  ${RED}Failed${NC}: $FAIL"
echo "========================================="

if [[ $FAIL -gt 0 ]]; then
    echo -e "\n${RED}TESTS FAILED${NC}"
    exit 1
else
    echo -e "\n${GREEN}ALL TESTS PASSED${NC}"
    exit 0
fi
