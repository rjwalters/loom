/**
 * The fleet overview — the landing view.
 *
 * One card per host, each showing the whole `host.health` field set plus a
 * `tokens.snapshot` summary "at a glance", and linking to that host's
 * drill-down (`#/hosts/<id>`). This is the server-aggregated replacement for
 * `loom-daemon serve`'s browser-side `--peers` fan-out: same "all hosts in one
 * pane" goal, one request, no per-host reachability requirement.
 *
 * Every value is rendered through `format.ts`, so an unmeasurable probe shows
 * `—` and never a fabricated zero.
 */

import { el, field } from "../dom";
import { isAuthenticatedViewer } from "../api";
import {
  UNKNOWN,
  formatDiskFree,
  formatDuration,
  formatPercent,
  formatRatio,
  formatRelative,
  formatAbsolute,
  formatCount,
  protectionBadgeStatus,
  protectionText,
  roleTickAggregateText,
  roleTickCompactText,
} from "../format";
import { forgeLink, repoUrl, sweepWorkTitle, sweepWorkUrl } from "../forgeLinks";
import { providerDisplayName, providerMark, sweepAgentMark } from "../providers";
import type { FleetView, HostStatus, HostView, ProviderSummary } from "../fleet";
import type { HostHealthRecord, HostProtection, ManagedRepoEntry } from "../types";
import { emptyFleetView } from "./states";
import { computeSubprocessList, runningComputeSection, type RunningComputeOptions } from "./runningCompute";

const STATUS_LABEL: Record<HostStatus, string> = {
  ok: "OK",
  degraded: "Degraded",
  throttled: "Throttled",
  stale: "Stale",
  unknown: "No data",
  missing: "Missing",
  unprovisioned: "Unprovisioned",
};

/**
 * Fallback tooltip text per status, used when a host has no more specific
 * reason to show — `"degraded"` almost always does (see `distressReason` /
 * the `"token pool..."` fallback in `fleet.ts`'s `buildHostView`), so its
 * entry here is only a defensive backstop, never the normal case.
 */
const STATUS_TITLE: Record<HostStatus, string> = {
  ok: "Reporting recently; token pool has healthy capacity",
  degraded: "Reporting recently, but something needs attention",
  throttled: "Healthy, but holding back new work by design (load shedding or a low token pool) — clears on its own",
  stale: "No telemetry received recently — the daemon may be stopped or offline",
  unknown: "This host has not pushed host.health or tokens.snapshot yet",
  missing:
    "Expected by the fleet roster and holding an active ingest key, but the backend has " +
    "no host.health record for it at all — something that should be reporting is not",
  unprovisioned:
    "Expected by the fleet roster, but no active ingest key exists for it — it has not " +
    "been enrolled yet, so it cannot report",
};

/**
 * Roster-expected hosts (#8792 backend, #8804 here) — hosts the operator's
 * `EXPECTED_HOSTS` roster names that have no telemetry at all.
 *
 * Rendered as their own card shape rather than an ordinary host card with a
 * different badge: every `host.health` field would be `—` by definition, so a
 * full card would be ten rows of nothing wrapped around the single fact that
 * matters. The card says what state the host is in, why, and what to do —
 * and `missing` and `unprovisioned` never share wording, because one is an
 * incident to investigate and the other is a provisioning step to take.
 */
const ROSTER_SUBTITLE: Record<"missing" | "unprovisioned", string> = {
  missing: "Never reported — expected by the roster, and enrolled",
  unprovisioned: "Never reported — expected by the roster, not enrolled yet",
};

const ROSTER_DETAIL: Record<"missing" | "unprovisioned", string> = {
  missing:
    "An active ingest key exists for this host, but the backend holds no host.health " +
    "record for it: its daemon may never have started, may predate telemetry export, or " +
    "may have been silent long enough for its last record to be pruned.",
  unprovisioned:
    "No active ingest key exists for this host (never enrolled, or its key was revoked), " +
    "so it cannot report yet. Provision one to bring it online — deploy runbook §8.",
};

/**
 * `reason` — from `HostView.degradedReason` — names the specific cause
 * (`"dispatch halted: host-distress breaker …"`, `"token pool at or near
 * exhaustion"`, …) rather than a generic "Degraded" (#4975). Every other
 * status keeps its fixed `STATUS_TITLE` line.
 */
