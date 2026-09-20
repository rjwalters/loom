/**
 * The "running now" panel: every live `ephemeral_compute` job instance
 * (Issue #8306, Phase 3 of #8257).
 *
 * Renders the Durable Object's `compute:<jobId>` live-state entries — the
 * launch-record side of the two-record job lifecycle Phase 1 established
 * (`../../migrations/0003_ephemeral_compute.sql`) and Phase 2 turned into live
 * state (`../../src/fleetState.ts`). The completion record deletes the entry,
 * so *presence in this list is the definition of "running"*; there is no
 * "finished" row to filter out.
 *
 * ## Why this is a fleet-level list, not a per-host card section
 *
 * Every other live entity the overview renders (`activeSweeps`) belongs to a
 * `loom-daemon` host and renders inside that host's card. A compute job does
 * not: the reference emitter is a hostless elastic batch runner that
 * authenticates as one synthetic id for its whole fleet (see
 * `defaults/docs/observability.md` §5d), so `hostId` here names the reporting
 * process, not a machine with a `host.health` panel to sit beside. Grouping by
 * it would pile every instance under one card that has nothing else to show.
 *
 * ## Leaked vs. running (parent #8257 AC 4)
 *
 * A leaked job is one whose completion record never arrived within
 * `STALE_COMPUTE_MS` (24h) — the instance is very likely still billing with
 * nothing watching it, which is the single most expensive state this panel
 * exists to surface. It is therefore distinguished four ways, not one: a row
 * modifier class, a `LEAKED` badge with `role="status"`, a `data-leaked`
 * attribute (the test hook), and promotion to the top of the list
 * (`fleet.ts`'s `sortComputeJobs`). Same belt-and-braces treatment the
 * analytics panel's `EXHAUSTED` badge gets, and for the same reason: a purely
 * chromatic difference is invisible to a screen reader and to a colour-blind
 * reader both.
 *
 * ## Withheld is not empty — so neither surface gets an empty block
 *
 * `/public/fleet-state` returns `activeCompute: []` for an unauthenticated
 * viewer: the redaction policy withholds every `ephemeral_compute` field
 * *including the count* (`../../src/redaction.ts`). An empty list therefore
 * means two different things on the two surfaces — "nothing is running" when
 * signed in, "you are not being told" when not — and this panel renders
 * **neither** as a block, because it has nothing true and useful to say in
 * either case.
 *
 * Nor does the overview headline carry a "0 compute jobs" count for the
 * authenticated zero: most fleets run no elastic compute at all, and a
 * permanently-zero counter is noise on every one of them. The route that
 * always has an answer — including zero, and including the public
 * "withheld" — is `#/spend` (`../spendPanel.ts`), which is where someone
 * asking about elastic compute lands. A permanent sign-in notice on the
 * landing page would be nag, not information.
 *
 * The `authenticated` flag is still load-bearing, as defense in depth: if a
 * future backend change ever let live entries reach `/public/fleet-state`,
 * this renders the operator-only notice rather than the table. The frontend
 * does not rely on that — `src/redaction.ts` is the enforcement point — but it
 * costs one branch to not be the surface that publishes the leak.
 */

import { el } from "../dom";
import { isAuthenticatedViewer } from "../api";
import { UNKNOWN, formatAbsolute, formatDuration, formatText, secondsSince } from "../format";
import type { ActiveComputeJob } from "../types";

export interface RunningComputeOptions {
  /** Whether the viewer is signed in. Defaults to the server-injected auth
   * state (`../api.ts`'s `isAuthenticatedViewer`) — the same source
   * `panels.ts` and `analytics/bootstrap.ts` resolve their surface from, so
   * all three agree. Overridable for tests. */
  authenticated?: boolean;
}

/** How long a job has been running, from its own `started_at`.
 *
 * Uses `startedAt` (the emitter's clock) rather than `updatedAt` (this
 * backend's ingest clock) because the question a reader is asking is "how long
 * has this instance been billing", which the emitter's start time answers
 * directly. Leak *detection* deliberately uses `updatedAt` instead so a skewed
 * emitter clock cannot fake liveness — the two timestamps answer two different
 * questions and the panel uses each for its own.
 *
 * An absent or unparseable `startedAt` renders `—`, never `0s`. */
