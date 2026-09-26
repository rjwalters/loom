# Trace identity and provenance policy

Loom traces are forensic records. Two rules apply to every span Loom emits:

1. **Deterministic IDs.** No production trace or span ID is random. Each ID is
   derived from the natural key of the work it records, so anyone holding that
   key can recompute the ID and find the trace.
2. **Provenance on every span.** Every span names the Loom build that created
   it, by version **and full git SHA**. Each sweep span also names the
   installed Loom surface and the exact prompt files the run used.

## 1. Deterministic IDs

Every derived ID is a prefix of SHA-256 over NUL-separated parts
(`derived_hex`). The first part is a tag naming the kind of ID:
`loom.<kind>.trace` for a trace, `loom.<kind>.root` for its root span, and
`loom.span.child` for a child span (`TraceContext::derived` /
`derived_child` in `loom-daemon/src/telemetry/trace/context.rs`). Instants
inside a key use RFC 3339 UTC with nanosecond precision.

| Trace | Trace ID is derived from |
|-------|--------------------------|
| Issue story (`loom.story.trace`) | lowercased `owner/repo` (from the checkout's GitHub `origin`), issue number — see [`tracing.md`](tracing.md) (#9037) |
| Sweep outside a story (`loom.execution.trace`) | lowercased repo key (`$LOOM_REPO`, else the workspace basename), sweep id — used for PR-set sweeps and checkouts with no GitHub origin |
| Dispatch tick (`loom.dispatch.tick`) | tick start instant |
| Pool hold (`loom.pool.hold`) | hold start instant (`since`) |
| CI run/job (`loom.ci.*`) | repo, run id, attempt (job: job id) — [`ci-observability.md`](ci-observability.md) |

Every sweep of an issue is a `loom.sweep` span in that issue's story trace.
Its span ID is derived from the story root and the sweep id
(`sweep-issue-42-1790000000`).

A child span's ID is derived from its trace ID, its parent span ID, its span
name, its `loom.role` (if any), and its start instant. Every one of those inputs
is carried on the exported span, so each ID can be checked against its own data.
Dispatch admissions also key on `loom.issue`.

Consequences:

- Every sweep of an issue, on any host, lands in the **same** trace: the
  issue's story. Each retry is a distinct sibling span because the sweep id
  carries the dispatch time. The sweep id is in the registry, the lease
  comment, logs and the `loom.sweep_id` attribute.
- Re-emitting the same work (such as a replay, a restart re-report, or a second
  host) produces the same IDs, so a backend can deduplicate on them.
- The sweep span carries its derivation inputs: `loom.repo`, `loom.issue` and
  `loom.story_id` in a story, or `loom.repo` and `loom.sweep_id` outside one.

**Enforcement:** the random constructors `TraceContext::root()` and
`TraceContext::child()` only compile under `cfg(test)`, so production code that
tries to mint a random ID fails to build. A new root trace must choose a
natural key and call `TraceContext::derived`. If a derivation ever has to
change, give it a new tag rather than silently changing its inputs, so old and
new IDs cannot collide.

## 2. Provenance

| Attribute | On | Value |
|-----------|----|-------|
| `loom.daemon.version` | every span | `CARGO_PKG_VERSION` of the binary that created the span |
| `loom.daemon.revision` | every span | full git SHA that binary was built from (`build.rs`), `unknown` for a tarball build |
| `loom.install.version` | sweep span | `loom_version` from the workspace's `.loom/install-metadata.json` |
| `loom.install.revision` | sweep span | `loom_commit` from the same file |
| `loom.prompts.digest` | sweep span | `sha256:` over every file under `.claude/commands/loom/` and `.loom/roles/` (sorted path, length, bytes), taken at dispatch |

Why all three: the daemon SHA identifies the code that dispatched and recorded
the work, but a sweep child runs the prompts **installed in the workspace**,
which can lag behind or differ from the daemon. `loom.install.revision` says
which Loom commit the prompts were installed from. `loom.prompts.digest`
detects local edits and partial resyncs that the install metadata cannot see.

Stamping happens once, when the span is created (`provenance::stamp`), and never
overwrites an existing value. A span restored from the export queue after an
upgrade therefore keeps the build that created it. New `SpanRecord`
construction sites must call `provenance::stamp`. `Journal::start` does it for
every lifecycle span.
