/**
 * Work-queue rendering (Issue #8852, phase 3): what each host's work finder
 * has queued, running and blocked, and why.
 *
 * Three surfaces, one model (`../workQueue.ts`):
 *
 *  - [`workQueueSummarySection`] — on the fleet overview: fleet totals, one
 *    row per host with its counts and freshness, and a link to the full view.
 *  - [`workQueueView`] — the `#/queue` route: every issue across the fleet,
 *    folded by `repo#issue`, in Running / Ready / Blocked lists with issue and
 *    PR links, the assigned host, phase, waiting time and blocking reason.
 *  - [`hostQueuePanel`] — on host detail: that host's own ranked queue, in
 *    the daemon's dispatch order.
 *
 * Every surface states *how current* its numbers are. A host whose queue is
 * stale shows its last-known counts dimmed and badged, never as current, and
 * a host that never sent a queue says so rather than showing zeros.
 */

import { el } from "../dom";
import type { FleetView, HostView } from "../fleet";
import { forgeLink, issueUrl, pullUrl, repoUrl } from "../forgeLinks";
import { UNKNOWN, formatAbsolute, formatCount, formatDuration, formatRelative, secondsSince } from "../format";
import type { QueueRow } from "../queueTypes";
import type { ActiveSweep } from "../types";
import {
  fleetQueueTotals,
  isCurrent,
  mergeFleetQueue,
  QUEUE_STALE_AFTER_SEC,
  openPrNumber,
  rankText,
  reasonText,
  summarizeHostQueue,
  type FleetQueueItem,
  type HostQueueSummary,
  type QueueHealth,
} from "../workQueue";

const HEALTH_LABEL: Readonly<Record<QueueHealth, string>> = {
  absent: "no queue data",
  idle: "idle",
  active: "live",
  stale: "stale",
  offline: "offline",
};

const HEALTH_BADGE: Readonly<Record<QueueHealth, string>> = {
  absent: "badge--unknown",
  idle: "badge--ok",
  active: "badge--ok",
  stale: "badge--stale",
  offline: "badge--stale",
};

/** One sentence saying how far the counts can be trusted. */
export function queueFreshnessText(summary: HostQueueSummary, now: Date = new Date()): string {
  const queue = summary.queue;
  if (!queue) {
    return "No queue telemetry from this host — its daemon predates queue export, or its work finder is off.";
  }
  const tick = `last work-finder tick ${formatRelative(queue.record.tick_at, now)}`;
  const received = `received ${formatRelative(queue.updatedAt, now)}`;
  let text: string;
  switch (summary.health) {
    case "idle":
      text = `Idle: the queue was empty at the ${tick} (${received}).`;
      break;
    case "active":
      text = `Live: ${tick} (${received}).`;
      break;
    default:
      text =
        `Stale: no new tick received for ${formatDuration(summary.ageSec)}; ${tick}. ` +
        "The work finder or the host's telemetry has stopped — these counts are last-known, not current.";
  }
  if (summary.incomplete) {
    const failed = queue.record.listing_failed.length + queue.record.listing_failed_unresolved;
    text += ` Incomplete: ${failed} repo${failed === 1 ? "" : "s"} could not be listed on that tick.`;
  }
  return text;
}

export function queueHealthBadge(summary: HostQueueSummary, now: Date = new Date()): HTMLElement {
  return el(
    "span",
    {
      class: `badge ${HEALTH_BADGE[summary.health]}`,
      title: queueFreshnessText(summary, now),
      data: { testid: "queue-health", health: summary.health },
    },
    HEALTH_LABEL[summary.health],
  );
}

function issueCell(repo: string | undefined, issue: number | undefined, visibility: string): HTMLElement {
  if (issue === undefined) {
    return el("span", { class: "queue__withheld", title: "Private repository — sign in to see it" }, "private issue");
  }
  return forgeLink(`#${issue}`, issueUrl(repo, issue), "queue__issue", visibility === "private" ? "Private repository" : undefined);
}

