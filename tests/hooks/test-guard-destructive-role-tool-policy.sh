#!/usr/bin/env bash
# Test suite for defaults/hooks/guard-destructive-generic.sh — the PER-ROLE
# TOOL-RESTRICTION allowlist backstop (issue #8256).
#
# One slice of the tests/hooks/test-guard-destructive-*.sh family (#7741).
# Shared fixtures, assertions and catastrophic-phrase payloads live in
# tests/hooks/lib/guard-destructive-harness.sh.
#
# WHAT THIS SUITE IS FOR. #8256's acceptance criteria are all of the form "a
# persuaded read-only role CANNOT reach X" — a claim about what the harness
# refuses, not about what a prompt says. A configuration that parses correctly
# but never denies would satisfy every other check in this repo and leave the
# security control non-functional, so every case below asserts an actual
# deny/allow VERDICT from the guard, driven by a real role declaration on disk.
#
# Fixture roles are named `fixture-*` deliberately: the guard resolves a role's
# declaration from its OWN sibling roles directory first (defaults/roles/ under
# the test harness), so a fixture that reused a shipped role's name could never
# be overridden from a temp repo. The `fixture-*` names exist only in the temp
# repo, resolve through the $REPO_ROOT fallback, and therefore exercise the
# allowlist semantics independently of what the shipped roles happen to declare
# — while the SHIPPED-ROLE section below separately pins what those roles
# actually declare today.
#
# Usage: ./tests/hooks/test-guard-destructive-role-tool-policy.sh

set -euo pipefail
# shellcheck source=tests/hooks/lib/guard-destructive-harness.sh
. "$(cd "$(dirname "$0")" && pwd)/lib/guard-destructive-harness.sh"

echo -e "${YELLOW}--- Per-role tool-restriction allowlist (toolPolicy.allowedCapabilities, #8256) ---${NC}"

# Build a throwaway git repo carrying fixture role declarations under
# .loom/roles/. These resolve through the guard's third (REPO_ROOT) lookup, so
# each fixture is a self-contained policy the shipped roles cannot influence.
make_role_repo() {
    local dir
    dir=$(mktemp -d 2>/dev/null)
    dir=$(cd "$dir" && pwd -P)
    git -C "$dir" init -q >/dev/null 2>&1
    mkdir -p "$dir/.loom/roles"
    # Restricted to nothing — the shape every read-only role ships with.
    printf '%s' '{"name":"Fixture RO","toolPolicy":{"allowedCapabilities":[]}}' \
        > "$dir/.loom/roles/fixture-restricted.json"
    # Explicit wildcard — the shape builder/doctor/driver/loom ship with.
    printf '%s' '{"name":"Fixture Open","toolPolicy":{"allowedCapabilities":["*"]}}' \
        > "$dir/.loom/roles/fixture-open.json"
    # Partial grant — proves the allowlist is per capability, not all-or-nothing.
    printf '%s' '{"name":"Fixture Cloud","toolPolicy":{"allowedCapabilities":["cloud-cli"]}}' \
        > "$dir/.loom/roles/fixture-cloud.json"
    # No toolPolicy at all — a consumer repo's custom role, or a pre-#8256
    # resync. MUST stay unrestricted.
    printf '%s' '{"name":"Fixture Undeclared","suggestedModel":"sonnet"}' \
        > "$dir/.loom/roles/fixture-undeclared.json"
    # A declaration naming only capabilities the guard does not know about.
    # An unknown name is inert — it must NOT act as a wildcard.
    printf '%s' '{"name":"Fixture Bogus","toolPolicy":{"allowedCapabilities":["not-a-capability"]}}' \
        > "$dir/.loom/roles/fixture-bogus.json"
    echo "$dir"
}

ROLE_REPO=$(make_role_repo)

# assert_role_deny <description> <LOOM_ROLE> <command> [cwd]
assert_role_deny() {
    assert_deny_env "$1" "LOOM_ROLE=$2" "$3" "${4:-$ROLE_REPO}"
}
# assert_role_allow <description> <LOOM_ROLE> <command> [cwd]
assert_role_allow() {
    assert_allow_env "$1" "LOOM_ROLE=$2" "$3" "${4:-$ROLE_REPO}"
}

