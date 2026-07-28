#!/usr/bin/env bash
# scripts/install/provision-dispatcher.sh — machine-level `loom` dispatcher
# provisioning (Epic #3835 Phase 3a, #4157).
#
# Sibling of provision-daemon.sh. Where that installs the loom-daemon *binary*
# to ~/.local/bin/loom-daemon, this installs the `loom` *dispatcher* to
# ~/.local/bin/loom AND establishes the machine-level Loom checkout at
# ~/.local/share/loom that the dispatcher resolves into.
#
# Source this file with:
#     source "$LOOM_ROOT/scripts/install/provision-dispatcher.sh"
# then call `provision_loom_dispatcher "$LOOM_ROOT" [bin_dir] [checkout_dir]`.
#
# Deliberately self-contained (defines its own output helpers) so the test suite
# can source it without pulling in the full installer.
#
# ── AC1: checkout resolution (fresh clone vs link to an existing dev checkout) ──
# The installer always runs FROM a Loom source checkout ($LOOM_ROOT). Rather than
# cloning a *second* copy — which a developer running Loom on the Loom repo could
# then let drift out of sync with their working tree — this provisions the machine
# checkout as a SYMLINK ~/.local/share/loom -> $LOOM_ROOT. A symlink cannot
# diverge, so AC1's hard constraint ("must not end up with two divergent copies")
# holds by construction. If ~/.local/share/loom already exists as a real
# directory (an operator's pre-existing standalone clone), it is LEFT AS-IS and
# never clobbered — that is the supported "fresh clone" resolution.
#
# ── AC6: this provisioning writes EXACTLY two machine-level paths ──────────────
# ~/.local/bin/loom (the dispatcher) and ~/.local/share/loom (the checkout link).
# It never adds, removes, or repoints any of the ~/.local/bin/loom-* Python
# console-script symlinks (Epic #4081's scope).

# Emit a dispatcher-provision status line (plain text so sourcing tests can
# assert on it).
_pdisp_ok()   { echo "  [loom-dispatcher] $*"; }
_pdisp_warn() { echo "  [loom-dispatcher] WARNING: $*" >&2; }

# Set on every return (success or soft-failure) so the caller can VERIFY the
# destinations it wrote rather than trust a success message (the #4053
# "expose enough for the caller to verify" contract, matching
# provision-daemon.sh's PROVISIONED_DAEMON_BIN). Assigned as GLOBALS (no `local`)
# so they survive the function return. Consumed by callers/tests, not this file.
# shellcheck disable=SC2034
PROVISIONED_DISPATCHER_BIN=""
# shellcheck disable=SC2034
PROVISIONED_LOOM_CHECKOUT=""

# provision_loom_dispatcher <loom_root> [bin_dir] [checkout_dir]
#
# Best-effort — never fatal. Returns 1 on a soft failure so the caller can note
# it, but the installer must NOT abort on a non-zero return.
provision_loom_dispatcher() {
    local loom_root="${1:-}"
    local bin_dir="${2:-${LOOM_DAEMON_BIN_DIR:-$HOME/.local/bin}}"
    local checkout_dir="${3:-$HOME/.local/share/loom}"
    local src="$loom_root/scripts/loom"
    local dest_bin="$bin_dir/loom"

    # Publish resolved destinations up front so EVERY return path communicates
    # where things live (the caller gates on the return code).
    PROVISIONED_DISPATCHER_BIN="$dest_bin"
    PROVISIONED_LOOM_CHECKOUT="$checkout_dir"

    if [[ -z "$loom_root" || ! -f "$src" ]]; then
        _pdisp_warn "dispatcher source not found at '${src:-<unset>}'; skipping."
        return 1
    fi

    local soft_fail=0

    # ── 1. establish the machine-level checkout (AC1) ─────────────────────────
    if [[ -L "$checkout_dir" ]]; then
        local cur
        cur="$(readlink "$checkout_dir" 2>/dev/null || true)"
        if [[ "$cur" == "$loom_root" ]]; then
            _pdisp_ok "checkout already linked: $checkout_dir -> $loom_root"
        else
            _pdisp_warn "checkout $checkout_dir already links elsewhere ($cur); leaving as-is."
        fi
    elif [[ -e "$checkout_dir" ]]; then
        # A real directory (or file) already there — an operator's standalone
        # clone is the supported "fresh clone" resolution; never clobber it.
        _pdisp_warn "checkout $checkout_dir already exists (not a symlink); leaving as-is."
    else
        if mkdir -p "$(dirname "$checkout_dir")" 2>/dev/null \
            && ln -s "$loom_root" "$checkout_dir" 2>/dev/null; then
            _pdisp_ok "linked machine checkout: $checkout_dir -> $loom_root"
        else
            _pdisp_warn "could not create checkout link at $checkout_dir"
            soft_fail=1
        fi
    fi

    # ── 2. install the dispatcher to ~/.local/bin/loom (AC2) ──────────────────
    if ! mkdir -p "$bin_dir" 2>/dev/null; then
        _pdisp_warn "could not create $bin_dir; skipping dispatcher install."
        return 1
    fi
    if install -m 755 "$src" "$dest_bin" 2>/dev/null \
        || { cp -f "$src" "$dest_bin" 2>/dev/null && chmod 755 "$dest_bin" 2>/dev/null; }; then
        _pdisp_ok "installed loom dispatcher -> $dest_bin"
    else
        _pdisp_warn "failed to install dispatcher to $dest_bin"
        return 1
    fi

    _pdisp_check_path "$bin_dir"
    return "$soft_fail"
}

# Warn (one clear line, never fatal) when <dir> is not on PATH.
_pdisp_check_path() {
    local dir="$1"
    case ":${PATH:-}:" in
        *":$dir:"*) return 0 ;;
        *)
            _pdisp_warn "$dir is not on your PATH — add it so 'loom' resolves:"
            _pdisp_warn "    export PATH=\"$dir:\$PATH\"   # add to ~/.zshrc or ~/.bashrc"
            return 0
            ;;
    esac
}
