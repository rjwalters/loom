# Unified Hyperparameters

**Issue #9683.** One typed, validated surface for Loom's operational
tunables — the knobs that govern dispatch cadence, concurrency, lease
lifetimes, and review-debt backoff — with run-level provenance (a digest
stamped onto every telemetry span) and a programmatic override vector for
external optimizers (CMA-ES, Bayesian search).

Implementation: `loom-daemon/src/hyperparams.rs`. Consumers overlay their
legacy config readers (`work_finder/config.rs`, `work_finder/build_backoff.rs`,
`idle_exit.rs`, `claim_reconciliation.rs`) — the legacy homes keep working.

## The config block

Live in `.loom/config.json` under the top-level `"hyperparameters"` key
(same tier-merge resolution as every other config key: machine defaults →
host config → repo config — see `config_resolver.rs`).

```json
{
  "hyperparameters": {
    "dispatch": {
      "tickIntervalSecs": 60,
      "maxConcurrent": 3,
      "maxAdmissionsPerTick": 3
    },
    "lifecycle": {
      "leaseTtlMinutes": 15,
      "idleExitMinutes": 60
    },
    "rework": {
      "buildBackoffHigh": 40,
      "buildBackoffLow": 25
    }
  }
}
```

### Fields

Every field is optional; absent fields fall through the precedence chain
below. Types/ranges are strict **on this surface** (see Validation).

