#!/usr/bin/env bash
# post-comment.sh — the ONE shell/agent entry point for posting an issue or PR
# comment (#9774).
#
# Usage:
#   post-comment.sh <NUMBER> (--body TEXT | --body-file PATH) [--repo OWNER/REPO] [--pr]
#
#     --body-file -   reads the body from stdin
#     --pr            NUMBER names a pull request (the dashboard link says
#                     `/pull/N`; the POST endpoint is the same either way)
#     --repo          defaults to the checkout's `origin` remote
#
# THIN STUB. The implementation is `loom-daemon forge comment` (Rust,
# `loom-daemon/src/forge_comment.rs`, #9772): it appends the dashboard footer
# (`<!-- loom:dashboard-link -->`) and POSTs to
# `repos/{owner}/{repo}/issues/{N}/comments`. Per ADR-0018 the behaviour owns
# there and this file only reaches it — do not grow policy here.
#
# WHY THE FALLBACK IS NOT OPTIONAL
#
# Posting a comment is how a sweep reports what it did; the one host state where
# that matters most is the host whose binary is unbuilt, mid-upgrade, or being
# replaced. So a missing binary — or one predating the verb, which is the same
# thing from here — degrades to the shell transport
# (`forge_gh_comment_rl_safe`), which appends the identical footer bytes from
# lib/dashboard-link.sh. The probe is `forge comment --help`, not a trial POST:
# an exit-code-based fallback after a real attempt could double-post.
#
# requires-daemon: forge optional   `forge comment --help` is probed first; a missing, older, or unbuilt binary falls back to forge_gh_comment_rl_safe (lib/forge-helpers.sh), which appends the same footer (#9774)
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]:-$0}")" && pwd)"
# shellcheck source=lib/locate-daemon-bin.sh
source "$SCRIPT_DIR/lib/locate-daemon-bin.sh"

# `--body @path` posts the LITERAL string "@path" (it does not read the file) —
# the same foot-gun post-verdict.sh and the Bash guard refuse. Caught here too,
# because a call routed through this script is invisible to a guard that
# pattern-matches literal `gh ... comment` text (#6382).
_prev=""
for _arg in "$@"; do
  if [[ "$_prev" == "--body" && "$_arg" == @* ]]; then
    echo "post-comment.sh: --body @path posts the literal string, it does NOT read the file. Use --body-file <path> instead." >&2
    exit 2
  fi
  _prev="$_arg"
done

# QUIET=1: this runs on every comment, and the resolver's "resolved … via …"
# notice would bury the caller's own stderr in it.
BIN="$(loom_daemon_self_bin_override \
  || LOOM_LOCATE_DAEMON_BIN_QUIET=1 loom_locate_daemon_bin \
    "$(git -C "$SCRIPT_DIR" rev-parse --show-toplevel 2>/dev/null || pwd)")"
if [[ -z "$BIN" ]] || ! "$BIN" forge comment --help >/dev/null 2>&1; then
  NUMBER=""; REPO="${LOOM_REPO:-}"; BODY=""; KIND="issues"; HAVE_BODY=0
  while [[ $# -gt 0 ]]; do
    case "$1" in
      --repo) REPO="${2:-}"; shift 2 ;;
      --body) BODY="${2:-}"; HAVE_BODY=1; shift 2 ;;
      --body-file) BODY="$(cat -- "${2:-}")"; HAVE_BODY=1; shift 2 ;;
      --pr) KIND="pull"; shift ;;
      --) shift ;;
      *) NUMBER="$1"; shift ;;
    esac
  done
  if [[ -z "$NUMBER" || "$HAVE_BODY" -eq 0 ]]; then
    echo "post-comment.sh: usage: post-comment.sh <NUMBER> (--body TEXT | --body-file PATH) [--repo OWNER/REPO] [--pr]" >&2
    exit 2
  fi
  # shellcheck source=lib/forge-helpers.sh
  source "$SCRIPT_DIR/lib/forge-helpers.sh"
  echo "post-comment.sh: loom-daemon 'forge comment' unavailable — posting through the shell transport instead (#9774)" >&2
  forge_gh_comment_rl_safe "$REPO" "$NUMBER" "$BODY" "$KIND"
  exit $?
fi
exec "$BIN" forge comment "$@"
