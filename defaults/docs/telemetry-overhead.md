# Instrumentation overhead on a representative run

`loom-daemon telemetry-overhead` answers one narrow, checkable question: when
Loom traces a sweep, how much wall time and how many bytes does the
instrumentation itself add, and do the emitted attributes/events stay inside
their declared caps?

It is **not** the synthetic fixture generator
([`telemetry-fixtures.md`](telemetry-fixtures.md)). That generator hand-builds
span records and therefore could not measure instrumentation cost at all. This
harness drives the real path instead: the same `observability::lifecycle` entry
points a dispatched sweep uses, the same journal fsync per boundary, the same
backfill drain onto a durable queue, and the same `SpanRecord::bounded`
emission policy.

```console
loom-daemon telemetry-overhead --repetitions 5 --output ./overhead.json
```

Both measured arms run inside throwaway workspaces. No provider, forge, or
network endpoint is contacted. The only path read outside those workspaces is
the reference workspace's own sweep-outcome journal, read-only.

| Flag | Meaning |
| --- | --- |
| `--repetitions N` | Runs per arm; every reported time is a median over `N`. Default 5. |
| `--tools-per-attempt N` | Owned `loom.tool` spans per role attempt. A **parameter of the shape being measured**, not a measured fleet average — Loom owns a tool span only at its own native-tool bridge, so the true count depends on the runtime. Default 4. |
| `--reference-workspace DIR` | Workspace whose recorded sweep outcomes supply the reference denominator. Read-only; defaults to the current directory. |
| `--output PATH` | Also write the report here. It is always printed to stdout. |

The command requires an OTLP-enabled binary. Without the `otlp` feature nothing
is instrumented, so it refuses rather than reporting a zero that would read as
"free". Check with `loom-daemon telemetry-capabilities --require-otlp`.

## Measure a release build, and say which build you measured

A debug build's absolute nanosecond figures are not the fleet's. Measure the
same profile the fleet runs, and record the profile alongside the numbers —
an overhead figure without its build profile, host and commit is not evidence.

```console
cargo build --release -p loom-daemon --features otlp
/path/to/target/release/loom-daemon telemetry-overhead --output ./overhead.json
```

## The representative run

The default shape is the repair waterfall this instrumentation exists to make
legible: a Builder success, a **rejected** Judge, a Doctor recovery, a second
**accepted** Judge, then merge. That is deliberately the longest ordinary
lifecycle, so the reported overhead is an upper bound among normal outcomes
rather than a best case. With the default four tool spans per attempt it
persists 41 spans — one sweep root plus phase/attempt/preflight/run/tools for
each of the five phases.

The same execution is both the measurement and the shape fixture: the unit
tests assert the waterfall nests correctly and that both Judge attempts survive
as distinct spans with distinct attempt numbers, using the very run whose cost
is reported. A passing overhead number therefore cannot describe a span graph
nobody checked.

## Reading the report

`added_median_ns` is the instrumentation cost of one representative run:
median instrumented wall time minus the median of the identical call sequence
with tracing disabled. With tracing off every lifecycle entry point
short-circuits, so the baseline is the same program minus the instrumentation,
not a different one. `measure` asserts up front that one arm is tracing and the
other is not — an ambient `LOOM_OBSERVABILITY_*` override that silently
equalised the arms would otherwise produce a confidently wrong number.

`bytes` reports what the run persisted, because an exporter that is fast but
writes megabytes is not cheap: `journal_bytes` on disk before the durable
drain, and `bounded_record_bytes` for the span records handed to the exporter
after bounding. The fixed per-batch OTLP resource/scope envelope is excluded
because it does not scale with span count.

`bounds` is realised, not declared — the largest attribute value, attribute
count, event count and link count actually observed after `bounded()`, plus the
sorted list of attribute keys that survived the allowlist. The list is reported
so a reviewer can check it instead of trusting a boolean.

`reference` is the denominator, and it is **observed, not invented**: p50/p90 of
this host's own recorded sweep durations, with the sample size named.
Zero-length records are excluded as unmeasured runs rather than counted as
zero-second sweeps, which would deflate the denominator. Percentiles are
nearest-rank over the sorted sample, never interpolated, so each one is a
duration the host actually recorded — over ten records p90 is the ninth value,
not the maximum. A host with no recorded history reports `reference` and
`overhead_fraction_of_p50` as **absent** — an unknown denominator is never
rendered as zero or guessed.

`excludes` names what the number is not, so no reader mistakes it for
end-to-end cost: network export latency to a real backend, backend
ingestion/indexing, and any provider-side cost. A run with no model work has no
model cost to attribute, and this harness never calls a provider.

## Recorded measurement

A point-in-time record, **not a budget and not a threshold** — nothing gates on
these numbers, and they are kept only so a later measurement has something to be
compared against. Re-run the command rather than citing this table as current.

| Field | Value |
| --- | --- |
| Measured | 2026-09-22, from the working tree that introduced this command |
| Build | `debug` profile, `--features otlp`, macOS aarch64 |
| Host state | 1-minute load average ≈ 37 (a busy multi-sweep host, not an idle one) |
| Shape | 5 phases (repair waterfall), 4 tool spans per attempt, 41 spans, 5 repetitions |
| `added_median_ns` | 5,572,089,583 (≈5.57 s per representative run) |
| `added_ns_per_span` | 135,904,623 (≈136 ms per span boundary pair) |
| `journal_bytes` | 44,751 |
| `bounded_record_bytes` | 18,661 (`bytes_per_span` 455) |
| `bounds` | max attribute value 14 B, 7 attributes, 0 events, 0 links per span |
| `reference` | 712 observed sweeps, p50 109 s, p90 2,846 s |
| `overhead_fraction_of_p50` | 0.0511 |

Read it with its conditions attached, in both directions. It is a **debug build
on a saturated host measuring the longest ordinary lifecycle**, so it is an
upper bound, not a fleet figure — a release build on an idle host will be
materially cheaper. But 5% of the p50 observed sweep is not nothing, and the
p50 here (109 s) is low because short-lived and failed dispatches are sweeps
too; against p90 the same cost is ≈0.2%. The cost is dominated by the
per-boundary durable journal write that makes the trace survive a crash, which
is the point of the design rather than an accident of it.

The `bounds` row is the acceptance-relevant half: every attribute key emitted
survived the allowlist, and the realised maxima sit far under the declared caps
(256 B per value, 32 events, 16 links).

## What this cannot establish

This is an offline measurement of Loom's own instrumentation. It does not, and
must not be read to, establish backend ingestion behaviour, export latency
against a real endpoint, resolved provider/model identity for a live run, or
that backend log links resolve to the relevant spans. Those need an authorized
live run against a configured backend and are tracked as separate evidence.