# assert_role_deny_reason_matches <description> <LOOM_ROLE> <command> <ere> [cwd]
#
# The harness's assert_deny_reason_matches() runs the guard with NO env, which
# for this category means no role identity and therefore no deny at all. This
# is its env-carrying sibling: everything in this category's value is in what
# the deny message SAYS (which declaration to change, that there is no toggle,
# and that an unintended hit is itself a signal), so the message text is
# asserted rather than just the verdict.
assert_role_deny_reason_matches() {
    local description="$1" role="$2" cmd="$3" pattern="$4" cwd="${5:-$ROLE_REPO}"
    TOTAL=$((TOTAL + 1))
    local output reason
    output=$(run_guard_env "LOOM_ROLE=$role" "$cmd" "$cwd") || true
    reason=$(echo "$output" | jq -r '.hookSpecificOutput.permissionDecisionReason // empty' 2>/dev/null)
    if echo "$output" | jq -e '.hookSpecificOutput.permissionDecision == "deny"' >/dev/null 2>&1 && \
       grep -qE "$pattern" <<<"$reason"; then
        PASS=$((PASS + 1))
        echo -e "  ${GREEN}PASS${NC}: $description"
    else
        FAIL=$((FAIL + 1))
        echo -e "  ${RED}FAIL${NC}: $description"
        echo -e "       Command: $cmd (LOOM_ROLE=$role)"
        echo -e "       Expected: deny with reason matching /$pattern/"
        echo -e "       Got: $output"
    fi
}

# =========================================================================
# 1. The four named surfaces from #8256's acceptance criteria, denied for a
#    role whose declaration grants nothing.
# =========================================================================
echo -e "${YELLOW}  [1] restricted fixture: each of the four named surfaces denies${NC}"

assert_role_deny "role-tool-policy: ssh denies for a restricted role" \
    "fixture-restricted" "ssh deploy@example.com"
assert_role_deny "role-tool-policy: scp denies for a restricted role" \
    "fixture-restricted" "scp /tmp/f deploy@example.com:/tmp/f"
assert_role_deny "role-tool-policy: ssh-keygen denies for a restricted role" \
    "fixture-restricted" "ssh-keygen -t ed25519 -f /tmp/k"
assert_role_deny "role-tool-policy: aws denies for a restricted role" \
    "fixture-restricted" "aws sts get-caller-identity"
assert_role_deny "role-tool-policy: gcloud denies for a restricted role" \
    "fixture-restricted" "gcloud auth print-access-token"
assert_role_deny "role-tool-policy: gh secret set denies for a restricted role" \
    "fixture-restricted" "gh secret set DEPLOY_KEY --body xyz"
assert_role_deny "role-tool-policy: gh secret list denies for a restricted role" \
    "fixture-restricted" "gh secret list"
assert_role_deny "role-tool-policy: gh auth token denies for a restricted role" \
    "fixture-restricted" "gh auth token"
assert_role_deny "role-tool-policy: a write under ~/.ssh denies for a restricted role" \
    "fixture-restricted" "echo ssh-ed25519 AAAA > ~/.ssh/authorized_keys"
assert_role_deny "role-tool-policy: a \$HOME-spelled write under .ssh denies too" \
    "fixture-restricted" 'cp /tmp/pubkey $HOME/.ssh/authorized_keys'
assert_role_deny "role-tool-policy: tee into ~/.aws/credentials denies" \
    "fixture-restricted" "echo x | tee ~/.aws/credentials"
assert_role_deny "role-tool-policy: mkdir ~/.ssh denies" \
    "fixture-restricted" "mkdir -p ~/.ssh"
assert_role_deny "role-tool-policy: reading a private key with cat denies" \
    "fixture-restricted" "cat ~/.ssh/id_ed25519"
assert_role_deny "role-tool-policy: exfiltrating a token store with tar denies" \
    "fixture-restricted" "tar czf /tmp/t.tgz .loom/tokens"

# The deny must name the mechanism, the declaration, and the remedy — a bare
# "blocked" would leave a persuaded role hunting for another route rather than
# recognising the anomaly.
assert_role_deny_reason_matches "role-tool-policy: deny names the per-role mechanism and issue" \
    "fixture-restricted" "ssh deploy@example.com" 'per-role tool restriction, issue #8256'
assert_role_deny_reason_matches "role-tool-policy: deny names the toolPolicy field to change" \
    "fixture-restricted" "ssh deploy@example.com" 'toolPolicy\.allowedCapabilities'
