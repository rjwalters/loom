import { createExecutionContext, env, waitOnExecutionContext } from "cloudflare:test";
import { beforeEach, describe, expect, it } from "vitest";
import worker from "../src/index";
import { isPublicPageDisplayKind, renderFleetOverview, renderHistoryTable, renderPublicPage } from "../src/publicPage";
import type { HistoryQueryResult, HistoryRecord } from "../src/query";
import type { PublicActiveSweep, RedactedFleetSnapshot } from "../src/redaction";
import { seedHost, sweepOutcomeEnvelope, sweepStartedEnvelope, tokensSnapshotEnvelope } from "./testHelpers";

// ---------------------------------------------------------------------------
// Redaction test suite for the `/public` page itself (issue #4753's AC:
// "Automated redaction test/snapshot asserting the rendered public page
// contains zero occurrences of repo names, issue titles, branch names, or PR
// links for a private-repo fixture record").
//
// The fixtures below feed data through this page exactly the way the real
// `/public` route does: pretend-redacted objects shaped the way
// `redactFleetSnapshot`/`redactHistoryQueryResult` actually produce them
// (private records have repo/issue/sweepId already nulled/absent, per-kind
// payload already allowlisted) — `publicPage.ts`'s job is to format that
// faithfully, not to redact, so these tests exercise the formatting layer
// with adversarial leftover fields to catch a rendering-side leak.
// ---------------------------------------------------------------------------

const PRIVATE_REPO_NAME = "rjwalters/super-secret-repo";
const PRIVATE_ISSUE_TITLE = "Fix the embarrassing private bug";
const PRIVATE_BRANCH = "feature/issue-9999";
const PRIVATE_SWEEP_ID = "sweep-issue-9999-0";
const PRIVATE_PR_LINK = "https://github.com/rjwalters/super-secret-repo/pull/9999";

const PUBLIC_REPO_NAME = "rjwalters/loom";

function privateHistoryFixture(): HistoryRecord {
  // Shaped exactly like `redactHistoryRecord`'s output for a private,
  // unauthenticated record: repo/issue/sweepId nulled, payload reduced to
  // the sweep.outcome allowlist — plus adversarial fields
  // (`branch`/`issue_title`/`pr_link`) that must never survive even if a
  // future bug reintroduces them into the payload upstream of this module.
  return {
    id: 1,
    schemaVersion: 1,
    emittedAt: "2026-07-31T12:00:00Z",
    hostId: "host-abc",
    kind: "sweep.outcome",
    repo: null,
    visibility: "private",
    issue: null,
    sweepId: null,
    ingestedAt: "2026-07-31T12:00:01Z",
    record: {
      kind: "sweep.outcome",
      model: "opus",
      effort: "high",
      result: "success",
      total_duration_sec: 512,
      // Adversarial: these fields should never appear on a real redacted
      // payload, but if a bug ever put them there, this page must still not
      // render them (formatDetails only reads its own display allowlist).
      branch: PRIVATE_BRANCH,
      issue_title: PRIVATE_ISSUE_TITLE,
      pr_link: PRIVATE_PR_LINK,
      repo: PRIVATE_REPO_NAME,
      sweep_id: PRIVATE_SWEEP_ID,
    },
  };
}

function publicHistoryFixture(): HistoryRecord {
  return {
    id: 2,
    schemaVersion: 1,
    emittedAt: "2026-07-31T12:05:00Z",
    hostId: "host-abc",
    kind: "sweep.outcome",
    repo: PUBLIC_REPO_NAME,
    visibility: "public",
    issue: 4703,
    sweepId: "sweep-issue-4703-0",
    ingestedAt: "2026-07-31T12:05:01Z",
    record: {
      kind: "sweep.outcome",
      repo: PUBLIC_REPO_NAME,
      visibility: "public",
      issue: 4703,
      sweep_id: "sweep-issue-4703-0",
      model: "opus",
      effort: "high",
      result: "success",
      total_duration_sec: 340,
    },
  };
}

