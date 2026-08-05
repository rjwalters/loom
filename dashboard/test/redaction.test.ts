import { createExecutionContext, env, waitOnExecutionContext } from "cloudflare:test";
import { beforeAll, beforeEach, describe, expect, it } from "vitest";
import worker from "../src/index";
import {
  deriveTokenPoolAggregate,
  redactActiveSweep,
  redactFleetSnapshot,
  redactHistoryQueryResult,
  redactHistoryRecord,
  redactManagedRepos,
  redactPayload,
  redactSseFrame,
} from "../src/redaction";
import type { HistoryRecord } from "../src/query";
import type { ActiveSweepState, FleetSnapshot } from "../src/fleetState";
import {
  authedRequest,
  hostHealthEnvelope,
  initAccessTestKeys,
  mockJwksFetch,
  seedHost,
  sweepCompletedEnvelope,
  sweepOutcomeEnvelope,
  sweepPhaseEnvelope,
  sweepStartedEnvelope,
  tokensSnapshotEnvelope,
} from "./testHelpers";

// ---------------------------------------------------------------------------
// Adversarial redaction test suite (issue #4727).
//
// Covers every record kind from the Phase-1 schema
// (`.loom/docs/telemetry-schema.md`) across visibility × auth, asserting
// field-level absence (not just "doesn't crash") of the four leak vectors
// the acceptance criteria name: repo names, issue numbers, sweep/branch
// identifiers, and PR links (`pr_number`).
// ---------------------------------------------------------------------------

async function callWorker(request: Request): Promise<Response> {
  const ctx = createExecutionContext();
  const response = await worker.fetch(request as Request<unknown, IncomingRequestCfProperties>, env, ctx);
  await waitOnExecutionContext(ctx);
  return response;
}

function ingestRequest(body: unknown, authHeader = "Bearer abc-ingest-key"): Request {
  return new Request("https://ingest.example/ingest", {
    method: "POST",
    headers: { "content-type": "application/json", authorization: authHeader },
    body: JSON.stringify(body),
  });
}

async function ingest(envelopes: unknown[]): Promise<Response> {
  return callWorker(ingestRequest(envelopes));
}

beforeAll(async () => {
  // `/api/*` verifies the Access JWT in-Worker (src/index.ts), so this suite
  // needs a real signed cookie and a stubbed JWKS endpoint. Installed once —
  // nothing here tests the failure path, which is index.test.ts's job.
  await initAccessTestKeys();
  mockJwksFetch();
});

beforeEach(async () => {
  await seedHost(env.DB, "host-abc", "abc-ingest-key");
});

// ---------------------------------------------------------------------------
// Unit tests: `redactPayload` (the per-kind field allowlist), one fixture
// per record kind the schema defines. Each fixture is private-visibility
// (or, for the host-level kinds, has no visibility at all) with every field
// the schema documents, plus adversarial extras (`branch`, `issue_title`)
// that are NOT part of today's schema — the allowlist must drop unknown
// fields by default, exactly the "future field leaks by accident" scenario
// the module doc calls out.
// ---------------------------------------------------------------------------

