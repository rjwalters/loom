#!/usr/bin/env bash
# check-guard-scan-contracts.sh — enforce the consumer-tier contract on every
# derived scan string in guard-destructive-generic.sh (issue #7755).
#
# ---------------------------------------------------------------------------
# WHY
# ---------------------------------------------------------------------------
# The guard answers one question over and over: "is this text executable code,
# or inert quoted data?" It answers it by building a chain of LOSSY derived
# copies of $COMMAND (regex/awk masking passes) and matching patterns against
# those copies:
#
#   COMMAND
#    |- COMMAND_NO_LITERAL_TEXT      -> ALWAYS_BLOCK (catastrophic) scan
#    |- COMMAND_HEREDOC_MASKED       -> COMMAND_GH_API_RAWFIELD_SCAN
#    `- COMMAND_NO_COMMENT           -> COMMAND_ASK_SCAN
#                                        |- COMMAND_CLOUD_ASK_SCAN
#                                        `- COMMAND_STASH_SCAN
#
# Every derivation carries a safety claim SCOPED TO A CONSUMER TIER. Historically
# that claim was prose only — e.g. COMMAND_NO_COMMENT's own header: "explicitly
# reserved for the ASK/DDL tier only ... the catastrophic tier is kept strictly
# stricter so a missed BLOCK can never happen from a shared masking pass."
# Nothing enforced the reservation, and it WAS violated: COMMAND_ASK_SCAN's
# masking was justified as "worst case is a missed ASK", then a later change
# routed it into extract_write_targets()'s hard-DENY write-confinement check as
# well, silently converting an accepted-risk tradeoff (a missed ask on quoted
# data) into a security bypass (a silent ALLOW where #4178/#4921 require a
# DENY). Root-caused and fixed in #6252 / ADR-0016
# (docs/adr/0016-write-target-confinement-approach.md).
#
# The failure direction flips with the CONSUMER, and nobody changed the masking
# — they added a reader. That is the class this script makes unmissable.
#
# This script does NOT change the guard's decision logic (#7755 is explicitly a
# no-behaviour-change forcing function, not a guard rewrite) and it does NOT
# adjudicate whether a declared tier is factually correct — that stays a
# human-reviewed property, argued in each derivation's own header comment and
# regression-tested by tests/hooks/test-guard-destructive*.sh's "narrows, never
# widens" coverage. What it enforces is that the DECLARED graph is internally
# consistent and that nobody silently widens a reader past what the string it
# reads was declared safe for.
#
# ---------------------------------------------------------------------------
# THE MARKERS
# ---------------------------------------------------------------------------
# 1. DERIVATION (one per derived scan variable, on its declaring assignment or
#    in the header comment block immediately above it):
#
#      # scan-contract: <VAR>=<TIER> from=<PARENT>
#
#    <TIER> is the STRICTEST consumer tier this copy may legitimately feed:
#      ask-only          a missed match here is a missed ask() — an accepted
#                        risk. Must never feed a deny().
#      deny-safe         the masking provably only removes text that cannot
#                        execute, so a missed match cannot manufacture a silent
#                        ALLOW. May feed deny() and ask().
#      catastrophic-safe additionally safe for the ungated ALWAYS_BLOCK denial
#                        floor (the `catastrophic:*` reason codes), which the
#                        guard deliberately keeps stricter than every other
#                        tier. May feed anything.
#
#    <PARENT> is the variable this copy is derived FROM (`COMMAND` for a copy
#    branched straight off the raw, unmasked command — raw $COMMAND is
#    implicitly catastrophic-safe, it is the ground truth).
#
# 2. READ SITE (one per deny()/ask() call, on the same line):
#
#      # scan-reads: <VAR>[,<VAR>...]
#      # scan-reads: none            <- reads only the raw, unmasked $COMMAND
#
#    Only the VARIABLE LIST is hand-written. The site's own emitted tier is
#    derived MECHANICALLY from the line itself, never from an annotation an
#    author could get wrong:
#      ask()                                  -> tier `ask`
#      deny() with a "catastrophic:…" reason   -> tier `catastrophic`
#      deny() otherwise                        -> tier `deny`
#
# 3. WAIVER (optional, on the same scan-reads comment):
#
#      # scan-reads: <VAR>  scan-waiver: <reason>
#
#    A waived violation still prints a WARNING (so it stays visible in CI
#    output and in grep) but does not fail the build. #7755's own acceptance
#    criteria require that a deny-emitting read of an ask-only string be either
#    fixed OR "explicitly waived with a recorded reason" — never silently
#    skipped.
#
# ---------------------------------------------------------------------------
# WHAT IS CHECKED
# ---------------------------------------------------------------------------
#   1. Every `scan-contract:` names a known tier and a `from=` parent that is
#      itself declared (or is the raw `COMMAND`).
#   2. NO LAUNDERING: a derived copy may never be declared SAFER than the copy
#      it is derived from. Masking is monotonically lossy, so branching off an
#      ask-only string and declaring the branch `deny-safe` is exactly how a
#      future author would (accidentally or otherwise) defeat check 5 below.
#   3. Every `^COMMAND_*=` assignment in the file has a contract declaration
#      somewhere — a NEW derived copy cannot be added without classifying it.
#   4. Every deny()/ask() call carries a `scan-reads:` annotation — a NEW
#      decision site cannot be added without recording what it reads.
#   5. THE ACTUAL INVARIANT: a read site's emitted tier must never exceed the
#      declared tier of any string it reads. An `ask-only` string reaching a
#      deny() is the #6252 shape; a `deny-safe` string reaching the ALWAYS_BLOCK
#      catastrophic floor is the reservation COMMAND_NO_COMMENT's header has
#      always claimed in prose.
#   6. No stale annotations: a `scan-reads:` comment on a line that calls
#      neither deny() nor ask(), a reference to an undeclared variable, or the
#      same variable declared at two different tiers, are all errors.
#
# Deliberately grep-level, not a parser. guard-destructive-generic.sh is ONE
# file with a small, enumerable set of derived copies and decision sites, and
# ADR-0016 already rejected introducing a shell AST here; #7755 does not reopen
# that.
#
# ---------------------------------------------------------------------------
# SECOND CHECK: THE PreToolUse MATCHER COVERAGE CONTRACT (#9108)
# ---------------------------------------------------------------------------
# The tier contract above answers "does the guard read the right copy of the
# command?". It says nothing about the prior question: "is the guard on the tool
# call's path at all?" Until #9108 the answer for one whole tool class was no.
# `.claude/settings.json` wired exactly three `PreToolUse` matchers — `Bash`
# twice and `Edit|Write` — so every `mcp__loom__*` tool call bypassed the guard
# surface entirely, while `get_agent_metrics` turned its raw MCP arguments into
# a shell command line (fixed server-side in #9107). The failure was invisible
# for the same reason
# #7755's was: nothing asserted the wiring, and **a matcher that matches nothing
# does not error — it simply never fires**.
#
# So [`check_mcp_guard_wiring`] asserts two things no test could otherwise see:
#
#   1. the `mcp__loom__.*` matcher exists in this repo's own
#      `.claude/settings.json` AND in `scripts/install/provision-hooks.sh`'s
#      `_PHOOK_*` wiring set (the repo's own file covers THIS checkout; the
#      installer's arrays are what every fresh consumer gets — one without the
#      other is a hole);
#   2. the entry carries the same FAIL-CLOSED floor the `Bash` / `Edit|Write`
#      entries carry: it routes through `hook-wiring.sh`, and its inline
#      fallback denies rather than allows when the hook file is absent from a
#      `.loom/hooks`-bearing workspace, with the `LOOM_GUARD_WIRING_FAILOPEN`
#      escape hatch as the only way past it.
#
# Deliberately a text-level assertion over the settings file rather than a jq
# query: this checker already runs without jq, and the fail-closed floor it is
# asserting is a property of the emitted COMMAND STRING, which is what a
# reviewer reads and what breaks when someone "simplifies" the wrapper.
#
# ---------------------------------------------------------------------------
# Usage:
#   check-guard-scan-contracts.sh [PATH]
#     PATH  guard script to check (default:
#           <repo-root>/defaults/hooks/guard-destructive-generic.sh). With an
#           explicit PATH this stays a pure per-file check; the #9108 wiring
#           contract only runs in the default (whole-repo) mode.
#
#   check-guard-scan-contracts.sh --wiring
#     Run only the #9108 PreToolUse matcher-coverage contract.
#
#   check-guard-scan-contracts.sh --self-test
#     Synthetic-fixture regression test of the checker's own discriminating
#     power: a compliant fixture, the #6252 ask-only-into-deny shape (unwaived
#     and waived), a deny-safe string reaching the catastrophic floor, a
#     laundering branch, a missing annotation, a stale annotation, an
#     undeclared reference, an inconsistent re-declaration, an unclassified
#     new derivation, and — for the #9108 wiring contract — a compliant wiring
#     fixture plus one missing the MCP matcher, one missing the fail-closed
#     floor, one whose entry bypasses hook-wiring.sh, and one whose installer
#     arrays never learned the matcher. Touches only $TMPDIR, never the repo
#     tree.
#
# Portability: this must run in the same environment as the file it checks,
# which on macOS is stock /bin/bash 3.2.57 (#7751, #7728) — so: no associative
# arrays, no `mapfile`, no `${var,,}`, no `declare -A`. Variable -> (tier,
# parent) lookups use a newline-delimited "VAR TIER PARENT" text blob searched
# with grep, not a bash 4+ associative array.
#
# Exit codes: 0 = clean (or --self-test passed); 1 = violation / missing
# annotation / inconsistent declaration (or --self-test failed). Details on
# stderr.

