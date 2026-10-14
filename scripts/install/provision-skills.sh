#!/usr/bin/env bash
# scripts/install/provision-skills.sh — user-scope Loom skills + agents
# provisioning (Epic #3835 Phase 4, #4261).
#
# Sibling of provision-dispatcher.sh. Where that installs the machine-level
# `loom` dispatcher (~/.local/bin/loom + ~/.local/share/loom), this establishes
# the user-scope resolution of the `/loom:*` slash-command skills and the
# `loom-*` subagents, so they resolve from the SINGLE machine-level checkout
# instead of a per-repo copy that can drift stale:
#
#     ~/.claude/commands/loom  -> <checkout>/defaults/.claude/commands/loom
#     ~/.claude/agents/loom-*.md -> <checkout>/defaults/.claude/agents/loom-*.md
#
# Source this file with:
#     source "$LOOM_ROOT/scripts/install/provision-skills.sh"
# then call `provision_loom_skills [checkout_dir] [claude_dir]`.
#
# Deliberately self-contained (defines its own output helpers) so the test suite
# can source it without pulling in the full installer.
#
# ── Why a whole-directory link for commands, per-file links for agents ────────
# `.claude/commands/loom/` is a Loom-owned namespace: one whole-directory symlink
# (`~/.claude/commands/loom`) is the minimal surface, keeps `.claude/commands/`
# itself a real directory that a co-installed tool can write siblings into, and
# preserves the relative cross-references between skill files (e.g. one skill
# `@`-including `probe-protocol.md`). `~/.claude/agents/`, by contrast, is SHARED
# with the operator's own agents, so it cannot be wholesale-symlinked — each
# `loom-<role>.md` is linked individually and non-Loom agents are left untouched.
#
# ── Why link THROUGH the checkout path, not $LOOM_ROOT ────────────────────────
# The links target `<checkout>/defaults/...` (default checkout
# `${LOOM_HOME:-$HOME/.local/share/loom}`), NOT the installer's transient
# $LOOM_ROOT. `~/.local/share/loom` is itself a symlink to the source checkout
# (provisioned by provision-dispatcher.sh), so the links stay valid even if the
# machine checkout is later re-provisioned from a different source tree, and
# `loom update` (which refreshes the checkout) updates the skills/agents seen by
# every repo at once.
#
# ── Clobber avoidance (mirrors provision-dispatcher.sh's semantics) ───────────
# A real file/dir at a destination (an operator's hand-tuned
# `~/.claude/agents/loom-judge.md`, or a real `~/.claude/commands/loom/`
# directory) is LEFT AS-IS and warned — never clobbered. A SYMLINK that points
# somewhere other than the current checkout may be repointed (e.g. a link left
# behind by an older checkout location). Dangling `loom-*` agent links whose
# target no longer exists in the checkout are pruned so a renamed/removed role
# does not leave a broken agent behind forever.
#
# ── Precedence: the user-scope copy WINS (it is not a fallback) ──────────────
# This header used to say Claude Code resolves a project-level definition over a
# user-level one when names collide, so a repo's committed
# `.claude/commands/loom/*` copies would keep winning until migration. That is
# not what a fleet observed (2026-10-14): on hosts whose
# `~/.claude/commands/loom` linked into a month-old `~/.local/share/loom` clone,
# every `/loom:<role>` session loaded the USER-scope file (the transcript's
# prompt matched it, not the repo's fresh copy), and agents
# re-applied labels the repo's own docs had retired. So the checkout these
# links point into is the role prompt every repo on the host runs, and it must
# never move to an older tree than the one it has: see
# `provision_loom_skills`'s [reference_root].

# Emit a skills-provision status line (plain text so sourcing tests can assert).
_pskill_ok()   { echo "  [loom-skills] $*"; }
_pskill_warn() { echo "  [loom-skills] WARNING: $*" >&2; }

# Set on every return (success or soft-failure) so the caller can VERIFY the
# destinations it wrote rather than trust a success message (the #4053
# "expose enough for the caller to verify" contract, matching
# provision-dispatcher.sh's PROVISIONED_DISPATCHER_BIN). Assigned as GLOBALS
# (no `local`) so they survive the function return.
# shellcheck disable=SC2034
PROVISIONED_SKILLS_LINK=""
# shellcheck disable=SC2034
PROVISIONED_AGENT_LINKS=""

