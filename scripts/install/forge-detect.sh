#!/usr/bin/env bash
# Detect forge type (GitHub or Gitea) from git remote URL
#
# Usage: source this file, then call detect_forge_and_repo
#
# Sets the following variables:
#   FORGE_TYPE    - "github" or "gitea"
#   FORGE_OWNER   - repository owner
#   FORGE_REPO    - repository name
#   FORGE_API_BASE - base URL for API calls (Gitea only)
#   FORGE_TOKEN   - auth token for API calls (Gitea only)
#
# For Gitea, expects either $GITEA_TOKEN or $FORGE_TOKEN to be set.
# The base URL is auto-detected from the remote URL.

# Detect forge type and extract owner/repo from a git remote URL.
#
# Arguments:
#   $1 - git remote URL (required)
#
# Returns 0 on success, 1 on failure.
detect_forge_and_repo() {
  local origin_url="${1:-}"

  if [[ -z "$origin_url" ]]; then
    echo "ERROR: origin URL is required" >&2
    return 1
  fi

  FORGE_TYPE=""
  FORGE_OWNER=""
  FORGE_REPO=""
  FORGE_API_BASE=""
  FORGE_TOKEN=""

  # Detect forge type from URL
  if [[ "$origin_url" =~ github\.com ]]; then
    FORGE_TYPE="github"
  else
    # For any non-GitHub URL, try to detect if it's a Gitea instance.
    # We extract the host and probe the Gitea API endpoint.
    local host=""
    host=$(_extract_host "$origin_url")

    if [[ -n "$host" ]]; then
      # Check if this host responds to the Gitea API
      local api_base="https://${host}/api/v1"
      local token="${GITEA_TOKEN:-${FORGE_TOKEN:-}}"

      if _probe_gitea_api "$api_base" "$token"; then
        FORGE_TYPE="gitea"
        FORGE_API_BASE="$api_base"
        FORGE_TOKEN="$token"
      else
        # Try HTTP as fallback (some self-hosted instances)
        api_base="http://${host}/api/v1"
        if _probe_gitea_api "$api_base" "$token"; then
          FORGE_TYPE="gitea"
          FORGE_API_BASE="$api_base"
          FORGE_TOKEN="$token"
        fi
      fi
    fi

    if [[ -z "$FORGE_TYPE" ]]; then
      echo "ERROR: Could not detect forge type from URL: $origin_url" >&2
      echo "For Gitea instances, ensure the server is reachable." >&2
      return 1
    fi
  fi

  # Extract owner/repo from URL (handles HTTPS and SSH for any host)
  # HTTPS: https://host/owner/repo.git -> owner/repo
  # SSH: git@host:owner/repo.git -> owner/repo
  local repo_path=""
  # Strip .git suffix first, then extract owner/repo
  local cleaned_url
  cleaned_url=$(echo "$origin_url" | sed -E 's/\.git$//')
  repo_path=$(echo "$cleaned_url" | sed -E 's#^.*[:/]([^/]+/[^/]+)$#\1#')

  if [[ ! "$repo_path" =~ ^[^/]+/[^/]+$ ]]; then
    echo "ERROR: Could not extract valid owner/repo from URL: $origin_url" >&2
    return 1
  fi

  FORGE_OWNER=$(echo "$repo_path" | cut -d'/' -f1)
  FORGE_REPO=$(echo "$repo_path" | cut -d'/' -f2)

  # For Gitea, validate that we have an auth token
  if [[ "$FORGE_TYPE" == "gitea" && -z "$FORGE_TOKEN" ]]; then
    echo "WARNING: No Gitea auth token found. Set GITEA_TOKEN or FORGE_TOKEN." >&2
  fi

  return 0
}

