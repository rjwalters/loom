#!/usr/bin/env bash
# lib/cargo-target-dir.sh — resolve, and safely reclaim, a worktree's Cargo
# target directory (issue #7239).
#
# ## Why this exists
#
# Cargo's build output is NOT always `<workspace>/target`. `CARGO_TARGET_DIR`
# or `build.target-dir` in any `config.toml` on the lookup path can redirect it
# anywhere — commonly to a large external volume. Loom's worktree lifecycle
# creates one such directory per worktree, but every removal path
# (`worktree.sh remove`, the daemon reaper) only ever removed the worktree
# directory itself, so a redirected target dir outlived its worktree forever.
# On one multi-agent host that accumulated tens of orphaned directories and
# hundreds of GB before anyone noticed.
#
# ## Three parts
#
#   1. `loom_resolve_cargo_target_dir <workspace_root>` — Cargo's resolution
#      order (env → `cargo metadata` → `<root>/target`). This is a LIBRARY
#      twin of the standalone `scripts/cargo-target-dir.sh` (which is part of
#      the Loom repo's own install path and is not shipped to consumer repos).
#      **The two must stay byte-identical in behavior**;
#      `defaults/scripts/tests/test-cargo-target-dir-reclaim.sh` asserts parity
#      between them on every branch of the resolution order, so drift fails CI
#      rather than silently changing what gets deleted.
#
#   2. `loom_reclaim_worktree_target_dir …` — the removal-time gate: reclaim a
#      resolved target dir ONLY when it is redirected outside the worktree, is
#      attributable to that worktree, is not shared with any other live
#      worktree, is not held open by a running process, and is not one of the
#      paths this pass must never touch.
#
#   3. `loom_provision_worktree_target_dir …` (issue #8458) — the CREATION-time
#      counterpart: give the worktree its own target dir under the
#      otherwise-shared root (`<root>/wt/<worktree name>`) and record it in a
#      marker file INSIDE the worktree, so part 2 can attribute and reclaim it.
#      See "Per-worktree target dirs" below.
#
# ## The attribution rule (the one that keeps costing data)
#
# Only a redirect derived from the WORKTREE ITSELF can be evidence that a
# directory belongs to it. Three inputs look like evidence and are not:
#
#   * the remover's own `CARGO_TARGET_DIR` — read from this process's
#     environment, machine- or session-global by construction;
#   * a `build.target-dir` in `$CARGO_HOME/config.toml` or in any ancestor
#     `.cargo/config.toml` above the worktree — equally global, and resolving
#     identically for every path on the host;
#   * a DEGRADED resolution of some other live worktree — when `cargo metadata`
#     fails for a sibling, "it builds somewhere else" and "we could not find out
#     where it builds" are the same `<root>/target` string, and only the first
#     of those licenses a deletion.
#
# Each of the three deleted a machine-global shared cargo cache in review
# reproductions of this file. The first two are handled by
# `loom_cargo_target_dir_redirect_possible` (which consults only the worktree's
# own config files) plus gate 2f; the third by
# `loom_resolve_worktree_target_dir_checked` plus the exit-3 fail-closed path in
# `loom_target_dir_shared_with`.
#
# The Rust daemon has an equivalent (`loom-daemon/src/worktree_ops/cargo_target.rs`)
# for its own removal path; the two are deliberately parallel implementations
# of the same rules, each with its own tests, because the daemon runs against
# repos where this library is not installed.
#
# Everything here writes diagnostics to stderr only: the reclaim function
# emits ONE tab-separated record on stdout so callers (which may be in
# `--json` stdout-purity mode) decide how to render it.

# --------------------------------------------------------------------------
# Resolution
# --------------------------------------------------------------------------

# Resolve a possibly-relative Cargo path against a workspace root, without
# requiring it to exist (the target dir is created by the build itself).
_loom_ctd_absolutize() {
    local value="$1" root="$2"
    case "$value" in
        /*) printf '%s\n' "$value" ;;
        *)  printf '%s\n' "$root/$value" ;;
    esac
}

# loom_resolve_cargo_target_dir <workspace_root>
#
# Prints Cargo's actual target directory for <workspace_root>. Mirrors
# scripts/cargo-target-dir.sh exactly:
#   1. $CARGO_TARGET_DIR when set and non-empty (env beats config in Cargo).
#   2. `cargo metadata --format-version 1 --no-deps` (applies the full
#      config.toml hierarchy, including build.target-dir).
#   3. `<workspace_root>/target` — Cargo's default.
# Always exits 0 with a path on stdout; a resolution hiccup degrades to the
# historical hardcoded assumption rather than to a hard failure.
#
# `_loom_ctd_metadata_target_dir <root>` is the middle step, factored out so the
# reclaim path can tell "cargo says <root>/target" apart from "cargo could not
# answer" — a distinction `loom_resolve_cargo_target_dir` itself must NOT make,
# because it has to stay behaviorally identical to scripts/cargo-target-dir.sh.
# Prints the absolutized target_directory on stdout, exit 0. Exit 1 means cargo
# is missing, exited non-zero, or emitted no usable field.
_loom_ctd_metadata_target_dir() {
    local root="$1"
    command -v cargo >/dev/null 2>&1 || return 1

    local metadata resolved=""
    metadata="$(cd "$root" 2>/dev/null && cargo metadata --format-version 1 --no-deps 2>/dev/null)" || metadata=""
    [[ -n "$metadata" ]] || return 1

    if command -v jq >/dev/null 2>&1; then
        resolved="$(printf '%s' "$metadata" | jq -r '.target_directory // empty' 2>/dev/null)"
    else
        resolved="$(printf '%s' "$metadata" | sed -n 's/.*"target_directory":"\([^"]*\)".*/\1/p')"
    fi
    [[ -n "$resolved" && "$resolved" != "null" ]] || return 1

    _loom_ctd_absolutize "$resolved" "$root"
}

