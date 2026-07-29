#!/usr/bin/env bash
# scripts/install/migrate-consumer.sh — one-pass migration of a historical
# file-copy Loom install to the machine-level daemon model (Epic #3835 Phase 6,
# #4254).
#
# A pre-daemon consumer repo (anvil, kicad-tools, …) holds a full committed copy
# of the Loom implementation under .loom/, .claude/commands/loom/,
# .claude/agents/loom-*.md, plus a legacy .loom/config.json. The machine-level
# model (Phases 1–5) resolves skills/agents/hooks/mcp at USER scope from the
# machine checkout (~/.local/share/loom) and reads project policy from a TRACKED
# .loom-project/project.json. This migration takes a repo from the former to the
# latter in one idempotent pass:
#
#   1. Detect the historical install from .loom/install-metadata.json +
#      installed_files manifest (refuse cleanly when absent).
#   2. Extract project config from legacy .loom/config.json into the tracked
#      .loom-project/project.json — EXCLUDING sweep.modelAliases (scope guard 1:
#      the Rust/Python resolvers diverge on that key, do not freeze the divergence
#      into every migrated repo).
#   3. Untrack the committed implementation per the manifest (git rm --cached) and
#      gitignore it, touching NOTHING outside the manifest (#3450 ownership rule).
#      Stale Loom-owned artifacts no longer shipped (loom-iteration.md,
#      loom-parent.md, shepherd-era scripts) are removed outright.
#   4. Preserve: .loom/resync-ignore pins stay tracked; locally-modified files are
#      surfaced (git rm --cached never deletes the working copy); runtime state
#      (.loom/logs/, .loom/sweep-checkpoint/, .loom/tokens/) and the loom.sh /
#      .loom/bin/loom shims are never touched.
#   5. Register the repo via `loom-daemon workspace add`.
#   6. Re-stamp .loom/install-metadata.json and refresh the CLAUDE.md marker
#      section to describe the daemon-model surface.
#
# Source this file (for the test suite) OR run it directly:
#     bash scripts/install/migrate-consumer.sh <repo> [--dry-run] [--priority N] [--force]
#
# Reuses the established installer patterns:
#   - scripts/install/manifest.sh        (_emit_loom_ownership_set — #3450 boundary)
#   - scripts/install/local-mode.sh      (gitignore block + git rm --cached idiom)
#   - defaults/scripts/resync-installed.sh (.loom/resync-ignore pin-list reading)
#
# Bash 3.2 compatible (stock macOS) — no associative arrays, no mapfile.

set -uo pipefail

# ── output helpers (plain text so sourcing tests can assert on them) ──────────
if [[ -t 1 ]]; then
    _MC_GRN=$'\033[0;32m'; _MC_YEL=$'\033[1;33m'; _MC_RED=$'\033[0;31m'
    _MC_BLU=$'\033[0;34m'; _MC_NC=$'\033[0m'
else
    _MC_GRN=''; _MC_YEL=''; _MC_RED=''; _MC_BLU=''; _MC_NC=''
fi
mc_err()   { printf '%bERROR: %s%b\n' "$_MC_RED" "$*" "$_MC_NC" >&2; }
mc_warn()  { printf '%bWARN: %s%b\n'  "$_MC_YEL" "$*" "$_MC_NC" >&2; }
mc_info()  { printf '%b%s%b\n'        "$_MC_BLU" "$*" "$_MC_NC"; }
# Per-file report line (#3777 resync reporting style): "  <verb>   <path> (note)".
mc_report() {
    local verb="$1" path="$2" note="${3:-}"
    local color="$_MC_GRN"
    case "$verb" in
        skipped|preserved|surfaced) color="$_MC_YEL" ;;
        would*)                     color="$_MC_BLU" ;;
    esac
    if [[ -n "$note" ]]; then
        printf '  %b%-16s%b %s %b(%s)%b\n' "$color" "$verb" "$_MC_NC" "$path" "$_MC_YEL" "$note" "$_MC_NC"
    else
        printf '  %b%-16s%b %s\n' "$color" "$verb" "$_MC_NC" "$path"
    fi
}

# ── resolve LOOM_ROOT (the machine checkout that ships defaults/) ──────────────
# Precedence: LOOM_MACHINE_CHECKOUT (exported by the dispatcher, #4229) > the
# script's own location (…/scripts/install/migrate-consumer.sh -> two dirs up).
mc_resolve_loom_root() {
    if [[ -n "${LOOM_ROOT:-}" && -d "${LOOM_ROOT}/defaults" ]]; then
        printf '%s\n' "$LOOM_ROOT"
        return 0
    fi
    if [[ -n "${LOOM_MACHINE_CHECKOUT:-}" && -d "${LOOM_MACHINE_CHECKOUT}/defaults" ]]; then
        printf '%s\n' "$LOOM_MACHINE_CHECKOUT"
        return 0
    fi
    local self_dir root
    self_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
    root="$(cd "$self_dir/../.." && pwd)"
    if [[ -d "$root/defaults" ]]; then
        printf '%s\n' "$root"
        return 0
    fi
    return 1
}

