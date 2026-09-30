import { describe, expect, it } from "vitest";
import { redactPayload, redactPhaseDurations } from "../src/redaction";

// ---------------------------------------------------------------------------
// `sweep.outcome.phase_durations` redaction (Issue #9443).
//
// A sibling suite rather than more lines in `redaction.test.ts`, which is at
// the `scripts/check-file-size-budget.sh` 1000-code-line threshold — the
// file-size policy's preferred remedy is "put the new code in a new sibling
// module", not "trim something unrelated to make room". Same reasoning
// `redactionAdmissionBrake.test.ts` was split out on.
//
// The boundary under test: `phase_durations` has always been public, because
// its entries were `{phase, duration_sec}` — lifecycle *shape*, the same
// category `total_duration_sec` and `result` are allowed through on. Issue
// #9443 put per-phase `tokens_in`/`tokens_out`/`tokens_by_model` INSIDE each
// entry. The field allowlist is a shallow copy, so leaving `phase_durations`
// in it would have carried exactly the token counts `redaction.test.ts`'s
// #5357 cases assert are withheld — and in a finer-grained form than the
// sweep totals they partition. The field therefore reaches a public response
// only through `redactPhaseDurations`.
// ---------------------------------------------------------------------------

/** A realistic post-#9443 record: the five-entry
 * `curator → builder → judge(fail) → doctor → judge(pass)` lifecycle, with the
 * re-judge unmeasured and the remainder reported explicitly. */
function outcomeWithPerPhaseUsage(): Record<string, unknown> {
  return {
    kind: "sweep.outcome",
    repo: "rjwalters/loom",
    visibility: "private",
    issue: 9443,
    sweep_id: "sweep-issue-9443-0",
    model: "opus",
    effort: "high",
    config: { runtime: "claude" },
    phase_durations: [
      { phase: "curator", duration_sec: 12, attempt: 1, tokens_in: 4200, tokens_out: 510 },
      { phase: "builder", duration_sec: 340, attempt: 1, tokens_in: 38_000, tokens_out: 4900 },
      {
        phase: "judge",
        duration_sec: 60,
        attempt: 1,
        tokens_in: 3100,
        tokens_out: 400,
        tokens_by_model: [{ model: "claude-opus-5", speed: "standard", input: 3100, output: 400 }],
      },
      { phase: "doctor", duration_sec: 45, attempt: 1, tokens_in: 1800, tokens_out: 200 },
      // Unmeasured: no token keys at all (absent, never 0).
      { phase: "judge", duration_sec: 20, attempt: 2 },
    ],
    total_duration_sec: 512,
    result: "success",
    pr_number: 9450,
    tokens_in: 48_213,
    tokens_out: 6120,
    tokens_unattributed: { tokens_in: 1113, tokens_out: 110 },
  };
}

describe("redactPhaseDurations — per-phase token usage never reaches a public view", () => {
  it("keeps phase/duration_sec/attempt on every entry and drops every usage key", () => {
    const redacted = redactPayload("sweep.outcome", outcomeWithPerPhaseUsage());
    expect(redacted.phase_durations).toEqual([
      { phase: "curator", duration_sec: 12, attempt: 1 },
      { phase: "builder", duration_sec: 340, attempt: 1 },
      { phase: "judge", duration_sec: 60, attempt: 1 },
      { phase: "doctor", duration_sec: 45, attempt: 1 },
      // The re-judge survives as its own entry: the lifecycle's *shape* — that
      // a second judge happened — is exactly what `phase_durations` is public
      // for. Only its cost is withheld.
      { phase: "judge", duration_sec: 20, attempt: 2 },
    ]);
    // The allowlisted scalars still survive alongside the projection.
    expect(redacted).toMatchObject({ kind: "sweep.outcome", model: "opus", result: "success" });
  });

  it("strips the sweep-level tokens_unattributed remainder too", () => {
    const redacted = redactPayload("sweep.outcome", outcomeWithPerPhaseUsage());
    // It is a remainder of `tokens_in`/`tokens_out`, which #5357 already
    // withholds — publishing it would leak a bound on what it is a remainder of.
    expect(redacted).not.toHaveProperty("tokens_unattributed");
    expect(redacted).not.toHaveProperty("tokens_in");
    expect(redacted).not.toHaveProperty("tokens_out");
  });

  it("leaves no token count anywhere in the serialized public projection", () => {
    const json = JSON.stringify(redactPayload("sweep.outcome", outcomeWithPerPhaseUsage()));
    for (const leaked of [
      "tokens_in",
      "tokens_out",
      "tokens_by_model",
      "tokens_unattributed",
      "4200",
      "38000",
      "3100",
      "1800",
      "48213",
      "6120",
      "1113",
      "claude-opus-5",
    ]) {
      expect(json).not.toContain(leaked);
    }
  });

  // The fail-safe direction: a field added to the phase-entry schema tomorrow
  // must be dropped by default, not copied through because nobody updated this
  // module. The projection is a per-field pick, not a spread.
  it("drops an unknown per-entry field by default", () => {
    expect(
      redactPhaseDurations([
        { phase: "builder", duration_sec: 1, attempt: 1, cost_usd: 4.2, agent_id: "a1" },
      ]),
    ).toEqual([{ phase: "builder", duration_sec: 1, attempt: 1 }]);
  });

  // A pre-#9443 record (no `attempt`, no usage) is unchanged by the
  // projection, so the change is invisible to an existing public consumer.
  it("passes a pre-#9443 entry through unchanged", () => {
    expect(redactPhaseDurations([{ phase: "builder", duration_sec: 340 }])).toEqual([
      { phase: "builder", duration_sec: 340 },
    ]);
  });

  // No type-confusion path carries raw usage out: a malformed entry projects
  // to `{}` rather than being copied.
  it("projects a non-object entry to {} rather than copying it", () => {
    expect(redactPhaseDurations(["builder", null, 7, [{ tokens_in: 7 }]])).toEqual([{}, {}, {}, {}]);
    const json = JSON.stringify(redactPhaseDurations([[{ tokens_in: 4242 }]]));
    expect(json).not.toContain("4242");
  });

  // Absent stays absent: a sweep whose transitions were never sampled carries
  // no breakdown, and the derivation must not invent an empty one — the
  // field-presence contract stays "the daemon sent this".
  it("adds no phase_durations to a record that carries none", () => {
    expect(
      redactPayload("sweep.outcome", { kind: "sweep.outcome", visibility: "private", result: "failure" }),
    ).not.toHaveProperty("phase_durations");
    // A non-array value (malformed payload) is likewise not projected.
    expect(
      redactPayload("sweep.outcome", {
        kind: "sweep.outcome",
        visibility: "private",
        phase_durations: { phase: "builder", tokens_in: 4242 },
        result: "failure",
      }),
    ).not.toHaveProperty("phase_durations");
  });

  // An empty measured breakdown is distinct from an absent one and survives as
  // `[]`, matching the schema doc's "unknown != empty" contract.
  it("preserves a measured-but-empty breakdown as []", () => {
    const redacted = redactPayload("sweep.outcome", {
      kind: "sweep.outcome",
      visibility: "private",
      phase_durations: [],
      result: "success",
    });
    expect(redacted.phase_durations).toEqual([]);
  });
});