describe("redactPayload — per-kind field allowlist", () => {
  it("sweep.started: keeps lifecycle/model fields, strips repo/issue/sweep_id and any unknown field", () => {
    const redacted = redactPayload("sweep.started", {
      kind: "sweep.started",
      repo: "rjwalters/loom",
      visibility: "private",
      issue: 4703,
      sweep_id: "sweep-issue-4703-0",
      started_at: "2026-07-30T12:00:00Z",
      model: "opus",
      effort: "high",
      branch: "feature/issue-4703",
      issue_title: "Fix the thing",
    });
    expect(redacted).toEqual({
      kind: "sweep.started",
      started_at: "2026-07-30T12:00:00Z",
      model: "opus",
      effort: "high",
    });
  });

  it("sweep.phase: keeps phase/entered_at, strips repo/issue/sweep_id", () => {
    const redacted = redactPayload("sweep.phase", {
      kind: "sweep.phase",
      repo: "rjwalters/loom",
      visibility: "private",
      issue: 4703,
      sweep_id: "sweep-issue-4703-0",
      phase: "builder",
      entered_at: "2026-07-30T12:03:20Z",
      branch: "feature/issue-4703",
    });
    expect(redacted).toEqual({ kind: "sweep.phase", phase: "builder", entered_at: "2026-07-30T12:03:20Z" });
  });

  it("sweep.completed: keeps completed_at/result, strips repo/issue/sweep_id", () => {
    const redacted = redactPayload("sweep.completed", {
      kind: "sweep.completed",
      repo: "rjwalters/loom",
      visibility: "private",
      issue: 4703,
      sweep_id: "sweep-issue-4703-0",
      completed_at: "2026-07-30T12:08:32Z",
      result: "success",
    });
    expect(redacted).toEqual({ kind: "sweep.completed", completed_at: "2026-07-30T12:08:32Z", result: "success" });
  });

  it("sweep.outcome: keeps model/config/phase_durations/result, strips repo/issue/sweep_id/pr_number (the PR link vector)", () => {
    const redacted = redactPayload("sweep.outcome", {
      kind: "sweep.outcome",
      repo: "rjwalters/loom",
      visibility: "private",
      issue: 4703,
      sweep_id: "sweep-issue-4703-0",
      model: "opus",
      effort: "high",
      config: { runtime: "claude" },
      phase_durations: [{ phase: "builder", duration_sec: 340 }],
      total_duration_sec: 512,
      result: "success",
      pr_number: 4710,
    });
    expect(redacted).toEqual({
      kind: "sweep.outcome",
      model: "opus",
      effort: "high",
      config: { runtime: "claude" },
      phase_durations: [{ phase: "builder", duration_sec: 340 }],
      total_duration_sec: 512,
      result: "success",
    });
    expect(redacted).not.toHaveProperty("pr_number");
  });

  // Issue #5357: the work-output fields (tokens_in/tokens_out,
  // lines_added/lines_deleted) are workload detail for a private repo — the
  // same category `pr_number` is already held back for — so a private,
  // unauthenticated viewer must never see them, regardless of how many of
  // the four are present on a given record (a no-PR sweep has no LOC pair,
  // a pruned-logs sweep has no token pair).
  it("sweep.outcome: strips the #5357 work-output fields (tokens_in/tokens_out/lines_added/lines_deleted) for a private record", () => {
    const redacted = redactPayload("sweep.outcome", {
      kind: "sweep.outcome",
      repo: "rjwalters/loom",
      visibility: "private",
      issue: 4703,
      sweep_id: "sweep-issue-4703-0",
      model: "opus",
      effort: "high",
      config: { runtime: "claude" },
      phase_durations: [{ phase: "builder", duration_sec: 340 }],
      total_duration_sec: 512,
      result: "success",
      pr_number: 4710,
      tokens_in: 48_213,
      tokens_out: 6_120,
      lines_added: 214,
      lines_deleted: 37,
    });
    for (const field of ["tokens_in", "tokens_out", "lines_added", "lines_deleted", "pr_number"]) {
      expect(redacted).not.toHaveProperty(field);
    }
    // The fields the allowlist DOES keep still survive alongside the strip.
    expect(redacted).toMatchObject({ kind: "sweep.outcome", model: "opus", result: "success" });
  });

  // A record carrying only a SUBSET of the four fields (e.g. a no-PR sweep
  // with tokens but no LOC) must still have every present field stripped —
  // the allowlist is field-by-field, not "all four or none".
  it("sweep.outcome: strips a partial work-output field set (tokens present, LOC absent) for a private record", () => {
    const redacted = redactPayload("sweep.outcome", {
      kind: "sweep.outcome",
      repo: "rjwalters/loom",
      visibility: "private",
      issue: 4703,
      total_duration_sec: 90,
      result: "success",
      tokens_in: 1_000,
      tokens_out: 200,
    });
    expect(redacted).not.toHaveProperty("tokens_in");
    expect(redacted).not.toHaveProperty("tokens_out");
    expect(redacted).not.toHaveProperty("lines_added");
    expect(redacted).not.toHaveProperty("lines_deleted");
  });

  it("tokens.snapshot: per-account rows are replaced by a non-identifying aggregate", () => {
    const redacted = redactPayload("tokens.snapshot", {
      kind: "tokens.snapshot",
      captured_at: "2026-07-30T12:00:00Z",
      accounts: [
        { account: "agent-1", rank: 0, usage_fraction: 0.42, exhausted: false },
        { account: "agent-2", rank: 1, usage_fraction: 0.9, exhausted: false },
        // Shaped like a real daemon push post-#4874: an exhausted account
        // carries the instant its 7d window resets, so the public view's
        // fleet-level "capacity returns at" is a real time rather than the
        // permanent `null` it was while the daemon hardcoded the field away.
        {
          account: "agent-3",
          rank: 2,
          usage_fraction: 0,
          limit_window_reset_at: "2026-08-02T03:00:00Z",
          exhausted: true,
        },
      ],
    });

    expect(redacted).toEqual({
      kind: "tokens.snapshot",
      captured_at: "2026-07-30T12:00:00Z",
      account_count: 3,
      exhausted_count: 1,
      mean_usage_fraction: 0.44,
      max_usage_fraction: 0.9,
      next_limit_window_reset_at: "2026-08-02T03:00:00Z",
    });
  });

  it("tokens.snapshot: a reset instant survives redaction without naming the account it came from", () => {
    // Issue #4874's aggregate half: the reset is the one per-account field the
    // public view is allowed to keep, precisely because "capacity returns at
    // 03:00Z" describes the pool, not who is in it. Guard that the row it was
    // lifted from still does not survive alongside it.
    const serialized = JSON.stringify(
      redactPayload("tokens.snapshot", {
        kind: "tokens.snapshot",
        captured_at: "2026-07-30T12:00:00Z",
        accounts: [
          { account: "agent5-2amlogic", rank: 4, limit_window_reset_at: "2026-08-04T11:00:00Z", exhausted: true },
          { account: "robb-2amlogic", rank: 0, limit_window_reset_at: "2026-08-02T03:00:00Z", exhausted: true },
        ],
      }),
    );
    // The *earliest* reset across the pool, not the first row's.
    expect(JSON.parse(serialized).next_limit_window_reset_at).toBe("2026-08-02T03:00:00Z");
    expect(serialized).not.toContain("agent5-2amlogic");
    expect(serialized).not.toContain("robb-2amlogic");
    expect(serialized).not.toContain("accounts");
    // The per-account field itself is gone — only the derived aggregate key
    // (`next_limit_window_reset_at`) remains.
    expect(serialized).not.toContain('"limit_window_reset_at"');
  });

  it("tokens.snapshot: no account identifier survives, at any depth", () => {
    const serialized = JSON.stringify(
      redactPayload("tokens.snapshot", {
        kind: "tokens.snapshot",
        captured_at: "2026-07-30T12:00:00Z",
        accounts: [{ account: "agent5-2amlogic", rank: 4, usage_fraction: 0.91, exhausted: false }],
      }),
    );
    expect(serialized).not.toContain("agent5-2amlogic");
    expect(serialized).not.toContain("accounts");
    expect(serialized).not.toContain("rank");
  });

  it("host.health: host-level, no repo/issue reference — passes through unchanged (documented decision)", () => {
    const payload = {
      kind: "host.health",
      captured_at: "2026-07-30T12:00:00Z",
      daemon_version: "0.16.0",
      uptime_sec: 86400,
      logical_cpus: 28,
      cpu_idle_fraction: 0.83,
    };
    expect(redactPayload("host.health", payload)).toEqual(payload);
  });

  it("host.health: worktree_root_total_gb survives redaction alongside worktree_root_free_gb (#5356)", () => {
    // Deliberate decision (#5356): total disk capacity of a build host
    // describes the machine's size, not any repo/operator/workload — the
    // same reasoning as `worktree_root_free_gb`, which is already public.
    const payload = {
      kind: "host.health",
      captured_at: "2026-08-04T12:00:00Z",
      daemon_version: "0.18.0",
      uptime_sec: 86400,
      logical_cpus: 28,
      worktree_root_free_gb: 200,
      worktree_root_total_gb: 1000,
    };
    expect(redactPayload("host.health", payload)).toEqual(payload);
  });

  it("host.health: a free-but-no-total record redacts with no total key fabricated (#5356)", () => {
    // A daemon that has not measured total capacity (or predates #5356)
    // must not have a denominator invented for it anywhere on the redaction
    // path — the allowlist can only pass through keys that are present.
    const payload = {
      kind: "host.health",
      captured_at: "2026-08-04T12:00:00Z",
      daemon_version: "0.18.0",
      uptime_sec: 86400,
      logical_cpus: 28,
      worktree_root_free_gb: 200,
    };
    const redacted = redactPayload("host.health", payload);
    expect(redacted).toEqual(payload);
    expect(redacted).not.toHaveProperty("worktree_root_total_gb");
  });

  it("host.health: build identity (build_commit/built_at) survives redaction alongside daemon_version", () => {
    // #4956 — the commit is the only field that tells two builds sharing a
    // `daemon_version` apart, so stripping it here would re-blind the
    // dashboard it was added for.
    const payload = {
      kind: "host.health",
      captured_at: "2026-08-02T12:00:00Z",
      daemon_version: "0.17.0",
      build_commit: "8c16fb5b",
      built_at: "2026-08-02T03:09:51Z",
      uptime_sec: 86400,
      logical_cpus: 28,
    };
    expect(redactPayload("host.health", payload)).toEqual(payload);
  });

  it("host.health: dispatch-attention state (dispatch_halted/halt_reason) survives redaction (#4975)", () => {
    // Describes the machine's own admission behavior, not any repo/operator
    // — same reasoning as `cpu_idle_fraction`/`load_per_core` directly above
    // it in the allowlist.
    const payload = {
      kind: "host.health",
      captured_at: "2026-08-02T12:00:00Z",
      daemon_version: "0.17.0",
      uptime_sec: 86400,
      logical_cpus: 28,
      dispatch_halted: true,
      halt_reason: "load-per-core 4.24 >= 2.50 sustained for 3 consecutive tick(s)",
    };
    expect(redactPayload("host.health", payload)).toEqual(payload);
  });

  it("host.health: watchdog/crash-protection state (protection) survives redaction (#5352)", () => {
    // Describes the machine's own crash-protection posture, not any
    // repo/operator — same reasoning as `dispatch_halted`/`halt_reason`
    // directly above it in the allowlist.
    const payload = {
      kind: "host.health",
      captured_at: "2026-08-02T12:00:00Z",
      daemon_version: "0.17.0",
      uptime_sec: 86400,
      protection: { state: "unprotected", watchdog_provisioned: false },
    };
    expect(redactPayload("host.health", payload)).toEqual(payload);
  });

  it("host.health: role-tick health (roles) survives redaction, but each persistent root is basenamed and detail is dropped for the public view (#5022, #5065)", () => {
    // The counts and each failure's role/failures/last_at survive (they
    // describe the machine, not the work). The workspace `root` is a full
    // absolute filesystem path whose home-directory segment names the
    // operator on the common macOS/Linux layout — so the public,
    // unauthenticated surface only ever gets its basename, mirroring the
    // daemon's `RoleFailure::label()` and the frontend's `pathBasename`.
    // `detail` is dropped entirely (#5065): it is a free-form failure string
    // the daemon builds by interpolating another absolute path plus a log
    // tail, so — unlike `root` — there is no basename-style truncation that
    // makes it safe for the public surface.
    const payload = {
      kind: "host.health",
      captured_at: "2026-08-02T12:00:00Z",
      daemon_version: "0.17.0",
      uptime_sec: 86400,
      logical_cpus: 28,
      roles: {
        total: 3,
        ok: 1,
        persistent: [
          {
            root: "/Users/alice/GitHub/loom",
            role: "judge",
            failures: 2,
            last_at: "2026-08-02T11:59:00Z",
            detail: "no-token-pool",
          },
        ],
      },
    };
    const redacted = redactPayload("host.health", payload);
    // Counts and non-path detail survive unchanged.
    expect(redacted).toMatchObject({
      kind: "host.health",
      captured_at: "2026-08-02T12:00:00Z",
      daemon_version: "0.17.0",
      uptime_sec: 86400,
      logical_cpus: 28,
    });
    expect(redacted.roles).toEqual({
      total: 3,
      ok: 1,
      persistent: [
        {
          // Basenamed — the operator-identifying home-directory prefix is gone.
          root: "loom",
          role: "judge",
          failures: 2,
          last_at: "2026-08-02T11:59:00Z",
          // NOTE: no `detail` key at all — dropped, not truncated.
        },
      ],
    });
    // The raw absolute path (and its operator-identifying username segment)
    // never reaches the public surface, in any field.
    expect(JSON.stringify(redacted)).not.toContain("/Users/alice");
    expect(JSON.stringify(redacted)).not.toContain("alice");
  });

  it("host.health: a realistic role-tick `detail` (absolute path + log tail) never reaches the public surface (#5065)", () => {
    // The most common role-tick failure path (`RoleTickOutcome::Failure` in
    // `loom-daemon/src/role_runner.rs` for a non-zero exit) builds `detail`
    // by interpolating the absolute path to `spawn-worker.sh` plus a tail of
    // the failing role child's own log output.
    const payload = {
      kind: "host.health",
      captured_at: "2026-08-02T12:00:00Z",
      daemon_version: "0.17.0",
      roles: {
        total: 1,
        ok: 0,
        persistent: [
          {
            root: "/Users/alice/GitHub/loom",
            role: "judge",
            failures: 1,
            last_at: "2026-08-02T11:59:00Z",
            detail:
              "`/Users/alice/GitHub/loom/.loom/scripts/spawn-worker.sh` exited with exit status: 1: some log tail",
          },
        ],
      },
    };
    const redacted = redactPayload("host.health", payload);
    const serialized = JSON.stringify(redacted);
    expect(serialized).not.toContain("alice");
    expect(serialized).not.toContain("/Users/alice/GitHub/loom/.loom/scripts/spawn-worker.sh");
    expect((redacted.roles as { persistent: Record<string, unknown>[] }).persistent[0]).not.toHaveProperty("detail");
  });

  it("host.health: no roles key at all when the payload carries no role-tick summary (#5022)", () => {
    const payload = { kind: "host.health", daemon_version: "0.17.0", uptime_sec: 100 };
    expect(redactPayload("host.health", payload)).not.toHaveProperty("roles");
  });

  it("host.health: a total: 0 role-tick summary round-trips distinctly (no persistent list) (#5022)", () => {
    const payload = {
      kind: "host.health",
      daemon_version: "0.17.0",
      uptime_sec: 100,
      roles: { total: 0, ok: 0 },
    };
    const redacted = redactPayload("host.health", payload);
    expect(redacted.roles).toEqual({ total: 0, ok: 0 });
  });

  it("host.health: a private repo's slug is redacted, but every other field survives unchanged (#4976)", () => {
    const payload = {
      kind: "host.health",
      captured_at: "2026-08-02T12:00:00Z",
      daemon_version: "0.17.0",
      uptime_sec: 86400,
      logical_cpus: 28,
      managed_repos: [
        { slug: "rjwalters/loom", visibility: "public" },
        { slug: "2AMLogic/gf180-pll", visibility: "private" },
      ],
    };
    const redacted = redactPayload("host.health", payload);
    expect(redacted).toMatchObject({
      daemon_version: "0.17.0",
      uptime_sec: 86400,
      logical_cpus: 28,
    });
    expect(redacted.managed_repos).toEqual([
      { slug: "rjwalters/loom", visibility: "public" },
      { visibility: "private" },
    ]);
    expect(JSON.stringify(redacted)).not.toContain("gf180-pll");
  });

  it("host.health: no managed_repos key at all when the payload carries no roster", () => {
    const payload = { kind: "host.health", daemon_version: "0.17.0", uptime_sec: 100 };
    expect(redactPayload("host.health", payload)).not.toHaveProperty("managed_repos");
  });

  it("an unrecognized (forward-compatible) kind reveals only `kind`", () => {
    const redacted = redactPayload("future.kind", {
      kind: "future.kind",
      repo: "rjwalters/loom",
      some_new_field: "unexpected",
    });
    expect(redacted).toEqual({ kind: "future.kind" });
  });
});

