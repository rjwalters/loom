#!/usr/bin/env bash
# Setup branch protection for Loom workflow
#
# Supports both GitHub (rulesets API) and Gitea (branch protection API).
#
# Usage:
#   ./scripts/install/setup-branch-protection.sh /path/to/target-repo [branch-name]
#
# Environment:
#   LOOM_NON_INTERACTIVE=true  Skip prompts; on overlap conflict, default to
#                              "skip" (preserves existing protection, avoids
#                              creating duplicate rulesets — issue #3216).
#   LOOM_DRY_RUN=true          Print the exact ruleset payload that WOULD be
#                              applied and exit without mutating anything
#                              (#8103). Read-only lookups still happen, so the
#                              preview can show the live bypass actors an
#                              in-place update would preserve (#8239).
#   LOOM_REQUIRED_STATUS_CHECKS
#                              Comma- or newline-separated check-run names to
#                              require. Overrides the target repo's
#                              .loom/config.json -> branchProtection
#                              .requiredStatusChecks (#8103).
#   LOOM_REQUIRED_STATUS_CHECKS_STRICT=true
#                              Also require branches to be up to date before
#                              merging. Overrides branchProtection
#                              .strictRequiredStatusChecks; default false —
#                              see the rationale at the rule itself.
#
# For GitHub, creates or updates a ruleset with recommended rules:
#   - Prevent branch deletion and force pushes
#   - Require linear history (squash merges only)
#   - Require pull requests (0 approvals for solo dev/Loom workflows)
#   - Require status checks, when the target repo names any (#8103)
#
# For Gitea, creates or updates branch protection with equivalent settings:
#   - Prevent force pushes
#   - Require pull requests (0 approvals)
#   - Dismiss stale approvals
#   - Warns about features without Gitea equivalents (linear history, role-based bypass)

set -euo pipefail

# Source helper functions
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

# Define helper functions for consistent output
info() { echo -e "\033[0;34mℹ $*\033[0m"; }
success() { echo -e "\033[0;32m✓ $*\033[0m"; }
warning() { echo -e "\033[1;33m⚠ $*\033[0m"; }
error() { echo -e "\033[0;31m✗ $*\033[0m"; }

# Source forge detection helper
source "${SCRIPT_DIR}/forge-detect.sh"

# --- GitHub branch protection (rulesets API) ---

# Enumerate rulesets that target the default branch via cross-name overlap.
# Sets array OVERLAPPING_RULESETS to "id|name|enforcement" lines for any
# active/evaluate ruleset whose conditions include ~DEFAULT_BRANCH,
# refs/heads/<branch>, or refs/heads/*, AND whose name does not match
# RULESET_NAME (the same-named case is handled by the in-place update path).
# Disabled rulesets are ignored (they cannot conflict at runtime).
detect_overlapping_rulesets() {
  local owner="$1"
  local repo="$2"
  local our_name="$3"
  local branch="$4"

  OVERLAPPING_RULESETS=()

  local rulesets_json
  rulesets_json=$(gh api "repos/${owner}/${repo}/rulesets" 2>/dev/null || echo "[]")

  while IFS='|' read -r rs_id rs_name rs_enforcement; do
    [[ -z "$rs_id" ]] && continue
    # Skip our own same-named ruleset; handled by existing in-place update path
    if [[ "$rs_name" == "$our_name" ]]; then
      continue
    fi
    # Ignore disabled rulesets (inactive, can't conflict at runtime)
    if [[ "$rs_enforcement" == "disabled" ]]; then
      continue
    fi

    # Fetch detail to inspect ref_name conditions
    local detail
    detail=$(gh api "repos/${owner}/${repo}/rulesets/${rs_id}" 2>/dev/null || echo "{}")

    local includes
    includes=$(echo "$detail" | jq -r '.conditions.ref_name.include // [] | .[]' 2>/dev/null || echo "")

    # Match ~DEFAULT_BRANCH token, refs/heads/<branch>, or wildcard refs/heads/*
    if echo "$includes" | grep -qE "^(~DEFAULT_BRANCH|refs/heads/${branch}|refs/heads/\*)$"; then
      OVERLAPPING_RULESETS+=("${rs_id}|${rs_name}|${rs_enforcement}")
    fi
  done < <(echo "$rulesets_json" | jq -r '.[] | select(.target == "branch") | "\(.id)|\(.name)|\(.enforcement)"' 2>/dev/null || true)
}