# provision_loom_skills [checkout_dir] [claude_dir] [reference_root]
#
# [reference_root] is the Loom source tree the caller just synced from (e.g.
# resync-installed.sh's SOURCE_ROOT). The links then point into the NEWEST (by
# VERSION) of <checkout_dir>, the tree the commands link points into now, and
# <reference_root>; ties keep that order, so an equal tree never moves the
# link. A stale machine checkout would otherwise shadow every repo's fresher
# project-level prompts (see "Precedence" above), and taking the newest rather
# than the latest caller's tree means a later resync from an OLDER source never
# moves the link backwards. A reference under a temporary directory is refused
# (a throwaway clone would leave the link dangling once it is deleted).
#
# Best-effort — never fatal. Returns 1 on a soft failure so the caller can note
# it, but the installer must NOT abort on a non-zero return.
provision_loom_skills() {
    local checkout_dir="${1:-${LOOM_HOME:-$HOME/.local/share/loom}}"
    local claude_dir="${2:-$HOME/.claude}"
    local reference_root="${3:-}"

    checkout_dir="$(_pskill_pick_checkout "$checkout_dir" "$reference_root" "$claude_dir")"

    local commands_src="$checkout_dir/defaults/.claude/commands/loom"
    local agents_src="$checkout_dir/defaults/.claude/agents"
    local commands_dest="$claude_dir/commands/loom"
    local agents_dest_dir="$claude_dir/agents"
    local star_src="$commands_src/star.md"
    local star_dest="$claude_dir/commands/star.md"

    # Publish resolved destinations up front so EVERY return path communicates
    # where things live (the caller gates on the return code). Consumed by
    # callers/tests, not this file — hence the SC2034 suppressions.
    # shellcheck disable=SC2034
    PROVISIONED_SKILLS_LINK="$commands_dest"
    # shellcheck disable=SC2034
    PROVISIONED_AGENT_LINKS=""

    # -L follows the checkout symlink to the real defaults/ tree.
    if [[ ! -d "$commands_src" ]]; then
        _pskill_warn "skills source not found at '$commands_src'; skipping (is the machine checkout provisioned?)."
        return 1
    fi

    local soft_fail=0

    # ── 1. whole-directory commands link ──────────────────────────────────────
    if ! mkdir -p "$claude_dir/commands" 2>/dev/null; then
        _pskill_warn "could not create $claude_dir/commands; skipping skills link."
        soft_fail=1
    else
        _pskill_link_dir "$commands_src" "$commands_dest" || soft_fail=1
    fi

    # ── 1b. `/star` short-form alias (#10154) ─────────────────────────────────
    # ~/.claude/commands/star.md -> <checkout>/.../loom/star.md so `/star` and
    # `/loom:star` share ONE body. A real operator star.md is preserved + warned.
    if [[ -f "$star_src" && -d "$claude_dir/commands" ]]; then
        _pskill_link_file "$star_src" "$star_dest" || soft_fail=1
    fi

    # ── 2. per-file agent links ───────────────────────────────────────────────
    if [[ ! -d "$agents_src" ]]; then
        _pskill_warn "agents source not found at '$agents_src'; skipping agent links."
        soft_fail=1
    elif ! mkdir -p "$agents_dest_dir" 2>/dev/null; then
        _pskill_warn "could not create $agents_dest_dir; skipping agent links."
        soft_fail=1
    else
        local agent_src agent_base agent_dest links=""
        shopt -s nullglob
        for agent_src in "$agents_src"/loom-*.md; do
            agent_base="$(basename "$agent_src")"
            agent_dest="$agents_dest_dir/$agent_base"
            if _pskill_link_file "$agent_src" "$agent_dest"; then
                links="${links}${agent_dest}"$'\n'
            else
                soft_fail=1
            fi
        done
        shopt -u nullglob
        # shellcheck disable=SC2034
        PROVISIONED_AGENT_LINKS="${links%$'\n'}"

        # ── 3. prune dangling loom-* agent links (renamed/removed roles) ──────
        _pskill_prune_dangling_agents "$agents_dest_dir" "$agents_src"
    fi

    return "$soft_fail"
}