loom_resolve_cargo_target_dir() {
    local root="$1"

    if [[ -n "${CARGO_TARGET_DIR:-}" ]]; then
        _loom_ctd_absolutize "$CARGO_TARGET_DIR" "$root"
        return 0
    fi

    local resolved
    if resolved="$(_loom_ctd_metadata_target_dir "$root")"; then
        printf '%s\n' "$resolved"
        return 0
    fi

    printf '%s\n' "$root/target"
}

# loom_cargo_target_dir_redirect_possible <workspace_root>
#
# Cheap pre-check: is a redirect even conceivable *for this worktree*? Returns
# 0 (yes) when the root carries a Cargo manifest AND either CARGO_TARGET_DIR is
# set or a `.cargo/config.toml` INSIDE the root mentions `target-dir`. Returns
# 1 (no) otherwise — in which case this pass treats the target dir as
# `<workspace_root>/target` and the caller can skip the `cargo metadata`
# subprocess entirely.
#
# This keeps the common (unredirected) host at zero added cost per removal:
# a handful of small file reads instead of a cargo invocation.
#
# ## Why the candidates stop at the worktree boundary
#
# This is an ATTRIBUTION question ("is this directory the worktree's own?"),
# not a faithful reimplementation of Cargo's config lookup. A `target-dir` in
# `$CARGO_HOME/config.toml` — or in any ancestor `.cargo/config.toml` above the
# worktree — is exactly as machine- or session-global as the CARGO_TARGET_DIR
# env var: it resolves identically for every path on the host, so it can never
# establish that a directory belongs to the one worktree being removed. Before
# this restriction, a `$CARGO_HOME` redirect made a worktree resolve straight to
# the machine-global cache (which the sharing scan then failed to see any
# referent for, because every manifest-less tree is skipped and the primary
# checkout resolves through cargo, which can fail), and the shared cache was
# `rm -rf`'d. The per-worktree redirect this pass exists to reclaim can only
# come from a config INSIDE the worktree, so nothing reclaimable is lost.
loom_cargo_target_dir_redirect_possible() {
    local root="$1"

    # No manifest ⇒ nothing here ever built with cargo ⇒ nothing to redirect,
    # and in particular an ambient CARGO_TARGET_DIR is NOT evidence that this
    # tree owns the directory it names. This test comes FIRST, before the env
    # short-circuit, so the worktree being removed is judged by exactly the
    # same manifest rule `loom_target_dir_shared_with` already applies to every
    # OTHER worktree. When it came second, a manifest-less worktree resolved to
    # the machine-global env path while every sibling was skipped as a referent
    # — so nothing looked shared and the shared cache was deleted.
    [[ -f "$root/Cargo.toml" ]] || return 1

    # A Loom-provisioned per-worktree redirect (#8458). Read from a file INSIDE
    # the worktree, so — exactly like the `.cargo/config.toml` case below — it is
    # worktree-derived attribution evidence, not a machine-global source.
    [[ -f "$root/$LOOM_WT_TARGET_MARKER" ]] && return 0

    # An ambient CARGO_TARGET_DIR still makes a redirect *possible* (Cargo
    # honors it), so resolution must not skip it — but the reclaim step refuses
    # to delete a path that is only ever that env value. See gate 2f.
    [[ -n "${CARGO_TARGET_DIR:-}" ]] && return 0

    # ONLY the worktree's own config files. Ancestors and $CARGO_HOME are
    # machine-global sources; see the header comment above.
    local f
    for f in "$root/.cargo/config.toml" "$root/.cargo/config"; do
        [[ -f "$f" ]] || continue
        grep -qE '^[[:space:]]*target-dir[[:space:]]*=' "$f" 2>/dev/null && return 0
    done
    return 1
}

# loom_resolve_worktree_target_dir_checked <worktree_path>
#
# Resolve <worktree_path>'s target dir AND report whether the answer is
# trustworthy. Prints the resolved path on stdout in every case.
#
# Exit status:
#   0  the answer is definite (no redirect is configured, or the redirect was
#      read successfully)
#   2  a redirect IS configured for this tree but could not be read (`cargo
#      metadata` is missing, exited non-zero — a mid-edit manifest, a
#      conflicted merge — or emitted no target_directory). stdout carries the
#      degraded `<worktree_path>/target` fallback.
#
# Callers that are about to DELETE something must fail closed on 2. Silently
# degrading to `<root>/target` is how a sibling stops looking like a sharer of
# the dir we are about to remove: "this tree builds somewhere else" and "we
# could not find out where this tree builds" must not look alike, the same rule
# `loom_target_dir_shared_with` already applies to `git worktree list`.
loom_resolve_worktree_target_dir_checked() {
    local worktree_path="$1"

    # #8458: a Loom-provisioned per-worktree redirect wins outright, ahead of
    # even the env var. It is the strongest available *per-worktree* statement:
    # written into the worktree by `loom_provision_worktree_target_dir`, and the
    # same value the spawn path exports as CARGO_TARGET_DIR for that worktree's
    # builds — so in the normal case the two agree and the order is moot. When
    # they disagree (an operator exported a private dir of their own over the
    # top), preferring the marker reclaims only the directory Loom itself
    # provisioned and leaves the operator's alone: the safe direction, and the
    # only one of the two that is attributable to this worktree at all.
    local marker
    if marker="$(loom_read_worktree_target_dir_marker "$worktree_path")"; then
        printf '%s\n' "$marker"
        return 0
    fi

    if ! loom_cargo_target_dir_redirect_possible "$worktree_path"; then
        printf '%s\n' "$worktree_path/target"
        return 0
    fi

    # Env beats config in Cargo, and needs no subprocess: a definite answer.
    if [[ -n "${CARGO_TARGET_DIR:-}" ]]; then
        _loom_ctd_absolutize "$CARGO_TARGET_DIR" "$worktree_path"
        return 0
    fi

    local resolved
    if resolved="$(_loom_ctd_metadata_target_dir "$worktree_path")"; then
        printf '%s\n' "$resolved"
        return 0
    fi

    printf '%s\n' "$worktree_path/target"
    return 2
}

