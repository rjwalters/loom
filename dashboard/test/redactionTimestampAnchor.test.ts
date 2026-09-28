import { describe, expect, it } from "vitest";
import { redactHistoryRecord } from "../src/redaction";
import { sweepOutcomeEnvelope } from "./testHelpers";

// ---------------------------------------------------------------------------
// Regression coverage for the #9021 flake, extracted from redaction.test.ts's
// end-to-end block (the file-size ratchet holds that file under the
// 1000-code-line budget, so this test lives in a sibling — see
// scripts/check-file-size-budget.sh and .loom/docs/file-size-policy.md).
//
// The end-to-end `/public/history` block anchors its private-field leak
// assertions on JSON key/value shapes (`"issue":<n>`, `"pr_number":`, …)
// precisely so they cannot match the digits of a wall-clock `ingestedAt`
// timestamp. Merge train #9020 failed when a timestamp's milliseconds read
// `.214` and the old bare `not.toContain("214")` (lines_added) tripped with
// nothing leaked. Workerd's wall clock cannot be frozen from
// vitest-pool-workers (see fleetState.test.ts's module doc), so the exact
// failing body shape is reproduced deterministically here through the real
// redaction path instead.
// ---------------------------------------------------------------------------

describe("redaction leak anchors vs ingestedAt millisecond digits (#9021)", () => {
  it("the anchored leak assertions cannot match ingestedAt millisecond digits (#9021)", () => {
    const millis214 = "2026-09-26T05:54:56.214Z";
    const redacted = redactHistoryRecord(
      {
        id: 7,
        schemaVersion: 1,
        emittedAt: millis214,
        hostId: "host-abc",
        kind: "sweep.outcome",
        repo: "rjwalters/loom",
        visibility: "private",
        issue: 4703,
        sweepId: "sweep-issue-4703-0",
        ingestedAt: millis214,
        record: sweepOutcomeEnvelope({ visibility: "private" }).record as Record<string, unknown>,
      },
      false,
    );
    const publicText = JSON.stringify(redacted);

    // Witness: the body really does carry the `.214` millis digits, and the
    // bare-number form the e2e block used demonstrably false-fails on them —
    // the exact #9020 signature, kept executable so the anchored forms'
    // premise cannot silently rot.
    expect(publicText).toContain(".214");
    expect(() => expect(publicText).not.toContain("214")).toThrow();

    // The anchored forms used in redaction.test.ts's e2e block are immune to
    // those same digits …
    expect(publicText).not.toMatch(/"issue"\s*:\s*4703\b/);
    expect(publicText).not.toMatch(/"pr_number"\s*:/);
    expect(publicText).not.toMatch(/"tokens_in"\s*:/);
    expect(publicText).not.toMatch(/"tokens_out"\s*:/);
    expect(publicText).not.toMatch(/"lines_added"\s*:/);

    // … and still fail on a real leak (every anchored pattern trips when
    // the private fields are present, whatever their values).
    const leaked = JSON.stringify({
      ...redacted,
      issue: 4703,
      record: {
        ...redacted.record,
        pr_number: 4710,
        tokens_in: 48213,
        tokens_out: 6120,
        lines_added: 214,
      },
    });
    expect(leaked).toMatch(/"issue"\s*:\s*4703\b/);
    expect(leaked).toMatch(/"pr_number"\s*:/);
    expect(leaked).toMatch(/"tokens_in"\s*:/);
    expect(leaked).toMatch(/"tokens_out"\s*:/);
    expect(leaked).toMatch(/"lines_added"\s*:/);
  });
});
