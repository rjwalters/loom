# Guardrail parity: Pi and OpenCode

Loom-managed native roles use four tools: `loom_read`, `loom_write`,
`loom_edit`, and `loom_bash`. Their small harness bindings call the Rust
`loom-daemon runtime-tool` service. Policy is not copied into either binding:
Rust normalizes requests through the existing shared guard bridge and its
workflow, destructive-command, and worktree policies.

Tested versions and live outcomes: [verification receipt](native-runtime-verification-2026-09-19.md).

## Enforcement boundary

| Intent | Implementation |
| --- | --- |
| Native edits stay in managed worktrees | Every write/edit target is checked by the existing worktree policy before access. Shell commands use the existing shell-write policy. |
| Protected branches and workflow rules | The same destructive and workflow guards used by the existing runtimes inspect each shell call. |
| Broken guard cannot permit a tool | Missing guard files, malformed/unknown output, nonzero exit, and a 20-second policy timeout refuse the operation. |
| Binding fails to load | Pi starts with builtin tools and extension discovery disabled. OpenCode uses a dedicated primary agent whose default permission is deny, with only the four named tools enabled. Missing bindings therefore leave no executable unguarded tool surface. |
| Concurrent file edits | File operations share a workspace mutation lock; an edit must match exactly one nonempty old-text occurrence. |
| Large output and hung commands | Reads/output are bounded; shell execution uses the existing Rust bounded process executor and a maximum 600-second deadline. SIGTERM/SIGINT cancels the owned shell process group. |
| Model/provider selection | Existing named profiles and explicit provider/model selections; no Claude token-pool preflight or implicit Sonnet default on native sweeps. |
| Credentials | The existing harness credential store or profile environment mapping; keys are never embedded in binding source or arguments. |

Bindings are generated from the binary under `.loom/native-tools/`, which is
machine-local ignored state. The OpenCode binding depends on the matching
`@opencode-ai/plugin` package; OpenCode installs it into that isolated config
directory. No global CLI model/login settings are rewritten by Loom dispatch.

Free-form trials without a Loom role retain the harness's ordinary tools.
The guarded tool contract applies to role-tagged launches and `/loom:<role>`
invocations, including the full sweep. Direct interactive `pi` / `opencode`
sessions remain user-controlled and are not automatically confined by Loom.

## Control capability and lifecycle

`loomControl` means the runtime can execute the installed role's filesystem,
forge and Loom-helper workflow. It is independent of MCP transport. Builder
and Doctor require `loomControl` plus `worktreeIsolation`; Judge requires
`loomControl`. Claude and Codex retain their existing control route. Aider's
unverified generic adapter remains unadmitted for these roles. A custom runtime
must declare the new capability only after verifying its control route.

Pi/OpenCode keep `mcp: no` and `subagents: no`: these adapters neither provision
an MCP server nor expose unverified delegation. `hooks: partial` is deliberate:
the tool-policy boundary is implemented, but Claude Stop hooks and its other
hook events are not supplied. Full native sweeps execute the role lifecycle
sequentially using [native sweep instructions](native-sweep.md) and the installed
role prompts, with the same issue claims, CI, review and merge helpers.

Upgrade the binary and resync installed defaults together. An older installed
role sidecar that still requires MCP will continue refusing Pi until resynced;
an older binary does not implement this tool service.

## Run issue work

With the current binary, installed defaults, and authenticated CLI:

```sh
LOOM_RUNTIME=pi loom-daemon spawn-worker -- --profile zai-flash \
  --log /tmp/issue-worker.log -p '/loom:sweep 123'
LOOM_RUNTIME=opencode loom-daemon spawn-worker -- --profile zai-flash \
  --log /tmp/pr-review.log -p '/loom:judge 456'
```

For daemon-driven work, set `runtimes.default` (or its `sweep-lifecycle` role
binding) to `pi` or `opencode`, and `runtimes.defaultModelProfile` to the desired
profile. Remove stale explicit Claude model pins when changing runtimes;
explicit incompatible pins fail rather than silently choosing another model.
Standalone scheduled roles can have different runtime bindings; one sweep uses
one runtime throughout. Keep the purchased provider plan's concurrency limit
in mind when setting the existing worker concurrency configuration.

## Residual limits

These are application policy guards, not an OS sandbox. They inherit the
existing shell-policy limits (arbitrary programs can have effects beyond what
a shell command analyzer understands), filesystem check/use races, and the
existing path-derived managed-worktree ownership limits. Trusted installed
hooks, helper binaries, harness plugins and configuration are executable code.
Do not treat this integration as containment of a malicious plugin or host.

The initial guarded tool set deliberately excludes native task delegation,
MCP tools, interactive stdin, and unclassified plugin tools. It uses the same
small edit interface in both harnesses; this is not a benchmark of OpenCode's
full native editing/tool ecosystem. CLI exit zero is not acceptance evidence.
Record failed attempts and independently verify code and forge outcomes.

The shared bridge's historical filename is `guard-codex-bridge.sh`; the Rust
boundary uses its established internal request/decision protocol, not a Codex
process or account. Future policy retirement should replace that shared service,
not introduce harness-specific policy copies.
