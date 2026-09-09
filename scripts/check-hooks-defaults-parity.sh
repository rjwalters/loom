#!/usr/bin/env bash
# check-hooks-defaults-parity.sh — fail CI when a defaults/hooks/*.sh or
# defaults/scripts/*.sh file has drifted from its installed .loom/ copy.
#
# Why (#7416): .loom/hooks/ and .loom/scripts/ are the surfaces the harness
# ACTUALLY executes (this repo's own PreToolUse guard hook runs
# .loom/hooks/guard-destructive-generic.sh, not defaults/hooks/...). They are
# populated FROM defaults/hooks/ and defaults/scripts/ by resync-installed.sh,
# but that propagation is not automatic — a PR that edits only the defaults/
# copy (the source of truth) leaves the INSTALLED copy silently stale
# indefinitely, exactly as check-docs-defaults-parity.sh (#4841) already
# guards against for defaults/docs/ vs .loom/docs/.
#
# The concrete incident this fixes: PR #7378 (commit c67b6cc3) fixed a
# false-positive in defaults/hooks/guard-destructive-generic.sh's qsplit()
# quote-tracking (the embedded-apostrophe idiom, #6968) but never touched
# .loom/hooks/guard-destructive-generic.sh. A later commit (fc469d82, #6207)
# touched BOTH copies for an unrelated fix, computed against each copy's own
# already-divergent baseline, so it silently carried the drift forward
# instead of surfacing it. No CI check caught either step.
#
# What it checks:
#   1. Every *.sh directly under defaults/hooks/ (resync-installed.sh's own
#      hooks scope: TOP-LEVEL *.sh only, not recursive) must be byte-identical
#      to .loom/hooks/<name>, when that installed copy exists as a real file
#      (not a symlink — a symlinked .loom/hooks/ target already resolves
#      straight through to the defaults/ source and can never drift).
#   2. Every *.sh anywhere under defaults/scripts/ (resync-installed.sh's
#      scripts scope: recursive) must be byte-identical to the same-relative-
#      path file under .loom/scripts/, under the same real-file-only
#      condition (this repo's own .loom/scripts/ is a symlink straight to
#      defaults/scripts/, so this check is a structural no-op for the source
#      repo itself and only bites in a consumer repo carrying a real copy).
#   3. A missing installed copy (defaults/ has the file, .loom/ does not) is
#      also a failure — same failure mode as an unresynced NEW file.
#
# Both checks honor .loom/resync-ignore pins (see resync-installed.sh's own
# is_ignored()) — a pin of "hooks/foo.sh" or "scripts/sub/bar.sh" excludes
# that one file from the parity requirement, mirroring the exact opt-out
# resync-installed.sh itself already respects so this checker can never fail
# on a deliberately-pinned local override.
#
# Usage:
#   check-hooks-defaults-parity.sh [ROOT]
#     ROOT  Repository root containing defaults/{hooks,scripts}/ and
#           .loom/{hooks,scripts}/. Defaults to `git rev-parse
#           --show-toplevel`, then the script's own repo root. If
#           <ROOT>/defaults does not exist (e.g. an installed downstream repo
#           with no source tree), the check is a clean no-op.
#
#   check-hooks-defaults-parity.sh --self-test
#     Runs an isolated, synthetic-fixture regression test (mirroring
#     check-docs-defaults-parity.sh --self-test): builds a fixture with
#     defaults/hooks/ and defaults/scripts/ files, asserts the check passes
#     when the .loom/ copies match, fails when one is deliberately
#     desynchronized (both a content-drift case and a missing-file case), and
#     passes again once resynced — plus a resync-ignore pin case. Exits
#     non-zero if any of this checker's discriminating power has regressed.
#     Does not touch the real repo tree.
#
# Exit codes: 0 = clean (or self-test passed); 1 = violation(s) found (or
# self-test failed) — details printed to stderr.

set -euo pipefail

