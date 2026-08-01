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
# then call `provision_loom_hooks [claude_dir]` (user-scope wiring) and/or
# `ensure_project_hook_wiring <target>` (the pre-Phase-6 project-level fallback
# described under "Why the project-level fallback exists" below).
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
#
# ── Why the project-level fallback exists (issue #4401) ───────────────────────
# The user-scope wiring above is only HALF a guard-hook execution path. Two
# preconditions must ALSO hold for it to actually fire:
#   (a) a machine checkout must exist at ${LOOM_HOME:-$HOME/.local/share/loom}
#       (established by provision_loom_dispatcher on the FULL install path only), and
#   (b) the repo must carry NO per-repo `.loom/hooks/<name>` copy — otherwise the
#       TRANSITION DEDUP step above makes the wrapper defer to the project-level
#       entry that runs that copy.
# `install.sh --quick` satisfies NEITHER: it never provisions a machine checkout,
# and `install_hooks_and_cli` writes a fresh `.loom/hooks/*.sh` copy set on every
# run. Meanwhile the 0.16.0 `defaults/.claude/settings.json` carries no `hooks`
# block, and a `--confirm-reinstall`'s chained `uninstall-loom.sh` jq-strips every
# `.loom/hooks/`-prefixed command out of the project `.claude/settings.json`. Net
# result before #4401: copies present → user-scope defers; project entries gone →
# ZERO guards ran on a supported update path (the #4401 report).
#
# `ensure_project_hook_wiring` closes that hole from the other side: whenever a
# target still carries per-repo `.loom/hooks/<name>` copies, it (re)asserts the
# matching project-level `${CLAUDE_PROJECT_DIR}/.loom/hooks/<name>` entries so the
# copies the quick path just wrote are actually reachable. It is a no-op on a
# post-Phase-6 (migrated, copy-free) repo, where the user-scope wiring is the one
# true path. Exactly one path fires in either case — never both, never neither.
#
# NOTE this is deliberately re-asserted on EVERY install rather than made to
# "survive" an uninstall: `uninstall-loom.sh` strips on the `.loom/hooks/` command
# prefix, not on provenance, so no project-level entry can be made strip-proof.
# Re-adding after init is the only durable shape (curator addendum, #4401).

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
# Same contract for the project-level fallback (`ensure_project_hook_wiring`,
# #4401): the resolved project settings path and how many per-repo hook copies it
# asserted entries for (0 on a migrated, copy-free repo).
# shellcheck disable=SC2034
PROJECT_HOOKS_SETTINGS=""
# shellcheck disable=SC2034
PROJECT_HOOKS_WIRED=0

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
#
# TRANSITION DEDUP (#4806): the deferral to a present per-repo `.loom/hooks/
# <name>` copy is CONDITIONAL, not unconditional — it only steps aside when the
# project's `.claude/settings.json` actually references that copy (i.e. there is
# something for it to defer TO). Without this guard, a repo that carries stale
# `.loom/hooks/` copies but has had its project-level `hooks` block deleted
# (accident or #4401-style installer gap) would silently run ZERO guard hooks:
# the user-scope wrapper steps aside for an entry that was never wired. The
# `[ -f ... ]` check matters — `$(<missing-file)` writes an error to stderr,
# which would leak into hook output.
# shellcheck disable=SC2016  # literal $-expansion is intentional (a fixed marker string, not evaluated here)
_PHOOK_WRAPPER_MARKER='ROOT=$(cd "$(git rev-parse --git-common-dir'
_phook_cmd() {
    local name="$1"
    # shellcheck disable=SC2016  # literal $-expansions are intentional (per-user, at hook time)
    printf '%s' "bash -c 'ROOT=\$(cd \"\$(git rev-parse --git-common-dir 2>/dev/null)/..\" 2>/dev/null && pwd); [ -n \"\$ROOT\" ] || exit 0; { [ -f \"\$ROOT/.loom-project/project.json\" ] || [ -f \"\$ROOT/.loom/config.json\" ]; } || exit 0; [ -x \"\$ROOT/.loom/hooks/${name}\" ] && [ -f \"\$ROOT/.claude/settings.json\" ] && case \"\$(<\"\$ROOT/.claude/settings.json\")\" in *\".loom/hooks/${name}\"*) exit 0 ;; esac; H=\"\${LOOM_HOME:-\$HOME/.local/share/loom}/defaults/hooks/${name}\"; [ -x \"\$H\" ] && LOOM_PROJECT_ROOT=\"\$ROOT\" exec \"\$H\" || exit 0'"
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
        if _phook_merge_one "$settings" "$htype" "$matcher" "defaults/hooks/$name" "$cmd" "" "$_PHOOK_WRAPPER_MARKER"; then
            _phook_ok "wired $htype/${matcher:-<all>} -> $name"
        else
            _phook_warn "failed to wire $name into $settings"
            soft_fail=1
        fi
    done

    return "$soft_fail"
}

