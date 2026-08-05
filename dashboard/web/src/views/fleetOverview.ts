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
import {
  UNKNOWN,
  formatDiskFree,
  formatDuration,
  formatPercent,
  formatRatio,
  formatRelative,
  formatAbsolute,
  formatCount,
  roleTickCompactText,
} from "../format";
import type { FleetView, HostStatus, HostView } from "../fleet";
import type { HostHealthRecord, ManagedRepoEntry } from "../types";
import { emptyFleetView } from "./states";

const STATUS_LABEL: Record<HostStatus, string> = {
  ok: "OK",
  degraded: "Degraded",
  stale: "Stale",
  unknown: "No data",
};

/**
 * Fallback tooltip text per status, used when a host has no more specific
 * reason to show — `"degraded"` almost always does (see `distressReason` /
 * the `"token pool..."` fallback in `fleet.ts`'s `buildHostView`), so its
 * entry here is only a defensive backstop, never the normal case.
 */
const STATUS_TITLE: Record<HostStatus, string> = {
  ok: "Reporting recently; token pool has healthy capacity",
  degraded: "Reporting recently, but showing signs of distress",
  stale: "No telemetry received recently — the daemon may be stopped or offline",
  unknown: "This host has not pushed host.health or tokens.snapshot yet",
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

function tokenSummaryText(host: HostView): string {
  const { total, exhausted, peakUsage } = host.tokens;
  if (total === 0) return UNKNOWN;
  const peak = peakUsage === undefined ? UNKNOWN : formatPercent(peakUsage);
  return `${exhausted}/${total} exhausted · peak ${peak}`;
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

export function hostCard(host: HostView, now: Date = new Date()): HTMLElement {
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
      statusBadge(host.status, host.degradedReason),
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
      field("Token pool", tokenSummaryText(host), "tokens.snapshot"),
      field(
        "Active sweeps",
        sweepCount === 0 ? "none" : String(sweepCount),
        "Sweeps currently in flight on this host",
      ),
      field(
        "Repositories",
        repoCount === 0 ? "none" : String(repoCount),
        "Repositories this host's daemon manages (its workspace registry, whether idle or busy)",
      ),
    ),
    sweepCount > 0
      ? el(
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
              el("span", { class: "chip" }, sweep.phase ?? "starting"),
              el(
                "span",
                { class: "card__sweep-label" },
                sweep.issue === undefined ? sweep.sweepId : `#${sweep.issue}`,
              ),
              sweep.repo ? el("span", { class: "card__sweep-repo" }, sweep.repo) : null,
            ),
          ),
        )
      : null,
    repoCount > 0
      ? el(
          "ul",
          { class: "card__repos", data: { testid: "card-repos" } },
          namedRepos
            .slice()
            .sort((a, b) => a.slug.localeCompare(b.slug))
            .map((repo) => {
              const count = sweepCounts.get(repo.slug) ?? 0;
              return el(
                "li",
                { class: "card__repo" },
                el("span", { class: "card__repo-label" }, repo.slug),
                count > 0 ? el("span", { class: "chip" }, `×${count}`) : null,
              );
            }),
          hiddenPrivateCount > 0
            ? el(
                "li",
                { class: "card__repo card__repo--private" },
                `+ ${hiddenPrivateCount} private`,
              )
            : null,
        )
      : null,
  );
}

export function fleetOverviewView(view: FleetView, now: Date = new Date()): HTMLElement {
  if (view.hosts.length === 0) return emptyFleetView();

  return el(
    "section",
    { class: "overview", data: { testid: "fleet-overview" } },
    el(
      "div",
      { class: "overview__summary", data: { testid: "fleet-summary" } },
      el(
        "span",
        {},
        `${view.reportingHosts} host${view.reportingHosts === 1 ? "" : "s"}`,
      ),
      el("span", {}, `${view.totalSweeps} active sweep${view.totalSweeps === 1 ? "" : "s"}`),
      el(
        "span",
        { class: view.needsAttention > 0 ? "overview__attention" : undefined },
        `${view.needsAttention} need${view.needsAttention === 1 ? "s" : ""} attention`,
      ),
    ),
    el(
      "div",
      { class: "overview__grid" },
      view.hosts.map((host) => hostCard(host, now)),
    ),
  );
}
