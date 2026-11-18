#!/bin/bash

# resolve-role-model.sh - Thin stub delegating to `loom-daemon
# resolve-role-model` (issue #13956).
#
# The ATTENDED subagent path's chain lookup: given a role name, walk the same
# precedence chain the `/loom:sweep` skill applies by hand (explicit --model >
# terminals[].roleConfig.model > .loom/roles/<role>.json suggestedModel >
# inherit) and print the resolved model plus — via --json — WHICH tier
# supplied it. A session spawning a `loom-<role>` subagent through the
# Task/Agent tool runs this immediately before the spawn and passes the value
# via the Task `model` parameter; exit 3 (or `source: "inherit"`) means
# nothing is configured, so the caller omits the parameter and the subagent
# inherits the parent session's model — the zero-config behavior is
# byte-for-byte unchanged.
#
# The shipped agent definitions carry NO hardcoded `model:` frontmatter (the
# operator declined hardcoded tiers, #13956): this dispatch-time lookup IS the
# mechanism, so a config edit (`.loom/config.json` or the per-host
# `.loom-local/local.json`) takes effect on the next spawn with no resync and
# no shipped-file edit.
#
# Usage:
#   resolve-role-model.sh <role>                    # prints the resolved model
#   resolve-role-model.sh <role> --task-alias       # nearest Task-tool alias (#4282)
#   resolve-role-model.sh <role> --json             # {"role","model","source"} record
#   resolve-role-model.sh <role> --model <m>        # explicit tier-1 override
#   resolve-role-model.sh <role> --config <path>    # explicit .loom/config.json path
#
# Exit codes: 0 resolved (or the --json record printed — always exit 0),
# 2 bad usage (clap), 3 nothing configured / no Task-passable alias, with NO
# output: the caller omits the `model` parameter.
#
# Examples:
#   resolve-role-model.sh judge            # -> opus          (roleConfig.model pin)
#   resolve-role-model.sh Judge            # -> opus          (case-insensitive)
#   resolve-role-model.sh builder --json   # -> {"role":"builder","model":"opus","source":"suggestedModel"}
#   resolve-role-model.sh auditor          # -> (exit 3; unconfigured role, inherit)
#   resolve-role-model.sh judge --task-alias
#                                          # -> opus (claude-opus-5 degraded for the Task tool)
#
# NOT handled here (deliberately): the sweep skill's dispatch policy — the
# escalation ladder, No-Fable-Judge, sweep.modelAliases resolution — stays in
# `sweep-execution-model.md` where the policy lives. This is the mechanical
# chain lookup only. Reasoning effort has no Task-tool knob (#4282), so an
# operator's roleEfforts setting remains a daemon-path setting; see
# `defaults/docs/model-selection.md` § "Attended subagents".

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

# shellcheck source=/dev/null
source "$SCRIPT_DIR/lib/script-helper.sh"

# `exec`s the native subcommand, so exit 3 ("nothing configured — omit the
# model param") reaches the caller unmodified. See lib/script-helper.sh.
# requires-daemon: resolve-role-model >= 0.19.1312   #13956 — the subcommand merged (2de465076) after v0.19.1311 was cut, so 0.19.1311 is the last version WITHOUT it and 0.19.1312 is the first to ship it (the classify-dependency-block #7952 precedent). Hard, not `optional`: this stub only execs, it never probes or degrades. Default LOOM_SCRIPT_HELPER_MISSING_RC (1) is left alone deliberately — the DATA code here is 3 ("nothing configured"), and 2 is the bad-argument code; 1 is used by neither, so the refusal is already unconfusable with an answer. Subcommand-level floor only, per the convention: a later FLAG still surfaces as clap's own argument error on a binary between the floor and that flag's release (#8484).
loom_exec_script_helper resolve-role-model "$@"
