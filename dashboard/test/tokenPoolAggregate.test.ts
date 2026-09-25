import { describe, expect, it } from "vitest";
import { deriveTokenPoolAggregate } from "../src/redaction";

describe("deriveTokenPoolAggregate", () => {
  it("reports null rather than a misleading zero when no account measured usage", () => {
    expect(deriveTokenPoolAggregate({ accounts: [{ account: "a", exhausted: false }] })).toEqual({
      account_count: 1,
      exhausted_count: 0,
      mean_usage_fraction: null,
      max_usage_fraction: null,
      next_limit_window_reset_at: null,
      providers: [
        {
          provider: "claude",
          account_count: 1,
          exhausted_count: 0,
          max_usage_fraction: null,
          next_limit_window_reset_at: null,
        },
      ],
    });
  });

  it("splits the aggregate per provider, in first-seen order, naming no account", () => {
    const aggregate = deriveTokenPoolAggregate({
      accounts: [
        { account: "agent-1", provider: "claude", usage_fraction: 0.5, exhausted: false },
        { account: "agent-2", provider: "claude", usage_fraction: 1, exhausted: true, limit_window_reset_at: "2026-08-02T03:00:00Z" },
        { account: "cx-1", provider: "codex", exhausted: false },
        { account: "cx-2", provider: "codex", exhausted: true, limit_window_reset_at: "2026-07-31T01:00:00Z" },
        { account: "cx-3", provider: "codex", exhausted: true, limit_window_reset_at: "2026-07-30T20:00:00Z" },
      ],
    });
    expect(aggregate.providers).toEqual([
      {
        provider: "claude",
        account_count: 2,
        exhausted_count: 1,
        max_usage_fraction: 1,
        next_limit_window_reset_at: "2026-08-02T03:00:00Z",
      },
      {
        provider: "codex",
        account_count: 3,
        exhausted_count: 2,
        // Codex accounts report no usage fraction — null, never 0.
        max_usage_fraction: null,
        next_limit_window_reset_at: "2026-07-30T20:00:00Z",
      },
    ]);
    // Pool-wide totals still span every provider.
    expect(aggregate.account_count).toBe(5);
    expect(aggregate.exhausted_count).toBe(3);
    expect(JSON.stringify(aggregate)).not.toMatch(/agent-|cx-/);
  });

  it("yields no provider slices at all for an empty pool", () => {
    expect(deriveTokenPoolAggregate({ accounts: [] }).providers).toEqual([]);
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
      providers: [],
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
