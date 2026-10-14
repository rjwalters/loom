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
# not what the fleet observed (2AMLogic/2am#4345, 2026-10-14): on hosts whose
# `~/.claude/commands/loom` linked into a month-old `~/.local/share/loom` clone,
# every `/loom:<role>` session loaded the USER-scope file (the transcript's
# prompt matched it, not the repo's fresh copy), and agents
# re-applied labels the repo's own docs had retired. So the checkout these
# links point into is the role prompt every repo on the host runs, and it must
# never be older than the source a caller just synced from: see
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
# resync-installed.sh's SOURCE_ROOT). When given, and <checkout_dir> is a
# different tree whose VERSION is OLDER (or unreadable, or it lacks the skills
# tree), the links point into <reference_root> instead, with a warning: a
# stale machine checkout would otherwise shadow every repo's fresher
# project-level role prompts (see "Precedence" above).
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
        _pskill_prune_dangling_agents "$agents_dest_dir"
    fi

    return "$soft_fail"
}

# deprovision_loom_skills [checkout_dir] [claude_dir]
#
# Remove the user-scope links, but ONLY when they point into the given machine
# checkout — an operator's real files/dirs, or symlinks that point elsewhere,
# are left untouched. Best-effort; never fatal.
deprovision_loom_skills() {
    local checkout_dir="${1:-${LOOM_HOME:-$HOME/.local/share/loom}}"
    local claude_dir="${2:-$HOME/.claude}"

    local commands_src="$checkout_dir/defaults/.claude/commands/loom"
    local agents_src="$checkout_dir/defaults/.claude/agents"
    local commands_dest="$claude_dir/commands/loom"
    local agents_dest_dir="$claude_dir/agents"

    if _pskill_link_points_into "$commands_dest" "$commands_src"; then
        rm -f "$commands_dest" 2>/dev/null \
            && _pskill_ok "removed skills link $commands_dest"
    elif [[ -e "$commands_dest" ]]; then
        _pskill_ok "left $commands_dest as-is (not a link into the checkout)"
    fi

    local star_src="$commands_src/star.md"
    local star_dest="$claude_dir/commands/star.md"
    if _pskill_link_points_into "$star_dest" "$star_src"; then
        rm -f "$star_dest" 2>/dev/null \
            && _pskill_ok "removed star alias $star_dest"
    fi

    local agent_src agent_dest
    shopt -s nullglob
    for agent_src in "$agents_src"/loom-*.md; do
        agent_dest="$agents_dest_dir/$(basename "$agent_src")"
        if _pskill_link_points_into "$agent_dest" "$agent_src"; then
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

# _pskill_pick_checkout <checkout_dir> [reference_root] — print the tree the
# user-scope links should point into: <checkout_dir>, unless <reference_root>
# is given, carries the skills tree, is a different tree, and <checkout_dir>
# is older than it (or has no readable VERSION, or no skills tree). The
# fallback is warned on stderr so the operator can refresh the machine
# checkout (`loom update` fast-forwards it).
#
# Two guards keep the fallback from making things worse. A reference under a
# temp dir is refused (a throwaway clone can vanish and leave the links
# dangling; set LOOM_PROVISION_ALLOW_TMP_REFERENCE=1 to override, e.g. in
# tests). And the link is never repointed to a tree older than the live target
# of the current link: the newest of {checkout, current target, reference} wins,
# so one repo's resync cannot downgrade what another repo's resync linked.
_pskill_pick_checkout() {
    local checkout="$1" ref="${2:-}" claude_dir="${3:-}"
    if [[ -z "$ref" || ! -d "$ref/defaults/.claude/commands/loom" ]]; then
        printf '%s\n' "$checkout"; return 0
    fi
    local c_real r_real
    if [[ "${LOOM_PROVISION_ALLOW_TMP_REFERENCE:-0}" != "1" ]]; then
        local ref_abs t
        ref_abs="$(cd "$ref" 2>/dev/null && pwd -P || true)"
        for t in "${TMPDIR:-/tmp}" /tmp /private/tmp /var/folders; do
            t="${t%/}"
            if [[ -n "$t" && ( "$ref_abs" == "$t"/* || "$ref" == "$t"/* ) ]]; then
                _pskill_warn "reference $ref is under $t (may vanish); keeping machine checkout $checkout."
                printf '%s\n' "$checkout"; return 0
            fi
        done
    fi
    c_real="$(cd "$checkout" 2>/dev/null && pwd -P || true)"
    r_real="$(cd "$ref" 2>/dev/null && pwd -P || true)"
    if [[ -n "$c_real" && "$c_real" == "$r_real" ]]; then
        printf '%s\n' "$checkout"; return 0
    fi
    local cv rv why=""
    cv="$(_pskill_version "$checkout")"
    rv="$(_pskill_version "$ref")"
    if [[ ! -d "$checkout/defaults/.claude/commands/loom" ]]; then
        why="has no skills tree"
    elif [[ -z "$cv" ]]; then
        why="has no readable VERSION"
    elif [[ -n "$rv" ]] && _pskill_version_lt "$cv" "$rv"; then
        why="is at $cv, older than $rv"
    fi
    if [[ -n "$why" ]]; then
        # Never downgrade: keep the tree the current link already targets when
        # it is at least as new as the reference.
        local cur cur_tree curv
        cur="$(readlink "$claude_dir/commands/loom" 2>/dev/null || true)"
        cur_tree="${cur%/defaults/.claude/commands/loom}"
        if [[ -n "$claude_dir" && "$cur_tree" != "$cur" && -d "$cur" ]]; then
            curv="$(_pskill_version "$cur_tree")"
            if [[ -n "$curv" && "$cur_tree" != "$ref" ]] && ! _pskill_version_lt "$curv" "$rv"; then
                _pskill_warn "machine checkout $checkout $why in $ref; keeping the current link target $cur_tree ($curv), which is not older than $ref."
                printf '%s\n' "$cur_tree"; return 0
            fi
        fi
        _pskill_warn "machine checkout $checkout $why in $ref; linking user-scope skills into $ref instead, because a stale user-scope /loom:* command shadows every repo's own copy. Refresh it with 'loom update'."
        printf '%s\n' "$ref"; return 0
    fi
    printf '%s\n' "$checkout"
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

# Remove `loom-*.md` symlinks under <agents_dir> that are dangling AND point
# into a checkout's `defaults/.claude/agents/` tree (a role that was renamed or
# removed upstream). A broken link to something OUTSIDE the checkout is left
# alone — it is not Loom's to prune.
_pskill_prune_dangling_agents() {
    local agents_dir="$1" link tgt
    shopt -s nullglob
    for link in "$agents_dir"/loom-*.md; do
        [[ -L "$link" ]] || continue
        # -e follows the link; a true target still present is kept.
        [[ -e "$link" ]] && continue
        tgt="$(readlink "$link" 2>/dev/null || true)"
        if [[ "$tgt" == *"/defaults/.claude/agents/"* ]]; then
            rm -f "$link" 2>/dev/null \
                && _pskill_ok "pruned dangling agent link: $(basename "$link")"
        fi
    done
    shopt -u nullglob
}

# True iff <path> is a symlink whose readlink target equals <expected_src>.
_pskill_link_points_into() {
    local path="$1" expected="$2"
    [[ -L "$path" ]] || return 1
    local cur
    cur="$(readlink "$path" 2>/dev/null || true)"
    [[ "$cur" == "$expected" ]]
}
