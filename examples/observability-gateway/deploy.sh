#!/usr/bin/env bash
# Deploy the fleet gateway collector config (infra/observability/gateway) to
# this host's private runtime location and reconcile the Compose project.
#
# The private location (~/.loom/observability/cloud/gateway/) is what the
# com.loom.observability.collector LaunchAgent reconciles; per machine/README
# policy, machine-generated config and secrets stay outside Git. This script
# is the one-way sync: repo (source of truth) -> private copy -> apply.
#
# Usage: infra/observability/gateway/deploy.sh [--diff]
#
# Required environment (reads the private gateway.env automatically):
#   every key documented in gateway.env.example
set -euo pipefail

SCRIPT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
GATEWAY_DIR="${LOOM_GATEWAY_DIR:-$HOME/.loom/observability/cloud/gateway}"
ENV_FILE="$GATEWAY_DIR/gateway.env"
PROJECT=loom-cloud-observability
DIFF_ONLY=0
[[ ${1:-} == "--diff" ]] && DIFF_ONLY=1

for f in config.yaml compose.yaml gateway.env.example; do
  [[ -f "$SCRIPT_DIR/$f" ]] || { echo "missing $f next to this script" >&2; exit 1; }
done
[[ -f "$ENV_FILE" ]] || { echo "no $ENV_FILE — create it from gateway.env.example first" >&2; exit 1; }
set -a; source "$ENV_FILE"; set +a
for v in LOOM_HOST_ID LOOM_CODEX_SESSIONS_DIR LOOM_PI_SESSIONS_DIR LOOM_CLAUDE_PROJECTS_DIR \
         LOOM_COLLECTOR_UID LOOM_COLLECTOR_GID LOOM_COLLECTOR_STATE_DIR \
         LOOM_COLLECTOR_INGEST_KEY_FILE LOOM_SIGNOZ_INGEST_KEY_FILE LOOM_CLICKHOUSE_PASSWORD_FILE \
         LOOM_SIGNOZ_OTLP_ENDPOINT LOOM_CLICKHOUSE_ENDPOINT LOOM_CLICKHOUSE_USERNAME; do
  [[ -n ${!v:-} ]] || { echo "gateway.env is missing $v" >&2; exit 1; }
done

if (( DIFF_ONLY )); then
  diff -u "$GATEWAY_DIR/config.yaml" "$SCRIPT_DIR/config.yaml" || true
  diff -u "$GATEWAY_DIR/compose.yaml" "$SCRIPT_DIR/compose.yaml" || true
  exit 0
fi

STAMP=$(date -u +%Y%m%dT%H%M%SZ)
BACKUP="$GATEWAY_DIR/../gateway-before-deploy-$STAMP"
mkdir -p "$BACKUP"
cp "$GATEWAY_DIR/config.yaml" "$GATEWAY_DIR/compose.yaml" "$BACKUP/" 2>/dev/null || true

install -m 600 "$SCRIPT_DIR/config.yaml" "$GATEWAY_DIR/config.yaml"
install -m 600 "$SCRIPT_DIR/compose.yaml" "$GATEWAY_DIR/compose.yaml"

# Same invocation the LaunchAgent uses, so launchd's reconcile keeps working.
DOCKER=$(command -v docker || echo /usr/local/bin/docker)
exec "$DOCKER" compose --project-name "$PROJECT" --env-file "$ENV_FILE" \
  --file "$GATEWAY_DIR/compose.yaml" up -d
