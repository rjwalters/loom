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

## API-key account pool (`loom-daemon api-keys`, #8401)

A single exported `ZAI_API_KEY` (above) is the trial path — fine for one
account. Running a **fleet** of API-key subscriptions (e.g. several Z.ai GLM
coding-plan accounts) through OpenCode/Pi the way the fleet already rotates
Claude OAuth accounts ([`token-pool.md`](token-pool.md)) needs a registry and
per-spawn selection, which is what `loom-daemon api-keys` provides — the
API-key analogue of `loom-daemon tokens` (Claude OAuth) and `loom-daemon
accounts` (Codex `CODEX_HOME` profiles), implemented at
`loom-daemon/src/api_keys_pool/`.

**Registry.** One account is one file: `.loom/api-keys/<provider>/<account>.env`
(gitignored, `0600`, per host — never in `.loom/config.json` or
`model-profiles.json`), holding a single `KEY=value` assignment. `<provider>`
is a pool namespace derived from the profile's `credentialEnv`
(`ZAI_API_KEY` -> `zai`; override with a profile's `credentialPool` when two
profiles must share one pool, or when two credential sources for the same
model family — say a flat-rate subscription and a metered key — must be kept
as **separate** pools). `KEY` must be `UPPER_SNAKE_CASE` and must equal the
profile's `credentialEnv`: an account file that assigns any other variable is
withheld from that profile rather than injected under its harness variable.
`add` defaults `KEY` to `<PROVIDER>_API_KEY`, so an account for a
`credentialPool`-named pool needs an explicit `--env-var <credentialEnv>`.
Manage it with:

```sh
loom-daemon api-keys add zai alice --key-file /path/to/key   # or pipe on stdin
loom-daemon api-keys add zai-metered team --env-var ZAI_API_KEY --shared --key-file /path/to/key
loom-daemon api-keys list [--provider zai] [--json]
loom-daemon api-keys disable zai alice
loom-daemon api-keys enable zai alice
loom-daemon api-keys limit zai alice --max-concurrent 2      # or --unlimited
loom-daemon api-keys remove zai alice
```

No verb accepts key material on the command line; `add` reads it from
`--key-file` or stdin only. `list`/`health` render a secret-free account
record that cannot carry a value — on a host with no pool, `list` says so and
exits `0`. A hand-placed file whose left-hand side is not `UPPER_SNAKE_CASE`
(a bare base64 key with `=` padding, for instance) is reported `unusable` with
`variable=-`; its text is never echoed.

**Per-repo and shared pools.** There are two pool roots, in precedence order:

1. the per-repo pool, `<repo>/.loom/api-keys/` — what `add` writes by default;
2. the shared machine-level pool, `~/.loom/api-keys/` — what `add --shared`
   writes. `LOOM_SHARED_API_KEYS_DIR=<dir>` relocates it (`~` is expanded);
   `LOOM_SHARED_API_KEYS_DIR=` (set but empty) disables it entirely.

Precedence is resolved **per provider**, not per repo: a provider uses the
per-repo root if that root holds at least one account *for that provider*,
and the shared root otherwise. A repo with its own `zai` accounts therefore
still gets `zai-metered` (or `openai`, …) from the shared pool. Within one
provider the two roots are never merged — registering a per-repo account for
`zai` takes `zai` over for that repo, and `add` prints a note when doing so
puts shared accounts out of reach. `disable`/`enable`/`remove`/`mark-bad`/
`unblock` act on whichever root is effective for the provider they name.

**Selection at spawn.** When a profile's `credentialEnv` is set, the native
dispatcher resolves it in this order: explicit environment (unchanged from
the single-key trial path above) > pool selection for the profile's provider
> fail closed. Pool selection excludes disabled and bad-marked accounts, then
prefers a `.allowlist` pin (falling back to the full eligible set if the pin
is stale), then round-robins across what remains using the same rotation
cursor `spawn-claude.sh`'s selection ladder uses — consecutive spawns
alternate accounts. The chosen account's **name** (never the key) is recorded
in the `LOOM_LAUNCH` record's `credentialAccount`/`credentialProvider`/
`credentialSource` fields and in the sweep log; a host with no accounts
registered for the provider is not an error (the harness is left to its own
auth store), but an *all-exhausted or all-disabled* pool for a provider that
does have registered accounts fails closed at exit `78` (`EX_CONFIG`) with the
same "which binary decided, and why each account was excluded" diagnostic
shape an empty Claude pool produces.