# ── deprecated / preserved path predicates ────────────────────────────────────

# The shim / entry-point surfaces the migration must leave TRACKED and working
# (scope guard 2): loom.sh -> `exec loom "$@"` and the per-repo pool manager
# under .loom/bin/. Keeping them tracked is what preserves post-migration
# `./loom.sh` and `./.loom/bin/loom`.
mc_is_preserved_shim() {
    case "$1" in
        loom.sh)        return 0 ;;
        .loom/bin/*)    return 0 ;;
        *)              return 1 ;;
    esac
}

# The machine-served implementation namespaces — the ONLY paths migration
# untracks + gitignores, single-sourced with local-mode.sh's ignore patterns
# (/.loom/, /.claude/commands/loom/, /.claude/agents/loom-*.md). A manifest entry
# outside these (e.g. .github/labels.yml, .codex/config.toml, .gitignore) is
# genuinely repo-level and stays TRACKED — the daemon model never serves it from
# the machine checkout. Runtime state and the .loom/bin/ shims are carved out by
# the earlier predicates before this one is consulted.
mc_is_untrackable_namespace() {
    case "$1" in
        .loom/*)                  return 0 ;;
        .claude/commands/loom/*)  return 0 ;;
        .claude/agents/loom-*.md) return 0 ;;
        *)                        return 1 ;;
    esac
}

# Explicitly deprecated Loom artifacts to remove OUTRIGHT (disk + index), even
# though current defaults/ still ships the (dead) file. These predate the daemon
# model and only cause drift/confusion in a migrated repo — the loom-iteration /
# loom-parent two-tier daemon skills (removed as a subsystem in v0.10.0, #3372)
# still ship as tombstone files. Genuinely-removed shepherd-era scripts (no
# longer in defaults/) are caught by the ownership-set check below and need no
# entry here. Add explicit paths as more tombstones are identified.
mc_is_deprecated_artifact() {
    case "$1" in
        .claude/commands/loom/loom-iteration.md) return 0 ;;
        .claude/commands/loom/loom-parent.md)    return 0 ;;
        *)                                        return 1 ;;
    esac
}

# Runtime state that must never be touched (scope guard 2). These are never in a
# manifest (generated, not shipped) — this is belt-and-suspenders.
mc_is_runtime_state() {
    case "$1" in
        .loom/logs/*|.loom/sweep-checkpoint/*|.loom/tokens/*) return 0 ;;
        .loom/worktrees/*|.loom/worktrees-local/*)            return 0 ;;
        *)                                                    return 1 ;;
    esac
}

# ── .loom/resync-ignore pin-list (same convention as resync-installed.sh) ──────
# A pin is a defaults-relative path (e.g. "hooks/guard-destructive.sh",
# "config.json"); the manifest gives target-relative paths (".loom/hooks/…",
# ".loom/config.json"). Translate a manifest path back to its defaults-relative
# form for the pin comparison so both files speak the same language.
mc_manifest_to_defaults_rel() {
    local p="$1"
    case "$p" in
        .loom/config.json) printf 'config.json\n' ;;
        .loom/README.md)   printf '.loom-README.md\n' ;;
        .loom/config/*)    printf 'config/%s\n' "${p#.loom/config/}" ;;
        .loom/roles/*)     printf 'roles/%s\n'  "${p#.loom/roles/}" ;;
        .loom/scripts/*)   printf 'scripts/%s\n' "${p#.loom/scripts/}" ;;
        .loom/runtimes/*)  printf 'runtimes/%s\n' "${p#.loom/runtimes/}" ;;
        .loom/hooks/*)     printf 'hooks/%s\n'  "${p#.loom/hooks/}" ;;
        .loom/docs/*)      printf 'docs/%s\n'   "${p#.loom/docs/}" ;;
        .loom/bin/*)       printf 'bin/%s\n'    "${p#.loom/bin/}" ;;
        .claude/commands/loom/*) printf 'commands/loom/%s\n' "${p#.claude/commands/loom/}" ;;
        *)                 printf '%s\n' "$p" ;;
    esac
}

mc_is_pinned() {
    local repo="$1" manifest_path="$2"
    local ignore_file="$repo/.loom/resync-ignore"
    [[ -f "$ignore_file" ]] || return 1
    local defaults_rel line
    defaults_rel="$(mc_manifest_to_defaults_rel "$manifest_path")"
    while IFS= read -r line || [[ -n "$line" ]]; do
        line="${line%%#*}"
        line="${line#"${line%%[![:space:]]*}"}"
        line="${line%"${line##*[![:space:]]}"}"
        [[ -z "$line" ]] && continue
        [[ "$line" == "$defaults_rel" || "$line" == "$manifest_path" ]] && return 0
    done < "$ignore_file"
    return 1
}

# ── detection ─────────────────────────────────────────────────────────────────

# mc_detect_install <repo> — 0 when a historical install (metadata + a non-empty
# installed_files manifest) is present, 1 otherwise.
mc_detect_install() {
    local repo="$1"
    local meta="$repo/.loom/install-metadata.json"
    [[ -f "$meta" ]] || return 1
    grep -q '"installed_files"' "$meta" 2>/dev/null || return 1
    return 0
}

# mc_already_migrated <repo> — 0 when the repo already carries the target layout
# (tracked .loom-project/project.json), so a second run is a clean no-op.
mc_already_migrated() {
    local repo="$1"
    [[ -n "$(git -C "$repo" ls-files -- .loom-project/project.json 2>/dev/null)" ]]
}

# mc_read_manifest <repo> — echo the installed_files entries, one target-relative
# path per line. Handles single- and multi-line JSON array formats (same parse
# as scripts/uninstall-loom.sh).
mc_read_manifest() {
    local repo="$1"
    local meta="$repo/.loom/install-metadata.json"
    local content list
    content="$(tr '\n' ' ' < "$meta")"
    list="$(printf '%s' "$content" \
        | grep -o '"installed_files"[[:space:]]*:[[:space:]]*\[[^]]*\]' \
        | sed 's/.*\[//;s/\]//' | tr ',' '\n')"
    local line
    while IFS= read -r line; do
        line="$(printf '%s' "$line" | sed 's/^[[:space:]]*"//;s/"[[:space:]]*$//;s/^[[:space:]]*//;s/[[:space:]]*$//')"
        [[ -n "$line" ]] && printf '%s\n' "$line"
    done <<< "$list"
}