function tokensHistoryFixture(): HistoryRecord {
  // tokens.snapshot passes through the API unredacted (documented Phase-2
  // decision) — this page must still never render it (issue #4753's
  // resolution of the #4752 exposure question).
  return {
    id: 3,
    schemaVersion: 1,
    emittedAt: "2026-07-31T12:06:00Z",
    hostId: "host-abc",
    kind: "tokens.snapshot",
    repo: null,
    visibility: "private",
    issue: null,
    sweepId: null,
    ingestedAt: "2026-07-31T12:06:01Z",
    record: {
      kind: "tokens.snapshot",
      captured_at: "2026-07-31T12:06:00Z",
      accounts: [{ account: "agent-9@2amlogic.com", rank: 0, usage_fraction: 0.42, exhausted: false }],
    },
  };
}

function privateActiveSweepFixture(): PublicActiveSweep {
  // Shaped like `redactActiveSweep`'s output: repo/issue/sweepId entirely
  // absent (not null), only lifecycle/model fields survive.
  return {
    hostId: "host-abc",
    visibility: "private",
    phase: "builder",
    startedAt: "2026-07-31T12:00:00Z",
    enteredPhaseAt: "2026-07-31T12:03:00Z",
    model: "opus",
    effort: "high",
    updatedAt: "2026-07-31T12:03:00Z",
  };
}

function publicActiveSweepFixture(): PublicActiveSweep {
  return {
    hostId: "host-abc",
    visibility: "public",
    repo: PUBLIC_REPO_NAME,
    issue: 4703,
    sweepId: "sweep-issue-4703-0",
    phase: "builder",
    startedAt: "2026-07-31T12:00:00Z",
    enteredPhaseAt: "2026-07-31T12:03:00Z",
    model: "opus",
    effort: "high",
    updatedAt: "2026-07-31T12:03:00Z",
  };
}

function snapshotFixture(): RedactedFleetSnapshot {
  return {
    hosts: {
      "host-abc": {
        health: {
          record: { kind: "host.health", captured_at: "2026-07-31T12:00:00Z", uptime_sec: 100, logical_cpus: 8 },
          updatedAt: "2026-07-31T12:00:00Z",
        },
        // Present on the redacted snapshot exactly as the real API returns
        // it (tokens.snapshot is unredacted at the API layer) — this page
        // must still never render `accounts`.
        tokens: {
          record: {
            kind: "tokens.snapshot",
            captured_at: "2026-07-31T12:00:00Z",
            accounts: [{ account: "agent-9@2amlogic.com", rank: 0, usage_fraction: 0.9, exhausted: false }],
          },
          updatedAt: "2026-07-31T12:00:00Z",
        },
      },
    },
    activeSweeps: [privateActiveSweepFixture(), publicActiveSweepFixture()],
  };
}

function historyFixture(): HistoryQueryResult {
  return {
    records: [privateHistoryFixture(), publicHistoryFixture(), tokensHistoryFixture()],
    nextCursor: null,
  };
}

describe("renderFleetOverview — redaction", () => {
  it("a private active sweep renders no repo/issue/sweepId; a public one renders full detail", () => {
    const html = renderFleetOverview(snapshotFixture());
    expect(html).toContain(PUBLIC_REPO_NAME);
    expect(html).toContain("#4703");
    expect(html).toContain("sweep-issue-4703-0");
    // Private sweep still renders its safe lifecycle fields.
    expect(html).toContain("builder");
    expect(html).toContain("opus");
  });

  it("never renders tokens.snapshot account identifiers, even though the redacted snapshot carries them unredacted", () => {
    const html = renderFleetOverview(snapshotFixture());
    expect(html).not.toContain("agent-9@2amlogic.com");
    expect(html).not.toContain("accounts");
  });

  it("renders host.health lifecycle detail", () => {
    const html = renderFleetOverview(snapshotFixture());
    expect(html).toContain("uptime_sec=100");
  });
});

