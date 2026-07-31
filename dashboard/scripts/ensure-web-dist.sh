#!/usr/bin/env bash
# =============================================================================
# ensure-web-dist.sh — guarantee `web/dist/` exists before Wrangler parses
# `wrangler.toml`.
#
# The Worker serves the Phase-3 dashboard UI as Workers Assets
# (`[assets] directory = "./web/dist"`), which makes that directory part of the
# *configuration*, not just the build output: Wrangler hard-errors with
# "The directory specified by the assets.directory field ... does not exist"
# before it will parse anything else. That bites three commands which have
# nothing to do with the UI — `npm test` (the Miniflare pool reads the same
# config), `npm run preflight`, and `wrangler dev` — on any checkout where the
# UI has not been built yet, including a fresh clone.
#
# So: create the directory with an honest placeholder when there is no real
# build. The placeholder is deliberately self-describing rather than empty, so
# that if it ever *is* deployed the page says what happened instead of
# 404-ing mysteriously. `npm run deploy` always rebuilds first, so a real
# deploy never ships it.
#
# Building the UI for real is `npm run build:web` (or `npm --prefix web run
# build`), which overwrites everything here.
#
# Usage: bash scripts/ensure-web-dist.sh
# Exit codes: 0 always (this can only fail on an unwritable checkout).
# =============================================================================
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
APP_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
DIST_DIR="$APP_DIR/web/dist"
PLACEHOLDER_MARKER="loom-dashboard-placeholder"

# A real build always emits hashed asset files under `assets/`; the placeholder
# never does. That is the difference the preflight check keys off.
if [ -f "$DIST_DIR/index.html" ] && ! grep -q "$PLACEHOLDER_MARKER" "$DIST_DIR/index.html" 2>/dev/null; then
  exit 0
fi

mkdir -p "$DIST_DIR"
cat >"$DIST_DIR/index.html" <<HTML
<!doctype html>
<html lang="en">
  <head>
    <meta charset="utf-8" />
    <meta name="$PLACEHOLDER_MARKER" content="true" />
    <title>Loom fleet — not built</title>
  </head>
  <body>
    <h1>Dashboard UI not built</h1>
    <p>
      This is a placeholder written by <code>dashboard/scripts/ensure-web-dist.sh</code>
      so Wrangler can parse <code>wrangler.toml</code>. Build the real UI with:
    </p>
    <pre>cd dashboard/web &amp;&amp; npm install &amp;&amp; npm run build</pre>
    <p>The API routes (<code>/api/*</code>, <code>/public/*</code>) are unaffected.</p>
  </body>
</html>
HTML
