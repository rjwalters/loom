# Shrinking the `loom-daemon` lib-test compile unit (Issue #12134)

This is the design doc for #12134, a follow-up to #12089. #12089 found no
flag-level lever for the `loom-daemon` `cfg(test)` lib-test compile, which is
the serial critical path of every `Rust Unit Tests (k/5)` leg. This doc audits
where the test code lives, picks a migration path from that audit, projects the
saving, and sets out a PR sequence. It changes documentation only.

**Status: proposal.** No source, Cargo or CI file changes here. The child
issue(s) execute the migration.

**Source SHA.** Counts are against `bd80ef018` (`main`, 2026-10-15). Paths are
repo-relative. Re-run the commands in section 2 before relying on a number.

**Measured vs inferred.** Section 2 numbers are measured (`git ls-files`,
`git grep`, `wc -l`). The `safehouse` row was re-counted by step 0 (#12157) on
`df8089253`; the other rows are still the original counts. The compile-time
projection in section 4 was inferred from line counts. Step 0 (#12157) replaced
the `safehouse` part of it with a measurement, and found the extraction does
not compile without a base crate roughly the size of the whole daemon.

**Contents**

- [1. Baseline from #12089](#1-baseline-from-12089)
- [2. Audit](#2-audit)
- [3. What the audit rules out](#3-what-the-audit-rules-out)
- [4. Recommended path and projected saving](#4-recommended-path-and-projected-saving)
- [5. Incremental migration](#5-incremental-migration)
- [6. `build-gate.sh` fallback stays green](#6-build-gatesh-fallback-stays-green)
- [7. Rejected alternatives](#7-rejected-alternatives)

## 1. Baseline from #12089

Taken from the #12089 Phase 1 measurement (not re-measured here):

- `lib (test)` unit: about 87-100 s at low load, about 270 s per CI leg under
  load, repeated in all 5 legs (about 22 runner-min per PR).
- Frontend (`cargo check --profile test`) is about 55% of the unit; codegen and
  link are the other 45%.
- Debug-info and codegen-unit flags move the time by -4% / -2% median (noise).
- `-j4` to `-j15` saves about 8%. Both halves are serial rustc work that scales
  with the amount of code in the crate.
- 16.4M LLVM IR lines; functions in `tests` modules are 37.8% of them.
- The unit compiles about 663k non-test lines a second time plus about 395k
  lines of test code.

## 2. Audit

Commands run from the repo root. `T` is the set of test files:

```bash
T=$(git ls-files loom-daemon/src | grep -E '(^|/)(tests\.rs|[^/]*tests[^/]*\.rs)$')
```

| Measure | Command | Result |
|---|---|---|
| Tracked files in `loom-daemon/src` | `git ls-files loom-daemon/src \| wc -l` | 2,557 |
| Total `.rs` lines | `git ls-files 'loom-daemon/src/*.rs' \| xargs cat \| wc -l` | 1,059,021 |
| Top-level entries in `src/` | `ls loom-daemon/src \| wc -l` | 427 |
| Files containing `#[cfg(test)]` | `git grep -l '#\[cfg(test)\]' -- loom-daemon/src \| wc -l` | 1,198 |
| Dedicated test files (`T`) | `echo "$T" \| wc -l` | 747 |
| Lines in `T` | `echo "$T" \| xargs cat \| wc -l` | 311,562 |
| `pub(crate)` occurrences in `src` | `git grep -c 'pub(crate)' -- loom-daemon/src` (summed) | 2,983 |
| Existing integration targets | `ls loom-daemon/tests/*.rs \| wc -l` | 174 |
| `cfg(test)` in `main.rs` (the `--bins` unit) | `grep -c 'cfg(test)' loom-daemon/src/main.rs` | 0 |

The 311k lines in `T` are below the issue's ~395k test-line figure because the
remainder is inline `#[cfg(test)] mod tests { ... }` blocks inside non-test
files (1,180 non-test files contain a `cfg(test)` item).

### Classification of the 747 test files

| Class | Meaning | Test: `use super::` | Files | Lines |
|---|---|---|---|---|
| (a) | Public API only; movable to `loom-daemon/tests/` with no visibility change | no `super::`, no `crate::` | 3 | not separately counted (tiny) |
| (a') | Imports only via `crate::` paths; movable only if every imported item is `pub` and re-exported | no `super::`, has `crate::` | 13 | part of 4,181 |
| (b) | Reaches private / `pub(crate)` items of the parent module via `use super::*` | yes | 730 | 307,381 |
| (c) | Entangled with `cfg(test)` helpers in non-test code | not separable by grep | see below | see below |

```bash
echo "$T" | xargs grep -lE 'use super::|super::\*' | wc -l          # class b files: 730
echo "$T" | xargs grep -lE 'use super::|super::\*' | xargs cat | wc -l   # 307381
echo "$T" | xargs grep -LE 'use super::|super::\*' | wc -l         # a + a': 17
echo "$T" | xargs grep -LE 'use super::|super::\*' | xargs cat | wc -l  # 4181
echo "$T" | xargs grep -LE 'use super::|super::\*|use crate::|crate::' | wc -l  # a: 3
```

Class (c) is a lower bound only: 1,180 non-test files carry a `#[cfg(test)]`
item, which is the population where test helpers live inside production
modules. Splitting (b) from (c) needs a per-module read and is part of step 0.

**Result: 730 of 747 test files (98%, 307k of 311k lines) are class (b).**

### Subsystem extraction fan-in / fan-out

Fan-out is distinct `crate::<module>` roots a subsystem imports (excluding
itself); fan-in is files outside the subsystem that name it.

| Subsystem | Files | Lines | Test lines | Fan-out | Fan-in |
|---|---|---|---|---|---|
| `safehouse` | 11 | 11,283 | 5,736 | 21 | 13 |
| `ipc` | 19 | 11,045 | 6,885 | 33 | 47 |
| `merge_pr` | 126 | 35,645 | 16,290 | 22 | 32 |
| `tokens_pool` | 90 | 44,301 | 11,063 | 23 | 111 |
| `role_runner` | 57 | 22,789 | 8,113 | 49 | 53 |
| `work_finder` | 69 | 28,769 | 14,134 | 65 | 139 |
| `sweep_registry` | 176 | 82,661 | 32,302 | 81 | 164 |
| `observability` | 245 | 84,509 | 34,903 | 85 | 140 |

```bash
# per subsystem $m (directory or $m.rs):
git ls-files $p | xargs grep -ohE 'crate::[a-z_0-9]+' | sort -u | grep -v "crate::$m\$" | wc -l   # fan-out
git grep -lE "crate::$m\b" -- loom-daemon/src ':!'$p | wc -l                                         # fan-in
```

## 3. What the audit rules out

The issue's direction 1 (move `pub`-only tests to integration tests) covers 3
files, 16 at the very most. It cannot shrink the unit in any meaningful way.
Widening 2,983 `pub(crate)` items behind a `test-support` feature to move the
rest would also leave the code in one serial rustc unit for the non-test build
of the test crate, which is the cost being attacked.

That leaves crate extraction (directions 2 and 3). Tests move with the module
they test, so class (b) stops being a problem: `use super::*` keeps working
inside the new crate.

## 4. Recommended path and projected saving

**Hybrid, leaf-first extraction, ordered by fan-out.** Extract the subsystems
with the lowest fan-out first, since they need the fewest types moved down into
a shared base crate. Fan-out, not size, is the gating factor.

Order from the table: `safehouse` (12), `merge_pr` (22), `tokens_pool` (23),
`ipc` (33), `role_runner` (49), `work_finder` (65). `sweep_registry` and
`observability` are the largest but have the highest fan-out and are deferred
until the shared base crate exists.

Note that the issue lists `ipc`, `work_finder` and `role_runner` as
"self-contained". The fan-out numbers say otherwise: they import 33, 65 and 49
other top-level modules. Only `safehouse` is cheap to cut today.

### Projected saving (inferred, not measured)

Lines in the `lib (test)` unit leave it when their subsystem becomes its own
crate, because the lib-test unit of `loom-daemon` then links the extracted crate
as a normal (non-test) dependency. Its test and non-test lines both drop out of
`loom-daemon`'s unit and compile in a parallel unit.

| Extracted set | Lines leaving the `loom-daemon` unit | Share of ~1.06M |
|---|---|---|
| `safehouse` | 6.5k | 0.6% |
| + `merge_pr`, `tokens_pool`, `ipc` | 6.5k + 35.6k + 44.3k + 11.0k = 97.4k | 9% |
| + `role_runner`, `work_finder` | 97.4k + 22.8k + 28.8k = 149.0k | 14% |
| + `sweep_registry`, `observability` | 149.0k + 82.7k + 84.5k = 316.2k | 30% |

Assuming compile time scales linearly with lines (the #12089 observation that
both halves are serial and scale with code size), the unit shrinks from about
270 s per leg to roughly 235 s after the second tier and about 190 s after the
third, **if** the extracted crates compile in parallel with the remainder and
the remainder does not get slower. Two things push the real figure the other
way: each new crate adds its own link/metadata cost, and extracted crates that
depend on each other serialise. Treat the 30% row as an upper bound and the 9%
row as the first realistic target.

This estimate was the weakest number in the doc. Step 0 (#12157) measured the
parts it could; the result is below.

### Step 0 measurement (#12157, `df8089253`, `loom-worker-1`)

**Result: no-go for extracting `safehouse` ahead of a base crate. The
projection's premise (a leaf subsystem can leave the unit) does not hold.**

*Baseline, cold target dir, 16-core shared host at load about 4:*

```bash
cargo test -p loom-daemon --lib --no-run --timings -j2
```

| Figure | Value |
|---|---|
| `lib (test)` unit (`loom-daemon`) | 180.85 s |
| Whole build, wall clock (289 units, `-j2`) | 4 m 22 s |
| Peak RSS | 12.5 GB |
| `cargo nextest list -p loom-daemon --lib --profile ci --message-format json` test count | 17,220 (164 under `safehouse::`) |

*Why there is no "after" figure.* `safehouse` is not a leaf. The design's fan-out
of 12 was low: the §2 command gives 21 distinct `crate::` roots, 20 of them in
non-test code (`activity`, `cmd_out`, `config_resolver`, `event_bus`,
`filing_lock`, `gh_invocation`, `launch_record`, `loom_ui_alerts`,
`observability`, `peer_claims`, `rate_limit_breaker`, `script_helpers`,
`step_role`, `sweep_registry`, `transcript_tokens`, `types`, `usage_source`,
`workspace_pool`, `workspace_registry`, `worktree_ops`). Those roots are 254k
lines, led by `sweep_registry` (84k), `observability` (76k) and `worktree_ops`
(21k). Following `crate::` references transitively at top-level-module
granularity, the non-test closure is 239 modules and 973,680 of 1,041,110 lines
(94%). Module granularity over-counts (a file may use one item of a module), so
read 94% as an upper bound on what must move, not a plan. It is still far above
anything a "small base crate" (step 1) can absorb. A prototype that compiles
would have to move or stub most of the daemon, so a before/after of the
`loom-daemon` lib-test unit would measure a different refactor, not this one.
No prototype branch was built; the per-crate fixed cost (metadata, link) was
not measured for the same reason.

*Upper bound on the saving.* Even with a free extraction, 11,283 of about 1.04M
lines is 1.1%. Applying the linear assumption to the measured 180.85 s gives at
most about 2 s, below the run-to-run noise at this load and below the fixed
cost one more crate adds. The design's "6.5k lines, 0.6%" row was already small;
re-counted it is 1.1% and still not worth a crate.

*What this means for the path.* The leaf-first ordering by fan-out is still the
right order, but fan-out is not small for any candidate, and `safehouse` was the
cheapest by the §2 table. Step 1 (shared base crate) is where the cost is, and
the closure above says the base crate would be most of `loom-daemon`. Do not
start step 1 on this design. A revised design should first pick a seam that
cuts the dependency graph (for example `types` plus `event_bus` plus
`peer_claims`, 14k lines, which many subsystems share) and measure that, or
treat crate extraction as not viable and look at reducing monomorphised test
code instead (37.8% of LLVM IR lines are in `tests` modules, section 1).

## 5. Incremental migration

One crate or module per PR. Every PR runs the same two checks.

**Step 0: prototype and measure (done in #12157; result: no-go).** The plan was
to extract `safehouse` into a `loom-safehouse` workspace member and time the
`loom-daemon` lib-test unit before and after. The dependency closure showed the
extraction cannot compile without moving about 94% of the crate (section 4), so
only the baseline was timed. Steps 1..N below are on hold until the design is
revised around a seam that cuts the graph.

**Step 1: shared base crate.** Move the types that the leaf subsystems import
from `crate::` (config, error and path types) into `loom-core` (name TBD) so
extracted crates do not depend back on `loom-daemon`. Re-export from
`loom-daemon` at the old paths so call sites do not change.

**Steps 2..N: one subsystem per PR**, in the section 4 order. Per PR:

1. Create the workspace crate, move the module and its test files together, keep
   `pub use` shims at the old `crate::<module>` path.
2. Test-count check (no regression):
   ```bash
   cargo nextest list --workspace --lib --bins --profile ci 2>/dev/null | grep -c '::' > before.txt   # on main
   cargo nextest list --workspace --lib --bins --profile ci 2>/dev/null | grep -c '::' > after.txt    # on the PR branch
   diff before.txt after.txt
   ```
   Test names gain a crate prefix, so compare counts, not names. The PR body
   pastes both counts.
3. Fallback check (section 6).
4. CI shard check: `Rust Unit Tests (k/5)` partitions by the nextest filter; a
   new crate changes the partition, so the PR confirms all 5 legs still run
   tests (no empty shard).

Child issues are sized at one subsystem each. Total migration is a multi-PR
project; it is not scheduled by this doc.

## 6. `build-gate.sh` fallback stays green

`defaults/docs/build-gate.md` documents that `build-gate.sh` runs
`cargo nextest run --workspace --lib --bins --profile ci` and falls back to
`cargo test --workspace --lib --bins` when `cargo-nextest` is absent.

Both commands select by `--workspace`, so a new workspace member is picked up
by both without edits, **provided it is added to `[workspace] members`**. Each
step checks the fallback directly:

```bash
cargo test --workspace --lib --bins --no-run     # compiles every unit, including the new crate's lib-test
cargo test -p <new-crate> --lib                   # runs the moved tests
```

The `--no-run` form is the cheap per-PR check (compile only). The moved tests
run under `-p <new-crate>`. The full fallback is CI's job, not the Builder's
(#11690).

Edge case: `--bins` means the `loom-daemon` binary's own unit. `main.rs` has no
`cfg(test)` items (audit table), so nothing is lost from the bins unit.

## 7. Rejected alternatives

- **Crate-wide split in one PR.** 2,557 files; not reviewable.
- **`test-support` feature widening visibility everywhere.** Covers a handful
  of files at best (section 3), widens the public surface, and keeps the code in
  one serial unit.
- **Moving class (b) tests to `loom-daemon/tests/`.** Would need the items they
  reach to be `pub`, which is the previous bullet again.
- **Nextest-archive build/test split, cache, incremental and runner-size
  changes.** Out of scope per the issue; the archive split was rejected in
  #12089 for costing wall clock.
