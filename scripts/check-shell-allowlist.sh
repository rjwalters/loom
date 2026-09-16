#!/usr/bin/env bash
# check-shell-allowlist.sh — fail when a tracked `.sh` file is not accounted for
# in scripts/shell-allowlist.txt.
#
# Why (#7762, ADR-0018): Rust owns behavior; shell exists only to reach it. That
# is a policy, and a prose-only policy rots — this repo has the receipts (#7755:
# a safety contract written as a comment, silently violated the moment someone
# added a consumer). So the policy gets the treatment `ci-wired.txt` gets: a
# committed list where every `.sh` carries a category and a one-line reason, and
# a check that fails when a file exists that is in neither state. A new script
# can then still be written — it just cannot be written *silently*.
#
# WHAT THIS IS NOT: a rewrite mandate, and not a size limit. Nothing listed in
# the baseline has to move. The size of these files is a separate, already-built
# mechanism (#7711, check-file-size-budget.sh). This gate fires only when a
# tracked `.sh` appears that nobody classified.
#
# The invariant, mirroring check-ci-suite-manifest.sh's "exactly one of
# wired/excluded, reason required":
#
#   1. Every in-scope `.sh` appears in the manifest exactly once.
#   2. Every entry names a category from the closed list and a non-empty reason.
#   3. No entry references a file that does not exist (or is out of scope).
#   4. `contract` is valid only in the `@section baseline` block. A file that
#      did not exist yet cannot already be called by name, so "existing
#      invocation contract" is structurally unavailable to new shell.
#   5. A `stub` entry is MACHINE-CHECKED against the trivial-glue cap: under
#      STUB_MAX_CODE_LINES code lines AND a last code line that is `exec`. The
#      one category meant to be easy to claim is the one a human never has to
#      take on trust — that is what stops the allowlist becoming a rubber stamp.
#
# Scope: `git ls-files '*.sh'`, minus `.loom/**` and build output. `.loom/scripts`
# is a symlink to `defaults/scripts` here and a resync copy downstream, and
# `.loom/hooks/*.sh` is a tracked resync copy of `defaults/hooks/*.sh`; listing
# either would double-count every entry against its `defaults/` source, which is
# where it is governed (see check-hooks-defaults-parity.sh). Untracked files are
# out of scope on purpose — CI checks out a commit, so anything that can reach
# main is tracked by then.
#
# Usage:
#   check-shell-allowlist.sh              Check the tree against the manifest.
#   check-shell-allowlist.sh --list       Print every in-scope .sh and category.
#   check-shell-allowlist.sh --self-test  Verify the gate itself still works.
#   check-shell-allowlist.sh --root DIR   Check a different tree (used by --self-test).
#   check-shell-allowlist.sh --help
#
# There is deliberately NO `--update`/`--fix` mode. Regenerating the manifest
# from the tree would make every unlisted file self-justifying, which is the
# whole failure this exists to prevent: the reason has to be typed by whoever
# adds the file.
#
# Exit codes: 0 = invariant holds; 1 = violation (details on stderr); 2 = bad args.
#
# Portability: this runs on developer machines, and macOS ships bash 3.2 — the
# recurring class behind #7717, #7728, #7730, #7749 and #7751. No associative
# arrays, no `mapfile`, no `${x,,}`, no `declare -A`, no empty-array expansion
# under `set -u`. Sets are kept in sorted temp files and compared with `comm`.
# Pipelines never end in an early-exit consumer (`head`, `grep -q`, `read`)
# under `pipefail` — that class is ratcheted by check-pipefail-early-exit.sh.

set -euo pipefail

VALID_CATEGORIES="bootstrap hook-entry vendored stub contract test"
# Categories a NEW file may claim. `contract` is absent by construction.
NEW_CATEGORIES="bootstrap hook-entry vendored stub test"
STUB_MAX_CODE_LINES=40

MODE="check"
ROOT_ARG=""

while [ $# -gt 0 ]; do
  case "$1" in
    --list)      MODE="list"; shift ;;
    --self-test) MODE="self-test"; shift ;;
    --root)      ROOT_ARG="${2:?--root needs a directory}"; shift 2 ;;
    --help|-h)   sed -n '2,59p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *)           echo "check-shell-allowlist: unknown argument '$1'" >&2; exit 2 ;;
  esac
done

