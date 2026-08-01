#!/usr/bin/env bash
# locate-daemon-bin.sh — Resolve the loom-daemon binary to invoke.
#
# Source this file (do not exec). Defines two functions:
#
#   loom_locate_daemon_bin <repo_root> -> echoes the absolute path to a
#   loom-daemon binary on stdout, or an empty string if none could be
#   resolved.
#
#   loom_daemon_bin_search_paths <repo_root> -> echoes the ordered list of
#   locations that WOULD be checked (one per line, in precedence order),
#   for use in "binary not found" error text. Does not touch the
#   filesystem itself -- it just renders the same candidate list
#   loom_locate_daemon_bin walks.
#
# Resolution precedence (first match wins):
#   1. $LOOM_DAEMON_BIN — must be executable.
#   2. `loom-daemon` on PATH.
#   3. The machine-level install location: $LOOM_DAEMON_BIN_DIR (default
#      $HOME/.local/bin) — this is where loom-daemon-update.sh's --provision
#      path installs, and the location `ssh host 'cmd'` (a non-interactive
#      shell that never sources the login profile, so $HOME/.local/bin is
#      NOT on PATH) needs an explicit check for (#4875).
#   4. Build-output-relative candidates under <repo_root>:
#        loom-daemon/target/release/loom-daemon
#        loom-daemon/target/debug/loom-daemon
#        target/release/loom-daemon
#        target/debug/loom-daemon
#
# Extracted (issue #4080) from the identical inline copies in
# loom-daemon-start.sh and loom-daemon-update.sh so probe-tokens.sh's
# daemon-binary resolution does not add a fourth copy of this logic. #4875
# added the machine-level install fallback (step 3) plus
# loom_daemon_bin_search_paths(), and migrated the remaining inline copies in
# loom-daemon-start.sh / loom-daemon-watchdog.sh / loom-daemon-update.sh /
# loom-status.sh / .loom/bin/loom onto this single shared definition so a
# future new candidate path never has to be ported by hand across six copies
# again.

loom_locate_daemon_bin() {
    local root="$1"
    if [[ -n "${LOOM_DAEMON_BIN:-}" && -x "${LOOM_DAEMON_BIN}" ]]; then
        echo "${LOOM_DAEMON_BIN}"; return 0
    fi
    if command -v loom-daemon >/dev/null 2>&1; then
        command -v loom-daemon; return 0
    fi
    local machine_bin="${LOOM_DAEMON_BIN_DIR:-$HOME/.local/bin}/loom-daemon"
    if [[ -x "$machine_bin" ]]; then
        echo "$machine_bin"; return 0
    fi
    local candidate
    for candidate in \
        "$root/loom-daemon/target/release/loom-daemon" \
        "$root/loom-daemon/target/debug/loom-daemon" \
        "$root/target/release/loom-daemon" \
        "$root/target/debug/loom-daemon"; do
        if [[ -x "$candidate" ]]; then echo "$candidate"; return 0; fi
    done
    echo ""
}

# loom_daemon_bin_search_paths <repo_root> -- render the candidate list for
# error messages. Mirrors loom_locate_daemon_bin's precedence exactly (kept
# side by side in this same file so the two can never drift apart).
loom_daemon_bin_search_paths() {
    local root="$1"
    if [[ -n "${LOOM_DAEMON_BIN:-}" ]]; then
        echo "\$LOOM_DAEMON_BIN=${LOOM_DAEMON_BIN}"
    fi
    echo "loom-daemon on \$PATH"
    echo "${LOOM_DAEMON_BIN_DIR:-$HOME/.local/bin}/loom-daemon"
    echo "$root/loom-daemon/target/release/loom-daemon"
    echo "$root/loom-daemon/target/debug/loom-daemon"
    echo "$root/target/release/loom-daemon"
    echo "$root/target/debug/loom-daemon"
}
