# CI telemetry daemon memory attribution (#11114, parent #10420)

Dated 2026-10-09, two passes the same day. This is an investigation report. It claims **no remediation**.
Measured on one Linux host only; no macOS comparison was possible.

- First pass (Parts 1-3): idle telemetry off/on and the streamed export, measured at `b426c9ecd` / `4f03f19bb`. Its figures are reproduced as originally recorded.
- Second pass (Parts 4-7): the poll cycle, the dedupe ledger and live daemon runs with realistic polling, all at **`dd15ae12a`** (v0.19.967). The sources this report measures (`loom-daemon/src/ci_telemetry`, `observability`, `telemetry`, `loom-daemon/Cargo.toml`) are byte-identical between `dd15ae12a` and `main` at `b24c5b092` (`git diff` is empty), so the second-pass evidence applies to current `main`. **In the second pass "MB" means MiB (2^20 bytes) throughout**, for RSS, heap and file sizes alike.

## Verdict

- The historical symptom (about 2.2 GB baseline plus 2.2 GB for CI telemetry) was **not reproduced** from an empty or small state. Telemetry off idles at 37 MB; telemetry on, after a realistic first poll (24 h backfill of a synthetic 111-repo org, 167 MB journal) and its export, settles at 123 MB.
- **One mechanism that grows with history was found: the dedupe ledger.** `Ledger::open` runs every poll cycle, reads the whole `seen.jsonl` into memory and keeps every key forever. Cost is about 250 B held and 330 B peak per unit in a single process, linear in history. A live daemon with an 857,160-unit (97 MB) ledger sat at **422-558 MB steady (605-725 MB high-water)** with default glibc arenas, against 183 MB (325 MB high-water) with `MALLOC_ARENA_MAX=1`. At the measured slopes 2.2 GB corresponds to roughly 2.5-3.0 million units (live, default arenas) up to 6.5-7 million (single process). **The real captain's ledger size is unobserved, so this is a measured mechanism, not a confirmed cause of the historical 2.2 GB.**
- The poll-cycle path the first pass left unexercised (artifact spans, job drafts, suite and JUnit span materialisation) is **bounded**: 21 MB peak live heap for a 24 h backfill of 9,400 units and 71,000 spans, 0.13 MB live afterwards. That hypothesis is refuted for the synthetic workload.
- Three actionable defects, each filed: #11137 (`DurableQueue` rewrites and fsyncs its whole file per push), #11159 (ledger `seen` set never pruned, fully materialised every cycle), #11160 (`compact_if_large` rewrites and fsyncs the whole ledger every cycle once the compacted ledger exceeds 8 MiB).
- Not attributed: the roughly 2.2 GB Linux baseline with telemetry **off** (see "Unresolved").

## Environment and build

| item | value |
|---|---|
| host | AWS Linux 7.0.0-1014-aws, Intel Xeon Platinum 8488C, 16 vCPU, 126 GB RAM, no swap (shared dispatch worker) |
| first-pass commits | allocation harness `4f03f19bb` (v0.19.963); daemon binary `b426c9ecd` (v0.19.960) |
| second-pass commit | `dd15ae12a` (v0.19.967) for the daemon binary (`loom-daemon --version`: `0.19.967 (commit dd15ae12a ... clean)`), the plain harness and the dhat harness |
| build | `cargo build --release`, default features, isolated target dir under `/tmp`; the harness is a disposable crate depending on `loom-daemon` by path, `debug = "line-tables-only"`, with an optional `dhat` 0.3.3 feature (fetched into the per-user cargo registry cache; no production dependency, no profiler feature in the repo, no host tool installed or changed) |
| allocator | glibc malloc (`ldd`: libc/libm/libgcc_s only); `MALLOC_ARENA_MAX` unset except where stated |
| isolation | `env -i`, `HOME`/`LOOM_WORKSPACE`/`LOOM_SOCKET_PATH` under `/tmp/loom-11114-*/`, `LOOM_GH_BIN` = synthetic `gh` stub, `LOOM_HOST_ID=scratch`; no production daemon touched, no credentials, dummy ingest key |
| export sink | local Python HTTP sink on `127.0.0.1:18099` that discards bodies |

Effective nonsecret config for the "on" runs (`.loom/config.json` in the scratch workspace):