# loom_resolve_worktree_target_dir <worktree_path>
#
# The removal-path entry point: resolve <worktree_path>'s target dir, skipping
# the expensive branch when no redirect is possible. MUST be called while the
# worktree still exists on disk — `cargo metadata` needs its manifest.
#
# Always exits 0. For the worktree BEING REMOVED a degraded answer is the safe
# direction (`<worktree>/target` is `inside` ⇒ a no-op, i.e. a missed reclaim),
# so the status is deliberately dropped here; the sharing scan, where a degraded
# answer would instead license a deletion, uses the `_checked` form above.
loom_resolve_worktree_target_dir() {
    loom_resolve_worktree_target_dir_checked "$1" || true
}

# --------------------------------------------------------------------------
# Reclaim gates
# --------------------------------------------------------------------------

# Best-effort physical path (symlinks resolved) for comparison purposes. A
# non-existent path is returned unchanged rather than dropped: containment
# checks still need something to compare.
_loom_ctd_realpath() {
    local p="$1"
    if [[ -d "$p" ]]; then
        (cd "$p" 2>/dev/null && pwd -P) || printf '%s\n' "$p"
    else
        printf '%s\n' "$p"
    fi
}

# _loom_ctd_machine_global_target_dirs <worktree_path>
#
# Every target-dir value that comes from a MACHINE- OR SESSION-GLOBAL source
# rather than from the worktree itself, one per line as:
#
#     <absolute value>\t<human description of where it came from>
#
# Two such sources exist:
#   * the remover's own CARGO_TARGET_DIR environment variable, and
#   * `build.target-dir` in a `.cargo/config.toml` OUTSIDE the worktree — any
#     ancestor directory, or $CARGO_HOME.
#
# Both resolve identically for every path on the host, so neither can ever be
# evidence that a directory belongs to the one worktree being removed. Used by
# gate 2f. Unlike the resolver, this reads the config files directly: it runs
# AFTER the worktree is off disk, when `cargo metadata` is no longer possible,
# and the files it reads all live outside the worktree so they are still there.
_loom_ctd_machine_global_target_dirs() {
    local worktree_path="$1"

    if [[ -n "${CARGO_TARGET_DIR:-}" ]]; then
        printf '%s\t%s\n' \
            "$(_loom_ctd_absolutize "${CARGO_TARGET_DIR%/}" "$worktree_path")" \
            "the ambient CARGO_TARGET_DIR"
    fi

    local candidates=()
    # Ancestors ABOVE the worktree only — a config inside the worktree is the
    # one legitimate form of per-worktree attribution and must not appear here.
    local dir
    dir="$(dirname "$worktree_path")"
    while [[ -n "$dir" && "$dir" != "/" && "$dir" != "." ]]; do
        candidates+=("$dir/.cargo/config.toml" "$dir/.cargo/config")
        dir="$(dirname "$dir")"
    done
    candidates+=("/.cargo/config.toml" "/.cargo/config")
    local cargo_home="${CARGO_HOME:-${HOME:-}/.cargo}"
    [[ -n "${CARGO_HOME:-}" || -n "${HOME:-}" ]] && candidates+=("$cargo_home/config.toml" "$cargo_home/config")

    local f value base
    for f in "${candidates[@]}"; do
        [[ -f "$f" ]] || continue
        # TOML strings only (`target-dir = "…"` / `'…'`); a value Cargo would
        # reject is not a value we need to compare against.
        value="$(sed -n 's/^[[:space:]]*target-dir[[:space:]]*=[[:space:]]*["'"'"']\([^"'"'"']*\)["'"'"'].*/\1/p' "$f" 2>/dev/null | head -1)"
        [[ -n "$value" ]] || continue
        # Cargo resolves a relative config path against the directory holding
        # the `.cargo` directory. Getting this wrong only costs a refusal we
        # would not otherwise have made, never a deletion.
        base="$(dirname "$(dirname "$f")")"
        printf '%s\t%s\n' "$(_loom_ctd_absolutize "${value%/}" "$base")" "the target-dir in $f"
    done
}

# loom_dir_size_human <dir> — human-readable size, or "unknown".
loom_dir_size_human() {
    local dir="$1" size
    size="$(du -sh "$dir" 2>/dev/null | awk '{print $1}')" || size=""
    printf '%s\n' "${size:-unknown}"
}