assert_role_deny_reason_matches "role-tool-policy: deny names the declaration FILE it read" \
    "fixture-restricted" "ssh deploy@example.com" '\.loom/roles/fixture-restricted\.json'
assert_role_deny_reason_matches "role-tool-policy: deny names the capability to add" \
    "fixture-restricted" "ssh deploy@example.com" "add 'remote-shell' to that array"
assert_role_deny_reason_matches "role-tool-policy: deny states there is no toggle/env bypass" \
    "fixture-restricted" "ssh deploy@example.com" 'no guards\.\* toggle and no env override'
assert_role_deny_reason_matches "role-tool-policy: deny points at the untrusted-content convention" \
    "fixture-restricted" "ssh deploy@example.com" 'untrusted-external-content\.md'
assert_role_deny_reason_matches "role-tool-policy: a partial grant is echoed back in the deny" \
    "fixture-cloud" "ssh deploy@example.com" 'currently grants: cloud-cli'
assert_role_deny_reason_matches "role-tool-policy: an empty grant renders as '(nothing)', not a blank" \
    "fixture-restricted" "ssh deploy@example.com" 'currently grants: \(nothing\)'
assert_role_deny_reason_matches "role-tool-policy: a credential-store write deny names the resolved path" \
    "fixture-restricted" "echo k > ~/.ssh/authorized_keys" "writing '${HOME}/.ssh/authorized_keys'"

# =========================================================================
# 2. The unrestricted fixture is UNCHANGED — the acceptance criterion that
#    Builder/Doctor keep what they need.
# =========================================================================
echo -e "${YELLOW}  [2] unrestricted fixtures: no new denials${NC}"

assert_role_allow "role-tool-policy: ssh allows for a \"*\" fixture" \
    "fixture-open" "ssh deploy@example.com"
assert_role_allow "role-tool-policy: aws allows for a \"*\" fixture" \
    "fixture-open" "aws sts get-caller-identity"
assert_role_allow "role-tool-policy: gh secret set allows for a \"*\" fixture" \
    "fixture-open" "gh secret set DEPLOY_KEY --body xyz"
assert_role_allow "role-tool-policy: a write under ~/.ssh allows for a \"*\" fixture" \
    "fixture-open" "echo k > ~/.ssh/authorized_keys"

# A role file with NO toolPolicy at all must behave exactly as it did before
# #8256 — this is the compatibility contract for consumer repos' custom roles.
assert_role_allow "role-tool-policy: ssh allows for a role declaring no toolPolicy" \
    "fixture-undeclared" "ssh deploy@example.com"
assert_role_allow "role-tool-policy: gh secret allows for a role declaring no toolPolicy" \
    "fixture-undeclared" "gh secret list"

# ...and so must a LOOM_ROLE naming a role file that does not exist at all.
assert_role_allow "role-tool-policy: an unknown LOOM_ROLE is unrestricted (fails open on identity)" \
    "fixture-does-not-exist" "ssh deploy@example.com"

# An unset LOOM_ROLE — every interactive session — is untouched.
assert_allow "role-tool-policy: ssh allows with LOOM_ROLE unset" \
    "ssh deploy@example.com" "$ROLE_REPO"
assert_allow "role-tool-policy: gh secret allows with LOOM_ROLE unset" \
    "gh secret list" "$ROLE_REPO"

# =========================================================================
# 3. The allowlist is PER CAPABILITY, and unknown names are inert.
# =========================================================================
echo -e "${YELLOW}  [3] partial grants and unknown capability names${NC}"

assert_role_allow "role-tool-policy: a cloud-cli grant permits aws" \
    "fixture-cloud" "aws sts get-caller-identity"
assert_role_deny "role-tool-policy: a cloud-cli grant does NOT permit ssh" \
    "fixture-cloud" "ssh deploy@example.com"
assert_role_deny "role-tool-policy: a cloud-cli grant does NOT permit gh secret" \
    "fixture-cloud" "gh secret list"
assert_role_deny "role-tool-policy: a cloud-cli grant does NOT permit a ~/.ssh write" \
    "fixture-cloud" "echo k > ~/.ssh/authorized_keys"

# An unrecognised capability name must not act as a wildcard: the role still
# has an allowlist, and that allowlist grants nothing the guard knows about.
assert_role_deny "role-tool-policy: an unknown capability name is inert, not a wildcard (ssh)" \
    "fixture-bogus" "ssh deploy@example.com"
