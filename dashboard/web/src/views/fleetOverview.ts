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
  formatDuration,
  formatGigabytes,
  formatPercent,
  formatRatio,
  formatRelative,
  formatAbsolute,
  formatCount,
} from "../format";
import type { FleetView, HostStatus, HostView } from "../fleet";
import { emptyFleetView } from "./states";

const STATUS_LABEL: Record<HostStatus, string> = {
  ok: "OK",
  degraded: "Degraded",
  stale: "Stale",
  unknown: "No data",
};

const STATUS_TITLE: Record<HostStatus, string> = {
  ok: "Reporting recently; token pool has capacity (some accounts may be exhausted — that's normal rotation)",
  degraded: "Reporting recently, but the token pool has no or almost no accounts available",
  stale: "No telemetry received recently — the daemon may be stopped or offline",
  unknown: "This host has not pushed host.health or tokens.snapshot yet",
};

export function statusBadge(status: HostStatus): HTMLElement {
  return el(
    "span",
    {
      class: `badge badge--${status}`,
      title: STATUS_TITLE[status],
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

/** The `host.health` field set, in the order the schema documents them. */
export function healthFields(host: HostView): DocumentFragment {
  const health = host.entry.health?.record ?? {};
  const fragment = document.createDocumentFragment();
  fragment.appendChild(field("Daemon", health.daemon_version ?? UNKNOWN));
  fragment.appendChild(
    field("Uptime", formatDuration(health.uptime_sec), "host.health.uptime_sec"),
  );
  fragment.appendChild(field("CPUs", formatCount(health.logical_cpus)));
  fragment.appendChild(field("CPU idle", formatPercent(health.cpu_idle_fraction)));
  fragment.appendChild(field("Load/core", formatRatio(health.load_per_core)));
  fragment.appendChild(field("Worktree free", formatGigabytes(health.worktree_root_free_gb)));
  return fragment;
}

export function hostCard(host: HostView, now: Date = new Date()): HTMLElement {
  const sweepCount = host.sweeps.length;
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
      statusBadge(host.status),
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
    el("dl", { class: "card__fields" }, healthFields(host)),
    el(
      "dl",
      { class: "card__fields card__fields--wide" },
      field("Token pool", tokenSummaryText(host), "tokens.snapshot"),
      field(
        "Active sweeps",
        sweepCount === 0 ? "none" : String(sweepCount),
        "Sweeps currently in flight on this host",
      ),
    ),
    sweepCount > 0
      ? el(
          "ul",
          { class: "card__sweeps", data: { testid: "card-sweeps" } },
          host.sweeps.slice(0, 3).map((sweep) =>
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
          sweepCount > 3
            ? el("li", { class: "card__sweep card__sweep--more" }, `+${sweepCount - 3} more`)
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
      el("span", {}, `${view.hosts.length} host${view.hosts.length === 1 ? "" : "s"}`),
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
