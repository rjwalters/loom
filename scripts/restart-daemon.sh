#!/usr/bin/env bash
# Restart the daemon

set -e

echo "Restarting daemon..."

# Stop daemon
./scripts/stop-daemon.sh

# Wait a moment for cleanup
sleep 1

# Start daemon
./scripts/start-daemon.sh