# Read the bypass_actors currently configured on a live ruleset.
# Prints a compact JSON array; "[]" when the ruleset cannot be read or reports
# no bypass actors. GitHub omits/nulls the field for a token that is not a repo
# admin, and such a token is also refused the ruleset PUT — so the preserve-on-
# update guarantee holds for every token that can actually perform the update,
# and a read-only token just previews the old admin-only default.
fetch_live_bypass_actors() {
  local owner="$1"
  local repo="$2"
  local rs_id="$3"

  local detail
  detail=$(gh api "repos/${owner}/${repo}/rulesets/${rs_id}" 2>/dev/null || echo "{}")
  echo "$detail" | jq -c '.bypass_actors // []' 2>/dev/null || echo "[]"
}

# Union a live ruleset's bypass_actors into the payload's bypass_actors.
#
# A ruleset PUT REPLACES bypass_actors wholesale, so sending the hardcoded
# admin-only list at an EXISTING ruleset silently deletes every other actor's
# bypass (issue #8239). On rjwalters/loom that would have dropped the
# `loom-fleet-dispatch` App, whose post-merge `git push origin HEAD:main` in
# .github/workflows/version-bump-on-merge.yml depends on bypassing the
# pull_request rule.
#
# Entries are keyed by (actor_id, actor_type). The LIVE entry wins on conflict,
# so a deliberately narrowed bypass_mode (e.g. "pull_request" for the admin
# role) is not silently widened back to "always" by an installer re-run; payload
# actors with no live counterpart are appended. A fresh POST never calls this —
# a brand new ruleset keeps the admin-only default.
merge_bypass_actors() {
  local payload="$1"
  local live_actors="$2"

  echo "$payload" | jq --argjson live "$live_actors" '
    .bypass_actors = (
      $live
      + [ (.bypass_actors // [])[]
          | . as $p
          | select(
              [ $live[]
                | select(.actor_id == $p.actor_id and .actor_type == $p.actor_type)
              ] | length == 0
            )
        ]
    )'
}

# Resolve the id of the ruleset that an apply would UPDATE in place, or print
# nothing when it would create a fresh one. Read-only.
#
# Used by the LOOM_DRY_RUN preview so the printed payload matches what would
# actually be sent (#8239) — the preview runs before the apply path's own
# lookups, so without this it can only ever show the hardcoded defaults.
#
# Preference mirrors the apply path: the same-named ruleset is the ordinary
# in-place update target, while a differently-named overlapping ruleset is only
# updated when the operator answers "u" at the overlap prompt, so it is the
# fallback.
resolve_update_target_id() {
  local owner="$1"
  local repo="$2"
  local our_name="$3"
  local branch="$4"

  local rulesets_json same_id
  rulesets_json=$(gh api "repos/${owner}/${repo}/rulesets" 2>/dev/null || echo "[]")
  same_id=$(echo "$rulesets_json" | jq -r --arg n "$our_name" \
    'if type == "array" then (map(select(.name == $n)) | .[0].id // "") else "" end' 2>/dev/null || echo "")

  if [[ -n "$same_id" && "$same_id" != "null" ]]; then
    printf '%s' "$same_id"
    return 0
  fi

  detect_overlapping_rulesets "$owner" "$repo" "$our_name" "$branch"
  if (( ${#OVERLAPPING_RULESETS[@]} > 0 )); then
    local first_id
    IFS='|' read -r first_id _ _ <<< "${OVERLAPPING_RULESETS[0]}"
    printf '%s' "$first_id"
  fi
}

setup_github_branch_protection() {
  local owner="$FORGE_OWNER"
  local repo="$FORGE_REPO"
  local ruleset_name="$RULESET_NAME"
  local branch="$BRANCH_NAME"

  # Check if user has admin permissions
  local has_admin
  has_admin=$(gh api "repos/${owner}/${repo}" --jq '.permissions.admin' 2>/dev/null || echo "false")
  if [[ "$has_admin" != "true" ]]; then
    warning "You may not have admin permissions to configure rulesets"
    warning "Attempting anyway (may fail with permission error)..."
  fi

  # Ruleset payload
  # bypass_actors: actor_id 5 = RepositoryRole/admin — allows repo admins to push
  # directly to main without a PR (e.g. for hotfixes or initial setup).
  # This admin-only list is the default for a FRESH ruleset. When an existing
  # ruleset is updated in place, merge_bypass_actors() unions the live actors in
  # first, because a ruleset PUT replaces the list wholesale (#8239).
  local ruleset_payload='{
    "name": "'"$ruleset_name"'",
    "target": "branch",
    "enforcement": "active",
    "bypass_actors": [
      {
        "actor_id": 5,
        "actor_type": "RepositoryRole",
        "bypass_mode": "always"
      }
    ],
    "conditions": {
      "ref_name": {
        "include": ["~DEFAULT_BRANCH"],
        "exclude": []
      }
    },
    "rules": [
      {"type": "deletion"},
      {"type": "non_fast_forward"},
      {"type": "required_linear_history"},
      {
        "type": "pull_request",
        "parameters": {
          "required_approving_review_count": 0,
          "dismiss_stale_reviews_on_push": true,
          "require_code_owner_review": false,
          "require_last_push_approval": false,
          "required_review_thread_resolution": false,
          "allowed_merge_methods": ["squash"]
        }
      }
    ]
  }'

  # --- Required status checks (#8103) ---------------------------------------
  #
  # Until #8103 this payload had no `required_status_checks` rule, so on every
  # repo it configured, CI was purely advisory: a red check could not block a
  # merge, and no rule could force a stale branch to re-run against the current
  # base before landing. The incident that made this concrete is rjwalters/loom
  # #8095 — a PR's only CI run measured a tree that `main` had since moved off,
  # the squash-merge tree was never measured by anything, and the first signal
  # was `main` going red after the fact.
  #
  # The contexts are NOT hardcoded here, because this script installs into any
  # repository and a required check that never reports blocks every merge in
  # that repo indefinitely. They come from the TARGET repo, in precedence order:
  #
  #   1. $LOOM_REQUIRED_STATUS_CHECKS — comma- or newline-separated context
  #      names (matches the env > config > default precedence Loom uses
  #      elsewhere).
  #   2. `.loom/config.json` -> `.branchProtection.requiredStatusChecks`, a JSON
  #      array of context names.
  #   3. Neither present -> no rule is emitted and behavior is exactly as before.
  #
  # A "context" is the check-run NAME GitHub posts to the Checks tab (a job's
  # `name:`, with the matrix values substituted), not the workflow job id.
  #
  # Only name checks that ALWAYS run. A path-filtered job (`needs: changes` and
  # friends) that is skipped never reports a conclusion, and a required check
  # with no conclusion blocks the merge forever.
  #
  # `strict_required_status_checks_policy` is GitHub's "require branches to be
  # up to date before merging". It defaults to FALSE here and is opt-in via
  # `.branchProtection.strictRequiredStatusChecks` /
  # $LOOM_REQUIRED_STATUS_CHECKS_STRICT. Reasoning: strict forces a
  # rebase-and-full-re-run immediately before every merge, so its cost scales
  # with merge rate. On a repo merging ~3.3 PRs/hour with CI runs longer than
  # the gap between merges (loom's own measured rate, `ci.yml`'s concurrency
  # comment), every merge would invalidate the branch its successors just
  # re-ran, and the queue serializes behind CI wall-clock. Non-strict still
  # blocks the case that actually motivates this — a PR landing with its OWN
  # required check red — which is the larger and cheaper half. Repos that merge
  # rarely, or whose ratchets compare absolute numbers against a moving base,
  # should turn it on deliberately.
  local rsc_contexts rsc_strict
  rsc_contexts="${LOOM_REQUIRED_STATUS_CHECKS:-$(jq -r '(.branchProtection.requiredStatusChecks // []) | join(",")' .loom/config.json 2>/dev/null || true)}"
  rsc_strict="${LOOM_REQUIRED_STATUS_CHECKS_STRICT:-$(jq -r '(.branchProtection.strictRequiredStatusChecks // false) | tostring' .loom/config.json 2>/dev/null || true)}"
  [[ "$rsc_strict" == "true" ]] || rsc_strict=false
  if [[ -n "${rsc_contexts//[[:space:],]/}" ]]; then
    ruleset_payload="$(printf '%s' "$ruleset_payload" | jq --arg c "$rsc_contexts" --argjson s "$rsc_strict" '.rules += [{"type": "required_status_checks", "parameters": {"strict_required_status_checks_policy": $s, "do_not_enforce_on_create": false, "required_status_checks": ($c | [splits("[,\n]+")] | map(gsub("^\\s+|\\s+$"; "")) | map(select(length > 0)) | map({"context": .}))}}]')"
  fi

  # Preview the exact payload without touching the repository. This is the
  # supported way to review a ruleset change before applying it to a live repo:
  # it makes no MUTATING API call, so it is also how the test suite asserts the
  # payload's shape.
  #
  # The preview must match what would actually be SENT. When an existing ruleset
  # would be updated in place, the payload sent carries that ruleset's live
  # bypass_actors merged in (#8239), so resolve the update target here — the
  # read-only lookups the apply path does further down have not run yet.
  if [[ "${LOOM_DRY_RUN:-false}" == "true" ]]; then
    local preview_id
    preview_id="$(resolve_update_target_id "$owner" "$repo" "$ruleset_name" "$branch")"
    if [[ -n "$preview_id" ]]; then
      info "Preview targets existing ruleset id=${preview_id} (its live bypass actors are preserved)"
      ruleset_payload="$(merge_bypass_actors "$ruleset_payload" "$(fetch_live_bypass_actors "$owner" "$repo" "$preview_id")")"
    fi
    printf '%s\n' "$ruleset_payload" | jq .
    return 0
  fi

  # Detect cross-name overlapping rulesets BEFORE the same-name update path,
  # so we don't silently POST a second ruleset overlapping a differently-named
  # pre-existing one. See issue #3216 for the bug this fixes.
  detect_overlapping_rulesets "$owner" "$repo" "$ruleset_name" "$branch"

  if (( ${#OVERLAPPING_RULESETS[@]} > 0 )); then
    warning "Found ${#OVERLAPPING_RULESETS[@]} existing ruleset(s) targeting the default branch:"
    local entry rs_id rs_name rs_enforcement
    for entry in "${OVERLAPPING_RULESETS[@]}"; do
      IFS='|' read -r rs_id rs_name rs_enforcement <<< "$entry"
      echo "    - id=${rs_id} name='${rs_name}' enforcement=${rs_enforcement}"
    done
    echo ""

    # Determine action: in non-interactive mode, default to Skip (safest).
    local action="skip"
    if [[ "${LOOM_NON_INTERACTIVE:-false}" != "true" ]]; then
      echo "How would you like to handle this?"
      echo "  [s] Skip    - keep existing ruleset(s), do not add Loom's (default, safest)"
      echo "  [r] Replace - delete the conflicting ruleset(s), then add Loom's"
      echo "  [u] Update  - update the first conflicting ruleset in-place with Loom's rules"
      echo ""
      local reply
      read -p "Choose [s/r/u] (default: s): " -n 1 -r reply
      echo ""
      case "$reply" in
        r|R) action="replace" ;;
        u|U) action="update" ;;
        *)   action="skip" ;;
      esac
    else
      info "Non-interactive mode: defaulting to 'skip' to avoid creating duplicate rulesets"
    fi

    case "$action" in
      skip)
        info "Skipping ruleset creation; existing protection is preserved."
        info "To replace later, re-run interactively or delete the existing ruleset first."
        return 0
        ;;
      replace)
        info "Replacing conflicting ruleset(s)..."
        for entry in "${OVERLAPPING_RULESETS[@]}"; do
          IFS='|' read -r rs_id rs_name rs_enforcement <<< "$entry"
          info "  Deleting ruleset id=${rs_id} name='${rs_name}'"
          if ! gh api --method DELETE "repos/${owner}/${repo}/rulesets/${rs_id}" > /dev/null 2>&1; then
            error "Failed to delete ruleset id=${rs_id}; aborting to avoid duplicates."
            return 1
          fi
        done
        # Fall through to standard create/update path below.
        ;;
      update)
        # Update the first conflicting ruleset in place with Loom's rules.
        # Preserves the existing ruleset's name/id; PUTs the rules onto it.
        IFS='|' read -r rs_id rs_name rs_enforcement <<< "${OVERLAPPING_RULESETS[0]}"
        info "Updating ruleset id=${rs_id} name='${rs_name}' in place with Loom rules..."
        local update_payload live_actors
        # A PUT replaces bypass_actors wholesale — carry the live ones over so
        # an update never revokes a bypass this script did not grant (#8239).
        live_actors="$(fetch_live_bypass_actors "$owner" "$repo" "$rs_id")"
        update_payload=$(merge_bypass_actors "$ruleset_payload" "$live_actors" | jq --arg n "$rs_name" '.name = $n')
        if echo "$update_payload" | gh api --method PUT "repos/${owner}/${repo}/rulesets/${rs_id}" --input - > /dev/null 2>&1; then
          success "Branch ruleset updated in place (id=${rs_id} name='${rs_name}')"
          echo ""
          info "To modify: GitHub Settings > Rules > Rulesets"
          return 0
        else
          error "Failed to update existing ruleset id=${rs_id}"
          return 1
        fi
        ;;
    esac
  fi

  # Check if a ruleset named "main" already exists (same-name in-place update).
  local existing_id
  existing_id=$(gh api "repos/${owner}/${repo}/rulesets" --jq '.[] | select(.name == "'"$ruleset_name"'") | .id' 2>/dev/null || echo "")

  local api_method api_url
  if [[ -n "$existing_id" ]]; then
    info "Found existing ruleset '${ruleset_name}' (ID: ${existing_id}), updating..."
    api_method="PUT"
    api_url="repos/${owner}/${repo}/rulesets/${existing_id}"
    # Same as the overlap "update" branch: the PUT replaces bypass_actors, so
    # merge the live ones in rather than dropping them (#8239).
    ruleset_payload="$(merge_bypass_actors "$ruleset_payload" "$(fetch_live_bypass_actors "$owner" "$repo" "$existing_id")")"
  else
    info "Creating new ruleset '${ruleset_name}'..."
    api_method="POST"
    api_url="repos/${owner}/${repo}/rulesets"
  fi

  if echo "$ruleset_payload" | gh api --method "$api_method" "$api_url" --input - > /dev/null 2>&1; then
    success "Branch ruleset configured successfully"
    echo ""
    echo "Applied rules:"
    echo "  - Prevent branch deletion"
    echo "  - Prevent force pushes"
    echo "  - Require linear history (squash merges only)"
    echo "  - Require pull requests (0 approvals required)"
    echo "  - Dismiss stale reviews on new commits"
    [[ -n "${rsc_contexts//[[:space:],]/}" ]] && echo "  - Required status checks (up-to-date branch required: ${rsc_strict}): ${rsc_contexts}"
    echo "  - Admin bypass: repository admins can push directly without a PR"
    echo ""
    echo "Note: 0 approvals required supports solo development and Loom's label-based review system."
    echo "Note: Admin bypass allows repo owners to push hotfixes directly to main when needed."
    echo ""
    info "To modify: GitHub Settings > Rules > Rulesets"
    return 0
  else
    error "Failed to configure branch ruleset"
    echo ""
    echo "This can happen if:"
    echo "  - You lack admin permissions on ${owner}/${repo}"
    echo "  - GitHub API is unreachable"
    echo ""
    info "To configure manually:"
    echo "  1. Go to: https://github.com/${owner}/${repo}/settings/rules"
    echo "  2. Create a new ruleset for the default branch"
    echo "  3. Enable: Prevent deletion, prevent force push"
    echo "  4. Enable: Require linear history"
    echo "  5. Enable: Require pull request (0 approvals)"
    return 1
  fi
}