# loom_target_dir_holders <dir>
#
# PIDs of live processes whose cwd or executable image is inside <dir> — the
# "never unlink a running program's files" gate the daemon applies to the
# primary checkout's own artifacts. Fail-open (prints nothing) when neither
# /proc nor lsof can answer; the other gates are the primary protection.
loom_target_dir_holders() {
    local dir="$1"
    local self=$$ parent=${PPID:-0}
    local pids=""

    if [[ -d /proc ]]; then
        local link pid target
        for link in /proc/[0-9]*; do
            pid="${link#/proc/}"
            [[ "$pid" =~ ^[0-9]+$ ]] || continue
            [[ "$pid" == "$self" || "$pid" == "$parent" ]] && continue
            local probe
            for probe in cwd exe; do
                target="$(readlink "$link/$probe" 2>/dev/null)" || continue
                [[ -n "$target" ]] || continue
                if [[ "$target" == "$dir" || "$target" == "$dir"/* ]]; then
                    pids+="$pid"$'\n'
                    break
                fi
            done
        done
    elif command -v lsof >/dev/null 2>&1; then
        local pid
        while read -r pid; do
            [[ "$pid" =~ ^[0-9]+$ ]] || continue
            [[ "$pid" == "$self" || "$pid" == "$parent" ]] && continue
            pids+="$pid"$'\n'
        done < <(lsof -t +d "$dir" 2>/dev/null || true)
    fi

    printf '%s' "$pids"
}

# loom_target_dir_shared_with <repo_root> <worktree_path> <resolved>
#
# Prints the path of another LIVE worktree (or the primary checkout) that
# resolves to the same target dir, or nothing when the dir is exclusive to
# <worktree_path>. Registered worktrees come from `git worktree list`, so this
# sees user-provisioned worktrees too, not just `.loom/worktrees/*`.
#
# Containment counts as sharing in BOTH directions: a sibling whose target dir
# is a parent of ours (the host-optimize convention of a single shared
# `target-dir` for the whole machine) must never be unlinked, and neither must
# a parent whose subtree another worktree is building into.
#
# Exit status: 0 = answered (empty stdout means "exclusive"), 2 = the question
# could not be answered at all (`git worktree list` failed: no git, not a repo,
# I/O error), 3 = a live worktree has a redirect configured that could not be
# READ (`cargo metadata` failed for it), so whether it shares this dir is
# unknown. The caller MUST fail closed on both — an empty worktree list and a
# failed enumeration are indistinguishable on stdout, and a sibling that
# silently degraded to `<root>/target` is indistinguishable from one that
# genuinely builds elsewhere. Treating either as "nobody else uses it" is
# precisely how a sibling's cache gets deleted mid-build.
#
# ## The one exception: a Loom-provisioned per-worktree dir (#8458)
#
# When `resolved` carries the per-worktree SHAPE for `worktree_path` —
# `<root>/wt/<that worktree's own directory name>`, which only
# `loom_provision_worktree_target_dir` ever writes — two of the rules above are
# deliberately relaxed, because both of them otherwise veto EVERY reclaim of
# such a dir and the feature can never free a byte:
#
#   * **Containment stops counting; only an exact match does.** The whole point
#     of `<root>/wt/<name>` is that it lives *under* the otherwise-shared root,
#     so the primary checkout (which resolves to `<root>` itself via the host's
#     `~/.cargo/config.toml`) is always a containing "sharer". Deleting
#     `<root>/wt/<name>` cannot harm a tree that builds into `<root>`: cargo
#     writes `debug/`, `release/`, `CACHEDIR.TAG` … directly under its own target
#     dir and never into a `wt/` subtree.
#   * **Other worktrees are resolved with the ambient CARGO_TARGET_DIR ignored.**
#     An absolute env value resolves identically for EVERY path on the host, so
#     when the remover's own environment holds this worktree's per-worktree value
#     (which is exactly what the spawn path exports), every sibling — and the
#     primary checkout — "resolves" to it and looks like a sharer. That is an
#     artifact of the env var, not evidence about the sibling. The per-worktree
#     shape is itself the proof the value is worktree-specific, so the siblings
#     are judged on what THEY carry: their own marker, or their own in-worktree
#     `.cargo/config.toml`.
#
# Neither relaxation is reachable for a path without that shape, so the
# machine-global shared cache this function exists to protect is untouched by it.
loom_target_dir_shared_with() {
    local repo_root="$1" worktree_path="$2" resolved="$3"
    local resolved_real
    resolved_real="$(_loom_ctd_realpath "$resolved")"
    local worktree_real
    worktree_real="$(_loom_ctd_realpath "$worktree_path")"

    local attributed=false
    if loom_is_per_worktree_target_dir "$worktree_path" "$resolved" ||
        loom_is_per_worktree_target_dir "$worktree_path" "$resolved_real"; then
        attributed=true
    fi

    local listing
    listing="$(git -C "$repo_root" worktree list --porcelain 2>/dev/null)" || return 2

    local other other_real other_target other_target_real
    while read -r other; do
        [[ -n "$other" ]] || continue
        [[ -d "$other" ]] || continue
        other_real="$(_loom_ctd_realpath "$other")"
        [[ "$other_real" == "$worktree_real" ]] && continue
        # A tree with no manifest never builds with cargo, so it cannot be
        # depending on this target dir — skip it. This matters beyond
        # performance: an ambient absolute CARGO_TARGET_DIR resolves the SAME
        # for every path, so counting manifest-less trees would report every
        # redirected dir as "shared" and reclaim nothing, ever.
        [[ -f "$other/Cargo.toml" ]] || continue
        # FAIL CLOSED on a degraded answer. `loom_resolve_worktree_target_dir`
        # silently falls back to `<root>/target` when a configured redirect
        # cannot be read — one transient `cargo metadata` failure (a mid-edit
        # Cargo.toml, a conflicted merge) would otherwise stop this sibling
        # from counting as a sharer of the very directory we are about to
        # delete. Same rule as the `git worktree list` failure above.
        if [[ "$attributed" == true ]]; then
            # See "The one exception" above: judge the sibling on what IT
            # carries, with the host-/session-global env value out of the way.
            other_target="$(CARGO_TARGET_DIR="" loom_resolve_worktree_target_dir_checked "$other")" || return 3
        else
            other_target="$(loom_resolve_worktree_target_dir_checked "$other")" || return 3
        fi
        other_target_real="$(_loom_ctd_realpath "$other_target")"
        if [[ "$other_target_real" == "$resolved_real" ]]; then
            printf '%s\n' "$other"
            return 0
        fi
        if [[ "$attributed" != true ]] && \
           [[ "$resolved_real" == "$other_target_real"/* || \
              "$other_target_real" == "$resolved_real"/* ]]; then
            printf '%s\n' "$other"
            return 0
        fi
    done < <(printf '%s\n' "$listing" | awk '/^worktree /{print substr($0, 10)}')

    return 0
}

# loom_reclaim_worktree_target_dir <repo_root> <worktree_path> <resolved> <dry_run>
#
# Decide, and (unless dry_run is "true") act. Emits exactly one
# tab-separated record on stdout:
#
#   <status>\t<path>\t<detail>
#
# status:
#   inside          resolved to the worktree itself — removed with it, no-op
#   absent          nothing on disk at the resolved path
#   refused         a path this pass must never delete (detail = why)
#   shared          another live worktree resolves here (detail = that worktree)
#   protected       live process(es) hold it (detail = pids)
#   would-reclaim   dry run; detail = human size
#   reclaimed       removed; detail = human size
#   failed          removal attempted and failed (detail = error)
_loom_ctd_record() { printf '%s\t%s\t%s\n' "$1" "$2" "$3"; }

loom_reclaim_worktree_target_dir() {
    local repo_root="$1" worktree_path="$2" resolved="$3" dry_run="${4:-false}"

    if [[ -z "$resolved" ]]; then
        _loom_ctd_record "absent" "" "target dir could not be resolved"
        return 0
    fi

    # Normalize away a trailing slash so every comparison below is exact.
    resolved="${resolved%/}"

    local worktree_real repo_real resolved_real
    worktree_real="$(_loom_ctd_realpath "$worktree_path")"
    repo_real="$(_loom_ctd_realpath "$repo_root")"
    resolved_real="$(_loom_ctd_realpath "$resolved")"

    # 1. The default, in-worktree location: it goes away with the worktree.
    if [[ "$resolved_real" == "$worktree_real" || "$resolved_real" == "$worktree_real"/* ]]; then
        _loom_ctd_record "inside" "$resolved" "inside the worktree — removed with it"
        return 0
    fi

    # 2. Paths that must never be deleted by this pass, however they resolved.
    local depth
    depth="$(printf '%s' "${resolved_real#/}" | awk -F/ '{print NF}')"
    if [[ "$resolved_real" == "/" || "${depth:-0}" -lt 2 ]]; then
        _loom_ctd_record "refused" "$resolved" "suspiciously shallow path"
        return 0
    fi
    if [[ -n "${HOME:-}" && "$resolved_real" == "$(_loom_ctd_realpath "$HOME")" ]]; then
        _loom_ctd_record "refused" "$resolved" "resolves to \$HOME"
        return 0
    fi
    if [[ "$resolved_real" == "$repo_real" || "$repo_real" == "$resolved_real"/* ]]; then
        _loom_ctd_record "refused" "$resolved" "contains the repository itself"
        return 0
    fi
    if [[ "$resolved_real" == "$repo_real/target" ]]; then
        # The primary checkout's own build cache. It is regenerable, but it
        # belongs to the deep-clean pass (which gates on disk pressure and the
        # machine build slot), never to a single worktree's removal.
        _loom_ctd_record "refused" "$resolved" "the primary checkout's own target/"
        return 0
    fi

    # 2f. The resolved path is nothing but a MACHINE-GLOBAL redirect value: the
    #     remover's own ambient CARGO_TARGET_DIR, or a `build.target-dir`
    #     declared by a `.cargo/config.toml` outside the worktree ($CARGO_HOME
    #     or an ancestor directory). Neither is read from anything belonging to
    #     the worktree, and both resolve identically for every path on the host,
    #     so neither can establish that this directory is exclusive to the
    #     worktree being removed — while the sharing scan below deliberately
    #     skips manifest-less trees, leaving a shared cache with no visible
    #     referent at all.
    #
    #     This costs the feature nothing: the per-worktree redirect this pass
    #     exists to reclaim comes from `build.target-dir` in a `.cargo/config.toml`
    #     INSIDE the worktree, whose value is per-worktree by construction and so
    #     does not equal any of these. A genuinely per-worktree CARGO_TARGET_DIR
    #     exported into the remover's environment merely gets reported instead of
    #     deleted — the safe direction.
    #
    #     Backstop, not the primary defense: `loom_cargo_target_dir_redirect_possible`
    #     already declines to resolve THROUGH an out-of-worktree config at all.
    #     This gate additionally catches a worktree-local config that names the
    #     same directory a machine-global one does.
    #
    #     #8458 EXEMPTION: a value carrying the Loom per-worktree SHAPE —
    #     `<root>/wt/<the removed worktree's own directory name>` — is
    #     per-worktree BY CONSTRUCTION, whichever variable happens to be holding
    #     it. The shape is checked against `$worktree_path`'s own basename, so
    #     matching it is itself the attribution: a machine-global shared root
    #     (`/big/cargo-target`) can never satisfy it, and the only way an ambient
    #     CARGO_TARGET_DIR can is by naming exactly the directory the spawn path
    #     provisioned for this worktree. Without this, the per-worktree scheme
    #     would be refused on every removal made from inside the very sweep that
    #     owns the worktree — which is every removal that matters.
    #
    #     Structural rather than marker-reading on purpose: this gate runs AFTER
    #     the worktree is off disk, so the in-worktree marker that produced the
    #     value is already gone and cannot be consulted here.
    local mg_value mg_source
    while IFS=$'\t' read -r mg_value mg_source; do
        [[ -n "$mg_value" ]] || continue
        if [[ "$resolved_real" == "$(_loom_ctd_realpath "$mg_value")" ]]; then
            if loom_is_per_worktree_target_dir "$worktree_path" "$resolved" ||
                loom_is_per_worktree_target_dir "$worktree_path" "$resolved_real"; then
                break
            fi
            _loom_ctd_record "refused" "$resolved" \
                "$mg_source is machine-global, not exclusive to this worktree"
            return 0
        fi
    done < <(_loom_ctd_machine_global_target_dirs "$worktree_path")

    # 3. Nothing there (never built, or already reclaimed).
    if [[ ! -d "$resolved_real" ]]; then
        _loom_ctd_record "absent" "$resolved" "no directory at the resolved path"
        return 0
    fi

    # 4. Shared with a still-live worktree (the host-optimize single-shared-
    #    target-dir convention). Deleting it would destroy a sibling's cache
    #    mid-build. Exit 2 means the question was unanswerable — fail closed.
    # `|| shared_rc=$?` rather than a bare assignment plus `$?`: the non-zero
    # fail-closed statuses must reach the checks below intact even when a
    # caller (worktree.sh, merge-pr.sh) runs under `set -e`.
    local sharer="" shared_rc=0
    sharer="$(loom_target_dir_shared_with "$repo_root" "$worktree_path" "$resolved_real")" || shared_rc=$?
    if [[ "$shared_rc" -eq 2 ]]; then
        _loom_ctd_record "refused" "$resolved" \
            "could not enumerate live worktrees (git worktree list failed)"
        return 0
    fi
    if [[ "$shared_rc" -eq 3 ]]; then
        _loom_ctd_record "refused" "$resolved" \
            "could not resolve a live worktree's target dir (cargo metadata failed)"
        return 0
    fi
    if [[ -n "$sharer" ]]; then
        _loom_ctd_record "shared" "$resolved" "$sharer"
        return 0
    fi

    # 5. A running process is using it. Checked under dry_run too: a preview
    #    that claims it "would remove" a live build's output is a preview an
    #    operator would act on.
    local holders
    holders="$(loom_target_dir_holders "$resolved_real")"
    if [[ -n "$holders" ]]; then
        _loom_ctd_record "protected" "$resolved" "pid(s) $(printf '%s' "$holders" | tr '\n' ' ' | sed 's/ $//')"
        return 0
    fi

    local size
    size="$(loom_dir_size_human "$resolved_real")"
    if [[ "$dry_run" == true ]]; then
        _loom_ctd_record "would-reclaim" "$resolved" "$size"
        return 0
    fi

    local err
    if err="$(rm -rf "$resolved_real" 2>&1)"; then
        _loom_ctd_record "reclaimed" "$resolved" "$size"
    else
        _loom_ctd_record "failed" "$resolved" "${err:-rm failed}"
    fi
    return 0
}

# --------------------------------------------------------------------------
# Per-worktree target dirs (issue #8458)
# --------------------------------------------------------------------------
#
# ## What this buys
#
# Cargo keys a *workspace* crate's artifacts (and its incremental session) by
# the crate's ABSOLUTE SOURCE PATH, so two worktrees never share workspace-crate
# build output even inside one shared target dir — the sharing buys nothing for
# the crates Loom actually rebuilds, and costs two things:
#
#   * unbounded growth (issue #8453: 213 GB of `debug/incremental/` and 231 GB of
#     `debug/deps/` on one host, ~6400 orphaned session dirs — the output of
#     every worktree ever built, which nothing prunes because it does not live
#     under the worktree), and
#   * WRONG TEST RESULTS. Cargo "uplifts" the final binary to one un-hashed path,
#     `<target>/debug/loom-daemon`, overwritten by whichever worktree built last.
#     Integration tests execute that path. #8453 records three incidents in a
#     single day, including a Judge run with 12 false failures that passed
#     194/194 in isolation.
#
# Giving each worktree `<root>/wt/<worktree name>` fixes both: the uplifted
# binary is per-worktree, so a concurrent sibling cannot substitute its own, and
# the directory is attributable so the removal paths above can reclaim it.
#
# ## Why a marker file and not `.cargo/config.toml`
#
# #7239's attribution rule is that ONLY a redirect derived from the worktree
# itself can prove a directory belongs to it, and it names `build.target-dir` in
# a `.cargo/config.toml` INSIDE the worktree as the way that happens. That
# vehicle is unavailable in a repo that TRACKS `.cargo/config.toml` (Loom itself
# does — it carries the CI-mirroring clippy `rustflags`): writing the redirect
# there would leave every worktree with a modified tracked file, showing up in
# `git status` and one careless `git add -A` away from being committed.
#
# So the per-worktree redirect is recorded in `$LOOM_WT_TARGET_MARKER`, a
# gitignored Loom runtime marker in the same family as `.loom-managed`. It is
# inside the worktree, so it is worktree-derived evidence in exactly the sense
# #7239 requires; it is not a Cargo config, so it does not touch tracked state.
# Cargo itself learns the redirect from `CARGO_TARGET_DIR`, exported by the spawn
# path (spawn-claude.sh) and by `worktree.sh` for its own post-worktree hook.
#
# ## The shape is the attribution
#
# `<root>/wt/<worktree name>` is checked structurally by
# `loom_is_per_worktree_target_dir`, against the basename of the worktree being
# asked about. That predicate is what licenses the two relaxations documented at
# `loom_target_dir_shared_with` and gate 2f, and it is deliberately narrow: a
# machine-global root (`/big/cargo-target`, `$CARGO_HOME`-configured or
# env-exported) cannot satisfy it, and a corrupted/hand-written marker can only
# ever name `<something>/wt/<this worktree's own name>` — never a parent, never a
# sibling's directory, never the shared root.

# Loom runtime marker recording a worktree's provisioned target dir. Gitignored
# via the loom-managed block (loom-daemon/src/init/post_init.rs) and filtered out
# of worktree.sh's dirty-worktree guard, like every other marker in its family.
LOOM_WT_TARGET_MARKER=".loom-cargo-target-dir"

# The single path component that separates per-worktree dirs from whatever else
# lives under the shared root. Load-bearing: it is half of the structural
# attribution check.
LOOM_WT_TARGET_SUBDIR="wt"

# loom_per_worktree_target_dir_enabled <repo_root>
#
# Exit 0 when per-worktree target dirs are turned on for this repo.
# `LOOM_PER_WORKTREE_TARGET_DIR` (env) > `cargo.perWorktreeTargetDir` (config) >
# default OFF.
#
# Default OFF is deliberate, and is the #6013/#6014 lesson rather than timidity:
# on a host with no redirect configured the scheme is a pure no-op anyway (see
# `loom_provision_worktree_target_dir`), and on a host WITH one it trades
# cross-worktree reuse of third-party `deps/` for isolation. That trade is
# excellent when a `rustc-wrapper` (sccache) carries third-party crates — the
# configuration #8453 measured — and is a fleet-wide rebuild storm when nothing
# does, which is precisely the #6013/#6014 incident. So it is the operator's
# switch, on the same host where the shared `build.target-dir` was configured.
loom_per_worktree_target_dir_enabled() {
    local repo_root="$1" value="${LOOM_PER_WORKTREE_TARGET_DIR:-}"

    if [[ -z "$value" ]]; then
        local resolver
        resolver="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/config-resolver.sh"
        if [[ -f "$resolver" ]]; then
            # shellcheck source=./config-resolver.sh
            source "$resolver"
            value="$(loom_config_get "$repo_root" "cargo.perWorktreeTargetDir" "" 2>/dev/null)" || value=""
        fi
    fi

    case "$value" in
        1 | true | yes | on) return 0 ;;
        *) return 1 ;;
    esac
}

# loom_per_worktree_target_dir_path <target_root> <worktree_name>
#
# Derive `<target_root>/wt/<worktree_name>`. IDEMPOTENT: when `<target_root>` is
# already this worktree's per-worktree dir it is returned unchanged rather than
# nested a second level. That case is routine, not pathological — the spawn path
# exports CARGO_TARGET_DIR for the sweep, so `worktree.sh`, running inside that
# sweep, re-resolves the env value as its "root". Without this guard the second
# invocation would provision `<root>/wt/issue-N/wt/issue-N`, and the marker and
# the env var would then name different directories.
loom_per_worktree_target_dir_path() {
    local root="${1%/}" name="$2"
    if [[ "$root" == *"/$LOOM_WT_TARGET_SUBDIR/$name" ]]; then
        printf '%s\n' "$root"
        return 0
    fi
    printf '%s/%s/%s\n' "$root" "$LOOM_WT_TARGET_SUBDIR" "$name"
}

# loom_is_per_worktree_target_dir <worktree_path> <candidate>
#
# Exit 0 when <candidate> has the Loom per-worktree shape FOR <worktree_path>:
# an absolute path of at least three components ending in
# `/wt/<basename of worktree_path>`. Purely structural — no disk access — so it
# is still answerable after the worktree has been removed, which is what gate 2f
# needs.
#
# This predicate licenses deletions, so it is written to be un-widenable: the
# leaf must equal the worktree's own directory name, which ties the directory to
# one worktree by name and makes it impossible for a truncated or hand-edited
# value to name the shared root, a parent of it, or another worktree's dir.
loom_is_per_worktree_target_dir() {
    local worktree_path="${1%/}" candidate="${2%/}" name
    name="$(basename "$worktree_path")"

    [[ -n "$name" && "$name" != "/" && "$name" != "." ]] || return 1
    [[ "$candidate" == /* ]] || return 1
    [[ "$candidate" == *"/$LOOM_WT_TARGET_SUBDIR/$name" ]] || return 1

    # `/wt/<name>` alone is two components; require a real root above it so the
    # "suspiciously shallow path" family can never be reached through here.
    local depth
    depth="$(printf '%s' "${candidate#/}" | awk -F/ '{print NF}')"
    [[ "${depth:-0}" -ge 3 ]]
}

# loom_read_worktree_target_dir_marker <worktree_path>
#
# Print the target dir recorded by `loom_provision_worktree_target_dir` for this
# worktree, or exit 1 when there is no usable marker. MUST be called while the
# worktree is still on disk.
#
# Every failure mode — absent marker, empty marker, a value that does not carry
# the per-worktree shape, a tree with no Cargo manifest — exits 1, so a corrupt
# marker degrades to "no per-worktree redirect" (the pre-#8458 behavior) rather
# than to a path this library would then act on. The manifest requirement
# deliberately mirrors `loom_cargo_target_dir_redirect_possible`'s first test:
# a tree cargo never built in must resolve to its own in-worktree `target/`,
# which is the #7239 regression the manifest-first ordering pins.
loom_read_worktree_target_dir_marker() {
    local worktree_path="${1%/}" value
    local file="$worktree_path/$LOOM_WT_TARGET_MARKER"

    [[ -f "$worktree_path/Cargo.toml" ]] || return 1
    [[ -s "$file" ]] || return 1

    value="$(head -n 1 "$file" 2>/dev/null)" || return 1
    value="${value%$'\r'}"
    value="${value%/}"
    [[ -n "$value" ]] || return 1

    loom_is_per_worktree_target_dir "$worktree_path" "$value" || return 1
    printf '%s\n' "$value"
}

# _loom_ctd_provision_report <status> <path> <detail>
#
# One human line on STDERR. Stderr, not stdout, because `worktree.sh --json`
# holds stdout for its JSON document and the provisioning call site sits inside
# that flow; and a line rather than a machine record because — unlike the
# reclaim — nothing downstream needs to branch on the outcome.
_loom_ctd_provision_report() {
    case "$1" in
        provisioned) printf '  per-worktree cargo target dir: %s\n' "$2" >&2 ;;
        existing) printf '  per-worktree cargo target dir (reused): %s\n' "$2" >&2 ;;
        failed) printf '  could not provision a per-worktree cargo target dir: %s\n' "$3" >&2 ;;
        *) : ;; # disabled / not-cargo / unredirected: silent, the common cases
    esac
}

# loom_provision_worktree_target_dir <repo_root> <worktree_path>
#
# Give <worktree_path> its own Cargo target dir and record it, so that (a) the
# builds Loom drives in that worktree cannot collide with a sibling's, and (b)
# the removal paths can attribute and reclaim the directory.
#
# On success exports `LOOM_WORKTREE_CARGO_TARGET_DIR` into the CALLER's shell.
# That variable, not `CARGO_TARGET_DIR`, is deliberately what gets exported here:
# `worktree.sh` goes on to run the post-worktree hook, whose whole job is to find
# the MAIN workspace's pre-built binary, and an ambient `CARGO_TARGET_DIR`
# pointing at the brand-new (empty) per-worktree dir would make that binary look
# missing on every single worktree creation — which is #6013/#6014's rebuild
# storm, reintroduced by the fix for #8453. The hook reads the destination from
# `LOOM_WORKTREE_CARGO_TARGET_DIR` (or from the marker) and keeps resolving the
# SOURCE the way it always did.
#
# Always exits 0: a worktree must still be created when this cannot be done.
# Statuses (reported to stderr, see above): `existing`, `not-cargo`, `disabled`,
# `unredirected`, `refused`, `failed`, `provisioned`.
loom_provision_worktree_target_dir() {
    local repo_root="$1" worktree_path="${2%/}"
    local name root dir existing worktree_real root_real

    # An existing valid marker is authoritative: `worktree.sh <N>` is idempotent
    # and re-run against live worktrees, and re-deriving could relocate the dir
    # out from under build output that is already there.
    if existing="$(loom_read_worktree_target_dir_marker "$worktree_path")"; then
        mkdir -p "$existing" 2>/dev/null || true
        export LOOM_WORKTREE_CARGO_TARGET_DIR="$existing"
        _loom_ctd_provision_report "existing" "$existing" ""
        return 0
    fi

    [[ -f "$worktree_path/Cargo.toml" ]] || { _loom_ctd_provision_report "not-cargo" "" ""; return 0; }
    if ! loom_per_worktree_target_dir_enabled "$repo_root"; then
        _loom_ctd_provision_report "disabled" "" ""
        return 0
    fi

    # FULL resolution, not the cheap `loom_cargo_target_dir_redirect_possible`
    # pre-check: this is the "what would Cargo do here" question, and the shared
    # root that makes this feature worth having comes from `~/.cargo/config.toml`
    # — a source the attribution pre-check deliberately refuses to look at. The
    # `cargo metadata` subprocess is only paid when the feature is enabled, and
    # only once per worktree creation.
    root="$(loom_resolve_cargo_target_dir "$worktree_path")"
    root="${root%/}"

    # Act ONLY when Cargo's output would otherwise land outside the worktree.
    # An unredirected host already has a per-worktree target dir (`<worktree>/
    # target`, removed with the worktree), so there is nothing to fix and no
    # reason to move its build cache onto a new path — and "no redirect
    # configured" is exactly the population that a per-worktree split would cost
    # a full rebuild for no benefit.
    worktree_real="$(_loom_ctd_realpath "$worktree_path")"
    root_real="$(_loom_ctd_realpath "$root")"
    if [[ "$root_real" == "$worktree_real" || "$root_real" == "$worktree_real"/* ]]; then
        _loom_ctd_provision_report "unredirected" "$root" ""
        return 0
    fi

    name="$(basename "$worktree_path")"
    dir="$(loom_per_worktree_target_dir_path "$root" "$name")"

    # Self-check: never write a marker this library's own reclaim path would not
    # recognize. A shape mismatch here would strand the directory forever.
    if ! loom_is_per_worktree_target_dir "$worktree_path" "$dir"; then
        _loom_ctd_provision_report "refused" "$dir" "derived path is not attributable to $name"
        return 0
    fi

    if ! mkdir -p "$dir" 2>/dev/null; then
        _loom_ctd_provision_report "failed" "$dir" "could not create $dir"
        return 0
    fi
    if ! printf '%s\n' "$dir" > "$worktree_path/$LOOM_WT_TARGET_MARKER" 2>/dev/null; then
        _loom_ctd_provision_report "failed" "$dir" "could not write $worktree_path/$LOOM_WT_TARGET_MARKER"
        return 0
    fi

    export LOOM_WORKTREE_CARGO_TARGET_DIR="$dir"
    _loom_ctd_provision_report "provisioned" "$dir" ""
    return 0
}
