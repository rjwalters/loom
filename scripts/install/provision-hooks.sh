#!/usr/bin/env bash
# scripts/install/provision-hooks.sh — user-scope Loom guard-hook wiring
# (Epic #3835 Phase 5, #4262).
#
# Sibling of provision-skills.sh / provision-dispatcher.sh. Where those establish
# the machine-level `loom` dispatcher and the user-scope `/loom:*` skills, this
# wires the Loom PreToolUse / UserPromptSubmit / Stop guard HOOKS into the
# operator's user-scope `~/.claude/settings.json`, pointing at hook scripts that
# execute from the SINGLE machine-level checkout instead of a per-repo
# `.loom/hooks/` copy that drifts stale (the recurring resync-installed.sh pain).
#
# Source this file with:
#     source "$LOOM_ROOT/scripts/install/provision-hooks.sh"
# then call `provision_loom_hooks [claude_dir]`.
#
# Deliberately self-contained (defines its own output helpers) so the test suite
# can source it without pulling in the full installer.
#
# ── Why user-scope (design decision 1) ────────────────────────────────────────
# The epic's end state is "hook scripts machine-level". A user-scope entry in
# ~/.claude/settings.json fires in every repo the operator opens, resolving the
# hook script from the machine checkout — so a fresh consumer repo needs NO
# per-repo `.loom/hooks/` copy and NO project-level settings.json hook entry to
# get the guards. The project-level entries `loom-daemon init` used to write are
# no longer added on fresh installs (the defaults settings.json drops its `hooks`
# block); existing project-level entries on already-installed repos are left
# untouched (Phase 6 / #4254 migration territory).
#
# ── The fail-open command wrapper (design decisions 2/3/4) ────────────────────
# Because a user-scope hook fires in EVERY repo — including non-Loom ones — each
# wired command is a self-gating one-liner (see [`_phook_cmd`]) that:
#   1. Resolves the main repo root worktree-aware (`git rev-parse
#      --git-common-dir`/..), so guards still fire from inside `.loom/worktrees/*`.
#   2. WORKSPACE GATE (AC3): exit 0 (silent no-op) unless that root holds
#      `.loom-project/project.json` OR `.loom/config.json` — i.e. is a Loom
#      workspace (migrated or legacy). Non-Loom repos, and the case where the
#      machine checkout is absent, no-op cleanly.
#   3. TRANSITION DEDUP (design decision 3): if the repo still carries a per-repo
#      `.loom/hooks/<name>` copy (pre-Phase-6), exit 0 and let the project-level
#      entry run that copy — the project copy WINS until Phase 6 strips it, so a
#      transition repo runs each guard exactly ONCE (no double-fire, no
#      duplicated decision-log lines).
#   4. Otherwise exec the machine-checkout hook
#      `${LOOM_HOME:-$HOME/.local/share/loom}/defaults/hooks/<name>`, passing the
#      resolved repo root through `LOOM_PROJECT_ROOT` (so guard-destructive.sh's
#      dispatcher resolves the consuming repo's canonical Repo-Skills guard from
#      a checkout-shaped SCRIPT_DIR), and fail open (exit 0) if it is missing.
# `$HOME` / `$LOOM_HOME` are expanded per-user at hook-invocation time (the
# command is shell-executed), so a single wired command is correct for every
# operator — this is the dogfood repo's proven worktree-aware wrapper pattern.
#
# ── Ownership + idempotence (the #4200 lesson) ────────────────────────────────
# Every wired command carries the substring `/defaults/hooks/<name>` — the same
# machine-level marker `loom-daemon init`'s scaffolding.rs recognizes
# (MACHINE_HOOK_MARKER). Dedup and removal both key on that substring, which
# survives any requoting Claude Code applies to settings.json, so a re-run never
# duplicates an entry and deprovision never orphans one.

# Emit a hooks-provision status line (plain text so sourcing tests can assert).
_phook_ok()   { echo "  [loom-hooks] $*"; }
_phook_warn() { echo "  [loom-hooks] WARNING: $*" >&2; }

# Set on every return (success or soft-failure) so the caller can VERIFY what was
# written rather than trust a success message (the #4053 "expose enough for the
# caller to verify" contract, matching provision-dispatcher.sh's
# PROVISIONED_DISPATCHER_BIN). Assigned as GLOBALS (no `local`) so they survive
# the function return. Consumed by callers/tests, not this file.
# shellcheck disable=SC2034
PROVISIONED_HOOKS_SETTINGS=""
# shellcheck disable=SC2034
PROVISIONED_HOOKS_BACKUP=""

