# Guardrail parity: Pi and OpenCode

Loom-managed native roles use four tools: `loom_read`, `loom_write`,
`loom_edit`, and `loom_bash`. Their small harness bindings call the Rust
`loom-daemon runtime-tool` service. Policy is not copied into either binding:
Rust normalizes requests through the existing shared guard bridge and its
workflow, destructive-command, and worktree policies.

Tested versions and live outcomes: [verification receipt](native-runtime-verification-2026-09-19.md)
— Pi 0.85.1 and OpenCode **1.18.31**. Everything this page says about OpenCode's
guard was verified on OpenCode 1.x only. **No OpenCode 2.x guarded receipt
exists yet**, so a guarded launch on 2.x is refused before spawn; see
"OpenCode major versions" below.

## Enforcement boundary

| Intent | Implementation |
| --- | --- |
| Native edits stay in managed worktrees | Every write/edit target is checked by the existing worktree policy before access. Shell commands use the existing shell-write policy. |
| Protected branches and workflow rules | The same destructive and workflow guards used by the existing runtimes inspect each shell call. |
| Broken guard cannot permit a tool | Missing guard files, malformed/unknown output, and nonzero exit refuse the operation with a `policy error:`-prefixed message. A policy timeout (default 20s, see below) refuses too, but is reported with a distinct `policy timeout:` prefix — see "Policy timeout vs. denial" below. |
| Binding fails to load | Pi starts with builtin tools and extension discovery disabled. OpenCode uses a dedicated primary agent whose default permission is deny, with only the four named tools enabled. Missing bindings therefore leave no executable unguarded tool surface. *(OpenCode: verified on 1.18.31 only.)* A role tick that exits 0 having used no `loom_*` tool is additionally reported as a failed tick, not a success — see "Toolless launch detection". |
| Concurrent file edits | File operations share a workspace mutation lock; an edit must match exactly one nonempty old-text occurrence. |
| Large output and hung commands | Reads/output are bounded; shell execution uses the existing Rust bounded process executor and a maximum 600-second deadline. SIGTERM/SIGINT cancels the owned shell process group. |
| Model/provider selection | Existing named profiles and explicit provider/model selections; no Claude token-pool preflight or implicit Sonnet default on native sweeps. |
| Credentials | The existing harness credential store or profile environment mapping; keys are never embedded in binding source or arguments. |

Bindings are generated from the binary under `.loom/native-tools/`, which is
machine-local ignored state. The OpenCode binding depends on the matching
`@opencode-ai/plugin` package; OpenCode installs it into that isolated config
directory. That package is pinned to 1.18.31 and was verified against an
OpenCode 1.18.31 host only; whether a 2.x host loads it is unverified. No
global CLI model/login settings are rewritten by Loom dispatch.

## Policy timeout vs. denial (#8451)

The guard bridge runs as a subprocess (`guard-codex-bridge.sh`, which itself
forks `guard-loom-workflow.sh` and `guard-destructive.sh`) under a bounded
deadline in `loom-daemon/src/native_tools/guard.rs`. On a CPU-saturated host
that fork chain can outrun the deadline before it ever produces a decision —
observed live on a host at load average 55/28 cores (another tenant's test run
holding ~19 cores, not an exec-scan or a guard defect): 4 of 21 `loom_bash`
calls refused in the first 9 minutes of a sweep, including plain `cat
.loom/config.json`.

Both outcomes fail closed (no tool runs either way), but they mean different
things to the model driving a native sweep, so every `loom_bash`/`loom_read`/
`loom_write`/`loom_edit` failure text carries a class prefix:

| Prefix | Meaning | Retry? |
| --- | --- | --- |
| `policy denied: <reason>` | The check ran to completion and the shared guards said no. | No — this is a real refusal; change the command, not the wording. |
| `policy timeout: …` | The check did not finish inside the budget; the command was never evaluated. | Yes — transient, most likely host load. Retrying the identical command is reasonable once. |
| `policy error: …` | The guard scripts are missing, crashed, or returned something the bridge could not parse. | Treat as a refusal (fail closed), but it is a provisioning defect, not a policy decision about this command — do not keep retrying the same command in a loop. |

[`native-sweep.md`](native-sweep.md) tells the model to apply this distinction
directly.

### Configuring the budget

The fixed 20-second budget was chosen on an idle host and is now configurable,
still bounded and fail-closed at the limit (never "wait forever"):

