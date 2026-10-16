#!/usr/bin/env bash
# prompt-tree-session.sh — SessionStart hook: record how far this attended
# session's checkout is behind GitHub's tip (issue #12509).
#
# Wired at user scope (scripts/install/provision-hooks.sh), matcher "" so it
# covers startup, resume, clear and compact. A thin entry point: the logic is
# `loom-daemon prompt-tree session-start`, which reads the hook JSON on stdin
# for `session_id`, `cwd` and `source` only, then starts a detached copy of
# itself that does the network reads and posts one `session.prompt_tree`
# record. Nothing it does can block the session.
#
# Always exits 0 and prints nothing (SessionStart stdout is injected into the
# model's context): a missing or older binary must never block a session.
# Inert for a daemon-dispatched agent (LOOM_DAEMON_ITEM_ID); role ticks already
# record their own prompt tree.
#
# requires-daemon: prompt-tree optional   #12509 — an older daemon exits 2 on the unknown subcommand; the hook swallows it and no record is sent.
[[ -n "${LOOM_DAEMON_ITEM_ID:-}" ]] && exit 0
_bin="${LOOM_PROMPT_TREE_BIN:-}"
_lib="$(cd "$(dirname "${BASH_SOURCE[0]}")/../scripts/lib" 2>/dev/null && pwd)/locate-daemon-bin.sh"
if [[ -z "$_bin" && -r "$_lib" ]] && source "$_lib" 2>/dev/null; then
    _bin="$(loom_resolve_self_daemon_bin 2>/dev/null)" || _bin=""
fi
_bin="${_bin:-loom-daemon}"
# Output is discarded so the hook never prints into the session; failures of
# the detached half land in <root>/.loom/logs/prompt-tree/last.log.
"$_bin" prompt-tree session-start >/dev/null 2>&1
exit 0
