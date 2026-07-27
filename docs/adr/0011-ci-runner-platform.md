# ADR-0011: CI Runner Platform — Speedup Ceiling and Decision

## Status

Accepted

## Context

Issue #4038 was filed to evaluate standing up a dedicated CI runner (a
shared AWS host, provisioned across the project fleet) on two distinct
grounds: **speed** (`cargo test --workspace` on `ubuntu-latest` is the
critical path of `ci.yml`, and more cores / a warm `target/` should cut
it) and **platform parity** (Loom runs in production on aarch64 macOS;
CI has only ever tested x86-64 Linux, and macOS-specific defects — the
tmux-dependent tests excluded from the local gate in #3985, and
Gatekeeper/`syspolicyd` saturation under parallel `cargo` builds — are
invisible to CI by construction).

The acceptance criteria for *this* issue were narrowly scoped to
measurement and a decision record, not provisioning: measure the
speedup ceiling before any hardware is bought, and record whether the
ceiling justifies new hardware. Provisioning itself — Terraform, AWS
spend, ephemeral-runner isolation, per-repo concurrency limits — is
split to the sibling issue #4057 (`loom:operator-only`), which depends
on the decision recorded here.

### Measured breakdown of `cargo test --workspace` (curator pass, CI run `30292022378`, `main`, green)

| Segment | Duration | % of step | Does a bigger runner help? |
|---|---|---|---|
| Compile (`Finished \`test\` profile ... in 19.52s`) | 19.5s | 10.3% | Yes — but only 10% |
| `loom_api` lib (4 tests) | 0.2s | 0.1% | n/a |
| `loom_daemon` lib — 1000 tests | 126.8s | 66.7% | Only for its non-serial fraction (unmeasured at curator time) |
| `loom_daemon` bin — 10 tests | 5.5s | 2.9% | Partially |
| `integration_basic` — 9 tests | 14.9s | 7.8% | No — sleep/timing-bound |
| `integration_factory_reset` — 2 tests | 15.9s | 8.4% | No — sleep/timing-bound |
| `integration_security` — 14 tests | 3.9s | 2.1% | Partially |
| `integration_singleton_guard` — 2 tests | 2.3s | 1.2% | No — timing-bound |
| doc-lint + conformance binaries (34 tests) | <0.5s | 0.2% | n/a |
| **Step total** | **190s** | 100% | |

