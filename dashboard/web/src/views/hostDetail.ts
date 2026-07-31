/**
 * Per-host drill-down.
 *
 * Everything the snapshot knows about one host: the full `host.health` field
 * set, every `tokens.snapshot` account, and a row per in-flight sweep with the
 * fields the issue calls out — `repo`, `issue`, `phase`, `model`, `effort`,
 * `startedAt`, `enteredPhaseAt`.
 *
 * The two empty states here are ordinary, not exceptional: an **idle host**
 * (zero `activeSweeps`) is a healthy host, and a host with **no `health`/
 * `tokens` entry yet** is one whose first push was a sweep record. Neither may
 * look like an error.
 */

import { el, field } from "../dom";
import {
  UNKNOWN,
  formatAbsolute,
  formatDuration,
  formatGigabytes,
  formatPercent,
  formatRatio,
  formatCountdown,
  formatRelative,
  formatCount,
  formatText,
  secondsSince,
} from "../format";
import type { HostView } from "../fleet";
import type { ActiveSweep, TokenAccount } from "../types";
import { statusBadge } from "./fleetOverview";

function noticeRow(message: string, testid: string): HTMLElement {
  return el("p", { class: "panel__notice", data: { testid } }, message);
}

function healthPanel(host: HostView, now: Date): HTMLElement {
  const timestamped = host.entry.health;
  if (!timestamped) {
    return el(
      "section",
      { class: "panel", data: { testid: "health-panel" } },
      el("h2", { class: "panel__title" }, "Host health"),
      noticeRow(
        "This host has not pushed a host.health record yet. It is known only from " +
          "its sweep activity.",
        "health-missing",
      ),
    );
  }

  const health = timestamped.record;
  return el(
    "section",
    { class: "panel", data: { testid: "health-panel" } },
    el(
      "h2",
      { class: "panel__title" },
      "Host health",
      el(
        "span",
        { class: "panel__timestamp", title: formatAbsolute(timestamped.updatedAt) },
        formatRelative(timestamped.updatedAt, now),
      ),
    ),
    el(
      "dl",
      { class: "panel__fields" },
      field("Daemon version", health.daemon_version ?? UNKNOWN),
      field("Uptime", formatDuration(health.uptime_sec)),
      field("Logical CPUs", formatCount(health.logical_cpus)),
      field("CPU idle", formatPercent(health.cpu_idle_fraction, 1)),
      field("Load per core", formatRatio(health.load_per_core)),
      field("Worktree root free", formatGigabytes(health.worktree_root_free_gb)),
      field(
        "Captured at",
        health.captured_at ? formatAbsolute(health.captured_at) : UNKNOWN,
        "Daemon clock, as reported in the record",
      ),
    ),
  );
}

function accountRow(account: TokenAccount, now: Date): HTMLElement {
  const usage = account.usage_fraction;
  return el(
    "tr",
    {
      class: account.exhausted ? "row row--exhausted" : "row",
      data: { testid: "token-account", account: account.account ?? "" },
    },
    el("td", {}, formatText(account.account)),
    el("td", {}, account.rank === undefined ? UNKNOWN : String(account.rank)),
    el(
      "td",
      {},
      // The bar is decorative; the text is the value. When usage is unknown
      // there is no bar at all rather than an empty one that reads as 0%.
      usage === undefined
        ? UNKNOWN
        : el(
            "span",
            { class: "meter", title: formatPercent(usage, 1) },
            el(
              "span",
              { class: "meter__track" },
              el("span", {
                class: "meter__fill",
                // Clamp: a backend/daemon that ever reports >1 must not paint
                // outside the track.
                style: `width:${Math.min(100, Math.max(0, usage * 100))}%`,
              }),
            ),
            el("span", { class: "meter__label" }, formatPercent(usage)),
          ),
    ),
    el(
      "td",
      { title: account.limit_window_reset_at ? formatAbsolute(account.limit_window_reset_at) : undefined },
      formatCountdown(account.limit_window_reset_at, now),
    ),
    el(
      "td",
      {},
      el(
        "span",
        { class: account.exhausted ? "badge badge--degraded" : "badge badge--ok" },
        account.exhausted ? "exhausted" : "available",
      ),
    ),
  );
}