- `guards.nativePolicyTimeoutSecs` in the effective (tiered) `.loom/config.json`.
- `LOOM_NATIVE_POLICY_TIMEOUT_SECS` env var, which outranks the config key
  (env > config > default, the repo's usual precedence).
- Default `20`; both sources are clamped to `[5, 120]` seconds, so a stray
  value (e.g. `0`, or a typo like `99999`) cannot turn the guard into an
  instant-refuse or an unbounded hang.

### Per-worker timeout counter

Every policy timeout appends one line to
`.loom/native-tools/policy-timeouts.jsonl` (`{"worker_pid", "budget_secs",
"at"}`), keyed by `LOOM_NATIVE_WORKER_PID` — the stable per-sweep identity
`native-sweep.md` already uses for session liveness. A starving sweep is
therefore visible by reading that file (or counting lines for its own worker
pid via `loom-daemon`'s `policy_timeout_count` helper) rather than only from
scrollback. This is a best-effort local log, not a daemon-side telemetry
record; a host too saturated to append one small file has bigger problems
than a missed counter increment.

### Read-only fast path reachability

`guards.readOnlyFastPath` (default on) is implemented inside
`guard-destructive-generic.sh` itself, and `guard-destructive.sh` (which the
bridge's `shell_command` branch runs) `exec`s straight into it — so a shell
command's destructive-guard leg **does** reach the fast path today; an
in-workspace `cat`/`ls`/`grep` skips the expensive parts of that leg (the git
`rev-parse` and the deny/ask array scan) before ever forking further. The
bridge's *other* forked guard for a shell command,
`guard-loom-workflow.sh`, always runs in full — it has no fast path of its own
and is out of scope here, since it is a separate, comparatively cheap
protected-branch/workflow check, not the destructive-command analyzer this
issue is about.

## OpenCode major versions

The adapter probes `opencode --version` before `exec` and tracks, in code, which
majors have a **live guarded-canary receipt** (`Major::guard_verified` in
`loom-daemon/src/worker_spawn/opencode_version.rs`). Today that is 1.x only. A
guarded launch — `LOOM_ROLE` set, or a `/loom:<role>` prompt — on any other
major is refused before spawn with exit 78, before any binding is provisioned.
Unguarded free-form launches on 2.x are unaffected. This is evidence tracking,
not a setting: there is no configuration key or environment override, and a
major is added only by the change that also adds its dated receipt here.

Why a passing fake-CLI test suite is not enough: OpenCode 2.x documents
`run --auto` as approving every permission that is *not explicitly denied*, and
Loom passes `--auto` for `--dangerously-skip-permissions`. If 2.x ignores any of
the deny-by-default agent, top-level `permission`, or `tools` keys Loom injects,
a role launch would run OpenCode's built-in write and shell tools, auto-approved
and outside Loom's guards — and still exit 0. Fixture tests prove Loom *sets*
that configuration, never that a real CLI *honors* it. Only a live canary that
includes the deliberately-broken-binding case (pass condition: no file written
and no executable unguarded tool, not the exit code) distinguishes "failed
closed" from "fell open". Still unverified on 2.x: the isolated
`OPENCODE_CONFIG_DIR`, `plugins/loom.ts` discovery, the plugin SDK pin above,
the agent/permission/tools config keys, and `OPENCODE_CONFIG_CONTENT` being read
by the private server `--standalone` starts.

A live guarded canary was run on **OpenCode 2.0.10** on 2026-09-20 (issue #8448)
and **failed, safely**: the binding is never loaded, so the worker started with
the deny-by-default agent and no tools at all, used zero tools, recorded zero
policy denials, changed no protected bytes, did not do the task — and exited
`0` in 9s. An identical control run on 1.18.31 passed (four `loom_*` tools
offered, seven tool uses, three recorded denials, task completed). So on 2.x:
the deny-by-default configuration **is** honored under `--auto` (it fails
closed, it does not fall open), and what is broken is binding *discovery* — no
`node_modules/` appears in the isolated config dir and `opencode plugin list`
reports none, with or without an explicit `plugin` entry. The 2.x-native
mechanism for loading a local, per-launch tool binding is still unknown, so
`Major::guard_verified`'s `V2 => false` refusal stays until a passing 2.x
receipt exists.

## Toolless launch detection

CLI exit zero is not acceptance evidence (see "Residual limits"), and since
#8448 it is no longer *treated* as evidence either. The 2.x canary above and its
passing 1.x control both exited `0`; nothing in the exit status distinguished
"did the work with four guarded tools" from "had no tools and did nothing".

A guarded native role tick is therefore classified from its own native event
stream, not its exit code (`loom-daemon/src/role_runner/toolless_launch.rs`,
built on `worker_spawn::launch_outcome`). A tick that exits 0 with **zero
`loom_*` tool uses** in its own region of `.loom/logs/role-<role>.log` is
reported as a `Failure`, counted and escalated like any other failed tick,
rather than as a healthy `Success`. A tool call that the guard *denied* still
counts as a use — the binding loaded, policy then said no; that is a working
guard, not a toolless launch.

The check deliberately stands down — leaving the pre-#8448 success verdict
untouched — for any non-native runtime, a tick with no resolved runtime
admission, a log whose per-tick anchor is missing, and a stream containing no
events this classifier can parse. The last one matters most: an unparsed stream
is a gap in Loom's own observation, never evidence about the launch, so a future
harness release that renames its event types degrades the check to "no opinion"
instead of failing every tick.

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

OpenCode 2.x `run` attaches by default to a shared, long-lived background
service. The guarded binding reads `LOOM_WORKSPACE`, `LOOM_NATIVE_TOOL_BIN` and
its working directory from *whichever process loads the plugin*; under a shared
service those belong to the service — absent, so no tools, or stale from an
earlier launch, so tool calls are guarded against the wrong workspace. Loom
therefore always passes `--standalone` on 2.x. An operator who starts OpenCode
some other way, or points a worker at a shared server, is outside this boundary.

The initial guarded tool set deliberately excludes native task delegation,
MCP tools, interactive stdin, and unclassified plugin tools. It uses the same
small edit interface in both harnesses; this is not a benchmark of OpenCode's
full native editing/tool ecosystem. CLI exit zero is not acceptance evidence
(see "Toolless launch detection" for the check that now enforces this on role
ticks). Record failed attempts and independently verify code and forge outcomes.

An OS-level backstop for exactly those residual limits is available, opt-in, as
per-sweep ephemeral containment (`runtimes.containment.native: "ephemeral"`,
issue #8403) — the same container lifetime Claude sweeps use, with an image
pinning the CLI versions recorded above, per-launch XDG/config/session
directories, and env-only credential injection. It does not narrow the
application-policy limits described here; it bounds their blast radius. See
[runtime adapters](runtime-adapters.md) § "Native-harness ephemeral
containment". Uncontained dispatch remains the default and is unchanged.

The shared bridge's historical filename is `guard-codex-bridge.sh`; the Rust
boundary uses its established internal request/decision protocol, not a Codex
process or account. Future policy retirement should replace that shared service,
not introduce harness-specific policy copies.