export function runningForText(job: ActiveComputeJob, now: Date = new Date()): string {
  return formatDuration(secondsSince(job.startedAt, now));
}

/** `"c7i.4xlarge · us-east-1 · spot"` — the instance's shape in one line,
 * with each part dropped when the launch record did not carry it. Returns `—`
 * when none of the three is known, so the cell is never blank.
 *
 * `spot` is rendered only as an affirmative `"spot"`: an on-demand instance is
 * the unremarkable default and `undefined` (a pre-field emitter) must not read
 * as "on-demand", so neither gets a chip. */
export function instanceShapeText(job: ActiveComputeJob): string {
  const parts = [job.instanceType, job.region, job.spot === true ? "spot" : undefined].filter(
    (part): part is string => typeof part === "string" && part.length > 0,
  );
  return parts.length > 0 ? parts.join(" · ") : UNKNOWN;
}

function jobRow(job: ActiveComputeJob, now: Date): HTMLElement {
  const leaked = job.leaked === true;
  return el(
    "tr",
    {
      class: `compute-row${leaked ? " compute-row--leaked" : ""}`,
      data: { testid: "compute-row", job: job.jobId, leaked: String(leaked) },
    },
    el(
      "td",
      { class: "compute-row__job" },
      el("span", { class: "compute-row__job-id" }, job.jobId),
      leaked
        ? el(
            "span",
            {
              class: "badge badge--leaked",
              role: "status",
              title:
                "No completion record for over 24 hours — this instance may still be running " +
                "and billing. Check the cloud console and terminate it if it is orphaned.",
              data: { testid: "leaked-badge" },
            },
            "LEAKED",
          )
        : null,
    ),
    el("td", { class: "compute-row__instance" }, formatText(job.instanceId)),
    el("td", { class: "compute-row__shape" }, instanceShapeText(job)),
    el(
      "td",
      {
        class: "compute-row__age",
        title: job.startedAt ? `Started ${formatAbsolute(job.startedAt)}` : undefined,
      },
      runningForText(job, now),
    ),
  );
}

/** The operator-only notice the public surface gets in place of the list —
 * see the module doc's "Withheld is not empty". */
function withheldNotice(): HTMLElement {
  return el(
    "p",
    { class: "compute__note compute__note--withheld", data: { testid: "compute-withheld" } },
    "Ephemeral compute activity is operator-only — job, instance, region and cost detail " +
      "describe a private compute fleet. Sign in to see what is running.",
  );
}

/**
 * The whole panel, or `null` when there is nothing to render — an empty job
 * list on either surface (see the module doc's "Withheld is not empty").
 *
 * Most fleets run no elastic compute at all, so an always-present "no jobs"
 * block would be permanent noise on the overview of every one of them; the
 * `#/spend` route is where zero always has an answer.
 */
export function runningComputeSection(
  jobs: readonly ActiveComputeJob[],
  now: Date = new Date(),
  options: RunningComputeOptions = {},
): HTMLElement | null {
  if (jobs.length === 0) return null;
  const authenticated = options.authenticated ?? isAuthenticatedViewer();

  const leakedCount = jobs.filter((job) => job.leaked === true).length;

  return el(
    "section",
    { class: "compute", data: { testid: "running-compute" } },
    el(
      "header",
      { class: "compute__header" },
      el("h2", { class: "compute__title" }, "Ephemeral compute running now"),
      leakedCount > 0
        ? el(
            "span",
            {
              class: "badge badge--leaked",
              data: { testid: "compute-leaked-count" },
              title: "Jobs with no completion record for over 24 hours — likely orphaned instances",
            },
            `${leakedCount} possibly leaked`,
          )
        : null,
    ),
    authenticated
      ? el(
          "table",
          { class: "compute__table" },
          el(
            "thead",
            {},
            el(
              "tr",
              {},
              el("th", {}, "Job"),
              el("th", {}, "Instance"),
              el("th", {}, "Type · region"),
              el("th", {}, "Running for"),
            ),
          ),
          el(
            "tbody",
            {},
            jobs.map((job) => jobRow(job, now)),
          ),
        )
      : withheldNotice(),
  );
}