function tokensPanel(host: HostView, now: Date): HTMLElement {
  const timestamped = host.entry.tokens;
  const title = el("h2", { class: "panel__title" }, "Token pool");
  if (timestamped) {
    title.appendChild(
      el(
        "span",
        { class: "panel__timestamp", title: formatAbsolute(timestamped.updatedAt) },
        formatRelative(timestamped.updatedAt, now),
      ),
    );
  }

  if (!timestamped) {
    return el(
      "section",
      { class: "panel", data: { testid: "tokens-panel" } },
      title,
      noticeRow("This host has not pushed a tokens.snapshot record yet.", "tokens-missing"),
    );
  }
  if (host.tokens.accounts.length === 0) {
    return el(
      "section",
      { class: "panel", data: { testid: "tokens-panel" } },
      title,
      noticeRow(
        "The most recent tokens.snapshot reported no accounts — this host has no " +
          "token pool provisioned.",
        "tokens-empty",
      ),
    );
  }

  return el(
    "section",
    { class: "panel", data: { testid: "tokens-panel" } },
    title,
    el(
      "table",
      { class: "table" },
      el(
        "thead",
        {},
        el(
          "tr",
          {},
          el("th", {}, "Account"),
          el("th", {}, "Rank"),
          el("th", {}, "Usage"),
          el("th", {}, "Window resets"),
          el("th", {}, "State"),
        ),
      ),
      el("tbody", {}, host.tokens.accounts.map((account) => accountRow(account, now))),
    ),
  );
}

/** One in-flight sweep. `startedAt`/`enteredPhaseAt` are rendered both as an
 * elapsed duration (what an operator scans for — "which sweep is wedged") and
 * as an absolute timestamp in the tooltip. */
export function sweepRow(sweep: ActiveSweep, now: Date = new Date()): HTMLElement {
  const runningSec = secondsSince(sweep.startedAt, now);
  const inPhaseSec = secondsSince(sweep.enteredPhaseAt, now);
  return el(
    "tr",
    { class: "row", data: { testid: "sweep-row", sweep: sweep.sweepId } },
    el(
      "td",
      { title: sweep.sweepId },
      sweep.issue === undefined ? sweep.sweepId : `#${sweep.issue}`,
    ),
    el("td", {}, formatText(sweep.repo)),
    el("td", {}, el("span", { class: "chip" }, sweep.phase ?? "starting")),
    el("td", {}, formatText(sweep.model)),
    el("td", {}, formatText(sweep.effort)),
    el(
      "td",
      { title: formatAbsolute(sweep.startedAt) },
      runningSec === undefined ? UNKNOWN : formatDuration(Math.max(0, runningSec)),
    ),
    el(
      "td",
      { title: formatAbsolute(sweep.enteredPhaseAt) },
      inPhaseSec === undefined ? UNKNOWN : formatDuration(Math.max(0, inPhaseSec)),
    ),
  );
}

function sweepsPanel(host: HostView, now: Date): HTMLElement {
  if (host.sweeps.length === 0) {
    return el(
      "section",
      { class: "panel", data: { testid: "sweeps-panel" } },
      el("h2", { class: "panel__title" }, "Active sweeps"),
      noticeRow(
        "No sweeps are in flight on this host right now. Completed sweeps are not " +
          "live state — query them through /api/history.",
        "sweeps-empty",
      ),
    );
  }

  return el(
    "section",
    { class: "panel", data: { testid: "sweeps-panel" } },
    el(
      "h2",
      { class: "panel__title" },
      "Active sweeps",
      el("span", { class: "panel__timestamp" }, String(host.sweeps.length)),
    ),
    el(
      "table",
      { class: "table" },
      el(
        "thead",
        {},
        el(
          "tr",
          {},
          el("th", {}, "Issue"),
          el("th", {}, "Repo"),
          el("th", {}, "Phase"),
          el("th", {}, "Model"),
          el("th", {}, "Effort"),
          el("th", {}, "Running"),
          el("th", {}, "In phase"),
        ),
      ),
      el(
        "tbody",
        {},
        host.sweeps.map((sweep) => sweepRow(sweep, now)),
      ),
    ),
  );
}

export function hostDetailView(host: HostView, now: Date = new Date()): HTMLElement {
  return el(
    "section",
    { class: "detail", data: { testid: "host-detail", host: host.hostId } },
    el(
      "nav",
      { class: "detail__breadcrumb" },
      el("a", { class: "link", href: "#/" }, "← All hosts"),
    ),
    el(
      "header",
      { class: "detail__header" },
      el("h1", { class: "detail__title" }, host.hostId),
      statusBadge(host.status),
      el(
        "span",
        { class: "detail__last-report", title: formatAbsolute(host.lastReportAt) },
        host.lastReportAt ? `Last report ${formatRelative(host.lastReportAt, now)}` : "Never reported",
      ),
    ),
    healthPanel(host, now),
    tokensPanel(host, now),
    sweepsPanel(host, now),
  );
}
