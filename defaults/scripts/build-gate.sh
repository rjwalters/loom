#!/usr/bin/env bash
# build-gate.sh - buildGate.command for this repo: fast build+test backstop
# across Rust, Python, and bash. Runs in the worktree; exits non-zero on the
# first failing stage (set -e) so buildGate.command's single exit code is
# meaningful. See .loom/docs/build-gate.md.
#
# Scope decisions (issue #3749):
#   - cargo test covers the Rust crates (loom-daemon, loom-api). As of #3985
#     the Rust step is scoped to `--lib --bins` (crate unit tests only) and
#     deliberately EXCLUDES the integration test TARGETS under
#     loom-daemon/tests/ (integration_basic.rs et al). Those spin up a real
#     tmux server and are therefore host-dependent — green only where a live
#     tmux is reachable. The local gate runs on the very host that is actively
#     running Loom (a busy, sometimes tmux-less machine), so a host-dependent
#     assertion there measures the host, not `main`. CI (.github/workflows/
#     ci.yml) controls its environment and still runs the FULL
#     `cargo test --workspace`, so the integration targets are covered exactly
#     where their environment is guaranteed. See "Local gate vs. CI" in
#     .loom/docs/build-gate.md.
#   - loom-tools pytest runs via `uv run` so the package is importable from
#     the project venv; scoped to exclude the live-network e2e suite
#     (tests/integration) and the known slow real-time bypass-poll integration
#     test file (already stubbed in-tree, but excluded here to keep the gate
#     fast and stable).
#   - bash scripts/test-installer.sh runs the 131-case installer suite.
#   - mcp-loom (TypeScript) is intentionally EXCLUDED: it needs npm install/ci
#     in a fresh worktree (no guaranteed warm node_modules), which adds
#     unpredictable latency to a gate that also runs once per PR. CI still
#     gates the mcp-loom build.
set -euo pipefail

# Run the whole gate at reduced CPU/IO priority (#3985) so it can never starve
# the sweeps it shares a host with. The gate historically ran the workspace
# suite at ~100% duty cycle; at nice 0 that saturation starved the timing-
# sensitive sweeps (and the gate's own timing tests). Re-exec once under `nice`
# (guarded by a sentinel so we don't loop), best-effort — if `nice` is absent
# we just proceed at normal priority. Honors LOOM_BUILD_GATE_NICENESS (default
# 19, the lowest priority) and can be disabled with LOOM_BUILD_GATE_NICE=0.
if [[ -z "${LOOM_BUILD_GATE_NICED:-}" \
      && "${LOOM_BUILD_GATE_NICE:-1}" != "0" ]] \
   && command -v nice >/dev/null 2>&1; then
  export LOOM_BUILD_GATE_NICED=1
  exec nice -n "${LOOM_BUILD_GATE_NICENESS:-19}" "$0" "$@"
fi

cd "$(git rev-parse --show-toplevel)"

echo "[build-gate] cargo test --lib --bins (workspace unit tests; host-dependent integration targets are CI-only, #3985)"
cargo test --workspace --lib --bins

echo "[build-gate] loom-tools pytest (scoped, excludes network e2e + known slow poll test)"
(
  cd loom-tools
  uv run pytest tests/ -q \
    --ignore=tests/integration \
    --ignore=tests/tokens/test_agent_spawn_integration.py
)

echo "[build-gate] bash installer suite"
bash scripts/test-installer.sh

echo "[build-gate] all stages passed"