Job total was 213s (the extra ~17s is `Swatinem/rust-cache` restore).
`cargo test` runs each test binary strictly sequentially — the
`Running …` timestamps in the log form a contiguous chain with zero
cross-binary overlap, so there is speedup available today with **no
new hardware** by parallelizing across binaries (see "Cheaper
alternatives" below).

Also verified against `origin/main`: **140** `#[serial]`/
`#[serial_test::serial]` annotations exist repo-wide (up from the
original issue's estimate of 93, which predated #4051's
`config_resolver.rs` additions and missed the `#[serial_test::serial]`
spelling). None use a named key — all 140 contend on
`serial_test`'s single default global lock, which is in-process and
therefore serializes per test binary, not repo-wide. **111** of the 140
live in the `loom_daemon` **lib** binary — the 126.8s line above.

### New measurement: the lib binary's serial-vs-parallel split

The one number the curator pass could not derive from existing logs —
how much of the `loom_daemon` lib binary's 126.8s is the 111-test
serial chain versus the ~889 parallel tests — was measured directly in
CI. Two temporary steps were added to the `backend-tests` job on
`ubuntu-latest` (4 vCPU), run after the normal `cargo test --workspace`
step so `rust-cache` was already warm:

```bash
# Default parallelism (the existing 126.8s baseline, re-measured)
time cargo test --package loom-daemon --lib
# Fully sequential — the ceiling if de-serialization achieved nothing
time cargo test --package loom-daemon --lib -- --test-threads=1
```

Run [`30294816703`](https://github.com/rjwalters/loom/actions/runs/30294816703)
(job `90073172969`), on commit `90fa2a4e`, conclusion `success`:

| Measurement | Wall time |
|---|---|
| Default parallelism (`cargo test --package loom-daemon --lib`) | **127.24s** |
| Fully serial (`--test-threads=1`) | **153.25s** |
| Delta (parallel headroom at 4 vCPUs) | **26.01s** (~17% of the serial figure) |

The re-measured default-parallelism figure (127.24s) matches the
curator's earlier-recovered 126.8s baseline within measurement noise,
confirming the measurement is stable and not an artifact of one run.
Compile time on this run was logged twice at 17.98s and 19.31s — both
close to the original 19.52s baseline, confirming `rust-cache` hit
(warm cache) and that the "compile is only ~10%" finding is **not** a
cold-cache artifact.

### The inference this measurement forces

Forcing full serialization only adds ~26s over default 4-way
parallelism. That means the ~889 non-`#[serial]` tests are **already
almost entirely hidden** behind the 111-test serial lock chain at just
4 vCPUs — while the serial chain runs on one thread, the other threads
finish the parallel tests well within that window. Consequences:

- The serial chain's own duration is approximately the entire
  default-parallelism figure, **~127s**, and that duration is **not
  reducible by adding cores**: a global mutex forces 111 tests to run
  one at a time regardless of how many cores are available.
- The 889 parallel tests cost only ~26s of aggregate wall time beyond
  the serial floor, and are already fully parallelized away today at 4
  vCPUs — more cores cannot meaningfully shrink an already-near-zero
  marginal cost.
- Extending this to the whole `cargo test --workspace` step (~190–213s
  total): compile (~19s, warm cache) is the only clearly
  core-addressable segment; the ~33s of sleep/timing-bound integration
  tests (`integration_basic`, `integration_factory_reset`,
  `integration_singleton_guard`) are wall-clock fixed regardless of
  cores; and the lib binary's ~127s is serial-lock-bound per above.

### Numeric speedup ceiling

**An infinite-core runner cannot take the `loom_daemon` lib test binary
below ~127s**, because that duration is the serial-lock chain's own
wall time, not a function of available parallelism. Extending to the
whole `ci.yml` critical path: the only segment a bigger/faster runner
can meaningfully compress is the ~19s compile step (and, marginally,
the non-serial-bound bin/`integration_security` fractions, on the
order of a few seconds). So **cores/hardware alone cannot take
`ci.yml`'s critical path below roughly S ≈ 190–200s** out of the
current ~213s job total — an at-most ~10–15s (5–7%) win, not the
"more cores solve it" story the original issue's speed argument
assumed.

## Decision

**(a) The speed case does not justify new hardware on its own.**
The measured ceiling (~5–7% off the job total) is too small to justify
provisioning, maintaining, and securing a dedicated AWS host. Chasing
that number with hardware would spend real recurring cost and
operational complexity (ephemeral-runner isolation for a public repo,
per-repo concurrency limits, a shared cache layer) to shave single-digit
seconds off a ~213s job. **The actual high-leverage fix is
de-serializing the `#[serial]` tests**, not adding cores — see (c)
below.

**(b) aarch64-Linux (Graviton) parity is judged sufficient for the
"custom runner" question; full macOS parity is out of scope here.**
The original issue's own tradeoff table lists Graviton (arch parity,
not OS parity) as cheap and GitHub-hosted macOS runners / EC2 macOS
dedicated hosts as expensive (EC2 macOS carries a 24-hour minimum
billing floor per dedicated host). Since this ADR has just eliminated
the speed justification, the platform-parity argument would need to
carry a runner decision on its own — and full macOS parity's cost
profile (dedicated-host billing floor, ephemeral-isolation design for
a *macOS* fleet) is a materially bigger and separately-costed
commitment than a Linux/Graviton job. Reasoning it through: the
concrete macOS-specific defects already surfaced (#3985's
tmux-dependent test exclusion, `syspolicyd`/Gatekeeper saturation under
parallel `cargo` builds) are real correctness gaps, but they are gaps
in *macOS-specific* behavior — an aarch64-**Linux** runner closes zero
of them, because Gatekeeper and tmux-on-macOS are OS-level, not
architecture-level, concerns. So Graviton buys architecture parity
(useful for catching aarch64-specific bugs in, e.g., pointer-width or
endianness-sensitive code, though none are known today) without buying
the OS parity that actually matters for the two concrete defects on
record. **Decision: if a runner is provisioned at all (per #4057), it
should default to Graviton for cost/architecture reasons, but full
macOS parity — if pursued — must be justified and costed as its own,
separate decision**, not bundled into this one. This ADR does not
itself decide to pursue macOS parity; it only says the two questions
(Linux/Graviton vs. macOS) are not equivalent and should not be
conflated.

**(c) Cheaper alternatives — accept or reject:**

1. **Parallelize across test binaries (`cargo-nextest` or equivalent).**
   **Reject for `#[serial]`-heavy binaries without further work; worth
   a narrow follow-up evaluation, not a blanket adoption.** nextest
   runs each test in its own **process**, but `serial_test`'s lock is
   **in-process** — process-per-test provides no mutual exclusion
   across the 140 `#[serial]` tests that currently rely on one shared
   in-process lock. Adopting nextest as-is would silently drop the
   isolation these tests depend on (most were marked `#[serial]`
   specifically because they mutate process-global env vars or shared
   state), which is exactly the kind of flakiness that would poison
   #3974's forge-CI green-verdict corroboration. nextest could still be
   adopted for the ~43s currently-sequential small-binary tail
   (`integration_basic`, `integration_factory_reset`,
   `integration_singleton_guard`, the bin binary, doc-lint binaries) —
   none of which share `#[serial]` state across binaries — but that is
   a scoped follow-up, not a drop-in fix for the 126.8s lib binary.
2. **De-serialize the env-var tests.** **Accept as the primary
   follow-up.** Most of the 111 `#[serial]` tests in the lib binary are
   `#[serial]` only because they mutate process-global env vars
   (`config_resolver.rs`, `main_health_gate.rs`, `work_finder.rs`,
   `watch_registry.rs`, `sweep_registry.rs`). Where a test's true
   dependency is "don't race on this specific env var / global," the
   fix is scoping the mutation (thread-local overrides, injected config
   structs, or a test harness that saves/restores per-test) rather than
   a repo-wide mutex. This directly attacks the ~127s serial-lock floor
   that cores cannot touch.
3. **Named `#[serial(key)]` groups.** **Accept as a cheap incremental
   step alongside (2).** Tests that mutate genuinely disjoint globals
   (e.g. `sweep_registry`'s state vs. `watch_registry`'s state) can
   take disjoint keyed locks and run concurrently with each other while
   still serializing within their own group. This requires no
   test-isolation redesign, just an audit of which of the 111 tests
   share a *real* contended resource versus which only share the
   default lock by default. Lower ceiling-impact than (2) alone but
   composes with it.

None of (1)–(3) are implemented by this issue — this issue is a
measurement and a decision record. Acting on (c)(2)/(c)(3) is follow-up
work; the resulting reduction in the ~127s serial floor is a
prerequisite for revisiting whether new hardware becomes worthwhile
later (a diminished serial floor would make the parallel-tail argument
in (a) more favorable to cores, if it is ever revisited).

**Cross-reference:** #4057 (operator-only, shared AWS host
provisioning across loom/anvil/repo/kicad-tools/vibesql/geode-fem)
depends on this ADR's decision. Given (a) and (b) above, #4057 should
not proceed on the speed justification alone; if it proceeds, it
should default to Graviton and treat macOS parity as a separate,
independently-costed decision, not a bundled requirement.

## Consequences

### Positive

- The speedup ceiling is now a measured number (~5–7% off the job
  total from hardware alone), not a guess — #4057 can be evaluated
  against evidence instead of the "more cores will help" assumption the
  original issue's speed argument rested on.
- The actual high-leverage lever (de-serializing the 111-test lock
  chain) is now identified and prioritized ahead of any hardware spend.
- The platform-parity argument is preserved on its own merits (it was
  never a speed argument) and is explicitly decoupled from the
  now-collapsed speed case, so it isn't retired by association.
- #4057 gets an explicit, evidence-backed default (Graviton) instead of
  an open architecture/OS question.

### Negative

- The ~127s serial-lock floor remains in `ci.yml` until the
  de-serialization follow-up (accepted alternatives (c)(2)/(c)(3)) is
  actually done — this ADR does not itself shrink CI wall time.
- `cargo-nextest` adoption is deferred rather than resolved; a future
  attempt still needs to work out per-test-binary lock semantics before
  it can safely include the lib binary, so the ~43s small-binary tail
  savings are not captured by this issue either.
- If #4057 or a future issue decides macOS parity is worth pursuing
  despite the cost profile, that decision will need its own ADR/cost
  case — this ADR deliberately does not pre-answer it.

## Alternatives Considered

**Size the runner for the full 210s job total, assuming linear
core scaling**

Rejected. This was the original issue's implicit assumption. The
serial-vs-parallel split measured here shows the assumption is false
for the single largest segment (66.7% of the step): a global
in-process mutex does not parallelize regardless of core count. Sizing
a runner against a linear-scaling assumption would have bought orders
of magnitude less benefit than expected.

**Decide the runner-platform question without the serial-split
measurement (act on the curator's earlier table alone)**

Rejected. The curator's table left exactly one number unknown — the
serial-vs-parallel split inside the largest segment — and explicitly
flagged that this was the number that decides the hardware question
("if the serial chain dominates → more cores buy almost nothing").
Deciding without it would have been guessing on the load-bearing
number. This ADR exists specifically to close that gap before #4057
proceeds.

**Bundle macOS parity into this ADR's decision**

Rejected. Full macOS parity (GitHub-hosted macOS runners or EC2 macOS
dedicated hosts) has a materially different cost profile — most
notably EC2 macOS's 24-hour dedicated-host billing floor — from a
Linux/Graviton job. Conflating the two would let a Linux-only decision
implicitly greenlight a much more expensive macOS commitment, or
conversely let macOS's cost concerns block a comparatively cheap
Graviton default. Keeping them separate lets #4057 make the Graviton
call now without waiting on a macOS cost case that has not been done.

## References

- Original issue: #4038 (this ADR's own tracking issue)
- Sibling issue (operator-only provisioning, depends on this decision): #4057
- Related: #4020 (build-gate concurrent sweep CPU contention — same
  underlying contention class, different consumer)
- Related: #3985 (scoped the local build gate to `--lib --bins`
  precisely because host-dependent targets measure the host, not the
  commit — the same macOS/host-shaped correctness gap this ADR's §(b)
  discusses)
- Related: #3974 (forge-CI green-verdict corroboration — the reason
  nextest's per-process `#[serial]` semantics gap in (c)(1) is a hard
  reject-without-further-work rather than a simple adoption)
- Related: #4051 (added 10 `#[serial]` tests to `config_resolver.rs`,
  explaining the drift between the original issue's count of 93 and
  the verified count of 140)
- Measurement source (curator per-segment breakdown): CI run
  [`30292022378`](https://github.com/rjwalters/loom/actions/runs/30292022378)
  (`main`, green)
- Measurement source (this ADR's serial-vs-parallel split): CI run
  [`30294816703`](https://github.com/rjwalters/loom/actions/runs/30294816703),
  job `90073172969`, commit `90fa2a4e`, conclusion `success` — the
  temporary instrumentation that produced this data was removed from
  `ci.yml` before merge; the ADR is the durable record of the result
