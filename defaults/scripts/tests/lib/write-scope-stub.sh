#!/usr/bin/env bash
# write-scope-stub.sh — test helper: let a suite's fixture repo pass the #9548
# write-scope check, so the suite keeps testing what it tests.
#
# Every Loom write path now vets its target with `loom_write_repo`
# (lib/forge-helpers.sh), which asks `loom-daemon forge may-write`. Suites use
# fixture repositories (`owner/repo`, `test-owner/test-repo`) that no real
# installation manages, and CI has no loom-daemon at all, so without this every
# fixture write is — correctly — refused. This helper is how a suite says "the
# write-scope decision is not what I am testing"; test-write-scope.sh is the
# suite that tests it.
#
#   source "$TEST_DIR/lib/write-scope-stub.sh"
#   write_scope_allow_all "$STUB_DIR"      # after the suite's own daemon setup
#
# It writes "$STUB_DIR/loom-daemon-write-scope" and exports LOOM_DAEMON_BIN at
# it. That wrapper answers `forge may-write [--repo R]` with R, or with what the
# (stubbed) `gh repo view` names when no repo is given, and hands every other
# invocation to the daemon the suite already resolved (LOOM_DAEMON_BIN, else
# `loom-daemon` on PATH, else exit 127 as if absent).

write_scope_allow_all() {
  local dir="$1" inner="${LOOM_DAEMON_BIN:-}"
  [[ -n "$inner" ]] || inner="$(command -v loom-daemon 2>/dev/null || true)"
  cat > "$dir/loom-daemon-write-scope" <<EOF
#!/usr/bin/env bash
if [[ "\${1:-} \${2:-}" == "forge may-write" ]]; then
  if [[ "\${3:-}" == "--repo" ]]; then echo "\$4"; exit 0; fi
  nwo="\$(gh repo view --json nameWithOwner --jq .nameWithOwner 2>/dev/null | sed -E 's/.*"nameWithOwner" *: *"([^"]+)".*/\\1/')"
  echo "\${nwo:-owner/repo}"; exit 0
fi
[[ -n "$inner" ]] || exit 127
exec "$inner" "\$@"
EOF
  chmod +x "$dir/loom-daemon-write-scope"
  export LOOM_DAEMON_BIN="$dir/loom-daemon-write-scope"
}
