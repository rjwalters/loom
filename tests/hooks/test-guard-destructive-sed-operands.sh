#!/usr/bin/env bash
# Test suite for defaults/hooks/guard-destructive-generic.sh — `sed -i` script
# options vs. file operands under worktree-write-confinement (#11074).
#
# Sibling of test-guard-destructive-write-confinement.sh, which owns the rest
# of the #4178 Bash-tool write-confinement surface; this one is split out per
# .loom/docs/file-size-policy.md (that suite is over the size threshold and is
# frozen, so new coverage goes in a new sibling module rather than growing it).
# Shared fixtures, assertions and the GUARD path live in
# tests/hooks/lib/guard-destructive-harness.sh exactly as they do there.
#
# Usage: ./tests/hooks/test-guard-destructive-sed-operands.sh

set -euo pipefail
# shellcheck source=tests/hooks/lib/guard-destructive-harness.sh
. "$(cd "$(dirname "$0")" && pwd)/lib/guard-destructive-harness.sh"

echo -e "${YELLOW}--- sed -i script options vs. file operands (#11074) ---${NC}"
# =========================================================================
# #11074 — `-e`/`-f`/`--expression`/`--file` ARGUMENTS are script text or a
# script file, never file operands, and their presence means there is NO
# positional script: every remaining non-option token is a write target.
#
# Both directions are pinned:
#   * the option argument must not become a phantom target (ALLOW on /tmp)
#   * the first real file operand must not be swallowed as a positional
#     script (DENY on a checkout target) -- including the ATTACHED forms
#     (`-es/a/b/`, `-nes/a/b/`, `--expression=...`, `--file=...`), which carry
#     the script in the same token and so consume nothing after them.
# =========================================================================

WT_REPO=$(make_wt_repo)
EXT="/tmp/loom-test-$$-11074.txt"

# --- Separate-argument forms ---------------------------------------------
assert_allow "sed-operands (#11074): sed -i with multiple -e and an external /tmp target allows" \
    "sed -i -e 's/a/b/' -e 's/c/d/' $EXT" "$WT_REPO"
assert_deny "sed-operands (#11074): sed -i with multiple -e and a main-checkout target still denies" \
    "sed -i -e 's/a/b/' -e 's/c/d/' $WT_REPO/f" "$WT_REPO"
assert_deny "sed-operands (#11074): sed -i with multiple -e and a checkout-relative target still denies" \
    "sed -i -e 's/a/b/' -e 's/c/d/' README.md" "$WT_REPO"
assert_allow "sed-operands (#11074): sed -i -f script.sed with external /tmp target allows" \
    "sed -i -f script.sed $EXT" "$WT_REPO"
assert_deny "sed-operands (#11074): sed -i -f script.sed with checkout-relative target denies" \
    "sed -i -f script.sed README.md" "$WT_REPO"
assert_allow "sed-operands (#11074): sed -i --expression/--file space-separated forms with external target allow" \
    "sed -i --expression 's/a/b/' --expression 's/c/d/' --file s.sed $EXT" "$WT_REPO"
assert_deny "sed-operands (#11074): sed -i --expression space-separated with checkout target denies" \
    "sed -i --expression 's/a/b/' --expression 's/c/d/' README.md" "$WT_REPO"
assert_allow "sed-operands (#11074): sed -i -e single script with external target allows" \
    "sed -i -e 's/a/b/' $EXT" "$WT_REPO"
assert_deny "sed-operands (#11074): sed -i single positional script with checkout target still denies" \
    "sed -i 's/a/b/' README.md" "$WT_REPO"

# --- Attached forms: script in-token, nothing after it consumed -----------
assert_allow "sed-operands (#11074): attached -es/a/b/ + --expression= with external target allow" \
    "sed -i -es/a/b/ --expression=s/c/d/ $EXT" "$WT_REPO"
assert_allow "sed-operands (#11074): attached -nes/a/b/ + --file= with external target allow" \
    "sed -i -nes/a/b/ --file=s.sed $EXT" "$WT_REPO"
assert_deny "sed-operands (#11074): attached -es/a/b/ with checkout-relative target denies" \
    "sed -i -es/a/b/ README.md" "$WT_REPO"