# deprovision_loom_skills [checkout_dir] [claude_dir]
#
# Remove the user-scope links, but ONLY when they point into the given machine
# checkout or into another Loom `defaults/.claude/` tree (provision may have
# linked a newer reference tree) — an operator's real files/dirs, or symlinks
# that point elsewhere, are left untouched. Best-effort; never fatal.
deprovision_loom_skills() {
    local checkout_dir="${1:-${LOOM_HOME:-$HOME/.local/share/loom}}"
    local claude_dir="${2:-$HOME/.claude}"

    local commands_src="$checkout_dir/defaults/.claude/commands/loom"
    local agents_src="$checkout_dir/defaults/.claude/agents"
    local commands_dest="$claude_dir/commands/loom"
    local agents_dest_dir="$claude_dir/agents"

    if _pskill_link_points_into "$commands_dest" "$commands_src" \
        || _pskill_link_matches "$commands_dest" "*/defaults/.claude/commands/loom"; then
        rm -f "$commands_dest" 2>/dev/null \
            && _pskill_ok "removed skills link $commands_dest"
    elif [[ -e "$commands_dest" ]]; then
        _pskill_ok "left $commands_dest as-is (not a link into the checkout)"
    fi

    local star_src="$commands_src/star.md"
    local star_dest="$claude_dir/commands/star.md"
    if _pskill_link_points_into "$star_dest" "$star_src" \
        || _pskill_link_matches "$star_dest" "*/defaults/.claude/commands/loom/star.md"; then
        rm -f "$star_dest" 2>/dev/null \
            && _pskill_ok "removed star alias $star_dest"
    fi

    local agent_dest
    shopt -s nullglob
    for agent_dest in "$agents_dest_dir"/loom-*.md; do
        if _pskill_link_points_into "$agent_dest" "$agents_src/$(basename "$agent_dest")" \
            || _pskill_link_matches "$agent_dest" "*/defaults/.claude/agents/loom-*.md"; then
            rm -f "$agent_dest" 2>/dev/null \
                && _pskill_ok "removed agent link $agent_dest"
        fi
    done
    shopt -u nullglob
    return 0
}

# ── internal helpers ─────────────────────────────────────────────────────────

# _pskill_version <tree> — the tree's VERSION (first line, whitespace
# stripped), or empty when unreadable.
_pskill_version() {
    local f="$1/VERSION" v=""
    [[ -r "$f" ]] || return 0
    IFS= read -r v < "$f" || true
    printf '%s' "${v//[[:space:]]/}"
}

# _pskill_version_lt <a> <b> — true iff dotted-numeric <a> < <b>. Compares
# numerically per component (0.19.27 < 0.19.1160), like
# defaults/scripts/lib/locate-daemon-bin.sh's _loom_daemon_version_lt. A
# non-numeric component returns "not less".
_pskill_version_lt() {
    local a="$1" b="$2" ap bp
    for _ in 1 2 3 4; do
        ap="${a%%.*}"; bp="${b%%.*}"
        [[ "$ap" =~ ^[0-9]+$ && "$bp" =~ ^[0-9]+$ ]] || return 1
        (( 10#$ap < 10#$bp )) && return 0
        (( 10#$ap > 10#$bp )) && return 1
        case "$a" in *.*) a="${a#*.}" ;; *) a="0" ;; esac
        case "$b" in *.*) b="${b#*.}" ;; *) b="0" ;; esac
    done
    return 1
}

# _pskill_is_temp <dir> — true iff <dir> resolves under a temporary root:
# /tmp, /private/tmp, /var/folders, /private/var/folders or $TMPDIR.
# LOOM_SKILLS_TEMP_ROOTS (space-separated) replaces that list; the test suite
# sets it because its fixtures live under mktemp.
_pskill_is_temp() {
    local real root roots
    real="$(cd "$1" 2>/dev/null && pwd -P)" || return 1
    roots="${LOOM_SKILLS_TEMP_ROOTS-/tmp /private/tmp /var/folders /private/var/folders ${TMPDIR:-}}"
    for root in $roots; do
        root="$(cd "$root" 2>/dev/null && pwd -P)" || continue
        [[ "$real/" == "${root%/}/"* ]] && return 0
    done
    return 1
}

# _pskill_pick_checkout <checkout_dir> [reference_root] [claude_dir] — print
# the tree the user-scope links should point into: the newest (by VERSION) of
# <checkout_dir>, the tree <claude_dir>/commands/loom points into now, and
# <reference_root>. A candidate needs the skills tree and a readable VERSION
# and must not live under a temporary root; <checkout_dir> is the default and
# wins ties, so with no reference nothing changes. A switch is warned on stderr
# so the operator can refresh the machine checkout (`loom update`).
_pskill_pick_checkout() {
    local checkout="$1" ref="${2:-}" claude_dir="${3:-$HOME/.claude}"
    local skills="defaults/.claude/commands/loom" cur="" best bestv="" cand cv
    [[ -n "$ref" ]] || { printf '%s\n' "$checkout"; return 0; }
    if [[ -L "$claude_dir/commands/loom" ]]; then
        cur="$(readlink "$claude_dir/commands/loom" 2>/dev/null || true)"
        cur="${cur%/"$skills"}"
    fi
    if [[ -d "$ref/$skills" ]] && _pskill_is_temp "$ref"; then
        _pskill_warn "not linking user-scope skills into $ref: it is under a temporary directory and would leave the link dangling once deleted."
        ref=""
    fi
    best="$checkout"
    [[ -d "$checkout/$skills" ]] && bestv="$(_pskill_version "$checkout")"
    for cand in "$cur" "$ref"; do
        [[ -n "$cand" && -d "$cand/$skills" ]] || continue
        [[ "$(cd "$cand" && pwd -P)" == "$(cd "$best" 2>/dev/null && pwd -P)" ]] && continue
        _pskill_is_temp "$cand" && continue
        cv="$(_pskill_version "$cand")"
        [[ -n "$cv" ]] || continue
        if [[ -z "$bestv" ]] || _pskill_version_lt "$bestv" "$cv"; then
            best="$cand"; bestv="$cv"
        fi
    done
    if [[ "$best" != "$checkout" ]]; then
        cv="$(_pskill_version "$checkout")"
        if [[ ! -d "$checkout/$skills" ]]; then cv="has no skills tree"
        elif [[ -z "$cv" ]]; then cv="has no readable VERSION"
        else cv="is at $cv, older than $bestv"; fi
        _pskill_warn "machine checkout $checkout $cv; linking user-scope skills into $best instead, because a stale user-scope /loom:* command shadows every repo's own copy. Refresh it with 'loom update'."
    fi
    printf '%s\n' "$best"
}