set -uo pipefail

# Tier lattice, weakest consumer first. A contract's `<X>-safe` / `ask-only`
# spelling maps onto the same ordinal as the consumer tier it permits.
tier_rank() { # <tier-or-contract> -> 1|2|3, or empty for unknown
    case "$1" in
        ask | ask-only) printf '1' ;;
        deny | deny-safe) printf '2' ;;
        catastrophic | catastrophic-safe) printf '3' ;;
        *) printf '' ;;
    esac
}

VALID_CONTRACTS="ask-only deny-safe catastrophic-safe"

# The implicit root of every derivation chain: the raw, unmasked command string
# as the harness handed it to the guard. Nothing is masked out of it, so it is
# safe for every tier by construction.
ROOT_VAR="COMMAND"

# --- Extraction helpers ------------------------------------------------------
# Both anchor on a literal marker substring rather than "split on the first #":
# several deny()/ask() MESSAGE strings contain a literal `#` (issue references
# like "(#4178)"), so a naive split truncates mid-message on those lines.
# Neither marker token ever appears in the guard's own message text.

# Prints "<lineno> <var> <tier> <parent>" per `scan-contract:` declaration.
# `from=` is optional in the grammar so a malformed declaration is reported as
# a missing parent rather than silently skipped by the regex.
extract_contract_lines() { # <file>
    grep -noE 'scan-contract:[[:space:]]*[A-Za-z_][A-Za-z0-9_]*=[a-z-]+([[:space:]]+from=[A-Za-z_][A-Za-z0-9_]*)?' "$1" |
        sed -E 's/^([0-9]+):scan-contract:[[:space:]]*/\1 /; s/=/ /; s/[[:space:]]+from=/ /'
}

# Every deny()/ask() CALL line (excluding whole-line comments), as "<lineno>:<text>".
extract_decision_lines() { # <file>
    grep -nE '(^|[;&|`(]|[[:space:]])(deny|ask)[[:space:]]+"' "$1" |
        grep -vE '^[0-9]+:[[:space:]]*#'
}

# Mechanically derive a decision line's emitted tier from the line itself.
decision_tier() { # <line-text> -> ask|deny|catastrophic|BOTH|NEITHER
    local rest="$1" t="NEITHER"
    if printf '%s' "$rest" | grep -qE '(^|[;&|`(]|[[:space:]])deny[[:space:]]+"'; then
        # The ungated ALWAYS_BLOCK denial floor tags itself with a
        # "catastrophic:<pattern>" reason code — the one tier the guard's own
        # prose keeps strictly stricter than every other.
        if printf '%s' "$rest" | grep -q '"catastrophic:'; then
            t="catastrophic"
        else
            t="deny"
        fi
    fi
    if printf '%s' "$rest" | grep -qE '(^|[;&|`(]|[[:space:]])ask[[:space:]]+"'; then
        if [[ "$t" != "NEITHER" ]]; then t="BOTH"; else t="ask"; fi
    fi
    printf '%s' "$t"
}

