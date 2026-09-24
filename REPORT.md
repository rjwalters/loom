# Issue #8793 — session_exec_docker launch-failure-preservation timing flake (127 vs 143)

## Verdict

**Code-side fix.** The launch path mis-attributes launch failure under the race.
143 is _not_ a legitimate SIGTERM-race outcome: no signal is involved — the
worker throws away a lease grant it had already accepted because the channel
EOF landed in the same read burst. ADR-0017 §2 (headless exec dispatch,
"preserving exit codes … unchanged") requires the invocation's own outcome —
launch failure 127 — to be recorded once a grant is in hand; the test was
asserting the correct invariant, and the 100 ms stdin hold + drop is exactly
the trigger that exposes the workerside race.

## Root cause

`loom-daemon/src/session_exec/worker.rs`, `read_lease()`:

- The pre-launch gate accepts the invocation when a read pass has parsed a
  valid, in-window lease line (host grant: `now_ms()+LEASE_MS`).
- The host cancels with an explicit `cancel\n` line, **never** by closing
  stdin; stdin EOF is only observed when the host *dies*, which the 2 s
  lease-freshness window exists precisely to bound.
- But `read_lease` returned `Some(false)` the moment any `read()` saw EOF
  (`Ok(0) => return Some(false)`). Under load, the worker's first stdin read
  lands **after** the test's 100 ms hold has closed the pipe, so one pass
  reads `[valid lease line, EOF]`: the grant is parsed and accepted, then the
  same pass's EOF vetoes it. `lease_valid` stays `false` → `run()` returns
  143 without ever attempting the spawn. The identical effective input
  (granted lease + dead host) produced 127 when scheduling let the worker
  read the line before the close — outcome depended on pipe-buffer luck.

## Changes

`loom-daemon/src/session_exec/worker.rs` only (no test-file change):

- `read_lease()` now tracks `accepted` (some valid lease line fully parsed in
  this pass) and `eof`. EOF no longer aborts the pass; at the pass end,
  `accepted` → `Some(true)` (the grant wins the burst), pure-EOF pass →
  `Some(false)` (dead host cancels immediately, pre- and post-launch, exactly
  as before), WouldBlock → unchanged (`None` = keep waiting; freshness
  evaluation otherwise). Explicit `cancel\n` / broken expiry / `>32` pending
  all still cancel, including after a grant (the suite's case 4).
- Added 5 unit tests pinning the pass semantics: mixed burst (EOF + accepted
  lease → grant), pure EOF, EOF-without-lease, explicit-cancel-after-grant,
  and WouldBlock-before-first-beat (`None`, launch gate keeps polling).

## Reproduction protocol & numbers (build host: 8-core Ubuntu 22.04, Docker 29.1.3,
load = 16× `yes > /dev/null`, load avg ~14 at runtime, warm
`CARGO_TARGET_DIR=/home/ubuntu/shared/target`)

Target test: `worker_rejects_missing_expired_cancelled_leases_and_preserves_launch_failure`
(`loom-daemon/tests/session_exec_docker.rs`, `--ignored`).

| Condition | N runs | Failures | Exact assertion on failure |
|---|---|---|---|
| Unloaded baseline (pre-fix) | 4 | 0 | — |
| 16× `yes` load (pre-fix) | 30 | **7** (23%) | `session_exec_docker.rs:338:9` `assertion \`left == right\` failed: left: Some(143) right: Some(127)` (every one) |

Iterations: 4, 7, 8, 23, 24, 26, 30 of 30 (capped loop, 900 s budget — ended
by iteration count).

## Post-fix verification (same box, same load protocol)

- Target test under 16× `yes` load: **30/30 pass** (same protocol as repro).
- Full module suite (`--test session_exec_docker -- --ignored`, all 3 tests
  incl. the 4 s reaping assertions): 3×3/3 under load, 3/3 unloaded.
- Unit tests: `cargo test -p loom-daemon --lib session_exec` → 7/7 (5 new + 2
  pre-existing).
- Full daemon lib suite, calm host: 7825/7825. (Under load, 1–3
  load-induced lock-contention flakes in `telemetry::trace::journal::tests`
  appeared on the pre- and post-fix trees alike — unrelated module, no shared
  state with this diff; 5/5 in isolation at load avg 0.08.)
- CI-parity hermetic suites: `test-spawn-codex.sh` 267/267 (build copy must
  be a git worktree — two initial failures were rsync `--exclude=.git`
  artifacts, fixed with `git init` on the copy),
  `test-spawn-codex-session-exec.sh` 10/10.
- `cargo fmt --all -- --check` clean; `cargo clippy -p loom-daemon
  --all-targets` clean.
- Load generators stopped after each phase (`pkill -x yes`; 0 remaining).