assert_deny "sed-operands (#11074): attached -nes/a/b/ with checkout-relative target denies" \
    "sed -i -nes/a/b/ README.md" "$WT_REPO"
assert_deny "sed-operands (#11074): attached --file=s.sed with checkout-relative target denies" \
    "sed -i --file=s.sed README.md" "$WT_REPO"
assert_deny "sed-operands (#11074): attached --expression=s/a/b/ with checkout-relative target denies" \
    "sed -i --expression=s/a/b/ README.md" "$WT_REPO"
assert_deny "sed-operands (#11074): attached -es/a/b/ with absolute main-checkout target denies" \
    "sed -i -es/a/b/ $WT_REPO/f" "$WT_REPO"
assert_deny "sed-operands (#11074): attached -nes/a/b/ with absolute main-checkout target denies" \
    "sed -i -nes/a/b/ $WT_REPO/f" "$WT_REPO"
assert_deny "sed-operands (#11074): attached --file=s.sed with absolute main-checkout target denies" \
    "sed -i --file=s.sed $WT_REPO/f" "$WT_REPO"
assert_deny "sed-operands (#11074): attached --expression=s/a/b/ with absolute main-checkout target denies" \
    "sed -i --expression=s/a/b/ $WT_REPO/f" "$WT_REPO"
# `-ie` stays GNU suffix `e` + positional script: the checkout target denies.
assert_deny "sed-operands (#11074): -ie (suffix e) + positional script with checkout target denies" \
    "sed -ie 's/a/b/' README.md" "$WT_REPO"

# --- BSD `-i ''` suffix token ------------------------------------------------
assert_allow "sed-operands (#11074): BSD sed -i '' -e with external target allows" \
    "sed -i '' -e 's/a/b/' -e 's/c/d/' $EXT" "$WT_REPO"
assert_deny "sed-operands (#11074): BSD sed -i '' -e with checkout target still denies" \
    "sed -i '' -e 's/a/b/' -e 's/c/d/' README.md" "$WT_REPO"
assert_deny "sed-operands (#11074): BSD sed -i '' attached -es/a/b/ with checkout target denies" \
    "sed -i '' -es/a/b/ README.md" "$WT_REPO"
assert_deny "sed-operands (#11074): BSD sed -i '' positional script with checkout target still denies" \
    "sed -i '' 's/a/b/' README.md" "$WT_REPO"

# --- `--` ends options: every later token is a file operand ----------------
# GNU sed treats `-e`/`-f`/`--file=...` AFTER `--` as filenames; reading them as
# script options would swallow the real target (`-- -e README.md`).
assert_deny "sed-operands (#11074): sed -i -e ... -- -e README.md (-- terminates options) denies" \
    "sed -i -e s/a/b/ -- -e README.md" "$WT_REPO"
assert_deny "sed-operands (#11074): sed -i -e ... -- -f README.md denies" \
    "sed -i -e s/a/b/ -- -f README.md" "$WT_REPO"
assert_deny "sed-operands (#11074): sed -i -e ... -- --file=x README.md denies" \
    "sed -i -e s/a/b/ -- --file=x README.md" "$WT_REPO"
assert_deny "sed-operands (#11074): sed -i -e ... -- --expression README.md denies" \
    "sed -i -e s/a/b/ -- --expression README.md" "$WT_REPO"
assert_deny "sed-operands (#11074): sed -i -e ... -- -es/a/b/ README.md denies" \
    "sed -i -e s/a/b/ -- -es/a/b/ README.md" "$WT_REPO"
assert_deny "sed-operands (#11074): sed -i -e ... -- with absolute main-checkout target denies" \
    "sed -i -e s/a/b/ -- $WT_REPO/f" "$WT_REPO"
assert_deny "sed-operands (#11074): sed -i -- positional script then checkout target denies" \
    "sed -i -- s/a/b/ README.md" "$WT_REPO"
assert_allow "sed-operands (#11074): sed -i -e ... -- with external /tmp target allows" \
    "sed -i -e s/a/b/ -- $EXT" "$WT_REPO"
assert_allow "sed-operands (#11074): sed -i -- positional script with external /tmp target allows" \
    "sed -i -- s/a/b/ $EXT" "$WT_REPO"

rm -rf "$WT_REPO"

echo ""

# =========================================================================

print_summary