function repoCell(repo: string | undefined): HTMLElement {
  return repo === undefined ? el("span", { class: "queue__withheld" }, UNKNOWN) : forgeLink(repo, repoUrl(repo), "queue__repo");
}

function reasonCell(row: QueueRow, repo: string | undefined): HTMLElement {
  const pr = openPrNumber(row);
  return el(
    "span",
    { class: "queue__reason", title: `disposition: ${row.disposition}` },
    reasonText(row),
    pr !== undefined && pullUrl(repo, pr) !== undefined ? " " : null,
    pr !== undefined && pullUrl(repo, pr) !== undefined ? forgeLink("PR", pullUrl(repo, pr), "queue__pr") : null,
  );
}

/** Time since the issue was created. The daemon does not report when an
 * issue became ready, so this is the oldest honest bound on time in queue. */
function waitingCell(row: QueueRow, now: Date): HTMLElement {
  const seconds = secondsSince(row.created_at, now);
  return el(
    "span",
    {
      class: "queue__waiting",
      title: row.created_at ? `Issue created ${formatAbsolute(row.created_at)}` : "Creation time not reported",
    },
    seconds === undefined ? UNKNOWN : formatDuration(Math.max(0, seconds)),
  );
}

function flags(row: QueueRow): HTMLElement | null {
  const parts: HTMLElement[] = [];
  if (row.urgent) parts.push(el("span", { class: "badge badge--urgent", title: "loom:urgent" }, "urgent"));
  if (row.tier) {
    parts.push(el("span", { class: "queue__tier", title: "Informational: the daemon does not order by tier" }, row.tier));
  }
  return parts.length > 0 ? el("span", { class: "queue__flags" }, parts) : null;
}

function hostLink(hostId: string): HTMLElement {
  return el("a", { class: "link", href: `#/hosts/${encodeURIComponent(hostId)}` }, hostId);
}

function phaseText(sweep: ActiveSweep | undefined, now: Date): string {
  if (!sweep) return UNKNOWN;
  const running = sweep.startedAt ? `, running ${formatDuration(Math.max(0, secondsSince(sweep.startedAt, now) ?? 0))}` : "";
  return `${sweep.phase ?? "starting"}${running}`;
}

// ---------------------------------------------------------------------------
// Fleet overview summary
// ---------------------------------------------------------------------------

function countCell(value: number, current: boolean): HTMLElement {
  return el("td", { class: current ? "queue__count" : "queue__count queue__count--stale" }, formatCount(value));
}

function summaryRow(summary: HostQueueSummary, now: Date): HTMLElement {
  const record = summary.queue?.record;
  const current = isCurrent(summary.health);
  return el(
    "tr",
    { data: { testid: "queue-host-row", host: summary.hostId, health: summary.health } },
    el("td", {}, hostLink(summary.hostId)),
    record ? countCell(record.seen, current) : el("td", {}, UNKNOWN),
    record ? countCell(record.counts.running, current) : el("td", {}, UNKNOWN),
    record ? countCell(record.counts.ready, current) : el("td", {}, UNKNOWN),
    record ? countCell(record.counts.blocked, current) : el("td", {}, UNKNOWN),
    el(
      "td",
      { title: record ? formatAbsolute(record.tick_at) : undefined },
      record ? formatRelative(record.tick_at, now) : UNKNOWN,
    ),
    el(
      "td",
      {},
      queueHealthBadge(summary, now),
      summary.incomplete ? el("span", { class: "badge badge--degraded", title: queueFreshnessText(summary, now) }, "incomplete") : null,
    ),
  );
}

function totalsText(view: FleetView, summaries: HostQueueSummary[]): string {
  const totals = fleetQueueTotals(summaries);
  const base =
    `Backlog ${totals.backlog} · running ${totals.running} · ready ${totals.ready} · blocked ${totals.blocked}` +
    ` across ${totals.currentHosts} host${totals.currentHosts === 1 ? "" : "s"}`;
  const notes: string[] = [];
  if (totals.staleHosts > 0) notes.push(`${totals.staleHosts} stale host${totals.staleHosts === 1 ? "" : "s"} excluded`);
  const silent = summaries.filter((s) => s.health === "absent").length;
  if (silent > 0) notes.push(`${silent} of ${view.hosts.length} not reporting a queue`);
  return notes.length > 0 ? `${base} (${notes.join("; ")})` : base;
}

