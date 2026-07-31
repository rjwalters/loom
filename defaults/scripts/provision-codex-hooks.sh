#!/usr/bin/env bash
# provision-codex-hooks.sh — install / verify / remove Loom's managed Codex
# `pre_tool_use` hook inside a selected CODEX_HOME profile (issue #4495,
# epic #4489 Phase 6).
#
# ============================================================================
# WHAT IT DOES
# ============================================================================
#
# `$CODEX_HOME/hooks.json` is SHARED USER CONFIGURATION — an operator may have
# their own hooks in it, for events Loom never touches. This script therefore
# never overwrites the file wholesale. It parses it, merges exactly one
# Loom-owned `PreToolUse` entry (identified by the `guard-codex-bridge.sh`
# command plus a `--loom-hook-version` marker), validates the result, and
# replaces the file atomically with mode 0600.
#
#   install : add/update ONLY Loom's entry, preserving every other event,
#             group, and handler byte-for-byte.
#   remove  : delete ONLY Loom's entry (and any now-empty group/event it
#             leaves behind), preserving everything else.
#   verify  : report readiness WITHOUT mutating anything (exit 0 ready,
#             78 not ready).
#
# Credentials are never read, copied, parsed, or logged. `auth.json` is not
# touched by any subcommand. Only the profile DIRECTORY NAME is ever printed.
#
# ============================================================================
# HOOK TRUST (Codex 0.146.0) — WHY THIS FAILS CLOSED
# ============================================================================
#
# Codex persists hook trust as `hooks.state."<identity>".trusted_hash` in
# `$CODEX_HOME/config.toml`, established interactively (the TUI's hook-trust
# prompt) or waived with `--dangerously-bypass-hook-trust`.
#
# On 0.146.0 there is NO documented non-interactive command to establish it:
# `codex` exposes no `hooks` subcommand, and `codex doctor` reports no hook
# check (verified 2026-07-31 against the shipped 0.146.0 binary). Loom will not
# guess the identity string or the hash algorithm, and #4495's scope guards
# forbid `--dangerously-bypass-hook-trust`. So this script takes the second
# option #4495's acceptance criteria explicitly allow: **fail closed before
# mutable-role dispatch when trust cannot be verified.**
#
# `verify` therefore checks three things:
#
#   1. STRUCTURE — hooks.json contains Loom's managed entry at the expected
#      version, and the bridge it names is readable and points at the current
#      workspace's provisioned guard.
#   2. CODEX TRUST — `config.toml` carries at least one `hooks.state` entry
#      with a non-empty `trusted_hash`. This is the only Codex-owned trust
#      signal observable from outside the CLI; it proves the operator has been
#      through the trust prompt for this profile.
#   3. STALENESS — Loom's own receipt (`<CODEX_HOME>/loom-codex-hooks.json`,
#      non-secret, Loom-owned) pins the SHA-256 of the managed entry as it was
#      when it was last installed. If the entry has since changed, trust
#      established for the OLD entry cannot be assumed to cover the new one, so
#      readiness is STALE and mutable-role dispatch fails closed.
#
# Check 2 is deliberately labelled as a coarse signal in
# defaults/docs/guardrail-parity-codex.md: it proves "this profile has trusted
# some hook", not "this profile has trusted LOOM's hook". That imprecision is
# one of the reasons `defaults/runtimes/codex.json` stays at
# `hooks: partial` / `worktreeIsolation: partial`.
#
# ============================================================================
# USAGE
# ============================================================================
#
#   provision-codex-hooks.sh install --codex-home <dir> --workspace <dir>
#                                    [--bridge <path>] [--timeout <secs>]
#                                    [--matcher <pattern>]
#   provision-codex-hooks.sh verify  --codex-home <dir> [--workspace <dir>]
#                                    [--bridge <path>] [--json]
#   provision-codex-hooks.sh remove  --codex-home <dir>
#
# `--all-profiles` replaces `--codex-home` on any subcommand and applies it to
# EVERY pooled profile under the Codex profile root
# (`LOOM_CODEX_PROFILE_ROOT`, default `~/.loom/codex-profiles` — the same root
# `spawn-codex.sh` resolves `LOOM_CODEX_PROFILE` against, and the same one
# `loom-daemon accounts add codex` populates). This is how "the managed hook is
# installed in every selected pooled CODEX_HOME" is satisfied in one operation
# instead of a hand-written loop:
#
#   provision-codex-hooks.sh install --all-profiles --workspace "$PWD"
#   provision-codex-hooks.sh verify  --all-profiles --workspace "$PWD" --json
#
# A profile directory is any immediate subdirectory of the root. The exit code
# is the WORST of the per-profile results (so a single unready profile fails the
# whole `verify`), and `--json` emits one object per line (JSONL), never a
# credential or a path's contents.
#
# Exit codes:
#   0   success / ready
#   78  EX_CONFIG — not ready (verify), or an unusable configuration (install)
#   1   usage error
#
# Env:
#   LOOM_CODEX_HOOK_MATCHER   default tool matcher for the managed entry
#                             (default "*", Claude-compatible "all tools").
#   LOOM_CODEX_HOOK_TIMEOUT   per-call hook timeout in seconds (default 30).
#   LOOM_CODEX_PROFILE_ROOT   profile root for `--all-profiles`
#                             (default `~/.loom/codex-profiles`).