# --- Gitea branch protection ---
setup_gitea_branch_protection() {
  local owner="$FORGE_OWNER"
  local repo="$FORGE_REPO"

  if [[ -z "$FORGE_TOKEN" ]]; then
    error "Gitea API token required. Set GITEA_TOKEN or FORGE_TOKEN environment variable."
    return 1
  fi

  # Check admin permissions via repo info
  local repo_response http_code
  repo_response=$(gitea_api GET "/repos/${owner}/${repo}")
  http_code=$(echo "$repo_response" | tail -1)

  if [[ "$http_code" != "200" ]]; then
    warning "Could not verify permissions on ${owner}/${repo} (HTTP ${http_code})"
    warning "Attempting branch protection setup anyway..."
  fi

  # Branch protection payload for Gitea
  local protection_payload
  protection_payload='{
    "branch_name": "'"${BRANCH_NAME}"'",
    "enable_push": true,
    "enable_force_push": false,
    "enable_force_push_allowlist": false,
    "dismiss_stale_approvals": true,
    "required_approvals": 0,
    "block_on_rejected_reviews": false,
    "block_admin_merge_override": false
  }'

  # Check if branch protection already exists.
  # Gitea's API is keyed by branch name (unlike GitHub's rulesets, which are
  # keyed by id and can have many overlapping rulesets per branch). So this
  # upsert by branch name fully covers conflict detection — no analogue of
  # the cross-name overlap bug from issue #3216 exists here.
  local existing_response existing_code
  existing_response=$(gitea_api GET "/repos/${owner}/${repo}/branch_protections/${BRANCH_NAME}")
  existing_code=$(echo "$existing_response" | tail -1)

  if [[ "$existing_code" == "200" ]]; then
    info "Found existing branch protection for '${BRANCH_NAME}', updating..."
    local update_response update_code
    update_response=$(gitea_api PATCH "/repos/${owner}/${repo}/branch_protections/${BRANCH_NAME}" "$protection_payload")
    update_code=$(echo "$update_response" | tail -1)

    if [[ "$update_code" == "200" ]]; then
      _gitea_protection_success
      return 0
    else
      _gitea_protection_failure "$owner" "$repo" "$update_code"
      return 1
    fi
  else
    info "Creating new branch protection for '${BRANCH_NAME}'..."
    local create_response create_code
    create_response=$(gitea_api POST "/repos/${owner}/${repo}/branch_protections" "$protection_payload")
    create_code=$(echo "$create_response" | tail -1)

    if [[ "$create_code" == "201" || "$create_code" == "200" ]]; then
      _gitea_protection_success
      return 0
    else
      _gitea_protection_failure "$owner" "$repo" "$create_code"
      return 1
    fi
  fi
}