export function statusBadge(status: HostStatus, reason?: string): HTMLElement {
  return el(
    "span",
    {
      class: `badge badge--${status}`,
      title: reason ?? STATUS_TITLE[status],
      data: { testid: "status-badge", status },
    },
    STATUS_LABEL[status],
  );
}

/**
 * A dedicated warning indicator for `host.health.protection` (#5352) —
 * distinct from `statusBadge`, which answers "is this host healthy right
 * now" from telemetry freshness/distress signals. An unprotected host can
 * otherwise look perfectly healthy on every other signal (reporting on time,
 * no distress) while having zero crash-detection coverage; this badge is
 * what makes that gap visible without an operator ssh-ing in and running
 * `loom-daemon status`.
 *
 * Renders only for the `"unprotected"` verdict: `"protected"` is the
 * routine, unremarkable common case (no badge needed, the same restraint
 * `statusBadge`'s own "busy is not degraded" callers exercise), and
 * `"unknown"` (an absent field, a pre-#5352 daemon, or a probe that could not
 * answer) must never render as an alarm — see `protectionBadgeStatus`'s doc.
 * The host-detail drill-down's health panel always shows the full
 * `protectionText` sentence for every state, including `"unknown"`/absent,
 * regardless of whether this badge renders.
 */
export function protectionBadge(protection: HostProtection | undefined): HTMLElement | null {
  if (protectionBadgeStatus(protection) !== "unprotected") return null;
  return el(
    "span",
    {
      class: "badge badge--degraded",
      title: protectionText(protection),
      data: { testid: "protection-badge" },
    },
    "Unprotected",
  );
}

/**
 * A dedicated warning for a singleton job reported armed on a host that is
 * NOT the fleet captain (#8848 acceptance criterion (a)) — see
 * `singletonsArmedOnNonCaptain`'s doc for why this should be rare. Renders
 * only when `host.armedSingletonsOnNonCaptain` is nonempty; an ordinary host
 * (nothing armed, or armed exactly on the captain) shows no badge at all,
 * the same restraint `protectionBadge` applies to its own routine case.
 */
export function captainAnomalyBadge(host: HostView): HTMLElement | null {
  if (host.armedSingletonsOnNonCaptain.length === 0) return null;
  return el(
    "span",
    {
      class: "badge badge--degraded",
      title: `Singleton job(s) armed on a non-captain host (#8848): ${host.armedSingletonsOnNonCaptain.join(", ")}`,
      data: { testid: "captain-anomaly-badge" },
    },
    "Singleton on non-captain",
  );
}

/** One provider pool's summary line: `"17/21 exhausted · peak 100%"`, or
 * just `"1/3 exhausted"` for a pool whose accounts report no usage fraction
 * (Codex) — omitted rather than shown as a fake `peak 0%`. */
export function providerPoolText(slice: ProviderSummary): string {
  const base = `${slice.exhausted}/${slice.total} exhausted`;
  return slice.peakUsage === undefined ? base : `${base} · peak ${formatPercent(slice.peakUsage)}`;
}

/**
 * The token-pool section, one `<dt>/<dd>` pair per provider (`Claude`,
 * `Codex`, …) so each provider's availability reads independently — a
 * blended "17/21 exhausted" cannot tell "Claude is spent, Codex is fine"
 * from the reverse, and only one of those stalls the sweeps. The label
 * carries the provider's mark (`providers.ts`). A host that has reported no
 * pool at all keeps the single unknown "Token pool" row it always had.
 */
function tokenPoolFields(host: HostView): DocumentFragment {
  const fragment = document.createDocumentFragment();
  if (host.tokens.providers.length === 0) {
    fragment.appendChild(field("Token pool", UNKNOWN, "tokens.snapshot"));
    return fragment;
  }
  for (const slice of host.tokens.providers) {
    fragment.appendChild(
      el(
        "dt",
        { class: "field__label field__label--provider", data: { testid: "token-pool-label", provider: slice.provider } },
        providerMark(slice.provider, true),
        el("span", { class: "field__label-text" }, "pool"),
      ),
    );
    fragment.appendChild(
      el(
        "dd",
        {
          class: "field__value",
          title: `tokens.snapshot — ${providerDisplayName(slice.provider)} accounts on this host`,
          data: { testid: "token-pool-value", provider: slice.provider },
        },
        providerPoolText(slice),
      ),
    );
  }
  return fragment;
}

