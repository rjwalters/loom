-- Pass activity (Issue #10752): what the daemon's artifact passes did, from
-- their `pass.verdict` logs. Until #11301 the `loom:blocked` release pass
-- (#10556, mechanism `stale_blocked_release`) emitted them, and a per-pass
-- `pass.summary` (retired, #12094) rolled them up.
-- Role verdicts (Guide, Champion) are planned to land in the same records with
-- `loom.role` set.
--
-- STATUS: vocabulary-guarded by `loom-daemon/src/telemetry/kinds/pass_tests.rs`
-- (every `loom.pass.*` / `github.*` key below must be emitted by the daemon and
-- forwarded by the gateway's keep_keys). Not yet executed in CI against the
-- pinned ClickHouse.

-- 1. Why is each artifact still held? The newest verdict per (repo, number)
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

-- 2. Who removed `loom:blocked`? Every removal the loom-ui webhook export
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