# Strip leading/trailing spaces and tabs (bash 3.2: no extglob assumptions).
trim() { # <string>
    local s="$1"
    while [[ "$s" == " "* || "$s" == $'\t'* ]]; do s="${s# }"; s="${s#$'\t'}"; done
    while [[ "$s" == *" " || "$s" == *$'\t' ]]; do s="${s% }"; s="${s%$'\t'}"; done
    printf '%s' "$s"
}

# Look up a declared field: field 2 = tier, field 3 = parent.
contract_field() { # <contracts-blob> <var> <field-index>
    printf '%s\n' "$1" | grep -E "^$2 " | head -1 | awk -v f="$3" '{print $f}'
}

# --- Core check --------------------------------------------------------------
# Prints violations/errors to stderr. Returns 0 clean, 1 otherwise.
check_guard_scan_contracts() { # <file>
    local file="$1"
    local fail=0
    local contracts="" lineno var tier parent existing

    if [[ ! -f "$file" ]]; then
        echo "check-guard-scan-contracts: no such file: $file" >&2
        return 1
    fi

    # --- Pass 1: collect and validate the contract declarations --------------
    while read -r lineno var tier parent; do
        [[ -z "$lineno" ]] && continue
        if [[ -z "$(tier_rank "$tier")" || " $VALID_CONTRACTS " != *" $tier "* ]]; then
            echo "ERROR: $file:$lineno: scan-contract for '$var' declares unknown tier '$tier' (expected one of: $VALID_CONTRACTS)" >&2
            fail=1
            continue
        fi
        if [[ -z "$parent" ]]; then
            echo "ERROR: $file:$lineno: scan-contract for '$var' has no 'from=<PARENT>' clause — every derived copy must name the copy it is derived from (use 'from=$ROOT_VAR' for a branch off the raw command)" >&2
            fail=1
            continue
        fi
        existing="$(contract_field "$contracts" "$var" 2)"
        if [[ -n "$existing" && "$existing" != "$tier" ]]; then
            echo "ERROR: $file:$lineno: '$var' is declared '$tier' here but '$existing' elsewhere — a derived scan variable must have exactly one, consistent scan-contract" >&2
            fail=1
            continue
        fi
        if [[ -z "$existing" ]]; then
            contracts="${contracts}
${var} ${tier} ${parent}"
        fi
    done < <(extract_contract_lines "$file")

    if [[ -z "$(trim "$contracts")" ]]; then
        echo "ERROR: $file: no '# scan-contract:' declarations found at all — expected at least the derived COMMAND_* scan-string family" >&2
        return 1
    fi

    # --- Pass 2: no laundering — a child may never outrank its parent --------
    # Masking is monotonically lossy: whatever the parent already masked out is
    # still masked out downstream. So branching off an ask-only copy and
    # declaring the branch deny-safe would launder the contract and defeat
    # Pass 5 — the single most likely way to "fix" a future failure wrongly.
    while read -r var tier parent; do
        [[ -z "$var" ]] && continue
        local parent_tier prank crank
        if [[ "$parent" == "$ROOT_VAR" ]]; then
            parent_tier="catastrophic-safe" # raw $COMMAND masks nothing
        else
            parent_tier="$(contract_field "$contracts" "$parent" 2)"
            if [[ -z "$parent_tier" ]]; then
                echo "ERROR: $file: '$var' declares 'from=$parent', but '$parent' has no scan-contract declaration of its own (typo, or the parent needs classifying too)" >&2
                fail=1
                continue
            fi
        fi
        prank="$(tier_rank "$parent_tier")"
        crank="$(tier_rank "$tier")"
        if [[ "$crank" -gt "$prank" ]]; then
            echo "LAUNDERING: $file: '$var' is declared '$tier' but is derived from '$parent', which is only '$parent_tier' — a masking pass can only ever REMOVE text, so a derived copy can never be safe for a stricter consumer than the copy it was built from. Fix the derivation or lower '$var' to '$parent_tier'." >&2
            fail=1
        fi
    done < <(printf '%s\n' "$contracts" | grep -E '^[A-Za-z_]')

    # --- Pass 3: every derived assignment must be classified ----------------
    # Catches a NEW derived scan copy added with no contract anywhere in the
    # file. The declaration need not sit on this exact line — a re-narrowing
    # self-assignment of an already-declared variable is fine.
    while IFS= read -r var; do
        [[ -z "$var" ]] && continue
        if ! printf '%s\n' "$contracts" | grep -qE "^${var} "; then
            echo "ERROR: $file: derived variable '$var' is assigned but never classified — add '# scan-contract: ${var}=<ask-only|deny-safe|catastrophic-safe> from=<PARENT>'" >&2
            fail=1
        fi
    done < <(grep -oE '^[[:space:]]*COMMAND_[A-Z0-9_]+=' "$file" | sed -E 's/^[[:space:]]*//; s/=$//' | sort -u)

    # --- Pass 4: every decision site must record what it reads --------------
    local rest
    while IFS= read -r hit; do
        [[ -z "$hit" ]] && continue
        lineno="${hit%%:*}"
        rest="${hit#*:}"
        if ! printf '%s' "$rest" | grep -q 'scan-reads:'; then
            echo "ERROR: $file:$lineno: deny()/ask() call has no '# scan-reads:' annotation — add '# scan-reads: <VAR>[,<VAR>...]', or '# scan-reads: none' if it matches only the raw, unmasked \$COMMAND" >&2
            fail=1
        fi
    done < <(extract_decision_lines "$file")

    # --- Pass 5: the invariant — reader tier must not exceed string tier ----
    local site_tier tag varlist waiver srank vrank var_tier
    while IFS= read -r hit; do
        [[ -z "$hit" ]] && continue
        lineno="${hit%%:*}"
        rest="${hit#*:}"
        site_tier="$(decision_tier "$rest")"

        if [[ "$site_tier" == "NEITHER" ]]; then
            echo "ERROR: $file:$lineno: line carries a 'scan-reads:' annotation but calls neither deny() nor ask() — the annotation is stale or misplaced" >&2
            fail=1
            continue
        fi
        if [[ "$site_tier" == "BOTH" ]]; then
            echo "ERROR: $file:$lineno: line appears to call both deny() and ask() — split the annotation across the two real call lines so each records its own tier" >&2
            fail=1
            continue
        fi

        tag="$(printf '%s' "$rest" | grep -oE 'scan-reads:.*$')"
        tag="$(trim "${tag#scan-reads:}")"
        waiver=""
        if [[ "$tag" == *"scan-waiver:"* ]]; then
            varlist="$(trim "${tag%%scan-waiver:*}")"
            waiver="$(trim "${tag#*scan-waiver:}")"
        else
            varlist="$tag"
        fi
        [[ "$varlist" == "none" || -z "$varlist" ]] && continue

        srank="$(tier_rank "$site_tier")"
        local IFS_SAVE="$IFS"
        IFS=','
        for var in $varlist; do
            IFS="$IFS_SAVE"
            var="$(trim "$var")"
            [[ -z "$var" ]] && continue
            var_tier="$(contract_field "$contracts" "$var" 2)"
            if [[ -z "$var_tier" ]]; then
                echo "ERROR: $file:$lineno: scan-reads references '$var', which has no scan-contract declaration — add one, or fix the typo" >&2
                fail=1
                IFS=','
                continue
            fi
            vrank="$(tier_rank "$var_tier")"
            if [[ "$srank" -gt "$vrank" ]]; then
                if [[ -n "$waiver" ]]; then
                    echo "WARNING: $file:$lineno: WAIVED — '$var' is declared '$var_tier' but is read by a '$site_tier'-tier decision site (reason: $waiver)" >&2
                else
                    echo "VIOLATION: $file:$lineno: '$var' is declared '$var_tier' but is read by a '$site_tier'-tier decision site." >&2
                    echo "  This is the #6252 shape: a lossy copy whose lossiness was justified for a WEAKER consumer now decides a STRICTER one, so a missed match silently becomes a permissive outcome where the guard is required to refuse." >&2
                    echo "  Fix it one of three ways: (a) point this site at a copy declared for its own tier, (b) prove and re-declare '$var' at the stricter tier (as #6252/ADR-0016 did for COMMAND_NO_COMMENT by making mask_comment() quote-aware) — its 'from=' parent must be re-declared too, or (c) record 'scan-waiver: <reason>' on this line's scan-reads comment if the exposure is deliberate and accepted." >&2
                    fail=1
                fi
            fi
            IFS=','
        done
        IFS="$IFS_SAVE"
    done < <(grep -n 'scan-reads:' "$file" | grep -vE '^[0-9]+:[[:space:]]*#')

    return "$fail"
}

# --- The #9108 PreToolUse matcher-coverage contract ---------------------------
#
# The wiring strings this asserts. Each is a substring of the emitted hook
# command, so "someone simplified the wrapper" fails loudly instead of quietly
# restoring the silent-allow hole (#7761) for the MCP surface.
MCP_MATCHER='mcp__loom__.*'
MCP_GUARD='guard-mcp-tools.sh'
# Route-through: the launcher whose rung ladder owns the absent/non-executable
# cases (defaults/hooks/hook-wiring.sh), plus the exact arguments it is handed.
# Two substrings rather than one, because the launcher path and its argument
# list are separated by the wrapper's own `$L` indirection.
MCP_WIRING_LAUNCHER='hook-wiring.sh'
MCP_WIRING_ARGS='PreToolUse guard-mcp-tools.sh'
# The fail-closed floor, as three independent properties of the SAME command:
# the workspace gate, the deny document, and the single sanctioned escape hatch.
MCP_FLOOR_WORKSPACE_GATE='.loom/hooks'
MCP_FLOOR_DENY='permissionDecision'
MCP_FLOOR_ESCAPE='LOOM_GUARD_WIRING_FAILOPEN'

# Fork-free literal-substring test. Deliberately `case`, not
# `printf … | grep -qF`: under `set -o pipefail` an early-exit consumer like
# `grep -q` can close the pipe mid-write and report the whole pipeline as
# failed (scripts/check-pipefail-early-exit.sh, #7060/#7771). `case` also needs
# no metacharacter escaping, which matters because every needle here contains
# `.` and one contains `*`.
_mcp_contains() { # <haystack> <needle>
    case "$1" in
        *"$2"*) return 0 ;;
        *) return 1 ;;
    esac
}