/**
 * The emitting binary's identity, as one line: `"0.17.0 @ 8c16fb5b, built 6h
 * ago"` (#4956).
 *
 * `daemon_version` alone cannot answer "is this host's daemon current?" — it
 * only moves once per release, so every build between two releases reports the
 * same string and a day-stale binary reads identically to `main`. The commit
 * is the precise identity; the build age is what makes staleness obvious at a
 * glance.
 *
 * Each part degrades independently: a record from a pre-#4956 daemon (no
 * commit, no build time) renders exactly as it did before, and an `"unknown"`
 * commit sentinel (a build host with no git) is dropped rather than shown as a
 * fake SHA.
 */
export function daemonIdentityText(health: HostHealthRecord, now: Date = new Date()): string {
  const version = health.daemon_version ?? UNKNOWN;
  const commit = health.build_commit;
  const identity = commit && commit !== "unknown" ? `${version} @ ${commit}` : version;
  const age = formatRelative(health.built_at, now);
  // An absent/unparseable build stamp shows no age clause at all — "built —"
  // would be noise, not information.
  return health.built_at && age !== UNKNOWN ? `${identity}, built ${age}` : identity;
}

/** The `host.health` field set, in the order the schema documents them. */
export function healthFields(host: HostView, now: Date = new Date()): DocumentFragment {
  const health = host.entry.health?.record ?? {};
  const fragment = document.createDocumentFragment();
  fragment.appendChild(
    field(
      "Daemon",
      daemonIdentityText(health, now),
      health.built_at ? `Built ${formatAbsolute(health.built_at)}` : undefined,
    ),
  );
  fragment.appendChild(
    field("Uptime", formatDuration(health.uptime_sec), "host.health.uptime_sec"),
  );
  fragment.appendChild(field("CPUs", formatCount(health.logical_cpus)));
  fragment.appendChild(field("CPU idle", formatPercent(health.cpu_idle_fraction)));
  fragment.appendChild(field("Load/core", formatRatio(health.load_per_core)));
  fragment.appendChild(
    field("Worktree free", formatDiskFree(health.worktree_root_free_gb, health.worktree_root_total_gb)),
  );
  fragment.appendChild(
    field(
      "Roles",
      roleTickCompactText(health.roles),
      "Role-tick health — see the host drill-down for which role(s) are persistently failing (#5022)",
    ),
  );
  fragment.appendChild(
    field(
      "Protection",
      protectionText(health.protection),
      "Watchdog/crash-protection state — whether a future daemon death on this host would be detected (#5352)",
    ),
  );
  // #8848: only shown once this fleet has actually opted into `fleet.captain`
  // — `is_captain === undefined` means the mechanism does not apply here at
  // all, and a "Captain: —" row on every ordinary card would be noise, not
  // information (mirrors `tokenPoolFields`'s restraint for a provider-less
  // host, and `protectionBadge`'s restraint for the routine "protected"
  // case).
  if (health.is_captain !== undefined) {
    fragment.appendChild(
      field(
        "Captain",
        health.is_captain ? "Yes" : "No",
        "Fleet singleton-job captain (#8848) — the one host declared to run this fleet's declared singleton jobs",
      ),
    );
  }
  return fragment;
}

/** This host's managed-repository roster (#4976), or `[]` when the host has
 * not reported one yet (a pre-#4976 daemon, or no registered workspaces). */
function managedRepos(host: HostView): ManagedRepoEntry[] {
  return host.entry.health?.record.managed_repos ?? [];
}

/** How many of `host.sweeps` are in flight against each named repo — the
 * card's existing sweep list already carries a repo slug per entry (#4868),
 * so the roster section's per-repo counts are grouped from it rather than
 * plumbing a second, redundant count through `host.health`. */
function sweepCountsByRepo(host: HostView): Map<string, number> {
  const counts = new Map<string, number>();
  for (const sweep of host.sweeps) {
    if (!sweep.repo) continue;
    counts.set(sweep.repo, (counts.get(sweep.repo) ?? 0) + 1);
  }
  return counts;
}

