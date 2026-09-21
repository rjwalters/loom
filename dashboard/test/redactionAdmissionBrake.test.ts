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
});
