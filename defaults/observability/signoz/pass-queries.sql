-- Pass activity (Issue #10752): what the daemon's artifact passes did, from
-- their `pass.summary` / `pass.verdict` logs and the `invoke github` spans
-- their caller scope stamps with `github.caller`. Today one pass emits them:
-- the `loom:blocked` release pass (#10556), mechanism `stale_blocked_release`.
-- Role verdicts (Guide, Champion) are planned to land in the same records with
-- `loom.role` set.
--
-- STATUS: vocabulary-guarded by `loom-daemon/src/telemetry/kinds/pass_tests.rs`
-- (every `loom.pass.*` / `github.*` key below must be emitted by the daemon and
-- forwarded by the gateway's keep_keys). Not yet executed in CI against the
-- pinned ClickHouse.

-- 1. Release passes per repo, last 24 h, in one row per (mechanism, repo):
--    how many passes ran (and how many were refused), how many blocks they
--    released or re-parked (`released` / `reparked` / `failed` count mode `on`
--    passes only: a dry-run pass reports the same verdicts as a plan and
--    removes nothing, so those land in `planned_released` / `planned_reparked`),
--    how many artifacts they skipped and why
--    (`skip_reasons` sums the body's per-reason map), and the GitHub calls
--    the pass made by operation (`calls_by_op`, from its spans).
WITH passes AS (
    SELECT attributes_string['loom.pass.mechanism'] AS mechanism,
           lower(attributes_string['loom.repo']) AS repo,
           count() AS passes,
           countIf(attributes_string['loom.pass.outcome'] = 'refused') AS refused,
           countIf(attributes_string['loom.pass.mode'] = 'dry_run') AS dry_run_passes,
           sumIf(JSONExtractUInt(body, 'verdicts', 'released'),
                 attributes_string['loom.pass.mode'] = 'on') AS released,
           sumIf(JSONExtractUInt(body, 'verdicts', 'reparked'),
                 attributes_string['loom.pass.mode'] = 'on') AS reparked,
           sumIf(JSONExtractUInt(body, 'verdicts', 'failed'),
                 attributes_string['loom.pass.mode'] = 'on') AS failed,
           sumIf(JSONExtractUInt(body, 'verdicts', 'released'),
                 attributes_string['loom.pass.mode'] = 'dry_run') AS planned_released,
           sumIf(JSONExtractUInt(body, 'verdicts', 'reparked'),
                 attributes_string['loom.pass.mode'] = 'dry_run') AS planned_reparked,
           sum(toUInt64(attributes_number['loom.pass.skipped'])) AS skipped,
           sumMap(arrayMap(kv -> kv.1, JSONExtractKeysAndValues(body, 'skipped', 'UInt64')),
                  arrayMap(kv -> kv.2, JSONExtractKeysAndValues(body, 'skipped', 'UInt64')))
               AS skip_reasons,
           countIf(attributes_bool['loom.pass.write_cap_hit']) AS write_cap_hits,
           sum(toUInt64(attributes_number['loom.pass.github_calls'])) AS github_calls,
           argMax(attributes_string['loom.pass.version'], timestamp) AS latest_version
    FROM signoz_logs.distributed_logs_v2
    WHERE attributes_string['loom.kind'] = 'pass.summary'
      AND timestamp >= toUnixTimestamp64Nano(now64(9) - INTERVAL 1 DAY)
    GROUP BY mechanism, repo
),
calls AS (
    SELECT attributes_string['github.caller'] AS mechanism,
           lower(attributes_string['github.repo']) AS repo,
           sumMap([attributes_string['github.operation']], [toUInt64(1)]) AS calls_by_op,
           countIf(attributes_string['github.access_intent'] = 'write') AS github_writes
    FROM signoz_traces.distributed_signoz_index_v3
    WHERE name = 'invoke github'
      AND attributes_string['github.caller'] != ''
      AND timestamp >= now() - INTERVAL 1 DAY
    GROUP BY mechanism, repo
)
SELECT p.mechanism, p.repo, p.passes, p.refused, p.dry_run_passes,
       p.released, p.reparked, p.failed, p.planned_released, p.planned_reparked,
       p.skipped, p.skip_reasons,
       p.write_cap_hits, p.github_calls, c.github_writes, c.calls_by_op,
       p.latest_version
FROM passes AS p
LEFT JOIN calls AS c ON c.mechanism = p.mechanism AND c.repo = p.repo
ORDER BY p.mechanism, p.repo;

-- 2. Why is each artifact still held? The newest verdict per (repo, number)
--    in the last 24 h. Unchanged verdicts are re-emitted at least hourly
--    (LOOM_RELEASE_STALE_BLOCKED_VERDICT_HEARTBEAT_SECS), so every artifact a
--    pass still decides shows up here. `mode` and `applied` say whether the
--    newest verdict was written: `released` with `applied` false (or mode
--    `dry_run`) is a plan, and the artifact is still held.
SELECT attributes_string['loom.repo'] AS repo,
       toUInt64(attributes_number['loom.pass.number']) AS number,
       argMax(attributes_string['loom.pass.artifact'], timestamp) AS artifact,
       argMax(attributes_string['loom.pass.verdict'], timestamp) AS verdict,
       argMax(attributes_string['loom.pass.reason'], timestamp) AS reason,
       argMax(attributes_string['loom.pass.blockers'], timestamp) AS blockers,
       argMax(attributes_string['loom.pass.mechanism'], timestamp) AS mechanism,
       argMax(attributes_string['loom.pass.mode'], timestamp) AS mode,
       argMax(attributes_bool['loom.pass.applied'], timestamp) AS applied,
       fromUnixTimestamp64Nano(max(timestamp)) AS decided_at
FROM signoz_logs.distributed_logs_v2
WHERE attributes_string['loom.kind'] = 'pass.verdict'
  AND timestamp >= toUnixTimestamp64Nano(now64(9) - INTERVAL 1 DAY)
GROUP BY repo, number
ORDER BY repo, number;

-- 3. Who removed `loom:blocked`? Every removal the loom-ui webhook export
--    (`label.transition`, GitHub's event time in `at`) saw in the last 24 h,
--    matched to an applied `pass.verdict` that removed the label from the
--    same repo#n within 15 minutes. `mechanism = ''` means no daemon pass did
--    it (a role, a human, or an emitter without these records). `actor` is
--    the GitHub login, which is the same fleet App for every role and pass.
WITH removals AS (
    SELECT lower(JSONExtractString(body, 'repo')) AS repo,
           JSONExtractUInt(body, 'number') AS number,
           parseDateTime64BestEffortOrNull(JSONExtractString(body, 'at'), 3) AS at,
           JSONExtractString(body, 'actor') AS actor
    FROM signoz_logs.distributed_logs_v2
    WHERE resources_string['service.name'] = 'loom-ui-d1-export'
      AND JSONExtractString(body, 'kind') = 'label.transition'
      AND JSONExtractString(body, 'action') = 'unlabeled'
      AND JSONExtractString(body, 'label') = 'loom:blocked'
      AND timestamp >= toUnixTimestamp64Nano(now64(9) - INTERVAL 1 DAY)
),
verdicts AS (
    SELECT lower(attributes_string['loom.repo']) AS repo,
           toUInt64(attributes_number['loom.pass.number']) AS number,
           attributes_string['loom.pass.mechanism'] AS mechanism,
           fromUnixTimestamp64Nano(timestamp) AS at
    FROM signoz_logs.distributed_logs_v2
    WHERE attributes_string['loom.kind'] = 'pass.verdict'
      AND attributes_bool['loom.pass.applied']
      AND has(splitByChar(',', attributes_string['loom.pass.labels_removed']), 'loom:blocked')
      AND timestamp >= toUnixTimestamp64Nano(now64(9) - INTERVAL 1 DAY - INTERVAL 15 MINUTE)
)
SELECT r.repo, r.number, r.at, r.actor,
       anyIf(v.mechanism, abs(dateDiff('second', v.at, r.at)) <= 900) AS mechanism
FROM removals AS r
LEFT JOIN verdicts AS v ON v.repo = r.repo AND v.number = r.number
GROUP BY r.repo, r.number, r.at, r.actor
ORDER BY r.at;
