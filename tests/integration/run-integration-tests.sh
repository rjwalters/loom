#!/usr/bin/env bash
# Run Gitea integration tests end-to-end.
#
# Usage:
#   ./tests/integration/run-integration-tests.sh          # Full setup + test
#   ./tests/integration/run-integration-tests.sh --skip-setup  # Tests only (Gitea already running)
#   ./tests/integration/run-integration-tests.sh --teardown    # Stop Gitea after tests
#
# This script:
#   1. Starts the Gitea Docker container
#   2. Runs the bootstrap script
#   3. Executes the integration test suite
#   4. Optionally tears down the container

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
SKIP_SETUP=false
TEARDOWN=false

for arg in "$@"; do
    case "$arg" in
        --skip-setup) SKIP_SETUP=true ;;
        --teardown)   TEARDOWN=true ;;
        --help|-h)
            echo "Usage: $0 [--skip-setup] [--teardown]"
            echo ""
            echo "  --skip-setup  Skip Docker and bootstrap (Gitea already running)"
            echo "  --teardown    Stop and remove Gitea container after tests"
            exit 0
            ;;
    esac
done

if [ "$SKIP_SETUP" = false ]; then
    echo "=== Starting Gitea ==="
    docker compose -f "$SCRIPT_DIR/docker-compose.yml" up -d --wait
    echo ""

    echo "=== Bootstrapping Gitea ==="
    "$SCRIPT_DIR/setup-gitea.sh"
    echo ""
fi

# Load token
TOKEN_FILE="$SCRIPT_DIR/.gitea-token"
if [ ! -f "$TOKEN_FILE" ]; then
    echo "ERROR: Token file not found at $TOKEN_FILE" >&2
    echo "Run setup-gitea.sh first or pass GITEA_TOKEN env var." >&2
    exit 1
fi

export GITEA_URL="${GITEA_URL:-http://localhost:3000}"
export GITEA_TOKEN="${GITEA_TOKEN:-$(cat "$TOKEN_FILE")}"
export GITEA_REPO="${GITEA_REPO:-loom-test/test-repo}"

echo "=== Running Integration Tests ==="
cd "$REPO_ROOT/loom-tools"
python -m pytest tests/integration/ -v --tb=short "$@"
EXIT_CODE=$?

if [ "$TEARDOWN" = true ]; then
    echo ""
    echo "=== Tearing down Gitea ==="
    docker compose -f "$SCRIPT_DIR/docker-compose.yml" down -v
fi

exit $EXIT_CODE
