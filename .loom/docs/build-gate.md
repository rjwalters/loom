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

These fields are available in shepherd state and milestone logs for postmortem analysis.

## Why orchestrator-side?

The gate intentionally lives in the shepherd's builder phase (`loom_tools/shepherd/phases/builder.py`), not in the builder *role* prompt. The point is deterministic enforcement independent of agent self-discipline: an agent that crashed, was rate-limited, or simply ignored its prompt should still not produce a PR.