# Prints violations to stderr. Returns 0 clean, 1 otherwise.
check_mcp_guard_wiring() { # <repo-root>
    local root="$1"
    local settings="$root/.claude/settings.json"
    local provision="$root/scripts/install/provision-hooks.sh"
    local guard="$root/defaults/hooks/$MCP_GUARD"
    local fail=0 entry=""

    if [[ ! -f "$settings" ]]; then
        echo "ERROR: $settings: no settings file — cannot verify the $MCP_MATCHER PreToolUse matcher exists (#9108)" >&2
        return 1
    fi

    # (1) the matcher itself. Extracted as ONE physical line: every hook entry
    # in this file is a single-line JSON string, so the matcher line and the
    # command line are adjacent and the command can be found by name without a
    # JSON parser.
    if ! grep -qF -- "\"$MCP_MATCHER\"" "$settings"; then
        echo "MISSING MCP MATCHER: $settings has no PreToolUse entry with matcher \"$MCP_MATCHER\" (#9108)." >&2
        echo "    Every mcp__loom__* tool call then runs with NO guard hook on its path, which is" >&2
        echo "    the exact gap #9108 closed: mcp-loom is registered at user scope and callable" >&2
        echo "    from every agent Loom spawns, and get_agent_metrics built a shell command line" >&2
        echo "    from its raw arguments until #9107. A matcher that is absent does not error — it" >&2
        echo "    just never fires, which is why this is asserted rather than tested." >&2
        fail=1
    fi

    # (2) an entry that actually runs THIS guard, through hook-wiring.sh.
    # Anchored on `"command"` rather than on `PreToolUse`: an entry that
    # BYPASSES hook-wiring.sh has no `PreToolUse` argument in it at all, and
    # filtering on that would misreport the bypass as "no command wired".
    entry="$(grep -F -- "$MCP_GUARD" "$settings" | grep -F -- '"command"' | head -1)"
    if [[ -z "$entry" ]]; then
        echo "MISSING MCP GUARD COMMAND: $settings wires no PreToolUse command for $MCP_GUARD (#9108)." >&2
        fail=1
    else
        if ! _mcp_contains "$entry" "$MCP_WIRING_LAUNCHER" \
            || ! _mcp_contains "$entry" "$MCP_WIRING_ARGS"; then
            echo "MCP ENTRY BYPASSES hook-wiring.sh: $settings's $MCP_GUARD command does not route through '$MCP_WIRING_LAUNCHER $MCP_WIRING_ARGS' (#9108)." >&2
            echo "    The launcher owns the absent / lost-+x / machine-level-fallback rungs; an" >&2
            echo "    entry that execs the guard directly loses all of them." >&2
            fail=1
        fi
        local prop
        for prop in "$MCP_FLOOR_WORKSPACE_GATE" "$MCP_FLOOR_DENY" "$MCP_FLOOR_ESCAPE"; do
            if ! _mcp_contains "$entry" "$prop"; then
                echo "MCP FAIL-CLOSED FLOOR MISSING ('$prop'): $settings's $MCP_GUARD command does not carry the same broken-install floor the Bash / Edit|Write entries carry (#9108/#7761)." >&2
                echo "    Required, all three: the '$MCP_FLOOR_WORKSPACE_GATE' workspace gate, a" >&2
                echo "    '$MCP_FLOOR_DENY' deny document for a .loom/hooks-bearing workspace whose" >&2
                echo "    copy is absent, and '$MCP_FLOOR_ESCAPE' as the only way past it. A" >&2
                echo "    missing guard is a broken install, not an opt-out." >&2
                fail=1
            fi
        done
    fi

    # (3) the installer's own wiring set. This repo's settings file covers THIS
    # checkout only; _PHOOK_MATCHERS/_PHOOK_NAMES are what a fresh consumer
    # gets, and one without the other is a hole nobody would notice here.
    if [[ -f "$provision" ]]; then
        if ! grep -qF -- "$MCP_MATCHER" "$provision"; then
            echo "MISSING MCP MATCHER IN INSTALLER: $provision's _PHOOK_MATCHERS does not include \"$MCP_MATCHER\" (#9108) — this repo would be guarded but every fresh install would not." >&2
            fail=1
        fi
        if ! grep -qF -- "$MCP_GUARD" "$provision"; then
            echo "MISSING MCP GUARD IN INSTALLER: $provision's _PHOOK_NAMES does not include $MCP_GUARD (#9108)." >&2
            fail=1
        fi
    fi

    # (4) the hook file the wiring names must exist at its source of truth.
    if [[ ! -f "$guard" ]]; then
        echo "MISSING MCP GUARD FILE: $guard does not exist, but the wiring names it (#9108) — every workspace with a .loom/hooks/ directory would DENY every MCP tool call via hook-wiring.sh rung 5." >&2
        fail=1
    fi

    return "$fail"
}

