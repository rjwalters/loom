#!/usr/bin/env bash
# check-vendored-private-refs.sh — fail if defaults/ names a repository or host
# that is not a public/placeholder identifier.
#
# Why (#6190): every file under defaults/ is copy-installed into every consumer
# repo's .loom/{scripts,hooks,roles,docs,bin}/ + .claude/commands/loom/ tree. A
# prose incident narrative written here — "this is exactly what happened to
# some-private-org/some-repo#56" — therefore ships that private identifier into
# every repo Loom is installed on, and a fix made downstream silently reverts on
# the next resync. A /repo:scrub pass in one public consumer repo counted ~70
# such occurrences before this check existed.
#
# The fix is durable only if reintroduction is mechanically blocked, so this is
# a STRUCTURAL check rather than a denylist of specific names: it does not know
# (and must not encode) any private org, repo, or host name. It asserts the
# inverse — that every cross-repo issue reference and every hostname appearing
# under defaults/ belongs to a small allowlist of public or obviously-placeholder
# identifiers. Anything else fails, whoever it belongs to.
#
# Incident narratives keep their instructional value: genericize the identifiers
# (example-org/tool-repo#202, dashboard.example.com) and keep the story.
#
# Usage:
#   check-vendored-private-refs.sh [--root <dir>]
#   check-vendored-private-refs.sh --self-test
#   check-vendored-private-refs.sh --help
#
# Exit codes: 0 = clean; 1 = disallowed identifier found; 2 = bad usage.

set -euo pipefail

# ---------------------------------------------------------------------------
# Allowlists
# ---------------------------------------------------------------------------
#
# Owners permitted in an `owner/repo#N` cross-repo reference under defaults/:
# this repo's own org, the RFC-2606-style placeholders used by the genericized
# narratives, and the single-letter/obvious stand-ins used by the shell test
# fixtures ("no"/"nowner" are artifacts of `\n`-escaped fixture strings).
ALLOWED_OWNERS=(
  rjwalters
  example-org
  owner OWNER
  o a no nowner private
  my-org some-owner
)

# Hosts permitted anywhere under defaults/: public services Loom actually talks
# to or cites, plus the example.* / test.* placeholder families. Matched
# case-insensitively, as a suffix (so `api.github.com` is covered by
# `github.com`). Adding a genuinely public host here is the intended way to
# extend this list; an operator-owned deployment hostname is not.
ALLOWED_HOST_SUFFIXES=(
  example.com example.net example.org
  test.com t.com
  github.com githubusercontent.com
  anthropic.com claude.com
  openai.com
  apple.com
  npmjs.org crates.io
  biomejs.dev workers.dev
  developercertificate.org
  percy.io
  ghcr.io
)

usage() {
  sed -n '2,27p' "$0" | sed 's/^# \{0,1\}//'
}

ROOT=""
SELF_TEST=0

while [[ $# -gt 0 ]]; do
  case "$1" in
    --root)      ROOT="${2:-}"; shift 2 ;;
    --self-test) SELF_TEST=1; shift ;;
    --help|-h)   usage; exit 0 ;;
    *)
      echo "check-vendored-private-refs: unknown argument: $1" >&2
      exit 2
      ;;
  esac
done

# ---------------------------------------------------------------------------
# Scan
# ---------------------------------------------------------------------------

owner_allowed() {
  local owner="$1" a
  for a in "${ALLOWED_OWNERS[@]}"; do
    [[ "$owner" == "$a" ]] && return 0
  done
  return 1
}

host_allowed() {
  local host s
  host="$(printf '%s' "$1" | tr '[:upper:]' '[:lower:]')"
  for s in "${ALLOWED_HOST_SUFFIXES[@]}"; do
    [[ "$host" == "$s" || "$host" == *".$s" ]] && return 0
  done
  return 1
}