**One account supplies one variable.** An account file is a single
`KEY=value`, so the pool fills at most **one** unset source variable of a
profile's credential mapping; every variable that *is* exported passes through
from the environment exactly as described under
[Multi-variable credentials](#multi-variable-credentials-and-provider-options).
In practice that means pooling is for the single-string `"credentialEnv":
"VAR"` form. The array form is by definition a set that must already be present
in the launching environment — that check runs first, so the pool is never
reached for it — and a profile that sets `credentialPool` while leaving more
than one variable unset is rejected at `78`.

**"No pool" means the directory does not exist — nothing else.** A provider
directory that exists but cannot be read (`EACCES` because it is `0700` and
owned by a different uid, a uid-mismatched bind mount in a container, `EIO`,
`ESTALE`) also fails closed at `78`, naming the path and the error: accounts
may well be registered there, and all disabled to stop spend, so falling back
to the harness's own auth store would run the spawn on a credential the
operator did not choose. `list` and `health` report the unreadable directory
and exit non-zero instead of printing "no accounts". The same rule covers the
state files that decide eligibility: an unreadable `.disabled`/`.allowlist`, or
a `.bad_accounts.json` that is empty or does not parse, **withholds** the
provider's accounts (`health` counts them `unverifiable`) and `mark-bad`/
`unblock` refuse to rewrite it. To recover, repair the file, or delete it to
discard that provider's marks. All pool files are written atomically (temp
file + `rename`, `0600`), so Loom itself never leaves one half-written.

**Exhaustion / bad-marking.** `loom-daemon api-keys mark-bad <provider>
<name> --reason <text> [--cooldown-secs N] [--model-class <id>]` removes an
account from selection until its reset horizon (or indefinitely, until
`api-keys unblock`); a bad mark self-heals once its horizon passes, with no
operator action required. `loom_daemon::api_keys_pool::classify` recognises a
harness's own quota/rate-limit error text (`insufficient balance`, HTTP `429`,
…) and produces the classification that is recorded.