# --- Self-test ---------------------------------------------------------------
# Each fixture isolates ONE discriminating property. A fixture that stops
# failing (or starts failing for a different reason) means the checker lost
# the power it was added for.
_st_fail=0

_st_expect_pass() { # <label> <file>
    local out
    if out="$(check_guard_scan_contracts "$2" 2>&1)"; then
        echo "  ok: $1"
    else
        echo "SELF-TEST FAIL: $1 — fixture was rejected:" >&2
        printf '%s\n' "$out" >&2
        _st_fail=1
    fi
}

_st_expect_fail() { # <label> <file> <expected-substring>
    local out
    if out="$(check_guard_scan_contracts "$2" 2>&1)"; then
        echo "SELF-TEST FAIL: $1 — fixture was NOT rejected (discriminating power regressed):" >&2
        printf '%s\n' "$out" >&2
        _st_fail=1
    elif ! printf '%s\n' "$out" | grep -q "$3"; then
        echo "SELF-TEST FAIL: $1 — rejected for the wrong reason (expected to see: $3):" >&2
        printf '%s\n' "$out" >&2
        _st_fail=1
    else
        echo "  ok: $1"
    fi
}

_st_expect_pass_with() { # <label> <file> <expected-substring>
    local out
    if ! out="$(check_guard_scan_contracts "$2" 2>&1)"; then
        echo "SELF-TEST FAIL: $1 — fixture was rejected:" >&2
        printf '%s\n' "$out" >&2
        _st_fail=1
    elif ! printf '%s\n' "$out" | grep -q "$3"; then
        echo "SELF-TEST FAIL: $1 — passed, but silently (expected to see: $3)" >&2
        printf '%s\n' "$out" >&2
        _st_fail=1
    else
        echo "  ok: $1"
    fi
}

