#!/usr/bin/env bash
# check-dangling-links.sh — fail if a tracked markdown file contains a
# relative internal link (or image reference) to a path that does not exist
# in the repo.
#
# Why (#5488, Epic #5038 Phase 2): six dangling links shipped in Loom's own
# docs before anything caught them (#4988), discovered only by a periodic
# filer well after the introducing PR merged. This generalizes that failure
# class into a pre-merge CI gate — per #5038's reasoning, failing the PR that
# introduces the drift is strictly better than filing a follow-up issue after
# the fact, and the check is deterministic (zero marginal LLM/agent cost).
#
# scripts/check-docs-defaults-parity.sh already guards a narrow slice of this
# (same-directory sibling links and `../`-escaping links, but ONLY within
# defaults/docs/*.md). This script generalizes the "does the link target
# exist" half of that check to every tracked markdown file OUTSIDE
# `defaults/` (README.md, docs/**, .loom/docs/**, .loom/roles/**, etc.), PLUS
# a destination-aware mode for two `defaults/**` subtrees (see below) — it
# does not replace check-docs-defaults-parity.sh's shipped-tree-specific
# parity and vendored-root checks.
#
# `defaults/**` is mostly still excluded (see SKIP_PREFIXES below): every
# file under `defaults/` is an install-time template that `install.sh` /
# `resync-installed.sh` copies into a CONSUMER repo's tree, and most
# subtrees rename their install destination (e.g. `defaults/roles/*.md` ->
# `.loom/roles/*.md`, `defaults/docs/*.md` -> `.loom/docs/*.md`) in a way
# this script does not attempt to model — resolving THOSE literally at their
# source-repo path would produce guaranteed false positives. `defaults/docs/*.md`
# is the one such subtree whose own link semantics ARE already checked, by
# check-docs-defaults-parity.sh, which is deliberately relative-root-aware.
#
# Two subtrees map to their installed destination via a plain "defaults/"
# prefix strip with NO rename (`defaults/.loom/X` -> `.loom/X`,
# `defaults/.claude/X` -> `.claude/X`, `defaults/.github/X` -> `.github/X`)
# — for these, DIRECT_MAP_PREFIXES below opts them INTO an installed-
# destination-aware check instead of skipping them (issue #6321: this is the
# class of bug that shipped a dangling link in
# `defaults/.claude/commands/loom/builder.md`, installed 3 directories
# deeper than the template's own location, with no CI check to catch it).
# Two files in this set — `defaults/.loom/CLAUDE.md` and
# `defaults/.loom/AGENTS.md` — additionally get their `.loom/`- and
# `.github/`-prefixed link *targets* rewritten at install time by
# `localize_dotloom_doc_links()` (`loom-daemon/src/init/templates.rs`, issue
# #5975 / PR #6001, already merged); NEEDS_LOCALIZE below mirrors that exact
# transform so this script validates what actually ships, not the
# pre-rewrite template text (which is intentionally still authored
# repo-root-relative — see that function's own doc comment).
#
# What it checks:
#   For every git-tracked *.md file outside `defaults/` (plus the two
#   destination-aware `defaults/` subtrees above), every markdown-style
#   link/image target `[text](target)` / `![alt](target)` whose target is NOT
#   an absolute URL (http(s)://, mailto:), NOT a bare "#anchor" fragment, and
#   NOT an unresolvable templated placeholder (contains a literal "<" — e.g.
#   "<workspace>" in prose-as-example text) is resolved relative to the
#   linking file's own directory (its INSTALLED directory, for the
#   destination-aware subtrees — after stripping any "#anchor" suffix and a
#   leading "./"). If the resolved path does not exist on disk (file OR
#   directory), that's a dangling link. Text inside fenced code blocks
#   (``` ... ```) is skipped, since those commonly show illustrative link
#   syntax or example paths that are not meant to resolve.
#
#   Path resolution normalizes "." / ".." segments algebraically rather than
#   relying on the OS to walk a literal path — a destination-aware target
#   like `../../../.loom/docs/foo.md` resolved from an installed directory
#   such as `.claude/commands/loom/` needs `.claude/commands/loom/` to
#   algebraically cancel out even when that directory does not physically
#   exist in THIS checkout (e.g. a fresh git worktree, which — unlike this
#   repo's own dogfooded primary checkout — has no `.claude/commands/loom`
#   symlink at all; #6321). A plain `[[ -e path/with/../../nonexistent/.. ]]`
#   test would false-positive there.
#
#   Only git-tracked files are scanned (`git ls-files`), so this
#   automatically respects .gitignore — build outputs, node_modules/,
#   target/, .loom/worktrees/, etc. are never walked. Directory-symlinked
#   trees (e.g. `.loom/roles -> ../defaults/roles`, `.loom/scripts ->
#   ../defaults/scripts`) are never traversed by `git ls-files` either — git
#   tracks the symlink itself as one entry, not its target's contents — so
#   they are naturally out of scope here too (their real files live under
#   `defaults/`, covered structurally above where applicable).
#
# Usage:
#   check-dangling-links.sh [ROOT]
#     ROOT  Repository root to scan. Defaults to `git rev-parse
#           --show-toplevel`, then the script's own repo root.
#
#   check-dangling-links.sh --self-test
#     Runs isolated, synthetic-fixture regression tests — the plain-mode
#     checks (a dangling relative link, a fenced-code-block example that must
#     NOT be flagged, and a valid link) plus the destination-aware
#     `defaults/**` mode (a link that only resolves once rebased to its
#     installed directory, and a `defaults/.loom/CLAUDE.md`-style fixture
#     needing the localize-rewrite transform) — and asserts the checker
#     discriminates correctly in both modes. Does not touch the real repo tree.
#
# Exit codes: 0 = clean (or self-test passed); 1 = dangling link(s) found (or
# self-test failed) — details printed to stderr as `file:line`.