# Extract hostname (with optional port) from a git remote URL.
# Handles HTTPS URLs, SSH URLs, and SCP-style URLs.
_extract_host() {
  local url="$1"

  if [[ "$url" =~ ^https?:// ]]; then
    # HTTPS: https://host:port/owner/repo.git
    echo "$url" | sed -E 's#^https?://([^/]+)/.*#\1#'
  elif [[ "$url" =~ ^ssh:// ]]; then
    # SSH: ssh://git@host:port/owner/repo.git
    echo "$url" | sed -E 's#^ssh://[^@]*@([^/]+)/.*#\1#'
  elif [[ "$url" =~ ^git@ ]]; then
    # SCP-style: git@host:owner/repo.git
    echo "$url" | sed -E 's#^git@([^:]+):.*#\1#'
  else
    echo ""
  fi
}

# Probe whether a URL responds like a Gitea API.
# Returns 0 if it looks like Gitea, 1 otherwise.
_probe_gitea_api() {
  local api_base="$1"
  local token="$2"

  local auth_header=""
  if [[ -n "$token" ]]; then
    auth_header="Authorization: token $token"
  fi

  # Try the Gitea version endpoint — lightweight and always available
  local response=""
  if [[ -n "$auth_header" ]]; then
    response=$(curl -s -m 5 -H "$auth_header" "${api_base}/version" 2>/dev/null || echo "")
  else
    response=$(curl -s -m 5 "${api_base}/version" 2>/dev/null || echo "")
  fi

  # Gitea returns {"version":"X.Y.Z"} from this endpoint
  if echo "$response" | grep -q '"version"'; then
    return 0
  fi

  return 1
}

# Make a Gitea API call using curl.
#
# Arguments:
#   $1 - HTTP method (GET, POST, PATCH, PUT, DELETE)
#   $2 - API path (e.g., /repos/owner/repo)
#   $3 - JSON body (optional, for POST/PATCH/PUT)
#
# Requires FORGE_API_BASE and FORGE_TOKEN to be set.
# Outputs the response body on stdout.
# Returns the curl exit code.
gitea_api() {
  local method="$1"
  local path="$2"
  local body="${3:-}"

  local url="${FORGE_API_BASE}${path}"
  local -a curl_args=(
    -s
    -X "$method"
    -H "Content-Type: application/json"
    -w "\n%{http_code}"
  )

  if [[ -n "$FORGE_TOKEN" ]]; then
    curl_args+=(-H "Authorization: token $FORGE_TOKEN")
  fi

  if [[ -n "$body" ]]; then
    curl_args+=(-d "$body")
  fi

  curl "${curl_args[@]}" "$url"
}

# Detect which merge strategy the target repository actually allows (#7844).
#
# Mirrors defaults/scripts/lib/forge-merge-method.sh's forge_detect_merge_method
# (#7754), but for the installer's pre-binary bootstrap call path: it reuses
# THIS file's FORGE_TYPE / FORGE_OWNER / FORGE_REPO / gitea_api() state instead
# of forge-helpers.sh's. Sourcing that module here would clobber the forge
# state detect_forge_and_repo already established (forge-helpers.sh resets
# FORGE_TYPE and friends at source time), so the probe is reimplemented against
# the state this file already owns rather than shared.
#
# Usage: detect_merge_method
# Outputs on stdout: "squash", "merge", or "rebase" — never anything else.
#
# Preference order when more than one strategy is allowed: squash > merge >
# rebase. This matches forge-merge-method.sh and Loom's own installer default
# (setup-repository-settings.sh) purely as a tie-break — it is NOT a claim that
# squash is universally available.
#
# Fails OPEN to "squash" (the historical hardcoded behavior) on a network/auth
# error, an unparseable response, or a forge reporting every allow_* flag
# false. A transient probe failure therefore never blocks a merge outright —
# worst case it reproduces the pre-#7844 behavior for that one call.
detect_merge_method() {
  local allow_squash="" allow_merge="" allow_rebase=""

  if [[ "${FORGE_TYPE:-}" == "gitea" ]]; then
    local response body code parsed
    response=$(gitea_api GET "/repos/${FORGE_OWNER}/${FORGE_REPO}" 2>/dev/null) || {
      echo "squash"
      return 0
    }
    code=$(printf '%s\n' "$response" | tail -1)
    body=$(printf '%s\n' "$response" | sed '$d')

    if [[ "$code" != "200" ]]; then
      echo "squash"
      return 0
    fi

    # Gitea spells the merge-commit flag `allow_merge_commits` (plural).
    parsed=$(printf '%s' "$body" | python3 -c '
import json, sys
try:
    data = json.load(sys.stdin)
except Exception:
    sys.exit(1)
print("%s %s %s" % (
    str(data.get("allow_squash_merge", "")).lower(),
    str(data.get("allow_merge_commits", "")).lower(),
    str(data.get("allow_rebase_merge", "")).lower(),
))
' 2>/dev/null) || {
      echo "squash"
      return 0
    }
    read -r allow_squash allow_merge allow_rebase <<< "$parsed"
  else
    local tsv
    # GitHub spells the merge-commit flag `allow_merge_commit` (singular).
    # `gh api --jq` uses gh's built-in jq engine, so this adds no `jq`
    # dependency to the GitHub install path.
    tsv=$(gh api "repos/${FORGE_OWNER}/${FORGE_REPO}" \
      --jq '[(.allow_squash_merge // false), (.allow_merge_commit // false), (.allow_rebase_merge // false)] | @tsv' \
      2>/dev/null) || {
      echo "squash"
      return 0
    }
    IFS=$'\t' read -r allow_squash allow_merge allow_rebase <<< "$tsv"
  fi

  if [[ "$allow_squash" == "true" ]]; then
    echo "squash"
  elif [[ "$allow_merge" == "true" ]]; then
    echo "merge"
  elif [[ "$allow_rebase" == "true" ]]; then
    echo "rebase"
  else
    echo "squash"
  fi
}