// ---------------------------------------------------------------------------
// Unit tests: `redactHistoryRecord` / `redactHistoryQueryResult`
// ---------------------------------------------------------------------------

function historyRecordFixture(overrides: Partial<HistoryRecord> = {}): HistoryRecord {
  return {
    id: 1,
    schemaVersion: 1,
    emittedAt: "2026-07-30T12:00:00Z",
    hostId: "host-abc",
    kind: "sweep.started",
    repo: "rjwalters/loom",
    visibility: "private",
    issue: 4703,
    sweepId: "sweep-issue-4703-0",
    ingestedAt: "2026-07-30T12:00:01Z",
    record: {
      kind: "sweep.started",
      repo: "rjwalters/loom",
      visibility: "private",
      issue: 4703,
      sweep_id: "sweep-issue-4703-0",
      started_at: "2026-07-30T12:00:00Z",
      model: "opus",
    },
    ...overrides,
  };
}

describe("redactHistoryRecord", () => {
  it("authenticated viewer sees full detail regardless of visibility", () => {
    const record = historyRecordFixture();
    expect(redactHistoryRecord(record, true)).toBe(record);
  });

  it("public-visibility record is returned unchanged even without authentication", () => {
    const record = historyRecordFixture({ visibility: "public" });
    expect(redactHistoryRecord(record, false)).toBe(record);
  });

  it("private + unauthenticated: repo/issue/sweepId nulled at the top level, payload allowlisted", () => {
    const record = historyRecordFixture();
    const redacted = redactHistoryRecord(record, false);
    expect(redacted.repo).toBeNull();
    expect(redacted.issue).toBeNull();
    expect(redacted.sweepId).toBeNull();
    expect(redacted.hostId).toBe("host-abc"); // host id is not repo-identifying
    expect(redacted.record).toEqual({ kind: "sweep.started", started_at: "2026-07-30T12:00:00Z", model: "opus" });
    // Adversarial: the serialized JSON must never contain the repo name,
    // issue number, or sweep id anywhere, not just at the fields we checked.
    const serialized = JSON.stringify(redacted);
    expect(serialized).not.toContain("rjwalters/loom");
    expect(serialized).not.toContain("4703");
    expect(serialized).not.toContain("sweep-issue-4703-0");
  });

  it("a missing/invalid visibility value fails safe to private (never accidentally decodes to public)", () => {
    const record = historyRecordFixture({ visibility: "not-a-real-value" });
    const redacted = redactHistoryRecord(record, false);
    expect(redacted.repo).toBeNull();
  });
});