# The Loom guard-hook wiring set. Parallel arrays (matcher strings contain `|`,
# so a delimited single array is unsafe). Order mirrors defaults/.claude/
# settings.json, plus the Edit|Write worktree guard (design decision 4) that
# fresh consumers never had wired before.
_PHOOK_TYPES=(PreToolUse PreToolUse PreToolUse UserPromptSubmit UserPromptSubmit Stop)
_PHOOK_MATCHERS=(Bash Bash "Edit|Write" "" "" "")
_PHOOK_NAMES=(
    guard-destructive.sh
    guard-loom-workflow.sh
    guard-worktree-paths.sh
    skill-router.sh
    methodology-inject.sh
    guard-background-subagents.sh
)

# Emit the fail-open, workspace-gated, transition-deferring command wrapper for a
# single hook script <name>. A single-quoted `bash -c '...'` body with NO
# embedded single quotes (all internal quoting is double quotes) so it survives
# JSON round-tripping cleanly. See the header for the four steps this encodes.
_phook_cmd() {
    local name="$1"
    # shellcheck disable=SC2016  # literal $-expansions are intentional (per-user, at hook time)
    printf '%s' "bash -c 'ROOT=\$(cd \"\$(git rev-parse --git-common-dir 2>/dev/null)/..\" 2>/dev/null && pwd); [ -n \"\$ROOT\" ] || exit 0; { [ -f \"\$ROOT/.loom-project/project.json\" ] || [ -f \"\$ROOT/.loom/config.json\" ]; } || exit 0; [ -x \"\$ROOT/.loom/hooks/${name}\" ] && exit 0; H=\"\${LOOM_HOME:-\$HOME/.local/share/loom}/defaults/hooks/${name}\"; [ -x \"\$H\" ] && LOOM_PROJECT_ROOT=\"\$ROOT\" exec \"\$H\" || exit 0'"
}

# provision_loom_hooks [claude_dir]
#
# Idempotently merge the Loom guard-hook wiring into the user-scope
# ~/.claude/settings.json. Best-effort — never fatal. Returns 1 on a soft
# failure (invalid existing JSON, jq missing, unwritable dir) so the caller can
# note it, but the installer must NOT abort on a non-zero return.
provision_loom_hooks() {
    local claude_dir="${1:-$HOME/.claude}"
    local settings="$claude_dir/settings.json"

    # Publish the resolved destination up front so EVERY return path communicates
    # where things live (the caller gates on the return code).
    # shellcheck disable=SC2034
    PROVISIONED_HOOKS_SETTINGS="$settings"
    # shellcheck disable=SC2034
    PROVISIONED_HOOKS_BACKUP=""

    if ! command -v jq >/dev/null 2>&1; then
        _phook_warn "jq not available; cannot wire user-scope hooks."
        return 1
    fi

    if ! mkdir -p "$claude_dir" 2>/dev/null; then
        _phook_warn "could not create $claude_dir; skipping hook wiring."
        return 1
    fi

    # Refuse to touch an existing file that is not valid JSON — a blind write
    # would clobber the operator's settings. A missing OR empty file is fine
    # (we start from {}).
    if [[ -s "$settings" ]]; then
        if ! jq empty "$settings" >/dev/null 2>&1; then
            _phook_warn "$settings is not valid JSON; leaving it untouched."
            return 1
        fi
    fi

    # Back up before the first mutation (only when there is existing content to
    # preserve). Timestamped so repeated runs never clobber an earlier backup.
    if [[ -s "$settings" ]]; then
        local backup
        backup="${settings}.loom-backup-$(date -u +%Y%m%dT%H%M%SZ)"
        if cp "$settings" "$backup" 2>/dev/null; then
            # shellcheck disable=SC2034
            PROVISIONED_HOOKS_BACKUP="$backup"
            _phook_ok "backed up existing settings -> $(basename "$backup")"
        else
            _phook_warn "could not back up $settings; refusing to mutate it."
            return 1
        fi
    fi

    # Seed a missing/empty file with an empty object so jq has a base document.
    if [[ ! -s "$settings" ]]; then
        printf '{}\n' > "$settings" 2>/dev/null || {
            _phook_warn "could not initialize $settings."
            return 1
        }
    fi

    local soft_fail=0 i
    for i in "${!_PHOOK_NAMES[@]}"; do
        local htype="${_PHOOK_TYPES[$i]}"
        local matcher="${_PHOOK_MATCHERS[$i]}"
        local name="${_PHOOK_NAMES[$i]}"
        local cmd
        cmd="$(_phook_cmd "$name")"
        if _phook_merge_one "$settings" "$htype" "$matcher" "$name" "$cmd"; then
            _phook_ok "wired $htype/${matcher:-<all>} -> $name"
        else
            _phook_warn "failed to wire $name into $settings"
            soft_fail=1
        fi
    done

    return "$soft_fail"
}

