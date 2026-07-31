#!/usr/bin/env bash
# =============================================================================
# check-deploy-config.sh — pre-deploy configuration check for the Loom fleet
# observability Workers backend (epic #4702, Phase 2).
#
# `wrangler.toml` doubles as a template: it ships with obviously-fake
# placeholders a deployer must replace with values from their own Cloudflare
# account. This script is what stops a half-configured template from reaching
# `wrangler deploy` — it fails on any surviving placeholder and warns about
# the configuration mistakes that are silent but dangerous (chiefly: leaving
# workers.dev enabled next to an Access-gated custom domain, which is a
# wide-open bypass around the Access policy).
#
# Usage:
#   npm run preflight                  # config checks + bundle dry run
#   bash scripts/check-deploy-config.sh --skip-bundle
#   bash scripts/check-deploy-config.sh --remote      # also checks secrets
#
# Exit codes: 0 = ready to deploy, 1 = at least one blocking error.
# Full walkthrough: docs/deploy-runbook.md
# =============================================================================
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
APP_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
CONFIG="${LOOM_DASHBOARD_WRANGLER_CONFIG:-$APP_DIR/wrangler.toml}"

SKIP_BUNDLE=0
CHECK_REMOTE=0
WRANGLER_ENV=""

while [ $# -gt 0 ]; do
  case "$1" in
    --skip-bundle) SKIP_BUNDLE=1 ;;
    --remote) CHECK_REMOTE=1 ;;
    --env)
      shift
      WRANGLER_ENV="${1:-}"
      [ -n "$WRANGLER_ENV" ] || { echo "--env requires a value" >&2; exit 2; }
      ;;
    -h | --help)
      # The file header, up to the first line of actual code.
      awk 'NR > 1 { if (/^#/) print; else exit }' "${BASH_SOURCE[0]}"
      exit 0
      ;;
    *)
      echo "unknown argument: $1 (try --help)" >&2
      exit 2
      ;;
  esac
  shift
done

errors=0
warnings=0
fail() {
  printf 'ERROR  %s\n' "$1" >&2
  errors=$((errors + 1))
}
warn() {
  printf 'WARN   %s\n' "$1" >&2
  warnings=$((warnings + 1))
}
pass() { printf 'ok     %s\n' "$1"; }

# ---------------------------------------------------------------------------
# Config file + "active" (non-comment) lines
# ---------------------------------------------------------------------------
if [ ! -f "$CONFIG" ]; then
  fail "no wrangler config at $CONFIG"
  exit 1
fi
pass "config file: $CONFIG"

# Only uncommented lines are live configuration. Everything the template
# leaves commented out (the account_id hint, the [[routes]] example, the
# [env.staging] block) is documentation, not a misconfiguration.
ACTIVE="$(grep -v '^[[:space:]]*#' "$CONFIG" | grep -v '^[[:space:]]*$' || true)"

# ---------------------------------------------------------------------------
# 1. No surviving template placeholders
# ---------------------------------------------------------------------------
if printf '%s\n' "$ACTIVE" | grep -q 'REPLACE_WITH_'; then
  printf '%s\n' "$ACTIVE" | grep -n 'REPLACE_WITH_' >&2 || true
  fail "template placeholder(s) above are still in $CONFIG — replace them with values from your own account"
else
  pass "no REPLACE_WITH_* placeholders remain"
fi

# ---------------------------------------------------------------------------
# 2. D1 database id was filled in
# ---------------------------------------------------------------------------
db_id="$(printf '%s\n' "$ACTIVE" |
  grep -E '^[[:space:]]*database_id[[:space:]]*=' |
  head -1 |
  sed -E 's/.*=[[:space:]]*"?([^"]*)"?.*/\1/')"
if [ -z "$db_id" ]; then
  fail "no database_id found in $CONFIG — run 'wrangler d1 create loom-observability' and paste the id"
elif [ "$db_id" = "00000000-0000-0000-0000-000000000000" ]; then
  fail "database_id is still the all-zeros placeholder — run 'wrangler d1 create loom-observability' and paste the id it prints"
else
  pass "database_id is set ($db_id)"
fi

# ---------------------------------------------------------------------------
# 3. Account id is resolvable (toml value, or the env var, or a single-account
#    credential Wrangler can infer from — the last case is only a notice)
# ---------------------------------------------------------------------------
if printf '%s\n' "$ACTIVE" | grep -qE '^[[:space:]]*account_id[[:space:]]*='; then
  pass "account_id is set in $CONFIG"
elif [ -n "${CLOUDFLARE_ACCOUNT_ID:-}" ]; then
  pass "account_id supplied via \$CLOUDFLARE_ACCOUNT_ID"
else
  warn "no account_id in $CONFIG and \$CLOUDFLARE_ACCOUNT_ID is unset — fine if your credential has exactly one account, otherwise wrangler will refuse to deploy ('wrangler whoami' lists them)"
fi

# ---------------------------------------------------------------------------
# 4. Custom domain / workers.dev interaction (the Access-bypass trap)
# ---------------------------------------------------------------------------
has_route=0
if printf '%s\n' "$ACTIVE" | grep -qE '^[[:space:]]*pattern[[:space:]]*='; then
  has_route=1