SCRIPT_PATH="$(cd "$(dirname "$0")" && pwd)/$(basename "$0")"

resolve_root() {
  if [ -n "$ROOT_ARG" ]; then
    ( cd "$ROOT_ARG" && pwd )
    return
  fi
  local script_dir
  script_dir="$(dirname "$SCRIPT_PATH")"
  if git -C "$script_dir" rev-parse --show-toplevel >/dev/null 2>&1; then
    git -C "$script_dir" rev-parse --show-toplevel
  else
    ( cd "$script_dir/.." && pwd )
  fi
}

ROOT="$(resolve_root)"
MANIFEST="$ROOT/scripts/shell-allowlist.txt"

fail=0
err() { printf 'check-shell-allowlist: ERROR: %s\n' "$1" >&2; fail=1; }

# --- Scope -------------------------------------------------------------------
in_scope() { # <repo-relative path>
  case "$1" in
    .loom/*)                                           return 1 ;;
    target/*|*/target/*|node_modules/*|*/node_modules/*) return 1 ;;
    */dist/*)                                          return 1 ;;
  esac
  case "$1" in
    *.sh) return 0 ;;
    *)    return 1 ;;
  esac
}

# Every tracked `.sh` under $ROOT that is in scope, one per line, sorted.
list_tracked() {
  ( cd "$ROOT" && git ls-files '*.sh' 2>/dev/null ) | while IFS= read -r p; do
    [ -n "$p" ] || continue
    if in_scope "$p"; then printf '%s\n' "$p"; fi
  done | LC_ALL=C sort
}

# --- Shape-A stub measurement ------------------------------------------------
# Code lines = lines that are neither blank nor comment-only, the same counting
# rule check-file-size-budget.sh uses.
code_line_count() { # <abs path>
  grep -cvE '^[[:space:]]*(#|$)' "$1" 2>/dev/null || true
}

last_code_line() { # <abs path>
  grep -vE '^[[:space:]]*(#|$)' "$1" 2>/dev/null | tail -1 | sed 's/^[[:space:]]*//'
}

is_shape_a_stub() { # <abs path> -> 0 if it satisfies the trivial-glue cap
  local n last
  n="$(code_line_count "$1")"
  [ -n "$n" ] || return 1
  [ "$n" -lt "$STUB_MAX_CODE_LINES" ] || return 1
  last="$(last_code_line "$1")"
  case "$last" in
    exec\ *|exec) return 0 ;;
    *)            return 1 ;;
  esac
}

is_in_list() { # <needle> <space-separated haystack>
  local needle="$1" item
  for item in $2; do
    [ "$item" = "$needle" ] || continue
    return 0
  done
  return 1
}

# --- Manifest parsing --------------------------------------------------------
# Emits "<section>\t<path>\t<category>\t<reason>" for each non-comment entry.
# Section markers are the comment lines `# @section baseline` / `# @section new`.
parse_manifest() {
  awk '
    /^[[:space:]]*#[[:space:]]*@section[[:space:]]+baseline[[:space:]]*$/ { section = "baseline"; next }
    /^[[:space:]]*#[[:space:]]*@section[[:space:]]+new[[:space:]]*$/      { section = "new"; next }
    /^[[:space:]]*#/ { next }
    /^[[:space:]]*$/ { next }
    {
      path = $1
      category = $2
      reason = ""
      for (i = 3; i <= NF; i++) { reason = reason (i > 3 ? " " : "") $i }
      printf "%s\t%s\t%s\t%s\n", (section == "" ? "-" : section), path, category, reason
    }
  ' "$MANIFEST"
}