```json
{"version":"2","fleet":{"captain":"scratch"},
 "autonomous":{"ciTelemetry":{"enabled":true,"owners":["example"],"intervalSecs":120}},
 "observability":{"enabled":true,"endpoint":"http://127.0.0.1:18099/ingest","flushIntervalSecs":30}}
```

"Off" is `{"version":"2"}`. Queue capacity is the default 2000. ETA was not toggled (the parent already showed it had no effect and it is outside this path).

**Synthetic org (second pass).** A Python `gh` stub serves 111 repos. Each repo gets a run every 2 h (staggered); a plain run has 6 jobs of 8 steps; every third run of the first 10 repos is a "CI" run with 3 sharded nextest legs (a JUnit artifact of 3,000 test cases each) and 2 shell-suite shards (120 suites each). About 9,500 units/day. Response shapes follow `loom-daemon/tests/fixtures/ci_telemetry/responses.json`. A real org's volume and artifact sizes are unknown (see "Limits").

**Sampling.** Live daemon runs: every 5 s, `/proc/<pid>/smaps_rollup` (Rss, Pss, Anonymous) and `status` (VmHWM, Threads) for the daemon, and the same summed over its descendants, reported separately. Children were the Python `gh` stub only (peak 16-17 MB RSS, excluded from every daemon column below). "Steady" is the mean of the last 60 s. PSS on file-backed pages depends on what else maps the same binary, so **Anonymous is the comparable column** between runs.

**Profiling overhead.** dhat (every allocation tracked): the 90-day ledger load takes 9.0 s versus 1.1 s plain, and RSS is inflated (353 MB versus 258 MB held). RSS and wall-time claims below use the **plain** build; dhat is used only for allocation attribution.

## Part 1 (first pass): live daemon, telemetry off vs on, idle (720 s windows)

Same binary, scratch state, empty workspace.

| run | journal | peak RSS | steady RSS (last 60 s) | steady PSS | steady anon | VmHWM |
|---|---|---|---|---|---|---|
| s0 telemetry off | none | 36.0 MB | 36.0 MB | 33.5 MB | 4.5 MB | 36.0 MB |
| s1 on, empty journal | none | 52.7 MB | 52.7 MB | 50.1 MB | 15.3 MB | 59.2 MB |
| s2 on, 562.2 MB journal (1.2 M `ci.job` lines) | 562,200,000 B | 53.5 MB | 53.5 MB | 50.9 MB | 15.9 MB | 53.5 MB |

Telemetry on costs about +16.7 MB RSS idle, mostly anonymous; a 562 MB journal adds about 0.8 MB. In s2 the export pass did not finish inside the window (the per-offer cost in Part 3), so peak RSS during a *completed* pass was not observed live; Part 2 covers a completed pass at library level.

## Part 2 (first pass): allocation evidence for the export path

Scratch crate with a counting `#[global_allocator]` driving the real `ci_telemetry::export::backfill` (atomic counters per allocation; wall times are upper bounds).

| scenario | offered | wall | total alloc | alloc calls | peak live heap | end live heap |
|---|---|---|---|---|---|---|
| empty journal, no-op sink | 0 | 0 s | 0.44 MB | 153 | 363 KB | 299 KB |
| 562 MB journal, no-op sink | 1,200,000 | 2.8 s | 1.30 GB | 13.2 M | 365 KB | 299 KB |

Streamed export is bounded: about 1.1 KB and 11 allocations churned per record, all freed immediately, largest single allocation the 96 KB line buffer. The end-of-pass cursor was `{"byte_offset":0,"exported":1200000}` and the journal rotated to `ci-telemetry.jsonl.1`, as designed.

## Part 3 (first pass): the queue behind the export

Same harness, real `DurableQueue` (capacity 2000) behind the export (`offer_durable` per record):

| offered | wall | total alloc | alloc calls | peak live heap | queue len / dropped |
|---|---|---|---|---|---|
| 1,000 | 6.2 s | 0.55 GB | 1.5 M | 1.7 MB | 1000 / 0 |
| 5,000 | 48.8 s | 7.9 GB | 24 M | 3.1 MB | 2000 / 3000 |
| 20,000 | 194 s | 36.5 GB | 114 M | 3.1 MB | 2000 / 18,000 |

