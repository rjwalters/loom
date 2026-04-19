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

# Determine the Gitea container name
CONTAINER_NAME=""
for candidate in integration-gitea-1 gitea gitea-1; do
    if docker ps --format '{{.Names}}' | grep -q "^${candidate}$"; then
        CONTAINER_NAME="$candidate"
        break
    fi
done

if [ -z "$CONTAINER_NAME" ]; then
    echo "ERROR: No running Gitea container found. Run 'docker compose up -d' first." >&2
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

# Create admin user via gitea CLI inside the container
echo "Creating admin user..."
docker exec "$CONTAINER_NAME" gitea admin user create \
    --username "$ADMIN_USER" \
    --password "$ADMIN_PASS" \
    --email "$ADMIN_EMAIL" \
    --admin \
    --must-change-password=false 2>/dev/null || echo "(user may already exist)"

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
