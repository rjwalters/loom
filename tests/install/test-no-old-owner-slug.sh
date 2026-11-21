#!/usr/bin/env bash
# Guard: files Loom ships to consumers must not name the pre-transfer owner slug.
#
# Usage: ./tests/install/test-no-old-owner-slug.sh
#
# Loom moved from rjwalters/loom to 2AMLogic/loom (org transfer, #14170).
# Everything an install or resync copies into a consumer repo -- defaults/, the
# installer and uninstaller, the CLAUDE.md/README.md text, the daemon's
# install-time scaffolding text -- must name the new slug where it is a live
# reference. GitHub redirects the old slug, so a stray hit is not an outage,
# but it silently re-teaches every consumer the stale owner (#14221).
#
# What is scanned: SHIPPED_PATHS below (tracked files only, via `git grep`).
# What is never scanned (history that legitimately names the old owner):
#   CHANGELOG.md, docs/adr/, docs/migration/, WORK_LOG.md
# Fixtures that deliberately use the old slug live outside the scanned set and
# are named here so a future widening of the set does not trip over them:
#   tests/install/test-forge-detect.sh, tests/hooks/test-guard-loom-workflow.sh
# Allowlisted lines inside the scanned set (matched by regex, not by line
# number, so they survive edits):
#   ghcr\.io/rjwalters/   # #14158: the GHCR namespace is renamed with the
#                         # image-publishing workflows, not here. Drop this
#                         # entry when #14158 lands.
#   scaffolding.rs "**Loom Repository**"  # the LEGACY_LOOM_SIGNATURES entry
#                         # must keep matching what pre-transfer installers
#                         # wrote; it detects history, it is not a reference.
#
# The scan is a function over a repo root so the suite can also prove, against
# a throwaway repo, that it flags a regression and honours the allowlist.
#
# Exit code 0 = all tests pass, 1 = failures detected.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"

source "$SCRIPT_DIR/lib/assert.sh"

OLD_SLUG='rjwalters/loom'

# Paths an install/resync ships to consumers, plus the tracked in-repo mirrors
# of them (.loom/, .claude/, .github/CONFIGURATION.md) that CI requires to stay
# byte-equal to defaults/.
SHIPPED_PATHS=(
  defaults
  scripts/install
  scripts/uninstall-loom.sh
  scripts/test-installer.sh
  CLAUDE.md
  README.md
  .loom/README.md
  .loom/config
  .loom/docs
  .loom/install-metadata.json
  .claude/README.md
  .github/CONFIGURATION.md
  loom-daemon/src/init/scaffolding.rs
)

# Lines matching this ERE are allowed to carry the old slug (see header).
# The second alternative permits only the LEGACY_LOOM_SIGNATURES entry in
# scaffolding.rs: that detector must keep matching the `**Loom Repository**:`
# line pre-transfer installers actually wrote, so it stays a live detector
# of history rather than a renamed reference (#14221 review).
ALLOWLIST_RE='ghcr\.io/rjwalters/|loom-daemon/src/init/scaffolding\.rs:[0-9]+:.*\*\*Loom Repository\*\*: https://github\.com/rjwalters/loom'

# scan_old_slug <repo-root>: print "path:line:text" for each non-allowlisted hit.
# Prints "MISSING: <path>" for a SHIPPED_PATHS entry that does not exist, so a
# renamed directory cannot turn the guard into a silent no-op.
scan_old_slug() {
  local root="$1" p hits
  for p in "${SHIPPED_PATHS[@]}"; do
    [[ -e "$root/$p" ]] || echo "MISSING: $p"
  done
  hits="$(git -C "$root" grep -n -I -e "$OLD_SLUG" -- "${SHIPPED_PATHS[@]}" 2>/dev/null || true)"
  [[ -n "$hits" ]] || return 0
  grep -Ev -e "$ALLOWLIST_RE" <<<"$hits" || true
}

# ============================================================================
# The real tree
# ============================================================================
echo ""
echo "=== Shipped files do not name $OLD_SLUG ==="

violations="$(scan_old_slug "$REPO_ROOT")"
assert_eq "no old-owner slug in shipped files (first hits: $(head -n 5 <<<"$violations" | tr '\n' '|'))" \
  "" \
  "$violations"

# ============================================================================
# The scanner itself (throwaway repo)
# ============================================================================
echo ""
echo "=== Scanner flags regressions and honours the allowlist ==="

FIXTURE="$(mktemp -d)"
trap 'rm -rf "$FIXTURE"' EXIT
git -C "$FIXTURE" init -q
for p in "${SHIPPED_PATHS[@]}"; do
  case "$p" in
    *.*) mkdir -p "$FIXTURE/$(dirname "$p")"; echo "clean" >"$FIXTURE/$p" ;;
    *)   mkdir -p "$FIXTURE/$p"; echo "clean" >"$FIXTURE/$p/placeholder.md" ;;
  esac
done
git -C "$FIXTURE" add -A

assert_eq "clean fixture: no hits" "" "$(scan_old_slug "$FIXTURE")"

echo "see https://github.com/$OLD_SLUG/issues/1" >"$FIXTURE/defaults/regressed.md"
echo "image ghcr.io/$OLD_SLUG-worker:latest" >"$FIXTURE/defaults/ghcr-line.md"
git -C "$FIXTURE" add -A
hits="$(scan_old_slug "$FIXTURE")"
assert_eq "regressed file is reported by name" \
  "defaults/regressed.md:1:see https://github.com/$OLD_SLUG/issues/1" \
  "$hits"

echo "history: $OLD_SLUG" >"$FIXTURE/CHANGELOG.md"
mkdir -p "$FIXTURE/docs/adr"
echo "history: $OLD_SLUG" >"$FIXTURE/docs/adr/0001.md"
git -C "$FIXTURE" add -A
assert_eq "history files outside the shipped set are not scanned" \
  "defaults/regressed.md:1:see https://github.com/$OLD_SLUG/issues/1" \
  "$(scan_old_slug "$FIXTURE")"

git -C "$FIXTURE" rm -q -rf scripts/install
assert_eq "a vanished shipped path is reported, not silently skipped" \
  "MISSING: scripts/install" \
  "$(scan_old_slug "$FIXTURE" | grep '^MISSING:')"

# ============================================================================
# Summary
# ============================================================================
echo ""
echo "=========================================="
echo -e "Results: ${PASS} passed, ${FAIL} failed, ${TOTAL} total"
echo "=========================================="

if [[ $FAIL -gt 0 ]]; then
  exit 1
fi
exit 0