| Group | Key | Default | Range | Governs | Legacy home | Single-knob env |
|---|---|---|---|---|---|---|
| `dispatch` | `tickIntervalSecs` | 60 | 5–3600 | Work-finder tick cadence | `autonomous.workFinder.intervalSecs` | `LOOM_WORK_FINDER_INTERVAL_SECS` |
| `dispatch` | `maxConcurrent` | 3 | 1–256 | Sweep concurrency ceiling (dynamic cap stays bounded by disk/RAM) | `autonomous.workFinder.maxConcurrent` | `LOOM_WORK_FINDER_MAX_CONCURRENT` |
| `dispatch` | `maxAdmissionsPerTick` | 3 | 1–64 | Ramp cap: new sweeps admitted per tick (#4234) | `autonomous.workFinder.maxAdmissionsPerTick` | `LOOM_WORK_FINDER_MAX_ADMISSIONS_PER_TICK` |
| `lifecycle` | `leaseTtlMinutes` | 15.0 | >0–1440 | Lease-freshness TTL (#6286) | — | `LOOM_LEASE_TTL_MINUTES` |
| `lifecycle` | `idleExitMinutes` | 60 | 1–10080 | Idle-exit turnaround (#4467) | `autonomous.idleExit.idleMinutes` | `LOOM_AUTONOMOUS_IDLE_EXIT_MINUTES` |
| `rework` | `buildBackoffHigh` | 40 | 1–100000 | PR-debt level that engages the build back-off (#9410) | `autonomous.workFinder.buildBackoff.high` | — |
| `rework` | `buildBackoffLow` | 25 | 0–100000, `< high` | PR-debt level that releases it | `autonomous.workFinder.buildBackoff.low` | — |
| `supervision` | `epicSupervisorIntervalSecs` | 300 | 30–3600 | Epic-supervisor tick cadence | — | `LOOM_EPIC_SUPERVISOR_INTERVAL_SECS` |
| `supervision` | `epicInflightTtlSecs` | 900 | 60–86400 | Epic-supervisor inflight-record freshness | — | `LOOM_EPIC_INFLIGHT_TTL_SECS` |
| `supervision` | `sweepReaperIntervalSecs` | 30 | 5–3600 | Sweep-registry reaper tick cadence | — | `LOOM_SWEEP_REAPER_INTERVAL_SECS` |
| `supervision` | `reapGhTimeoutSecs` | 5 | 1–600 | One reaper `gh api` call budget | — | `LOOM_REAP_GH_TIMEOUT_SECS` |
| `supervision` | `sweepInflightStaleSecs` | 14400 | 60–604800 | Sweep inflight-record staleness | — | `LOOM_INFLIGHT_STALE_SECS` |
| `supervision` | `apiKeyInflightStaleSecs` | 14400 | 60–604800 | API-key pool inflight staleness | — | `LOOM_API_KEY_INFLIGHT_STALE_SECS` |
| `supervision` | `tokenExhaustionCooldownSecs` | 21600 | 60–604800 | Exhausted-token cooldown before re-probe | — | `LOOM_TOKEN_EXHAUSTION_COOLDOWN_SECS` |
| `supervision` | `badTokenCleanupMaxAgeSecs` | 86400 | 3600–2592000 | Age at which a cleaned bad-token record is deleted | — | — |
| `supervision` | `worktreeActivityWindowMinutes` | 30 | 1–1440 | Inactivity window the activity classifier treats as idle | — | `LOOM_WORKTREE_ACTIVITY_WINDOW_MINUTES` |
| `headroom` | `perWorktreeGb` | 2 | 1–1024 | Disk GB reserved per live worktree in admission accounting | — | `LOOM_PER_WORKTREE_GB` |
| `headroom` | `perWorktreeRamGb` | 2 | 1–1024 | RAM GB reserved per live worktree in admission accounting | — | `LOOM_PER_WORKTREE_RAM_GB` |
| `process` | `restartPollSecs` | 30 | 5–3600 | Restart-verification poll cadence | — | `LOOM_DAEMON_RESTART_POLL_SECS` |
| `process` | `restartKickstartPollSecs` | 15 | 1–600 | Kickstart poll cadence while a restart recovers | — | `LOOM_DAEMON_RESTART_KICKSTART_POLL_SECS` |
| `process` | `restartPollIntervalMs` | 1000 | 50–60000 | Fast in-process restart poll interval (ms) | — | `LOOM_DAEMON_RESTART_POLL_INTERVAL` |
| `process` | `bootoutSettleSecs` | 5 | 1–120 | launchd settle wait after bootout | — | `LOOM_DAEMON_BOOTOUT_SETTLE_SECS` |
| `process` | `bootstrapRetryAttempts` | 4 | 1–20 | launchd re-bootstrap attempts | — | `LOOM_DAEMON_BOOTSTRAP_RETRY_ATTEMPTS` |
| `process` | `bootstrapRetrySecs` | 2 | 1–60 | launchd re-bootstrap retry spacing | — | `LOOM_DAEMON_BOOTSTRAP_RETRY_SECS` |
| `process` | `ipcTimeoutMs` | 30000 | 1000–3600000 | Raise-only floor on client-side daemon IPC round-trips | — | `LOOM_DAEMON_IPC_TIMEOUT_MS` |
| `process` | `leaseGuardTimeoutSecs` | 10 | 1–600 | Lease co-occupancy guard's one `gh api` read budget | — | `LOOM_WORKTREE_LEASE_GUARD_TIMEOUT` |
| `observability` | `dispatchDispositionRefreshSecs` | 600 | 30–86400 | Dispatch-disposition view refresh cadence | — | `LOOM_DISPATCH_DISPOSITION_REFRESH_SECS` |
| `observability` | `queueStarvationSecs` | 21600 | 300–604800 | Unclaimed age that flags queue starvation | — | `LOOM_QUEUE_STARVATION_SECS` |
| `update` | `staleWarnCommits` | 10 | 1–100000 | Commits behind that trips the stale-install warning | — | `LOOM_SELF_UPDATE_STALE_WARN_COMMITS` |
| `update` | `staleWarnHours` | 12 | 1–720 | Hours stale that trips the same warning | — | `LOOM_SELF_UPDATE_STALE_WARN_HOURS` |

Tranche 2 notes:

- The `supervision` / `headroom` / `process` / `observability` / `update`
  groups consolidate knobs that were previously **env-only** (a `LOOM_*` var
  plus a built-in constant, no config tier). Their single-knob env vars keep
  working, above the layer, exactly as before.
- The layer tier for these fields is **startup-anchored** (the daemon's
  workspace root): it applies inside the daemon, and CLI-only code paths
  (e.g. `loom-daemon dispatch`'s IPC budget) read env > default as before —
  `loom-daemon hyperparams` still resolves and validates the full vector from
  any checkout.
- `process.ipcTimeoutMs` is **raise-only**, like its env var: a configured
  value can only widen a client's IPC budget above its per-command floor,
  never shorten it.
- `sweep-lease-publish.sh` reads `lifecycle.leaseTtlMinutes` from the
  committed block as a fallback (env > block > 15) so the shell-side
  lease guard agrees with the daemon; the `$LOOM_HYPERPARAMS` vector tier is
  daemon-only and invisible to shell — export `LOOM_LEASE_TTL_MINUTES`
  alongside a vector that tunes the lease TTL.

### Deliberately NOT moved onto this surface

- **Bool feature toggles** (`LOOM_QUARANTINE_RECONCILE`,
  `LOOM_MERGE_SEQUENCE_RECONCILE`, `LOOM_REVIEW_CONFLICT_RECONCILE`,
  `LOOM_VERDICT_TREE_CARVEOUT`, `LOOM_EPIC_SUPERVISOR`,
  `LOOM_DAEMON_RESTART_VERIFY`, `LOOM_PROFILE_PROVISION_ON_START`) — these
  are opt-in operational switches a human flips during incident response,
  not coordinates an optimizer samples.
- **Path/binary overrides** (`LOOM_PID_FILE`, `LOOM_INFLIGHT_DIR`,
  `LOOM_SHARED_TOKENS_DIR`, `LOOM_SWEEP_SPAWN_BIN`, `LOOM_DOCKER_BIN`,
  journal/snapshot path vars, tool-home discovery like `LOOM_CODEX_HOME`) —
  host-local by nature; a committed, fleet-shared value would be actively
  wrong.
- **Process/IPC markers** (`LOOM_SWEEP_ID`, `LOOM_TRACEPARENT`,
  `LOOM_PROVENANCE_*`, claim-owned markers) — inter-process communication,
  not configuration.
- **Page sizes and similarity heuristics in scripts**
  (`check-duplicate.sh`'s forge page limits and `threshold=18`,
  `run-job.sh`'s SSH `ConnectTimeout` — already reachable via
  `SSH_OPTS_RAW`), and the remaining bare constants in diagnostic probes
  (`foreign_load.rs`'s 3s probe timeout / top-3 report): real values, but
  not operator-tuning surface — promoting them would be ratchet-bait, not
  leverage.

## Precedence

```
single-knob env var                      (one-off operator override, one run)
  > $LOOM_HYPERPARAMS vector             (JSON, programmatic — optimizer loops)
  > "hyperparameters" config block       (committed, validated, canonical)
  > legacy autonomous.* config key       (still honored; block wins where both set)
  > built-in default                     (the constant each knob used before)
```

`$LOOM_HYPERPARAMS` is a JSON object whose keys may be nested
(`{"dispatch":{"maxConcurrent":6}}`) or flat dotted
(`{"dispatch.maxConcurrent":6}`). It requires no file edit, so an optimizer
loop can tune a whole run with one env export.

Notes:

- Every tranche-1 field **hot-applies** (#9768): the work-finder trio, idle
  exit, and the backoff pair re-read config every tick, and the lease TTL
  re-resolves from the layer on every lease check — a committed-block edit
  lands without a daemon restart. (The single-knob env vars are process
  environment: fixed at launch, as always.)
- A legacy key and a block key may coexist; the block wins per field, the
  legacy key fills fields the block omits.

## Validation (fail fast at startup)

`hyperparams::startup_init` runs once at daemon startup (before any span
exists) and **aborts startup** when the hyperparameters surface — the
committed block and/or the env vector — has:

- an unknown group or unknown key inside a known group (typo catcher),
- a wrongly-typed value, or a value outside its documented range,
- a crossed `rework` pair (`buildBackoffLow >= buildBackoffHigh`).

The error names every offending dotted path, so an optimizer that samples an
invalid vector learns exactly which coordinate was rejected. An unparseable
`$LOOM_HYPERPARAMS` is likewise fatal at startup.

Legacy `autonomous.*` values are **never** gated by this — those keys keep
their own documented soft-fallback semantics (e.g. a crossed legacy backoff
pair falls back to 40/25 with a warning), so an existing committed config
can never start failing this gate after an upgrade.

## Run provenance

After validation, `startup_init` resolves the effective vector down the full
precedence chain and records a digest — `sha256:<hex>` over the vector's
canonical JSON — in a process global. The trace provenance stamper
(`telemetry/trace/provenance.rs`) then writes it as
**`loom.hyperparams.digest`** on every `loom.*` span, alongside
`loom.daemon.revision` and `loom.prompts.digest` (policy:
[trace-identity](trace-identity.md)). A run's telemetry is therefore
reproducible from the exact hyperparameter vector it ran under: re-set the
same `$LOOM_HYPERPARAMS` (or config block), get the same digest.

The digest covers the whole resolved vector including inherited defaults —
two runs with identical digests ran identical tunables, even if they got
there by different tiers.

## Inspecting & validating: `loom-daemon hyperparams`

```
$ loom-daemon hyperparams              # human-readable table + digest
$ loom-daemon hyperparams --json       # {"params":…, "sources":…, "digest":…}
$ loom-daemon hyperparams --validate   # run the startup gate without a daemon
```

`sources` reports, per field, which tier supplied it
(`env-vector | config | legacy | default`) — the first thing to check when
an injected vector "didn't take".

`--validate` runs the same strict gate daemon startup enforces (unknown
keys, types, ranges, crossed backoff pair, unparseable vector) against a
workspace **without booting one** — a config lint for a proposed
`.loom/config.json` edit or `$LOOM_HYPERPARAMS` vector. Exit 0 and
`hyperparams: OK` when valid; non-zero naming every offending path
otherwise. Combine with `--json` for a machine-readable violations array
(#9768).

## Optimizer recipe (CMA-ES)

```bash
# 1. Baseline
digest=$(loom-daemon hyperparams --json | jq -r .digest)

# 2. Sample a candidate vector and run the fleet under it
export LOOM_HYPERPARAMS='{"dispatch.maxConcurrent": 5, "rework.buildBackoffHigh": 55}'
restart-daemon                        # startup validates; invalid samples fail fast

# 3. Confirm the injection took
loom-daemon hyperparams --json | jq '.sources["dispatch.maxConcurrent"]'  # env-vector

# 4. Attribute outcomes by digest: every span of the run carries
#    loom.hyperparams.digest — group cycle-time / token-efficiency metrics on it.
```

Invalid samples abort startup with the offending path named — treat a
non-booting daemon as an infeasible point, not a crash.

## Tranche roadmap

Tranche 1 consolidated the seven dispatch/lifecycle/rework fields. Tranche 2
consolidated the env-only janitor/lifecycle numeric knobs into the
`supervision`, `headroom`, `process`, `observability` and `update` groups
(table above). Still on the roadmap: knobs that already have legacy
`autonomous.*` config homes — host-breaker thresholds, admission-brake load,
merge-train bounds, role execution budgets — migrate onto the same surface in
a later tranche (they are already settable per-repo today, so the move is
provenance/optimizer surface, not new capability).
Adding a field is: one struct entry + range check in `hyperparams.rs`, one
consumer overlay, one row in the table above.
