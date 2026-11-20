#!/usr/bin/env bash
# Installed entry point. The binary must come from a trusted CI checkout or pin.
exec "${LOOM_DAEMON_BIN:-loom-daemon}" ci-docs-only "$@"