describe("redactHistoryQueryResult", () => {
  it("redacts every record and preserves the pagination cursor unchanged", () => {
    const result = redactHistoryQueryResult(
      { records: [historyRecordFixture({ id: 1 }), historyRecordFixture({ id: 2, visibility: "public" })], nextCursor: 1 },
      false,
    );
    expect(result.nextCursor).toBe(1);
    expect(result.records[0]?.repo).toBeNull();
    expect(result.records[1]?.repo).toBe("rjwalters/loom"); // public record untouched
  });
});

// ---------------------------------------------------------------------------
// Unit tests: `redactActiveSweep` / `redactFleetSnapshot`
// ---------------------------------------------------------------------------

function activeSweepFixture(overrides: Partial<ActiveSweepState> = {}): ActiveSweepState {
  return {
    hostId: "host-abc",
    sweepId: "sweep-issue-4703-0",
    repo: "rjwalters/loom",
    visibility: "private",
    issue: 4703,
    phase: "builder",
    startedAt: "2026-07-30T12:00:00Z",
    enteredPhaseAt: "2026-07-30T12:03:20Z",
    model: "opus",
    effort: "high",
    updatedAt: "2026-07-30T12:03:20Z",
    ...overrides,
  };
}

describe("redactActiveSweep", () => {
  it("private + unauthenticated: sweepId/repo/issue are entirely absent (not just null)", () => {
    const redacted = redactActiveSweep(activeSweepFixture(), false);
    expect(redacted).not.toHaveProperty("sweepId");
    expect(redacted).not.toHaveProperty("repo");
    expect(redacted).not.toHaveProperty("issue");
    expect(redacted.phase).toBe("builder");
    expect(redacted.model).toBe("opus");
    const serialized = JSON.stringify(redacted);
    expect(serialized).not.toContain("rjwalters/loom");
    expect(serialized).not.toContain("4703");
    expect(serialized).not.toContain("sweep-issue-4703-0");
  });

  it("public visibility or authenticated: unchanged", () => {
    const sweep = activeSweepFixture({ visibility: "public" });
    expect(redactActiveSweep(sweep, false)).toBe(sweep);
    const privateSweep = activeSweepFixture();
    expect(redactActiveSweep(privateSweep, true)).toBe(privateSweep);
  });
});

