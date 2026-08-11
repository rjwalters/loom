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
# scripts/cargo-target-dir.sh vendored alongside it (unless SKIP_HELPER=1).
make_main_workspace() {
    local root="$1" skip_helper="${2:-0}"
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
make_main_workspace "$MAIN4" 1  # skip_helper=1: no scripts/cargo-target-dir.sh
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