# Merge a single hook entry into <settings_file>, deduplicating by the
# machine-level marker substring `defaults/hooks/<name>` (survives requoting).
#   $1 settings_file  $2 hook_type  $3 matcher  $4 name  $5 command
_phook_merge_one() {
    local f="$1" htype="$2" matcher="$3" name="$4" cmd="$5"
    local marker="defaults/hooks/$name"
    local tmp
    tmp="$(mktemp 2>/dev/null)" || return 1
    if jq \
        --arg ht "$htype" \
        --arg m "$matcher" \
        --arg marker "$marker" \
        --arg cmd "$cmd" '
        .hooks = (.hooks // {})
        | .hooks[$ht] = (.hooks[$ht] // [])
        | (.hooks[$ht] | map(.matcher // "") | index($m)) as $idx
        | if $idx == null then
            .hooks[$ht] += [{matcher: $m, hooks: [{type: "command", command: $cmd}]}]
          else
            .hooks[$ht][$idx].hooks = (.hooks[$ht][$idx].hooks // [])
            | if (.hooks[$ht][$idx].hooks | map(.command // "") | any(contains($marker)))
              then .
              else .hooks[$ht][$idx].hooks += [{type: "command", command: $cmd}]
              end
          end
        ' "$f" > "$tmp" 2>/dev/null; then
        mv "$tmp" "$f" 2>/dev/null || { rm -f "$tmp"; return 1; }
        return 0
    fi
    rm -f "$tmp"
    return 1
}

# deprovision_loom_hooks [claude_dir]
#
# Remove Loom-owned guard-hook entries from the user-scope ~/.claude/settings.json
# — identified by the machine-level marker substring `/defaults/hooks/` in the
# command (the same MACHINE_HOOK_MARKER scaffolding.rs uses). Never touches a
# non-Loom hook. Backs up before mutating. Cleans up emptied matcher entries and
# hook-type arrays. Best-effort; never fatal.
#
# NOTE: a per-repo Loom UNINSTALL does NOT call this — the user-scope wiring is a
# single machine-level resource shared by every repo Loom is installed into
# (exactly like the ~/.local/bin/loom dispatcher and the user-scope skills from
# Phase 4 / #4261). A machine-level teardown is the correct caller.
deprovision_loom_hooks() {
    local claude_dir="${1:-$HOME/.claude}"
    local settings="$claude_dir/settings.json"

    command -v jq >/dev/null 2>&1 || return 0
    [[ -s "$settings" ]] || return 0
    jq empty "$settings" >/dev/null 2>&1 || {
        _phook_warn "$settings is not valid JSON; leaving it untouched."
        return 0
    }

    # Nothing Loom-owned to remove? Leave the file (and its mtime) alone.
    if ! jq -e '
        (.hooks // {}) | to_entries | any(.value[]?.hooks[]?.command // "" | contains("/defaults/hooks/"))
        ' "$settings" >/dev/null 2>&1; then
        return 0
    fi

    local backup
    backup="${settings}.loom-backup-$(date -u +%Y%m%dT%H%M%SZ)"
    cp "$settings" "$backup" 2>/dev/null && _phook_ok "backed up settings -> $(basename "$backup")"

    local tmp
    tmp="$(mktemp 2>/dev/null)" || return 0
    if jq '
        if .hooks then
            .hooks |= (
                to_entries
                | map(
                    .value |= (
                        map(
                            .hooks |= map(select((.command // "") | contains("/defaults/hooks/") | not))
                        )
                        | map(select((.hooks | length) > 0))
                    )
                )
                | map(select((.value | length) > 0))
                | from_entries
            )
            | if (.hooks | length) == 0 then del(.hooks) else . end
        else . end
        ' "$settings" > "$tmp" 2>/dev/null; then
        mv "$tmp" "$settings" 2>/dev/null \
            && _phook_ok "removed Loom user-scope hook entries from $settings"
    else
        rm -f "$tmp"
    fi
    return 0
}
