#!/usr/bin/env bash
# check-workflow-self-repo.sh -- pin the canonical self-repository identity used
# by workflow `if:` guards (#14158).
#
# After the transfer to 2AMLogic/loom, guards comparing github.repository to the
# former owner evaluate false and silently skip publishing. Each guard must name
# the canonical repo (fork refusal is preserved: a fork's github.repository
# differs). GHCR image refs (ghcr.io/rjwalters/...) are deliberately NOT checked:
# package ownership is independent of repo ownership.
#
# Usage: check-workflow-self-repo.sh [--self-test]
set -euo pipefail
CANON="2AMLogic/loom"
FORMER="rjwalters/loom"

check() { # <file>... ; exits nonzero on violation
  local bad=0 f n
  for f in "$@"; do
    if grep -nE "github\.repository *[!=]= *'${FORMER}'" "$f"; then
      echo "FAIL: $f compares github.repository to former owner ${FORMER}" >&2; bad=1
    fi
  done
  for spec in ".github/workflows/ci.yml:1" ".github/workflows/release.yml:4"; do
    f=${spec%%:*}; want=${spec##*:}
    n=$(grep -cE "github\.repository == '${CANON}'" "${ROOT:-.}/$f" || true)
    if [ "$n" -ne "$want" ]; then
      echo "FAIL: $f has $n canonical-repo guards, expected $want" >&2; bad=1
    fi
  done
  return "$bad"
}

if [ "${1:-}" = "--self-test" ]; then
  d=$(mktemp -d); trap 'rm -rf "$d"' EXIT
  mkdir -p "$d/.github/workflows"
  printf "if: github.repository == '%s'\n" "$FORMER" > "$d/.github/workflows/ci.yml"
  : > "$d/.github/workflows/release.yml"
  if ROOT="$d" check "$d/.github/workflows/ci.yml" 2>/dev/null; then
    echo "self-test FAIL: former-owner guard not rejected" >&2; exit 1
  fi
  echo "self-test ok"; exit 0
fi

check .github/workflows/ci.yml .github/workflows/release.yml
echo "ok: workflow self-repo guards name ${CANON}"
