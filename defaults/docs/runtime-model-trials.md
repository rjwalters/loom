# Native harness and model trials

Pi and OpenCode are experimental headless Loom runtimes. Their launch adapters live in
`loom-daemon/src/worker_spawn/`; the existing `spawn-worker.sh` path is an exec
stub. No background daemon or MCP server is needed for a free-form trial.

## Separate the choices

- **Harness** (`LOOM_RUNTIME`, or `runtimes.default`/`runtimes.roles`) selects
  the CLI, argument translation, instruction loading and event format.
- **Model profile** selects a model, each harness's provider ID, reasoning
  settings and credential environment mapping. `defaults/model-profiles.json`
  is bundled in the binary. Configuration can add or replace profiles under
  `runtimes.modelProfiles`; no new launcher is needed to add a model.
- **Capability admission** remains Loom's role-specific gate. Role launches now use [guarded native tools](guardrail-parity-native.md) for
  Builder, Doctor, Judge and sequential sweeps. MCP remains unprovisioned;
  `loomControl` records the verified CLI route independently of transport.
- **Evaluation** is external to the worker: freeze inputs, retain raw events,
  run independent acceptance checks, and include failures and repairs. A CLI
  exit status alone does not establish that an issue was solved.

Adding a harness requires implementing its launch translation, declaring its
capabilities, and passing the subprocess/instruction/admission contract tests.
Adding a provider usually requires configuring that provider in the CLI and
binding its provider ID in a profile. Loom does not duplicate the CLIs' provider
HTTP clients, credential stores or model catalogs.

## First profile: Z.ai Coding Plan

Tested CLI versions: Pi (`@earendil-works/pi-coding-agent`) 0.85.1 and OpenCode
(`opencode-ai`) 1.18.31. Install the CLIs using their upstream instructions and
put them on PATH, or set `LOOM_PI_BIN` / `LOOM_OPENCODE_BIN` to their executables.

Export `ZAI_API_KEY` in the launching environment, or authenticate the selected
provider using the CLI's own login flow. Loom never sources your shell startup
files. The profile maps that key to Pi's `ZAI_API_KEY` and OpenCode's
`ZHIPU_API_KEY` only in the child environment. Credentials are not arguments,
profile values or launch-log fields.

The shipped `zai-flash` profile selects `glm-5.3-flash`, effort `max`, and the
Coding Plan provider (`zai` in Pi, `zai-coding-plan` in OpenCode), whose endpoint
is `https://api.z.ai/api/coding/paas/v4`. It does not select the general PAYG
provider. CLI configuration can override upstream endpoints, so use a clean
CLI profile for reproducible experiments. Paid plan allowance and PAYG prices
are separate; a harness's cost estimate is not a measured subscription charge.

Run in a disposable checkout with `ZAI_API_KEY` already exported:

```sh
LOOM_RUNTIME=pi loom-daemon spawn-worker -- --profile zai-flash \
  --log /tmp/pi-trial.log -p 'Read the task, implement it, and run its tests.'
LOOM_RUNTIME=opencode loom-daemon spawn-worker -- --profile zai-flash \
  --dangerously-skip-permissions --log /tmp/opencode-trial.log \
  -p 'Read the task, implement it, and run its tests.'
```

Pi's tools execute without interactive approval. OpenCode's Loom skip-permission
flag maps to `run --auto`, which still respects explicit denials. Role launches separately provision Loom
guarded tools; free-form trials use the ordinary harness tools. The actual working directory is preserved; OpenCode gets
an explicit `--dir` so an inherited stale `PWD` cannot redirect a trial.

The compatibility entry point accepts the same worker options:
`LOOM_RUNTIME=pi .loom/scripts/spawn-worker.sh --profile zai-flash -p '...'`.
When testing an uninstalled build, set `LOOM_DAEMON_SELF_BIN` to the newly built
binary before invoking the stub. Existing Claude/Codex adapters still receive
all their arguments unchanged.

## More models

Use `--model provider/model` for a one-off selection using the selected CLI's
provider vocabulary. This bypasses the default model profile; pass `--effort`
explicitly if desired. There is no fuzzy Claude-alias translation or silent
provider fallback. For repeatable comparisons, define a named profile:

```json
{
  "runtimes": {
    "defaultModelProfile": "my-comparison",
    "modelProfiles": {
      "my-comparison": {
        "model": "EXACT_MODEL_ID",
        "providers": { "pi": "PI_PROVIDER_ID", "opencode": "OPENCODE_PROVIDER_ID" },
        "credentialEnv": "MY_PROVIDER_KEY",
        "credentialTargets": { "pi": "PI_KEY_ENV", "opencode": "OPENCODE_KEY_ENV" }
      }
    }
  }
}
```

Native adapters require `--prompt`; use the harness CLI directly for interactive
sessions. Legacy runtimes retain their interactive behavior.

Profile selection: `--profile` > `LOOM_MODEL_PROFILE` >
`runtimes.defaultModelProfile` > shipped `zai-flash` for these two adapters.
`--effort` overrides the profile's effort. Profiles may specify `allowedEfforts`
to reject unsupported reasoning levels before making a request. Unknown worker
flags fail closed; model/profile options do not pass secrets through to CLI
arguments. A bare `--model` must match the selected profile's model; otherwise
use a qualified model or another profile.

For an eligible scheduled role, use `runtimes.roles.<role>` and explicitly pin
`autonomous.roleRunner.roleModels.<role>` to a qualified model or `"default"`
to use the selected profile. An unconfigured native role uses its profile,
never the shipped Claude `sonnet` alias. Its authentication does not require
a Claude token pool. `/loom:<role> arguments` expands the installed role text
and repository instructions, and checks admission before launch.

## Evidence and limits

`--log` appends both streams, launch identity and native JSON events to a file.
Without it, events stream to stdout and launch diagnostics to stderr. The
`LOOM_LAUNCH` record names the harness, provider, model, profile and effort.
The process uses Unix exec, preserving PID, signal death and exit status;
Loom's existing role runner owns timeout and descendant termination.

Pi usage appears on assistant `message_end` events; count each once, not again
inside `agent_end`. OpenCode reports usage on `step_finish`. Retain input,
output, reasoning, cache counters and tool failures. Missing counters mean
unmeasured, not zero. The existing Claude transcript/cost dashboard is not a
generic evaluator and should not be used to infer these trials' billed cost.

For full issues measure cost per independently accepted issue, with identical
base commits, dependencies, task text and reviewer. Include failed attempts and
reviewer repairs. A tiny tool-use canary establishes connectivity and basic edit
handling, not comparative model quality or Builder admission.

Sources: [Pi](https://github.com/earendil-works/pi/tree/main/packages/coding-agent),
[Z.ai Pi setup](https://docs.z.ai/devpack/tool/pi),
[Z.ai OpenCode setup](https://docs.z.ai/devpack/tool/opencode),
[OpenCode CLI](https://opencode.ai/docs/cli/).

The [2026-09-19 canary receipt](https://github.com/rjwalters/loom/blob/main/docs/experiments/native-harness-canary-2026-09-19.json)
records CLI versions, observed model/provider IDs, token counters, event hashes
and independent results. Pi passed; OpenCode initially selected the wrong
working directory under stale `PWD`, then passed after explicit directory
selection. Both used read/edit tools and ran the unchanged tests. OpenCode's
reported cost of zero is retained as a harness report, not proof of free usage.