Live heap is capped near 3 MB by the capacity bound (dominant allocation: the `VecDeque<TelemetryEnvelope>`, 868 KB, `export::backfill` -> `scan_lines` -> `DurableQueue::push_inner` -> `VecDeque::grow`). Every push re-serializes the whole queue, rewrites a temp file and renames it, and a durable push also fsyncs file and directory: about 9.7 ms and 1.8 MB of transient allocation per record at full queue, extrapolating to about 3.3 hours and about 2 TB of allocator churn for 1.2 M records. Filed as #11137. Draining a backlog larger than the queue evicts the oldest records (98.5 % of 20,000 dropped); owned by #11115.

## Part 4 (second pass): the poll cycle

Real `poll::run_cycle` through `GhCliApi` against the synthetic org, one process, 4 cycles with the clock advanced 2 h per cycle, empty ledger at start. A dhat build gives attribution; a plain build gives wall time, RSS and the table below.

| cycle | work | wall (plain) | ledger after | journal after |
|---|---|---|---|---|
| 0 | 24 h backfill: 1,321 runs + 8,126 jobs, 9,600 suite spans (80 shard records), 61,440 test spans (120 JUnit records), 666 stories stitched, 1,675 requests | 107 s | 1.0 MB | 152 MB |
| 1 | 111 runs + 706 jobs, 1,920 suite + 12,288 test spans | 15.8 s | 1.1 MB | 174 MB |
| 2 | 111 runs + 676 jobs, 480 suite + 3,072 test spans | 13.9 s | 1.1 MB | 184 MB |
| 3 | 111 runs + 666 jobs | 12.8 s | 1.2 MB | 191 MB |

Plain build: RSS 28.9 MB after cycle 0, 29.3 MB after cycles 1-3, VmHWM 33.6 MB. dhat build: peak live heap **21.1 MB** at the global peak, **0.13 MB** live at exit, 5.7 GB allocated in total (4.5 GB of it in cycle 0, the 24 h backfill). Largest live allocations at the peak are all small and per-cycle: the journal append buffer (`journal.rs`, 2.5 MB), the envelope collection in `emit` (`poll.rs`, 1.9 MB), the run/job record vectors (`records.rs`, 1.6 MB), the pending-unit envelope clones (`ledger.rs`, 1.5 MB). The largest *churn* is the ledger commit buffer (`ledger.rs` `commit`, 669 MB allocated, about 1 MB live at most), transient and freed.

Reading: the poll path holds memory proportional to one cycle's units, then frees it. It does not accumulate. Limit: synthetic artifacts (3,000 test cases per JUnit file, 120 suites per shard); real JUnit and span volumes may be larger.

## Part 5 (second pass): the dedupe ledger

`run_locked` opens `seen.jsonl` on every cycle (`poll.rs`, `Ledger::open`). `read_repaired_lines` reads the **whole file** into a `Vec<String>`; `from_lines` parses each line into `seen: HashSet<UnitKey>`. No code path removes a `seen` key (`compact_if_large` drops envelope payloads and re-emits every key). Memory per cycle therefore scales with all history.

Plain build, `Ledger::open_read_only` on synthetic ledgers of 30, 90 and 180 days:

| ledger | units | file | held after open | peak RSS (VmHWM) | RSS after the ledger is dropped |
|---|---|---|---|---|---|
| 30 d | 346,320 | - | 80.0 MB | 101.9 MB | 51.5 MB |
| 90 d | 1,038,960 | 118.0 MB | 258.2 MB | 338.6 MB | 144.2 MB |
| 180 d | 2,077,920 | - | 511.3 MB | 672.4 MB | 283.3 MB |

Per unit: 242-261 B held, 309-342 B peak, linear in history. **143-156 B per unit stays resident after the ledger is freed.**

dhat on the 90-day ledger: peak live heap **222.7 MB**, 1.2 KB live after drop, 1,111 MB allocated in total.

| call path (`ledger.rs`) | live at global peak | note |
|---|---|---|
| `from_lines` -> `seen.insert` | 171.0 MB | the `HashSet` table, 20 doublings |
| `read_complete_lines` -> `lines.push` | 24.0 MB buffer + 13.7 MB text | the whole file as `Vec<String>` |
| serde `Deserialize` of `repo: String` | 14.0 MB | one `String` per line |

702 MB of the 1,111 MB allocated is serde parse temporaries and 228 MB is hash-table doubling: freed immediately, but it is the churn the allocator absorbs every cycle.

