#!/usr/bin/env bash
# check-daemon-subcommand-versions.sh — every shell script that depends on a
# `loom-daemon` subcommand must declare the minimum daemon version it needs
# (issue #8285).
#
# ---------------------------------------------------------------------------
# WHY
# ---------------------------------------------------------------------------
# A shell script that shells out to `loom-daemon <subcommand>` acquires a
# version floor the moment that subcommand is added: every host whose resolved
# binary predates it now fails. The failure is correct — `merge-pr.sh`'s
# closing-reference analysis fails CLOSED on purpose, because an empty answer is
# indistinguishable from "no closing refs" and would silently close an
# unfinished issue — but the floor itself is invisible. Nothing in the tree
# records WHICH version each script needs, so:
#
#   * the refusal cannot name the version to roll to (all it can say is "a
#     loom-daemon predating #NNNN has no such subcommand");
#   * a reviewer cannot see that a diff just moved a fleet-wide version floor;
#   * the operator finds out when merges stop on a host.
#
# That is not hypothetical. On 2026-09-18 an operator host ran 0.19.161 while
# `main` carried 0.19.170+ and the auto-update loop was deferring behind the
# build-stampede guard (#8252). `.loom/scripts` is a symlink into
# `defaults/scripts` in the primary checkout, so the instant `main` was pulled,
# every merge on that host stopped. A second instance landed a day later, in
# `skip-labels.sh`, whose bare `exec` produced only clap's "unrecognized
# subcommand" with no remediation at all.
#
# ---------------------------------------------------------------------------
# THE MARKER
# ---------------------------------------------------------------------------
# A script declares its floor with a comment, anywhere in the file (by
# convention in the header, or immediately above the invocation):
#
#   # requires-daemon: <subcommand> >= <version>    <free-text note>
#   # requires-daemon: <subcommand> optional        <free-text note>
#
# `>= <version>` is a HARD requirement: the script cannot do its job without
# that subcommand, and an older binary must be refused with an actionable
# message naming this version. `optional` says the script probes for the
# subcommand and degrades gracefully when it is absent (`merge-pr.sh`'s
# `loom-daemon forge auto-merge` ladder is the canonical example — it falls back
# to the shell path). Both are declarations; the point is that the author made
# the choice explicitly and a reviewer can see it.
#
# The marker is machine-readable so the refusal can quote it. `merge-pr.sh`
# reads its own marker out of `${BASH_SOURCE[0]}` when it refuses, so the
# version in the message and the version this gate enforces are the same string
# — there is no second copy to drift.
#
# ---------------------------------------------------------------------------
# WHAT THIS IS / IS NOT
# ---------------------------------------------------------------------------
# NOT a demand that every existing invocation be annotated first. There are
# dozens of them and most predate the convention; a gate nobody can satisfy gets
# disabled. So this is a ONE-WAY RATCHET, the same shape as
# check-pipefail-early-exit.sh (#7790) and check-file-size-budget.sh (#7711):
# every (file, subcommand) pair that exists today is recorded in
# scripts/daemon-subcommand-baseline.txt and grandfathered. A file may drop a
# pair freely; it may not gain a NEW one without declaring the floor. The gate
# therefore fires at exactly the moment someone adds a new daemon dependency —
# which is the moment the review question "what version does this need?" is
# actually answerable.
#
# NOT an assertion that a declared version is FACTUALLY the first release
# carrying the subcommand. Nothing in a bare checkout can establish that. What
# is checked is that the declaration exists, is well formed, names a subcommand
# the file actually invokes, and does not name a version this repo has not
# reached yet (a floor above VERSION is a typo or a copy-paste, and would make
# every host refuse forever).
#
# ---------------------------------------------------------------------------
# DETECTION, and its deliberate limits
# ---------------------------------------------------------------------------
# Command-position based, not a grep for the word `loom-daemon`. Each line is
# de-commented, quoted PROSE is blanked (a quoted run that is neither a command
# substitution nor a bare parameter expansion cannot be a command, so
# `warn "run loom-daemon foo"` is not an invocation), the result is split on
# command separators, and only the LEADING token of each fragment is considered.
# A fragment counts when its leading token is:
#
#   * `loom_exec_script_helper <sub>`  — the thin-stub shape
#     (lib/script-helper.sh);
#   * a resolved daemon binary — the literal `loom-daemon`, `$LOOM_DAEMON_BIN` /
#     `$LOOM_DAEMON_SELF_BIN` (with or without a `:-` default), or a local
#     variable this file assigned from one of the resolver entry points
#     (`loom_locate_daemon_bin`, `loom_daemon_self_bin_override`,
#     `loom_resolve_self_daemon_bin`), from `command -v loom-daemon`, or from
#     one of those env vars.
#
# Only the FIRST subcommand token is recorded, so `merge-pr verdict-contradiction`
# is recorded as `merge-pr`. That is the clap subcommand whose presence actually
# gates the call, and trying to guess how deep the nesting goes would mistake
# `tokens --json` for a two-level subcommand.
#
# Limits, all accepted: a binary resolved into a variable this file never
# assigns (passed in from a caller's environment under a novel name) is not
# seen; a subcommand built up in a variable and expanded is not seen; test
# suites are skipped entirely (they pin their own binary through
# tests/lib/require-daemon-bin.sh, which already verifies the subcommand
# exists). This is a ratchet on a known class, not a proof of absence.
#
# ---------------------------------------------------------------------------
# Usage:
#   check-daemon-subcommand-versions.sh              Check the tree against the baseline.
#   check-daemon-subcommand-versions.sh --list       Print every detected pair.
#   check-daemon-subcommand-versions.sh --update     Regenerate the baseline.
#   check-daemon-subcommand-versions.sh --baseline F Use baseline file F.
#   check-daemon-subcommand-versions.sh --require-baseline
#                                                    Fail if the baseline is
#                                                    absent (CI uses this; a
#                                                    deleted ledger must not
#                                                    read as "all clear").
#   check-daemon-subcommand-versions.sh --self-test  Run the built-in fixtures.
#   check-daemon-subcommand-versions.sh --quiet      Only print failures.
#   check-daemon-subcommand-versions.sh [file …]     Restrict the scan to these files.
#   check-daemon-subcommand-versions.sh --help
#
# --update is for recording a pair a file genuinely stopped using, or promoting
# one to a declaration. Adding a new row to fit a new dependency is the ratchet
# slipping, which is the whole thing this prevents; write the marker instead.
#
# Exit codes: 0 = clean; 1 = a violation (or a missing required baseline);
# 2 = bad arguments.

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
BASELINE="$REPO_ROOT/scripts/daemon-subcommand-baseline.txt"