run_self_test() {
    local tmp
    tmp="$(mktemp -d 2>/dev/null)" || {
        echo "self-test: mktemp -d failed" >&2
        return 1
    }
    # shellcheck disable=SC2064  # expand $tmp now, at trap-install time
    trap "rm -rf '$tmp'" RETURN

    echo "check-guard-scan-contracts --self-test: exercising synthetic fixtures..."

    # --- A: compliant — a faithful miniature of the real derivation graph ----
    cat >"$tmp/compliant.sh" <<'EOF'
#!/usr/bin/env bash
COMMAND_NO_LITERAL_TEXT="$COMMAND"  # scan-contract: COMMAND_NO_LITERAL_TEXT=catastrophic-safe from=COMMAND
COMMAND_NO_COMMENT="$COMMAND"  # scan-contract: COMMAND_NO_COMMENT=deny-safe from=COMMAND
COMMAND_ASK_SCAN="$COMMAND_NO_COMMENT"  # scan-contract: COMMAND_ASK_SCAN=deny-safe from=COMMAND_NO_COMMENT
COMMAND_CLOUD_ASK_SCAN="$COMMAND_ASK_SCAN"  # scan-contract: COMMAND_CLOUD_ASK_SCAN=ask-only from=COMMAND_ASK_SCAN

for pattern in "${ALWAYS_BLOCK_PATTERNS[@]}"; do
    if echo "$COMMAND_NO_LITERAL_TEXT" | grep -qiE "$pattern"; then
        deny "BLOCKED: Command matches dangerous pattern: $pattern" "catastrophic:$pattern"  # scan-reads: COMMAND_NO_LITERAL_TEXT
    fi
done

if echo "$COMMAND_ASK_SCAN" | grep -qiE "$SQL_DDL_PATTERN"; then
    deny "BLOCKED: dangerous pattern (#1234)" "sql-ddl"  # scan-reads: COMMAND_ASK_SCAN
fi

if echo "$COMMAND_CLOUD_ASK_SCAN" | grep -qE "$pattern"; then
    ask "Command requires confirmation: $COMMAND" "cloud-delete-ask"  # scan-reads: COMMAND_CLOUD_ASK_SCAN
fi

if echo "$COMMAND" | grep -qiE "$GH_COMMENT_BODY_AT_PATTERN"; then
    deny "BLOCKED: literal @path (#4523)" "gh-comment-body-literal-at"  # scan-reads: none
fi
EOF
    _st_expect_pass "compliant fixture passes" "$tmp/compliant.sh"

    # --- B: the #6252 shape — an ask-only copy decides a DENY ----------------
    cat >"$tmp/ask-into-deny.sh" <<'EOF'
#!/usr/bin/env bash
COMMAND_ASK_SCAN="$COMMAND"  # scan-contract: COMMAND_ASK_SCAN=ask-only from=COMMAND

WRITE_TARGETS=$(extract_write_targets "$COMMAND_ASK_SCAN" "$CWD")
if [[ -n "$WRITE_TARGETS" ]]; then
    deny "BLOCKED: write outside the worktree (#4178)" "worktree-write-confinement"  # scan-reads: COMMAND_ASK_SCAN
fi
EOF
    _st_expect_fail "the #6252 ask-only-into-deny shape is rejected" "$tmp/ask-into-deny.sh" \
        "VIOLATION:.*'COMMAND_ASK_SCAN' is declared 'ask-only' but is read by a 'deny'-tier decision site"

    # --- C: same shape, explicitly waived ------------------------------------
    cat >"$tmp/waived.sh" <<'EOF'
#!/usr/bin/env bash
COMMAND_ASK_SCAN="$COMMAND"  # scan-contract: COMMAND_ASK_SCAN=ask-only from=COMMAND

if echo "$COMMAND_ASK_SCAN" | grep -qE "$pattern"; then
    deny "BLOCKED: example" "example-deny"  # scan-reads: COMMAND_ASK_SCAN  scan-waiver: fixture-only, exercises the waiver path
fi
EOF
    _st_expect_pass_with "a recorded waiver passes but stays visible" "$tmp/waived.sh" \
        "WARNING:.*WAIVED"

    # --- D: deny-safe copy reaching the ungated catastrophic floor -----------
    # The reservation COMMAND_NO_COMMENT's header has always claimed in prose:
    # comment stripping is for the ASK/DDL tier, never for ALWAYS_BLOCK.
    cat >"$tmp/into-catastrophic.sh" <<'EOF'
#!/usr/bin/env bash
COMMAND_NO_COMMENT="$COMMAND"  # scan-contract: COMMAND_NO_COMMENT=deny-safe from=COMMAND

for pattern in "${ALWAYS_BLOCK_PATTERNS[@]}"; do
    if echo "$COMMAND_NO_COMMENT" | grep -qiE "$pattern"; then
        deny "BLOCKED: Command matches dangerous pattern: $pattern" "catastrophic:$pattern"  # scan-reads: COMMAND_NO_COMMENT
    fi
done
EOF
    _st_expect_fail "a deny-safe copy feeding the ALWAYS_BLOCK floor is rejected" "$tmp/into-catastrophic.sh" \
        "VIOLATION:.*'COMMAND_NO_COMMENT' is declared 'deny-safe' but is read by a 'catastrophic'-tier decision site"

    # --- E: laundering — a branch declared safer than its parent -------------
    cat >"$tmp/laundering.sh" <<'EOF'
#!/usr/bin/env bash
COMMAND_CLOUD_ASK_SCAN="$COMMAND"  # scan-contract: COMMAND_CLOUD_ASK_SCAN=ask-only from=COMMAND
COMMAND_LAUNDERED_SCAN="$COMMAND_CLOUD_ASK_SCAN"  # scan-contract: COMMAND_LAUNDERED_SCAN=deny-safe from=COMMAND_CLOUD_ASK_SCAN

if echo "$COMMAND_LAUNDERED_SCAN" | grep -qE "$pattern"; then
    deny "BLOCKED: example" "example-deny"  # scan-reads: COMMAND_LAUNDERED_SCAN
fi
EOF
    _st_expect_fail "laundering an ask-only copy through a 'deny-safe' branch is rejected" "$tmp/laundering.sh" \
        "LAUNDERING:.*'COMMAND_LAUNDERED_SCAN' is declared 'deny-safe' but is derived from 'COMMAND_CLOUD_ASK_SCAN'"

    # --- F: a decision site with no annotation at all ------------------------
    cat >"$tmp/missing-annotation.sh" <<'EOF'
#!/usr/bin/env bash
COMMAND_ASK_SCAN="$COMMAND"  # scan-contract: COMMAND_ASK_SCAN=deny-safe from=COMMAND

if echo "$COMMAND_ASK_SCAN" | grep -qE "$pattern"; then
    deny "BLOCKED: example" "example-deny"
fi
EOF
    _st_expect_fail "an unannotated deny() call is rejected" "$tmp/missing-annotation.sh" \
        "no '# scan-reads:' annotation"

    # --- G: a stale annotation on a non-decision line ------------------------
    cat >"$tmp/stale-annotation.sh" <<'EOF'
#!/usr/bin/env bash
COMMAND_ASK_SCAN="$COMMAND"  # scan-contract: COMMAND_ASK_SCAN=deny-safe from=COMMAND

RM_TARGETS=$(extract_rm_targets "$COMMAND_ASK_SCAN")  # scan-reads: COMMAND_ASK_SCAN
EOF
    _st_expect_fail "a stale scan-reads on a non-decision line is rejected" "$tmp/stale-annotation.sh" \
        "calls neither deny() nor ask()"

    # --- H: a reference to an undeclared variable ----------------------------
    cat >"$tmp/undeclared.sh" <<'EOF'
#!/usr/bin/env bash
COMMAND_ASK_SCAN="$COMMAND"  # scan-contract: COMMAND_ASK_SCAN=deny-safe from=COMMAND

if echo "$COMMAND_TYPO_SCAN" | grep -qE "$pattern"; then
    deny "BLOCKED: example" "example-deny"  # scan-reads: COMMAND_TYPO_SCAN
fi
EOF
    _st_expect_fail "a scan-reads reference to an undeclared variable is rejected" "$tmp/undeclared.sh" \
        "references 'COMMAND_TYPO_SCAN', which has no scan-contract declaration"

    # --- I: the same variable declared at two tiers --------------------------
    cat >"$tmp/inconsistent.sh" <<'EOF'
#!/usr/bin/env bash
COMMAND_ASK_SCAN="$COMMAND"  # scan-contract: COMMAND_ASK_SCAN=deny-safe from=COMMAND
COMMAND_ASK_SCAN="$COMMAND_ASK_SCAN"  # scan-contract: COMMAND_ASK_SCAN=ask-only from=COMMAND_ASK_SCAN
EOF
    _st_expect_fail "an inconsistent re-declaration is rejected" "$tmp/inconsistent.sh" \
        "is declared 'ask-only' here but 'deny-safe' elsewhere"

    # --- J: a new derived copy with no contract at all -----------------------
    cat >"$tmp/unclassified.sh" <<'EOF'
#!/usr/bin/env bash
COMMAND_ASK_SCAN="$COMMAND"  # scan-contract: COMMAND_ASK_SCAN=deny-safe from=COMMAND
COMMAND_NEW_SCAN="$COMMAND_ASK_SCAN"
EOF
    _st_expect_fail "a never-classified new derivation is rejected" "$tmp/unclassified.sh" \
        "'COMMAND_NEW_SCAN' is assigned but never classified"

    # --- K: a contract with no from= clause ----------------------------------
    cat >"$tmp/no-parent.sh" <<'EOF'
#!/usr/bin/env bash
COMMAND_ASK_SCAN="$COMMAND"  # scan-contract: COMMAND_ASK_SCAN=deny-safe
EOF
    _st_expect_fail "a contract with no 'from=' parent is rejected" "$tmp/no-parent.sh" \
        "has no 'from=<PARENT>' clause"

    # --- L-P: the #9108 PreToolUse matcher-coverage contract -----------------
    _st_wiring_fixtures "$tmp"

    if [[ "$_st_fail" -ne 0 ]]; then
        echo "" >&2
        echo "check-guard-scan-contracts --self-test: FAIL — the checker's discriminating power has regressed." >&2
        return 1
    fi

    echo "check-guard-scan-contracts --self-test: OK — compliant and waived fixtures pass; the #6252 ask-only-into-deny shape, a deny-safe copy reaching the catastrophic floor, a laundering branch, a missing annotation, a stale annotation, an undeclared reference, an inconsistent re-declaration, an unclassified derivation, and a parentless contract are all rejected; and the #9108 wiring contract rejects a settings file with no mcp__loom__.* matcher, one with no fail-closed floor, one that bypasses hook-wiring.sh, and an installer whose _PHOOK_* arrays never learned the matcher."
    return 0
}