_gitea_protection_success() {
  success "Branch protection configured successfully"
  echo ""
  echo "Applied rules:"
  echo "  - Allow push (not force push)"
  echo "  - Prevent force pushes"
  echo "  - Require pull requests (0 approvals required)"
  echo "  - Dismiss stale approvals on new commits"
  echo "  - Admin merge override: allowed"
  echo ""

  # Graceful degradation warnings
  warning "Gitea does not support 'required linear history' at the branch level."
  echo "  Mitigation: squash-only merging is enforced via repository settings."
  echo ""
  warning "Gitea does not support role-based bypass actors (admin bypass)."
  echo "  Mitigation: block_admin_merge_override is set to false, allowing admin overrides."
  echo ""
  warning "Gitea does not support per-branch merge method restrictions."
  echo "  Mitigation: merge methods are configured at the repository level."
  echo ""

  echo "Note: 0 approvals required supports solo development and Loom's label-based review system."
  echo ""
  info "To modify: Gitea Settings > Branches > Branch Protection"
}

_gitea_protection_failure() {
  local owner="$1"
  local repo="$2"
  local code="$3"

  error "Failed to configure branch protection (HTTP ${code})"
  echo ""
  echo "This can happen if:"
  echo "  - You lack admin permissions on ${owner}/${repo}"
  echo "  - Gitea API is unreachable"
  echo "  - The auth token is invalid or expired"
  echo ""
  info "To configure manually:"
  echo "  1. Go to your Gitea repository settings"
  echo "  2. Navigate to Branches > Branch Protection"
  echo "  3. Add protection for '${BRANCH_NAME}'"
  echo "  4. Disable force push"
  echo "  5. Set required approvals to 0"
  echo "  6. Enable 'Dismiss stale approvals'"
}

