#!/usr/bin/env bash
# Bootstrap a Gitea instance for integration testing.
#
# Prerequisites: Gitea container must be running on localhost:3000
# (use docker-compose.yml in this directory).
#
# Creates:
#   - Admin user "loom-test" with password "loom-test-password"
#   - API token written to stdout and tests/integration/.gitea-token
#   - Test repository "loom-test/test-repo" with initial commit
#   - All loom:* labels seeded from .github/labels.yml
#
# Usage:
#   ./tests/integration/setup-gitea.sh
#
# Environment outputs (for test consumption):
#   GITEA_URL=http://localhost:3000
#   GITEA_TOKEN=<token>
#   GITEA_REPO=loom-test/test-repo

set -euo pipefail

GITEA_URL="${GITEA_URL:-http://localhost:3000}"
ADMIN_USER="loom-test"
ADMIN_PASS="loom-test-password"
ADMIN_EMAIL="loom-test@example.com"
REPO_NAME="test-repo"
TOKEN_NAME="integration-test"
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
TOKEN_FILE="${SCRIPT_DIR}/.gitea-token"

# Determine the Gitea container name.
#
# Two environments are supported:
#   1. Local dev via docker-compose.yml -> container name like
#      `integration-gitea-1` (or `gitea` / `gitea-1` for older Compose).
#   2. GitHub Actions `services:` block -> container name is a runner-assigned
#      hash (e.g. `d39a49ea..._giteagitea122_f8b481`). The only stable handle
#      is the image `gitea/gitea:*`.
#
# Strategy: prefer the well-known compose names if present; otherwise fall back
# to a single container running the gitea/gitea image.
CONTAINER_NAME=""
for candidate in integration-gitea-1 gitea gitea-1; do
    if docker ps --format '{{.Names}}' | grep -q "^${candidate}$"; then
        CONTAINER_NAME="$candidate"
        break
    fi
done

if [ -z "$CONTAINER_NAME" ]; then
    # Fallback: look up by image (GitHub Actions services use hash names).
    # `docker ps --filter ancestor=...` matches the exact tag we know we use,
    # plus an untagged form for safety.
    matches=$(docker ps --filter 'ancestor=gitea/gitea:1.22' --format '{{.Names}}')
    if [ -z "$matches" ]; then
        matches=$(docker ps --format '{{.Names}}\t{{.Image}}' \
            | awk -F'\t' '$2 ~ /^gitea\/gitea(:|$)/ {print $1}')
    fi
    # Pick the first match (there should only be one in CI / dev).
    CONTAINER_NAME=$(echo "$matches" | head -n1)
fi

if [ -z "$CONTAINER_NAME" ]; then
    echo "ERROR: No running Gitea container found." >&2
    echo "  - Local dev: run 'docker compose up -d' in $(dirname "$0")" >&2
    echo "  - CI: ensure the workflow defines a 'gitea/gitea' service" >&2
    echo "Currently running containers:" >&2
    docker ps --format '  {{.Names}} ({{.Image}})' >&2 || true
    exit 1
fi

echo "Using Gitea container: $CONTAINER_NAME"

# Wait for Gitea to be healthy
echo "Waiting for Gitea to be ready..."
for i in $(seq 1 60); do
    if curl -sf "${GITEA_URL}/api/v1/version" >/dev/null 2>&1; then
        echo "Gitea is ready."
        break
    fi
    if [ "$i" -eq 60 ]; then
        echo "ERROR: Gitea did not become ready in time." >&2
        exit 1
    fi
    sleep 1
done

# Create admin user via gitea CLI inside the container.
#
# The official `gitea/gitea` image refuses to run the `gitea` binary as
# root (`setting.go:loadRunModeFrom: Gitea is not supposed to be run as
# root`). `docker exec` defaults to root, so we must explicitly use the
# `git` user that the entrypoint set up.
echo "Creating admin user..."
admin_create_output=$(docker exec -u git "$CONTAINER_NAME" gitea admin user create \
    --username "$ADMIN_USER" \
    --password "$ADMIN_PASS" \
    --email "$ADMIN_EMAIL" \
    --admin \
    --must-change-password=false 2>&1) || admin_create_rc=$?