describe("redactFleetSnapshot", () => {
  it("redacts private activeSweeps entries and leaves host health/tokens intact", () => {
    const snapshot: FleetSnapshot = {
      hosts: {
        "host-abc": {
          health: { record: { kind: "host.health", uptime_sec: 100 }, updatedAt: "2026-07-30T12:00:00Z" },
          tokens: { record: { kind: "tokens.snapshot", accounts: [] }, updatedAt: "2026-07-30T12:00:00Z" },
        },
      },
      activeSweeps: [activeSweepFixture()],
    };
    const redacted = redactFleetSnapshot(snapshot, false);
    expect(redacted.hosts["host-abc"]?.health?.record).toEqual({ kind: "host.health", uptime_sec: 100 });
    expect(redacted.activeSweeps[0]).not.toHaveProperty("sweepId");
  });

  it("collapses a private repo's slug for a public viewer, and shows every slug to an authenticated one (#4976)", () => {
    const snapshot: FleetSnapshot = {
      hosts: {
        "host-abc": {
          health: {
            record: {
              kind: "host.health",
              uptime_sec: 100,
              managed_repos: [
                { slug: "rjwalters/loom", visibility: "public" },
                { slug: "2AMLogic/gf180-pll", visibility: "private" },
              ],
            },
            updatedAt: "2026-07-30T12:00:00Z",
          },
        },
      },
      activeSweeps: [],
    };

    const publicRecord = redactFleetSnapshot(snapshot, false).hosts["host-abc"]?.health?.record;
    expect(publicRecord?.managed_repos).toEqual([
      { slug: "rjwalters/loom", visibility: "public" },
      { visibility: "private" },
    ]);
    expect(JSON.stringify(publicRecord)).not.toContain("gf180-pll");

    const authedRecord = redactFleetSnapshot(snapshot, true).hosts["host-abc"]?.health?.record;
    expect(authedRecord?.managed_repos).toEqual([
      { slug: "rjwalters/loom", visibility: "public" },
      { slug: "2AMLogic/gf180-pll", visibility: "private" },
    ]);
  });

  it("summarizes the token pool for a public viewer", () => {
    const snapshot: FleetSnapshot = {
      hosts: {
        "host-abc": {
          tokens: {
            record: {
              kind: "tokens.snapshot",
              accounts: [
                { account: "agent-1", rank: 0, usage_fraction: 0.5, exhausted: false },
                { account: "agent-2", rank: 1, usage_fraction: 0, exhausted: true },
              ],
            },
            updatedAt: "2026-07-30T12:00:00Z",
          },
        },
      },
      activeSweeps: [],
    };

    const record = redactFleetSnapshot(snapshot, false).hosts["host-abc"]?.tokens?.record;
    expect(record).toMatchObject({ account_count: 2, exhausted_count: 1, max_usage_fraction: 0.5 });
    expect(record).not.toHaveProperty("accounts");
  });

  // Regression: this function used to project host entries through
  // `redactPayload` unconditionally, ignoring `isAuthenticated`. That was
  // invisible while every allowlist named every schema field; once
  // `tokens.snapshot` started summarizing `accounts`, it would have stripped
  // per-account detail from the signed-in dashboard as well.
  it("leaves an authenticated viewer's host entries untouched", () => {
    const accounts = [{ account: "agent-1", rank: 0, usage_fraction: 0.5, exhausted: false }];
    const snapshot: FleetSnapshot = {
      hosts: {
        "host-abc": {
          tokens: { record: { kind: "tokens.snapshot", accounts }, updatedAt: "2026-07-30T12:00:00Z" },
        },
      },
      activeSweeps: [],
    };

    const record = redactFleetSnapshot(snapshot, true).hosts["host-abc"]?.tokens?.record;
    expect(record).toEqual({ kind: "tokens.snapshot", accounts });
  });

  it("keeps the full absolute root and detail in a role-tick failure for an authenticated viewer (#5065)", () => {
    const roles = {
      total: 1,
      ok: 0,
      persistent: [
        {
          root: "/Users/alice/GitHub/loom",
          role: "judge",
          failures: 1,
          last_at: "2026-08-02T11:59:00Z",
          detail: "`/Users/alice/GitHub/loom/.loom/scripts/spawn-worker.sh` exited with exit status: 1: log tail",
        },
      ],
    };
    const snapshot: FleetSnapshot = {
      hosts: {
        "host-abc": {
          health: {
            record: { kind: "host.health", uptime_sec: 100, roles },
            updatedAt: "2026-07-30T12:00:00Z",
          },
        },
      },
      activeSweeps: [],
    };

    const authedRecord = redactFleetSnapshot(snapshot, true).hosts["host-abc"]?.health?.record;
    expect(authedRecord).toEqual({ kind: "host.health", uptime_sec: 100, roles });

    const publicRecord = redactFleetSnapshot(snapshot, false).hosts["host-abc"]?.health?.record;
    const publicPersistent = (publicRecord?.roles as { persistent: Record<string, unknown>[] } | undefined)
      ?.persistent;
    expect(publicPersistent?.[0]).not.toHaveProperty("detail");
    expect(JSON.stringify(publicRecord)).not.toContain("alice");
  });
});

