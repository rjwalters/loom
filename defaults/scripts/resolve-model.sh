#!/bin/bash

# resolve-model.sh - Thin stub delegating to loom_tools.model_tiers (#3982)
#
# The /loom:sweep skill shells out to this to resolve a *logical* model tier
# (`opus`, `sonnet`, `sonnet@xhigh`) to the concrete model ID it should dispatch
# on the wire, BEFORE it passes the model to a subagent Task. This is the single
# indirection point that fixes the non-monotonic escalation ladder: every rung,
# the tier-2.5 bump, the Judge/refusal fallbacks, and the experiment's Arm A keep
# naming the alias `opus`, and exactly one place decides that `opus` means
# `claude-opus-5` (the CLI's own `opus` alias still resolves to a gen-4 model).
#
# The mapping is configurable in `.loom/config.json` -> `sweep.modelAliases`, so
# an operator can repoint a tier (or drop the pin once the CLI alias rolls to
# gen-5) with no code change. Unknown aliases and pinned IDs (`claude-sonnet-4-6`)
# pass through unchanged.
#
# Usage:
#   resolve-model.sh <tier|alias|id>            # prints the resolved model ID
#   resolve-model.sh <tier> --generation        # prints the resolved generation number
#   resolve-model.sh <tier> --config <path>     # explicit .loom/config.json path
#
# Examples:
#   resolve-model.sh opus            # -> claude-opus-5
#   resolve-model.sh sonnet@xhigh    # -> sonnet@xhigh   (passthrough; CLI resolves)
#   resolve-model.sh claude-sonnet-4-6   # -> claude-sonnet-4-6 (pinned ID, unchanged)

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

# Source shared loom-tools helper
source "$SCRIPT_DIR/lib/loom-tools.sh"

# Run the command with proper fallback chain
run_loom_tool "resolve-model" "model_tiers" "$@"
