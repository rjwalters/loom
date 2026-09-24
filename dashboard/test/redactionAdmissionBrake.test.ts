import { describe, expect, it } from "vitest";
import { redactPayload } from "../src/redaction";

// ---------------------------------------------------------------------------
// `host.health.admission_brake` redaction (Issue #8478).
//
// A sibling suite rather than more lines in `redaction.test.ts`, which is at
// the `scripts/check-file-size-budget.sh` 1000-code-line threshold — the
// file-size policy's preferred remedy is "put the new code in a new sibling
// module", not "trim something unrelated to make room".
//
// The boundary under test: every scalar of the brake summary is machine
// detail and survives for a public viewer, but `top_cpu_consumers` is a
// `ps`-derived list of executable basenames — workload detail — and must not.
// ---------------------------------------------------------------------------

describe("redactPayload — host.health.admission_brake (#8478)", () => {
  it("host.health: the admission-brake summary survives for the public view, minus the process list (#8478)", () => {
    const payload = {
      kind: "host.health",
      daemon_version: "0.19.250",
      uptime_sec: 43_440,
      admission_brake: {
        held: true,
        starving_since: "2026-09-20T02:00:00Z",
        starving_secs: 43_440,
        starvation_warn_secs: 300,
        escape_hatch_grants: 47,
        dispatch_suppressed_by_foreign_load: true,
        top_cpu_consumers: "ngspice ×25 (1843% cpu, parent launchd[1], reparented to pid 1)",
      },
    };
    const redacted = redactPayload("host.health", payload);
    // The whole point of #8478: a public fleet viewer must be able to see that
    // this host's dispatch has been suppressed for 12 hours by load Loom does
    // not own, without being told which executables were running.
    expect(redacted.admission_brake).toEqual({
      held: true,
      starving_since: "2026-09-20T02:00:00Z",
      starving_secs: 43_440,
      starvation_warn_secs: 300,
      escape_hatch_grants: 47,
      dispatch_suppressed_by_foreign_load: true,
    });
    expect(redacted.admission_brake).not.toHaveProperty("top_cpu_consumers");
  });

  it("host.health: no admission_brake key at all when the payload carries no brake (#8478)", () => {
    const payload = { kind: "host.health", daemon_version: "0.17.0", uptime_sec: 100 };
    expect(redactPayload("host.health", payload)).not.toHaveProperty("admission_brake");
  });

  // -------------------------------------------------------------------------
  // The end-to-end boundary (Judge finding on PR #8547).
  //
  // Dropping `top_cpu_consumers` from the brake row is only half a boundary:
  // `halt_reason` is an UNCONDITIONAL member of `host.health`'s public
  // allowlist, copied verbatim, and the daemon derives it from the same brake
  // summary. The first cut of this PR interpolated the attribution into it, so
  // the redacted row hid the process list and the allowlisted free-text field
  // handed it straight back — with a pair of tests that each passed while
  // contradicting the other.
  //
  // This test therefore pins BOTH properties at once, and does so by scanning
  // the WHOLE public projection rather than one named field, so a future
  // free-text field cannot reopen the same leak under a different key.
  // -------------------------------------------------------------------------

  /** Every attribution token a `ps`-derived clause can contain — the command
   * basename, its parent, and the structural markers unique to an attribution.
   * Mirrors `ATTRIBUTION_TOKENS` in
   * `loom-daemon/src/observability/collector/admission_brake_tests.rs`. */
  const ATTRIBUTION_TOKENS = [
    "ngspice",
    "launchd",
    "TOP CPU",
    "reparented",
    "pid 1",
    "ps` sample",
  ] as const;

  /** Every string reachable in `value`, at any depth (keys included) — so the
   * assertion is "no process name survives *anywhere* in the public
   * projection", not "no process name survives in the one field we remembered
   * to check". */
  function collectStrings(value: unknown, into: string[] = []): string[] {
    if (typeof value === "string") into.push(value);
    else if (Array.isArray(value)) for (const item of value) collectStrings(item, into);
    else if (value && typeof value === "object") {
      for (const [key, item] of Object.entries(value)) {
        into.push(key);
        collectStrings(item, into);
      }
    }
    return into;
  }

  it("host.health: no process attribution survives anywhere in the public projection, halt_reason included (#8478, PR #8547)", () => {
    const payload = {
      kind: "host.health",
      daemon_version: "0.19.250",
      uptime_sec: 43_440,
      dispatch_halted: true,
      // Byte-for-byte what `dispatch_halt_from_breaker`
      // (`loom-daemon/src/observability/collector.rs`) emits for this brake:
      // duration, the emitting host's own threshold, and the non-attributing
      // verdict — no process names. Its Rust-side counterpart is
      // `the_halt_reason_never_carries_process_attribution_past_the_public_boundary`.
      halt_reason:
        "admission brake STARVING for 43440s with 0 sweeps in flight (≥ this host's " +
        "starvationWarnSecs 300); dispatch is suppressed by load Loom does not own (#8478)",
      admission_brake: {
        held: true,
        starving_since: "2026-09-20T02:00:00Z",
        starving_secs: 43_440,
        starvation_warn_secs: 300,
        escape_hatch_grants: 47,
        dispatch_suppressed_by_foreign_load: true,
        top_cpu_consumers:
          " — TOP CPU (best-effort host-wide `ps` sample, includes work Loom does not own): " +
          "ngspice ×25 (1843% cpu, parent launchd[1], reparented to pid 1). A compute process " +
          "reparented to pid 1 is owned by NO live Loom session — see " +
          ".loom/docs/long-running-compute.md (#8478)",
      },
    };

    const redacted = redactPayload("host.health", payload);
    const haystack = collectStrings(redacted).join("\n");
    for (const token of ATTRIBUTION_TOKENS) {
      expect(haystack, `attribution token ${JSON.stringify(token)} leaked to the public view`)
        .not.toContain(token);
    }

    // Not a vacuous pass: the fleet-level signal this PR exists to surface is
    // still fully public — the host renders as degraded, with the duration and
    // the verdict, just without naming the binaries.
    expect(redacted.dispatch_halted).toBe(true);
    expect(redacted.halt_reason).toBe(payload.halt_reason);
    expect(redacted.admission_brake).toMatchObject({
      starving_secs: 43_440,
      dispatch_suppressed_by_foreign_load: true,
    });
  });
});