*Marks are automatic as well as operator-invoked (#8424).* After a native
sweep or role tick exits, Loom re-reads that run's own retained log
(`api_keys_pool::ingest`) and bad-marks the account the run used when the log
says its allowance ran out. This is deliberately **post-hoc**, not live
interception: a native harness spawn `exec`s the child to preserve PID/signal
parity for the reaper and role runner, so no Loom process survives to watch
the stream — the same shape `sweep_registry`'s Codex health bridge and
`worker_spawn::launch_outcome` already use. The cost, stated plainly: the mark
lands when the run exits rather than the moment it fails, and a run whose
output Loom never retains is never ingested. Four guards keep it from marking
a healthy account — only a pool-selected credential (never an operator's
one-off `export`), only from Loom's own `# LOOM_LAUNCH` record, never from an
exit-0 run, and never from an auth failure (below).

*Auth failures are not exhaustion.* A `provider.auth` / HTTP 401 harness event
is classified `credential-failure`: it is surfaced in the log and **never**
bad-marked, and it carries no reset horizon at all. On OpenCode 2.x a 401 is
also what a *correct* key looks like when the launch forgets `--standalone`
(#8438), so marking on it would take a healthy account out of the pool for a
launch-configuration bug.

*Pattern provenance.* Each entry in the classifier's table records whether it
was captured from real harness output or transcribed from provider
documentation. The captured `provider.auth`/401 event above is real (OpenCode
2.0.10, `run --format json`, observed 2026-09-20); **no live Z.ai coding-plan
*exhaustion* string has been captured yet**, so those rows remain documented
shapes and a unit test keeps that labelling honest. Fold each new capture in
as it is observed.

*Model-class scoping.* `--model-class <model-id>` (e.g. `glm-5.3-flash`, an
`#effort` suffix is stripped) scopes a mark to one allowance, mirroring the
Claude pool's `is_bad_for_class`: an exhausted `glm-5.3-flash` allowance
leaves the same account selectable for `glm-5`. Omit it for an account-wide
mark — which is what every pre-#8424 mark is, and what an automatic mark falls
back to when the launch record names no usable model. `api-keys unblock` takes
the same flag: with it, only that class's mark is cleared; without it, every
mark for the account is.

**Concurrency.** `loom-daemon api-keys limit <provider> <name>
--max-concurrent <N>` (or `--unlimited` to clear it; `api-keys add` accepts
`--max-concurrent` too) declares a provider-side concurrent-request ceiling
for one account. Selection **enforces** it: an account already holding `N`
live spawns is skipped in favour of another eligible account, and becomes
selectable again the moment one of those spawns exits — no operator action,
and no cooldown. A pool whose every account is at its cap fails closed at `78`
with a diagnostic naming the cap. An account with no declared cap is
unbounded, exactly as before #8424.

In-flight holds are counted as one lease file per live spawn under
`<provider>/.inflight/<account>/`, keyed on the spawning PID — which, because
the spawn `exec`s, *is* the harness's PID for the whole run, so a lease is
released by the process dying and there is nothing to forget. Stale leases are
reaped on read (dead PID, or older than `LOOM_API_KEY_INFLIGHT_STALE_SECS`,
default 4h). Unlike `.disabled`/`.bad_accounts.json`/`.limits.json` — which
encode operator decisions and therefore fail *closed* — an unusable lease
store degrades **open** and the spawn proceeds uncapped: a broken counter
directory must not become an outage.

**Health.** `loom-daemon api-keys health [--provider zai] [--json]` reports,
per provider: total/selectable/disabled/malformed/exhausted/at-concurrency-cap/
unverifiable counts, any `.allowlist` pin, accounts whose file permissions are
looser than `0600`, and any provider directory that cannot be read (non-zero
exit) — secret-free by construction, the same as `list`.

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

## Backstopping a trial tap behind the subscriptions (#8436)

A trial profile is usually a *second* way to reach a model, not a replacement
for the Claude/Codex seats already paid for. `runtimes.preference` orders those
taps and falls through only when a higher one has nothing spawnable, so a metered
endpoint stays a backstop rather than becoming the default:

```jsonc
"runtimes": {
  "preference": ["claude", "codex", {"runtime": "opencode", "modelProfile": "zai-metered"}]
}
```

Three things to know before pointing a trial at this:

- **A tap is (runtime, credential source), not a runtime id.** The same model is
  reachable through a flat-rate coding-plan subscription and through a metered
  serverless endpoint, under different provider ids. Name the `modelProfile` when
  the distinction matters — that is what makes "which tap did this run use"
  answerable from the launch record.
- **Exhaustion means different things per tap.** A flat-rate tap exhausts on plan
  limits and recovers on a clock, which is what the #8401 API-key pool's bad-mark
  model describes. A metered tap effectively never exhausts; its limiter is a
  **spend ceiling**, and it must not be modelled as a cooldown. A harness cost
  estimate is at least directionally meaningful for a metered tap and is not a
  charge at all for a flat-rate one.
- **Codex is not admitted for Builder/Doctor**
  (`worktreeIsolation: "partial"`), so a build-role chain is effectively
  `claude → <native tap>` whatever the list says; and because a native sweep runs
  every phase in one session with no subagents, set `rolePreference.judge` to keep
  review off the tap that produced the change.

Full semantics, the operator-pin rule, and the
`# LOOM_RUNTIME_PREFERENCE` observability marker: `runtime-adapters.md` §
"Ordered runtime preference with fall-through". Dispatch is not yet wired to the
resolver — see the follow-up issues on #8436.

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
