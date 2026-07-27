# Post-Builder Quality Gate (`buildGate`)

The post-builder quality gate is a deterministic, orchestrator-side check that runs **after the builder agent exits but before any PR is opened**. It short-circuits PR creation when the builder's work obviously isn't shippable, releases the issue claim, and lets the next builder re-attempt the issue.

See issue [#3347](https://github.com/rjwalters/loom/issues/3347) for the original proposal.

## Why a gate?

Builder agents (Claude Code as well as external engines invoked by parallel swarms) occasionally ship PRs that should never have been opened:

- broken builds,
- commits containing only logfiles / scratch files,
- no commits at all.

Without a gate, the Judge phase has to catch every one of these post-hoc, which wastes review cycles and pollutes the queue. The gate moves that filter ~30s of CPU instead of a multi-minute Judge cycle, and on parallel-shepherd fleets the savings compound.

## The three checks

The gate runs three checks in order. Any failure short-circuits PR creation:

1. **has-commits** — `git rev-list --count origin/main..HEAD > 0` in the worktree.
2. **has-real-changes** — at least one changed file matches the configured `realChangeGlobs` (or the default scratch-exclusion list when no globs are configured).
3. **build-passes** — the configured `buildGate.command` exits with code 0 inside the worktree.

When all three pass the builder phase proceeds normally to PR creation. When any one fails the orchestrator:

- Atomically releases the claim: `loom:building` -> `loom:issue`.
- Logs an `error` milestone with `reason=build_failed_post_builder` and `check=<failed_check>`.
- Cleans up the stale worktree.
- Returns a `FAILED` `PhaseResult` so the shepherd does not progress to Judge.

## Configuration

The gate is **opt-in**. Repos with no `buildGate` block in `.loom/config.json` see zero behavior change — the gate returns immediately.

```json
{
  "nextAgentNumber": 1,
  "terminals": [],
  "buildGate": {
    "enabled": true,
    "command": "cargo build --workspace",
    "realChangeGlobs": ["*.rs", "*.toml", "Cargo.lock"],
    "timeoutSeconds": 600
  }
}
```

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `enabled` | boolean | `true` when block is present | Set to `false` to disable the gate without removing the block. |
| `command` | string | _(none)_ | Shell-style command run in the worktree (parsed with `shlex.split`). When omitted, the build check is skipped but the has-commits and has-real-changes checks still run. |
| `realChangeGlobs` | array of strings | _(default exclusions)_ | Positive globs. A changed file must match at least one to count as "real." When omitted, every changed file counts unless it matches one of the default scratch exclusions: `.loom-*`, `*.log`, `.no-changes-needed`. |
| `timeoutSeconds` | integer | `600` | Timeout for the `command` run. |

> **Not a `buildGate` key:** the daemon-side main-health gate's optional forge
> verification-workflow name (`autonomous.mainHealthGate.ciWorkflow` /
> `LOOM_GATE_CI_WORKFLOW`, #3987) lives under `autonomous.mainHealthGate`, **not**
> here. `buildGate` is the builder-side worktree quality gate; it has no business
> knowing about forge CI. See [Optional named verification workflow](daemon-reference.md#optional-named-verification-workflow-loom_gate_ci_workflow-3987).

## Examples

### Rust workspace

```json
{
  "buildGate": {
    "command": "cargo build --workspace",
    "realChangeGlobs": ["*.rs", "*.toml", "Cargo.lock"]
  }
}
```

### Python project with pytest

```json
{
  "buildGate": {
    "command": "python -m pytest -x",
    "realChangeGlobs": ["*.py", "pyproject.toml"]
  }
}
```

### Node.js project

```json
{
  "buildGate": {
    "command": "pnpm check:ci",
    "realChangeGlobs": ["*.ts", "*.tsx", "*.js", "package.json"],
    "timeoutSeconds": 900
  }
}
```

### Disable without removing config

```json
{
  "buildGate": {
    "enabled": false,
    "command": "cargo build"
  }
}
```

### This repo's configuration (polyglot backstop)

Loom's own `.loom/config.json` points `buildGate.command` at a committed
wrapper script rather than a single-language one-liner, because this repo is
polyglot (Rust + Python + bash) and no single build tool covers it:

```json
{
  "buildGate": {
    "enabled": true,
    "command": "bash .loom/scripts/build-gate.sh",
    "realChangeGlobs": ["*.rs", "*.toml", "Cargo.lock", "*.py", "*.sh"],
    "timeoutSeconds": 600
  }
}
```

The wrapper lives at [`defaults/scripts/build-gate.sh`](../scripts/build-gate.sh)
(the installer-template source of truth; `.loom/scripts/build-gate.sh` resolves
to it via the `.loom/scripts -> ../defaults/scripts` symlink). It runs three
stages in order under `set -euo pipefail`, aborting on the first non-zero exit:

1. `cargo test --workspace --lib --bins` — the Rust crates' **unit tests**
   (`loom-daemon`, `loom-api`). The integration test **targets** under
   `loom-daemon/tests/` are deliberately excluded here — see "Local gate vs.
   CI" below.
2. `uv run pytest tests/ -q` in `loom-tools/`, scoped with
   `--ignore=tests/integration` (live-network/credentials e2e) and
   `--ignore=tests/tokens/test_agent_spawn_integration.py` (a slow real-time
   modal-poll integration file). `uv run` is used so `loom_tools` is importable
   from the project venv.
3. `bash scripts/test-installer.sh` — the 131-case bash installer suite.

**`mcp-loom` (TypeScript) is intentionally excluded** from the gate: it needs
`npm install`/`npm ci` in a fresh worktree (no guaranteed warm `node_modules`),
which would add unpredictable latency to a gate that also runs once per PR. CI
(`.github/workflows/ci.yml`) still gates the `mcp-loom` build. `timeoutSeconds:
600` gives ~2x headroom over the measured ~210s warm-cache total to absorb a
cold `target/` in a fresh worktree.

Beyond the per-wave step-8 gate, this same command runs after **every** builder
exit (the "Post-Builder Quality Gate" above), so it is deliberately kept fast
and free of network/npm dependencies. This is a repo-specific,
self-hosting-only config; `defaults/config.json` (the generic install template)
ships with no `buildGate` block. See issue
[#3749](https://github.com/rjwalters/loom/issues/3749).

### Local gate vs. CI: the environment-sensitivity boundary (#3985)

The local gate and GitHub CI run *nearly* the same Rust command, but they
measure different things, and that difference is deliberate:

| | Command | Measures |
|---|---------|----------|
| **CI** (`.github/workflows/ci.yml`) | `cargo test --workspace` (all targets, incl. integration) | **the commit**, in a controlled runner with a guaranteed-live tmux |
| **Local gate** (`build-gate.sh`) | `cargo test --workspace --lib --bins` (unit tests only) | **the commit**, on whatever host is actively running Loom |

The local gate runs on the machine that is *also running the sweeps* — a busy,
sometimes headless, sometimes tmux-less host. Any assertion in the gate that
depends on that host's configuration measures the **host**, not `main`, and so
can go red for a reason that has nothing to do with the code being gated. That
is inverted: a gate is supposed to be green when `main` is correct, not green
only when the host happens to be idle and fully provisioned.

Two concrete failure classes motivated the split (#3985):

- **Dead tmux server.** The `loom-daemon/tests/integration_basic.rs` terminal
  tests create real tmux sessions and assert they exist. On a host with no
  reachable tmux server they can only fail. These live in integration test
  **targets**, which `--lib --bins` excludes — so the gate never runs them,
  and CI (which always has tmux) still does. As belt-and-suspenders, those
  tests now **skip cleanly** (via a `require_tmux!()` probe) rather than fail
  when no tmux is available, so even a full local `cargo test --workspace`
  stays green on a tmux-less host.
- **Fixture-child wait bounds.** A few `sweep_registry` unit tests wait on a
  fixture child with a wall-clock deadline. Those bounds are now generous
  (`FIXTURE_CHILD_WAIT_MS`, 60s). This was originally attributed to *CPU
  starvation* under host load, but that attribution was **falsified**: #4044 /
  #4046 established the timing-test failures were macOS `syspolicyd`
  exec-latency artifacts, not contention (968/968 passed later with **no code
  change**). The generous bounds are still correct; the "sweeps starve the
  gate's timing tests" story that motivated them is not.

**The gate runs at a mild throttle relative to sweeps (#4020, revises #3985).**
`build-gate.sh` now defaults to `nice 5` — a mild positive niceness, a real but
small step down from the sweep children's `nice 0`. It previously re-exec'd
itself at `nice -n 19` (the lowest priority) on the rationale that a long
`cargo` compile would otherwise starve the sweeps and the timing-sensitive
tests it shares a host with. That rationale is **withdrawn**: #4044 / #4046
showed the timing-test failures were `syspolicyd` exec-latency artifacts, not
CPU contention, and the one real gate timeout was cold-compile cost (settled by
#4048 raising the budget 600→1200s; the gate then produced its first
determinate verdict, Green at ~726s). Handicapping the gate to the *bottom* of
the run queue solved a problem that was never demonstrated — and the gate is the
reliability substrate that halts dispatch when `main` goes red, so a gate
starved into `UNEVALUATED` is a gate that is not gating.

Why `nice 5` and not `nice 0` parity? Giving the gate a *strictly higher*
priority than sweeps is not possible from `build-gate.sh` on the daemon host
(macOS, non-root): a negative nice requires privilege the daemon does not hold
(`nice -n -5` → `setpriority: Permission denied`), so `0` is the best
unprivileged priority. Dropping the gate to `nice 0` would leave gate and sweeps
at the *same* value — an indistinguishable parity with no measurable gap, which
#4020's AC2 explicitly calls a failure ("A patch that leaves both at the same
value fails this AC"). `nice 5` is the smallest defensible choice that keeps a
real, non-zero, unprivileged-achievable gap in the achievable direction
(gate `5` > sweeps `0`): the gate yields *slightly* under contention rather than
starving the sweeps, without being drastically deprioritized as it was at 19.
The alternative — niceing sweep *children* up in the spawn path to force a
strict gate-below-sweep gap — is deliberately not done, since the issue directed
the spawn path be left untouched and the contention it would brace against is
the one the evidence above withdrew.

The re-exec mechanism and its knobs are preserved: `LOOM_BUILD_GATE_NICENESS`
overrides the value (e.g. `=0` for exact sweep parity, `=19` to restore the old
handicap), `LOOM_BUILD_GATE_NICE=0` disables the re-exec entirely, and the
`LOOM_BUILD_GATE_NICED` sentinel prevents a re-exec loop. The re-exec is skipped
when the effective niceness is 0 (a `nice -n 0` re-exec is a no-op fork); with
the default of 5 the gate re-execs once under `nice -n 5`. If `nice` is
unavailable the gate proceeds at normal priority.

> **Rule of thumb:** if a check's outcome can differ between an idle host and a
> busy one — or between a host with tmux and one without — it is
> **environment-sensitive** and belongs in CI (which controls its
> environment), not in the local post-builder gate. Keep the gate scoped to
> checks that are deterministic on any host.

**Forge-CI corroboration (deferred).** The curated issue's fourth acceptance
criterion — when the local gate disagrees with a *green* forge CI result on the
same evaluated SHA, prefer the forge signal and log the divergence loudly
rather than halting — is a distinct, larger daemon-side feature (it belongs
with the RED-classification work in #3974 AC4, not the test/scope hardening
here). It is intentionally **not** implemented in this change; the scope split
+ test hardening + `nice` above already break the self-reddening loop this
issue targets. Tracked as follow-up under #3974.

## Failure semantics

A gate failure is **not** the same as a builder failure: the issue is automatically re-queued (`loom:issue`) and a future builder can take a fresh attempt. The `PhaseResult.data` block carries:

```python
{
  "post_builder_gate_failed": True,
  "gate_check": "has_commits" | "has_real_changes" | "build_passes",
  "gate_detail": "<human-readable failure reason>",
  "reason": "build_failed_post_builder",
  "claim_released": True,
}
```

These fields are available in sweep logs (`.loom/logs/sweep-issue-N.log`) and sweep checkpoints (`.loom/sweep-checkpoint/issue-N.json`) for postmortem analysis.

## Why orchestrator-side?

The gate intentionally lives in the orchestrator's builder phase (the `/loom:sweep` skill's Builder step), not in the builder *role* prompt. The point is deterministic enforcement independent of agent self-discipline: an agent that crashed, was rate-limited, or simply ignored its prompt should still not produce a PR.
