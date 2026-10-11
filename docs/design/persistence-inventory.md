# Persistence Inventory and Target Journal Design (Epic #9908, Phase 1)

Phase 1 deliverable for #11288, part of epic #9908 ("Consolidate persistence:
one journal per concern"). This document lists every persisted store found in
the inventoried source trees. For each store it records the writer, the
readers, the authority, the schema/version field, the recovery behaviour and a
proposed disposition. It then proposes a target journal schema with one writer
per concern.

**Status: proposal for curation.** This phase changes documentation only. It
creates no runtime store, migrates no reader, deletes no format, and changes no
forge authority. Phase 4, which retires old formats, still needs the
operator's explicit approval of its diff before it lands (epic #9908).

This is a repo-local design doc under `docs/design/`. It is never installed
into consumer repos.

**Source SHA.** Every citation is `path:line` against
`febfc4335d0554865fadbecba718245cd88c6678` (`febfc4335`, `main`,
2026-10-10). Paths without a prefix are under `loom-daemon/src/`. Paths under
`defaults/`, `mcp-loom/`, `loom-api/` and `docs/` are written in full.

**Contents**

- [1. Coverage method and search receipts](#1-coverage-method-and-search-receipts)
- [2. Concerns and dispositions](#2-concerns-and-dispositions)
- [3. Inventory: execution / journal concern](#3-inventory-execution--journal-concern)
- [4. Inventory: telemetry concern](#4-inventory-telemetry-concern)
- [5. Inventory: configuration and configuration-snapshot concern](#5-inventory-configuration-and-configuration-snapshot-concern)
- [6. Inventory: host control state](#6-inventory-host-control-state)
- [7. Adjacent stores (catalogued, out of consolidation scope)](#7-adjacent-stores-catalogued-out-of-consolidation-scope)
- [8. Duplicated durable data](#8-duplicated-durable-data)
- [9. Target design: one journal per concern](#9-target-design-one-journal-per-concern)
- [10. Projections, rebuild and shadow-write divergence detection](#10-projections-rebuild-and-shadow-write-divergence-detection)
- [11. Failure semantics: restart, corrupt/partial/stale, version skew, downgrade](#11-failure-semantics-restart-corruptpartialstale-version-skew-downgrade)
- [12. Decomposition and revised estimates for Phases 2-5](#12-decomposition-and-revised-estimates-for-phases-2-5)
- [13. Anticipated net reduction](#13-anticipated-net-reduction)
- [14. Unresolved choices, ranked](#14-unresolved-choices-ranked)
- [15. Out of scope](#15-out-of-scope)

## 1. Coverage method and search receipts

Every search ran from the repository root at the SHA above. Counts are the
exact output at that SHA.

| # | Command | Result |
|---|---------|--------|
| R1 | `rg -n '\.jsonl\|rusqlite\|serde_json::to_(string\|writer)\|fs::write\|atomic' loom-daemon/src \| wc -l` (the Curator's suggested receipt) | 4391 lines in 914 files. Too broad to classify line by line, so R3 narrows it. |
| R2 | `rg -l 'Connection::open' loom-daemon/src loom-api/src -g '!*tests*'` | 11 files. All of them open `activity.db`, claude-monitor's `usage.db` or OpenCode's `opencode.db` (§4, §7). No other SQLite store exists. |
| R3 | The write-site scan below, which skips `#[cfg(test)] mod` bodies, `*tests.rs`, `tests/` and `test_support` | 222 files with 574 write sites. Each was classified as a store, a log, stdout or socket I/O, or a test fixture. |
| R4 | `rg -n 'const [A-Z_]+: &(str\|.static str) = "[^"]*\.(json\|jsonl\|ndjson\|db\|sqlite\|lock\|pid\|state)"' --type rust loom-daemon/src \| wc -l` | 70 path constants. Each resolves to a row in §3-§7. |
| R5 | `rg -n 'home_dir\(\)' --type rust loom-daemon/src -g '!*tests*' \| wc -l` | 84 sites. Used to find machine-level (`~/.loom/…`) stores. |
| R6 | `rg -l 'writeFile\|appendFile\|writeFileSync\|appendFileSync' mcp-loom/src -g '!*.test.ts' \| wc -l` | 2 files: `mcp-loom/src/shared/config.ts` and `mcp-loom/src/shared/ipc.ts` (§5, §7). |
| R7 | `rg -l '\.loom/(state\|stats\|logs\|sweep-run\|exit-codes\|retry-state\|signals\|status\|metrics\|claims\|locks\|sweep-checkpoint)' defaults/scripts -g '*.sh' -g '!tests/**' \| wc -l` | 41 shell scripts. The ones that own a store are listed in §3-§4 and §7. |
| R8 | `rg -n 'sweep-checkpoint' loom-daemon/src defaults/scripts defaults/.claude/commands/loom -g '!*tests*' -g '!**/tests/**' \| wc -l` | 131 references. Their readers are in §3, row E3. |
| R9 | `EPHEMERAL_PATTERNS` in `init/post_init.rs:146` | The installer-managed `.gitignore` list of Loom runtime paths. Every entry is either covered by a row or is a retired path noted in §3. |

The R3 scanner is run with `python3 -I scan.py loom-daemon/src`:

```python
import os, re, sys
root = sys.argv[1]
pat = re.compile(r'fs::write\(|File::create\(|OpenOptions::new\(\)|write_atomic\w*\(|atomic_write\('
                 r'|write_json_atomic\(|Connection::open|fs::rename\(|\.persist\(|tempfile_in'
                 r'|NamedTempFile|to_writer|\.write_all\(')
for dp, _, fns in os.walk(root):
    if '/tests' in dp: continue
    for f in fns:
        if not f.endswith('.rs') or f.endswith('tests.rs') or 'test_support' in f: continue
        p = os.path.join(dp, f); lines = open(p, errors='replace').read().split('\n'); hits = []
        for i, l in enumerate(lines):
            s = l.strip()
            if s.startswith('#[cfg(test)]') and any(re.match(r'\s*(pub(\(\w+\))?\s+)?mod \w+', x) for x in lines[i+1:i+3]): break
            if not s.startswith('//') and pat.search(l): hits.append(i + 1)
        if hits: print(len(hits), p[len(root)+1:], hits[:6])
```

**Coverage limits, stated plainly.**

- A writer that persists only through a helper whose name matches none of the
  patterns is found only by R4 or R5. One example is `fleet_sync.rs:770`,
  which calls `fetch::write_atomic`. An earlier version of the scan stopped at
  the first `#[cfg(test)]` and missed it.
- Stores written by agents through role-prompt prose are covered only when a
  script or CLI owns the path. Raw `echo >` in a markdown prompt is not.
- `epic_state.rs` was named as a starting point but has **no store**. It is a
  pure classification computed from the forge: the issue body plus the state
  of its child issues (`epic_state.rs:1-15`). The forge is its only authority.

## 2. Concerns and dispositions

The epic names three concerns. The inventory adds one more, "host control
state", because these files are neither journals nor telemetry and already
have a single writer each. Without the extra concern they would be forced
into one of the three.

| Concern | Question it answers | Target authority |
|---------|---------------------|------------------|
| **Execution / journal** | What happened to each unit of dispatched work on this host: claim, child process, checkpoint, terminal outcome? | **J1**, the execution journal (§9.2) |
| **Telemetry** | What did the fleet emit for observability and analytics? | **J2**, the telemetry journal (§9.3) |
| **Configuration snapshot** | What effective configuration was a process running, and where did it come from? | **J3**, the config-snapshot journal (§9.4). Config *inputs* stay authoritative files. |
| **Host control state** | What should this host do next: settle clocks, roll guards, registries? | One single-writer snapshot file per subsystem. No journal. |
| *Forge* (not local) | Issue/PR state, labels, lease records, park records | The forge, unchanged (#5057, ADR-0025) |

The disposition column uses five values:

- **J1/J2/J3**: becomes a record kind in the target journal. The legacy store
  is deleted in Phase 4.
- **PROJECTION**: kept, but redefined as rebuildable from the journal (§10).
- **DELETE**: no writer or no reader remains. Deleted in Phase 4.
- **RETAIN**: already has a single authority, or is outside the epic.
  Unchanged.
- **FIX**: a defect that needs no format change. It can land independently.

## 3. Inventory: execution / journal concern

| ID | Store | Path / format | Writer | Readers | Authority today | Schema / version | Recovery behaviour | Disposition |
|----|-------|---------------|--------|---------|-----------------|------------------|--------------------|-------------|
| E1 | Sweep liveness journal | `~/.loom/sweeps.json` (`LOOM_SWEEPS_JOURNAL_PATH`), JSON `{version, entries[{repo,issue,pid,started_at}]}`; machine-level (`sweep_journal.rs:104`) | `sweep_journal::record_sweep` (`sweep_journal.rs:243`) from dispatch; removal on reap | `live_claim.rs:481`, `claim_reconciliation.rs:2325`, `claim_reconciliation/pass_loop/building_heal.rs:55`, `startup_adoption.rs:113`, `worktree_ops/orphan_recovery.rs:215`, `sweep_registry/locks.rs:1214`, `cli/status.rs:574` | Authoritative for host-level liveness across daemon restarts and across instances | `version` = `JOURNAL_VERSION` 1 (`sweep_journal.rs:62`); missing version defaults to 1 | Missing, empty or corrupt file reads as an empty journal (`sweep_journal.rs:122`). Writes go to `json.tmp.<pid>` then rename (`sweep_journal.rs:153`), with no fsync. Dead PIDs are pruned on every read and write. | **PROJECTION → DELETE**: rebuilt from J1 `claim.*`/`sweep.terminal` (§10) |
| E2 | Claim-lock owner record | `<ws>/.loom/locks/issue-<N>/owner.json` and `pr-<N>`; JSON `LockOwner` (`sweep_registry/locks.rs:158`). The directory is the mutex. | `acquire_lock` (`sweep_registry/locks.rs:348`) writes at `:368`; also `:510` (PR) and `:791` (mid-build). `record_child_pid_in_lock` (`:408`) | `reconstruct` (`sweep_registry/locks.rs:883`), `live_claim.rs:264`, `worktree_ops/liveness.rs:148`, `disk_footprint.rs:411` | Authoritative for "which sweep owns this lock". This is the only place `pgid`, `model`, `scope_unit` and `resume_handle` survive a restart (`sweep_registry/resume_handle.rs:15-20`). | Unversioned. Additive `Option` + `serde(default)` fields (`sweep_registry/locks.rs:170-181`). | Plain `fs::write`: **not atomic**, so a torn write leaves unparseable JSON. Readers treat it as no owner. | Lock directory stays (it is a mutex, not data). `owner.json` becomes a **PROJECTION** of J1 `claim.*`. **FIX**: atomic write. |
| E3 | Sweep phase checkpoint | `<root>/.loom/sweep-checkpoint/issue-<N>.json`, JSON (phase, attempt, model, pr_number, task_id, jev) | `loom-daemon sweep-checkpoint write` (`cli/sweep_checkpoint.rs:69`, persist `:295`), invoked by `defaults/scripts/sweep-checkpoint.sh` from the sweep prompts | Reaper progress check (`sweep_registry/mod.rs:352`), `claim_reconciliation.rs:1730`, `jev_tier.rs:539`, `sweep_registry/outcome_journal.rs:128`, `tokens_pool/private_workspace/export.rs:220`, `activity/session_context.rs:220`, `worktree_ops/clean.rs:2574`, and the role prompts through the CLI `read/phase` | Authoritative for sweep resume (`/loom:sweep N` restarts from the last completed phase) | Unversioned. 64 KiB cap. | NamedTempFile, fsync, persist, then a directory fsync (`cli/sweep_checkpoint.rs:295-310`). Oversize or unparseable records are errors to the CLI caller. | **J1** `checkpoint.completed`. File becomes a **PROJECTION**, then **DELETE** once the CLI `read/phase` answers from J1. |
| E4 | Builder worktree checkpoint | `<worktree>/.loom-checkpoint` (`script_helpers/checkpoints.rs:38`) | `script_helpers/checkpoints.rs:230` through `write_json_file` (`script_helpers/mod.rs:201`) | `checkpoint.sh` / `loom-checkpoint` in the same worktree | Worktree-scoped builder progress. Dies with the worktree. | Stage enum; unversioned | Absent or invalid → `None` (`script_helpers/checkpoints.rs:175-177`) | **RETAIN**: scratch tied to the worktree's lifetime, not shared state |
| E5 | File-based claim (`loom-claim`) | `<root>/.loom/claims/issue-<N>.lock/claim.json` (`script_helpers/claim.rs:99`); `mkdir` gives atomicity | `script_helpers/claim.rs:185`, `:262` | `worktree_ops/claim_file.rs:40` (`has_valid_claim`) ← `worktree_ops/orphan_recovery.rs:381` | A third local claim primitive, alongside E2 and the forge label/lease | TTL fields; unversioned | Expiry and abandonment checks in `worktree_ops::claim_file` | Q5 (§14): **DELETE** recommended, keep if the operator decides otherwise |
| E6 | `issue_claims` SQLite table | `~/.loom/activity.db`, table at `activity/schema.rs:325` | IPC `Request::ClaimIssue` (`types.rs:80`, handled `ipc.rs:2625`) | `activity/claims.rs` only | A fourth claim registry. `rg ClaimIssue` finds **no in-repo client** in `mcp-loom/`, `defaults/` or the CLI. | `CREATE TABLE IF NOT EXISTS`; no user_version | SQLite's own | **DELETE** (Phase 4a): unused endpoint and table |
| E7 | Sweep-run registry | `<root>/.loom/sweep-run/` entries; `main-clean-baseline-<RUN>.txt` beside the checkpoints (`defaults/scripts/sweep-run-registry.sh:3`) | `sweep-run-registry.sh new/cleanup` | `sweep-run-registry.sh peers` (non-blocking peer warning) | Advisory, in-session `/loom:sweep` identity | Shell format | Dead PIDs pruned on `peers` | **RETAIN** for now. Candidate J1 `sweep_run.registered` (Q8). |
| E8 | Terminal outcome journal | `<ws>/.loom/logs/sweep-outcomes.jsonl` (`sweep_outcomes.rs:131`), JSONL `OutcomeRecord` | `sweep_outcomes::append_outcome` (`sweep_outcomes.rs:329`) ← `sweep_registry/outcome_journal.rs:249` | `sweep_outcome_summary.rs:341`, `observability/overhead.rs:196` | Authoritative "how did this sweep die" history | Unversioned record | Append without fsync. Rotates to one `.1` file at 5 MiB or 30 days (`sweep_outcomes.rs:135`, `:140`). Unparseable lines are skipped by `read_all` (`sweep_outcomes.rs:393`). | **J1** `sweep.terminal` → **DELETE** |
| E9 | PR-less tally sidecars | `<outcome-telemetry-journal>.prless-clears.json` and `.prless-counted.json` (`sweep_registry/prless_retry/durable.rs:104-124`) | `sweep_registry/prless_retry/durable.rs:146-147` (tmp then rename) | Same module: a floor recomputed from the `sweep.outcome` journal | A projection over E8/T1 plus clear marks | Unversioned map | Missing or unreadable → empty, so the floor falls back to the time window | **J1** `prless.counted`/`prless.cleared` → **DELETE** |
| E10 | Recovery events | `<root>/.loom/metrics/recovery-events.json`, bounded JSON array | `script_helpers/validate_phase.rs:311-355` | Operator / `validate_phase` | Write-mostly audit | Unversioned | Corrupt → reset to an empty array | **J1** `recovery.event` → **DELETE** |
| E11 | Wrapper retry state | `<ws>/.loom/retry-state/<TERMINAL_ID>.json` (`defaults/scripts/claude-wrapper.sh:179`) | `claude-wrapper.sh` | **None found**. Its consumers, `agent-wait-bg.sh` and the shepherd, are retired. | Write-only | — | — | **DELETE** (Phase 4a) |
| E12 | Wrapper exit-code sidecar | `<ws>/.loom/exit-codes/<TERMINAL_ID>.exit` (`defaults/scripts/claude-wrapper.sh:191`) | `claude-wrapper.sh` | **None found**. Only `.gitignore` and the main-clean allowlist reference it. | Write-only | — | — | **DELETE** (Phase 4a) |
| E13 | Spawn-loop state (retired) | `.loom/spawn-loop-state.json` | **No writer**. `spawn-loop.sh` was removed (`docs/migration/daemon-state-consumers.md`). | `worktree_ops/orphan_recovery.rs:194`, `:447` | None; always absent | — | Absent → source skipped | **DELETE** the dead reader (Phase 4a) |
| E14 | Daemon state (retired) | `.loom/daemon-state.json` | **No writer** (`init/post_init.rs:93`) | `defaults/scripts/archive-logs.sh:219` | None | — | — | **DELETE** the dead reader (Phase 4a) |
| E15 | Execution trace journal | `<ws>/.loom/logs/trace-context/<id>.jsonl` plus `<id>.json` context and `<id>.cursor` (`telemetry/trace/store.rs:51`, `telemetry/trace/journal.rs:67`) | `telemetry/trace/journal.rs` (`append` `:149`), from `observability/lifecycle.rs:447` (checkpoint spans) and the launch paths | The same module's `drain` (`telemetry/trace/journal.rs:457`) into the export path | Authoritative for span start/finish across parent and child processes | Tagged `Entry` enum (`telemetry/trace/journal.rs:45`); entries capped at 32 KiB and the journal at 16 MiB (`:13-14`) | File lock with a 1 s budget (`:78`). A torn tail is truncated under the lock (`:137-143`). `sync_data` on each append. Byte cursor committed atomically (`:432`). Retired once drained (`:379`). | **RETAIN** as a bounded per-execution journal. Its implementation is the **template** for J1/J2 (§9.1). |
| E16 | Sweep log + `# LOOM_LAUNCH` record | `<ws>/.loom/logs/sweep-issue-<N>.log` (`sweep_registry/log_paths.rs:9`) | Spawn scripts and the harness; `worker_spawn` writes the launch line | `launch_record.rs:1-20` re-parses credential attribution at terminal time | Logs (prose). The launch record is the only durable credential attribution. | Line prefix | Logs rotate, so the attribution is lost when the log is archived | **J1** `claim.child_recorded.credential` (attribution only), so `launch_record` no longer has to grep logs |

Forge-side execution state (`loom:building`, the `<!-- loom:lease … -->`
comment, body park records written by `sweep_registry/park_hold.rs`) stays
authoritative and is out of scope (#5057, ADR-0025). Local stores can only
prove a claim *dead on this host* (`worktree_ops/orphan_recovery.rs:15-18`).

## 4. Inventory: telemetry concern

| ID | Store | Path / format | Writer | Readers | Authority today | Schema / version | Recovery behaviour | Disposition |
|----|-------|---------------|--------|---------|-----------------|------------------|--------------------|-------------|
| T1 | `sweep.outcome` telemetry journal | `<ws>/.loom/logs/sweep-outcome-telemetry.jsonl` (`sweep_outcomes.rs:412`), JSONL `TelemetryEnvelope` | `append_outcome_telemetry` (`sweep_outcomes.rs:438`) ← `sweep_registry/outcome_journal.rs:1121` | Backfill (`observability/backfill.rs:178`), `sweep_outcome_summary.rs:310`, `cli/misc_cmds.rs:415`, `sweep_registry/start_facts.rs:33` (lineage), and the E9 floor | Export queue of record for `sweep.outcome`. Execution logic also reads it (lineage, PR-less floor). | Envelope `schema_version` per kind (`telemetry/envelope.rs:10`, `telemetry/mod.rs:101` = 2) | Same rotation as E8. A bad line is skipped. `start_facts` reads the whole file on every dispatch. | **J2** stream `sweep`. The execution-side reads move to J1. |
| T2 | Role-tick telemetry | `<ws>/.loom/logs/role-tick-telemetry.jsonl` (`sweep_outcomes.rs:584`) | `append_role_tick_telemetry` (`sweep_outcomes.rs:628`) ← `role_tick_telemetry.rs:778` | `read_all_role_tick_outcomes` | Queue of record for `role_tick.outcome` | Envelope | Rotates at 64 MiB (`sweep_outcomes.rs:605`) | **J2** stream `role_tick` |
| T3 | Rework events | `<ws>/.loom/logs/sweep-rework-events.jsonl` (`rework_events.rs:72`, `:137`) | `rework_events.rs:207` | `sweep_registry/outcome_journal/rework.rs` | Local append log | Per-line kind | Append; none | **J2** stream `rework` |
| T4 | Merge-queue events + dedupe set | `<root>/.loom/logs/merge-queue-events.jsonl` and `<root>/.loom/state/merge-queue/seen` (`forge_merge_queue/events.rs:141-142`) | `forge_merge_queue/events.rs:162`, `:176` | Operators, `pr-latency` | Local append log. The seen directory provides restart-safe dedupe. | Per-line kind | The dedupe set prevents repeats after a restart | **J2** stream `merge_queue`. Dedupe becomes a keyed J2 projection. |
| T5 | Shell telemetry logs | `<ws>/.loom/logs/guide-docs-telemetry.jsonl` (`defaults/scripts/guide-docs-telemetry.sh:55`), `<ws>/.loom/logs/merge-admission-telemetry.jsonl` (`defaults/scripts/merge-admission-telemetry.sh:65`) | Those scripts. The shape copies the T1 envelope by hand. | Operators | Local logs | Hand-copied envelope | Append; none | **J2** through a `loom-daemon telemetry append` subcommand (ADR-0018 language policy) |
| T6 | Sweep model stats | `<root>/.loom/stats/sweep-model-stats.jsonl` (`script_helpers/sweep_experiment.rs:45`) | `append_record` (`script_helpers/sweep_experiment.rs:440`) | `defaults/scripts/agent-metrics.sh` | Experiment log | Unversioned | Append | **J2** stream `experiment` |
| T7 | CI telemetry journal | `<root>/.loom/logs/ci-telemetry.jsonl` + `.N` rotations (`ci_telemetry/mod.rs:432`, `ci_telemetry/rotation.rs:92`) | `ci_telemetry/journal.rs:47` (open as writer, under the cycle lock), `:122` (append) | Export (`ci_telemetry/export.rs`) | Export queue of record for `ci.*` | Envelope | Opening as writer repairs a torn tail. Readers never repair. Rotates only behind the export cursor (#11045). | **J2** stream `ci`. This is the model J2 generalises. |
| T8 | CI dedupe ledger | `<root>/.loom/state/ci-telemetry/seen.jsonl` (`ci_telemetry/poll.rs:420`) | `ci_telemetry/ledger.rs:382` | The poller | **Commit point**: a unit's fsynced ledger line, including its full envelopes, comes before the journal write | Six line types | Replays only what the journal lacks. Compaction at `ci_telemetry/ledger.rs:763`. | **PROJECTION** (keys and watermarks only; Q4) |
| T9 | CI poller bookkeeping | `<root>/.loom/state/ci-telemetry/{status.json, discovery-cache.json, poll.lock, export-cursor.json}` (`ci_telemetry/state.rs:146`, `:327`, `:351`; `ci_telemetry/export.rs:46`) | `ci_telemetry/state.rs:158` (temp, fsync, rename) | `ci-telemetry status`, `health` | Status, cache, lock, cursor | Unversioned | Load falls back to the default | `status`/`discovery-cache`/`poll.lock`: **RETAIN**. `export-cursor`: merges into the J2 per-exporter cursor. |
| T10 | Observability export queue | `<ws>/.loom/logs/observability-queue.jsonl` and `observability-queue.<exporter>.jsonl` (`observability/queue.rs:43`, `:58`) | `DurableQueue`. The **whole file is rewritten** on every push or ack (`observability/queue.rs:334-335`, no fsync). | `observability/sender.rs` | A full **copy** of envelopes already held in T1/T2/T7 | Envelope | Replayed at open. Bounded drop-oldest. | **PROJECTION → DELETE**: the export reads J2 from a cursor |
| T11 | Backfill cursor | `<ws>/.loom/logs/observability-backfill-state.json` (`observability/backfill.rs:92`, `:98`) | `observability/backfill.rs:156` (tmp then rename) | Backfill pass | Cursor over T1 by `emitted_at` timestamp (`observability/backfill.rs:115`) | Unversioned | Corrupt → reprocess everything (the backend is idempotent) | Merges into the J2 per-exporter byte cursor |
| T12 | Forge call stats sink | `<host tmp>/loom-forge-call-stats/calls-<hour>.jsonl` (`forge_call_stats.rs:804`, `forge_call_stats_sink.rs:11`) | `forge_call_stats_sink.rs:23` | `forge_call_stats_ingest.rs` | Per-host and owner-only by design | Row schema | Pruned after `RETAIN_HOURS` (`forge_call_stats.rs:93`) | **RETAIN** (Q3: high volume, deliberately in host tmp) |
| T13 | `gh` invocation failures | `<host tmp>/loom-gh-invocations/failures-<hour>.jsonl` (`gh_invocation/telemetry.rs:526`, `:549`) | `gh_invocation/telemetry.rs:561` | Ingest | Per-host | Row | Hourly files | **RETAIN** (with T12) |
| T14 | Pick journal | `<host tmp>/loom-pick-journal/<role>-<n>.jsonl` (`observability/pick_journal.rs:103`) | `observability/pick_journal.rs:237` | Ingest | Per-tick diagnostics | Row | Tmp | **RETAIN** |
| T15 | Trace joins / attended output | `<ws>/.loom/logs/trace-joins/` (`observability/runtime_usage/join.rs:40`); attended `session.output` lock and queue files under `<ws>/.loom/logs/live-output-attended/` (`observability/session_output/attended.rs:523`) | Same modules | Exporter | Derived join artifacts | Row | Locked segments | **RETAIN** (derived, bounded) |
| T16 | Activity database | `~/.loom/activity.db` (`daemon_service.rs:259`, `limit_calibration.rs:343`), SQLite with 23 `CREATE TABLE IF NOT EXISTS` tables (19 from `activity/schema.rs:17` onward, 4 in `activity/tuning.rs`) | IPC handlers, transcript ingest, weekly-point sampler, tuning | `loom-daemon stats`, usage report, limit calibration. **`loom-api` reads `<workspace>/.loom/activity.db` instead** (`loom-api/src/main.rs:59`, `:194`) | Analytics store | `CREATE TABLE IF NOT EXISTS` plus ad-hoc `PRAGMA table_info` migrations (`activity/schema.rs:613-643`) | SQLite | **RETAIN** (analytics, not a journal). **FIX**: `loom-api` path divergence. E6 deleted. |
| T17 | Release signature evidence | `<repo>/.loom/logs/signature-evidence.jsonl` (`release_fetch/evidence.rs:67`, `:430-438`) | `release_fetch/evidence.rs:445` | Release adoption | Security audit | Schema-versioned records | Append without fsync. Rotates to one `.1` file past a size cap (`release_fetch/evidence.rs:449-453`). | **RETAIN** (audit; separate trust domain) |

## 5. Inventory: configuration and configuration-snapshot concern

| ID | Store | Path / format | Writer | Readers | Authority today | Schema / version | Recovery behaviour | Disposition |
|----|-------|---------------|--------|---------|-----------------|------------------|--------------------|-------------|
| C1 | Repo config | `<ws>/.loom/config.json` (`config_resolver.rs:23`), committed | Humans; `init` merge (`init/mod.rs:644`, rescue copy `:593`); `mcp-loom/src/shared/config.ts:230` | `config_resolver::resolve_effective_config` (`config_resolver.rs:233`) and about 20 call sites | Authoritative input | Unversioned; tolerant keys | Unparseable → rescue `.loom/*.bak` and replace with the template (`init/post_init.rs`, #4641) | **RETAIN** (input, not a snapshot) |
| C2 | Project tier | `<ws>/.loom-project/project.json` (`config_resolver.rs:28`) | Humans | Resolver | Input | — | — | **RETAIN** |
| C3 | Host-local tier | `<ws>/.loom-local/local.json` (`config_resolver.rs:32`) | Humans; fleet render (`fleet_store/render.rs:335`) | Resolver | Input (fleet-rendered when a store exists) | — | One `*.fleet-store-bak-<UTC>` backup (`fleet_store/render.rs:33`) | **RETAIN**. Renders are recorded in J3. |
| C4 | Machine tier | `~/.local/share/loom/config/defaults.json` (`config_resolver.rs:51`) | Humans; fleet render | Resolver | Input | — | Same backup rule | **RETAIN**. Renders are recorded in J3. |
| C5 | Fleet-store cache | `~/.loom/fleet-store/<owner>/<repo>/{manifest.json, blobs/<sha>}` (`fleet_store/mod.rs:224`, `fleet_store/fetch.rs:58-60`) | `fleet_store/fetch.rs:438` (atomic). Blobs are written before the manifest. | `fleet-config`, `fleet_sync` | **Cache** of the forge-hosted store; the forge is authoritative | `MANIFEST_VERSION` 1 (`fleet_store/fetch.rs:60`, checked `:402`) | Version mismatch or missing blob → refetch | **RETAIN** (cache, rebuildable from the forge) |
| C6 | Pending-restart marker | `<loom_dir>/fleet-config-pending-restart.json` (`fleet_store/pending_restart.rs:39`) | `fleet_store/pending_restart.rs:85` | `fleet-config status`, `loom-daemon status` | Derived: rendered-at PID compared with the current PID | Unversioned | Missing → nothing pending | **J3** `config.rendered` + **PROJECTION → DELETE** |
| C7 | Fleet sync status | `<loom_dir>/fleet-sync-status.json` (`fleet_sync.rs:121`, written `:770`) | `fleet_sync::publish` | `loom-daemon status` | Last-pass snapshot | Unversioned | Best effort | **J3** `fleet.sync` + **PROJECTION → DELETE** |
| C8 | Effective-config snapshot | **Does not exist.** Only hashes are persisted: `planner_config_hash` / `fleet_config_hash` on `fleet.state` telemetry (`observability/fleet_state.rs:634`, `telemetry/kinds/fleet_state.rs:420`, `:424`) | — | Backend | No local authority. A hash cannot be resolved back to a config. | — | — | **J3** `config.resolved` (new, content-addressed) |
| C9 | Install metadata | `<ws>/.loom/install-metadata.json` (`install_compat.rs:68`) | `scripts/install-loom.sh`, `defaults/scripts/resync-installed.sh` | Version gates, `create-pr.sh`, `check-main-freshness.sh` | Authoritative install record | Fields | — | **RETAIN** (install contract) |
| C10 | MCP state / command files | `~/.loom/state.json`, `~/.loom/mcp-command.json` (`mcp-loom/src/shared/config.ts:25`, `:28`) | `mcp-loom/src/shared/config.ts:189`, `mcp-loom/src/shared/ipc.ts:43` | `mcp-loom` | MCP-local | — | — | **RETAIN** (out of daemon scope) |

## 6. Inventory: host control state

These are single-writer snapshot files. Most already follow the
`auto_update/persisted_state.rs` pattern: a versioned file, a `LoadOutcome`
enum (`:148`), and temp + fsync + rename + directory fsync (`:207`).

| ID | Store | Path | Writer | Schema | Disposition |
|----|-------|------|--------|--------|-------------|
| H1 | Auto-update settle/floor/roll-attempt state | `~/.loom/auto_update_state.json` (`auto_update/persisted_state.rs:92`) | `persisted_state::store` (`:207`) | `schema_version` 1 (`:95`). Unknown version → start empty (`:175`). | **RETAIN**. This is the model for every control-state file. |
| H2 | Artifact-roll record | `~/.loom/auto-update-artifact-roll.json` (`auto_update.rs:629`, `:653`) | `auto_update.rs:684` (plain `fs::write`) | Unversioned | Fold additively into H1 (Q7) → **DELETE** |
| H3 | Pause manifest | `~/.loom/roll-pause-manifest.json` (`auto_update/pause_manifest.rs:38`) | `auto_update/pause_manifest.rs:425` | `SCHEMA_VERSION` 1, with a frozen v1 core that a rolled-back binary reads (`auto_update/pause_manifest.rs:40-44`) | **RETAIN** (rollback contract, #9735) |
| H4 | Failed-roll guard | `~/.loom/roll-failed-target.json` (`auto_update/pause_resume/attempt.rs:42`) | `auto_update/pause_resume/attempt.rs:128` | Unversioned. Read by the **old** binary after a failed roll. | **RETAIN** separately (Q7: the rollback path reads it) |
| H5 | Roll-pause per-item state | `<root>/.loom/state/roll-pause/<item>/{safe-point.json, handle.json, claim.json}` (`roll_pause/mod.rs:117-118`, `:143`; `roll_pause/claim_breadcrumb.rs:32`) | `roll_pause::write_atomic` (`roll_pause/mod.rs:190`) | Per design doc | **RETAIN** (pause protocol) |
| H6 | Workspace registry | `~/.loom/workspaces.json` (`workspace_registry.rs:136`) | `workspace_registry.rs:230` (tmp then rename) | `REGISTRY_VERSION` 1 (`:46`) | **RETAIN** |
| H7 | Watch registry + results | `~/.loom/watches.json`, `~/.loom/logs/watch-results.log` (`watch_registry.rs:273`, `:285`) | `watch_registry.rs:326`, `:354` | `WATCHES_VERSION` (`:69`) | **RETAIN** |
| H8 | Fleet host roster | `~/.loom/fleet.json` (`fleet/mod.rs:710`) | `fleet/mod.rs:797` | Fields | **RETAIN** |
| H9 | Daemon liveness markers | autonomy-desired marker (`daemon_start/marker.rs`), pidfile (`daemon_pidfile.rs`), heartbeat (`daemon_heartbeat.rs`), `.daemon.pid` (`autonomy_marker.rs:65`), operator-stop record (`operator_stop.rs:50`), host opt-out (`host_optout.rs`), idle-exit marker (`idle_exit.rs:20`) | Their modules | Text / JSON | **RETAIN** (OS-level supervision contract) |
| H10 | Dispatch / admission ledgers | drain-paused ledger (`ipc/drain_ledger.rs:47`), `ram-peaks.json` (`ram_peaks.rs:264-271`), `disk-footprints.json` (`disk_footprint.rs:632-642`), bucket book (`forge_bucket_book.rs:62`), stale-blocked release status (`stale_blocked/release_outcome.rs:27`), fleet-captain arm registry (`fleet_captain.rs:374`), fleet-alert state (`fleet_alert/task.rs:147`), safehouse completed (`safehouse.rs:2599`, being deleted by #11112) | Their modules | Mostly unversioned | **RETAIN** (each is the sole authority for its own observation) |

## 7. Adjacent stores (catalogued, out of consolidation scope)

These were found by the R3-R5 sweeps and are listed so the coverage claim is
complete. None of them holds execution, telemetry or configuration-snapshot
data. They are **RETAIN**.

- **Locks and mutexes**, which hold no data: `.loom/locks/{build-slot, issue-filing, worktree-add, docs-guide-lock}` (`build_slot.rs`, `filing_lock.rs`, `worktree_cli/lock.rs`); the machine in-flight registry `owner.json` (`inflight.rs:97`); the chain-head merge lock (`merge_pr/chain_lock.rs:60`); the ceiling control lock (`runtime_preference/ceiling.rs:123`); token-pool `.lock` siblings; the native binding locks (`native_tools/provision/shared.rs`).
- **Credentials**, which are secret-bearing and stay outside every repo per policy: `~/.loom/tokens/` and its `.bad_tokens`, allowlist, failure counts, rotation cursor, `ranking.json`, `.ranking.weekly.json`, `.ranking.classes.json` (`tokens.rs:68`, `tokens_pool/*`); session holds (`tokens_pool/session_hold.rs:55`); `.session-managed.json` (`tokens_pool/session_lifecycle.rs:531`); account health (`tokens_pool/health.rs:431`); codex profiles and the profile ledger (`tokens_pool/paths.rs:90`, `tokens_pool/profile_ledger.rs:37`); the API-key pool and its `.bad_accounts.json`, `.limits.json`, `.sync_state.json` (`api_keys_pool/paths.rs:54`, `api_keys_pool/bad_marks.rs:58`, `api_keys_pool/limits.rs:42`, `api_keys_pool/sync.rs:61`); the private-workspace leases (`tokens_pool/private_workspace/lease.rs:24`); the forge identity sidecar (`forge_identity/sidecar.rs:22`); and `.loom/gh-config*/`.
- **Caches**, rebuildable from their sources: the forge ETag / listing cache (`forge_etag_store.rs:60`); the forge-egress doctor cache (`forge_egress/gate.rs:40`); the package cache (`native_readiness/package_cache.rs:369`); `usage-cache.json` (`script_helpers/usage.rs:150`); `pr-merge-state.json` (`sweep_outcome_summary.rs:588`); `metrics_state.json` (`metrics_collector.rs:79`); the transcript archive manifests (`activity/transcript_archive.rs:29`); the token-ranking refresh summaries (`token_ranking_refresh.rs`).
- **Install and provisioning artifacts**: `.loom/manifest.json` (`defaults/scripts/verify-install.sh:117`); the daemon install-state record (`daemon_update/provision/txn.rs:69`); the native session record (`native_tools/provision/reap.rs:63`); `gh-config-rollback.json` (`forge_egress/publication.rs:197`); the resync pins (`resync_pin.rs:242`); release adoption (`release_fetch/source.rs:109`); fleet experiments (`script_helpers/fleet_experiment/lifecycle.rs:92`).
- **Audit and operator logs**: `worktree-removals.log` (`worktree_ops/removal_log.rs:17`), `stash-retirement.log` (`stash_retirement.rs:47`), `guard-decisions.log` (`mcp_tool_guard.rs:507`), `main-quarantine.log`, the role and daemon logs, and `native-tools/policy-timeouts.jsonl` (`native_tools/guard.rs:95`).
- **External stores read but never written**: claude-monitor's `usage.db` (`tokens_pool/monitor_db.rs:54`, `limit_calibration.rs:113`) and OpenCode's `opencode.db` (`opencode_usage.rs`).
- **Signals**: `.loom/signals/` (`defaults/scripts/signal.sh:39`).

## 8. Duplicated durable data

| ID | Same fact stored in | Evidence | Consequence today |
|----|---------------------|----------|-------------------|
| D1 | **Claim liveness** in five local places (E1 `sweeps.json`, E2 `owner.json`, E5 `.loom/claims`, E6 `issue_claims`, E13 `spawn-loop-state.json`) and on the forge (label + lease) | `worktree_ops/orphan_recovery.rs:11-18` *unions* the local sources. `live_claim.rs:8-12` tabulates each source's weakness. | Readers must union sources and fail safe. The #4275 incident: 7 dispatches in 77 minutes (`live_claim.rs:14-21`). |
| D2 | **Child PID / model / pgid** in E1 and E2, plus the in-memory registry | `sweep_registry/locks.rs:170-181` (`model` added to `owner.json` because a restart erased it) | Every new field needs a schema change in two stores |
| D3 | **Checkpoint completion** in E3 and as a span in E15 | `cli/sweep_checkpoint.rs:121-131`: write the file, then `observability::lifecycle::checkpoint_completed` (`observability/lifecycle.rs:447`) | Two durable copies with different durability (file fsync vs. trace journal) |
| D4 | **Terminal outcome** in E8 (`OutcomeRecord`) and T1 (`SweepOutcomeRecord`), at the same call sites | `sweep_registry/outcome_journal.rs:249` and `:1121`. Same rotation. | Two files and two rotations. Readers pick one: summary reads both (`sweep_outcome_summary.rs:310`, `:341`). |
| D5 | **Export envelopes** in T1/T2/T7 *and* in T10 *and* (for CI) in the T8 ledger | `observability/backfill.rs:31-45`, `observability/queue.rs:1-30`, `ci_telemetry/ledger.rs:1-12` | Up to three copies of one envelope. Two cursor schemes (timestamp at `observability/backfill.rs:115`, byte at `ci_telemetry/export.rs:46`). |
| D6 | **PR-less tally** reconstructed from T1 plus two sidecars | `sweep_registry/prless_retry/durable.rs:1-40` | An execution decision reads a telemetry journal |
| D7 | **Dispatch lineage** read back from T1 on every dispatch | `sweep_registry/start_facts.rs:1-5`, `:33` | Telemetry is on the dispatch path, with a whole-file read |
| D8 | **Failed-roll target** in H1 `roll_attempt` and H4 | `auto_update/pause_resume/attempt.rs:16-35` | Intentional (the rollback path reads H4). Kept, and documented in Q7. |
| D9 | **Activity DB location**: daemon `~/.loom/activity.db` vs. `loom-api` `<workspace>/.loom/activity.db` | `daemon_service.rs:259` vs. `loom-api/src/main.rs:59` | `loom-api` reads a different (likely empty) database. **FIX**. |

## 9. Target design: one journal per concern

### 9.1 Shared journal mechanics (one implementation)

All three journals use one module, `loom_daemon::journal`. It generalises
`telemetry/trace/journal.rs`, the only multi-process journal in the tree that
already has every property needed here:

- **One writer code path, serialised across processes** by an advisory lock on
  the active segment, held with a bounded retry (`telemetry/trace/journal.rs:78`).
  "Single writer" means one module and one lock. The daemon and the
  `loom-daemon` CLI subcommands (for example `sweep-checkpoint write`) are the
  same crate, so they share it. Q2 discusses why IPC was not chosen.
  *Implemented deviation (2a, #11345):* the lock is a stable per-stream
  `<stream>/.lock` file, not the active segment, whose identity changes on rotation.
- **Segments**: `<journal>/<stream>/<seq:010>.jsonl`. A new segment starts
  at a size bound. The segment holding the oldest unacknowledged cursor
  position is never deleted. This is the #11045 rule (`ci_telemetry/rotation.rs`)
  applied to every stream.
- **Durability**: append the whole line, then `sync_data`. The first append
  to a new segment also fsyncs the directory (`telemetry/trace/journal.rs:101-117`).
- **Torn-tail repair**: only by the lock holder. Readers never repair
  (`ci_telemetry/journal.rs:35-52`, `telemetry/trace/journal.rs:137-143`).
- **Bounds**: a per-entry cap (32 KiB, as the trace journal) and a per-segment cap.
- **Cursors**: one byte cursor per consumer, written with temp + rename + fsync
  (`telemetry/trace/journal.rs:432`).

### 9.2 J1, the execution journal

**Location (Q1):** `~/.loom/journal/execution/` (machine-level, override
`LOOM_JOURNAL_DIR`). Today's liveness journal is machine-level so that every
daemon instance on the host sees it (`live_claim.rs:36-44`), and local
evidence is host-scoped by construction (ADR-0025).

**Single writer:** the `journal::execution` module. Today's callers
(`SweepRegistry`, `sweep-checkpoint` CLI, `validate_phase`, the PR-less tally)
call it instead of their own file code.

**Envelope (every journal):**

```json
{
  "v": 1,
  "seq": 1842,
  "id": "01J…",
  "at": "2026-10-10T16:18:13.170Z",
  "writer": {"host": "host-34209a6e", "pid": 4242, "binary": "0.19.1019+febfc4335…"},
  "kind": "checkpoint.completed",
  "subject": {"workspace": "/…/loom", "issue": 11288, "sweep_id": "sweep-issue-11288-…"},
  "data": {}
}
```

`v` is the envelope major. `id` is a ULID, deterministic where a natural key
exists so that replays dedupe. `seq` is monotonic per stream and makes gaps
detectable. `writer.binary` follows the trace-identity policy (version + full
SHA). `data` depends on the kind. New fields are optional only (§11.4).

**Execution record kinds (they replace E1-E3, E8-E10, E16):**

| Kind | Replaces | `data` |
|------|----------|--------|
| `claim.acquired` | `owner.json` create (E2), `sweeps.json` upsert (E1) | `lock: issue\|pr`, `owner_pid`, `provisional` |
| `claim.child_recorded` | `record_child_pid_in_lock` (E2), E1 pid | `child_pid`, `pid_start_time`, `pgid`, `model`, `effort`, `scope_unit`, `resume_handle`, `overflow`, `credential {source, provider, account}` (E16) |
| `checkpoint.completed` | E3 file, D3 | `phase`, `attempt`, `model`, `pr_number`, `task_id`, `jev {tier, confidence}` |
| `sweep.terminal` | E8, the execution half of T1 | the `OutcomeRecord` fields (`sweep_outcomes.rs:142`+), `prless_counted: bool` |
| `prless.cleared` | E9 clears sidecar | `reason` |
| `claim.released` | E1 removal, lock release | `outcome: released\|retained\|lost` |
| `recovery.event` | E10 | the existing event payload |
| `snapshot` | (new) compaction | the materialised projections (§10) as of `seq` |

### 9.3 J2, the telemetry journal

**Location:** `<ws>/.loom/state/journal/telemetry/<stream>/`. Telemetry is
workspace-scoped today, and `.loom/state/*` is already ignored wholesale
(`init/post_init.rs:246`).

**Single writer:** `journal::telemetry::append(stream, TelemetryEnvelope)`.
Shell producers (T5, T6) go through a `loom-daemon telemetry append`
subcommand.

**Record:** the existing `TelemetryEnvelope` (`telemetry/envelope.rs:10`) as
`data`, so the backend wire schema does not change.

**Streams (Q3):** `sweep` (T1), `role_tick` (T2), `rework` (T3),
`merge_queue` (T4), `shell` (T5), `experiment` (T6), `ci` (T7). Each stream
has its own segments, so a reader of `sweep` never scans gigabytes of `ci`.
They all share one envelope, one writer and one cursor implementation.

**Export:** each exporter keeps one cursor per stream. The sender reads from
those cursors. T10 (the full-copy queue) and T11 (the timestamp cursor) become
unnecessary.

### 9.4 J3, the configuration-snapshot journal

**Location:** `<ws>/.loom/state/journal/config/`.

**Single writer:** `journal::config`, called from config resolution at daemon
start and on live reload, from `fleet_store::render`, and from `fleet_sync`.

| Kind | `data` |
|------|--------|
| `config.resolved` | `effective_hash` (the same function as `planner_config_hash`, `observability/fleet_state.rs:634`); `tiers[{tier, path, sha256\|absent}]`; `body`, redacted and included **only** the first time a hash is seen (content-addressed) |
| `config.rendered` | `target`, `backup`, `store_commit`, `restart_required_keys[]`, `daemon_pid_at_render` (replaces C6) |
| `fleet.sync` | the `FleetSyncStatus` fields (replaces C7) |

Config inputs (C1-C4) stay authoritative files. J3 records what was
*resolved*, not what is *desired*.

## 10. Projections, rebuild and shadow-write divergence detection

### 10.1 Retained projections and how each is rebuilt

| Projection | Rebuilt from | Rule |
|------------|--------------|------|
| `owner.json` (E2) | J1 `claim.*` for that lock | The latest `claim.acquired` plus later `claim.child_recorded`, ending at `claim.released`. Startup rewrites a missing or torn `owner.json` for a held lock directory. |
| Host liveness view (E1, until deleted) | J1 | Subjects with `claim.child_recorded` and no later `claim.released`/`sweep.terminal`, filtered by `pid` **and** `pid_start_time` liveness (this defeats PID reuse, which E1 cannot today) |
| Checkpoint view (E3, until deleted) | J1 `checkpoint.completed` | The last record per `(workspace, issue)` after the last `claim.released` that has a terminal outcome |
| PR-less floor (E9) | J1 `sweep.terminal{prless_counted}` and `prless.cleared` | The same window rule as `sweep_registry/prless_retry/durable.rs` |
| Lineage (D7) | J1 `sweep.terminal` index | An in-memory index built at startup and kept by appends. No whole-file read per dispatch. |
| Merge-queue dedupe (T4) | J2 `merge_queue` keys | A key set rebuilt by scanning retained segments plus the last `snapshot` |
| CI seen set (T8) | J2 `ci` keys plus ledger watermarks | Q4 |
| Pending restart (C6), sync status (C7) | J3 `config.rendered`, `fleet.sync` | Last record. "Resolved" means the current daemon PID differs from `daemon_pid_at_render`. |
| `loom-daemon status` sections | All of the above | Read from projections, never from legacy files after Phase 3 |

Compaction writes a `snapshot` record containing every projection's state as
of `seq`. A rebuild then reads only the last snapshot and the tail.

### 10.2 Divergence detection during shadow writes (Phases 2-3)

1. **At the write site.** Each legacy write (E1/E2/E3/E8/E9/T*/C6/C7) is
   followed by the J1/J2/J3 append in the same function, and the legacy write
   stays first. With `persistence.shadowVerify` on (default on in Phase 2),
   the write site then computes the projection for that one key from the
   in-memory projection state and compares it with the value it has just
   written.
2. **Normalisation.** The comparison ignores fields the legacy store does not
   carry. Timestamps are compared at whole seconds. The same dead-PID prune
   that E1 applies on read is applied to both sides. Absent and `null` are
   treated as equal.
3. **On a mismatch.** The `loom.persistence.divergence{concern, store, field}`
   counter is incremented, a WARN is logged once per `(store, key)`, and a
   `journal.divergence` record is appended to the same journal, so the
   divergence itself is durable and can be queried.
4. **Full sweep.** `loom-daemon journal verify --concern <c>` rebuilds every
   projection from scratch, diffs it against every legacy store, and exits
   non-zero on any unexplained difference. It runs at daemon startup (logged)
   and is also available on demand.
5. **Phase 3 cutover gate.** A concern's reader flips only after zero
   unexplained divergences over at least 7 days and at least 200 terminal
   sweeps on at least 2 hosts. The flip is a config flag
   (`persistence.<concern>.read = legacy | journal`, env override), so it can
   be reversed.

## 11. Failure semantics: restart, corrupt/partial/stale, version skew, downgrade

None of this changes a production contract in Phase 1. These are the rules
Phases 2-4 must implement and test.

### 11.1 Restart

The first writer to take the lock repairs a torn tail, reads the last `seq`,
and resumes from there. Projections load the last `snapshot` and replay the
tail. Today's per-store restart paths have five different shapes:
`reconstruct()` (`sweep_registry/locks.rs:883`), `adopt_live_journal_sweeps`
(`:1214`), `DurableQueue::open` replay, backfill re-scan, and CI ledger replay.
They collapse into the one replay path.

### 11.2 Corrupt, partial and stale records

- **Partial (torn tail):** truncated only by the lock holder. Readers stop
  at the last complete line.
- **Corrupt complete line** (bad JSON, over the entry cap, unknown envelope
  `v`): skipped and counted in `journal.corrupt_lines`, and never fatal. This
  keeps the soft-fail stance of every current store (`sweep_journal.rs:115-121`).
  The segment is copied once to `…/quarantine/` for diagnosis.
- **`seq` gap or regression:** logged and counted. The projection still applies
  records in file order. A regression (`seq` reused after a restore) forces a
  full rebuild.
- **Stale:** a journal record never proves that something is live. Liveness
  always needs the PID + start-time probe on this host, and the forge lease
  across hosts (ADR-0025). No liveness evidence still means "treat as alive"
  (#3651).

### 11.3 Concurrent writers

Lock contention past the retry budget defers or drops the record and logs it,
which is the trace journal's behaviour (`telemetry/trace/journal.rs:93-95`).
Execution-critical kinds (`claim.*`, `checkpoint.completed`) instead retry,
then fail the caller. While the legacy store is still written, the caller's
behaviour is unchanged.

### 11.4 Version skew

The envelope `v` is a major version. Readers accept their own major, ignore
unknown `kind`s and unknown fields, and never reject on a newer minor.
Writers add optional fields only within a major. If a reader meets an unknown
**major**, it reads that concern from the legacy store while one exists, and
after Phase 4 it fails closed with a health signal (`health` non-green). The
daemon and the CLI on one host can be different builds, and that is safe
because both are bound by the same additive rule.

### 11.5 Downgrade

- **Phases 2-3:** every legacy store is still written, so a downgraded binary
  reads legacy stores exactly as it does today. Journals are ignored by old
  binaries.
- **After Phase 4:** a binary older than the retirement release would find
  the legacy files missing. Its fail-safe reads mean that E1 absent is treated
  as "alive" (#3651) and E3 absent causes a resume from scratch. That costs
  re-work but is not unsafe. Mitigations:
  1. The retirement release raises the fleet `loom_min_version` floor, so
     auto-update will not roll below it.
  2. For one release, `loom-daemon journal materialize-legacy` re-creates the
     legacy files from projections.
  3. The rollback contract is untouched: H3's frozen v1 core and H4 are
     explicitly not migrated.

## 12. Decomposition and revised estimates for Phases 2-5

The epic's planning bounds were 9-18 issues in total. The inventory supports
**14-20**. The extra issues come from separating CLI-side writers and the
dead-store cleanup, so that each Phase 4 group can be approved on its own.

| Phase | Issue | Scope (rows) | Reversible? |
|-------|-------|--------------|-------------|
| **2 (shadow, additive)** — 4-6 issues | 2a | `journal` core module: segments, lock, torn-tail repair, envelope, cursor, `journal verify` skeleton. A generalisation of `telemetry/trace/journal.rs`, with no callers yet. | Yes (no callers) |
| | 2b | J1 shadow appends at the daemon sites (E1, E2, E8, E9, E10, E16) plus divergence checks | Yes (flag) |
| | 2c | J1 shadow appends from the CLI (`sweep-checkpoint write`, E3) | Yes |
| | 2d | J2 shadow appends (T1-T6) plus the `telemetry append` subcommand. T7 is optional (Q4). | Yes |
| | 2e | J3 `config.resolved` / `config.rendered` / `fleet.sync` | Yes |
| **3 (migrate readers, flagged)** — 5-7 issues | 3a | Liveness readers → J1 projection: `live_claim`, `claim_reconciliation`, `building_heal`, `startup_adoption`, `orphan_recovery`, `locks::adopt`, `cli/status` | `persistence.execution.read` |
| | 3b | Checkpoint readers → J1, including the CLI `read/phase/attempt/model` | same |
| | 3c | Outcome readers → J1/J2: summary, overhead, `start_facts` lineage, PR-less floor | same |
| | 3d | Export from J2 cursors. T10/T11 kept in shadow. | `persistence.telemetry.read` |
| | 3e | Status and config readers → J3 (C6, C7) | `persistence.config.read` |
| | 3f | **FIX** independent of the gate: `loom-api` activity.db path (D9) and atomic `owner.json` writes (E2) | Ordinary change |
| **4 (retire; explicit operator approval of each diff)** — 4-5 issues | 4a | Zero-writer or zero-reader formats: E6 (+ IPC `ClaimIssue`), E11, E12, E13 reader, E14 reader | No (format deletion) |
| | 4b | Execution legacy: E1, E3 files, E8, E9 sidecars, E10; E5 depending on Q5 | No |
| | 4c | Telemetry legacy: T1-T6 per-kind files, T10, T11 | No |
| | 4d | Config/control: C6, C7, H2 (Q7) | No |
| **5 (docs + cleanup)** — 1-2 issues | 5a | `defaults/docs/daemon-reference.md` and `troubleshooting.md` path tables; migration note under `docs/migration/`; `EPHEMERAL_PATTERNS` and `check-main-clean.sh` allowlist updates | — |

The Phase 4 approval gate is unchanged. No 4x issue may land before the
operator explicitly approves its diff. Phases 2-3 are additive and can be
reversed with flags.

## 13. Anticipated net reduction

**Stores in the consolidated concerns** (§3-§6 rows whose disposition is not
RETAIN):

| Concern | Before | After | Δ |
|---------|-------:|------:|--:|
| Execution | 15: E1, E2, E3, E5, E6, E7, E8, E9 (×2 sidecars), E10, E11, E12, E13, E14, E4 | 4: J1, `owner.json` projection, E4, E7 | **−11** (−10 if Q5 keeps E5) |
| Telemetry | 13: T1, T2, T3, T4 (log + seen), T5 (×2), T6, T7, T8, T10, T11, T9 export-cursor | 3: J2, per-exporter cursor file, CI seen index | **−10** |
| Config snapshot | 2: C6, C7 (C8 does not exist) | 1: J3 | **−1** (and config hashes become resolvable) |
| Host control | H1, H2 | H1 | **−1** |
| **Total** | | | **−23** (−22) |

**Recovery paths.** The in-scope stores have about 22 distinct
write/recovery implementations. Examples: tmp.pid + rename with no fsync (E1,
E9, T11), plain non-atomic `fs::write` (E2, H2), NamedTempFile + dir fsync
(E3), append + single-`.1` rotation (E8, T1, T2), whole-file rewrite (T10),
fsync-commit ledger + compaction (T8), cursor-gated rotation (T7), seen-dir
dedupe (T4), shell appends (T5), and `write_json_file` (E5, E10). After
consolidation there are **two**: the `journal` segment writer and reader (§9.1)
and the control-state snapshot pattern (H1), plus SQLite for the
out-of-scope analytics database.

**Reader burden.** After Phase 3, answering "is issue N live on this host?"
reads one projection plus the forge lease, instead of five unioned local
sources (D1).

## 14. Unresolved choices, ranked

| Q | Choice | Ranked options (recommendation first) |
|---|--------|----------------------------------------|
| Q1 | J1 location | **1.** Machine-level `~/.loom/journal/execution/`: every daemon instance shares it, as E1 does today, and local evidence is host-scoped (ADR-0025). **2.** Per-workspace `.loom/state/journal/execution/`: simpler ignore rules, but instances in worktrees and in the parent checkout need related-path matching (`live_claim.rs:39-43`). **3.** Both: rejected, because that is the duplication this epic removes. |
| Q2 | Single writer across processes | **1.** One module plus an advisory file lock, so the CLI writes directly. The trace journal proves the approach, and it works with no daemon running (`--no-daemon` sweeps). **2.** The daemon is the only writer via IPC, with a spool fallback: one more moving part, and the spool is a second store. **3.** Per-process files merged by readers: violates "single writer". |
| Q3 | Telemetry partitioning | **1.** One journal with a segment directory per stream and a shared envelope, writer and cursors. **2.** One interleaved file: the `sweep` readers would scan CI volume (T7 reached 5.4 GB, `ci_telemetry/rotation.rs:3-6`). **3.** Keep the per-kind files with a shared writer library: the smallest diff, but it removes no stores. |
| Q4 | CI ledger (T8) | **1.** The ledger keeps keys and watermarks only. The J2 `ci` append, fsynced, becomes the commit, which removes the envelope copy in D5. **2.** Leave `ci_telemetry` untouched: its exactly-once contract is coherent and recently hardened (#11045), so this is a low-risk deferral. **3.** Journal-only with a compacted seen-set snapshot: largest change, and the riskiest for exactly-once. |
| Q5 | `.loom/claims` (`loom-claim`, E5) | **1.** Retire it in favour of the forge lease + claim lock (E2). This needs an operator call because it is agent-facing (`builder-worktree.md`). **2.** Keep it, unchanged. |
| Q6 | Is J3 worth a new store? | **1.** Minimal J3 replacing C6 and C7, with content-addressed `config.resolved` so the `fleet.state` hashes can be resolved. **2.** Defer J3 and keep C6/C7. Net −1 is small, so this is a legitimate deferral. |
| Q7 | Host-control consolidation | **1.** Fold H2 additively into H1 (the `roll_attempt` precedent: no schema bump) and **keep H4 separate**, because the old binary reads it after a failed roll (`auto_update/pause_resume/attempt.rs:16-26`). **2.** Fold both: rejected, because H1 rejects unknown versions (`auto_update/persisted_state.rs:175`) and that could blind the rollback path. |
| Q8 | Sweep-run registry (E7) | **1.** Keep it until the shell sweep driver moves to Rust (ADR-0018). **2.** Port it to J1 `sweep_run.registered` in Phase 3. |

## 15. Out of scope

- Forge authority for issues, PRs, labels, leases and park records (#5057,
  ADR-0025), and every forge-side store.
- Credential stores, caches, locks, install artifacts and audit logs (§7).
- The `activity.db` analytics schema, apart from deleting E6 and fixing D9.
- Any code, schema migration or reader change in this phase.
