#!/usr/bin/env bash
# check-gitignore-convergence.sh — fail if the repo's own committed
# `.gitignore` Loom-managed block has drifted from what `loom-daemon
# update-gitignore` would generate from the current `EPHEMERAL_PATTERNS` set.
#
# Why (#5488, Epic #5038 Phase 2; motivated by #5014): #5014 found the
# managed gitignore block missing `.loom/account-health.json` — a hand
# discovery made well after the pattern was added to `EPHEMERAL_PATTERNS` in
# Rust but never regenerated into the committed `.gitignore`. Per #5038's
# reasoning, failing the introducing PR is strictly better than a periodic
# filer catching the class after the fact.
#
# `loom-daemon update-gitignore <workspace>` (loom-daemon/src/init/post_init.rs
# `update_gitignore()`, wired via loom-daemon/src/cli/misc_cmds.rs
# `handle_update_gitignore_command()`) already regenerates the marker-
# delimited block in place and is idempotent when converged — running it
# against an already-correct `.gitignore` is a byte-for-byte no-op. So this
# gate needs zero new Rust code: build the existing binary, run
# `update-gitignore` against the checked-out repo, then `git diff --exit-code
# .gitignore`. A nonzero exit means the committed block disagrees with
# `EPHEMERAL_PATTERNS` — exactly the class of gap #5014 found by hand.
#
# NOTE: on drift, this script leaves `.gitignore` REWRITTEN on disk (not
# restored) — in CI that's a throwaway checkout; for local use before
# committing, that rewritten file IS the fix, ready to `git add .gitignore`.
#
# Usage:
#   check-gitignore-convergence.sh [ROOT] [LOOM_DAEMON_BIN]
#     ROOT             Repo root whose .gitignore to check. Defaults to
#                       `git rev-parse --show-toplevel`, then the script's own
#                       repo root.
#     LOOM_DAEMON_BIN  Path to a built loom-daemon binary. Defaults to
#                       searching target/release/loom-daemon, then
#                       target/debug/loom-daemon under ROOT, then a
#                       `loom-daemon` found on PATH.
#
# Exit codes: 0 = clean (no drift); 1 = drift found, or no binary/`.gitignore`
# resolvable to check against.

set -euo pipefail

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

if [[ $# -ge 2 && -n "${2:-}" ]]; then
  LOOM_DAEMON_BIN="$2"
elif [[ -x "$ROOT/target/release/loom-daemon" ]]; then
  LOOM_DAEMON_BIN="$ROOT/target/release/loom-daemon"
elif [[ -x "$ROOT/target/debug/loom-daemon" ]]; then
  LOOM_DAEMON_BIN="$ROOT/target/debug/loom-daemon"
elif command -v loom-daemon >/dev/null 2>&1; then
  LOOM_DAEMON_BIN="$(command -v loom-daemon)"
else
  echo "check-gitignore-convergence: no built loom-daemon binary found (looked in" >&2
  echo "  \$ROOT/target/release, \$ROOT/target/debug, and PATH)." >&2
  echo "  Build it first: cargo build --package loom-daemon (add --release for CI)," >&2
  echo "  or pass an explicit path as the second argument." >&2
  exit 1
fi

if [[ ! -f "$ROOT/.gitignore" ]]; then
  echo "check-gitignore-convergence: no .gitignore at $ROOT — nothing to check (ok)."
  exit 0
fi

if ! git -C "$ROOT" rev-parse --show-toplevel >/dev/null 2>&1; then
  echo "check-gitignore-convergence: $ROOT is not inside a git repo — cannot diff .gitignore" >&2
  exit 1
fi

"$LOOM_DAEMON_BIN" update-gitignore "$ROOT" >/dev/null

if ! git -C "$ROOT" diff --exit-code -- .gitignore; then
  echo "" >&2
  echo "check-gitignore-convergence: FAIL — .gitignore's Loom-managed block does not" >&2
  echo "match what 'loom-daemon update-gitignore' generates from the current" >&2
  echo "EPHEMERAL_PATTERNS set (see loom-daemon/src/init/post_init.rs)." >&2
  echo "" >&2
  echo "  fix: the diff above IS the fix — 'git add .gitignore' and commit it (this" >&2
  echo "  script already rewrote the file on disk to the converged block)." >&2
  exit 1
fi

echo "check-gitignore-convergence: OK — .gitignore's Loom-managed block matches the current EPHEMERAL_PATTERNS set."
exit 0