# scan_tree <defaults-dir> -> prints violations, returns 1 if any
scan_tree() {
  local dir="$1"
  local violations=0

  # 1. Cross-repo issue references: owner/repo#N.
  #    The repo half must end in an alphanumeric so prose like "anti-#4736"
  #    is not read as a reference.
  local line file lineno ref owner
  while IFS= read -r line; do
    [[ -z "$line" ]] && continue
    file="${line%%:*}"; line="${line#*:}"
    lineno="${line%%:*}"; ref="${line#*:}"
    owner="${ref%%/*}"
    if ! owner_allowed "$owner"; then
      echo "  $file:$lineno: cross-repo reference to a non-allowlisted owner: $ref"
      violations=1
    fi
  done < <(grep -rnoE '[A-Za-z0-9][A-Za-z0-9._-]*/[A-Za-z0-9._-]*[A-Za-z0-9]#[0-9]+' "$dir" 2>/dev/null || true)

  # 2. Hostnames.
  local host
  while IFS= read -r line; do
    [[ -z "$line" ]] && continue
    file="${line%%:*}"; line="${line#*:}"
    lineno="${line%%:*}"; host="${line#*:}"
    if ! host_allowed "$host"; then
      echo "  $file:$lineno: non-allowlisted hostname: $host"
      violations=1
    fi
  done < <(grep -rnoE '\b[a-zA-Z0-9]([a-zA-Z0-9-]*[a-zA-Z0-9])?(\.[a-zA-Z0-9]([a-zA-Z0-9-]*[a-zA-Z0-9])?)*\.(com|net|org|io|dev)\b' "$dir" 2>/dev/null || true)

  return $violations
}

# ---------------------------------------------------------------------------
# Self-test
# ---------------------------------------------------------------------------

if [[ "$SELF_TEST" -eq 1 ]]; then
  tmp="$(mktemp -d "${TMPDIR:-/tmp}/cvpr-selftest.XXXXXX")"
  trap 'rm -rf "$tmp"' EXIT
  fails=0

  # Clean fixture: only allowlisted identifiers.
  mkdir -p "$tmp/clean/docs"
  cat > "$tmp/clean/docs/a.md" <<'EOF'
See example-org/tool-repo#202 and rjwalters/loom#1, hosted at dashboard.example.com.
A same-repo ref (#4736) and prose like scheduling/anti-#4736 must not trip this.
EOF
  if scan_tree "$tmp/clean" >/dev/null; then
    echo "self-test: OK — clean fixture passes"
  else
    echo "self-test: FAIL — clean fixture reported a violation" >&2
    scan_tree "$tmp/clean" >&2 || true
    fails=1
  fi

  # Dirty fixture: a private cross-repo ref and a private host.
  mkdir -p "$tmp/dirty/docs"
  cat > "$tmp/dirty/docs/b.md" <<'EOF'
This is exactly what happened to PrivateOrg/secret-repo#56.
The live deployment is at dashboard.privateorg.com.
EOF
  out="$(scan_tree "$tmp/dirty" || true)"
  if grep -q 'PrivateOrg/secret-repo#56' <<<"$out" \
     && grep -q 'dashboard.privateorg.com' <<<"$out"; then
    echo "self-test: OK — dirty fixture reports both the private ref and the private host"
  else
    echo "self-test: FAIL — dirty fixture was not fully detected. Got:" >&2
    echo "$out" >&2
    fails=1
  fi

  exit "$fails"
fi

# ---------------------------------------------------------------------------
# Main
# ---------------------------------------------------------------------------

if [[ -z "$ROOT" ]]; then
  SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
  if REPO_ROOT="$(git -C "$SCRIPT_DIR" rev-parse --show-toplevel 2>/dev/null)"; then
    :
  else
    REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
  fi
  ROOT="$REPO_ROOT/defaults"
fi

if [[ ! -d "$ROOT" ]]; then
  echo "check-vendored-private-refs: no such directory: $ROOT — nothing to check (ok)."
  exit 0
fi

if out="$(scan_tree "$ROOT")"; then
  echo "check-vendored-private-refs: OK — no non-allowlisted repo/host identifiers under $ROOT."
  exit 0
fi

{
  echo "check-vendored-private-refs: FAIL — defaults/ names identifiers that are not public or placeholder:"
  echo ""
  echo "$out"
  echo ""
  echo "Everything under defaults/ is copy-installed into every consumer repo, so"
  echo "these identifiers ship to every repo Loom is installed on — and a fix made"
  echo "downstream reverts on the next resync (#6190)."
  echo ""
  echo "Fix: genericize the identifier, keeping the narrative's instructional value:"
  echo "  private-org/private-repo#56  ->  example-org/tool-repo#202"
  echo "  dashboard.private-org.com    ->  dashboard.example.com"
  echo "  <operator machine name>      ->  studio-host / laptop-host"
  echo ""
  echo "If an identifier is genuinely public and belongs here, add it to"
  echo "ALLOWED_OWNERS / ALLOWED_HOST_SUFFIXES at the top of this script."
} >&2
exit 1