describe("redactManagedRepos", () => {
  it("keeps a public entry's slug, strips a private entry's slug but keeps its place", () => {
    expect(
      redactManagedRepos([
        { slug: "rjwalters/loom", visibility: "public" },
        { slug: "2AMLogic/gf180-pll", visibility: "private" },
        { slug: "2AMLogic/gf180-trng", visibility: "private" },
      ]),
    ).toEqual([
      { slug: "rjwalters/loom", visibility: "public" },
      { visibility: "private" },
      { visibility: "private" },
    ]);
  });

  it("treats a malformed row (missing/wrong-typed slug, any non-\"public\" visibility) as private", () => {
    expect(
      redactManagedRepos([
        { visibility: "public" }, // no slug at all
        { slug: 42, visibility: "public" }, // wrong-typed slug
        { slug: "owner/repo", visibility: "internal" }, // unrecognized visibility label
        { slug: "owner/repo" }, // missing visibility
      ]),
    ).toEqual([{ visibility: "private" }, { visibility: "private" }, { visibility: "private" }, { visibility: "private" }]);
  });

  it("an empty roster redacts to an empty roster", () => {
    expect(redactManagedRepos([])).toEqual([]);
  });
});

describe("deriveTokenPoolAggregate", () => {
  it("reports null rather than a misleading zero when no account measured usage", () => {
    expect(deriveTokenPoolAggregate({ accounts: [{ account: "a", exhausted: false }] })).toEqual({
      account_count: 1,
      exhausted_count: 0,
      mean_usage_fraction: null,
      max_usage_fraction: null,
      next_limit_window_reset_at: null,
    });
  });

  it("averages only over accounts that reported a usage_fraction", () => {
    const aggregate = deriveTokenPoolAggregate({
      accounts: [{ usage_fraction: 0.2 }, { usage_fraction: 0.8 }, { exhausted: true }],
    });
    expect(aggregate.mean_usage_fraction).toBe(0.5);
    expect(aggregate.max_usage_fraction).toBe(0.8);
    expect(aggregate.account_count).toBe(3);
  });

  it("takes the earliest limit-window reset across the pool", () => {
    const aggregate = deriveTokenPoolAggregate({
      accounts: [
        { limit_window_reset_at: "2026-07-30T18:00:00Z" },
        { limit_window_reset_at: "2026-07-30T14:00:00Z" },
      ],
    });
    expect(aggregate.next_limit_window_reset_at).toBe("2026-07-30T14:00:00Z");
  });

  // This runs on a live SSE response path, so a malformed payload must
  // degrade rather than throw and kill the stream.
  it.each([
    ["accounts absent", {}],
    ["accounts not an array", { accounts: "nope" }],
    ["accounts null", { accounts: null }],
  ])("degrades to a zero-count aggregate when %s", (_label, payload) => {
    expect(deriveTokenPoolAggregate(payload as Record<string, unknown>)).toEqual({
      account_count: 0,
      exhausted_count: 0,
      mean_usage_fraction: null,
      max_usage_fraction: null,
      next_limit_window_reset_at: null,
    });
  });

  it("ignores non-finite usage values rather than propagating NaN", () => {
    const aggregate = deriveTokenPoolAggregate({
      accounts: [{ usage_fraction: Number.NaN }, { usage_fraction: 0.4 }],
    });
    expect(aggregate.mean_usage_fraction).toBe(0.4);
    expect(aggregate.max_usage_fraction).toBe(0.4);
  });
});

// ---------------------------------------------------------------------------
// Unit tests: `redactSseFrame` (live tail)
// ---------------------------------------------------------------------------