/** The idle-roster `<summary>` text (#7662): the count of named idle repos,
 * plus a trailing `", N private"` clause when the redacted roster also hides
 * some. Kept as one string builder so the "N idle repositories" wording used
 * by both the collapsed-details summary and the zero-sweep edge case (where
 * it stands in for the whole roster) can't drift apart. */
function idleReposSummaryText(idleNamedCount: number, hiddenPrivateCount: number): string {
  const base = `${idleNamedCount} idle repositor${idleNamedCount === 1 ? "y" : "ies"}`;
  return hiddenPrivateCount > 0 ? `${base}, ${hiddenPrivateCount} private` : base;
}

/** localStorage key for a host card's idle-roster open/closed state (#7662).
 * Storing per host id, not globally, so expanding one busy host's idle list
 * does not also expand every other card's. */
function idleReposOpenStorageKey(hostId: string): string {
  return `loom-dashboard:idle-repos-open:${hostId}`;
}

/** Reads the persisted open/closed state for a host card's idle-roster
 * `<details>` (#7662) — defaults closed (`false`) whenever storage is
 * unavailable (private browsing, a pre-render/test environment with no
 * `window`) or has never recorded a preference for this host. */
function readIdleReposOpen(hostId: string): boolean {
  try {
    return window.localStorage.getItem(idleReposOpenStorageKey(hostId)) === "1";
  } catch {
    return false;
  }
}

/** Persists a host card's idle-roster open/closed state (#7662). Best-effort
 * only — a `localStorage` write can fail (quota, private browsing, a
 * disabled storage policy) without it being this feature's job to surface
 * that failure; the state just won't survive the next reload. */
function writeIdleReposOpen(hostId: string, open: boolean): void {
  try {
    window.localStorage.setItem(idleReposOpenStorageKey(hostId), open ? "1" : "0");
  } catch {
    // Storage unavailable — nothing to do; the toggle itself still worked.
  }
}

/** This host's in-flight sweeps as a list, or `null` when it has none.
 * Extracted so the roster-missing card (#8804) can show them too: a host that
 * never sent `host.health` can still have pushed `sweep.started` records, and
 * dropping that list would hide live work. */
function sweepList(host: HostView, now: Date = new Date()): HTMLElement | null {
  if (host.sweeps.length === 0) return null;
  return el(
    "ul",
    { class: "card__sweeps", data: { testid: "card-sweeps" } },
    // Every in-flight sweep, not the first three (#4868). This card is
    // the answer to "what is the fleet doing right now"; truncating to
    // three made that answer "click into each host" — on a working
    // fleet it hid 22 of 31 sweeps. The card grows instead.
    host.sweeps.map((sweep) =>
      el(
        "li",
        { class: "card__sweep" },
        // Resolved provider first; admitted runtime remains an honest
        // fallback when older daemons have no launch attribution.
        sweepAgentMark(sweep),
        sweep.model ? el("span", { class: "chip", title: "Sweep launch model" }, sweep.model) : null,
        el("span", { class: "chip" }, sweep.phase ?? "starting"),
        // `#N` links to the sweep's work on the forge: the
        // `feature/issue-N` branch once Builder has pushed one, the
        // issue itself before that (Curator has no branch yet).
        forgeLink(
          sweep.issue === undefined ? sweep.sweepId : `#${sweep.issue}`,
          sweepWorkUrl(sweep.repo, sweep.issue, sweep.phase),
          "card__sweep-label",
          sweepWorkTitle(sweep.issue, sweep.phase),
        ),
        sweep.repo ? forgeLink(sweep.repo, repoUrl(sweep.repo), "card__sweep-repo") : null,
        // Issue #8835: the sweep's live Spot/batch jobs, nested beneath it.
        // `null` for the overwhelming majority of sweeps (no compute jobs, or
        // an emitter that does not stamp `sweepId`), so an ordinary sweep's
        // markup is byte-identical to what it was before this feature.
        computeSubprocessList(host.computeBySweep.get(sweep.sweepId) ?? [], now),
      ),
    ),
  );
}