describe("renderHistoryTable — redaction", () => {
  it("a private-repo record renders zero occurrences of the repo name, issue title, branch name, or PR link", () => {
    const html = renderHistoryTable(historyFixture());
    expect(html).not.toContain(PRIVATE_REPO_NAME);
    expect(html).not.toContain(PRIVATE_ISSUE_TITLE);
    expect(html).not.toContain(PRIVATE_BRANCH);
    expect(html).not.toContain(PRIVATE_PR_LINK);
    expect(html).not.toContain(PRIVATE_SWEEP_ID);
    expect(html).not.toContain("9999");
  });

  it("a private-repo record renders only its per-kind allowlisted fields", () => {
    const html = renderHistoryTable({ records: [privateHistoryFixture()], nextCursor: null });
    expect(html).toContain("private</span>"); // repo cell placeholder
    expect(html).toContain("model=opus");
    expect(html).toContain("effort=high");
    expect(html).toContain("result=success");
    expect(html).toContain("total_duration_sec=512");
  });

  it("a public-repo record renders full detail: repo, issue, sweepId", () => {
    const html = renderHistoryTable({ records: [publicHistoryFixture()], nextCursor: null });
    expect(html).toContain(PUBLIC_REPO_NAME);
    expect(html).toContain("#4703");
    expect(html).toContain("sweep-issue-4703-0");
  });

  it("never renders tokens.snapshot rows at all", () => {
    const html = renderHistoryTable(historyFixture());
    expect(html).not.toContain("agent-9@2amlogic.com");
    expect(html).not.toContain("tokens.snapshot");
  });

  it("isPublicPageDisplayKind excludes tokens.snapshot and includes the sweep.* / host.health kinds", () => {
    expect(isPublicPageDisplayKind("tokens.snapshot")).toBe(false);
    expect(isPublicPageDisplayKind("sweep.outcome")).toBe(true);
    expect(isPublicPageDisplayKind("host.health")).toBe(true);
    expect(isPublicPageDisplayKind("some.future.kind")).toBe(false);
  });
});

describe("renderPublicPage — full document", () => {
  const html = renderPublicPage(snapshotFixture(), historyFixture());

  it("contains zero occurrences of the private repo name, issue title, branch name, sweep id, or PR link", () => {
    expect(html).not.toContain(PRIVATE_REPO_NAME);
    expect(html).not.toContain(PRIVATE_ISSUE_TITLE);
    expect(html).not.toContain(PRIVATE_BRANCH);
    expect(html).not.toContain(PRIVATE_SWEEP_ID);
    expect(html).not.toContain(PRIVATE_PR_LINK);
  });

  it("contains zero occurrences of token account identifiers", () => {
    expect(html).not.toContain("agent-9@2amlogic.com");
  });

  it("renders full detail for the public-repo record/sweep", () => {
    expect(html).toContain(PUBLIC_REPO_NAME);
    expect(html).toContain("sweep-issue-4703-0");
  });

  it("is read-only: no <form> elements and no mutating fetch/EventSource call", () => {
    expect(html).not.toContain("<form");
    expect(html.toLowerCase()).not.toContain('method="post"');
    expect(html.toLowerCase()).not.toContain("method: \"post\"");
    expect(html.toLowerCase()).not.toContain("method:'post'");
  });

  it("never references /api/*, /admin/*, or /ingest — only /public/*", () => {
    expect(html).not.toContain("/api/");
    expect(html).not.toContain("/admin/");
    expect(html).not.toContain("/ingest");
    expect(html).toContain("/public/events");
  });

  // Covered here rather than through `GET /`, which serves the SPA shell
  // whenever a UI build is present (see the e2e block below). This page is
  // the no-UI-build fallback, so its Sign in affordance needs its own test.
  it("offers a Sign in link pointing at /login", () => {
    expect(html).toContain('href="/login"');
  });
});