describe("redactSseFrame", () => {
  it("passes non-data frames through unchanged (retry/comment preamble, keepalive)", () => {
    const preamble = "retry: 3000\n: connected to loom fleet telemetry live tail\n\n";
    expect(redactSseFrame(preamble, false)).toBe(preamble);
    const keepalive = ": keepalive\n\n";
    expect(redactSseFrame(keepalive, false)).toBe(keepalive);
  });

  it("passes an authenticated frame through unchanged regardless of visibility", () => {
    const frame = `data: ${JSON.stringify({
      topic: "sweep.started",
      event: {
        hostId: "host-abc",
        emittedAt: "2026-07-30T12:00:00Z",
        schemaVersion: 1,
        record: { kind: "sweep.started", repo: "rjwalters/loom", visibility: "private", sweep_id: "sweep-issue-4703-0" },
      },
    })}\n\n`;
    expect(redactSseFrame(frame, true)).toBe(frame);
  });

  it("passes a public-visibility frame through unchanged", () => {
    const frame = `data: ${JSON.stringify({
      topic: "sweep.started",
      event: {
        hostId: "host-abc",
        emittedAt: "2026-07-30T12:00:00Z",
        schemaVersion: 1,
        record: { kind: "sweep.started", repo: "rjwalters/loom", visibility: "public", sweep_id: "sweep-issue-4703-0" },
      },
    })}\n\n`;
    expect(redactSseFrame(frame, false)).toBe(frame);
  });

  it("redacts a private-visibility frame's record payload, unauthenticated", () => {
    const frame = `data: ${JSON.stringify({
      topic: "sweep.started",
      event: {
        hostId: "host-abc",
        emittedAt: "2026-07-30T12:00:00Z",
        schemaVersion: 1,
        record: {
          kind: "sweep.started",
          repo: "rjwalters/loom",
          visibility: "private",
          issue: 4703,
          sweep_id: "sweep-issue-4703-0",
          started_at: "2026-07-30T12:00:00Z",
        },
      },
    })}\n\n`;
    const redacted = redactSseFrame(frame, false);
    expect(redacted).not.toContain("rjwalters/loom");
    expect(redacted).not.toContain("4703");
    expect(redacted).not.toContain("sweep-issue-4703-0");
    expect(redacted).toContain("sweep.started"); // topic + kind survive
    const parsed = JSON.parse(redacted.slice("data: ".length, -2)) as { event: { record: Record<string, unknown> } };
    expect(parsed.event.record).toEqual({ kind: "sweep.started", started_at: "2026-07-30T12:00:00Z" });
  });

  it("never throws on a malformed data frame — passes it through unchanged", () => {
    const malformed = "data: {not valid json\n\n";
    expect(redactSseFrame(malformed, false)).toBe(malformed);
  });
});

// ---------------------------------------------------------------------------
// Integration tests: the actual `/api/*` vs `/public/*` routes end to end,
// covering every record kind × private visibility, plus a spot-check that
// public-visibility data is NOT over-redacted on the public route.
// ---------------------------------------------------------------------------

const PRIVATE_ENVELOPE_BUILDERS: [string, () => Record<string, unknown>][] = [
  ["sweep.started", () => sweepStartedEnvelope({ visibility: "private" })],
  ["sweep.phase", () => sweepPhaseEnvelope({ visibility: "private" })],
  ["sweep.completed", () => sweepCompletedEnvelope({ visibility: "private" })],
  ["sweep.outcome", () => sweepOutcomeEnvelope({ visibility: "private" })],
];