MODE="check"
QUIET=0
REQUIRE_BASELINE=0
EXPLICIT_FILES=()

usage() {
    sed -n '2,/^set -uo pipefail$/p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//; $d'
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --list)             MODE="list"; shift ;;
        --update)           MODE="update"; shift ;;
        --self-test)        MODE="self-test"; shift ;;
        --quiet)            QUIET=1; shift ;;
        --require-baseline) REQUIRE_BASELINE=1; shift ;;
        --baseline)         BASELINE="$2"; shift 2 ;;
        -h|--help)          usage; exit 0 ;;
        -*)                 echo "check-daemon-subcommand-versions: unknown option '$1'" >&2; exit 2 ;;
        *)                  EXPLICIT_FILES+=("$1"); shift ;;
    esac
done

# ---------------------------------------------------------------------------
# Detection
# ---------------------------------------------------------------------------

# The awk program is a here-doc'd constant rather than a sibling file so this
# gate stays a single self-contained script runnable from a bare checkout.
read -r -d '' DETECT_AWK <<'DETECT_AWK_END' || true
function trim(s) { sub(/^[[:space:]]+/, "", s); sub(/[[:space:]]+$/, "", s); return s }

# Blank quoted PROSE so a command name mentioned inside a message string is not
# read as a command. Three cases for a double-quoted run:
#
#   * it contains a `$(` -- it is a command substitution (or wraps one), so its
#     contents ARE code. The quote is emitted and scanning CONTINUES inside it,
#     which is what makes `out="$("$bin" sub)"` resolve to a real invocation
#     instead of collapsing into prose;
#   * its whole content is a parameter expansion ("$bin",
#     "${LOOM_DAEMON_BIN:-loom-daemon}") -- the shape a quoted binary path
#     actually takes. Kept verbatim;
#   * anything else -- prose. Blanked, so `warn "run loom-daemon foo"` is not an
#     invocation.
#
# Single-quoted runs are always blanked: nothing expands inside them, so a
# daemon reference there is a literal in a message, not a resolved binary.
function blank_prose(s,   out, i, c, j, inner, endq) {
  out = ""; i = 1
  while (i <= length(s)) {
    c = substr(s, i, 1)
    if (c == "\"") {
      endq = index(substr(s, i + 1), "\"")
      if (endq == 0) { out = out substr(s, i); break }
      inner = substr(s, i + 1, endq - 1)
      if (index(inner, "$(") > 0) { out = out "\""; i = i + 1; continue }
      if (inner ~ /^\$[A-Za-z_{(]/) out = out "\"" inner "\""
      else                        { out = out "\""; for (j = 1; j <= length(inner); j++) out = out " "; out = out "\"" }
      i = i + endq + 1
    } else if (c == "'") {
      endq = index(substr(s, i + 1), "'")
      if (endq == 0) { out = out substr(s, i); break }
      inner = substr(s, i + 1, endq - 1)
      out = out "'"; for (j = 1; j <= length(inner); j++) out = out " "; out = out "'"
      i = i + endq + 1
    } else {
      out = out c; i++
    }
  }
  return out
}

# Record the names of local variables this file assigns a resolved daemon
# binary to, so `"$bin" merge-pr-refs` is recognised without hardcoding every
# variable name any script might pick.
/^[[:space:]]*(local[[:space:]]+|export[[:space:]]+)?[A-Za-z_][A-Za-z0-9_]*=.*(loom_locate_daemon_bin|loom_daemon_self_bin_override|loom_resolve_self_daemon_bin|resolve_self_daemon_bin|LOOM_DAEMON_BIN|LOOM_DAEMON_SELF_BIN|command -v loom-daemon)/ {
  v = $0
  sub(/^[[:space:]]*/, "", v)
  sub(/^(local|export)[[:space:]]+/, "", v)
  if (match(v, /^[A-Za-z_][A-Za-z0-9_]*=/)) BINVAR[substr(v, 1, RLENGTH - 1)] = 1
}

{ LINES[NR] = $0 }

END {
  for (n = 1; n <= NR; n++) {
    line = LINES[n]
    if (line ~ /^[[:space:]]*#/) continue
    line = blank_prose(line)
    gsub(/\$\(/, "\001", line)
    gsub(/&&|\|\||[;|&()]|\001|`/, "\002", line)
    m = split(line, frag, "\002")
    for (i = 1; i <= m; i++) {
      f = trim(frag[i])
      if (f == "") continue
      while (match(f, /^(if|then|else|elif|do|while|until|!|exec|command|time|env|sudo|[A-Za-z_][A-Za-z0-9_]*=[^[:space:]]*)[[:space:]]+/)) {
        f = trim(substr(f, RLENGTH + 1))
      }
      if (match(f, /^loom_exec_script_helper[[:space:]]+/)) {
        rest = trim(substr(f, RLENGTH + 1))
        split(rest, w, /[[:space:]]/)
        if (w[1] ~ /^[a-z][a-z0-9-]*$/) print w[1] "\t" n
        continue
      }
      head = ""
      if (match(f, /^"?\$\{?(LOOM_DAEMON_SELF_BIN|LOOM_DAEMON_BIN)(:-[^}"]*)?\}?"?[[:space:]]+/)) head = "env"
      else if (match(f, /^loom-daemon[[:space:]]+/)) head = "literal"
      else if (match(f, /^"?\$\{?([A-Za-z_][A-Za-z0-9_]*)\}?"?[[:space:]]+/)) {
        name = f
        sub(/^"?\$\{?/, "", name)
        sub(/\}?"?[[:space:]].*$/, "", name)
        if (name in BINVAR) head = "var"
      }
      if (head == "") continue
      rest = trim(substr(f, RLENGTH + 1))
      split(rest, w, /[[:space:]]/)
      if (w[1] ~ /^[a-z][a-z0-9-]*$/) print w[1] "\t" n
    }
  }
}
DETECT_AWK_END

# detect_file <path> -- print "<subcommand>\t<first-line-number>" per distinct
# subcommand this file invokes, sorted.
detect_file() {
    awk "$DETECT_AWK" "$1" | sort -u -k1,1 | sort -t$'\t' -k1,1
}

# scan_targets -- the files in scope, one per line.
#
# Test suites are excluded: they pin their own binary through
# tests/lib/require-daemon-bin.sh, which already fails loudly with the
# subcommand named. `.loom/` is excluded because in this repo it is a mirror
# (a symlink for scripts/, resynced copies for hooks/) of `defaults/`, and a
# finding there is the same finding counted twice.
scan_targets() {
    if [[ ${#EXPLICIT_FILES[@]} -gt 0 ]]; then
        printf '%s\n' "${EXPLICIT_FILES[@]}"
        return 0
    fi
    git -C "$REPO_ROOT" ls-files '*.sh' \
        | grep -v '/tests/' \
        | grep -v '^tests/' \
        | grep -v '^\.loom/'
}

# ---------------------------------------------------------------------------
# Markers
# ---------------------------------------------------------------------------

# declared_markers <path> -- print "<subcommand>\t<requirement>" for every
# well-formed marker, and "<subcommand>\tMALFORMED" for every marker line this
# grammar rejects (so a typo fails loudly rather than silently not counting).
declared_markers() {
    awk '
      /^[[:space:]]*#[[:space:]]*requires-daemon:/ {
        body = $0
        sub(/^[[:space:]]*#[[:space:]]*requires-daemon:[[:space:]]*/, "", body)
        if (match(body, /^[a-z][a-z0-9-]*[[:space:]]+>=[[:space:]]+[0-9]+\.[0-9]+\.[0-9]+([[:space:]]|$)/)) {
          split(body, w, /[[:space:]]+/); print w[1] "\t" w[3]
        } else if (match(body, /^[a-z][a-z0-9-]*[[:space:]]+optional([[:space:]]|$)/)) {
          split(body, w, /[[:space:]]+/); print w[1] "\toptional"
        } else {
          split(body, w, /[[:space:]]+/)
          print (w[1] == "" ? "?" : w[1]) "\tMALFORMED"
        }
      }
    ' "$1" 2>/dev/null
}

# version_gt <a> <b> -- true when semver a > b.
version_gt() {
    [[ "$1" != "$2" ]] && [[ "$(printf '%s\n%s\n' "$1" "$2" | sort -V | tail -1)" == "$1" ]]
}

REPO_VERSION=""
[[ -f "$REPO_ROOT/VERSION" ]] && REPO_VERSION="$(tr -d '[:space:]' <"$REPO_ROOT/VERSION")"

# ---------------------------------------------------------------------------
# Modes
# ---------------------------------------------------------------------------

emit_pairs() {
    local file sub line
    while IFS= read -r file; do
        [[ -n "$file" && -f "$REPO_ROOT/$file" || -f "$file" ]] || continue
        local abs="$file"
        [[ -f "$abs" ]] || abs="$REPO_ROOT/$file"
        while IFS=$'\t' read -r sub line; do
            [[ -n "$sub" ]] && printf '%s\t%s\t%s\n' "$file" "$sub" "$line"
        done < <(detect_file "$abs")
    done < <(scan_targets)
}

if [[ "$MODE" == "list" ]]; then
    emit_pairs | while IFS=$'\t' read -r file sub line; do
        printf '%s:%s\t%s\n' "$file" "$line" "$sub"
    done
    exit 0
fi

if [[ "$MODE" == "update" ]]; then
    {
        echo "# daemon-subcommand-baseline.txt — generated by scripts/check-daemon-subcommand-versions.sh --update"
        echo "#"
        echo "# Every (shell file, loom-daemon subcommand) dependency that predates the"
        echo "# \`# requires-daemon:\` convention (#8285), grandfathered. A file may DROP a"
        echo "# pair freely; it may not gain a new one without declaring the minimum daemon"
        echo "# version in the file itself. Do NOT hand-add a row to fit a new dependency —"
        echo "# that is the ratchet slipping. Write the marker instead:"
        echo "#"
        echo "#   # requires-daemon: <subcommand> >= <version>   <why / which PR added it>"
        echo "#   # requires-daemon: <subcommand> optional       <how it degrades>"
        echo "#"
        echo "# A pair the file DECLARES is deliberately absent from this ledger — the"
        echo "# declaration supersedes the grandfather, and carrying both would let a"
        echo "# later deletion of the marker pass unnoticed."
        echo "#"
        echo "# Format: one <path><TAB><subcommand> per line."
        emit_pairs | while IFS=$'\t' read -r file sub _line; do
            local_abs="$file"
            [[ -f "$local_abs" ]] || local_abs="$REPO_ROOT/$file"
            local_decl="$(declared_markers "$local_abs" | cut -f1)"
            case $'\n'"$local_decl"$'\n' in
                *$'\n'"$sub"$'\n'*) continue ;;
            esac
            printf '%s\t%s\n' "$file" "$sub"
        done | sort -u
    } >"$BASELINE"
    echo "check-daemon-subcommand-versions: wrote $BASELINE"
    exit 0
fi

if [[ "$MODE" == "self-test" ]]; then
    tmp="$(mktemp -d)"
    trap 'rm -rf "$tmp"' EXIT
    failures=0
    _expect() { # <name> <expected> <actual>
        if [[ "$2" == "$3" ]]; then
            echo "  PASS: $1"
        else
            echo "  FAIL: $1"
            echo "    expected: $2"
            echo "    actual:   $3"
            failures=$((failures + 1))
        fi
    }

    # THE FIXTURES ARE WRITTEN THROUGH A SUBSTITUTION, not verbatim. A fixture
    # is by construction a script shaped exactly like what this gate detects, so
    # spelling one out literally in a heredoc would make THIS file scan as a
    # script with undeclared daemon dependencies and stale markers — the gate
    # would fail itself, and the only ways out would be to stop scanning its own
    # source (blinding it) or to baseline its fixtures (a permanent lie in the
    # ledger). Substituting the two tokens that carry the meaning — the marker
    # keyword and the subcommand names — costs one sed and keeps this file
    # honestly in scope. Nothing about what the fixtures EXERCISE changes: the
    # files on disk are byte-identical to the literal versions.
    _fixture() {
        sed -e 's/@RD@/requires-daemon/g' \
            -e 's/@SUB@/widget-refs/g' \
            -e 's/@SUB2@/skip-widgets/g' \
            -e 's/@SUB3@/gone-refs/g' >"$1"
    }

    _fixture "$tmp/declared.sh" <<'FIXTURE'
#!/usr/bin/env bash
# @RD@: @SUB@ >= 0.0.1   added in #1
bin="$(loom_locate_daemon_bin "$root")"
out="$("$bin" @SUB@ closing-refs)"
FIXTURE

    _fixture "$tmp/undeclared.sh" <<'FIXTURE'
#!/usr/bin/env bash
BIN="$(loom_locate_daemon_bin "$root")"
exec "$BIN" @SUB@ "$@"
FIXTURE

    _fixture "$tmp/prose-only.sh" <<'FIXTURE'
#!/usr/bin/env bash
echo "run loom-daemon @SUB@ by hand (loom-daemon accounts add codex <name>)"
warn "      loom-daemon stashes retire --issue <N>"
# loom-daemon @SUB@ is also mentioned in this comment
FIXTURE

    _fixture "$tmp/stub.sh" <<'FIXTURE'
#!/usr/bin/env bash
# @RD@: @SUB2@ optional   degrades to the fixed list
loom_exec_script_helper @SUB2@ "$@"
FIXTURE

    _fixture "$tmp/malformed.sh" <<'FIXTURE'
#!/usr/bin/env bash
# @RD@: @SUB@ 0.0.1
bin="${LOOM_DAEMON_BIN:-loom-daemon}"
"$bin" @SUB@
FIXTURE

    _fixture "$tmp/stale.sh" <<'FIXTURE'
#!/usr/bin/env bash
# @RD@: @SUB3@ >= 0.0.1
bin="${LOOM_DAEMON_BIN:-loom-daemon}"
"$bin" @SUB@
FIXTURE

    _fixture "$tmp/future.sh" <<'FIXTURE'
#!/usr/bin/env bash
# @RD@: @SUB@ >= 999.0.0
bin="${LOOM_DAEMON_BIN:-loom-daemon}"
"$bin" @SUB@
FIXTURE

    # The substitution above is only sound if it actually produces the literal
    # fixture. Assert that rather than trusting it — a typo'd token would
    # silently turn several assertions below into vacuous passes.
    _expect "fixture substitution produces a real marker line" \
        "# requires-daemon: widget-refs >= 0.0.1   added in #1" \
        "$(sed -n '2p' "$tmp/declared.sh")"

    _expect "declared file detects its subcommand" \
        "widget-refs" "$(detect_file "$tmp/declared.sh" | cut -f1 | tr '\n' ' ' | sed 's/ $//')"
    _expect "stub shape is detected" \
        "skip-widgets" "$(detect_file "$tmp/stub.sh" | cut -f1 | tr '\n' ' ' | sed 's/ $//')"
    _expect "quoted prose is NOT detected" \
        "" "$(detect_file "$tmp/prose-only.sh" | cut -f1 | tr '\n' ' ' | sed 's/ $//')"
    _expect "well-formed >= marker parses" \
        "widget-refs	0.0.1" "$(declared_markers "$tmp/declared.sh")"
    _expect "optional marker parses" \
        "skip-widgets	optional" "$(declared_markers "$tmp/stub.sh")"
    _expect "malformed marker is reported" \
        "widget-refs	MALFORMED" "$(declared_markers "$tmp/malformed.sh")"

    empty_baseline="$tmp/empty-baseline.txt"
    : >"$empty_baseline"

    out="$(bash "${BASH_SOURCE[0]}" --baseline "$empty_baseline" "$tmp/declared.sh" 2>&1)"; rc=$?
    _expect "declared + empty baseline passes" "0" "$rc"

    out="$(bash "${BASH_SOURCE[0]}" --baseline "$empty_baseline" "$tmp/undeclared.sh" 2>&1)"; rc=$?
    _expect "undeclared + empty baseline fails" "1" "$rc"
    case "$out" in *"widget-refs"*) _expect "failure names the subcommand" "yes" "yes" ;;
                   *)               _expect "failure names the subcommand" "yes" "no: $out" ;; esac

    printf '%s\t%s\n' "$tmp/undeclared.sh" "widget-refs" >"$tmp/full-baseline.txt"
    out="$(bash "${BASH_SOURCE[0]}" --baseline "$tmp/full-baseline.txt" "$tmp/undeclared.sh" 2>&1)"; rc=$?
    _expect "undeclared but baselined passes" "0" "$rc"

    out="$(bash "${BASH_SOURCE[0]}" --baseline "$empty_baseline" "$tmp/malformed.sh" 2>&1)"; rc=$?
    _expect "malformed marker fails" "1" "$rc"

    out="$(bash "${BASH_SOURCE[0]}" --baseline "$empty_baseline" "$tmp/stale.sh" 2>&1)"; rc=$?
    _expect "stale marker (subcommand not invoked) fails" "1" "$rc"

    out="$(bash "${BASH_SOURCE[0]}" --baseline "$empty_baseline" "$tmp/future.sh" 2>&1)"; rc=$?
    _expect "floor above this repo's VERSION fails" "1" "$rc"

    out="$(bash "${BASH_SOURCE[0]}" --baseline "$tmp/does-not-exist.txt" --require-baseline "$tmp/declared.sh" 2>&1)"; rc=$?
    _expect "--require-baseline fails on a missing ledger" "1" "$rc"

    if [[ "$failures" -eq 0 ]]; then
        echo "check-daemon-subcommand-versions --self-test: OK"
        exit 0
    fi
    echo "check-daemon-subcommand-versions --self-test: $failures failure(s)" >&2
    exit 1
fi

# ---------------------------------------------------------------------------
# check
# ---------------------------------------------------------------------------

if [[ ! -f "$BASELINE" ]]; then
    if [[ "$REQUIRE_BASELINE" -eq 1 ]]; then
        echo "check-daemon-subcommand-versions: FAIL — required baseline '$BASELINE' is missing." >&2
        echo "  A deleted ledger must not read as 'all clear'. Restore it, or regenerate with --update." >&2
        exit 1
    fi
    [[ "$QUIET" -eq 1 ]] || echo "check-daemon-subcommand-versions: no baseline at $BASELINE — treating every pair as new."
fi

VIOLATIONS=0

# Membership is held in NEWLINE-DELIMITED strings tested with `case`, not in
# associative arrays: `declare -A` is bash 4+, and this gate has to run on a
# bare macOS host's /bin/bash 3.2 like every other structural check here
# (#7762). `case $'\n'"$list"$'\n' in *$'\n'"$key"$'\n'*)` is the repo's
# documented exact-membership idiom and needs no subprocess.
BASELINED=""
if [[ -f "$BASELINE" ]]; then
    while IFS=$'\t' read -r bfile bsub; do
        [[ -z "${bfile// }" || "${bfile:0:1}" == "#" ]] && continue
        [[ -n "$bsub" ]] && BASELINED="$BASELINED$bfile"$'\t'"$bsub"$'\n'
    done <"$BASELINE"
fi

# decl_lookup <decl-list> <subcommand> -- echo the declared requirement for
# <subcommand>, or nothing. The list is "<sub>\t<req>" lines.
decl_lookup() {
    local list="$1" want="$2" s r
    while IFS=$'\t' read -r s r; do
        [[ "$s" == "$want" ]] && { printf '%s' "$r"; return 0; }
    done <<<"$list"
    return 1
}

CHECKED_FILES=0
CHECKED_PAIRS=0

while IFS= read -r file; do
    [[ -n "$file" ]] || continue
    abs="$file"
    [[ -f "$abs" ]] || abs="$REPO_ROOT/$file"
    [[ -f "$abs" ]] || continue

    DECL=""
    DECL_SEEN=""
    while IFS=$'\t' read -r dsub dreq; do
        [[ -n "$dsub" ]] || continue
        if [[ "$dreq" == "MALFORMED" ]]; then
            echo "MALFORMED MARKER  $file" >&2
            echo "    A '# requires-daemon:' line in this file does not match the grammar." >&2
            echo "    Expected one of:" >&2
            echo "      # requires-daemon: <subcommand> >= <major.minor.patch>   <note>" >&2
            echo "      # requires-daemon: <subcommand> optional                 <note>" >&2
            VIOLATIONS=$((VIOLATIONS + 1))
            continue
        fi
        DECL="$DECL$dsub"$'\t'"$dreq"$'\n'
    done < <(declared_markers "$abs")

    CHECKED_FILES=$((CHECKED_FILES + 1))

    while IFS=$'\t' read -r sub line; do
        [[ -n "$sub" ]] || continue
        CHECKED_PAIRS=$((CHECKED_PAIRS + 1))
        req="$(decl_lookup "$DECL" "$sub" || true)"
        if [[ -n "$req" ]]; then
            DECL_SEEN="$DECL_SEEN$sub"$'\n'
            if [[ "$req" != "optional" && -n "$REPO_VERSION" ]] && version_gt "$req" "$REPO_VERSION"; then
                echo "FLOOR ABOVE VERSION  $file:$line  $sub >= $req" >&2
                echo "    This repo's VERSION is $REPO_VERSION, so no released loom-daemon can satisfy" >&2
                echo "    that floor and every host would refuse forever. Declare the version the" >&2
                echo "    subcommand actually landed in." >&2
                VIOLATIONS=$((VIOLATIONS + 1))
            fi
            continue
        fi
        case $'\n'"$BASELINED" in
            *$'\n'"$file"$'\t'"$sub"$'\n'*) continue ;;
        esac
        echo "UNDECLARED DAEMON DEPENDENCY  $file:$line  loom-daemon $sub" >&2
        echo "    This script invokes '$sub' but does not declare the minimum loom-daemon" >&2
        echo "    version it needs, so a host whose binary predates that subcommand fails with" >&2
        echo "    no way to learn what to roll to (#8285). Add ONE line to $file:" >&2
        echo "" >&2
        echo "      # requires-daemon: $sub >= <version>   <which PR added it>" >&2
        echo "" >&2
        echo "    …or, if the script probes for it and degrades gracefully when absent:" >&2
        echo "" >&2
        echo "      # requires-daemon: $sub optional        <how it degrades>" >&2
        echo "" >&2
        VIOLATIONS=$((VIOLATIONS + 1))
    done < <(detect_file "$abs")

    while IFS=$'\t' read -r dsub _dreq; do
        [[ -n "$dsub" ]] || continue
        case $'\n'"$DECL_SEEN" in
            *$'\n'"$dsub"$'\n'*) continue ;;
        esac
        echo "STALE MARKER  $file  requires-daemon: $dsub" >&2
        echo "    Nothing in this file invokes 'loom-daemon $dsub' any more. A stale floor is" >&2
        echo "    worse than none: it tells a reader the script needs a version it does not." >&2
        echo "    Delete the marker." >&2
        VIOLATIONS=$((VIOLATIONS + 1))
    done <<<"$DECL"
done < <(scan_targets)

if [[ "$VIOLATIONS" -gt 0 ]]; then
    echo "" >&2
    echo "check-daemon-subcommand-versions: FAIL — $VIOLATIONS finding(s)." >&2
    echo "  Convention: scripts/check-daemon-subcommand-versions.sh --help" >&2
    exit 1
fi

[[ "$QUIET" -eq 1 ]] || echo "check-daemon-subcommand-versions: OK — $CHECKED_PAIRS daemon-subcommand dependency/ies across $CHECKED_FILES file(s), all declared or baselined."
exit 0