run_check() {
  # Fail loudly rather than vacuously. The scan enumerates TRACKED files, so
  # outside a git work tree `git ls-files` returns nothing and every check below
  # would pass while having looked at zero files — a gate that cannot tell
  # "checked, fine" from "could not check" is the exact fail-open class
  # ADR-0018 catalogues (#7745, #7755, #7761).
  if ! git -C "$ROOT" rev-parse --is-inside-work-tree >/dev/null 2>&1; then
    err "$ROOT is not a git work tree; this gate enumerates tracked files and would otherwise report OK without having looked at any"
    return
  fi
  if [ ! -f "$MANIFEST" ]; then
    err "missing manifest: $MANIFEST"
    return
  fi

  local tmp
  tmp="$(mktemp -d)"
  # shellcheck disable=SC2064
  trap "rm -rf '$tmp'" RETURN

  parse_manifest > "$tmp/entries.tsv"

  local section path category reason n_listed=0
  while IFS="$(printf '\t')" read -r section path category reason; do
    [ -n "${path:-}" ] || continue
    n_listed=$((n_listed + 1))
    printf '%s\n' "$path" >> "$tmp/listed.raw"

    if [ "$section" = "-" ]; then
      err "entry '$path' appears before any '# @section baseline' / '# @section new' marker"
    fi
    if ! is_in_list "$category" "$VALID_CATEGORIES"; then
      err "entry '$path' has category '$category', which is not one of: $VALID_CATEGORIES"
      continue
    fi
    if [ -z "$(printf '%s' "${reason:-}" | tr -d '[:space:]')" ]; then
      err "entry '$path' ($category) has no reason — format: '<path>  <category>  <reason>'"
    fi
    if [ "$section" = "new" ] && ! is_in_list "$category" "$NEW_CATEGORIES"; then
      err "entry '$path' claims '$category' in the @section new block. A file that did not exist yet cannot already be invoked by name; use one of: $NEW_CATEGORIES — or make it a loom-daemon subcommand."
    fi
    if ! in_scope "$path"; then
      err "entry '$path' is outside this manifest's scope (tracked *.sh, excluding .loom/ and build output)"
      continue
    fi
    if [ ! -f "$ROOT/$path" ]; then
      err "manifest references nonexistent file: $path — delete the entry"
      continue
    fi
    if [ "$category" = "stub" ] && ! is_shape_a_stub "$ROOT/$path"; then
      err "entry '$path' claims 'stub' but fails the trivial-glue cap: it must be under $STUB_MAX_CODE_LINES code lines AND end in \`exec\` (it is $(code_line_count "$ROOT/$path") code lines, last code line: '$(last_code_line "$ROOT/$path")')"
    fi
  done < "$tmp/entries.tsv"

  touch "$tmp/listed.raw"
  LC_ALL=C sort "$tmp/listed.raw" > "$tmp/listed.sorted"
  LC_ALL=C sort -u "$tmp/listed.raw" > "$tmp/listed.uniq"

  local dupes
  dupes="$(LC_ALL=C uniq -d "$tmp/listed.sorted" | tr '\n' ' ')"
  if [ -n "$(printf '%s' "$dupes" | tr -d '[:space:]')" ]; then
    err "duplicate entries in the manifest: $dupes"
  fi

  list_tracked > "$tmp/actual"

  local unlisted
  unlisted="$(LC_ALL=C comm -23 "$tmp/actual" "$tmp/listed.uniq")"
  if [ -n "$unlisted" ]; then
    printf '%s\n' "$unlisted" | while IFS= read -r p; do
      [ -n "$p" ] || continue
      printf 'check-shell-allowlist: ERROR: %s\n' "'$p' is not in scripts/shell-allowlist.txt." >&2
    done
    cat >&2 <<'MSG'
check-shell-allowlist:
check-shell-allowlist: New executable logic belongs in Rust as a `loom-daemon`
check-shell-allowlist: subcommand (one match arm in main.rs plus a module; the
check-shell-allowlist: shared script_helpers utilities are already there). If the
check-shell-allowlist: file above genuinely has to be shell, add it under
check-shell-allowlist: `# @section new` with one of: bootstrap, hook-entry,
check-shell-allowlist: vendored, stub, test — and a one-line reason.
check-shell-allowlist: See .loom/docs/shell-language-policy.md.
MSG
    fail=1
  fi

  if [ "$fail" -eq 0 ]; then
    printf 'check-shell-allowlist: OK — %d tracked shell scripts, all accounted for (%d manifest entries).\n' \
      "$(wc -l < "$tmp/actual" | tr -d ' ')" "$n_listed"
  fi
}

run_list() {
  local tmp
  tmp="$(mktemp -d)"
  # shellcheck disable=SC2064
  trap "rm -rf '$tmp'" RETURN
  if [ -f "$MANIFEST" ]; then
    parse_manifest | awk -F'\t' '{ printf "%s\t%s\n", $2, $3 }' | LC_ALL=C sort > "$tmp/cat"
  else
    : > "$tmp/cat"
  fi
  list_tracked | while IFS= read -r p; do
    [ -n "$p" ] || continue
    c="$(awk -F'\t' -v want="$p" '$1 == want { print $2 }' "$tmp/cat")"
    printf '%-77s  %s\n' "$p" "${c:-<UNLISTED>}"
  done
}