describe("GET /public/history vs GET /api/history — end-to-end redaction", () => {
  for (const [kind, buildEnvelope] of PRIVATE_ENVELOPE_BUILDERS) {
    it(`${kind}: the public route never leaks repo/issue/sweep_id/pr_number; the authenticated route does`, async () => {
      await ingest([buildEnvelope()]);

      const publicResponse = await callWorker(new Request("https://ingest.example/public/history"));
      const publicText = await publicResponse.text();
      expect(publicText).not.toContain("rjwalters/loom");
      expect(publicText).not.toContain("4703");
      expect(publicText).not.toContain("sweep-issue-4703-0");
      if (kind === "sweep.outcome") {
        expect(publicText).not.toContain("4710"); // pr_number
        // Issue #5357 work-output fields: same private-only treatment.
        expect(publicText).not.toContain("48213"); // tokens_in
        expect(publicText).not.toContain("6120"); // tokens_out
        expect(publicText).not.toContain("214"); // lines_added
        // lines_deleted (37) is too short/common a substring to assert
        // absence of textually — covered precisely by the unit tests above.
      }

      const authResponse = await callWorker(await authedRequest("https://ingest.example/api/history"));
      const authText = await authResponse.text();
      expect(authText).toContain("rjwalters/loom");
      expect(authText).toContain("sweep-issue-4703-0");
    });
  }

  it("host.health passes through unredacted on the public route; tokens.snapshot is aggregated", async () => {
    await ingest([tokensSnapshotEnvelope(), hostHealthEnvelope()]);

    const publicResponse = await callWorker(new Request("https://ingest.example/public/history"));
    const publicText = await publicResponse.text();
    const body = JSON.parse(publicText) as {
      records: { kind: string; record: Record<string, unknown> }[];
    };
    const tokensRecord = body.records.find((r) => r.kind === "tokens.snapshot");
    const healthRecord = body.records.find((r) => r.kind === "host.health");

    // Capacity telemetry: fully public.
    expect(healthRecord?.record).toMatchObject({ daemon_version: "0.16.0", uptime_sec: 100 });

    // Token pool: aggregate only — no account names, no per-account rows.
    expect(tokensRecord?.record).toMatchObject({ account_count: 1, exhausted_count: 0 });
    expect(tokensRecord?.record).not.toHaveProperty("accounts");
    expect(publicText).not.toContain("agent-1");
  });

  it("a role-tick failure's `detail` never reaches GET /public/history, but GET /api/history keeps it in full (#5065)", async () => {
    // Mirrors the `root` basenaming case #5042 landed, for the sibling
    // `detail` field: the common role-tick failure path builds `detail` by
    // interpolating an absolute `spawn-worker.sh` path plus a log tail.
    const roleTickDetail =
      "`/Users/alice/GitHub/loom/.loom/scripts/spawn-worker.sh` exited with exit status: 1: some log tail";
    await ingest([
      hostHealthEnvelope({
        roles: {
          total: 1,
          ok: 0,
          persistent: [
            {
              root: "/Users/alice/GitHub/loom",
              role: "judge",
              failures: 1,
              last_at: "2026-08-02T11:59:00Z",
              detail: roleTickDetail,
            },
          ],
        },
      }),
    ]);

    const publicResponse = await callWorker(new Request("https://ingest.example/public/history"));
    const publicText = await publicResponse.text();
    expect(publicText).not.toContain("alice");
    expect(publicText).not.toContain(roleTickDetail);
    const publicBody = JSON.parse(publicText) as {
      records: { kind: string; record: { roles?: { persistent?: Record<string, unknown>[] } } }[];
    };
    const publicHealth = publicBody.records.find((r) => r.kind === "host.health");
    expect(publicHealth?.record.roles?.persistent?.[0]).not.toHaveProperty("detail");
    expect(publicHealth?.record.roles?.persistent?.[0]).toMatchObject({ root: "loom", role: "judge" });

    const authResponse = await callWorker(await authedRequest("https://ingest.example/api/history"));
    const authText = await authResponse.text();
    expect(authText).toContain(roleTickDetail);
    expect(authText).toContain("/Users/alice/GitHub/loom");
  });

  it("the authenticated route still serves the full per-account token rows", async () => {
    await ingest([tokensSnapshotEnvelope()]);

    const response = await callWorker(await authedRequest("https://ingest.example/api/history"));
    const body = (await response.json()) as {
      records: { kind: string; record: Record<string, unknown> }[];
    };
    const tokensRecord = body.records.find((r) => r.kind === "tokens.snapshot");
    expect(tokensRecord?.record).toMatchObject({ accounts: [{ account: "agent-1" }] });
    expect(tokensRecord?.record).not.toHaveProperty("account_count");
  });

  it("a public-visibility record is returned in full on the public route (no over-redaction)", async () => {
    await ingest([sweepStartedEnvelope({ visibility: "public" })]);

    const publicResponse = await callWorker(new Request("https://ingest.example/public/history"));
    const publicText = await publicResponse.text();
    expect(publicText).toContain("rjwalters/loom");
    expect(publicText).toContain("sweep-issue-4703-0");
  });

  it("an adversarial future field on a private record never survives to the public route", async () => {
    await ingest([
      sweepStartedEnvelope({
        visibility: "private",
        branch: "feature/issue-4703",
        issue_title: "Fix the private thing",
      }),
    ]);

    const publicResponse = await callWorker(new Request("https://ingest.example/public/history"));
    const publicText = await publicResponse.text();
    expect(publicText).not.toContain("feature/issue-4703");
    expect(publicText).not.toContain("Fix the private thing");
  });
});

describe("GET /public/fleet-state vs GET /api/fleet-state — end-to-end redaction", () => {
  it("a private in-flight sweep's repo/issue/sweepId never appear on the public route", async () => {
    await ingest([sweepStartedEnvelope({ visibility: "private" })]);

    const publicResponse = await callWorker(new Request("https://ingest.example/public/fleet-state"));
    const publicBody = (await publicResponse.json()) as { activeSweeps: { hostId: string; phase?: string }[] };
    const publicText = JSON.stringify(publicBody);
    expect(publicText).not.toContain("rjwalters/loom");
    expect(publicText).not.toContain("sweep-issue-4703-0");
    expect(publicBody.activeSweeps).toHaveLength(1);
    expect(publicBody.activeSweeps[0]?.hostId).toBe("host-abc");

    const authResponse = await callWorker(await authedRequest("https://ingest.example/api/fleet-state"));
    const authText = await authResponse.text();
    expect(authText).toContain("rjwalters/loom");
    expect(authText).toContain("sweep-issue-4703-0");
  });
});

describe("GET /public/events vs GET /api/events — live tail redaction", () => {
  it("a private record's live-tail frame is redacted on the public route", async () => {
    const response = await callWorker(new Request("https://ingest.example/public/events"));
    const reader = response.body?.getReader();
    if (!reader) throw new Error("expected a readable stream body");
    const decoder = new TextDecoder();
    let buffer = "";
    buffer += decoder.decode((await reader.read()).value);

    await ingest([sweepStartedEnvelope({ visibility: "private", sweep_id: "sweep-live-private" })]);

    const deadline = Date.now() + 8_000;
    while (!buffer.includes("sweep.started") && Date.now() < deadline) {
      const chunk = await reader.read();
      if (chunk.done) break;
      if (chunk.value) buffer += decoder.decode(chunk.value);
    }

    expect(buffer).toContain("sweep.started");
    expect(buffer).not.toContain("rjwalters/loom");
    expect(buffer).not.toContain("sweep-live-private");
    await reader.cancel();
  }, 15_000);

  it("a public record's live-tail frame is delivered unredacted on the public route", async () => {
    const response = await callWorker(new Request("https://ingest.example/public/events"));
    const reader = response.body?.getReader();
    if (!reader) throw new Error("expected a readable stream body");
    const decoder = new TextDecoder();
    let buffer = "";
    buffer += decoder.decode((await reader.read()).value);

    await ingest([sweepStartedEnvelope({ visibility: "public", sweep_id: "sweep-live-public" })]);

    const deadline = Date.now() + 8_000;
    while (!buffer.includes("sweep-live-public") && Date.now() < deadline) {
      const chunk = await reader.read();
      if (chunk.done) break;
      if (chunk.value) buffer += decoder.decode(chunk.value);
    }

    expect(buffer).toContain("rjwalters/loom");
    expect(buffer).toContain("sweep-live-public");
    await reader.cancel();
  }, 15_000);
});
