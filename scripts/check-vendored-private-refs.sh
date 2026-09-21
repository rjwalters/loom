#!/usr/bin/env bash
# check-vendored-private-refs.sh — fail if a scanned tree names a repository or
# host that is not a public/placeholder identifier.
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
# Scanned by default (#7814): defaults/ (the copy-installed surface above) AND
# loom-daemon/src/fleet/ — the module that renders a provisioned host's config
# from compiled-in defaults. That second tree is the same failure in daemon
# code rather than vendored prose: `fleet add-worker`'s egress defaults used to
# bake in this fleet's ingest endpoint and scrub list, so a fork provisioned a
# worker publishing to an unrelated operator's endpoint. The rest of
# loom-daemon/src/ is NOT scanned yet — it still carries operator-named test
# fixtures (token-pool account names and the like), which are test-only data
# rather than shipped defaults and are a separate scrub.
#
# Usage:
#   check-vendored-private-refs.sh [--root <dir>]...   # repeatable
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

# Hosts permitted anywhere in a scanned tree: public services Loom actually
# talks to or cites, plus the example.* / test.* placeholder families. Matched
# case-insensitively, as a suffix (so `api.github.com` is covered by
# `github.com`). Adding a genuinely public host here is the intended way to
# extend this list; an operator-owned deployment hostname is not.
ALLOWED_HOST_SUFFIXES=(
  example.com example.net example.org
  test.com t.com
  github.com githubusercontent.com
  anthropic.com claude.com
  openai.com
  opentelemetry.io
  apple.com
  npmjs.org crates.io
  biomejs.dev workers.dev
  developercertificate.org
  percy.io
  ghcr.io
  # Package/source origins the fleet bootstrap plan fetches from (#7814).
  tailscale.com
  sf.net
)

usage() {
  sed -n '2,38p' "$0" | sed 's/^# \{0,1\}//'
}

ROOTS=()
SELF_TEST=0