set -uo pipefail

RED='\033[0;31m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m'

log_info() { echo -e "${BLUE}[provision-codex-hooks]${NC} $*" >&2; }
log_warn() { echo -e "${YELLOW}[provision-codex-hooks] WARN${NC} $*" >&2; }
log_error() { echo -e "${RED}[provision-codex-hooks] ERROR${NC} $*" >&2; }

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

# Managed-entry contract. Bump LOOM_HOOK_VERSION whenever the bridge's wire
# behavior changes in a way that invalidates a previously-trusted entry.
LOOM_HOOK_VERSION=1
LOOM_HOOK_MARKER="guard-codex-bridge.sh"
RECEIPT_NAME="loom-codex-hooks.json"
CODEX_SCHEMA_PIN="0.146.0"

usage() {
    sed -n '2,/^set -uo/p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//' | sed '$d'
}

COMMAND="${1:-}"
[[ $# -gt 0 ]] && shift

CODEX_HOME_ARG=""
WORKSPACE_ARG=""
BRIDGE_ARG=""
MATCHER_ARG=""
TIMEOUT_ARG=""
JSON_OUT=0
ALL_PROFILES=0
PROFILE_ROOT_ARG=""

while [[ $# -gt 0 ]]; do
    case "$1" in
        --all-profiles) ALL_PROFILES=1; shift ;;
        --profile-root) PROFILE_ROOT_ARG="${2:-}"; shift 2 || shift ;;
        --profile-root=*) PROFILE_ROOT_ARG="${1#--profile-root=}"; shift ;;
        --codex-home) CODEX_HOME_ARG="${2:-}"; shift 2 || shift ;;
        --codex-home=*) CODEX_HOME_ARG="${1#--codex-home=}"; shift ;;
        --workspace) WORKSPACE_ARG="${2:-}"; shift 2 || shift ;;
        --workspace=*) WORKSPACE_ARG="${1#--workspace=}"; shift ;;
        --bridge) BRIDGE_ARG="${2:-}"; shift 2 || shift ;;
        --bridge=*) BRIDGE_ARG="${1#--bridge=}"; shift ;;
        --matcher) MATCHER_ARG="${2:-}"; shift 2 || shift ;;
        --matcher=*) MATCHER_ARG="${1#--matcher=}"; shift ;;
        --timeout) TIMEOUT_ARG="${2:-}"; shift 2 || shift ;;
        --timeout=*) TIMEOUT_ARG="${1#--timeout=}"; shift ;;
        --json) JSON_OUT=1; shift ;;
        -h|--help) usage; exit 0 ;;
        *) log_error "Unknown argument: $1"; exit 1 ;;
    esac
done

case "$COMMAND" in
    install|verify|remove) ;;
    -h|--help|"") usage; exit 0 ;;
    *) log_error "Unknown subcommand: $COMMAND"; usage >&2; exit 1 ;;
esac

if ! command -v jq >/dev/null 2>&1; then
    log_error "jq is required but not found on PATH."
    exit 78
fi