set -euo pipefail

# --- Path normalization ----------------------------------------------------
# Algebraically collapses "." and ".." segments in a slash-separated relative
# path WITHOUT touching the filesystem. A plain `[[ -e ]]` test on a literal
# path containing "../" through a directory that doesn't physically exist in
# THIS checkout (e.g. `.claude/commands/loom/../../../.loom/docs/foo.md`,
# where `.claude/commands/loom/` is only a symlink in this repo's own
# dogfooded primary checkout, absent from a fresh git worktree) fails even
# though the path is mathematically valid — see #6321. Portable to bash 3.2+
# (no negative array indices).
normalize_path() {
  local path="$1" seg
  local -a segs out
  IFS='/' read -r -a segs <<<"$path"
  out=()
  for seg in "${segs[@]}"; do
    case "$seg" in
      "" | ".") continue ;;
      "..")
        if [[ ${#out[@]} -gt 0 && "${out[$((${#out[@]} - 1))]}" != ".." ]]; then
          out=("${out[@]:0:$((${#out[@]} - 1))}")
        else
          out+=("..")
        fi
        ;;
      *) out+=("$seg") ;;
    esac
  done
  local IFS='/'
  echo "${out[*]}"
}

# --- Core check ----------------------------------------------------------
# Scans one markdown file for dangling relative links. Prints violations to
# stderr as "DANGLING-LINK: <file>:<line> ...". Returns 0 if clean, 1 if any
# dangling link was found.
#
# Optional args (both empty/"0" by default — the plain, non-`defaults/` mode):
#   dir_override  Resolve link targets against THIS directory instead of
#                 `dirname "$relfile"` — used for the `defaults/**`
#                 destination-aware mode, where the file's installed
#                 directory differs from its location in this source repo.
#   localize      "1" to apply the same `](.loom/` -> `](` and
#                 `](.github/` -> `](../.github/` link-target rewrite that
#                 `localize_dotloom_doc_links()` (loom-daemon/src/init/templates.rs)
#                 applies at install time, before extracting link targets —
#                 for the two files that transform actually runs on
#                 (`defaults/.loom/CLAUDE.md`, `defaults/.loom/AGENTS.md`).
check_file() {
  local root="$1" relfile="$2" dir_override="${3:-}" localize="${4:-0}"
  local f="$root/$relfile"
  local dir fail=0
  if [[ -n "$dir_override" ]]; then
    dir="$dir_override"
  else
    dir="$(dirname "$relfile")"
  fi

  # Single awk pass per file (not a per-line subprocess spawn — with ~225
  # tracked markdown files and some multi-thousand-line docs, a per-line
  # `read` + `grep` pipeline measured minutes; this is sub-second). Emits
  # "lineno<TAB>target" for every "](target)" match on a line that is not
  # inside a fenced code block (``` or ~~~ toggles fence state, mirroring
  # GitHub's own fencing rule of "any line starting with the fence marker,
  # ignoring leading whitespace"). `localize` runs the install-time link
  # rewrite through `sed` first so line numbers still match the source file
  # (the rewrite never changes line count, only `](...)` targets).
  while IFS=$'\t' read -r lineno target; do
    [[ -z "$lineno" ]] && continue

    local path_part resolved
    case "$target" in
      http://* | https://* | mailto:* | "#"*) continue ;;
    esac

    # Skip templated/placeholder targets (e.g. "<workspace>", "<PR_NUMBER>")
    # — these are prose examples, not real link targets.
    [[ "$target" == *"<"* ]] && continue

    # Strip a leading "./" (same-directory, spelled explicitly).
    target="${target#./}"

    # Split off any #anchor fragment before path checks.
    path_part="${target%%#*}"
    [[ -z "$path_part" ]] && continue

    # Skip anything that still looks like a URL scheme (e.g. "ssh://",
    # "ftp://") not covered by the case above.
    [[ "$path_part" == *"://"* ]] && continue

    resolved="$(normalize_path "${dir}/${path_part}")"

    # `.claude/commands/loom/` only exists on disk as a gitignored, dogfood-
    # only symlink in this repo's own PRIMARY checkout (`scripts/install/
    # dogfood-commands.sh`) — a fresh worktree or CI checkout has no such
    # path at all, even though the corresponding template content is real
    # and git-tracked under `defaults/.claude/...`. So in destination-aware
    # mode, a resolved target under `.claude/` is checked against its
    # `defaults/.claude/...` SOURCE instead of the (unreliable) installed
    # path — the two are byte-identical post-install (DIRECT_MAP_PREFIXES,
    # no rename). `.loom/` and `.github/` don't need this: `.loom/docs/**`
    # and `.loom/roles` are real git-tracked content in this repo (the
    # former dogfood-installed permanently, the latter a tracked symlink),
    # and `.github/**` is this repo's own independently-tracked directory.
    exists_check="$root/$resolved"
    if [[ -n "$dir_override" && "$resolved" == .claude/* ]]; then
      exists_check="$root/defaults/$resolved"
    fi

    if [[ ! -e "$exists_check" ]]; then
      echo "DANGLING-LINK: ${relfile}:${lineno} links '${path_part}' which does not resolve to an existing file/dir (resolved: ${resolved}${dir_override:+, installed dir: $dir_override})" >&2
      echo "  line: $(sed -n "${lineno}p" "$f" | sed 's/^[[:space:]]*//')" >&2
      fail=1
    fi
  done < <(
    if [[ "$localize" == "1" ]]; then
      sed -e 's/\](\.loom\//](/g' -e 's/\](\.github\//](..\/.github\//g' "$f"
    else
      cat "$f"
    fi | awk '
    BEGIN { infence = 0 }
    {
      line = $0
      if (line ~ /^[[:space:]]*(```|~~~)/) { infence = 1 - infence; next }
      if (infence) next
      s = line
      while (match(s, /\]\([^)]+\)/)) {
        target = substr(s, RSTART + 2, RLENGTH - 3)
        print NR "\t" target
        s = substr(s, RSTART + RLENGTH)
      }
    }
  ')

  return $fail
}

# --- Self-test -------------------------------------------------------------
run_self_test() {
  local tmp
  tmp="$(mktemp -d)"
  trap 'rm -rf "$tmp"' RETURN

  mkdir -p "$tmp/docs"
  echo "# Real target" >"$tmp/docs/real.md"

  cat >"$tmp/docs/fixture.md" <<'EOF'
# Fixture

A valid link: [real](real.md).

A dangling link: [missing](missing.md).

An example inside a fenced code block, which must NOT be flagged:

```
See [example](does-not-exist.md) for the syntax.
```

A templated placeholder, which must NOT be flagged: [ws](<workspace>/foo.md).
EOF

  local self_test_fail=0

  echo "check-dangling-links --self-test: asserting the checker flags the dangling link but not the valid link, fenced example, or placeholder..."
  local out
  if out="$(check_file "$tmp" "docs/fixture.md" 2>&1)"; then
    echo "SELF-TEST FAIL: check_file did not detect the dangling link" >&2
    self_test_fail=1
  else
    if [[ "$out" == *"missing.md"* ]]; then
      echo "  ok: dangling link to missing.md correctly flagged"
    else
      echo "SELF-TEST FAIL: expected a report mentioning missing.md, got: $out" >&2
      self_test_fail=1
    fi
    if [[ "$out" == *"does-not-exist.md"* ]]; then
      echo "SELF-TEST FAIL: fenced-code-block example was incorrectly flagged" >&2
      self_test_fail=1
    else
      echo "  ok: fenced-code-block example correctly NOT flagged"
    fi
    if [[ "$out" == *"real.md"* ]]; then
      echo "SELF-TEST FAIL: valid link to real.md was incorrectly flagged" >&2
      self_test_fail=1
    else
      echo "  ok: valid link to real.md correctly NOT flagged"
    fi
    if [[ "$out" == *"workspace"* ]]; then
      echo "SELF-TEST FAIL: templated placeholder was incorrectly flagged" >&2
      self_test_fail=1
    else
      echo "  ok: templated placeholder correctly NOT flagged"
    fi
  fi

  # Now correct the fixture and assert a clean pass.
  cat >"$tmp/docs/fixture.md" <<'EOF'
# Fixture

A valid link: [real](real.md).
EOF

  echo "check-dangling-links --self-test: asserting the checker passes once corrected..."
  if ! check_file "$tmp" "docs/fixture.md"; then
    echo "SELF-TEST FAIL: check_file still fails after the fixture was corrected" >&2
    self_test_fail=1
  else
    echo "  ok: clean fixture passes"
  fi

  if [[ "$self_test_fail" -ne 0 ]]; then
    echo "" >&2
    echo "check-dangling-links --self-test: FAIL — the checker's discriminating power has regressed." >&2
    return 1
  fi

  echo "check-dangling-links --self-test: OK."
  return 0
}

# --- Self-test: destination-aware `defaults/**` mode -----------------------
# Regression-tests the exact bug class from issue #6321: a template file
# whose links are correct once installed but dangling if resolved literally
# at the template's own source-repo path (and vice versa).
run_self_test_installed_destination_mode() {
  local tmp
  tmp="$(mktemp -d)"
  trap 'rm -rf "$tmp"' RETURN

  local self_test_fail=0

  # --- Case 1: a `defaults/.claude/commands/loom/*.md`-shaped file, checked
  # with dir_override set to its INSTALLED directory ("a/b/c" — 3 levels
  # deep, matching the real `.claude/commands/loom/` case). A naive,
  # not-rebased link is dangling once resolved against that deeper installed
  # directory; the properly-rebased `../../../` form (the actual fix applied
  # to defaults/.claude/commands/loom/builder.md in #6321) resolves
  # correctly — including through the "a/b/c" path segments, which do not
  # physically exist in this synthetic fixture at all (mirrors a fresh
  # worktree with no dogfood `.claude/commands/loom` symlink).
  mkdir -p "$tmp/shared"
  echo "# Real doc" >"$tmp/shared/real.md"

  cat >"$tmp/role-fixture.md" <<'EOF'
# Role fixture

A naive (wrong once installed 3 dirs deep) link: [bad](shared/real.md).

A correctly rebased link: [good](../../../shared/real.md).
EOF

  echo "check-dangling-links --self-test (installed-destination mode): asserting a naive not-rebased link is flagged once resolved against a deeper installed directory, but a correctly rebased link is not..."
  local out
  if out="$(check_file "$tmp" "role-fixture.md" "a/b/c" 0 2>&1)"; then
    echo "SELF-TEST FAIL: expected the naive link to be flagged as dangling" >&2
    self_test_fail=1
  elif [[ "$out" == *"'shared/real.md'"* && "$out" != *"'../../../shared/real.md'"* ]]; then
    echo "  ok: naive link flagged, correctly-rebased link not flagged"
  else
    echo "SELF-TEST FAIL: unexpected report: $out" >&2
    self_test_fail=1
  fi

  # --- Case 2: a `defaults/.loom/CLAUDE.md`-shaped fixture — a
  # repo-root-relative `](.loom/docs/...)` link that is WRONG if resolved
  # literally against the installed `.loom/` directory (localize=0), but
  # correct once the localize-rewrite (localize=1) strips the `.loom/`
  # prefix, mirroring `localize_dotloom_doc_links()`.
  mkdir -p "$tmp/installed/.loom/docs"
  echo "# Daemon reference" >"$tmp/installed/.loom/docs/daemon-reference.md"

  cat >"$tmp/claude-md-fixture.md" <<'EOF'
# CLAUDE.md fixture

See [daemon-reference](.loom/docs/daemon-reference.md).
EOF

  echo "check-dangling-links --self-test (installed-destination mode): asserting the .loom/CLAUDE.md-style link is dangling without the localize rewrite, but resolves with it..."
  if check_file "$tmp/installed" "../claude-md-fixture.md" ".loom" 0 >/dev/null 2>&1; then
    echo "SELF-TEST FAIL: expected the un-localized link to be flagged as dangling" >&2
    self_test_fail=1
  else
    echo "  ok: un-localized link correctly flagged as dangling"
  fi
  if ! check_file "$tmp/installed" "../claude-md-fixture.md" ".loom" 1 >/dev/null 2>&1; then
    echo "SELF-TEST FAIL: expected the localize-rewritten link to resolve cleanly" >&2
    self_test_fail=1
  else
    echo "  ok: localize-rewritten link correctly resolves"
  fi

  if [[ "$self_test_fail" -ne 0 ]]; then
    echo "" >&2
    echo "check-dangling-links --self-test (installed-destination mode): FAIL." >&2
    return 1
  fi

  echo "check-dangling-links --self-test (installed-destination mode): OK."
  return 0
}

# --- Entry point -------------------------------------------------------------
if [[ "${1:-}" == "--self-test" ]]; then
  self_test_status=0
  run_self_test || self_test_status=1
  echo ""
  run_self_test_installed_destination_mode || self_test_status=1
  exit "$self_test_status"
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

# See the header comment: most of `defaults/**` is still skipped structurally
# — its install destination renames the subtree in a way this script does not
# model (`defaults/docs/*.md` is separately covered by
# check-docs-defaults-parity.sh; the rest — hooks/, roles/, scripts/,
# runtimes/, optional/, config/, and loose top-level files — is unaudited).
SKIP_PREFIXES=("defaults/")

# `defaults/**` subtrees that map to their installed destination via a plain
# "defaults/" prefix strip (no rename) — opted INTO the destination-aware
# check below instead of being skipped. See the header comment.
DIRECT_MAP_PREFIXES=("defaults/.loom/" "defaults/.claude/" "defaults/.github/")

# The two files that get `localize_dotloom_doc_links()` applied to their
# link targets at install time (loom-daemon/src/init/templates.rs, #5975 /
# PR #6001) — see the header comment.
needs_localize() {
  [[ "$1" == "defaults/.loom/CLAUDE.md" || "$1" == "defaults/.loom/AGENTS.md" ]]
}

is_skipped() {
  local relfile="$1" prefix
  for prefix in "${SKIP_PREFIXES[@]}"; do
    [[ "$relfile" == "$prefix"* ]] && return 0
  done
  return 1
}

# Returns the direct-map prefix `relfile` falls under via stdout, or empty +
# non-zero exit if it isn't under any of DIRECT_MAP_PREFIXES.
direct_map_prefix_for() {
  local relfile="$1" prefix
  for prefix in "${DIRECT_MAP_PREFIXES[@]}"; do
    if [[ "$relfile" == "$prefix"* ]]; then
      echo "$prefix"
      return 0
    fi
  done
  return 1
}

overall_fail=0
files_checked=0

while IFS= read -r -d '' relfile; do
  if prefix="$(direct_map_prefix_for "$relfile")"; then
    # Installed path = "defaults/" stripped (the mapped subtrees rename
    # nothing beyond that — see DIRECT_MAP_PREFIXES above).
    installed_relfile="${relfile#defaults/}"
    localize_flag=0
    needs_localize "$relfile" && localize_flag=1
    files_checked=$((files_checked + 1))
    check_file "$ROOT" "$relfile" "$(dirname "$installed_relfile")" "$localize_flag" || overall_fail=1
    continue
  fi
  is_skipped "$relfile" && continue
  files_checked=$((files_checked + 1))
  check_file "$ROOT" "$relfile" || overall_fail=1
done < <(cd "$ROOT" && git ls-files -z -- '*.md')

if [[ "$overall_fail" -ne 0 ]]; then
  {
    echo ""
    echo "check-dangling-links: FAIL — see violations above."
    echo ""
    echo "A relative markdown link/image target must resolve to a real file or"
    echo "directory in the repo (checked against git-tracked *.md files only). Fix"
    echo "the path, or rewrite the link to an absolute https://github.com/... URL if"
    echo "the target is genuinely elsewhere."
  } >&2
  exit 1
fi

echo "check-dangling-links: OK — checked ${files_checked} tracked markdown file(s), no dangling internal links found."
exit 0