assert_role_deny "role-tool-policy: an unknown capability name is inert, not a wildcard (aws)" \
    "fixture-bogus" "aws sts get-caller-identity"

# =========================================================================
# 4. Precision: the detectors are command-word anchored, so ordinary role
#    traffic and prose that merely MENTIONS a surface are untouched. A guard
#    that false-denies here is one an agent learns to route around.
# =========================================================================
echo -e "${YELLOW}  [4] precision: no false denials on ordinary restricted-role traffic${NC}"

assert_role_allow "role-tool-policy: gh auth status is not a forge-secrets surface" \
    "fixture-restricted" "gh auth status"
assert_role_allow "role-tool-policy: gh pr list is unaffected" \
    "fixture-restricted" "gh pr list --label loom:review-requested"
assert_role_allow "role-tool-policy: gh issue view is unaffected" \
    "fixture-restricted" "gh issue view 8256 --comments"
assert_role_allow "role-tool-policy: git status is unaffected" \
    "fixture-restricted" "git status"
assert_role_allow "role-tool-policy: an ordinary /tmp write is unaffected" \
    "fixture-restricted" "echo hello > /tmp/loom-role-policy-$$.txt"
assert_role_allow "role-tool-policy: grepping prose that mentions ~/.ssh does not deny" \
    "fixture-restricted" "grep -rn '~/.ssh' defaults/docs"
assert_role_allow "role-tool-policy: a PR comment quoting 'ssh' as prose does not deny" \
    "fixture-restricted" "gh pr comment 1 --body 'consider whether ssh access is needed here'"
assert_role_allow "role-tool-policy: a variable named ssh_key does not deny" \
    "fixture-restricted" "echo \"ssh_key=absent\" > /tmp/loom-role-policy-var-$$.txt"

# `sudo`/`env` wrappers must NOT launder a command word past the detector —
# this mirrors the same unwrapping lifecycle_or_cloud_reason() does (#3586).
assert_role_deny "role-tool-policy: a sudo-wrapped ssh still denies" \
    "fixture-restricted" "sudo ssh deploy@example.com"
assert_role_deny "role-tool-policy: an env-wrapped aws still denies" \
    "fixture-restricted" "env AWS_PROFILE=prod aws sts get-caller-identity"
assert_role_deny "role-tool-policy: an absolute-path ssh still denies" \
    "fixture-restricted" "/usr/bin/ssh deploy@example.com"
assert_role_deny "role-tool-policy: ssh after a separator in a compound command still denies" \
    "fixture-restricted" "git status; ssh deploy@example.com"

# =========================================================================
# 5. The read-only fast path (#3687) must not short-circuit this category.
#
#    The fast path runs BEFORE REPO_ROOT and before every toggle read, and its
#    built-in allowlist admitted `gh <x> list|view` and read-only `aws` verbs
#    to a silent allow. `gh secret list` and `aws sts get-caller-identity` —
#    exactly the reconnaissance a persuaded role runs first — escaped through
#    it. These cases run with the fast path at its DEFAULT (enabled), which is
#    the configuration the hole existed in.
# =========================================================================
echo -e "${YELLOW}  [5] read-only fast-path deferral (#3687 x #8256)${NC}"

assert_role_deny "role-tool-policy: 'gh secret list' is not fast-pathed for a restricted role" \
    "fixture-restricted" "gh secret list"
assert_role_deny "role-tool-policy: 'gh variable list' is not fast-pathed for a restricted role" \
    "fixture-restricted" "gh variable list"
assert_role_deny "role-tool-policy: 'aws s3 ls' is not fast-pathed for a restricted role" \
    "fixture-restricted" "aws s3 ls"
assert_role_deny "role-tool-policy: 'aws ec2 describe-instances' is not fast-pathed" \
    "fixture-restricted" "aws ec2 describe-instances"

# The deferral is scoped: it must not cost the common role idioms their fast
# path, and must not change anything for a session with no role identity.
assert_role_allow "role-tool-policy: 'gh pr list' keeps its fast path under a restricted role" \
    "fixture-restricted" "gh pr list"
assert_allow "role-tool-policy: 'aws s3 ls' is still fast-pathed with LOOM_ROLE unset" \
    "aws s3 ls" "$ROLE_REPO"