# --- fan out over every pooled profile ------------------------------------
#
# Re-invokes THIS script once per profile with an explicit --codex-home, so the
# single-profile logic below stays the only implementation. Done before any
# single-profile state is resolved, and it never recurses (the child call has no
# --all-profiles).
if [[ "$ALL_PROFILES" == "1" ]]; then
    _root="${PROFILE_ROOT_ARG:-${LOOM_CODEX_PROFILE_ROOT:-$HOME/.loom/codex-profiles}}"
    _root="${_root%/}"
    if [[ ! -d "$_root" ]]; then
        log_error "Codex profile root '$_root' does not exist. Create profiles first (loom-daemon accounts add codex <name>), or pass --profile-root."
        exit 78
    fi
    _worst=0
    _seen=0
    # `find -maxdepth 1 -type d` rather than a glob so a root with no
    # subdirectories does not leave a literal `*/` behind, and so a profile name
    # containing spaces survives (read -d '').
    while IFS= read -r -d '' _profile; do
        [[ "$_profile" == "$_root" ]] && continue
        _seen=$((_seen + 1))
        _child_args=("$COMMAND" --codex-home "$_profile")
        [[ -n "$WORKSPACE_ARG" ]] && _child_args+=(--workspace "$WORKSPACE_ARG")
        [[ -n "$BRIDGE_ARG" ]] && _child_args+=(--bridge "$BRIDGE_ARG")
        [[ -n "$MATCHER_ARG" ]] && _child_args+=(--matcher "$MATCHER_ARG")
        [[ -n "$TIMEOUT_ARG" ]] && _child_args+=(--timeout "$TIMEOUT_ARG")
        [[ "$JSON_OUT" == "1" ]] && _child_args+=(--json)
        _rc=0
        bash "${BASH_SOURCE[0]}" "${_child_args[@]}" || _rc=$?
        [[ "$_rc" -gt "$_worst" ]] && _worst="$_rc"
    done < <(find "$_root" -maxdepth 1 -type d -print0 2>/dev/null | sort -z)
    if [[ "$_seen" -eq 0 ]]; then
        log_error "No Codex profiles found under '$_root'."
        exit 78
    fi
    log_info "$COMMAND applied to $_seen Codex profile(s) under the pool root (worst exit: $_worst)."
    exit "$_worst"
fi

CODEX_HOME_DIR="${CODEX_HOME_ARG:-${CODEX_HOME:-}}"
if [[ -z "$CODEX_HOME_DIR" ]]; then
    log_error "--codex-home (or --all-profiles, or a CODEX_HOME in the environment) is required."
    exit 1
fi
CODEX_HOME_DIR="${CODEX_HOME_DIR%/}"
PROFILE_NAME="$(basename "$CODEX_HOME_DIR")"

HOOKS_FILE="$CODEX_HOME_DIR/hooks.json"
CONFIG_FILE="$CODEX_HOME_DIR/config.toml"
RECEIPT_FILE="$CODEX_HOME_DIR/$RECEIPT_NAME"