/**
 * The card for a roster-expected host with no telemetry (#8804) — see
 * `ROSTER_SUBTITLE`/`ROSTER_DETAIL` for why this is its own shape rather than
 * an ordinary card with every field blank.
 *
 * `data-roster-state` (and the `card--missing`/`card--unprovisioned` class
 * the shared `card--${status}` template already yields) is what makes the two
 * states distinguishable from each other, and both from a host that reported
 * and went quiet (`card--stale`).
 */
function rosterHostCard(
  host: HostView,
  state: "missing" | "unprovisioned",
  now: Date = new Date(),
): HTMLElement {
  return el(
    "article",
    {
      class: `card card--${state}`,
      data: { testid: "host-card", host: host.hostId, "roster-state": state },
    },
    el(
      "header",
      { class: "card__header" },
      el("a", { class: "card__title", href: `#/hosts/${encodeURIComponent(host.hostId)}` }, host.hostId),
      el("div", { class: "card__badges" }, statusBadge(host.status)),
    ),
    el("p", { class: "card__subtitle" }, ROSTER_SUBTITLE[state]),
    el("p", { class: "card__notice", data: { testid: "roster-state-detail" } }, ROSTER_DETAIL[state]),
    sweepList(host, now),
  );
}

export function hostCard(host: HostView, now: Date = new Date()): HTMLElement {
  if (host.status === "missing" || host.status === "unprovisioned") {
    return rosterHostCard(host, host.status, now);
  }
  const sweepCount = host.sweeps.length;
  const repos = managedRepos(host);
  const repoCount = repos.length;
  const sweepCounts = sweepCountsByRepo(host);
  // A repo whose `slug` was stripped by the public-view redaction (a private
  // repo, unauthenticated viewer — `dashboard/src/redaction.ts`) collapses
  // into one trailing "+N private" row rather than N rows that each say
  // nothing but "private" (Issue #4976's anti-leak contract).
  const namedRepos = repos.filter((repo): repo is ManagedRepoEntry & { slug: string } => Boolean(repo.slug));
  const hiddenPrivateCount = repoCount - namedRepos.length;
  // Split the roster into the handful the card exists to answer ("what is
  // this host doing") and the rest, folded into a closed-by-default
  // <details> so ~45 idle rows don't bury a couple of busy ones (#7662).
  // Active: in-flight count desc, then slug — busiest first. Idle: the same
  // alphabetical order the flat list used before the split.
  const activeRepos = namedRepos
    .filter((repo) => (sweepCounts.get(repo.slug) ?? 0) > 0)
    .sort((a, b) => {
      const byCount = (sweepCounts.get(b.slug) ?? 0) - (sweepCounts.get(a.slug) ?? 0);
      return byCount !== 0 ? byCount : a.slug.localeCompare(b.slug);
    });
  const idleRepos = namedRepos
    .filter((repo) => (sweepCounts.get(repo.slug) ?? 0) === 0)
    .sort((a, b) => a.slug.localeCompare(b.slug));

  // The idle roster: rendered only when there is something to fold away —
  // when every named repo is active (and nothing is redacted), no <details>
  // exists at all. Built as a standalone element (rather than inline in the
  // `el(...)` tree below) because the open/closed persistence (#7662's "ask"
  // item 4) needs to read/write it after construction.
  const idleCount = idleRepos.length + hiddenPrivateCount;
  let idleDetails: HTMLDetailsElement | null = null;
  if (idleCount > 0) {
    idleDetails = el(
      "details",
      { class: "card__repos-idle", data: { testid: "card-repos-idle" } },
      el("summary", {}, idleReposSummaryText(idleRepos.length, hiddenPrivateCount)),
      el(
        "ul",
        { class: "card__repos" },
        idleRepos.map((repo) =>
          el("li", { class: "card__repo" }, forgeLink(repo.slug, repoUrl(repo.slug), "card__repo-label")),
        ),
        hiddenPrivateCount > 0
          ? el("li", { class: "card__repo card__repo--private" }, `+ ${hiddenPrivateCount} private`)
          : null,
      ),
    );
    idleDetails.open = readIdleReposOpen(host.hostId);
    idleDetails.addEventListener("toggle", () => {
      writeIdleReposOpen(host.hostId, idleDetails!.open);
    });
  }

  return el(
    "article",
    { class: `card card--${host.status}`, data: { testid: "host-card", host: host.hostId } },
    el(
      "header",
      { class: "card__header" },
      el(
        "a",
        { class: "card__title", href: `#/hosts/${encodeURIComponent(host.hostId)}` },
        host.hostId,
      ),
      el(
        "div",
        { class: "card__badges" },
        statusBadge(host.status, host.degradedReason),
        protectionBadge(host.entry.health?.record.protection),
        captainAnomalyBadge(host),
      ),
    ),
    el(
      "p",
      { class: "card__subtitle" },
      el(
        "span",
        { title: host.lastReportAt ? formatAbsolute(host.lastReportAt) : undefined },
        host.lastReportAt ? `Last report ${formatRelative(host.lastReportAt, now)}` : "Never reported",
      ),
    ),
    el("dl", { class: "card__fields" }, healthFields(host, now)),
    el(
      "dl",
      { class: "card__fields card__fields--wide" },
      tokenPoolFields(host),
      field(
        "Active sweeps",
        // "none" alone reads as "this host is idle" — but role ticks
        // (Curator/Champion/Judge/Doctor role-runner invocations, see the
        // "Roles" field above) never post here, so a host can be genuinely
        // busy and still show zero (#5642).
        sweepCount === 0 ? "none (excludes role ticks)" : String(sweepCount),
        "Sweeps currently in flight on this host — role ticks are tracked separately, see Roles above",
      ),
      field(
        "Repositories",
        repoCount === 0 ? "none" : String(repoCount),
        "Repositories this host's daemon manages (its workspace registry, whether idle or busy)",
      ),
    ),
    sweepList(host, now),
    activeRepos.length > 0
      ? el(
          "ul",
          { class: "card__repos", data: { testid: "card-repos" } },
          activeRepos.map((repo) => {
            const count = sweepCounts.get(repo.slug) ?? 0;
            return el(
              "li",
              { class: "card__repo" },
              forgeLink(repo.slug, repoUrl(repo.slug), "card__repo-label"),
              el("span", { class: "chip" }, `×${count}`),
            );
          }),
        )
      : null,
    idleDetails,
  );
}