# --- Self-test ---------------------------------------------------------------
# A gate that silently stops checking reports OK forever. Every rule above is
# exercised against a synthetic fixture tree, per the convention
# check-docs-defaults-parity.sh and check-file-size-budget.sh already follow.
self_test() {
  local tmp rc=0 out got i
  tmp="$(mktemp -d)"
  # shellcheck disable=SC2064
  trap "rm -rf '$tmp'" RETURN

  mkdir -p "$tmp/scripts" "$tmp/defaults/scripts" "$tmp/.loom/scripts"
  git -C "$tmp" init -q
  git -C "$tmp" config user.email t@t.test
  git -C "$tmp" config user.name t

  printf '#!/usr/bin/env bash\necho hi\n' > "$tmp/defaults/scripts/listed.sh"
  printf '#!/usr/bin/env bash\necho mirror\n' > "$tmp/.loom/scripts/mirror.sh"
  # A real shape-A stub: 2 code lines, last one is `exec`.
  printf '#!/usr/bin/env bash\n# a comment\nset -e\nexec loom-daemon thing "$@"\n' > "$tmp/scripts/good-stub.sh"
  # Fails the cap on the `exec` half: short, but does not end in exec.
  printf '#!/usr/bin/env bash\nloom-daemon thing "$@"\necho done\n' > "$tmp/scripts/not-a-stub.sh"
  # Fails the cap on the size half: ends in exec, but far too long.
  { printf '#!/usr/bin/env bash\n'; i=0; while [ "$i" -lt 60 ]; do echo "x=$i"; i=$((i + 1)); done; printf 'exec loom-daemon thing "$@"\n'; } > "$tmp/scripts/too-long.sh"

  git -C "$tmp" add -A >/dev/null
  git -C "$tmp" commit -qm fixtures

  _manifest() { cat > "$tmp/scripts/shell-allowlist.txt"; }
  # Runs the gate against the fixture tree, capturing BOTH its output and its
  # exit status. `A && B || C` would be ambiguous here (and `set -e` would abort
  # on the failing cases we are deliberately provoking), so the status is taken
  # explicitly.
  out=""; got=0
  _run() {
    if out="$(bash "$SCRIPT_PATH" --root "$tmp" 2>&1)"; then got=0; else got=$?; fi
  }
  _expect() { # <label> <want-rc> <got-rc>
    if [ "$2" = "$3" ]; then
      echo "  ok   $1"
    else
      echo "  FAIL $1 (want rc=$2, got rc=$3)" >&2
      rc=1
    fi
  }
  _expect_match() { # <label> <regex> <text>
    # Herestring, not a pipe: `grep -q` behind `|` under `set -o pipefail` is
    # the SIGPIPE class scripts/check-pipefail-early-exit.sh ratchets (#7790).
    if grep -qE "$2" <<<"$3"; then
      echo "  ok   $1"
    else
      echo "  FAIL $1 — output did not match /$2/" >&2
      printf '%s\n' "$3" | sed 's/^/        /' >&2
      rc=1
    fi
  }

  local base
  base='# @section baseline
defaults/scripts/listed.sh  contract  Invoked by name.
scripts/good-stub.sh        stub      Shape-A stub.
scripts/not-a-stub.sh       contract  Invoked by name.
scripts/too-long.sh         contract  Invoked by name.
'

  echo "check-shell-allowlist --self-test:"

  # 1. A complete manifest passes, and .loom/ is out of scope (mirror.sh is
  #    tracked but unlisted, and must NOT be reported).
  printf '%s' "$base" | _manifest
  _run; _expect "complete manifest passes" 0 "$got"
  _expect_match "reports an OK summary" 'OK — 4 tracked shell scripts' "$out"
  case "$out" in *mirror.sh*) echo "  FAIL .loom/ mirror was scanned" >&2; rc=1 ;; *) echo "  ok   .loom/ mirror was not scanned" ;; esac

  # 2. An unlisted tracked script fails, is named, and the message teaches.
  printf '#!/usr/bin/env bash\necho new\n' > "$tmp/defaults/scripts/sneaky.sh"
  git -C "$tmp" add -A >/dev/null && git -C "$tmp" commit -qm sneaky
  _run; _expect "unlisted script fails the gate" 1 "$got"
  _expect_match "names the unlisted file" "sneaky\.sh' is not in scripts/shell-allowlist" "$out"
  _expect_match "points at the Rust default" 'loom-daemon' "$out"

  # 3. Listing it in @section new with a valid category passes.
  printf '%s%s' "$base" '
