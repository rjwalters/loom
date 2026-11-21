#!/usr/bin/env bash
# Installed entry point. The binary must come from a trusted CI checkout or pin.
# requires-daemon: ci-docs-only optional   Older daemons cannot prove a skip and run full suites.
set -euo pipefail

diagnostics=$(mktemp)
trap 'rm -f -- "$diagnostics"' EXIT

if decision=$("${LOOM_DAEMON_BIN:-loom-daemon}" ci-docs-only "$@" 2>"$diagnostics"); then
    cat "$diagnostics" >&2
    printf '%s\n' "$decision"
else
    result=$?
    if [[ $result == 2 && -z $decision ]] &&
        IFS= read -r first_line < "$diagnostics" &&
        [[ $first_line == "error: unrecognized subcommand 'ci-docs-only'" ]]; then
        printf '%s\n' 'CI_DOCS_ONLY docs_only=false reason=command_unavailable' >&2
        printf '%s\n' 'docs_only=false'
    else
        cat "$diagnostics" >&2
        if [[ -n $decision ]]; then
            printf '%s\n' "$decision"
        fi
        exit "$result"
    fi
fi