Compaction repeats forever: with a compacted 92,413,717-byte (88.1 MB) ledger and **no new units**, three consecutive single-cycle runs each replaced the file (inode 322523 -> 322385 -> 322530 -> 322565, new mtime every time, size unchanged). Compaction keeps every key, so a ledger above about 78 k units is above the 8 MiB threshold after compaction too, and is rewritten and fsynced every cycle: 720 times a day at 120 s, about 66.5 GB (62 GiB) a day of writes at this size (arithmetic). Filed as #11160. The compaction buffer's allocation was not profiled separately: the single-process peak is 112 MB above the held ledger (288 MB versus 176 MB for the 857,160-unit ledger), consistent with it but not proof.

## Part 6 (second pass): live daemon with realistic polling

Isolated daemon, plain release build at `dd15ae12a`, real poll cycles every 120 s against the synthetic org, exporter on, same sink. "Ledger" is the seeded `seen.jsonl`; the aligned seed holds 857,160 units / 96.8 MB (88.1 MB after the first compaction). Daemon columns only; the stub child is excluded.

| run | window | ledger | allocator | peak RSS | steady RSS | steady PSS | steady anon | VmHWM |
|---|---|---|---|---|---|---|---|---|
| L0 telemetry off | 595 s | none | default | 37.4 MB | 37.4 MB | 19.9 MB | 4.4 MB | 37.4 MB |
| L1 on | 915 s | fresh (24 h backfill, 167 MB journal) | default | 123.3 MB | 122.5 MB | 101.3 MB | 79.5 MB | 123.3 MB |
| L2 on | 916 s | 857,160 units | default | 637.6 MB | 558.0 MB | 536.7 MB | 517.0 MB | 724.8 MB |
| L4 on (repeat of L2) | 896 s | 857,160 units | default | 512.7 MB | 422.4 MB | 419.9 MB | 380.1 MB | 604.7 MB |
| L3 on | 895 s | 857,160 units | `MALLOC_ARENA_MAX=1` | 216.0 MB | 183.3 MB | 162.1 MB | 140.6 MB | 325.3 MB |