assert_allow "role-tool-policy: 'gh secret list' is still fast-pathed with LOOM_ROLE unset" \
    "gh secret list" "$ROLE_REPO"

# =========================================================================
# 6. SHIPPED ROLES. #8256 names seven roles that must be restricted and two
#    that must not be. This section pins what defaults/roles/*.json actually
#    declares, resolved through the guard's FIRST lookup ($SCRIPT_DIR/../roles),
#    so a future edit that silently drops a role's declaration fails here.
# =========================================================================
echo -e "${YELLOW}  [6] shipped roles: the seven read-only roles are restricted, builder/doctor are not${NC}"

for _role in architect auditor champion curator guide hermit judge; do
    assert_role_deny "role-tool-policy: shipped role '$_role' cannot invoke ssh" \
        "$_role" "ssh deploy@example.com"
    assert_role_deny "role-tool-policy: shipped role '$_role' cannot invoke aws" \
        "$_role" "aws sts get-caller-identity"
    assert_role_deny "role-tool-policy: shipped role '$_role' cannot invoke gh secret" \
        "$_role" "gh secret list"
    assert_role_deny "role-tool-policy: shipped role '$_role' cannot write under ~/.ssh" \
        "$_role" "echo k > ~/.ssh/authorized_keys"
done

for _role in builder doctor driver loom; do
    assert_role_allow "role-tool-policy: shipped role '$_role' keeps ssh" \
        "$_role" "ssh deploy@example.com"
    assert_role_allow "role-tool-policy: shipped role '$_role' keeps aws" \
        "$_role" "aws sts get-caller-identity"
    assert_role_allow "role-tool-policy: shipped role '$_role' keeps gh secret" \
        "$_role" "gh secret set X --body y"
    assert_role_allow "role-tool-policy: shipped role '$_role' keeps ~/.ssh writes" \
        "$_role" "echo k > ~/.ssh/authorized_keys"
done

# The daemon's dispatch aliases must resolve to the SAME policy the role they
# name would get — a sweep child that landed on a restricted policy because its
# LOOM_ROLE spelling was not recognised would break every sweep on the fleet.
assert_role_allow "role-tool-policy: LOOM_ROLE=sweep-lifecycle resolves to builder's policy" \
    "sweep-lifecycle" "ssh deploy@example.com"
assert_role_allow "role-tool-policy: LOOM_ROLE=development-worker resolves to builder's policy" \
    "development-worker" "ssh deploy@example.com"
assert_role_allow "role-tool-policy: LOOM_ROLE=pr-fixer resolves to doctor's policy" \
    "pr-fixer" "ssh deploy@example.com"
assert_role_deny "role-tool-policy: LOOM_ROLE is matched case-insensitively (CURATOR)" \
    "CURATOR" "ssh deploy@example.com"

# A LOOM_ROLE carrying path separators or dots must never be turned into a
# file lookup outside the roles directory — it resolves to no role at all.
assert_role_allow "role-tool-policy: a path-traversal LOOM_ROLE resolves to no policy (fails open on identity)" \
    "../../etc/passwd" "ssh deploy@example.com"

# =========================================================================
# 7. Regression: the #6021 _WT_READONLY_ROLES carve-out this category sits
#    alongside is untouched. Both key on LOOM_ROLE; they must stay independent.
# =========================================================================
echo -e "${YELLOW}  [7] regression: the #6021 dist/ carve-out still holds${NC}"

WT_ROLE_REPO=$(make_wt_repo)
mkdir -p "$WT_ROLE_REPO/dist"

assert_allow_env "role-tool-policy: #6021 dist/ staging still allowed for a read-only role" \
    "LOOM_ROLE=auditor" "cp /tmp/loom-daemon $WT_ROLE_REPO/dist/loom-daemon-x86_64" "$WT_ROLE_REPO"
assert_deny_env "role-tool-policy: #4178 main-checkout write still denied for a read-only role" \
    "LOOM_ROLE=auditor" "echo x > $WT_ROLE_REPO/defaults/hooks/f.sh" "$WT_ROLE_REPO"
assert_deny_env "role-tool-policy: #4178 main-checkout write still denied for builder" \
    "LOOM_ROLE=builder" "echo x > $WT_ROLE_REPO/defaults/hooks/f.sh" "$WT_ROLE_REPO"

rm -rf "$ROLE_REPO" "$WT_ROLE_REPO"

print_summary
