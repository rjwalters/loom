#!/usr/bin/env bash
# champion-bot-pr.sh - Champion's trusted-bot dependency-PR classifier (#4765).
#
# THIN STUB. The implementation is `loom-daemon bot-pr` (Rust,
# `loom-daemon/src/bot_pr/`). This entry point exists because Champion's role
# prompt invokes it BY PATH and `eval`s its stdout, the same shape
# `classify-dependency-block.sh` already has.
#
# WHAT IT ANSWERS
#
# Dependabot PRs are structurally unmergeable by Champion: `Cargo.toml` /
# `package.json` are on criterion #3's critical-file blocklist, and the bumps
# age past criterion #5 into Doctor - which does not know `@dependabot rebase`
# is the correct refresh mechanism. So dependency updates, including security
# patches, accumulate forever. This classifier answers, deterministically,
# whether ONE PR is a trusted-bot, dependency-manifest-only change whose
# critical-file exclusion may be waived and whose staleness should be answered
# with a rebase request instead of a Doctor cycle. It never decides to merge:
# Judge approval (`loom:pr`), mergeable state and CI-green stay hard
# requirements, and CI is the real gate.
#
# Usage:
#   champion-bot-pr.sh config [--repo-root <path>]
#   champion-bot-pr.sh classify --author <login> [options] < <unified-diff>
#
# classify options:
#   --author <login>      `author.login` from `gh pr view --json author`. Never
#                         a branch name or title - both are forgeable.
#   --title <text>        PR title, for the dependabotMaxSemver guard.
#   --body-file <path>    PR body, for a grouped PR whose member bumps are
#                         listed there rather than in the title.
#   --files-from <path>   Authoritative changed-file list, one path per line,
#                         from `gh api .../pulls/N/files --paginate`. Defaults
#                         to the paths the diff itself names.
#   --repo-root <path>    Checkout to resolve config for.
#
# Output: `KEY='value'` lines, safe to `eval`. See
# .loom/docs/champion-bot-prs.md for the key list and the config block.
#
# Exit codes:
#   0  qualifies (config: the feature is enabled)
#   1  does not qualify - a verdict, not an error
#   2  invalid arguments, unreadable input, or no loom-daemon
#
# 2, not 1, when the binary is missing: exit 1 here MEANS "does not qualify",
# which a caller acts on by falling back to the strict criteria. That fallback
# is safe, so a missing binary reported as 1 would hide a broken install behind
# a plausible-looking answer forever.

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

# shellcheck source=lib/locate-daemon-bin.sh
source "$SCRIPT_DIR/lib/locate-daemon-bin.sh"

DAEMON_BIN="${LOOM_DAEMON_SELF_BIN:-$(loom_locate_daemon_bin "$SCRIPT_DIR")}"

if [[ -z "$DAEMON_BIN" ]]; then
    echo "[ERROR] champion-bot-pr: loom-daemon binary not found." >&2
    echo "Build it with: cd loom-daemon && cargo build --release" >&2
    exit 2
fi

exec "$DAEMON_BIN" bot-pr "$@"
