# Execution traces

Loom's opt-in OTLP exporter carries completed spans alongside lifecycle logs and
metrics. Trace IDs and span IDs are native OTLP fields, so a backend can join a
log to its exact execution span. Enable the `otlp` Cargo feature and configure
`observability.enabled`, `exporter: "otlp"`, an endpoint, and an ingest key file
as described in [observability](observability.md).

## Identity and process boundaries

Every new execution receives random nonzero 128-bit trace and 64-bit span IDs.
An issue number is metadata, never an identity. Before an owned sweep process is
spawned, Loom persists its root identity under `.loom/logs/trace-context/` and
passes `LOOM_TRACEPARENT` plus `LOOM_TRACE_CONTEXT_FILE` to that child. Reopening
the same execution after a daemon restart reuses its identity. A new attempt
gets a new execution identity. The parser accepts strict W3C version-00 context;
it rejects malformed, uppercase, and zero IDs.

The context directory is private and files are written atomically with fsync.
An exclusive file lock serializes creators. A corrupt, busy, or full context
store disables tracing for that launch with a diagnostic instead of delaying
issue execution. The store admits at most 1024 active executions. Terminal
instrumentation must queue the completed root before removing its context;
unfinished work retains its identity across restart. This propagation hook does
not imply that arbitrary third-party harness tools emit spans.

## Delivery and privacy

Completed spans enter the same bounded durable queue as other telemetry. Only
sampled, valid spans are exported. Span names are a fixed enumeration; attributes
are allowlisted, length bounded, and exclude prompts, source, tool arguments,
command output, environment values, headers, and credentials. Links and events
are bounded. No prompt or tool payload capture is enabled.

Span envelopes use schema version 3. Existing lifecycle envelopes remain version
2 with an optional trace context. Old queue records still decode. The native
HTTPS backend receives lifecycle envelopes with context stripped and never
receives trace-only records; use OTLP for traces.

Graceful daemon signal and IPC shutdown gives registered senders one shared
two-second final-drain budget. Export failure or timeout leaves unsent records
on disk. SIGKILL cannot flush and relies on persistence. Shutdown does not
manufacture completion for active execution spans. Delivery can duplicate an
accepted request when the sender loses the response; no exactly-once claim is
made. Trace IDs allow backend correlation, not universal backend deduplication.

## Implementation boundary

This foundation defines persistence, propagation, encoding, and shutdown.
Execution/phase/role/runtime/tool instrumentation is tracked separately in
issue #8525. The two-backend acceptance trial is tracked in issue #8529; exported
fixtures are transport evidence, not proof of live lifecycle instrumentation.