# ── config extraction (scope guard 1: exclude sweep.modelAliases) ─────────────

# mc_extract_config <repo> <dry_run> — split legacy .loom/config.json into the two
# resolver tiers the daemon model uses (config_resolver.rs):
#   - tracked .loom-project/project.json  (PROJECT_CONFIG_REL — shared team policy)
#       minus sweep.modelAliases (scope guard 1) AND minus host-local keys.
#   - gitignored .loom-local/local.json   (LOCAL_CONFIG_REL — per-host override)
#       carries the host-local keys (worktree.root — a per-host scratch-disk path).
# Routing host-local keys OUT of the tracked tier is load-bearing: since this same
# pass `git rm --cached`s the legacy .loom/config.json, project.json becomes the
# highest tier every fresh clone / CI run picks up — copying worktree.root there
# would silently propagate one operator's filesystem layout to the whole team.
# Idempotent: an existing project.json short-circuits (leaves both tiers untouched).
mc_extract_config() {
    local repo="$1" dry_run="$2"
    local legacy="$repo/.loom/config.json"
    local project_dir="$repo/.loom-project"
    local project="$project_dir/project.json"
    local local_dir="$repo/.loom-local"
    local localcfg="$local_dir/local.json"

    if [[ -f "$project" ]]; then
        mc_report skipped ".loom-project/project.json" "already present"
        return 0
    fi
    if [[ ! -f "$legacy" ]]; then
        mc_report skipped ".loom-project/project.json" "no legacy .loom/config.json to extract"
        return 0
    fi
    if ! command -v jq >/dev/null 2>&1; then
        mc_warn "jq not available — cannot extract config into .loom-project/project.json"
        return 1
    fi

    # Host-local-looking keys routed to the gitignored .loom-local/local.json tier
    # instead of the tracked, shared project.json. worktree.root is the only one
    # detected today; add more jq paths here as they are identified.
    local wr have_local=0
    wr="$(jq -r '.worktree.root // empty' "$legacy" 2>/dev/null || true)"
    if [[ -n "$wr" ]]; then
        have_local=1
        mc_report surfaced ".loom/config.json" "worktree.root=$wr is host-local → .loom-local/local.json (NOT the tracked project.json)"
    fi
    if jq -e '.sweep.modelAliases' "$legacy" >/dev/null 2>&1; then
        mc_report surfaced "sweep.modelAliases" "EXCLUDED from project.json (Rust/Python resolver divergence — scope guard 1)"
    fi

    if [[ "$dry_run" == "1" ]]; then
        mc_report "would create" ".loom-project/project.json" "from .loom/config.json minus sweep.modelAliases + host-local keys"
        [[ "$have_local" == "1" ]] \
            && mc_report "would create" ".loom-local/local.json" "host-local worktree.root=$wr (gitignored, per-host tier)"
        return 0
    fi

    # Host-local tier first. Never clobber an existing local override; only fill in
    # a missing worktree.root so a re-run converges without losing operator edits.
    if [[ "$have_local" == "1" ]]; then
        mkdir -p "$local_dir" || { mc_err "could not create $local_dir"; return 1; }
        if [[ -f "$localcfg" ]]; then
            local tmpl
            tmpl="$(mktemp)" || return 1
            if jq --arg r "$wr" \
                 '.worktree = (.worktree // {})
                  | (if (.worktree.root // "") == "" then .worktree.root = $r else . end)' \
                 "$localcfg" > "$tmpl" 2>/dev/null; then
                mv "$tmpl" "$localcfg"
                mc_report updated ".loom-local/local.json" "host-local worktree.root ensured (existing override kept)"
            else
                rm -f "$tmpl"
                mc_warn "could not update $localcfg — leaving it as-is"
            fi
        else
            if jq -n --arg r "$wr" '{worktree: {root: $r}}' > "$localcfg" 2>/dev/null; then
                mc_report created ".loom-local/local.json" "host-local worktree.root=$wr (gitignored)"
            else
                mc_err "failed to write $localcfg"; return 1
            fi
        fi
        # Deliberately NOT git-added: .loom-local/ is gitignored (per-host tier).
    fi

    mkdir -p "$project_dir" || { mc_err "could not create $project_dir"; return 1; }
    # Strip sweep.modelAliases (scope guard 1) and the host-local worktree.root
    # (routed to local.json above); prune emptied sweep/worktree objects so the
    # migrated project config is clean.
    if ! jq 'del(.sweep.modelAliases)
             | del(.worktree.root)
             | if (has("sweep") and (.sweep | length) == 0) then del(.sweep) else . end
             | if (has("worktree") and (.worktree | length) == 0) then del(.worktree) else . end' \
             "$legacy" > "$project" 2>/dev/null; then
        mc_err "failed to extract config from $legacy"
        rm -f "$project"
        return 1
    fi
    git -C "$repo" add -- .loom-project/project.json 2>/dev/null || true
    mc_report created ".loom-project/project.json" "from .loom/config.json minus sweep.modelAliases + host-local keys"
    return 0
}

# ── manifest-driven removal ───────────────────────────────────────────────────

# mc_untrack_manifest <repo> <dry_run> — for each manifest entry decide:
#   - shim / runtime state              → leave tracked, never touch (preserve)
#   - pinned in .loom/resync-ignore     → leave tracked (skipped)
#   - CLAUDE.md                         → handled by the marker-section step
#   - deprecated Loom-namespace artifact (not in current defaults/) → remove
#   - current Loom implementation       → git rm --cached (untrack, keep on disk)
#   - not Loom-owned by current defaults/ → preserve (#3450)
# Sets globals MC_N_UNTRACKED / MC_N_REMOVED / MC_N_PRESERVED / MC_N_SKIPPED.
mc_untrack_manifest() {
    local repo="$1" dry_run="$2"
    local loom_root ownership_set
    loom_root="$(mc_resolve_loom_root)" || { mc_err "cannot resolve a Loom checkout with defaults/"; return 1; }

    # Current ownership boundary (#3450). Source manifest.sh with the right env.
    # shellcheck source=scripts/install/manifest.sh
    LOOM_ROOT="$loom_root" TARGET_PATH="$repo" source "$loom_root/scripts/install/manifest.sh"
    ownership_set="$(LOOM_ROOT="$loom_root" TARGET_PATH="$repo" _emit_loom_ownership_set)"

    MC_N_UNTRACKED=0; MC_N_REMOVED=0; MC_N_PRESERVED=0; MC_N_SKIPPED=0
    local path is_tracked
    while IFS= read -r path; do
        [[ -n "$path" ]] || continue

        # Never touch the shim entry points or runtime state.
        if mc_is_preserved_shim "$path"; then
            mc_report preserved "$path" "shim entry point — kept tracked"; continue
        fi
        if mc_is_runtime_state "$path"; then
            mc_report preserved "$path" "runtime state — untouched"; continue
        fi
        # CLAUDE.md is refreshed in place by the marker-section step, never untracked.
        if [[ "$path" == "CLAUDE.md" ]]; then
            continue
        fi
        # Repo-level files outside the machine-served namespaces (.github/*,
        # .codex/*, .gitignore, package.json, …) stay tracked — the daemon model
        # never serves them from the machine checkout.
        if ! mc_is_untrackable_namespace "$path"; then
            mc_report preserved "$path" "repo-level — kept tracked"
            MC_N_PRESERVED=$((MC_N_PRESERVED + 1)); continue
        fi
        # Pinned local customization: keep tracked, report.
        if mc_is_pinned "$repo" "$path"; then
            mc_report skipped "$path" "pinned in .loom/resync-ignore"
            MC_N_SKIPPED=$((MC_N_SKIPPED + 1)); continue
        fi

        is_tracked="$(git -C "$repo" ls-files -- "$path" 2>/dev/null)"

        # Explicitly-deprecated tombstone (still shipped, but dead): remove it.
        # Deprecated artifacts no longer shipped fall through to the ownership
        # check below; both paths converge on the same removal.
        if mc_is_deprecated_artifact "$path" \
            || ! printf '%s\n' "$ownership_set" | grep -Fxq -- "$path"; then
            # under a machine-served Loom namespace (established above) → Loom
            # provenance is certain, so removing cannot touch a consumer file
            # (#3450 boundary).
            if [[ ! -e "$repo/$path" && -z "$is_tracked" ]]; then
                continue  # already gone
            fi
            if [[ "$dry_run" == "1" ]]; then
                mc_report "would remove" "$path" "deprecated Loom artifact (no longer shipped)"
            else
                [[ -n "$is_tracked" ]] && git -C "$repo" rm -q -f --ignore-unmatch -- "$path" >/dev/null 2>&1
                rm -f "$repo/$path" 2>/dev/null || true
                mc_report removed "$path" "deprecated Loom artifact"
            fi
            MC_N_REMOVED=$((MC_N_REMOVED + 1)); continue
        fi

        # Current Loom implementation → untrack (git rm --cached), keep on disk.
        if [[ -z "$is_tracked" ]]; then
            continue  # not tracked (e.g. already local-mode); nothing to untrack
        fi
        # Surface a locally-modified file (working tree differs from index). The
        # working copy is preserved on disk by --cached; we only flag it.
        if ! git -C "$repo" diff --quiet -- "$path" 2>/dev/null; then
            mc_report surfaced "$path" "locally modified — working copy kept on disk"
        fi
        if [[ "$dry_run" == "1" ]]; then
            mc_report "would untrack" "$path" "git rm --cached (kept on disk, gitignored)"
        else
            git -C "$repo" rm -q --cached -- "$path" >/dev/null 2>&1 \
                && mc_report untracked "$path" "kept on disk, gitignored"
        fi
        MC_N_UNTRACKED=$((MC_N_UNTRACKED + 1))
    done < <(mc_read_manifest "$repo")
    return 0
}

# ── gitignore ─────────────────────────────────────────────────────────────────
# Reuse local-mode.sh's marker-delimited block so the ignored implementation
# paths are single-sourced with `install-loom.sh --local`.
mc_apply_gitignore() {
    local repo="$1" dry_run="$2"
    local loom_root
    loom_root="$(mc_resolve_loom_root)" || return 1
    # shellcheck source=scripts/install/local-mode.sh
    source "$loom_root/scripts/install/local-mode.sh"
    if [[ "$dry_run" == "1" ]]; then
        mc_report "would ignore" ".gitignore" "add the loom-local implementation block"
        return 0
    fi
    if loom_local_apply_gitignore "$repo"; then
        git -C "$repo" add -- .gitignore 2>/dev/null || true
        mc_report updated ".gitignore" "loom-local implementation block"
    else
        mc_warn "failed to update .gitignore"
        return 1
    fi
}

# ── workspace registration ────────────────────────────────────────────────────
mc_register_workspace() {
    local repo="$1" priority="$2" dry_run="$3"
    if ! command -v loom-daemon >/dev/null 2>&1; then
        mc_warn "loom-daemon not on PATH — skipping workspace registration."
        mc_warn "  register later with: loom-daemon workspace add \"$repo\" --priority $priority"
        return 0
    fi
    if [[ "$dry_run" == "1" ]]; then
        mc_report "would register" "$repo" "loom-daemon workspace add --priority $priority"
        return 0
    fi
    if loom-daemon workspace add "$repo" --priority "$priority" >/dev/null 2>&1; then
        mc_report registered "$repo" "loom-daemon workspace (priority $priority)"
    else
        # add is idempotent-ish; a duplicate is not a failure worth aborting on.
        mc_warn "loom-daemon workspace add returned non-zero (already registered?)"
    fi
}

# ── metadata re-stamp ─────────────────────────────────────────────────────────
mc_restamp_metadata() {
    local repo="$1" dry_run="$2"
    local meta="$repo/.loom/install-metadata.json"
    local loom_root ver commit today
    loom_root="$(mc_resolve_loom_root)" || return 1
    today="$(date +%Y-%m-%d)"
    ver="unknown"
    if [[ -f "$loom_root/package.json" ]] && command -v jq >/dev/null 2>&1; then
        ver="$(jq -r '.version // "unknown"' "$loom_root/package.json" 2>/dev/null || echo unknown)"
    fi
    commit="$(git -C "$loom_root" rev-parse --short HEAD 2>/dev/null || echo unknown)"

    if [[ "$dry_run" == "1" ]]; then
        mc_report "would restamp" ".loom/install-metadata.json" "loom_version=$ver, migrated=$today"
        return 0
    fi
    if ! command -v jq >/dev/null 2>&1; then
        mc_warn "jq not available — cannot re-stamp install-metadata.json"
        return 1
    fi
    local tmp
    tmp="$(mktemp)" || return 1
    if jq --arg v "$ver" --arg c "$commit" --arg d "$today" \
        '.loom_version = $v | .loom_commit = $c | .migrated_to_machine_model = $d | .install_model = "machine-daemon"' \
        "$meta" > "$tmp" 2>/dev/null; then
        mv "$tmp" "$meta"
        git -C "$repo" add -- .loom/install-metadata.json 2>/dev/null || true
        mc_report restamped ".loom/install-metadata.json" "loom_version=$ver, migrated=$today"
    else
        rm -f "$tmp"
        mc_warn "failed to re-stamp install-metadata.json"
        return 1
    fi
}

# ── CLAUDE.md marker section ──────────────────────────────────────────────────
# Refresh the Loom-managed block delimited by
#   <!-- BEGIN LOOM ORCHESTRATION --> … <!-- END LOOM ORCHESTRATION -->
# with a short daemon-model description. Absent markers → append the block.
MC_CLAUDE_BEGIN="<!-- BEGIN LOOM ORCHESTRATION -->"
MC_CLAUDE_END="<!-- END LOOM ORCHESTRATION -->"

mc_claude_block() {
    cat <<EOF
$MC_CLAUDE_BEGIN
## Loom Orchestration (machine-level daemon model)

This repo is migrated to the machine-level Loom daemon model (Epic #3835). The
Loom implementation is no longer committed here — skills (\`/builder\`, \`/judge\`,
\`/curator\`, \`/doctor\`, \`/loom:sweep\`, \`/loom\`, …), agents, guard hooks, and the
mcp-loom server all resolve at user scope from the machine checkout
(\`~/.local/share/loom\`), so they track the installed daemon version instead of
drifting stale.

- **Project policy** lives in the tracked \`.loom-project/project.json\` (enabled
  agents, buildGate, guard toggles, priorities). Host-local overrides go in the
  gitignored \`.loom-local/local.json\`; the legacy \`.loom/config.json\` remains a
  lower-precedence tier on disk.
- **Drive work** with \`loom sweep <issue>\` (machine dispatcher) or the manual
  role skills in a Claude Code session. This repo is registered in the
  machine-level workspace registry for autonomous dispatch.

See \`~/.local/share/loom/defaults/docs/machine-dispatcher.md\`.
$MC_CLAUDE_END
EOF
}

mc_update_claude_md() {
    local repo="$1" dry_run="$2"
    local claude="$repo/CLAUDE.md"
    if [[ ! -f "$claude" ]]; then
        if [[ "$dry_run" == "1" ]]; then
            mc_report "would create" "CLAUDE.md" "daemon-model marker section"; return 0
        fi
        mc_claude_block > "$claude"
        git -C "$repo" add -- CLAUDE.md 2>/dev/null || true
        mc_report created "CLAUDE.md" "daemon-model marker section"
        return 0
    fi

    local has_begin has_end
    has_begin="$(grep -cF "$MC_CLAUDE_BEGIN" "$claude" 2>/dev/null || echo 0)"
    has_end="$(grep -cF "$MC_CLAUDE_END" "$claude" 2>/dev/null || echo 0)"

    if [[ "$dry_run" == "1" ]]; then
        if [[ "$has_begin" == "1" && "$has_end" == "1" ]]; then
            mc_report "would update" "CLAUDE.md" "refresh the daemon-model marker section"
        else
            mc_report "would update" "CLAUDE.md" "append the daemon-model marker section"
        fi
        return 0
    fi

    local tmp
    tmp="$(mktemp)" || return 1
    if [[ "$has_begin" == "1" && "$has_end" == "1" ]]; then
        # Replace the existing block in place.
        awk -v begin="$MC_CLAUDE_BEGIN" -v end="$MC_CLAUDE_END" '
            $0 == begin { inblock=1; skip=1 }
            inblock && $0 == end { inblock=0; print "@@LOOM_BLOCK@@"; next }
            inblock { next }
            { print }
        ' "$claude" > "$tmp"
        # Splice the fresh block in place of the sentinel.
        local out
        out="$(mktemp)" || { rm -f "$tmp"; return 1; }
        while IFS= read -r line || [[ -n "$line" ]]; do
            if [[ "$line" == "@@LOOM_BLOCK@@" ]]; then
                mc_claude_block
            else
                printf '%s\n' "$line"
            fi
        done < "$tmp" > "$out"
        mv "$out" "$claude"; rm -f "$tmp"
        mc_report updated "CLAUDE.md" "refreshed the daemon-model marker section"
    else
        # Append (separated by a blank line).
        printf '\n' >> "$claude"
        mc_claude_block >> "$claude"
        mc_report updated "CLAUDE.md" "appended the daemon-model marker section"
        rm -f "$tmp"
    fi
    git -C "$repo" add -- CLAUDE.md 2>/dev/null || true
}

# ── MCP registration cleanup (#4386, surfaced on #4254 2026-07-29) ────────────
# A historical install can carry a repo-scoped .mcp.json whose `loom` server entry
# points into a long-dead worktree bundle (e.g.
# .loom/worktrees/issue-N/mcp-loom/dist/index.js). Under the machine-level model
# the `loom` server is registered ONCE at USER scope; Claude Code MCP precedence is
# local > project > user, so a lingering repo-scoped `loom` entry SILENTLY outranks
# (shadows) the user-scope server — and when its path is a dead worktree it kills
# every daemon-dispatched child at the claude-wrapper MCP pre-flight (a fleet-wide
# spawn outage with no surfaced error). This step therefore:
#   1. strips any repo-scoped `loom` entry from .mcp.json (deleting the file if
#      `loom` was its only server; other servers such as `safehouse` are kept),
#      matching setup-mcp.sh's #4230 migration;
#   2. verifies the user-scope `loom` registration exists and points at the machine
#      checkout's mcp-loom/dist/index.js, (re)adding it if absent or mis-pointed.

# mc_strip_mcp_loom_entry <file> — remove the `loom` server from an .mcp.json in
# place. Prints one token: skip | noop | stripped | removed-file.
mc_strip_mcp_loom_entry() {
    local file="$1"
    [[ -f "$file" ]] || { echo skip; return 0; }
    command -v python3 >/dev/null 2>&1 || { echo skip; return 0; }
    python3 - "$file" <<'PYEOF'
import json, os, sys
path = sys.argv[1]
try:
    with open(path) as f:
        cfg = json.load(f)
except Exception:
    print("skip"); sys.exit(0)
servers = cfg.get("mcpServers", {})
if "loom" not in servers:
    print("noop"); sys.exit(0)
del servers["loom"]
cfg["mcpServers"] = servers
if not servers:
    os.remove(path)
    print("removed-file")
else:
    with open(path, "w") as f:
        json.dump(cfg, f, indent=2)
        f.write("\n")
    print("stripped")
PYEOF
}

mc_fix_mcp() {
    local repo="$1" dry_run="$2"
    local mcp="$repo/.mcp.json"

    # 1. Strip a shadowing repo-scoped `loom` entry from .mcp.json.
    if [[ -f "$mcp" ]] && command -v jq >/dev/null 2>&1; then
        local has_loom
        has_loom="$(jq -r 'if (.mcpServers.loom) then "yes" else "no" end' "$mcp" 2>/dev/null || echo no)"
        if [[ "$has_loom" == "yes" ]]; then
            local stale note
            stale="$(jq -r '[.mcpServers.loom.args[]? | select(type=="string")]
                            | map(select(endswith("index.js")))[0] // empty' "$mcp" 2>/dev/null || true)"
            note="repo-scoped loom entry shadows the user-scope server (#4230)"
            if [[ -n "$stale" && ! -e "$stale" ]]; then
                note="repo-scoped loom entry points at a missing bundle ($stale) — #4386 spawn-outage signature"
            fi
            local was_tracked
            was_tracked="$(git -C "$repo" ls-files -- .mcp.json 2>/dev/null)"
            if [[ "$dry_run" == "1" ]]; then
                mc_report "would remove" ".mcp.json" "$note"
            else
                local res
                res="$(mc_strip_mcp_loom_entry "$mcp")"
                case "$res" in
                    removed-file) mc_report removed ".mcp.json" "$note (was loom-only; file deleted)" ;;
                    stripped)     mc_report updated ".mcp.json" "$note (stripped; other servers kept)" ;;
                    *)            : ;;
                esac
                # Stage the change only when the file was already tracked; never
                # newly-track a repo-scoped .mcp.json (it is a per-repo artifact).
                [[ -n "$was_tracked" ]] && git -C "$repo" add -- .mcp.json 2>/dev/null || true
            fi
        fi
    fi

    # 2. Verify / (re)add the user-scope `loom` MCP registration.
    local loom_root entry
    loom_root="$(mc_resolve_loom_root 2>/dev/null || true)"
    entry="${LOOM_HOME:-$HOME/.local/share/loom}/mcp-loom/dist/index.js"
    if [[ ! -f "$entry" && -n "$loom_root" && -f "$loom_root/mcp-loom/dist/index.js" ]]; then
        entry="$loom_root/mcp-loom/dist/index.js"
    fi
    if ! command -v claude >/dev/null 2>&1; then
        mc_warn "claude CLI not on PATH — cannot verify the user-scope 'loom' MCP registration."
        mc_warn "  register later (one time, per machine): claude mcp add --scope user loom -- node \"$entry\""
        return 0
    fi
    # `claude mcp get` reports across scopes; treat a hit that already references
    # the expected bundle as good and skip. Otherwise (absent or mis-pointed)
    # (re)register at user scope, idempotently (remove-then-add like install-loom.sh).
    local cur
    if cur="$(claude mcp get loom 2>/dev/null)" && printf '%s' "$cur" | grep -qF "$entry"; then
        mc_report skipped "user-scope loom MCP" "already registered → $entry"
        return 0
    fi
    if [[ "$dry_run" == "1" ]]; then
        mc_report "would register" "user-scope loom MCP" "claude mcp add --scope user loom -- node $entry"
        return 0
    fi
    claude mcp remove --scope user loom >/dev/null 2>&1 || true
    if claude mcp add --scope user loom -- node "$entry" >/dev/null 2>&1; then
        mc_report registered "user-scope loom MCP" "-> $entry"
    else
        mc_warn "failed to register the user-scope 'loom' MCP server; register manually:"
        mc_warn "  claude mcp add --scope user loom -- node \"$entry\""
    fi
}

# ── orchestration ─────────────────────────────────────────────────────────────
mc_usage() {
    cat <<EOF
Usage: migrate-consumer.sh <repo> [options]

Migrate a historical file-copy Loom install to the machine-level daemon model.

Options:
  --dry-run          Print the full file-by-file plan; make no changes.
  --priority N       Workspace registry priority (default: 3).
  --force            Proceed even when the working tree has uncommitted changes.
  -h, --help         Show this help.
EOF
}

mc_main() {
    local repo="" dry_run=0 priority=3 force=0
    while [[ $# -gt 0 ]]; do
        case "$1" in
            --dry-run|-n) dry_run=1 ;;
            --priority)   priority="${2:-3}"; shift ;;
            --priority=*) priority="${1#--priority=}" ;;
            --force)      force=1 ;;
            -h|--help)    mc_usage; return 0 ;;
            -*)           mc_err "unknown option: $1"; mc_usage; return 2 ;;
            *)            repo="$1" ;;
        esac
        shift
    done

    if [[ -z "$repo" ]]; then
        repo="$(git rev-parse --show-toplevel 2>/dev/null || true)"
    fi
    if [[ -z "$repo" || ! -d "$repo" ]]; then
        mc_err "no target repo (pass a path or run inside a git repo)"
        return 2
    fi
    repo="$(cd "$repo" && pwd)"
    if ! git -C "$repo" rev-parse --git-dir >/dev/null 2>&1; then
        mc_err "$repo is not a git repository"
        return 2
    fi

    # Detection / refusal.
    if ! mc_detect_install "$repo"; then
        mc_err "no historical Loom install detected at $repo"
        mc_err "  (.loom/install-metadata.json with an installed_files manifest is required)"
        return 1
    fi

    if mc_already_migrated "$repo"; then
        mc_info "Already migrated: $repo carries a tracked .loom-project/project.json."
        mc_info "Re-running the idempotent passes to converge any remaining drift."
    fi

    # Dirty-tree guard (define behavior: refuse unless --force). Only pre-existing
    # tracked changes matter — untracked runtime files are fine.
    if [[ "$force" != "1" ]]; then
        if ! git -C "$repo" diff --quiet 2>/dev/null || ! git -C "$repo" diff --cached --quiet 2>/dev/null; then
            mc_err "working tree has uncommitted changes — commit/stash them first, or pass --force."
            return 1
        fi
    fi

    if [[ "$dry_run" == "1" ]]; then
        mc_info "── loom migrate — DRY RUN plan for $repo ──"
    else
        mc_info "── loom migrate — applying to $repo ──"
    fi

    mc_extract_config    "$repo" "$dry_run"
    mc_untrack_manifest  "$repo" "$dry_run"
    mc_apply_gitignore   "$repo" "$dry_run"
    mc_fix_mcp           "$repo" "$dry_run"
    mc_restamp_metadata  "$repo" "$dry_run"
    mc_update_claude_md  "$repo" "$dry_run"
    mc_register_workspace "$repo" "$priority" "$dry_run"

    if [[ "$dry_run" == "1" ]]; then
        mc_info "── dry run complete — no changes made ──"
    else
        mc_info "── migration complete ──"
        mc_info "Review staged changes with 'git -C \"$repo\" status', then commit."
    fi
    return 0
}

# Run only when executed directly (not when sourced by the test suite).
if [[ "${BASH_SOURCE[0]}" == "${0}" ]]; then
    mc_main "$@"
fi
