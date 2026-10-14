#!/usr/bin/env bash
# Existing Dependabot label/manifest gate; behavior lives in the native command.
# Usage: check-dependabot-labels.sh [ROOT]
# requires-daemon: check-dependabot-labels >= 0.19.1154  Native checker introduced by #11816.
set -euo pipefail
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="${1:-}"
if [[ -z "$ROOT" ]]; then
  ROOT="$(git -C "$SCRIPT_DIR" rev-parse --show-toplevel 2>/dev/null)" || ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
fi
if [[ ! -e "$ROOT/.github/dependabot.yml" ]]; then
  echo "check-dependabot-labels: no .github/dependabot.yml under $ROOT — nothing to check (ok)."
  exit 0
fi
# shellcheck source=lib/script-helper.sh
source "$SCRIPT_DIR/lib/script-helper.sh"
loom_exec_script_helper check-dependabot-labels --repo-root "$ROOT"