// ---------------------------------------------------------------------------
// End-to-end: the actual GET / (and GET /public redirect) routes, through
// the real Worker + D1 + Durable Object, mirroring test/redaction.test.ts's
// integration style. Issue #4795 moved the always-rendered public page from
// `/public` to `/` (with an authenticated variant alongside it, covered in
// index.test.ts) — `/public` itself now only 301s.
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

beforeEach(async () => {
  await seedHost(env.DB, "host-abc", "abc-ingest-key");
});

describe("GET /public — redirects to /", () => {
  it("301s to /", async () => {
    const response = await callWorker(new Request("https://ingest.example/public", { redirect: "manual" }));
    expect(response.status).toBe(301);
    expect(response.headers.get("location")).toBe("https://ingest.example/");
  });
});

describe("GET /login — bounces back to /", () => {
  // Access itself gates this path at the edge (config, not code), so by the
  // time the Worker sees the request the session cookie already exists —
  // all the route owes the visitor is a trip back to `/`.
  it("302s to /", async () => {
    const response = await callWorker(new Request("https://ingest.example/login", { redirect: "manual" }));
    expect(response.status).toBe(302);
    expect(response.headers.get("location")).toBe("https://ingest.example/");
  });
});

describe("GET / — end to end (anonymous)", () => {
  it("serves an HTML page reachable without authentication", async () => {
    const response = await callWorker(new Request("https://ingest.example/"));
    expect(response.status).toBe(200);
    expect(response.headers.get("content-type")).toContain("text/html");
  });

  it("tells the UI it is anonymous, so it requests the redacted dataset", async () => {
    const response = await callWorker(new Request("https://ingest.example/"));
    const html = await response.text();
    expect(html).toContain('window.__LOOM_FLEET__={"authenticated":false};');
  });

  it("a private sweep.outcome record's repo/issue/sweep_id never appear on the rendered page", async () => {
    await ingest([
      sweepOutcomeEnvelope({
        visibility: "private",
        repo: "rjwalters/e2e-private-repo",
        issue: 8888,
        sweep_id: "sweep-issue-8888-0",
      }),
    ]);

    const response = await callWorker(new Request("https://ingest.example/"));
    const html = await response.text();
    expect(html).not.toContain("rjwalters/e2e-private-repo");
    expect(html).not.toContain("sweep-issue-8888-0");
    expect(html).not.toContain("8888");
  });

  it("a public sweep.started record keeps full detail in the anonymous dataset", async () => {
    await ingest([sweepStartedEnvelope({ visibility: "public", repo: "rjwalters/loom", issue: 4703 })]);

    // Asserted against the dataset the anonymous UI actually fetches. The
    // shell at `/` carries no records at all, so "public repos stay visible
    // to anonymous visitors" is only observable here.
    const response = await callWorker(new Request("https://ingest.example/public/fleet-state"));
    const body = await response.text();
    expect(body).toContain("rjwalters/loom");
    expect(body).toContain("4703");
  });

  it("tokens.snapshot's account identifiers never appear on the rendered page", async () => {
    await ingest([tokensSnapshotEnvelope({ accounts: [{ account: "e2e-secret-account", rank: 0, usage_fraction: 0.5, exhausted: false }] })]);

    const response = await callWorker(new Request("https://ingest.example/"));
    const html = await response.text();
    expect(html).not.toContain("e2e-secret-account");
  });

  it("a garbage CF_Authorization cookie still falls back to the public view, never a 500", async () => {
    const response = await callWorker(
      new Request("https://ingest.example/", { headers: { cookie: "CF_Authorization=not-a-real-jwt" } }),
    );
    expect(response.status).toBe(200);
    const html = await response.text();
    expect(html).toContain('window.__LOOM_FLEET__={"authenticated":false};');
  });
});