# Establish <dest> as a symlink -> <src> (whole directory). Preserves a real
# file/dir; repoints a stale symlink; idempotent when already correct.
_pskill_link_dir() {
    local src="$1" dest="$2"
    if [[ -L "$dest" ]]; then
        local cur
        cur="$(readlink "$dest" 2>/dev/null || true)"
        if [[ "$cur" == "$src" ]]; then
            _pskill_ok "skills already linked: $dest -> $src"
            return 0
        fi
        _pskill_ok "repointing stale skills link: $dest ($cur -> $src)"
        rm -f "$dest" 2>/dev/null || true
    elif [[ -e "$dest" ]]; then
        _pskill_warn "$dest already exists (not a symlink); leaving as-is."
        return 0
    fi
    if ln -s "$src" "$dest" 2>/dev/null; then
        _pskill_ok "linked skills: $dest -> $src"
        return 0
    fi
    _pskill_warn "could not create skills link at $dest"
    return 1
}

# Establish <dest> as a symlink -> <src> (single file). Preserves a real file;
# repoints a stale symlink; idempotent when already correct.
_pskill_link_file() {
    local src="$1" dest="$2"
    if [[ -L "$dest" ]]; then
        local cur
        cur="$(readlink "$dest" 2>/dev/null || true)"
        if [[ "$cur" == "$src" ]]; then
            return 0
        fi
        rm -f "$dest" 2>/dev/null || true
    elif [[ -e "$dest" ]]; then
        _pskill_warn "$dest already exists (not a symlink); leaving operator file as-is."
        return 0
    fi
    if ln -s "$src" "$dest" 2>/dev/null; then
        _pskill_ok "linked: $(basename "$dest")"
        return 0
    fi
    _pskill_warn "could not create agent link at $dest"
    return 1
}

# Remove `loom-*.md` symlinks under <agents_dir> that point into a checkout's
# `defaults/.claude/agents/` tree and are dangling (a role that was renamed or
# removed upstream) or, given [agents_src], name an agent that tree does not
# ship (left behind when the links moved to a different tree). A link to
# something OUTSIDE a Loom tree is left alone — it is not Loom's to prune.
_pskill_prune_dangling_agents() {
    local agents_dir="$1" agents_src="${2:-}" link tgt
    shopt -s nullglob
    for link in "$agents_dir"/loom-*.md; do
        [[ -L "$link" ]] || continue
        # -e follows the link; a live target the current tree still ships is kept.
        [[ -e "$link" ]] && { [[ -z "$agents_src" || -e "$agents_src/$(basename "$link")" ]]; } && continue
        tgt="$(readlink "$link" 2>/dev/null || true)"
        if [[ "$tgt" == *"/defaults/.claude/agents/"* ]]; then
            rm -f "$link" 2>/dev/null \
                && _pskill_ok "pruned dangling agent link: $(basename "$link")"
        fi
    done
    shopt -u nullglob
}

# True iff <path> is a symlink whose readlink target matches glob <pattern>.
_pskill_link_matches() {
    [[ -L "$1" ]] || return 1
    # shellcheck disable=SC2053  # <pattern> is a glob on purpose
    [[ "$(readlink "$1" 2>/dev/null || true)" == $2 ]]
}

# True iff <path> is a symlink whose readlink target equals <expected_src>.
_pskill_link_points_into() {
    local path="$1" expected="$2"
    [[ -L "$path" ]] || return 1
    local cur
    cur="$(readlink "$path" 2>/dev/null || true)"
    [[ "$cur" == "$expected" ]]
}