/** The overview section, or `null` when no host has ever sent a queue — a
 * fleet on daemons that predate queue export gets nothing added. */
export function workQueueSummarySection(view: FleetView, now: Date = new Date()): HTMLElement | null {
  const summaries = view.hosts.map((host) => summarizeHostQueue(host, now));
  if (!summaries.some((summary) => summary.queue)) return null;
  return el(
    "section",
    { class: "queue", data: { testid: "work-queue-summary" } },
    el(
      "header",
      { class: "queue__header" },
      el("h2", { class: "queue__title" }, "Work queue"),
      el("span", { class: "queue__totals", data: { testid: "work-queue-totals" } }, totalsText(view, summaries)),
      el("a", { class: "link queue__more", href: "#/queue" }, "All queued, running and blocked issues →"),
    ),
    el(
      "table",
      { class: "queue__table" },
      el(
        "thead",
        {},
        el("tr", {}, ["Host", "Backlog", "Running", "Ready", "Blocked", "Last tick", "Freshness"].map((h) => el("th", {}, h))),
      ),
      el("tbody", {}, summaries.map((summary) => summaryRow(summary, now))),
    ),
  );
}

// ---------------------------------------------------------------------------
// `#/queue` — the fleet-wide lists
// ---------------------------------------------------------------------------

function otherHostsText(item: FleetQueueItem): string | undefined {
  if (item.others.length === 0) return undefined;
  return item.others.map((other) => `${other.hostId}: ${reasonText(other.row)}`).join("\n");
}

function itemRow(item: FleetQueueItem, now: Date): HTMLElement {
  const row = item.primary.row;
  const others = otherHostsText(item);
  return el(
    "tr",
    {
      class: `queue-row queue-row--${item.state}`,
      data: { testid: "queue-item", state: item.state, issue: item.issue, host: item.primary.hostId },
    },
    el("td", {}, issueCell(item.repo, item.issue, item.visibility), flags(row)),
    el("td", {}, repoCell(item.repo)),
    el(
      "td",
      { title: others ? `Also listed by:\n${others}` : undefined },
      hostLink(item.primary.hostId),
      (item.primary.observedAgeSec ?? 0) > QUEUE_STALE_AFTER_SEC
        ? el("span", { class: "badge badge--stale", title: "This host's queue is stale — last-known state" }, "stale")
        : null,
      item.others.length > 0 ? el("span", { class: "queue__also" }, ` +${item.others.length}`) : null,
    ),
    el("td", {}, item.state === "running" ? phaseText(item.sweep, now) : UNKNOWN),
    el("td", {}, waitingCell(row, now)),
    el("td", {}, reasonCell(row, item.repo)),
  );
}

const LIST_TITLES: Readonly<Record<"running" | "ready" | "blocked", string>> = {
  running: "Running",
  ready: "Ready (waiting on capacity)",
  blocked: "Blocked",
};

function itemList(state: "running" | "ready" | "blocked", items: FleetQueueItem[], now: Date): HTMLElement {
  const matching = items.filter((item) => item.state === state || (state === "blocked" && item.state === "unknown"));
  return el(
    "section",
    { class: "queue", data: { testid: `queue-list-${state}` } },
    el(
      "header",
      { class: "queue__header" },
      el("h2", { class: "queue__title" }, `${LIST_TITLES[state]} · ${matching.length}`),
    ),
    matching.length === 0
      ? el("p", { class: "queue__note" }, `Nothing ${state} on any reporting host.`)
      : el(
          "table",
          { class: "queue__table" },
          el(
            "thead",
            {},
            el("tr", {}, ["Issue", "Repository", "Host", "Phase", "Waiting", "Reason"].map((h) => el("th", {}, h))),
          ),
          el("tbody", {}, matching.map((item) => itemRow(item, now))),
        ),
  );
}

