-- Measured token/cost usage queries (#8528 scope item 4, the SigNoz half of
-- ClickStack's "Loom measured usage" saved view).
--
-- Source: `loom.runtime.usage` spans (#8908, #9204, #9303) in
-- `signoz_traces.signoz_index_v3`. Schema and emission rules:
-- ../../docs/telemetry-schema.md (`loom.runtime.usage` span row) and
-- ../../docs/tracing.md. The `loom.tokens.usage_fraction` /
-- `loom.tokens.exhausted` GAUGES are a different signal answering a different
-- question (how much of a subscription's weekly window is left) — they live in
-- `quota-utilization.sql` and `fixture-queries.sql` 7, and must never be mixed
-- with the measured per-execution counters below. A pool utilization percentage
-- is not a token count and never becomes dollars.
--
-- Five things make this family easy to query WRONGLY, each of which returns a
-- plausible number instead of an error:
--
--   1. EVERY span attribute is exported as an OTLP string (the trace mapper
--      renders the whole attribute map with `kv_string`), so SigNoz files all of
--      them under `attributes_string` — including the token counts and the USD
--      estimate. Subscripting `attributes_number` for `loom.tokens.total`
--      returns 0 on every row and nothing errors. Read them from
--      `attributes_string` and convert
--      with `toInt64OrNull` / `toFloat64OrNull`, which also keeps an ABSENT key
--      (map subscript yields '') as NULL rather than as a measured 0.
--
--   2. Scope is not additive. A daemon-dispatched sweep can carry BOTH an
--      `execution`-scoped span (the whole execution) and `attempt`-scoped spans
--      (its `claude -p` child recording each role attempt) in one trace. Total a
--      unit from its `execution` spans when present, ELSE from the sum of its
--      `attempt` spans — never both. Sections 1, 2 and 6 resolve that per
--      (trace, sweep) before aggregating; section 0 shows the mix.
--
--   3. An unpriced model carries NO cost attributes at all. `Pricing::attributes`
--      omits `loom.cost.usd_estimate` entirely for a model the rate card does
--      not know, deliberately, rather than fabricating a Sonnet-rate fallback.
--      ClickHouse `sum()` skips NULLs, so a spend total silently EXCLUDES those
--      models: every section that sums dollars also reports `unpriced_models`,
--      and a non-zero value there makes the total a LOWER BOUND, not a spend
--      figure. Section 4 names the models.
--
--   4. Absence is not zero. A unit whose usage could not be determined has no
--      `loom.runtime.usage` child span at all; a unit measured as having used
--      nothing has a span whose counter is the string "0". Section 3 keeps the
--      two in separate columns. Never impute one from the other.
--
--   5. `loom.repo` is NOT on the usage span. No caller of `model_usage_spans`
--      puts it in the span's common attributes, and it is not a resource
--      attribute either (the gateway's resource allowlist keeps only
--      service.name / service.version / service.instance.id / host.id). Repo
--      attribution therefore needs a join to the other spans of the same trace
--      — section 2 does it, and reports when a trace shows more than one.
--
-- Delivery is at least once, so every section de-duplicates on the derived span
-- identity `(trace_id, span_id)` before aggregating. A replayed batch is
-- byte-identical in those two columns, which is what makes that safe
-- (`spans::model_usage_spans` derives the span id from name + scope + model, so
-- a re-emit is the same id rather than a new one).
--
-- Horizon: traces are kept 7 days on this trial (see `retention.sql`), so a
-- `since` older than that returns only what retention has not yet deleted.
--
-- Vocabulary is pinned to what the daemon actually emits and the gateway
-- actually forwards: `loom-daemon/tests/signoz_trial_artifacts.rs` fails in
-- ordinary CI if any counter, cost, pricing or scope key below drifts from
-- `counter_attributes()` / `Pricing::attributes()`, if a span name drifts from
-- `SpanName`, or if any key is read from a container other than
-- `attributes_string`.
--
-- Parameters (ClickHouse query parameters):
--   since   DateTime lower bound on span start
--   repo    'owner/name' to scope section 2 to one repository, '' for all
--   top     row cap on the ranked sections
--
-- Run every statement in one pass:
--
--   docker compose --env-file /absolute/private/signoz.env \
--     -f pours/deployment/compose.yaml exec -T \
--     loom-signoz-telemetrystore-clickhouse-0-0 \
--     clickhouse-client --multiquery \
--       --param_since='2026-09-01 00:00:00' --param_repo='' --param_top=50 \
--     < usage-queries.sql
--
-- STATUS: not yet executed against a live deployment. These are static,
-- contract-checked artifacts; `evidence.md`'s acceptance ledger records the row
-- as open until a session with the trial host runs them over real canary data.

-- 0. Preflight: did the family arrive at all, and in what shape? `scopes` above
--    1 for a sweep is the normal both-scopes case section 2's resolution
--    handles, not a fault. `priced` counts spans that carry a USD estimate;
--    `unpriced` is the rate card not knowing that model, never a $0 model.
--    Run this FIRST: if `spans` is 0, nothing below distinguishes "no usage was
--    measured" from "the usage spans never reached this backend".
SELECT scope,
       count() AS spans,
       uniqExact(model) AS models,
       uniqExact(trace_id) AS traces,
       uniqExact(sweep_id) AS sweeps,
       countIf(usd IS NOT NULL) AS priced,
       countIf(usd IS NULL) AS unpriced,
       countIf(role != '') AS with_role,
       countIf(runtime != '') AS with_runtime,
       min(ts) AS first_seen,
       max(ts) AS last_seen
FROM (
    SELECT any(attributes_string['loom.usage.scope']) AS scope,
           any(attributes_string['loom.model']) AS model,
           any(attributes_string['loom.sweep_id']) AS sweep_id,
           any(attributes_string['loom.role']) AS role,
           any(attributes_string['loom.runtime']) AS runtime,
           any(toFloat64OrNull(attributes_string['loom.cost.usd_estimate'])) AS usd,
           any(timestamp) AS ts,
           trace_id
    FROM signoz_traces.signoz_index_v3
    WHERE name = 'loom.runtime.usage'
      AND timestamp >= {since:DateTime}
    GROUP BY trace_id, span_id
)
GROUP BY scope
ORDER BY scope;

-- 1. Spend and tokens by model. The headline view: what each model actually
--    cost over the window, from the resolved scope per (trace, sweep).
--    `usd_lower_bound` is a LOWER BOUND whenever `unpriced_spans` is non-zero —
--    those spans contribute tokens but no dollars, by design (see header note
--    3). The `gen_ai.usage.*` aliases are Anthropic's DISJOINT vocabulary, the
--    same as `loom.tokens.*`: `input` is UNCACHED input, so total input is
--    `input + cache_read + cache_write`, and summing the alias beside the
--    native key would double count. Only the native keys are read here.
WITH usage AS (
    SELECT trace_id,
           any(attributes_string['loom.sweep_id']) AS sweep_id,
           any(attributes_string['loom.usage.scope']) AS scope,
           any(attributes_string['loom.model']) AS model,
           any(toInt64OrNull(attributes_string['loom.tokens.input'])) AS input,
           any(toInt64OrNull(attributes_string['loom.tokens.output'])) AS output,
           any(toInt64OrNull(attributes_string['loom.tokens.cache_read'])) AS cache_read,
           any(toInt64OrNull(attributes_string['loom.tokens.cache_write'])) AS cache_write,
           any(toInt64OrNull(attributes_string['loom.tokens.total'])) AS total,
           any(toFloat64OrNull(attributes_string['loom.cost.usd_estimate'])) AS usd
    FROM signoz_traces.signoz_index_v3
    WHERE name = 'loom.runtime.usage'
      AND timestamp >= {since:DateTime}
    GROUP BY trace_id, span_id
),
resolved AS (
    SELECT *,
           max(scope = 'execution') OVER (PARTITION BY trace_id, sweep_id) AS has_execution
    FROM usage
)
SELECT model,
       count() AS spans,
       uniqExact(sweep_id) AS sweeps,
       sum(input) AS input_tokens,
       sum(output) AS output_tokens,
       sum(cache_read) AS cache_read_tokens,
       sum(cache_write) AS cache_write_tokens,
       sum(total) AS total_tokens,
       round(sum(usd), 4) AS usd_lower_bound,
       countIf(usd IS NULL) AS unpriced_spans
FROM resolved
WHERE scope = if(has_execution, 'execution', 'attempt')
GROUP BY model
ORDER BY total_tokens DESC, model
LIMIT {top:UInt32};

-- 2. The ClickStack "Loom measured usage" parity view: spend and tokens by
--    repo / role / runtime / model. `repo` comes from the OTHER spans of the
--    same trace, because the usage span itself never carries it (header note
--    5); `repos_in_trace` above 1 means the join is ambiguous for that trace
--    and its rows must not be attributed to either repository. An EMPTY `repo`
--    means no span of that trace within the window carried `loom.repo` — widen
--    `since` (the root span can start well before a late usage child) rather
--    than reading it as a repository named "".
--
--    An empty `role`, `runtime` or `model` cell is a MISSING attribute, not a
--    measured empty value: ClickHouse yields '' for an absent map key. Read
--    this beside section 0's `with_role` / `with_runtime` counts, which say how
--    much of the window carries them at all.
WITH usage AS (
    SELECT trace_id,
           any(attributes_string['loom.sweep_id']) AS sweep_id,
           any(attributes_string['loom.usage.scope']) AS scope,
           any(attributes_string['loom.role']) AS role,
           any(attributes_string['loom.runtime']) AS runtime,
           any(attributes_string['loom.model']) AS model,
           any(toInt64OrNull(attributes_string['loom.tokens.total'])) AS total,
           any(toFloat64OrNull(attributes_string['loom.cost.usd_estimate'])) AS usd
    FROM signoz_traces.signoz_index_v3
    WHERE name = 'loom.runtime.usage'
      AND timestamp >= {since:DateTime}
    GROUP BY trace_id, span_id
),
resolved AS (
    SELECT *,
           max(scope = 'execution') OVER (PARTITION BY trace_id, sweep_id) AS has_execution
    FROM usage
),
trace_repo AS (
    SELECT trace_id,
           min(attributes_string['loom.repo']) AS repo,
           uniqExact(attributes_string['loom.repo']) AS repos_in_trace
    FROM signoz_traces.signoz_index_v3
    WHERE timestamp >= {since:DateTime}
      AND mapContains(attributes_string, 'loom.repo')
    GROUP BY trace_id
)
SELECT r.repo AS repo,
       u.role AS role,
       u.runtime AS runtime,
       u.model AS model,
       count() AS spans,
       uniqExact(u.sweep_id) AS sweeps,
       sum(u.total) AS total_tokens,
       round(sum(u.usd), 4) AS usd_lower_bound,
       countIf(u.usd IS NULL) AS unpriced_spans,
       max(r.repos_in_trace) AS repos_in_trace
FROM resolved AS u
LEFT JOIN trace_repo AS r USING (trace_id)
WHERE u.scope = if(u.has_execution, 'execution', 'attempt')
  AND ({repo:String} = '' OR r.repo = {repo:String})
GROUP BY repo, role, runtime, model
ORDER BY total_tokens DESC, repo, role, runtime, model
LIMIT {top:UInt32};

-- 3. Missing usage versus measured zero, per role attempt. This is the
--    acceptance question "missing usage stays distinguishable from zero usage"
--    asked of the SPAN family, the way `fixture-queries.sql` 7 asks it of the
--    token gauges.
--
--      usage_unknown       the attempt has NO `loom.runtime.usage` child: its
--                          transcript was absent, oversized or carried no usage
--                          rows. NOT a zero-cost attempt.
--      measured_zero_only  it has usage children and every one reports total 0.
--                          A genuine measured zero.
--      measured_usage      it has at least one child with a non-zero total.
--
--    The LEFT JOIN relies on ClickHouse's default `join_use_nulls = 0`, so an
--    attempt with no usage child reads as `usage_spans = 0` rather than NULL —
--    do not "fix" this by enabling `join_use_nulls`, which turns every
--    countIf() below into a NULL comparison that is never true.
--
--    Attempt scope only: `attempt`-scoped usage spans are children of the
--    `loom.role_attempt` span, while `execution`-scoped ones hang off the
--    execution's `loom.runtime.run` span (or the trace root) and so are not
--    per-attempt coverage. Section 0's scope split is the execution-side view.
WITH attempts AS (
    SELECT trace_id, span_id,
           any(attributes_string['loom.role']) AS role,
           any(attributes_string['loom.result']) AS result
    FROM signoz_traces.signoz_index_v3
    WHERE name = 'loom.role_attempt'
      AND timestamp >= {since:DateTime}
    GROUP BY trace_id, span_id
),
usage_by_parent AS (
    SELECT trace_id, parent_span_id AS span_id,
           count() AS usage_spans,
           countIf(total > 0) AS nonzero_spans
    FROM (
        SELECT trace_id, span_id,
               any(parent_span_id) AS parent_span_id,
               any(toInt64OrNull(attributes_string['loom.tokens.total'])) AS total
        FROM signoz_traces.signoz_index_v3
        WHERE name = 'loom.runtime.usage'
          AND attributes_string['loom.usage.scope'] = 'attempt'
          AND timestamp >= {since:DateTime}
        GROUP BY trace_id, span_id
    )
    GROUP BY trace_id, span_id
)
SELECT a.role AS role,
       count() AS attempts,
       countIf(u.usage_spans = 0) AS usage_unknown,
       countIf(u.usage_spans > 0 AND u.nonzero_spans = 0) AS measured_zero_only,
       countIf(u.nonzero_spans > 0) AS measured_usage
FROM attempts AS a
LEFT JOIN usage_by_parent AS u USING (trace_id, span_id)
GROUP BY role
ORDER BY usage_unknown DESC, role;

-- 4. Unpriced models. A model the rate card does not know gets NO USD
--    attributes, so its tokens are real but its dollars are absent. This lists
--    exactly which models the spend totals above exclude, so an incomplete
--    total is never read as a cheap one. An empty result means every model in
--    the window was priced and section 1's total is complete.
--
--    Fixing a row here is a rate-card change (`defaults/pricing.json` /
--    the compiled card), not a query change — never paper over it by
--    substituting another model's rates.
SELECT model,
       count() AS unpriced_spans,
       uniqExact(sweep_id) AS sweeps,
       sum(total) AS total_tokens,
       min(ts) AS first_seen,
       max(ts) AS last_seen
FROM (
    SELECT any(attributes_string['loom.model']) AS model,
           any(attributes_string['loom.sweep_id']) AS sweep_id,
           any(toInt64OrNull(attributes_string['loom.tokens.total'])) AS total,
           any(toFloat64OrNull(attributes_string['loom.cost.usd_estimate'])) AS usd,
           any(timestamp) AS ts
    FROM signoz_traces.signoz_index_v3
    WHERE name = 'loom.runtime.usage'
      AND timestamp >= {since:DateTime}
    GROUP BY trace_id, span_id
)
WHERE usd IS NULL
GROUP BY model
ORDER BY total_tokens DESC, model
LIMIT {top:UInt32};

-- 5. Pricing provenance. Every USD estimate names the rate card it came from
--    (`loom.pricing.source` = `asset` for the resync-delivered
--    `.loom/pricing.json`, `compiled` for the card built into the binary) and
--    the date that card was last verified. Two `verified_on` values in one
--    window mean the fleet rolled a new card mid-window: dollar figures either
--    side of it are priced differently and must not be trended as one series
--    without saying so. `daemon_versions` is the binary that priced them.
--
--    This section is a CENSUS of priced spans, deliberately NOT scope-resolved:
--    a sweep carrying both scopes contributes its dollars twice, because the
--    question here is "which rate card priced what", not "what did we spend".
--    `usd_both_scopes` is named to say so — section 1 is the spend figure.
SELECT source,
       verified_on,
       count() AS priced_spans,
       uniqExact(model) AS models,
       round(sum(usd), 4) AS usd_both_scopes,
       groupUniqArray(daemon_version) AS daemon_versions
FROM (
    SELECT any(attributes_string['loom.pricing.source']) AS source,
           any(attributes_string['loom.pricing.verified_on']) AS verified_on,
           any(attributes_string['loom.model']) AS model,
           any(attributes_string['loom.daemon.version']) AS daemon_version,
           any(toFloat64OrNull(attributes_string['loom.cost.usd_estimate'])) AS usd
    FROM signoz_traces.signoz_index_v3
    WHERE name = 'loom.runtime.usage'
      AND mapContains(attributes_string, 'loom.cost.usd_estimate')
      AND timestamp >= {since:DateTime}
    GROUP BY trace_id, span_id
)
GROUP BY source, verified_on
ORDER BY verified_on, source;

-- 6. Cache composition per model, over the resolved scope. The four counters
--    are disjoint, so `input + cache_read + cache_write_5m + cache_write_1h` is
--    total INPUT-side tokens and `cache_read_pct` is how much of that came from
--    cache rather than being re-sent. The 5-minute and 1-hour cache writes are
--    kept apart because they are priced at different rates — a run that writes
--    1-hour cache is buying a longer horizon, and collapsing them hides the
--    rate difference behind one number.
WITH usage AS (
    SELECT trace_id,
           any(attributes_string['loom.sweep_id']) AS sweep_id,
           any(attributes_string['loom.usage.scope']) AS scope,
           any(attributes_string['loom.model']) AS model,
           any(toInt64OrNull(attributes_string['loom.tokens.input'])) AS input,
           any(toInt64OrNull(attributes_string['loom.tokens.cache_read'])) AS cache_read,
           any(toInt64OrNull(attributes_string['loom.tokens.cache_write_5m'])) AS cache_write_5m,
           any(toInt64OrNull(attributes_string['loom.tokens.cache_write_1h'])) AS cache_write_1h
    FROM signoz_traces.signoz_index_v3
    WHERE name = 'loom.runtime.usage'
      AND timestamp >= {since:DateTime}
    GROUP BY trace_id, span_id
),
resolved AS (
    SELECT *,
           max(scope = 'execution') OVER (PARTITION BY trace_id, sweep_id) AS has_execution
    FROM usage
)
SELECT model,
       spans,
       uncached_input,
       cache_read_tokens,
       cache_write_5m_tokens,
       cache_write_1h_tokens,
       input_side_total,
       round(100 * cache_read_tokens / nullIf(input_side_total, 0), 1) AS cache_read_pct
FROM (
    SELECT model,
           count() AS spans,
           sum(input) AS uncached_input,
           sum(cache_read) AS cache_read_tokens,
           sum(cache_write_5m) AS cache_write_5m_tokens,
           sum(cache_write_1h) AS cache_write_1h_tokens,
           sum(input) + sum(cache_read) + sum(cache_write_5m) + sum(cache_write_1h)
               AS input_side_total
    FROM resolved
    WHERE scope = if(has_execution, 'execution', 'attempt')
    GROUP BY model
)
ORDER BY input_side_total DESC, model
LIMIT {top:UInt32};