# --- resync-ignore pin support (mirrors resync-installed.sh's is_ignored) ---
is_ignored() {
  local ignore_file="$1" rel="$2" line normalized
  [[ -f "$ignore_file" ]] || return 1
  while IFS= read -r line || [[ -n "$line" ]]; do
    line="${line%%#*}"
    line="${line#"${line%%[![:space:]]*}"}"
    line="${line%"${line##*[![:space:]]}"}"
    [[ -z "$line" ]] && continue
    if [[ "$line" == "$rel" ]]; then
      return 0
    fi
    normalized="${line#./}"
    normalized="${normalized#.loom/}"
    if [[ "$normalized" != "$line" && "$normalized" == */* && "$normalized" == "$rel" ]]; then
      return 0
    fi
  done <"$ignore_file"
  return 1
}

# --- Check: content parity between a defaults/ subtree and its .loom/ copy --
# $1 = defaults subtree (e.g. "$ROOT/defaults/hooks")
# $2 = installed subtree (e.g. "$ROOT/.loom/hooks")
# $3 = find scope: "top" (maxdepth 1, mirrors hooks/) or "recursive" (mirrors
#      scripts/)
# $4 = resync-ignore prefix (e.g. "hooks" or "scripts") used to build the
#      "$prefix/$relpath" pin string checked against $5
# $5 = ignore file path (may not exist)
# Prints violations to stderr. Returns 0 if clean, 1 if any violation found.
check_content_parity() {
  local defaults_dir="$1" loom_dir="$2" scope="$3" ignore_prefix="$4" ignore_file="$5"
  local fail=0 f rel loom_file

  if [[ ! -d "$defaults_dir" ]]; then
    echo "check-hooks-defaults-parity: no $defaults_dir — nothing to check (ok)."
    return 0
  fi

  local -a find_args=("$defaults_dir")
  if [[ "$scope" == "top" ]]; then
    find_args+=(-maxdepth 1)
  fi
  find_args+=(-type f -name "*.sh")

  while IFS= read -r -d '' f; do
    rel="${f#"$defaults_dir"/}"
    if is_ignored "$ignore_file" "${ignore_prefix}/${rel}"; then
      continue
    fi
    loom_file="$loom_dir/$rel"

    if [[ ! -e "$loom_file" ]]; then
      echo "MISSING-INSTALL: ${defaults_dir}/${rel} has no installed counterpart at ${loom_file}" >&2
      echo "  fix: run ./.loom/scripts/resync-installed.sh (or --output for a staged resync from a worktree)" >&2
      fail=1
      continue
    fi

    # A symlinked installed copy resolves straight through to the source and
    # can never drift structurally — skip the content diff (still covers the
    # "missing" case above even for a would-be-symlink target).
    if [[ -L "$loom_file" ]]; then
      continue
    fi

    if ! cmp -s "$f" "$loom_file"; then
      echo "CONTENT-DRIFT: ${defaults_dir}/${rel} differs from installed ${loom_file}" >&2
      echo "  fix: run ./.loom/scripts/resync-installed.sh (or --output for a staged resync from a worktree), or pin ${ignore_prefix}/${rel} in .loom/resync-ignore if this is an intentional local override" >&2
      fail=1
    fi
  done < <(find "${find_args[@]}" -print0 | sort -z)

  return $fail
}

# --- Self-test -----------------------------------------------------------------
run_self_test() {
  local tmp
  tmp="$(mktemp -d)"
  trap 'rm -rf "$tmp"' RETURN

  mkdir -p "$tmp/defaults/hooks" "$tmp/.loom/hooks"
  mkdir -p "$tmp/defaults/scripts/sub" "$tmp/.loom/scripts/sub"

  # In-sync fixtures.
  echo '#!/usr/bin/env bash' >"$tmp/defaults/hooks/synced.sh"
  echo 'echo synced' >>"$tmp/defaults/hooks/synced.sh"
  cp "$tmp/defaults/hooks/synced.sh" "$tmp/.loom/hooks/synced.sh"

  echo '#!/usr/bin/env bash' >"$tmp/defaults/scripts/sub/synced.sh"
  echo 'echo synced-nested' >>"$tmp/defaults/scripts/sub/synced.sh"
  cp "$tmp/defaults/scripts/sub/synced.sh" "$tmp/.loom/scripts/sub/synced.sh"

  local self_test_fail=0

  echo "check-hooks-defaults-parity --self-test: asserting a clean fixture passes..."
  if ! check_content_parity "$tmp/defaults/hooks" "$tmp/.loom/hooks" "top" "hooks" "$tmp/.loom/resync-ignore"; then
    echo "SELF-TEST FAIL: check_content_parity flagged the in-sync hooks fixture" >&2
    self_test_fail=1
  else
    echo "  ok: in-sync defaults/hooks fixture passes"
  fi
  if ! check_content_parity "$tmp/defaults/scripts" "$tmp/.loom/scripts" "recursive" "scripts" "$tmp/.loom/resync-ignore"; then
    echo "SELF-TEST FAIL: check_content_parity flagged the in-sync scripts fixture" >&2
    self_test_fail=1
  else
    echo "  ok: in-sync defaults/scripts fixture passes"
  fi

  # Content-drift case (the #7378-into-.loom/ regression shape): edit the
  # defaults/ copy without touching the installed copy.
  echo 'echo an-added-line-the-installed-copy-never-got' >>"$tmp/defaults/hooks/synced.sh"

  echo "check-hooks-defaults-parity --self-test: asserting a desynchronized defaults/hooks/ file is flagged..."
  if check_content_parity "$tmp/defaults/hooks" "$tmp/.loom/hooks" "top" "hooks" "$tmp/.loom/resync-ignore" >/tmp/self-test-drift.$$ 2>&1; then
    echo "SELF-TEST FAIL: check_content_parity did not detect the content-drift fixture" >&2
    cat /tmp/self-test-drift.$$ >&2
    self_test_fail=1
  else
    echo "  ok: content-drift fixture correctly flagged"
  fi
  rm -f /tmp/self-test-drift.$$

  # Missing-install case: a brand new defaults/scripts/ file never resynced.
  echo '#!/usr/bin/env bash' >"$tmp/defaults/scripts/sub/new.sh"

  echo "check-hooks-defaults-parity --self-test: asserting a missing installed copy is flagged..."
  if check_content_parity "$tmp/defaults/scripts" "$tmp/.loom/scripts" "recursive" "scripts" "$tmp/.loom/resync-ignore" >/tmp/self-test-missing.$$ 2>&1; then
    echo "SELF-TEST FAIL: check_content_parity did not detect the missing-install fixture" >&2
    cat /tmp/self-test-missing.$$ >&2
    self_test_fail=1
  else
    echo "  ok: missing-install fixture correctly flagged"
  fi
  rm -f /tmp/self-test-missing.$$

  # Fix both by resyncing (what the real fix does: copy defaults/ -> .loom/).
  cp "$tmp/defaults/hooks/synced.sh" "$tmp/.loom/hooks/synced.sh"
  cp "$tmp/defaults/scripts/sub/new.sh" "$tmp/.loom/scripts/sub/new.sh"

  echo "check-hooks-defaults-parity --self-test: asserting both fixtures pass once resynced..."
  if ! check_content_parity "$tmp/defaults/hooks" "$tmp/.loom/hooks" "top" "hooks" "$tmp/.loom/resync-ignore"; then
    echo "SELF-TEST FAIL: check_content_parity still fails on defaults/hooks/ after resync" >&2
    self_test_fail=1
  else
    echo "  ok: defaults/hooks/ clean post-resync"
  fi
  if ! check_content_parity "$tmp/defaults/scripts" "$tmp/.loom/scripts" "recursive" "scripts" "$tmp/.loom/resync-ignore"; then
    echo "SELF-TEST FAIL: check_content_parity still fails on defaults/scripts/ after resync" >&2
    self_test_fail=1
  else
    echo "  ok: defaults/scripts/ clean post-resync"
  fi

  # resync-ignore pin case: desync again, but this time pin the file and
  # assert the check no longer flags it.
  echo 'echo desynced-again-but-pinned' >>"$tmp/defaults/hooks/synced.sh"
  echo "hooks/synced.sh # deliberately pinned for this self-test" >"$tmp/.loom/resync-ignore"

  echo "check-hooks-defaults-parity --self-test: asserting a resync-ignore pin exempts its file..."
  if ! check_content_parity "$tmp/defaults/hooks" "$tmp/.loom/hooks" "top" "hooks" "$tmp/.loom/resync-ignore"; then
    echo "SELF-TEST FAIL: check_content_parity flagged a pinned file (hooks/synced.sh)" >&2
    self_test_fail=1
  else
    echo "  ok: pinned file correctly exempted"
  fi
  rm -f "$tmp/.loom/resync-ignore"

  if [[ "$self_test_fail" -ne 0 ]]; then
    echo "" >&2
    echo "check-hooks-defaults-parity --self-test: FAIL — the checker's discriminating power has regressed." >&2
    return 1
  fi

  echo "check-hooks-defaults-parity --self-test: OK — content drift, missing installs, and resync-ignore pins all behave as expected."
  return 0
}

# --- Entry point ---------------------------------------------------------------
if [[ "${1:-}" == "--self-test" ]]; then
  run_self_test
  exit $?
fi

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
if [[ $# -ge 1 && -n "${1:-}" ]]; then
  ROOT="$1"
else
  if ROOT="$(git -C "$SCRIPT_DIR" rev-parse --show-toplevel 2>/dev/null)"; then
    :
  else
    ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
  fi
fi

if [[ ! -d "$ROOT/defaults" ]]; then
  echo "check-hooks-defaults-parity: no defaults/ under $ROOT — nothing to check (ok)."
  exit 0
fi

IGNORE_FILE="$ROOT/.loom/resync-ignore"

overall_fail=0

check_content_parity "$ROOT/defaults/hooks" "$ROOT/.loom/hooks" "top" "hooks" "$IGNORE_FILE" || overall_fail=1
check_content_parity "$ROOT/defaults/scripts" "$ROOT/.loom/scripts" "recursive" "scripts" "$IGNORE_FILE" || overall_fail=1

if [[ "$overall_fail" -ne 0 ]]; then
  {
    echo ""
    echo "check-hooks-defaults-parity: FAIL — see violations above."
    echo ""
    echo "Consumer repos (including this one, for its own self-hosted guard hooks)"
    echo "populate .loom/hooks/ and .loom/scripts/ FROM defaults/hooks/ and"
    echo "defaults/scripts/ (resync-installed.sh). A defaults/ fix that never gets"
    echo "resynced into .loom/ leaves the INSTALLED copy — the one actually executed"
    echo "— silently stale indefinitely (#7416, following PR #7378's fix landing in"
    echo "defaults/ only)."
  } >&2
  exit 1
fi

echo "check-hooks-defaults-parity: OK — every defaults/hooks/*.sh and defaults/scripts/*.sh file matches its installed .loom/ copy."
exit 0
