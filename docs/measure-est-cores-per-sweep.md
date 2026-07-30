# Measuring `LOOM_EST_CORES_PER_SWEEP` (HISTORICAL — the knob it tunes was retired in #4512)

> **⚠️ This document is historical. Do not follow it as current guidance.**
>
> **#4512 deleted the CPU-headroom term from the daemon's admission formula.**
> `resolve_dynamic_max_concurrent` is now `min(token axis, disk headroom,
> configured maxConcurrent)` — there is no CPU estimate in it, and therefore
> nothing for `LOOM_EST_CORES_PER_SWEEP` (or `LOOM_CPU_UTILIZATION_TARGET`) to
> tune. Both knobs, and their `autonomous.estCoresPerSweep` /
> `autonomous.cpuUtilizationTarget` config twins, are **accepted but ignored**;
> the daemon logs one deprecation warning per process
> (`work_finder::warn_deprecated_cpu_knobs`) naming any that are still set.
> Measuring a value and setting it will change nothing.
>
> **What to do instead:**
>
> 1. **Tune `autonomous.workFinder.maxConcurrent` empirically.** It is the one
>    per-machine admission knob. Run `loom-daemon calibrate` (measurement-only
>    since #4512) or read `loom-daemon status`: if the cap binds on `ceiling`
>    while the host sits idle, raise it; if it binds on `token`/`disk`, raising
>    it changes nothing.
> 2. **Rely on the machine-wide build slot for the heavy stages.** `cargo
>    build`/`test` and the build gate take a `mkdir`-atomic slot
>    (`LOOM_BUILD_SLOTS`, default 1) so at most one or two builds run
>    concurrently on a host regardless of how many sweeps are admitted. This is
>    what the per-sweep core estimate was a (poor) proxy for.
> 3. **Trust the host-distress circuit breaker (#4235)** as the safety net — it
>    trips on *measured* load-per-core rather than an estimated constant.
>
> See `defaults/docs/daemon-reference.md` → *Why there is no CPU term in
> admission (#4512)* and *Machine-wide build slot (#4512)*.
>
> The rest of this file is retained only as a record of how the retired constant
> was intended to be measured, and because the marginal-cores-per-sweep
> methodology is still a reasonable way to characterize a host before picking a
> `maxConcurrent`.

`LOOM_EST_CORES_PER_SWEEP` was the one hand-tuned constant left in the daemon's
CPU-headroom term (`loom-daemon/src/cpu_headroom.rs`). It estimates how many CPU
cores a **single** concurrent sweep consumes while its build/test step is
running. The headroom term is:

```
cpu_headroom = max(1, floor((logical_cpus × LOOM_CPU_UTILIZATION_TARGET − consumed_cores) / LOOM_EST_CORES_PER_SWEEP))
```

Since #4031 the `consumed_cores` half is **measured continuously at runtime**
(idle fraction → `logical_cpus × (1 − idle_fraction)`), so only this per-sweep
divisor remains a guess. Its default is `2.0` (a conservative estimate for a
Rust-heavy repo where `cargo build`/`clippy` parallelize rustc codegen across a
few cores). **Do not change the default without a measurement** — replacing one
guessed constant with a differently-guessed one is not an improvement.

This document is that measurement recipe. It is a per-host / per-repo property,
so it must be run on a real host running real sweeps — which a single Builder
worktree cannot provision, hence the recipe rather than a baked-in number.

## Recipe: marginal cores per concurrent sweep

The quantity we want is the **marginal** CPU consumption added by one more
concurrent sweep — not the whole-host load. Measure it as the slope of consumed
cores vs. concurrent-sweep count.

### 1. Establish the idle baseline

With **no sweeps running**, sample consumed cores over ~60s:

```bash
# Linux: idle fraction from /proc/stat delta
awk '/^cpu /{i=$5+$6; t=0; for(n=2;n<=NF;n++)t+=$n; print i, t}' /proc/stat
sleep 60
awk '/^cpu /{i=$5+$6; t=0; for(n=2;n<=NF;n++)t+=$n; print i, t}' /proc/stat
# consumed_cores = ncpu × (1 − (idle2-idle1)/(total2-total1))

# macOS: second (delta) line of iostat; `id` is idle %
iostat -c 2 -w 60 -n 0   # take the SECOND data line's `id` column
# consumed_cores = ncpu × (1 − id/100),  ncpu = sysctl -n hw.logicalcpu
```

Call this `C0` (cores consumed by the OS + daemon at rest).

### 2. Sample under a known concurrent-sweep count

Drive the daemon to a **fixed** number of concurrent sweeps `N` (e.g. via
`mcp__loom__dispatch_sweep` for N distinct issues, or set
`autonomous.workFinder.maxConcurrent = N` with a backlog of ≥ N ready
`loom:issue` items and let the work finder fill it). Confirm the live count:

```bash
loom-daemon status --json | jq '.in_flight_count'
```

Wait until all N sweeps are in their **build/test** phase (the CPU-heavy step —
this is what the term protects), then sample consumed cores again over ~60s
using the same commands as step 1. Call this `C_N`.

### 3. Compute the marginal cost

```
LOOM_EST_CORES_PER_SWEEP ≈ (C_N − C0) / N
```

Repeat for a couple of values of `N` (e.g. 2 and 4) and confirm the slope is
roughly linear; use the average. Sampling while sweeps are between build steps
(idle, waiting on the model) will understate the value — measure during peak
build concurrency, which is the moment the term exists to guard against.

### 4. Apply

Set the measured value with precedence **env > `.loom/config.json` >
default** (#4032) — pick whichever durability you want:

```bash
# Ephemeral (this shell's daemon runs only): env wins over any committed config.
export LOOM_EST_CORES_PER_SWEEP=<measured>
# then restart the daemon, or bake it into the daemon's environment

# Durable, shared with the team, and survives loom-daemon-start.sh re-rendering
# the plist on every start (unlike an env var, which only persists as long as
# every start exports it):
```
```json
{
  "autonomous": {
    "estCoresPerSweep": <measured>
  }
}
```

Cross-check against `loom-daemon status`, whose `cpu headroom` line now reports
the measured idle fraction and consumed-core estimate feeding the term.

## Notes

- **`LOOM_CPU_UTILIZATION_TARGET`** (default `0.75`) is a policy knob — the
  fraction of cores you are *willing* to dedicate to sweeps — not a measurement.
  Leave headroom for the OS, the daemon, and the build-gate's own `cargo`. It
  has the same committed-config counterpart, `autonomous.cpuUtilizationTarget`.
- Both knobs are resolved **once at daemon startup**, single-root (not
  per-workspace) — see the `autonomous.*` config surface in
  [`.loom/docs/daemon-reference.md`](../.loom/docs/daemon-reference.md)
  (#4032).
