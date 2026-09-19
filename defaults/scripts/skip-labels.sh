#!/usr/bin/env bash
# skip-labels.sh — combined "not a work item" label list for a role prompt's
# unfiltered fallback query (Issue #8255).
#
# THIN STUB. The implementation is `loom-daemon skip-labels` (Rust,
# `loom-daemon/src/cli/skip_labels.rs`) — see that module for why this exists:
# `hard-exclusion-labels.sh` is a fixed fleet-wide list a repo cannot extend,
# while `autonomous.workFinder.extraSkipLabels` (#6685) is the per-repo knob
# the daemon's own work finder already reads. This subcommand is the missing
# shell-facing union of both, e.g. 2AMLogic/2am's `journal` status label
# (upstream 2am#582/#625).
#
# Usage:
#   skip-labels.sh [--lines|--json|--jq-not|--search] [--repo-root PATH]
#
#     --lines      (default) one label name per line
#     --json       a JSON array, e.g. ["external","journal"]
#     --jq-not     a jq boolean expression TRUE when an issue carries none of
#                  the labels, for `gh issue list --jq 'select(...)'`
#     --search     gh/forge search qualifiers excluding the labels, e.g.
#                  -label:"external" -label:"journal"
#     --repo-root  defaults to the current directory
#
# With no `autonomous.workFinder.extraSkipLabels` configured, output is
# byte-identical to `hard-exclusion-labels.sh`'s.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=lib/locate-daemon-bin.sh
source "$SCRIPT_DIR/lib/locate-daemon-bin.sh"

REPO_ROOT="$(git -C "$SCRIPT_DIR" rev-parse --show-toplevel 2>/dev/null || pwd)"
BIN="$(loom_locate_daemon_bin "$REPO_ROOT")"

if [[ -z "$BIN" ]]; then
    echo "[ERROR] loom-daemon not found (needed for 'skip-labels')." >&2
    echo "Build it: cargo build --release --manifest-path loom-daemon/Cargo.toml" >&2
    echo "Or set LOOM_DAEMON_BIN to an existing binary." >&2
    exit 1
fi

exec "$BIN" skip-labels "$@"