while [[ $# -gt 0 ]]; do
  case "$1" in
    --root)      ROOTS+=("${2:-}"); shift 2 ;;
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

# scan_matches <ere-pattern> <dir> -> prints `file:lineno:match` lines
#
# This check is about what defaults/ SHIPS — i.e. committed content (#6190).
# Untracked or gitignored local state that happens to sit under defaults/ is
# never copy-installed anywhere, so it must not be able to fail the check: a
# stray runtime log dropped there by a locally-invoked guard hook used to do
# exactly that on long-lived fleet checkouts (#7882). So when the tree is
# inside a git work tree, scan `git ls-files` instead of walking the
# filesystem; otherwise (a --self-test fixture, or defaults/ extracted outside
# git) fall back to the raw recursive walk so the check still works.
#
# Symlinks are skipped, matching `grep -r`'s traversal semantics — defaults/
# carries symlinks (roles/*.md -> .claude/commands/loom/*.md) whose targets are
# themselves tracked, and following both would report every violation twice.
scan_matches() {
  local pattern="$1" dir="$2"
  local top prefix f
  local -a files=()

  if top="$(git -C "$dir" rev-parse --show-toplevel 2>/dev/null)" && [[ -n "$top" ]]; then
    prefix="$(git -C "$dir" rev-parse --show-prefix 2>/dev/null || true)"
    while IFS= read -r -d '' f; do
      [[ -L "$top/$f" ]] && continue
      files+=("$f")
    done < <(git -C "$top" ls-files -z -- "${prefix:-.}" 2>/dev/null || true)
    [[ ${#files[@]} -eq 0 ]] && return 0
    # The trailing /dev/null forces grep to prefix every match with its
    # filename even when the list happens to hold exactly one file.
    ( cd "$top" && grep -noE "$pattern" -- "${files[@]}" /dev/null 2>/dev/null ) || true
    return 0
  fi

  grep -rnoE "$pattern" "$dir" 2>/dev/null || true
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
  done < <(scan_matches '[A-Za-z0-9][A-Za-z0-9._-]*/[A-Za-z0-9._-]*[A-Za-z0-9]#[0-9]+' "$dir")

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
  done < <(scan_matches '\b[a-zA-Z0-9]([a-zA-Z0-9-]*[a-zA-Z0-9])?(\.[a-zA-Z0-9]([a-zA-Z0-9-]*[a-zA-Z0-9])?)*\.(com|net|org|io|dev)\b' "$dir")

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

  # Git-scoped fixture (#7882): inside a work tree the scan follows
  # `git ls-files`, so a TRACKED private ref still fails while an UNTRACKED one
  # (local-only state that is never copy-installed anywhere — a stray runtime
  # log, an editor scratch file) is ignored.
  mkdir -p "$tmp/gitrepo/defaults/docs"
  git -C "$tmp/gitrepo" init -q >/dev/null 2>&1 || true
  cat > "$tmp/gitrepo/defaults/docs/tracked.md" <<'EOF'
See example-org/tool-repo#202, hosted at dashboard.example.com.
EOF
  git -C "$tmp/gitrepo" add defaults/docs/tracked.md >/dev/null 2>&1 || true
  cat > "$tmp/gitrepo/defaults/docs/untracked.md" <<'EOF'
This local-only file names PrivateOrg/secret-repo#56 at dashboard.privateorg.com.
EOF
  if scan_tree "$tmp/gitrepo/defaults" >/dev/null; then
    echo "self-test: OK — untracked file under a git-tracked defaults/ is ignored"
  else
    echo "self-test: FAIL — untracked local state failed a check about committed content" >&2
    scan_tree "$tmp/gitrepo/defaults" >&2 || true
    fails=1
  fi

  cat > "$tmp/gitrepo/defaults/docs/tracked.md" <<'EOF'
This tracked file names PrivateOrg/secret-repo#56 at dashboard.privateorg.com.
EOF
  git -C "$tmp/gitrepo" add defaults/docs/tracked.md >/dev/null 2>&1 || true
  out="$(scan_tree "$tmp/gitrepo/defaults" || true)"
  if grep -q 'PrivateOrg/secret-repo#56' <<<"$out" \
     && grep -q 'dashboard.privateorg.com' <<<"$out"; then
    echo "self-test: OK — tracked file under a git-tracked defaults/ is still scanned"
  else
    echo "self-test: FAIL — tracked violation was not detected inside a work tree. Got:" >&2
    echo "$out" >&2
    fails=1
  fi

  # Repeatable --root (#7814): the check now scans several trees in one run
  # (defaults/ plus the daemon's fleet module), so a violation in ANY scanned
  # root must fail the whole run, and a clean run must name every root it
  # actually scanned.
  if multi_out="$("$0" --root "$tmp/clean" --root "$tmp/dirty" 2>&1)"; then
    echo "self-test: FAIL — a violation in the SECOND --root did not fail the run" >&2
    echo "$multi_out" >&2
    fails=1
  elif grep -q 'dashboard.privateorg.com' <<<"$multi_out"; then
    echo "self-test: OK — a violation in any scanned root fails the run"
  else
    echo "self-test: FAIL — multi-root run failed without reporting the violation. Got:" >&2
    echo "$multi_out" >&2
    fails=1
  fi

  if multi_clean="$("$0" --root "$tmp/clean" --root "$tmp/clean" 2>&1)"; then
    echo "self-test: OK — several clean roots pass in one run"
  else
    echo "self-test: FAIL — clean roots reported a violation. Got:" >&2
    echo "$multi_clean" >&2
    fails=1
  fi

  exit "$fails"
fi

# ---------------------------------------------------------------------------
# Main
# ---------------------------------------------------------------------------

if [[ ${#ROOTS[@]} -eq 0 ]]; then
  SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
  if REPO_ROOT="$(git -C "$SCRIPT_DIR" rev-parse --show-toplevel 2>/dev/null)"; then
    :
  else
    REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
  fi
  # defaults/ is the copy-installed surface (#6190); loom-daemon/src/fleet/ is
  # the daemon code that renders a provisioned host's config from compiled-in
  # defaults (#7814). See this file's header for why the rest of
  # loom-daemon/src/ is deliberately not scanned yet.
  ROOTS=("$REPO_ROOT/defaults" "$REPO_ROOT/loom-daemon/src/fleet")
fi

violations=""
scanned=()
for root in "${ROOTS[@]}"; do
  if [[ ! -d "$root" ]]; then
    echo "check-vendored-private-refs: no such directory: $root — nothing to check (ok)."
    continue
  fi
  scanned+=("$root")
  if ! out="$(scan_tree "$root")"; then
    violations+="$out"$'\n'
  fi
done

if [[ -z "${violations//[$'\n\t ']/}" ]]; then
  echo "check-vendored-private-refs: OK — no non-allowlisted repo/host identifiers under ${scanned[*]:-<nothing>}."
  exit 0
fi

{
  echo "check-vendored-private-refs: FAIL — a scanned tree names identifiers that are not public or placeholder:"
  echo ""
  printf '%s' "$violations"
  echo ""
  echo "Everything under defaults/ is copy-installed into every consumer repo, so"
  echo "these identifiers ship to every repo Loom is installed on — and a fix made"
  echo "downstream reverts on the next resync (#6190). Everything under"
  echo "loom-daemon/src/fleet/ is a compiled-in default a fork inherits when it"
  echo "provisions a host (#7814)."
  echo ""
  echo "Fix: genericize the identifier, keeping the narrative's instructional value:"
  echo "  private-org/private-repo#56  ->  example-org/tool-repo#202"
  echo "  dashboard.private-org.com    ->  dashboard.example.com"
  echo "  <operator machine name>      ->  studio-host / laptop-host"
  echo ""
  echo "For daemon code, an operator-specific value belongs in an operator-supplied"
  echo "input (a required flag / an overlay), not a compiled-in default."
  echo ""
  echo "If an identifier is genuinely public and belongs here, add it to"
  echo "ALLOWED_OWNERS / ALLOWED_HOST_SUFFIXES at the top of this script."
} >&2
exit 1
