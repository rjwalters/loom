import { describe, expect, it } from "vitest";
import { redactPayload } from "../src/redaction";

// ---------------------------------------------------------------------------
// `host.health` fleet-captain state redaction (Issue #8848).
//
// A sibling suite rather than more lines in `redaction.test.ts`, which is at
// the `scripts/check-file-size-budget.sh` 1000-code-line threshold — the
// file-size policy's preferred remedy is "put the new code in a new sibling
// module", not "trim something unrelated to make room". Same shape as
// `redactionAdmissionBrake.test.ts` next door.
//
// The boundary under test: `is_captain` and `armed_singleton_jobs` describe
// this machine's own role in an operator-assigned fleet-wide designation —
// the same "describes the machine, not the work" footing as
// `dispatch_halted`/`halt_reason`/`protection` — so both survive for a public
// viewer. A singleton job name is an allowlisted identifier a repo declares
// (`fleet_captain::arm_singleton_job`'s `job_name`), on the same footing as a
// role name; neither field names a repo, issue, branch, or operator.
// ---------------------------------------------------------------------------

describe("redactPayload — host.health fleet captain state (#8848)", () => {
  it("host.health: fleet captain state (is_captain/armed_singleton_jobs) survives redaction (#8848)", () => {
    // Describes the machine's own role in the fleet-wide singleton-job
    // mechanism, not any repo/operator — same reasoning as
    // `dispatch_halted`/`halt_reason`/`protection` above it in the
    // allowlist. Job names are allowlisted identifiers a repo declares, the
    // same footing as a role name.
    const payload = {
      kind: "host.health",
      captured_at: "2026-08-02T12:00:00Z",
      daemon_version: "0.17.0",
      uptime_sec: 86400,
      is_captain: true,
      armed_singleton_jobs: ["edge-queue-pull"],
    };
    expect(redactPayload("host.health", payload)).toEqual(payload);
  });

  it("host.health: is_captain false (a declared captain that is not this host) survives redaction (#8848)", () => {
    const payload = {
      kind: "host.health",
      captured_at: "2026-08-02T12:00:00Z",
      daemon_version: "0.17.0",
      uptime_sec: 86400,
      is_captain: false,
    };
    expect(redactPayload("host.health", payload)).toEqual(payload);
  });
});