const HOST_COUNT_TITLE =
  "Every host in the fleet: those pushing telemetry, plus any named by the expected-host " +
  "roster that have never reported — “missing” (enrolled but silent) and “unprovisioned” " +
  "(not enrolled yet)";

/**
 * The headline host count (#8804). Roster hosts that never reported are
 * included in the total — a fleet of five where one is silent has five hosts,
 * and a page that says "4 hosts" is exactly the silent-host blind spot the
 * roster exists to close — but the breakdown keeps "reporting" separate, so
 * the total can never be misread as "N hosts are pushing telemetry".
 *
 * With no roster hosts (a pre-#8792 backend, no `EXPECTED_HOSTS` configured,
 * or a roster with nothing missing) the text is byte-identical to what this
 * headline rendered before: `"3 hosts"`.
 */
export function hostCountText(view: FleetView): string {
  const rosterTotal = view.missingHosts + view.unprovisionedHosts;
  const total = view.reportingHosts + rosterTotal;
  const base = `${total} host${total === 1 ? "" : "s"}`;
  if (rosterTotal === 0) return base;
  const parts = [`${view.reportingHosts} reporting`];
  if (view.missingHosts > 0) parts.push(`${view.missingHosts} missing`);
  if (view.unprovisionedHosts > 0) parts.push(`${view.unprovisionedHosts} unprovisioned`);
  return `${base} (${parts.join(", ")})`;
}