# @section new
defaults/scripts/sneaky.sh  test  Tests something shell.
' | _manifest
  _run; _expect "new-section entry with a valid category passes" 0 "$got"

  # 4. `contract` is refused in @section new.
  printf '%s%s' "$base" '
# @section new
defaults/scripts/sneaky.sh  contract  Invoked by name.
' | _manifest
  _run; _expect "contract in @section new fails" 1 "$got"
  _expect_match "explains why contract is unavailable to new files" 'cannot already be invoked by name' "$out"

  # 5. A missing reason fails.
  printf '%s%s' "$base" '
# @section new
defaults/scripts/sneaky.sh  test
' | _manifest
  _run; _expect "missing reason fails" 1 "$got"
  _expect_match "names the reason requirement" 'has no reason' "$out"

  # 6. An unknown category fails.
  printf '%s%s' "$base" '
# @section new
defaults/scripts/sneaky.sh  convenient  Felt faster in bash.
' | _manifest
  _run; _expect "unknown category fails" 1 "$got"
  _expect_match "names the closed category list" 'not one of' "$out"

  rm -f "$tmp/defaults/scripts/sneaky.sh"
  git -C "$tmp" add -A >/dev/null && git -C "$tmp" commit -qm rm-sneaky

  # 7. The trivial-glue cap is machine-checked, both halves.
  printf '%s' '# @section baseline
defaults/scripts/listed.sh  contract  Invoked by name.
scripts/good-stub.sh        stub      Shape-A stub.
scripts/not-a-stub.sh       stub      Claims glue but never execs.
scripts/too-long.sh         contract  Invoked by name.
' | _manifest
  _run; _expect "stub claim without exec fails" 1 "$got"
  _expect_match "explains the cap" 'fails the trivial-glue cap' "$out"

  printf '%s' '# @section baseline
defaults/scripts/listed.sh  contract  Invoked by name.
scripts/good-stub.sh        stub      Shape-A stub.
scripts/not-a-stub.sh       contract  Invoked by name.
scripts/too-long.sh         stub      Claims glue but is 61 code lines.
' | _manifest
  _run; _expect "stub claim over the line cap fails" 1 "$got"
  _expect_match "reports the measured size" 'code lines' "$out"

  # 8. A stale entry (file deleted) fails.
  printf '%s%s' "$base" '
# @section new
defaults/scripts/gone.sh  test  Tests something that no longer exists.
' | _manifest
  _run; _expect "stale entry fails" 1 "$got"
  _expect_match "names the stale entry" 'nonexistent file' "$out"

  # 9. A duplicate entry fails.
  printf '%s%s' "$base" 'defaults/scripts/listed.sh  contract  Listed twice.
' | _manifest
  _run; _expect "duplicate entry fails" 1 "$got"
  _expect_match "names the duplicate" 'duplicate entries' "$out"

  # 10. A missing manifest fails rather than passing vacuously.
  rm -f "$tmp/scripts/shell-allowlist.txt"
  _run; _expect "missing manifest fails" 1 "$got"
  _expect_match "names the missing manifest" 'missing manifest' "$out"

  # 11. A non-git tree fails rather than reporting OK on zero files.
  local nogit
  nogit="$(mktemp -d)"
  mkdir -p "$nogit/scripts"
  printf '# @section baseline\n' > "$nogit/scripts/shell-allowlist.txt"
  if out="$(bash "$SCRIPT_PATH" --root "$nogit" 2>&1)"; then got=0; else got=$?; fi
  rm -rf "$nogit"
  _expect "non-git tree fails instead of passing vacuously" 1 "$got"
  _expect_match "says why it could not check" 'not a git work tree' "$out"

  if [ "$rc" -eq 0 ]; then
    echo "check-shell-allowlist: --self-test OK"
  else
    echo "check-shell-allowlist: --self-test FAILED" >&2
  fi
  return "$rc"
}

case "$MODE" in
  check)     run_check; exit "$fail" ;;
  list)      run_list; exit 0 ;;
  self-test) self_test ;;
esac