# ============================================================================
# Main
# ============================================================================

TARGET_PATH="${1:-}"
BRANCH_NAME="${2:-main}"
RULESET_NAME="main"

if [[ -z "$TARGET_PATH" ]]; then
  error "Target path required"
  echo "Usage: $0 /path/to/target-repo [branch-name]"
  exit 1
fi

cd "$TARGET_PATH"

# Detect the repository from origin remote
ORIGIN_URL=$(git config --get remote.origin.url 2>/dev/null || echo "")
if [[ -z "$ORIGIN_URL" ]]; then
  error "Failed to get repository information from git remote."
  exit 1
fi

# Detect forge type and extract owner/repo
if ! detect_forge_and_repo "$ORIGIN_URL"; then
  error "Could not detect forge type. Is this a GitHub or Gitea repository?"
  exit 1
fi

echo ""
info "Detected forge: ${FORGE_TYPE}"
info "Configuring branch protection for: ${FORGE_OWNER}/${FORGE_REPO} (${BRANCH_NAME})"

# Dispatch to the appropriate forge handler
if [[ "$FORGE_TYPE" == "github" ]]; then
  setup_github_branch_protection
elif [[ "$FORGE_TYPE" == "gitea" ]]; then
  setup_gitea_branch_protection
else
  error "Unsupported forge type: ${FORGE_TYPE}"
  exit 1
fi
