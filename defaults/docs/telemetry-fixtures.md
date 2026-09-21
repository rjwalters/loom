# Synthetic telemetry comparison fixtures

`loom-daemon telemetry-fixture` writes reproducible test input and a versioned
manifest for independent ClickStack and SigNoz queries. It does not contact a
provider, mutate a forge, run a daemon, or measure either backend. The shared
comparison issue remains open until its live acceptance criteria pass.

```console
loom-daemon telemetry-fixture --run-id comparison-001 --start-time 2026-09-21T12:00:00Z --output ./comparison-001
loom-daemon telemetry-export --input ./comparison-001/envelopes.jsonl --endpoint http://127.0.0.1:4318 --key-file /private/path/collector-ingress.key
```

The output directory must be new and its parent must exist. The generator never
overwrites a trial. It publishes `manifest.json` last; an interrupted directory
without that file is incomplete. The exporter needs an OTLP-enabled binary and
an explicitly configured Collector. Generation itself works without OTLP.

Choose the required timestamp inside the backend retention and query windows.
There is no historical default. Identical run ID and anchor
produce identical input bytes; repeating them tests duplicate delivery. Use a
new run ID for a distinct trial. IDs include the run and scenario, so issue 18
in two repositories cannot cross-parent. The host resource also contains the
run ID, allowing backend queries to isolate the trial.

## Expected dataset

The version-1 manifest describes **37 spans, 14 correlated logs, and 3 metric
data points**. These are expected distinct identities, not accepted issues or
observed backend counts. Delivery remains at least once.

| Scenario | Expected distinction |
| --- | --- |
| Success | Builder, Judge, merge, successful root |
| Repair | Rejected Judge, Doctor, second Judge with a different span ID, merge |
| Preflight rejection | Admission failure, no model/runtime launch metadata |
| Cancellation | Explicit cancelled outcome, never a successful ending |
| Timeout | Explicit timeout outcome |
| Crash/incomplete | Completed preflight child with an intentionally absent root ending |
| Concurrent repositories | Overlapping timestamps and the same issue number, distinct trace IDs |
| Token-pool usage | Observed `0.0` is present; unknown usage has no data point |

The graph nests attempts under phases under a sweep, including a synthetic
runtime and owned-tool span under the successful Builder. Attempt numbers are
per role; the repaired Judge has attempts 1 and 2. Crash-time model launch is
unknown, not inferred from a completed preflight. Logs use actual OTLP trace
and span fields, not merely string attributes. A log's stable identity is the
manifest's resource host, trace ID, span ID, event type and timestamp tuple;
there is no invented provider event ID. Span timestamps/statuses, parent IDs,
allowlisted attributes, and metric values are explicit in the manifest.

All repository and runtime/model names are synthetic. The input contains the
harmless string `LOOM_SYNTHETIC_PRIVATE_PROMPT_SENTINEL_8529` under a prohibited
prompt attribute. That string must not appear in any backend record after OTLP
mapping; its presence in the local input/manifest is intentional. It is not a
real prompt or secret. Private metadata remains tagged private, but this
fixture alone cannot establish backend access-control or retention behavior.

## Independent verification

Query each backend by the manifest's host resource and timestamp bounds. Compare
distinct trace/span identities, parentage, status, log correlation, metric values
and required absences. Report missing, duplicate, unexpected and late records
separately. A receiver HTTP response, Collector health check, or raw row total
does not establish this result.

Record the exact Loom commit, Collector/product versions, query text, time
window, observation time and sanitized query results beside the manifest.
Navigate the repair and incomplete graphs in each UI. Search all signal types
for the privacy sentinel. Record whether private metadata is inaccessible to
unauthorized viewers and whether retention actually removes old records.

`backend_verification: not_performed` and the manifest's `unproven` list are
deliberate. These inputs do not demonstrate real process crash/adoption,
instrumentation correctness, two-way outage isolation, Collector restart,
buffer saturation, UI usability, per-attempt inference usage, or paid inference
cost. Those require the live
trial protocol and a measured report. Never replace these gaps with a product
winner or a claim that the comparison issue is complete.