/** The `#/queue` route. */
export function workQueueView(view: FleetView, now: Date = new Date()): HTMLElement {
  const summaries = view.hosts.map((host) => summarizeHostQueue(host, now));
  const sweeps = view.hosts.flatMap((host) => host.sweeps);
  const items = mergeFleetQueue(summaries, sweeps);
  const withheld = summaries.reduce((sum, s) => sum + (s.queue?.record.withheld_rows ?? 0), 0);
  return el(
    "section",
    { class: "queue-page", data: { testid: "work-queue" } },
    el("nav", { class: "detail__breadcrumb" }, el("a", { class: "link", href: "#/" }, "← All hosts")),
    workQueueSummarySection(view, now) ??
      el(
        "p",
        { class: "queue__note", data: { testid: "work-queue-empty" } },
        "No host has reported a work queue yet. Queue telemetry needs a daemon with #8852 phase 2 and its work finder enabled.",
      ),
    el(
      "p",
      { class: "queue__note" },
      "One row per issue. When several hosts list the same issue, the host furthest along is shown and the rest " +
        "are in the host cell's tooltip. Hosts silent for over 4 hours are left out; a stale host's rows are badged. " +
        "Waiting is time since the issue was created." +
        (withheld > 0 ? ` ${withheld} row${withheld === 1 ? "" : "s"} from private repositories are shown without detail.` : ""),
    ),
    itemList("running", items, now),
    itemList("ready", items, now),
    itemList("blocked", items, now),
  );
}

// ---------------------------------------------------------------------------
// Host detail — the host's own ranked queue
// ---------------------------------------------------------------------------

function hostRow(row: QueueRow, sweeps: readonly ActiveSweep[], now: Date): HTMLElement {
  const sweep = sweeps.find((s) => s.issue !== undefined && s.issue === row.issue && s.repo === row.repo);
  return el(
    "tr",
    { class: `queue-row queue-row--${row.state}`, data: { testid: "host-queue-row", state: row.state, rank: row.rank } },
    el("td", {}, rankText(row)),
    el("td", {}, issueCell(row.repo, row.issue, row.visibility), flags(row)),
    el("td", {}, repoCell(row.repo)),
    el("td", {}, row.state),
    el("td", {}, row.state === "running" ? phaseText(sweep, now) : UNKNOWN),
    el("td", {}, waitingCell(row, now)),
    el("td", {}, reasonCell(row, row.repo)),
  );
}

/** The host-detail panel: always rendered, so a host with no queue says so. */
export function hostQueuePanel(host: HostView, now: Date = new Date()): HTMLElement {
  const summary = summarizeHostQueue(host, now);
  const record = summary.queue?.record;
  const dropped = record ? record.unresolved_rows + record.rows_truncated : 0;
  return el(
    "section",
    { class: "panel queue", data: { testid: "host-queue", health: summary.health } },
    el(
      "header",
      { class: "queue__header" },
      el("h2", { class: "queue__title" }, "Work queue (dispatch order)"),
      queueHealthBadge(summary, now),
    ),
    el("p", { class: "queue__note" }, queueFreshnessText(summary, now)),
    record
      ? el(
          "p",
          { class: "queue__note" },
          `Backlog ${record.seen} · running ${record.counts.running} · ready ${record.counts.ready} · ` +
            `blocked ${record.counts.blocked}` +
            (record.max_concurrent !== undefined ? ` · concurrency cap ${record.max_concurrent}` : "") +
            (dropped > 0 ? ` · ${dropped} row${dropped === 1 ? "" : "s"} not exported` : ""),
        )
      : null,
    record && record.rows.length > 0
      ? el(
          "table",
          { class: "queue__table" },
          el(
            "thead",
            {},
            el("tr", {}, ["#", "Issue", "Repository", "State", "Phase", "Waiting", "Reason"].map((h) => el("th", {}, h))),
          ),
          el("tbody", {}, record.rows.map((row) => hostRow(row, host.sweeps, now))),
        )
      : null,
  );
}