admin_create_rc="${admin_create_rc:-0}"
if [ "$admin_create_rc" -ne 0 ]; then
    # Idempotency: a second run will fail with "user already exists"; that's
    # fine. Surface anything else.
    if echo "$admin_create_output" | grep -qiE 'already exists|user_already_exist'; then
        echo "(user already exists, continuing)"
    else
        echo "ERROR: failed to create admin user (rc=$admin_create_rc):" >&2
        echo "$admin_create_output" >&2
        exit 1
    fi
fi

# Generate API token
echo "Generating API token..."
TOKEN_RESPONSE=$(curl -sf -X POST "${GITEA_URL}/api/v1/users/${ADMIN_USER}/tokens" \
    -u "${ADMIN_USER}:${ADMIN_PASS}" \
    -H "Content-Type: application/json" \
    -d "{\"name\": \"${TOKEN_NAME}\", \"scopes\": [\"all\"]}" 2>/dev/null || true)

if [ -z "$TOKEN_RESPONSE" ]; then
    # Token may already exist; delete and recreate
    curl -sf -X DELETE "${GITEA_URL}/api/v1/users/${ADMIN_USER}/tokens/${TOKEN_NAME}" \
        -u "${ADMIN_USER}:${ADMIN_PASS}" 2>/dev/null || true
    TOKEN_RESPONSE=$(curl -sf -X POST "${GITEA_URL}/api/v1/users/${ADMIN_USER}/tokens" \
        -u "${ADMIN_USER}:${ADMIN_PASS}" \
        -H "Content-Type: application/json" \
        -d "{\"name\": \"${TOKEN_NAME}\", \"scopes\": [\"all\"]}")
fi

TOKEN=$(echo "$TOKEN_RESPONSE" | python3 -c "import sys,json; print(json.load(sys.stdin)['sha1'])")
echo "$TOKEN" > "$TOKEN_FILE"
echo "Token saved to $TOKEN_FILE"

# Create test repository
echo "Creating test repository..."
curl -sf -X POST "${GITEA_URL}/api/v1/user/repos" \
    -H "Authorization: token $TOKEN" \
    -H "Content-Type: application/json" \
    -d "{
        \"name\": \"${REPO_NAME}\",
        \"auto_init\": true,
        \"default_branch\": \"main\",
        \"description\": \"Loom integration test repository\"
    }" >/dev/null 2>&1 || echo "(repo may already exist)"

# Seed loom:* labels
echo "Seeding labels..."
LABELS_FILE="${SCRIPT_DIR}/../../.github/labels.yml"
if [ -f "$LABELS_FILE" ]; then
    # Parse YAML labels (simple grep-based extraction)
    grep "^- name:" "$LABELS_FILE" | sed 's/^- name: //' | while read -r LABEL_NAME; do
        # Get color from next line
        COLOR=$(grep -A2 "name: ${LABEL_NAME}$" "$LABELS_FILE" | grep "color:" | sed 's/.*color: "//;s/".*//' | head -1)
        COLOR="${COLOR:-0075ca}"
        curl -sf -X POST "${GITEA_URL}/api/v1/repos/${ADMIN_USER}/${REPO_NAME}/labels" \
            -H "Authorization: token $TOKEN" \
            -H "Content-Type: application/json" \
            -d "{\"name\": \"${LABEL_NAME}\", \"color\": \"#${COLOR}\"}" >/dev/null 2>&1 || true
    done
    echo "Labels seeded."
else
    echo "WARNING: labels.yml not found at $LABELS_FILE, creating minimal labels..."
    for label in "loom:issue" "loom:building" "loom:review-requested" "loom:pr" "loom:curated" "loom:changes-requested"; do
        curl -sf -X POST "${GITEA_URL}/api/v1/repos/${ADMIN_USER}/${REPO_NAME}/labels" \
            -H "Authorization: token $TOKEN" \
            -H "Content-Type: application/json" \
            -d "{\"name\": \"${label}\", \"color\": \"#0075ca\"}" >/dev/null 2>&1 || true
    done
    echo "Minimal labels created."
fi

echo ""
echo "=== Integration Test Environment Ready ==="
echo "GITEA_URL=${GITEA_URL}"
echo "GITEA_TOKEN=${TOKEN}"
echo "GITEA_REPO=${ADMIN_USER}/${REPO_NAME}"
echo ""
echo "To run tests:"
echo "  export GITEA_URL=${GITEA_URL}"
echo "  export GITEA_TOKEN=${TOKEN}"
echo "  cd loom-tools && pytest tests/integration/ -v"
