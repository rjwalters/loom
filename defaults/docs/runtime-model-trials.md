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

Supported OpenCode versions, per launch path (#8438):

| OpenCode | Unguarded free-form launch | Guarded (role-tagged) launch |
|----------|----------------------------|------------------------------|
| 1.x | supported — verified live on 1.18.31 | supported — verified live on 1.18.31 |
| 2.x | supported — argv built for 2.0.10 | **refused before spawn (exit 78)** until a live 2.x guarded canary is recorded |
| any other major | refused before spawn (exit 78) | refused before spawn (exit 78) |

The adapter runs `opencode --version` before it `exec`s and builds the argv for
the reported major; an unreadable, failing or unrecognized version is refused
rather than guessed at, and a missing binary still exits 127. What "built for
2.0.10" does and does not claim: the 2.x shape was read from that release's
`run --help`, and a free-form `run --standalone` launch from the worker's
directory was reproduced live in #8438. The `#<effort>` model suffix and the
whole guarded path have **not** been executed against a real 2.x CLI.

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
guarded tools; free-form trials use the ordinary harness tools. The actual working directory is preserved, so an
inherited stale `PWD` cannot redirect a trial: `PWD` is pinned to the real
directory on the child, and the launch differs by OpenCode major:

| | OpenCode 1.x | OpenCode 2.x |
|---|---|---|
| Working directory | explicit `--dir <cwd>` | inherited across `exec` (2.x removed `--dir`) |
| Server | n/a | `--standalone`, always |
| Effort | `--model provider/model --variant <effort>` | `--model provider/model#<effort>` (2.x removed `--variant`) |

`--standalone` is unconditional on 2.x, guarded or not. There, `run` attaches by
default to a shared background service (`opencode serve --service`) that was
started by some earlier process and therefore never sees this launch's child
environment. Everything Loom hands a native harness travels that way — the
credential mapping, the `{env:VAR}` values it resolves, and the per-launch
`OPENCODE_CONFIG_CONTENT` that is the *only* carrier of a profile's provider
block (see "Multi-variable credentials and provider options" below). Against
the shared service a project-file provider fails with HTTP 401 and a
`providerDefinition` profile with an unknown provider/model. A private server
also keeps two workers bound to different credentials from sharing one process.

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

### Multi-variable credentials and provider options

One API key is the simplest case, not the only one. `credentialEnv` also takes
an **array** of variable names, and `credentialTargets.<harness>` a **map** from
each source variable to the name the harness expects in the child environment:

| Form | Meaning | Presence |
|------|---------|----------|
| `"credentialEnv": "VAR"` | one variable, mapped by a `credentialTargets.<harness>` **string** | optional — a harness login store may supply it instead |
| `"credentialEnv": ["A","B"]` | the **required set**, mapped by a `credentialTargets.<harness>` **object** | every entry must be present in the launching environment |

The array form fails closed before spawn (exit 78) with a diagnostic naming the
missing *variable names*; the legacy string form is unchanged, so `zai-flash`
still launches against a CLI login with `ZAI_API_KEY` unexported. Only mapped
variables are set on the child, and a `credentialTargets` entry naming a
variable `credentialEnv` did not declare is rejected.

Two optional per-harness fields carry **non-secret** provider configuration into
OpenCode's per-launch injected config (`OPENCODE_CONFIG_CONTENT`); Pi rejects
both, since it has no equivalent:

- `providerOptions.<harness>` — merged into `provider.<providerId>.options`
  (region, project, anything the provider block accepts).
- `providerDefinition.<harness>` — the whole `provider.<providerId>` block, for
  an endpoint the harness does not know natively (`npm`, `options`, `models`).

**Secrets travel only as environment variables the child inherits.** Never
interpolate a key into these blocks: reference it with OpenCode's own
`{env:VAR}` indirection and let the credential mapping populate that variable.
A profile whose provider configuration embeds the literal value of one of its
own `credentialEnv` variables is rejected before launch.

Three bundled **examples** (placeholder model ids, no account-specific values —
copy into `runtimes.modelProfiles` and edit, they are not usable as shipped):

| Profile | Provider | Requires |
|---------|----------|----------|
| `example-bedrock` | `amazon-bedrock` | `AWS_PROFILE`; `providerOptions.opencode.region` |
| `example-vertex` | `google-vertex` | `GOOGLE_APPLICATION_CREDENTIALS`, `GOOGLE_CLOUD_PROJECT`, `VERTEX_LOCATION` |
| `example-openai-compatible` | `loom-openweights` | `LOOM_OPENWEIGHTS_API_KEY`, `LOOM_OPENWEIGHTS_BASE_URL`; `providerDefinition.opencode` declares `@ai-sdk/openai-compatible` with `{env:…}` options |

Check any profile's resolvability without spawning a worker — it reads no
secret values, contacts no provider, and exits 78 when a bound harness cannot
be resolved:

```sh
loom-daemon worker profile-check example-bedrock
loom-daemon worker profile-check my-comparison --runtime opencode
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
