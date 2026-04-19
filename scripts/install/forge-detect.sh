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