export function fleetOverviewView(
  view: FleetView,
  now: Date = new Date(),
  options: RunningComputeOptions = {},
): HTMLElement {
  const rosterTotal = view.missingHosts + view.unprovisionedHosts;
  // Resolved once, then passed down, so the panel and the headline count below
  // can never disagree about who the viewer is.
  const authenticated = options.authenticated ?? isAuthenticatedViewer();
  // Rendered before the host-count check on purpose: a hostless elastic
  // emitter (`defaults/docs/observability.md` §5d) pushes `ephemeral_compute`
  // and nothing else, so a fleet can legitimately have running instances and
  // zero reporting hosts. Short-circuiting to the "no hosts" empty state would
  // hide the one thing that *is* running — including a leak (#8306).
  //
  // Fed `unattributedCompute`, not `activeCompute` (#8835): a job already
  // nested under its own sweep's card entry does not also need a row here, but
  // every job that could NOT be nested does — that is precisely the orphaned/
  // leaked instance this panel exists to surface. The headline count below
  // deliberately still uses `activeCompute` (the fleet-wide total), so nesting
  // never makes the fleet look like it is running less compute than it is.
  const compute = runningComputeSection(view.unattributedCompute, now, { authenticated });

  if (view.hosts.length === 0) {
    return compute
      ? el("div", { class: "overview overview--hostless" }, compute, emptyFleetView())
      : emptyFleetView();
  }

  return el(
    "section",
    { class: "overview", data: { testid: "fleet-overview" } },
    el(
      "div",
      { class: "overview__summary", data: { testid: "fleet-summary" } },
      el(
        "span",
        {
          title: rosterTotal > 0 ? HOST_COUNT_TITLE : undefined,
          data: { testid: "fleet-host-summary" },
        },
        hostCountText(view),
      ),
      el(
        "span",
        {
          title:
            "Sweep dispatches only — role ticks (Curator/Champion/Judge/Doctor role-runner " +
            "invocations) are not sweeps and are never counted here, even when they are why " +
            "this number is 0 (#5642)",
        },
        // "0 active sweeps" alone reads as "the fleet is idle" — but role
        // ticks never post to activeSweeps, so a fleet doing nothing but
        // role work legitimately shows 0 here. The trailing clause makes
        // that explicit, and the role-tick count (when any host has
        // reported one) gives the other half of the answer right next to
        // it, rather than requiring a drill-down into every host card.
        `${view.totalSweeps} active sweep${view.totalSweeps === 1 ? "" : "s"} (excludes role ticks)` +
          (view.roleTicks ? ` · ${roleTickAggregateText(view.roleTicks)}` : ""),
      ),
      el(
        "span",
        { class: view.needsAttention > 0 ? "overview__attention" : undefined },
        `${view.needsAttention} need${view.needsAttention === 1 ? "s" : ""} attention`,
      ),
      // #8848 acceptance criterion (b): this fleet has opted into
      // `fleet.captain` (some host reports `is_captain` at all) but none of
      // them is currently `true` — a typo'd/decommissioned captain id, or one
      // that has simply never reported `host.health`. Suppressed entirely for
      // the overwhelmingly common fleet that never declared `fleet.captain`,
      // since no host sends the field then (`noCaptainReporting` is
      // participation-gated — see its doc).
      view.noCaptainReporting
        ? el(
            "span",
            {
              class: "overview__attention",
              title:
                "No host currently reports is_captain: true, but this fleet declares fleet.captain — " +
                "check for a typo'd host id or a captain that has not reported host.health (#8848)",
              data: { testid: "fleet-captain-warning" },
            },
            "No fleet captain reporting",
          )
        : null,
      // Only when there is elastic compute to count (#8306). Most fleets run
      // none at all, and a permanent "0 compute jobs" in a headline already
      // carrying three carefully-worded counts is noise, not information —
      // the `#/spend` route is where "is anything running / what did it cost"
      // always has an answer, including zero. Also suppressed for a viewer
      // not entitled to the count: `/public/fleet-state` withholds it, so a
      // rendered `0` would not be a fact about the fleet.
      authenticated && view.activeCompute.length > 0
        ? el(
            "span",
            {
              class: view.leakedCompute > 0 ? "overview__attention" : undefined,
              title:
                "Ephemeral compute instances currently running (cloud batch jobs reported by a " +
                "non-daemon emitter) — see the panel below",
              data: { testid: "fleet-compute-summary" },
            },
            `${view.activeCompute.length} compute job${view.activeCompute.length === 1 ? "" : "s"}` +
              (view.leakedCompute > 0 ? ` · ${view.leakedCompute} possibly leaked` : ""),
          )
        : null,
    ),
    compute,
    el(
      "div",
      { class: "overview__grid" },
      view.hosts.map((host) => hostCard(host, now)),
    ),
  );
}