L1 trajectory: 19 MB at start, 74 MB (anon 32 MB) after the first poll, 121 MB (anon 78 MB) once the exporter backfilled the journal (the queue hit its 2000 capacity and dropped records, #11115). The +47 MB at export is not attributed: the library-level export keeps under 1 MB live (Part 2), so it is consistent with allocator retention but was not profiled in the daemon.

L2 and L4 (default arenas, same ledger size): RSS **stair-steps** across cycles instead of plateauing (L2: 185 MB after the first cycle, 256, 380, 537, 557 MB at the end; L4: 191 -> 136 -> 305 -> 422 -> 460 MB, not monotone), although each cycle loads the identical ledger and the live heap between cycles is near zero (Part 5). L3 (one arena) plateaus within the first cycle at 177-183 MB. The single-process harness over 8 consecutive cycles on the same 857,160-unit ledger plateaued at 134-143 MB RSS with a 288 MB peak (6.2-7.3 s per cycle), which is what L3 reproduces plus the daemon's own base. Default arenas are therefore **2.3-3.0x steady and 1.9-2.2x high-water** above the single-arena daemon for the same work (L2 and L4 versus L3). Cycles run on tokio `spawn_blocking` threads (`ci_telemetry/mod.rs`); glibc gives each thread an arena, and transient allocations freed in a non-main arena are returned to the OS only when the top of that heap is free. That mechanism fits the measurements but **per-arena retention was not directly inspected** (no `malloc_info` or `malloc_stats` capture); the evidence is the three-run comparison above.

An additional run with sharded CI legs on all 111 repos was aborted (queue at capacity, 1,890 drops) and is not used.

## Part 7 (second pass): extrapolation and the one check that would settle it

Per-unit VmHWM slopes (Parts 5 and 6): 309-342 B in a single process (180 d: 672 MB / 2.08 M units), and 740-887 B in the live default-arena daemon (L4 and L2: 605 and 725 MB / 857 k units). 2.2 GB of RSS corresponds to about **6.5-7 M units** at the first slope and about **2.5-3.0 M units** at the second. The synthetic org writes about 9.5 k units/day; the number for the real org is unknown. A ledger of that many units is roughly 0.3-0.8 GB on disk (about 118 B per line).

The single measurement that would confirm or refute this for the captain: `ls -l` and `wc -l` on its `.loom/state/ci-telemetry/seen.jsonl`. Not available from this worker.

## Limits

- One platform (Linux). No macOS comparison, so nothing here explains why Macs are reported at 166-222 MB.
- Synthetic workload: record shapes follow the repo fixtures, but volume, artifact sizes and history depth are modelled, not observed. The ledger finding in particular is scale-dependent and gives no number for the real org.
- Windows of 10-15 minutes, 5 s sampling; the default-arena runs had not plateaued by the end of their window (L2 still rising slowly), so their steady figures are lower bounds for longer runs. Two default-arena runs and one single-arena run: the 2.3-3.0x range is n=2 versus n=1.
- The arena experiment shows the ledger-load path is amplified by default glibc arena behaviour. It is a different workload from the parent's `MALLOC_ARENA_MAX=2` experiment (a production-like daemon, reported worse) and does not establish a safe global setting. No recommendation is made.
- The live daemon binary and harness are one commit (`dd15ae12a`); the first-pass numbers are at earlier commits (see Environment).

## Unresolved

1. **The ~2.2 GB Linux baseline with telemetry off**: not reproduced; the scratch daemon idles at 37 MB. It plausibly depends on workspace size (111 repos, registries, sweep state, restored sessions), which an empty workspace with `LOOM_NO_RESTORE=1` deliberately avoids. Not attributed, and this report does not examine the telemetry-off path beyond the idle measurement.
2. **The real captain's ledger and journal sizes** (Part 7).
3. **Allocator-retained pages in the baseline** (outside the ledger path): not measured.
4. **Mac vs Linux**: single platform.
5. **Real artifact volume** for JUnit and suite spans (Part 4 limit).

## Reproduction

Scripts and the harness live under `/tmp/loom-11114-r2` and `/tmp/loom-11114-r3` (scratch, intentionally not committed; raw sample TSVs, daemon logs and dhat JSON are there). Shape:

- Daemon runs: `env -i PATH=/usr/bin:/bin HOME=<scratch> LOOM_WORKSPACE=<scratch>/ws LOOM_SOCKET_PATH=<scratch>/daemon.sock LOOM_GH_BIN=<stub> LOOM_HOST_ID=scratch LOOM_OBSERVABILITY_INGEST_KEY_FILE=<scratch>/ingest-key [MALLOC_ARENA_MAX=1] loom-daemon`, with the config above in `<scratch>/ws/.loom/config.json` and, for the ledger runs, a pre-built `<scratch>/ws/.loom/state/ci-telemetry/seen.jsonl`. A sampler reads `smaps_rollup` for the pid and its descendants every 5 s.
- Ledger generator: compacted `seen` lines (one per run and per job) for the synthetic org over N days, keys aligned to the stub's run numbering so a live poll sees them as already seen; plus one `watermark` per repo and an `emitted` line.
- Harness (`poll <root> <cycles> <step_s> <now_file>`, `ledger <seen.jsonl>`, `export <root> <endpoint> <batch>`): calls `ci_telemetry::poll::run_cycle`, `Ledger::open_read_only` and `export::backfill`. `--features dhat-heap` swaps in `dhat::Alloc` (`trim_backtraces(Some(40))`); the dhat JSON is summarised by call path restricted to `loom-daemon` source frames.
- Ledger replacement check: copy a compacted ledger, run the harness `poll` for one cycle, compare `stat -c '%i %s %Y'` before and after, three times.

## Acceptance criteria map

- Diagnostic report with commit/build/features, platform, config, commands, state sizes, windows, telemetry off/on: Environment, Parts 1 and 6, Reproduction (ETA off/on not repeated: not relevant to this path).
- Heap profile with dominant live allocations and call paths, daemon separate from children: Parts 4-5 (dhat call paths) and Part 6 (children excluded and reported separately); measured explanations are kept apart from hypotheses in Part 6 (arena mechanism) and Unresolved.
- Live Linux verification at the investigated commit with peak and steady RSS/PSS: Part 6, runs L0-L4 at `dd15ae12a`.
- Follow-ups filed and linked to #10420 (#11137, #11159, #11160); the old symptom is absent from an empty state, and the reproduction matrix and its limits are recorded above. This investigation does not claim any remediation.

## Follow-ups

- #11137: `DurableQueue` rewrites and fsyncs the whole queue file on every push.
- #11159: ledger `seen` set never pruned and fully materialised every poll cycle.
- #11160: `compact_if_large` rewrites and fsyncs the whole ledger every poll cycle once the compacted ledger exceeds 8 MiB.
- Already tracked elsewhere: #11115 (admission and drops), #11062 (cursor crash safety; the cursor is saved only at the end of a pass, so a stopped long pass restarts from the beginning).
