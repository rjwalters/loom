import { describe, expect, it } from "vitest";
import { decodeVisibility, extractRecordFields, validateEnvelope } from "../src/telemetry";
import { hostHealthEnvelope, sweepStartedEnvelope } from "./testHelpers";

describe("decodeVisibility — fail-safe-to-private (mirrors the Rust RepoVisibility decode)", () => {
  it("maps exactly the string \"public\" (case-insensitive) to public", () => {
    expect(decodeVisibility("public")).toBe("public");
    expect(decodeVisibility("PUBLIC")).toBe("public");
    expect(decodeVisibility("PuBlIc")).toBe("public");
  });

  it("maps every other shape to private, never public", () => {
    const malformedInputs: unknown[] = [
      undefined, // missing field
      null,
      "private",
      "internal", // unknown label
      "Public ", // trailing space — not exactly "public"
      "",
      true,
      false,
      0,
      1,
      ["public"],
      { v: "public" },
    ];
    for (const value of malformedInputs) {
      expect(decodeVisibility(value), `decodeVisibility(${JSON.stringify(value)})`).toBe("private");
    }
  });
});

describe("validateEnvelope", () => {
  it("accepts a well-formed envelope", () => {
    const result = validateEnvelope(sweepStartedEnvelope(), 0);
    expect(result.ok).toBe(true);
  });

  it("rejects a missing schema_version (never coerces to 0)", () => {
    const envelope = sweepStartedEnvelope();
    delete (envelope as Record<string, unknown>).schema_version;
    const result = validateEnvelope(envelope, 2);
    expect(result.ok).toBe(false);
    if (!result.ok) {
      expect(result.error.index).toBe(2);
      expect(result.error.reason).toMatch(/schema_version/);
    }
  });

  it("rejects a non-integer schema_version", () => {
    const envelope = { ...sweepStartedEnvelope(), schema_version: "1" };
    const result = validateEnvelope(envelope, 0);
    expect(result.ok).toBe(false);
  });

  it("accepts an unrecognized HIGHER schema_version (forward-compatible)", () => {
    const envelope = { ...sweepStartedEnvelope(), schema_version: 99 };
    const result = validateEnvelope(envelope, 0);
    expect(result.ok).toBe(true);
    if (result.ok) {
      expect(result.envelope.schema_version).toBe(99);
    }
  });

  it("rejects a missing record", () => {
    const envelope = sweepStartedEnvelope();
    delete (envelope as Record<string, unknown>).record;
    const result = validateEnvelope(envelope, 0);
    expect(result.ok).toBe(false);
  });

  it("rejects a record with no kind", () => {
    const envelope = sweepStartedEnvelope();
    (envelope.record as Record<string, unknown>).kind = undefined;
    const result = validateEnvelope(envelope, 0);
    expect(result.ok).toBe(false);
  });

  it("rejects a non-object envelope (e.g. a bare string in the array)", () => {
    const result = validateEnvelope("not-an-envelope", 0);
    expect(result.ok).toBe(false);
  });
});

describe("extractRecordFields", () => {
  it("extracts repo/visibility/issue/sweep_id for a repo-scoped record", () => {
    const envelope = sweepStartedEnvelope();
    const result = validateEnvelope(envelope, 0);
    if (!result.ok) throw new Error("expected valid envelope");
    const fields = extractRecordFields(result.envelope.record);
    expect(fields).toEqual({
      kind: "sweep.started",
      repo: "rjwalters/loom",
      visibility: "public",
      issue: 4703,
      sweepId: "sweep-issue-4703-0",
    });
  });

  it("leaves repo/issue/sweep_id undefined for a host-level record", () => {
    const result = validateEnvelope(hostHealthEnvelope(), 0);
    if (!result.ok) throw new Error("expected valid envelope");
    const fields = extractRecordFields(result.envelope.record);
    expect(fields.kind).toBe("host.health");
    expect(fields.repo).toBeUndefined();
    expect(fields.issue).toBeUndefined();
    expect(fields.sweepId).toBeUndefined();
    // No visibility field at all on a host-level record — must still
    // fail-safe to private, never left undefined/public.
    expect(fields.visibility).toBe("private");
  });
});