# Resolve the bridge. Default: the sibling of this script's hooks directory,
# which is `.loom/hooks/` in an installed repo and `defaults/hooks/` in the
# Loom checkout.
resolve_bridge() {
    local -a candidates=()
    [[ -n "$BRIDGE_ARG" ]] && candidates+=("$BRIDGE_ARG")
    [[ -n "$WORKSPACE_ARG" && -z "$BRIDGE_ARG" ]] && candidates+=("${WORKSPACE_ARG%/}/.loom/hooks/guard-codex-bridge.sh")
    [[ -z "$BRIDGE_ARG" ]] && candidates+=("$SCRIPT_DIR/../hooks/guard-codex-bridge.sh")
    local candidate dir
    for candidate in ${candidates[@]+"${candidates[@]}"}; do
        if [[ -r "$candidate" ]]; then
            dir="$(cd "$(dirname "$candidate")" 2>/dev/null && pwd -P)" || dir=""
            if [[ -n "$dir" ]]; then
                printf '%s/%s' "$dir" "$(basename "$candidate")"
                return
            fi
        fi
    done
    # Nothing readable: report the LAST candidate so the caller's error names a
    # concrete path. Indexed the bash-3.2-portable way — macOS ships bash 3.2,
    # where a negative subscript (`${candidates[-1]}`) is a "bad array
    # subscript" error, and these scripts run on the operator's Mac.
    local last=$(( ${#candidates[@]} - 1 ))
    [[ "$last" -ge 0 ]] && printf '%s' "${candidates[$last]}"
    return 0
}

BRIDGE="$(resolve_bridge)"

# The exact command string the managed entry carries. The workspace is baked in
# so a bridge shared by several repos still resolves the right project root,
# and the version marker makes Loom's entry self-identifying.
managed_command() {
    local cmd="$BRIDGE"
    if [[ -n "$WORKSPACE_ARG" ]]; then
        cmd="$cmd --project-root ${WORKSPACE_ARG%/}"
    fi
    printf '%s --loom-hook-version %s' "$cmd" "$LOOM_HOOK_VERSION"
}

sha256_of() {
    if command -v sha256sum >/dev/null 2>&1; then
        printf '%s' "$1" | sha256sum | awk '{print $1}'
    elif command -v shasum >/dev/null 2>&1; then
        printf '%s' "$1" | shasum -a 256 | awk '{print $1}'
    else
        printf 'unavailable'
    fi
}

# Read the existing hooks.json, or `{}` when absent. A malformed file is a hard
# error: silently replacing an operator's unparseable config would destroy it.
read_hooks_json() {
    if [[ ! -e "$HOOKS_FILE" ]]; then
        printf '{}'
        return 0
    fi
    if [[ ! -r "$HOOKS_FILE" ]]; then
        return 1
    fi
    if ! jq -e . "$HOOKS_FILE" >/dev/null 2>&1; then
        return 2
    fi
    cat "$HOOKS_FILE"
}

# Atomic write with restrictive permissions. The temp file is created INSIDE
# CODEX_HOME so the rename is same-filesystem (hence atomic), and its mode is
# set before any content is written.
atomic_write() {
    local target="$1" content="$2"
    local tmp
    tmp="$(mktemp "${target}.loom-tmp.XXXXXX" 2>/dev/null)" || return 1
    chmod 600 "$tmp" 2>/dev/null || true
    if ! printf '%s\n' "$content" > "$tmp" 2>/dev/null; then
        rm -f "$tmp" 2>/dev/null || true
        return 1
    fi
    if ! mv -f "$tmp" "$target" 2>/dev/null; then
        rm -f "$tmp" 2>/dev/null || true
        return 1
    fi
    chmod 600 "$target" 2>/dev/null || true
    return 0
}

# ---------------------------------------------------------------------------
# install
# ---------------------------------------------------------------------------
do_install() {
    if [[ ! -d "$CODEX_HOME_DIR" ]]; then
        log_error "CODEX_HOME '$PROFILE_NAME' does not exist. Provision the profile first (loom-daemon accounts add codex <name>)."
        return 78
    fi
    if [[ ! -r "$BRIDGE" ]]; then
        log_error "Managed hook bridge is not readable: $BRIDGE"
        return 78
    fi

    local existing rc=0
    existing="$(read_hooks_json)" || rc=$?
    case "$rc" in
        1) log_error "hooks.json exists in profile '$PROFILE_NAME' but is not readable."; return 78 ;;
        2) log_error "hooks.json in profile '$PROFILE_NAME' is not valid JSON. Refusing to overwrite an operator's config — fix or move it first."; return 78 ;;
    esac

    local cmd matcher timeout
    cmd="$(managed_command)"
    matcher="${MATCHER_ARG:-${LOOM_CODEX_HOOK_MATCHER:-*}}"
    timeout="${TIMEOUT_ARG:-${LOOM_CODEX_HOOK_TIMEOUT:-30}}"

    # Merge: drop any prior Loom handler (identified by the marker), then append
    # a fresh Loom-owned group. Groups that become empty are removed; every
    # non-Loom group and every other event is preserved untouched.
    local merged
    merged="$(printf '%s' "$existing" | jq \
        --arg marker "$LOOM_HOOK_MARKER" \
        --arg cmd "$cmd" \
        --arg matcher "$matcher" \
        --argjson timeout "$timeout" '
        def strip_loom:
            if type == "array" then
                map(
                    if (.hooks? | type) == "array" then
                        .hooks |= map(select((.command? // "") | contains($marker) | not))
                    else . end
                )
                | map(select((.hooks? | type) != "array" or ((.hooks | length) > 0)))
            else . end;
        .hooks = ((.hooks // {}) | if type == "object" then . else {} end)
        | .hooks.PreToolUse = ((.hooks.PreToolUse // []) | if type == "array" then . else [] end | strip_loom)
        | .hooks.PreToolUse += [{
              matcher: $matcher,
              hooks: [{ type: "command", command: $cmd, timeout: $timeout }]
          }]
    ' 2>/dev/null)" || merged=""

    if [[ -z "$merged" ]] || ! printf '%s' "$merged" | jq -e . >/dev/null 2>&1; then
        log_error "Failed to build a valid merged hooks.json for profile '$PROFILE_NAME'. Nothing was written."
        return 78
    fi

    if ! atomic_write "$HOOKS_FILE" "$merged"; then
        log_error "Atomic write of hooks.json failed for profile '$PROFILE_NAME'. The previous file is unchanged."
        return 78
    fi

    # Receipt: Loom-owned, non-secret staleness detector (see the header).
    local entry_hash receipt
    entry_hash="$(sha256_of "$cmd")"
    receipt="$(jq -nc \
        --arg version "$LOOM_HOOK_VERSION" \
        --arg command "$cmd" \
        --arg hash "$entry_hash" \
        --arg matcher "$matcher" \
        --arg schema "$CODEX_SCHEMA_PIN" \
        --arg workspace "${WORKSPACE_ARG%/}" \
        '{loomManagedHook: {version: ($version|tonumber), command: $command,
                            commandSha256: $hash, matcher: $matcher,
                            codexSchemaPin: $schema, workspace: $workspace}}' 2>/dev/null)" || receipt=""
    if [[ -n "$receipt" ]]; then
        atomic_write "$RECEIPT_FILE" "$receipt" || log_warn "Could not write the managed-hook receipt; verify will report the entry as unpinned."
    fi

    log_info "Installed the managed PreToolUse hook (v$LOOM_HOOK_VERSION) into Codex profile '$PROFILE_NAME'."
    log_info "Codex hook trust is NOT established by this command. Run 'CODEX_HOME=<profile> codex' once and accept the hook-trust prompt; Loom never passes --dangerously-bypass-hook-trust."
    return 0
}

# ---------------------------------------------------------------------------
# remove
# ---------------------------------------------------------------------------
do_remove() {
    if [[ ! -e "$HOOKS_FILE" ]]; then
        log_info "No hooks.json in profile '$PROFILE_NAME' — nothing to remove."
        rm -f "$RECEIPT_FILE" 2>/dev/null || true
        return 0
    fi

    local existing rc=0
    existing="$(read_hooks_json)" || rc=$?
    case "$rc" in
        1) log_error "hooks.json exists in profile '$PROFILE_NAME' but is not readable."; return 78 ;;
        2) log_error "hooks.json in profile '$PROFILE_NAME' is not valid JSON. Refusing to rewrite it."; return 78 ;;
    esac

    local stripped
    stripped="$(printf '%s' "$existing" | jq --arg marker "$LOOM_HOOK_MARKER" '
        def strip_loom:
            if type == "array" then
                map(
                    if (.hooks? | type) == "array" then
                        .hooks |= map(select((.command? // "") | contains($marker) | not))
                    else . end
                )
                | map(select((.hooks? | type) != "array" or ((.hooks | length) > 0)))
            else . end;
        if (.hooks? | type) == "object" and (.hooks.PreToolUse? | type) == "array" then
            .hooks.PreToolUse |= strip_loom
            | if (.hooks.PreToolUse | length) == 0 then del(.hooks.PreToolUse) else . end
            | if (.hooks | length) == 0 then del(.hooks) else . end
        else . end
    ' 2>/dev/null)" || stripped=""

    if [[ -z "$stripped" ]] || ! printf '%s' "$stripped" | jq -e . >/dev/null 2>&1; then
        log_error "Failed to build a valid hooks.json without Loom's entry. Nothing was written."
        return 78
    fi

    if ! atomic_write "$HOOKS_FILE" "$stripped"; then
        log_error "Atomic write of hooks.json failed. The previous file is unchanged."
        return 78
    fi
    rm -f "$RECEIPT_FILE" 2>/dev/null || true
    log_info "Removed Loom's managed PreToolUse hook from Codex profile '$PROFILE_NAME'. All other hooks preserved."
    return 0
}

# ---------------------------------------------------------------------------
# verify
# ---------------------------------------------------------------------------
#
# Prints a machine-readable object under --json:
#   {"profile","ready":bool,"installed":bool,"version":N,"trusted":bool,
#    "stale":bool,"bridgeReadable":bool,"reason":"..."}
# The profile DIRECTORY NAME is the only identity ever emitted; no path
# contents, no credential material.
do_verify() {
    local installed=false trusted=false stale=false bridge_readable=false
    local installed_cmd="" reason="" ready=false

    if [[ -r "$BRIDGE" ]]; then
        bridge_readable=true
    fi

    local existing rc=0
    existing="$(read_hooks_json)" || rc=$?
    if [[ "$rc" -ne 0 ]]; then
        reason="hooks.json is unreadable or malformed"
    else
        installed_cmd="$(printf '%s' "$existing" | jq -r \
            --arg marker "$LOOM_HOOK_MARKER" '
            [ (.hooks?.PreToolUse? // []) | .[]? | (.hooks? // []) | .[]?
              | (.command? // "") | select(contains($marker)) ] | .[0] // empty
        ' 2>/dev/null)" || installed_cmd=""
        [[ -n "$installed_cmd" ]] && installed=true
    fi

    # Codex-owned trust signal: any hooks.state entry with a trusted_hash. Codex
    # writes it either as a `[hooks.state."<id>"]` table with `trusted_hash =
    # "..."` on its own line, or as a dotted key
    # (`hooks.state."<id>".trusted_hash = "..."`); both spellings are matched.
    # A `#`-commented line is excluded so a documentation comment in an
    # operator's config.toml cannot fake trust.
    if [[ -r "$CONFIG_FILE" ]] \
        && grep -qE '^[^#]*trusted_hash[[:space:]]*=[[:space:]]*"[^"]+"' "$CONFIG_FILE" 2>/dev/null; then
        trusted=true
    fi

    # Staleness: the installed command must match the receipt's pinned hash.
    if [[ "$installed" == true ]]; then
        local pinned=""
        if [[ -r "$RECEIPT_FILE" ]]; then
            pinned="$(jq -r '.loomManagedHook.commandSha256 // empty' "$RECEIPT_FILE" 2>/dev/null)" || pinned=""
        fi
        if [[ -z "$pinned" ]]; then
            stale=true
            [[ -n "$reason" ]] || reason="no managed-hook receipt: the installed entry is unpinned"
        elif [[ "$pinned" != "$(sha256_of "$installed_cmd")" ]]; then
            stale=true
            [[ -n "$reason" ]] || reason="the installed managed-hook entry does not match the pinned receipt (stale)"
        fi
        # Version marker must match the version this script installs.
        if [[ "$installed_cmd" != *"--loom-hook-version $LOOM_HOOK_VERSION"* ]]; then
            stale=true
            [[ -n "$reason" ]] || reason="the installed managed-hook entry is not version $LOOM_HOOK_VERSION"
        fi
        # The named bridge must be the one this workspace provisioned.
        if [[ "$bridge_readable" == true && "$installed_cmd" != "$BRIDGE"* ]]; then
            stale=true
            [[ -n "$reason" ]] || reason="the installed managed-hook entry points at a different bridge than this workspace's"
        fi
    else
        [[ -n "$reason" ]] || reason="Loom's managed PreToolUse hook is not installed in this profile"
    fi

    if [[ "$bridge_readable" != true ]]; then
        reason="the managed hook bridge is missing or unreadable"
    elif [[ "$trusted" != true && -z "$reason" ]]; then
        reason="Codex hook trust is not established for this profile (no hooks.state trusted_hash in config.toml)"
    fi

    if [[ "$installed" == true && "$trusted" == true && "$stale" == false && "$bridge_readable" == true ]]; then
        ready=true
        reason="managed hook v$LOOM_HOOK_VERSION installed, pinned, and the profile has established Codex hook trust"
    fi
    [[ -n "$reason" ]] || reason="not ready"

    if [[ "$JSON_OUT" == "1" ]]; then
        jq -nc \
            --arg profile "$PROFILE_NAME" \
            --argjson ready "$ready" \
            --argjson installed "$installed" \
            --argjson trusted "$trusted" \
            --argjson stale "$stale" \
            --argjson bridgeReadable "$bridge_readable" \
            --argjson version "$LOOM_HOOK_VERSION" \
            --arg reason "$reason" \
            '{profile: $profile, ready: $ready, installed: $installed,
              trusted: $trusted, stale: $stale, bridgeReadable: $bridgeReadable,
              version: $version, reason: $reason}'
    fi

    if [[ "$ready" == true ]]; then
        log_info "Codex profile '$PROFILE_NAME': hook parity READY ($reason)."
        return 0
    fi
    log_error "Codex profile '$PROFILE_NAME': hook parity NOT ready — $reason."
    return 78
}

case "$COMMAND" in
    install) do_install ;;
    verify)  do_verify ;;
    remove)  do_remove ;;
esac
exit $?