# --- Self-test: the #9108 wiring contract ------------------------------------

_stw_expect_pass() { # <label> <root>
    local out
    if out="$(check_mcp_guard_wiring "$2" 2>&1)"; then
        echo "  ok: $1"
    else
        echo "SELF-TEST FAIL: $1 — fixture was rejected:" >&2
        printf '%s\n' "$out" >&2
        _st_fail=1
    fi
}

_stw_expect_fail() { # <label> <root> <expected-substring>
    local out
    if out="$(check_mcp_guard_wiring "$2" 2>&1)"; then
        echo "SELF-TEST FAIL: $1 — fixture was NOT rejected (discriminating power regressed):" >&2
        printf '%s\n' "$out" >&2
        _st_fail=1
    elif ! _mcp_contains "$out" "$3"; then
        echo "SELF-TEST FAIL: $1 — rejected for the wrong reason (expected to see: $3):" >&2
        printf '%s\n' "$out" >&2
        _st_fail=1
    else
        echo "  ok: $1"
    fi
}

# Build a fixture workspace under <root>. The settings command string is the
# real shape, abbreviated to the properties the contract asserts.
#
# shellcheck disable=SC2016  # the `$L`/`$W`/`$G` expansions are FIXTURE TEXT — they must reach the file unexpanded, exactly as the real settings.json carries them
_stw_fixture() { # <root> [--no-matcher|--no-floor|--bypass-wiring|--no-installer]
    local root="$1" variant="${2:-}"
    local matcher='mcp__loom__.*'
    local route='exec bash "$L" PreToolUse guard-mcp-tools.sh'
    local floor='[ -d "$W/.loom/hooks" ] || exit 0; [ "${LOOM_GUARD_WIRING_FAILOPEN:-0}" = "1" ] && exit 0; printf %s "{\"hookSpecificOutput\":{\"permissionDecision\":\"deny\"}}"'
    local launcher='L=$W/.loom/hooks/hook-wiring.sh;'

    case "$variant" in
        --no-matcher) matcher='Bash' ;;
        --no-floor) floor='exit 0' ;;
        --bypass-wiring)
            launcher='G=$W/.loom/hooks/guard-mcp-tools.sh;'
            route='exec "$G"'
            ;;
    esac

    mkdir -p "$root/.claude" "$root/scripts/install" "$root/defaults/hooks"
    # One physical line per hook command, as in the real settings file.
    {
        echo '{ "hooks": { "PreToolUse": ['
        echo "  { \"matcher\": \"$matcher\","
        echo "    \"hooks\": [ { \"type\": \"command\", \"command\": \"bash -c '$launcher $route; $floor'\" } ] }"
        echo '] } }'
    } >"$root/.claude/settings.json"

    if [[ "$variant" == "--no-installer" ]]; then
        printf '_PHOOK_MATCHERS=(Bash)\n_PHOOK_NAMES=(guard-destructive.sh)\n' \
            >"$root/scripts/install/provision-hooks.sh"
    else
        printf '_PHOOK_MATCHERS=(Bash "mcp__loom__.*")\n_PHOOK_NAMES=(guard-destructive.sh guard-mcp-tools.sh)\n' \
            >"$root/scripts/install/provision-hooks.sh"
    fi
    printf '#!/usr/bin/env bash\nexit 0\n' >"$root/defaults/hooks/guard-mcp-tools.sh"
}