fi
if [ "$has_route" -eq 1 ] && printf '%s\n' "$ACTIVE" | grep -qE 'pattern[[:space:]]*=[[:space:]]*"[^"]*example\.com"'; then
  fail "route pattern still points at example.com — set your own hostname"
fi

workers_dev="$(printf '%s\n' "$ACTIVE" |
  grep -E '^[[:space:]]*workers_dev[[:space:]]*=' |
  head -1 |
  sed -E 's/.*=[[:space:]]*([a-z]+).*/\1/')"
if [ "$has_route" -eq 1 ] && [ "$workers_dev" != "false" ]; then
  warn "a custom domain route is configured but workers_dev is not false — the *.workers.dev URL stays publicly reachable and bypasses any Cloudflare Access policy on the custom domain (docs/cloudflare-access.md)"
elif [ "$has_route" -eq 1 ]; then
  pass "custom domain route configured with workers_dev disabled"
else
  pass "workers.dev-only deployment (no custom domain route yet — required before Cloudflare Access)"
fi

# ---------------------------------------------------------------------------
# 5. Source + migrations present
# ---------------------------------------------------------------------------
main_rel="$(printf '%s\n' "$ACTIVE" |
  grep -E '^[[:space:]]*main[[:space:]]*=' |
  head -1 |
  sed -E 's/.*=[[:space:]]*"([^"]*)".*/\1/')"
if [ -n "$main_rel" ] && [ -f "$APP_DIR/$main_rel" ]; then
  pass "entrypoint exists ($main_rel)"
else
  fail "entrypoint '$main_rel' declared in $CONFIG does not exist"
fi

migrations_rel="$(printf '%s\n' "$ACTIVE" |
  grep -E '^[[:space:]]*migrations_dir[[:space:]]*=' |
  head -1 |
  sed -E 's/.*=[[:space:]]*"([^"]*)".*/\1/')"
migrations_rel="${migrations_rel:-migrations}"
migration_count=0
if [ -d "$APP_DIR/$migrations_rel" ]; then
  migration_count="$(find "$APP_DIR/$migrations_rel" -maxdepth 1 -name '*.sql' | wc -l | tr -d ' ')"
fi
if [ "$migration_count" -gt 0 ]; then
  pass "$migration_count migration(s) in $migrations_rel/ (apply with 'wrangler d1 migrations apply <db> --remote')"
else
  fail "no .sql migrations found in $APP_DIR/$migrations_rel"
fi

# ---------------------------------------------------------------------------
# 6. Wrangler availability + bundle dry run
# ---------------------------------------------------------------------------
WRANGLER=""
if [ -x "$APP_DIR/node_modules/.bin/wrangler" ]; then
  WRANGLER="$APP_DIR/node_modules/.bin/wrangler"
elif command -v wrangler >/dev/null 2>&1; then
  WRANGLER="$(command -v wrangler)"
fi

if [ -z "$WRANGLER" ]; then
  warn "wrangler not found (run 'npm install' in $APP_DIR) — skipping bundle and remote checks"
  SKIP_BUNDLE=1
  CHECK_REMOTE=0
else
  pass "wrangler: $WRANGLER"
fi

env_args=""
[ -n "$WRANGLER_ENV" ] && env_args="--env $WRANGLER_ENV"

if [ "$SKIP_BUNDLE" -eq 0 ]; then
  outdir="$(mktemp -d)"
  # shellcheck disable=SC2086
  if bundle_output="$(cd "$APP_DIR" && "$WRANGLER" deploy --dry-run -c "$CONFIG" --outdir "$outdir" $env_args 2>&1)"; then
    pass "wrangler deploy --dry-run succeeded (config parses, Worker bundles)"
  else
    printf '%s\n' "$bundle_output" >&2
    fail "wrangler deploy --dry-run failed (see output above)"
  fi
  rm -rf "$outdir"
fi

# ---------------------------------------------------------------------------
# 7. Optional remote check: is the ADMIN_TOKEN secret set on the deployed
#    Worker? Without it every /admin/* route answers 503 and no host can be
#    provisioned. Requires a working Cloudflare credential.
# ---------------------------------------------------------------------------
if [ "$CHECK_REMOTE" -eq 1 ]; then
  # shellcheck disable=SC2086
  if secret_output="$(cd "$APP_DIR" && "$WRANGLER" secret list -c "$CONFIG" $env_args 2>&1)"; then
    if printf '%s\n' "$secret_output" | grep -q 'ADMIN_TOKEN'; then
      pass "ADMIN_TOKEN secret is set on the deployed Worker"
    else
      fail "ADMIN_TOKEN secret is NOT set — run 'wrangler secret put ADMIN_TOKEN${env_args:+ $env_args}' (until then every /admin/* route answers 503; an empty list also means the Worker is not deployed yet)"
    fi
  else
    warn "could not list secrets (not deployed yet, or no Cloudflare credential): $(printf '%s' "$secret_output" | tail -1)"
  fi
fi

# ---------------------------------------------------------------------------
echo
if [ "$errors" -gt 0 ]; then
  printf 'FAILED: %d error(s), %d warning(s)\n' "$errors" "$warnings" >&2
  exit 1
fi
printf 'PASSED: 0 errors, %d warning(s)\n' "$warnings"
