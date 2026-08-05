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
# `defaults/` (README.md, docs/**, .loom/docs/**, .loom/roles/**, etc.) — it
# does not replace check-docs-defaults-parity.sh's shipped-tree-specific
# parity and vendored-root checks.
#
# `defaults/**` is excluded structurally (see SKIP_PREFIXES below): every file
# under `defaults/` is an install-time template that `install.sh` /
# `resync-installed.sh` copies into a CONSUMER repo's tree (e.g.
# `defaults/.loom/CLAUDE.md` -> a consumer's `CLAUDE.md`,
# `defaults/.claude/commands/loom/*.md` -> a consumer's
# `.claude/commands/loom/*.md`). Its relative links are written to resolve
# post-install, at the consumer's path, not at the literal path the template
# lives at in this source repo — resolving them literally here produces
# guaranteed false positives (e.g. `defaults/.loom/CLAUDE.md` links
# `.loom/docs/daemon-reference.md` relative to a future repo root, not to
# `defaults/.loom/`). `defaults/docs/*.md` is the one subtree of `defaults/`
# whose own link semantics ARE already checked, by
# check-docs-defaults-parity.sh, which is deliberately relative-root-aware.
#
# What it checks:
#   For every git-tracked *.md file outside `defaults/`, every markdown-style
#   link/image target `[text](target)` / `![alt](target)` whose target is NOT
#   an absolute URL (http(s)://, mailto:), NOT a bare "#anchor" fragment, and
#   NOT an unresolvable templated placeholder (contains a literal "<" — e.g.
#   "<workspace>" in prose-as-example text) is resolved relative to the
#   linking file's own directory (after stripping any "#anchor" suffix and a
#   leading "./"). If the resolved path does not exist on disk (file OR
#   directory), that's a dangling link. Text inside fenced code blocks
#   (``` ... ```) is skipped, since those commonly show illustrative link
#   syntax or example paths that are not meant to resolve.
#
#   Only git-tracked files are scanned (`git ls-files`), so this
#   automatically respects .gitignore — build outputs, node_modules/,
#   target/, .loom/worktrees/, etc. are never walked. Directory-symlinked
#   trees (e.g. `.loom/roles -> ../defaults/roles`, `.loom/scripts ->
#   ../defaults/scripts`) are never traversed by `git ls-files` either — git
#   tracks the symlink itself as one entry, not its target's contents — so
#   they are naturally out of scope here too (their real files live under
#   `defaults/`, already excluded above).
#
# Usage:
#   check-dangling-links.sh [ROOT]
#     ROOT  Repository root to scan. Defaults to `git rev-parse
#           --show-toplevel`, then the script's own repo root.
#
#   check-dangling-links.sh --self-test
#     Runs an isolated, synthetic-fixture regression test (a dangling
#     relative link, a fenced-code-block example that must NOT be flagged,
#     and a valid link) and asserts the checker discriminates correctly.
#     Does not touch the real repo tree.
#
# Exit codes: 0 = clean (or self-test passed); 1 = dangling link(s) found (or
# self-test failed) — details printed to stderr as `file:line`.

set -euo pipefail

# --- Core check ----------------------------------------------------------
# Scans one markdown file for dangling relative links. Prints violations to
# stderr as "DANGLING-LINK: <file>:<line> ...". Returns 0 if clean, 1 if any
# dangling link was found.
check_file() {
  local root="$1" relfile="$2"
  local f="$root/$relfile"
  local dir fail=0
  dir="$(dirname "$relfile")"

  # Single awk pass per file (not a per-line subprocess spawn — with ~225
  # tracked markdown files and some multi-thousand-line docs, a per-line
  # `read` + `grep` pipeline measured minutes; this is sub-second). Emits
  # "lineno<TAB>target" for every "](target)" match on a line that is not
  # inside a fenced code block (``` or ~~~ toggles fence state, mirroring
  # GitHub's own fencing rule of "any line starting with the fence marker,
  # ignoring leading whitespace").
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

    resolved="${dir}/${path_part}"

    if [[ ! -e "$root/$resolved" ]]; then
      echo "DANGLING-LINK: ${relfile}:${lineno} links '${path_part}' which does not resolve to an existing file/dir (resolved: ${resolved})" >&2
      echo "  line: $(sed -n "${lineno}p" "$f" | sed 's/^[[:space:]]*//')" >&2
      fail=1
    fi
  done < <(awk '
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
  ' "$f")

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

# --- Entry point -------------------------------------------------------------
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

# See the header comment: `defaults/**` is an install-time template tree
# whose relative links resolve post-install at a consumer's path, not at this
# literal path — excluded structurally, not skipped as a false-positive
# allowlist. `defaults/docs/*.md` link validity is already covered by
# check-docs-defaults-parity.sh.
SKIP_PREFIXES=("defaults/")

is_skipped() {
  local relfile="$1" prefix
  for prefix in "${SKIP_PREFIXES[@]}"; do
    [[ "$relfile" == "$prefix"* ]] && return 0
  done
  return 1
}

overall_fail=0
files_checked=0

while IFS= read -r -d '' relfile; do
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