_st_wiring_fixtures() { # <tmpdir>
    local tmp="$1"

    _stw_fixture "$tmp/w-ok"
    _stw_expect_pass "(#9108) a compliant wiring fixture passes" "$tmp/w-ok"

    _stw_fixture "$tmp/w-no-matcher" --no-matcher
    _stw_expect_fail "(#9108) a settings file with no mcp__loom__.* matcher is rejected" \
        "$tmp/w-no-matcher" "MISSING MCP MATCHER"

    _stw_fixture "$tmp/w-no-floor" --no-floor
    _stw_expect_fail "(#9108) an MCP entry with no fail-closed floor is rejected" \
        "$tmp/w-no-floor" "MCP FAIL-CLOSED FLOOR MISSING"

    _stw_fixture "$tmp/w-bypass" --bypass-wiring
    _stw_expect_fail "(#9108) an MCP entry that bypasses hook-wiring.sh is rejected" \
        "$tmp/w-bypass" "MCP ENTRY BYPASSES hook-wiring.sh"

    _stw_fixture "$tmp/w-no-installer" --no-installer
    _stw_expect_fail "(#9108) an installer whose _PHOOK_* arrays lack the matcher is rejected" \
        "$tmp/w-no-installer" "MISSING MCP MATCHER IN INSTALLER"

    _stw_fixture "$tmp/w-no-guard"
    rm -f "$tmp/w-no-guard/defaults/hooks/guard-mcp-tools.sh"
    _stw_expect_fail "(#9108) wiring that names a guard file which does not exist is rejected" \
        "$tmp/w-no-guard" "MISSING MCP GUARD FILE"
}

# --- Entry point -------------------------------------------------------------
if [[ "${1:-}" == "--self-test" ]]; then
    run_self_test
    exit $?
fi

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
if ROOT="$(git -C "$SCRIPT_DIR" rev-parse --show-toplevel 2>/dev/null)"; then
    :
else
    ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
fi

# `--wiring` runs only the #9108 matcher-coverage contract, for anyone
# exercising that half on its own.
if [[ "${1:-}" == "--wiring" ]]; then
    if check_mcp_guard_wiring "$ROOT"; then
        echo "check-guard-scan-contracts: OK — the mcp__loom__.* PreToolUse matcher is wired in both .claude/settings.json and the installer, and carries the same fail-closed broken-install floor as the Bash / Edit|Write entries."
        exit 0
    fi
    echo "" >&2
    echo "check-guard-scan-contracts: FAIL — see above. MCP tool calls would run with" >&2
    echo "no guard hook on their path, which is the #9108 gap." >&2
    exit 1
fi

# An explicit PATH keeps this a pure per-file tier check (documented above); the
# wiring contract is a whole-repo property and only runs in the default mode.
WIRING=0
if [[ $# -ge 1 && -n "${1:-}" ]]; then
    TARGET="$1"
else
    TARGET="$ROOT/defaults/hooks/guard-destructive-generic.sh"
    WIRING=1
fi

if [[ ! -f "$TARGET" ]]; then
    echo "check-guard-scan-contracts: no such file: $TARGET — nothing to check (ok)."
    exit 0
fi

RC=0
if check_guard_scan_contracts "$TARGET"; then
    echo "check-guard-scan-contracts: OK — every derived scan copy is classified and derived from a copy at least as strict, every deny()/ask() site records what it reads, and no site decides at a stricter tier than the copy it reads was declared for."
else
    RC=1
    {
        echo ""
        echo "check-guard-scan-contracts: FAIL — see above."
        echo ""
        echo "guard-destructive-generic.sh decides 'is this executable code or inert"
        echo "data?' by matching patterns against a chain of LOSSY derived copies of"
        echo "\$COMMAND. Each copy's masking was only ever proven safe for ONE consumer"
        echo "tier. A copy reaching a STRICTER consumer than it was declared for turns"
        echo "an accepted risk (a missed ask) into a security bypass (a missed deny) —"
        echo "that shipped once already (#6252, ADR-0016)."
        echo ""
        echo "Marker format and the full rationale: this script's own header comment."
        echo "Current inventory: defaults/docs/guard-scan-contracts.md"
    } >&2
fi

if [[ "$WIRING" -eq 1 ]]; then
    if check_mcp_guard_wiring "$ROOT"; then
        echo "check-guard-scan-contracts: OK — the mcp__loom__.* PreToolUse matcher is wired in both .claude/settings.json and the installer, and carries the same fail-closed broken-install floor as the Bash / Edit|Write entries."
    else
        RC=1
        {
            echo ""
            echo "check-guard-scan-contracts: FAIL — see above. MCP tool calls would run with"
            echo "no guard hook on their path, which is the #9108 gap: mcp-loom is registered at"
            echo "user scope and callable from every agent Loom spawns, and get_agent_metrics"
            echo "built a shell command line from its raw arguments until #9107. A PreToolUse"
            echo "matcher that is missing does not error — it just never fires."
            echo ""
            echo "Catalog entry and the category's toggle: defaults/docs/guard-hooks.md"
        } >&2
    fi
fi

exit "$RC"