# Merge a single hook entry into <settings_file>, deduplicating by a marker
# substring that survives requoting — `defaults/hooks/<name>` for the
# machine-level (user-scope) form, `.loom/hooks/<name>` for the project-level
# form. The marker is passed in rather than derived so both callers share this
# one merge implementation.
# <$6 exclude_marker> (optional) narrows the duplicate test: a command counts as
# an existing copy only when it contains <dedup_marker> and does NOT contain
# <exclude_marker>. The project-level caller needs this because the MACHINE-level
# wrapper command embeds `$ROOT/.loom/hooks/<name>` in its transition-dedup probe
# — so a bare `.loom/hooks/<name>` substring test would mistake a stray
# machine-level entry for the project-level entry that entry defers TO, and skip
# writing the only command that actually executes anything.
#
# <$7 upgrade_marker> (optional, #4806): enables the stale-Loom-wrapper UPGRADE
# path. When a duplicate is found (by <dedup_marker>) that is NOT byte-identical
# to <command>, it is normally left alone (a `_phook_cmd()` edit would otherwise
# never reach already-provisioned installs, because the dedup marker matches
# regardless of which version of the wrapper generated it — the exact trap
# described in #4806). Passing <upgrade_marker> narrows that: a duplicate is
# REWRITTEN in place only when it ALSO contains <upgrade_marker> — a substring
# recognizable ONLY in a Loom-authored wrapper (see `_PHOOK_WRAPPER_MARKER`) —
# so a hand-written / non-Loom entry that happens to reference the same hook
# name is never touched, only ever left as-is.
#   $1 settings_file  $2 hook_type  $3 matcher  $4 dedup_marker  $5 command
#   $6 exclude_marker (optional)  $7 upgrade_marker (optional)
_phook_merge_one() {
    local f="$1" htype="$2" matcher="$3" marker="$4" cmd="$5" exclude="${6:-}" upgrade="${7:-}"
    local tmp
    tmp="$(mktemp 2>/dev/null)" || return 1
    if jq \
        --arg ht "$htype" \
        --arg m "$matcher" \
        --arg marker "$marker" \
        --arg exclude "$exclude" \
        --arg upgrade "$upgrade" \
        --arg cmd "$cmd" '
        def is_dup($c): ($c | contains($marker))
            and (($exclude | length) == 0 or ($c | contains($exclude) | not));
        def is_stale_loom_wrapper($c): ($upgrade | length) > 0
            and ($c != $cmd)
            and ($c | contains($upgrade));
        .hooks = (.hooks // {})
        | .hooks[$ht] = (.hooks[$ht] // [])
        | (.hooks[$ht] | map(.matcher // "") | index($m)) as $idx
        | if $idx == null then
            .hooks[$ht] += [{matcher: $m, hooks: [{type: "command", command: $cmd}]}]
          else
            .hooks[$ht][$idx].hooks = (.hooks[$ht][$idx].hooks // [])
            | if (.hooks[$ht][$idx].hooks | map(.command // "") | any(is_dup(.)))
              then
                # A duplicate (by dedup_marker) already exists. Rewrite it IN
                # PLACE only if it is a stale Loom-authored wrapper (recognized
                # by upgrade_marker) that differs from the current command —
                # never touch anything else (already-current, or hand-written).
                .hooks[$ht][$idx].hooks |= map(
                    if is_dup(.command // "") and is_stale_loom_wrapper(.command // "")
                    then .command = $cmd
                    else .
                    end
                )
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

# ensure_project_hook_wiring <target> [settings_rel]
#
# Guarantee that a target repo which still carries per-repo `.loom/hooks/<name>`
# copies has the matching PROJECT-level `.claude/settings.json` entries that
# execute them (issue #4401). See "Why the project-level fallback exists" in the
# header for the full rationale; the short version:
#
#   copies present  → project-level entries run them; the user-scope wrapper
#                     defers (transition dedup) so each guard fires exactly once.
#   copies absent   → no-op; the machine-level user-scope wiring is the one path.
#
# Additive and idempotent: an operator's own hook entries are never touched, and
# an entry already present (matched by the `.loom/hooks/<name>` substring, which
# also covers the legacy pre-#3277 bare-relative form) is never duplicated. Uses
# the `${CLAUDE_PROJECT_DIR}` prefix (#3277) for new entries so the command
# resolves from the project root regardless of Claude Code's cwd.
#
# Best-effort — never fatal. Returns 1 on a soft failure (jq missing, invalid
# existing JSON, unwritable path) so the caller can note it without aborting.
ensure_project_hook_wiring() {
    local target="${1:-}"
    local settings_rel="${2:-.claude/settings.json}"
    local settings="$target/$settings_rel"
    local hooks_dir="$target/.loom/hooks"

    # Publish resolved state on EVERY return path so the caller (and the test
    # suite) can VERIFY the outcome rather than trust a message (#4053 contract).
    # shellcheck disable=SC2034
    PROJECT_HOOKS_SETTINGS="$settings"
    # shellcheck disable=SC2034
    PROJECT_HOOKS_WIRED=0

    if [[ -z "$target" || ! -d "$target" ]]; then
        _phook_warn "project hook wiring: target '${target:-<unset>}' is not a directory."
        return 1
    fi

    # Post-Phase-6 (migrated / copy-free) repo: nothing to make reachable. The
    # user-scope wiring provisioned by provision_loom_hooks is the execution path.
    local have_copy=false i
    for i in "${!_PHOOK_NAMES[@]}"; do
        if [[ -f "$hooks_dir/${_PHOOK_NAMES[$i]}" ]]; then
            have_copy=true
            break
        fi
    done
    if [[ "$have_copy" != "true" ]]; then
        _phook_ok "no per-repo .loom/hooks/ copies — guards run from the machine checkout (no project entries needed)"
        return 0
    fi

    if ! command -v jq >/dev/null 2>&1; then
        _phook_warn "jq not available; cannot assert project-level hook entries in $settings."
        return 1
    fi

    if ! mkdir -p "$(dirname "$settings")" 2>/dev/null; then
        _phook_warn "could not create $(dirname "$settings"); skipping project hook wiring."
        return 1
    fi

    # Refuse to touch an existing file that is not valid JSON — a blind write
    # would clobber the consumer's project settings.
    if [[ -s "$settings" ]]; then
        if ! jq empty "$settings" >/dev/null 2>&1; then
            _phook_warn "$settings is not valid JSON; leaving it untouched."
            return 1
        fi
    else
        printf '{}\n' > "$settings" 2>/dev/null || {
            _phook_warn "could not initialize $settings."
            return 1
        }
    fi

    local soft_fail=0 wired=0
    for i in "${!_PHOOK_NAMES[@]}"; do
        local name="${_PHOOK_NAMES[$i]}"
        # Only wire a hook whose script is actually present on disk — a missing
        # copy (e.g. the #4041 vendored generic guard the installer deliberately
        # skips) must not get a dangling project-level entry.
        [[ -f "$hooks_dir/$name" ]] || continue
        local htype="${_PHOOK_TYPES[$i]}"
        local matcher="${_PHOOK_MATCHERS[$i]}"
        # shellcheck disable=SC2016  # ${CLAUDE_PROJECT_DIR} is expanded by Claude Code, not bash
        local cmd='${CLAUDE_PROJECT_DIR}/.loom/hooks/'"$name"
        if _phook_merge_one "$settings" "$htype" "$matcher" ".loom/hooks/$name" "$cmd" "defaults/hooks/"; then
            wired=$((wired + 1))
        else
            _phook_warn "failed to assert project-level entry for $name in $settings"
            soft_fail=1
        fi
    done

    # shellcheck disable=SC2034
    PROJECT_HOOKS_WIRED="$wired"
    _phook_ok "project-level hook entries asserted for $wired per-repo hook copy/copies -> $settings"
    return "$soft_fail"
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
