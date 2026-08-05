/**
 * Display formatting.
 *
 * The single rule every function here obeys: **`undefined` renders as
 * `UNKNOWN` ("—"), never as a zero.** `.loom/docs/telemetry-schema.md` is
 * explicit that a `host.health` probe which could not measure omits its field,
 * and that "A consumer MUST treat an absent measurement as unknown, never as
 * zero/full". Rendering an absent `cpu_idle_fraction` as `0%` would show a
 * pegged CPU on a host that merely has no CPU probe; rendering an absent
 * `worktree_root_free_gb` as `0 GB` would show a full disk. Both are tested.
 */

import { displayTimeZone, timeZoneAbbreviation } from "./timezone";
import type { HostProtection, RoleTickFailure, RoleTickHealth } from "./types";

export const UNKNOWN = "—";

/** `0.83` → `"83%"`. */
export function formatPercent(fraction: number | undefined, digits = 0): string {
  if (fraction === undefined) return UNKNOWN;
  return `${(fraction * 100).toFixed(digits)}%`;
}

/** `0.51` → `"0.51"` — load per core, where the absolute value matters. */
export function formatRatio(value: number | undefined, digits = 2): string {
  if (value === undefined) return UNKNOWN;
  return value.toFixed(digits);
}

/** `200` → `"200 GB"`; sub-10 values keep one decimal so "0.4 GB free" is
 * distinguishable from "0 GB free". */
export function formatGigabytes(value: number | undefined): string {
  if (value === undefined) return UNKNOWN;
  return value < 10 ? `${value.toFixed(1)} GB` : `${Math.round(value)} GB`;
}

/** `(200, 1000)` → `"200 GB (80% used)"` — `worktree_root_total_gb` (#5356)
 * is the denominator `worktree_root_free_gb` needs to become a percentage,
 * which is comparable across a heterogeneous fleet in a way an absolute GB
 * figure is not.
 *
 * Falls back to bare `formatGigabytes(freeGb)` — exactly the pre-#5356
 * rendering — whenever `totalGb` is missing or not a usable denominator
 * (`undefined`, `<= 0`): a free-but-no-total record must never have a
 * percentage fabricated for it. */
export function formatDiskFree(freeGb: number | undefined, totalGb: number | undefined): string {
  const free = formatGigabytes(freeGb);
  if (freeGb === undefined || totalGb === undefined || totalGb <= 0) return free;
  const usedFraction = 1 - freeGb / totalGb;
  return `${free} (${formatPercent(usedFraction)} used)`;
}

/** `86400` → `"1d 0h"`. Coarse by design: fleet uptime is read for "did this
 * daemon restart recently", not for precision. */
export function formatDuration(seconds: number | undefined): string {
  if (seconds === undefined) return UNKNOWN;
  if (seconds < 0) return UNKNOWN;
  const total = Math.floor(seconds);
  const days = Math.floor(total / 86400);
  const hours = Math.floor((total % 86400) / 3600);
  const minutes = Math.floor((total % 3600) / 60);
  if (days > 0) return `${days}d ${hours}h`;
  if (hours > 0) return `${hours}h ${minutes}m`;
  if (minutes > 0) return `${minutes}m ${total % 60}s`;
  return `${total}s`;
}

/** Seconds between an RFC 3339 timestamp and `now`, or `undefined` when the
 * timestamp is missing/unparseable. Negative for a future timestamp (clock
 * skew between a fleet host and the viewer) — callers clamp rather than
 * rendering "in 3 minutes ago". */
export function secondsSince(iso: string | undefined, now: Date = new Date()): number | undefined {
  if (!iso) return undefined;
  const then = Date.parse(iso);
  if (Number.isNaN(then)) return undefined;
  return (now.getTime() - then) / 1000;
}

/** `"2026-07-30T12:00:00Z"` → `"3m ago"` / `"just now"`. */
export function formatRelative(iso: string | undefined, now: Date = new Date()): string {
  const seconds = secondsSince(iso, now);
  if (seconds === undefined) return UNKNOWN;
  if (seconds < 0) return "just now"; // clock skew; see secondsSince
  if (seconds < 10) return "just now";
  return `${formatDuration(seconds)} ago`;
}

/**
 * Like `formatRelative`, but for a timestamp that is *expected* to be in the
 * future: `"in 5h 50m"` / `"5m 0s ago"`.
 *
 * Separate from `formatRelative` on purpose. `formatRelative` reads a
 * timestamp of something that already happened (a push, a sweep start), so a
 * future value there means clock skew and is clamped to "just now".
 * `tokens.snapshot`'s `limit_window_reset_at` is the opposite — a future value
 * is the normal case, and clamping it would hide exactly the number an
 * operator is looking for ("when does this exhausted account come back?").
 */
export function formatCountdown(iso: string | undefined, now: Date = new Date()): string {
  const seconds = secondsSince(iso, now);
  if (seconds === undefined) return UNKNOWN;
  if (seconds < 0) return `in ${formatDuration(-seconds)}`;
  if (seconds < 10) return "now";
  return `${formatDuration(seconds)} ago`;
}

/**
 * Absolute timestamp for a `title=` tooltip — the relative form is what is
 * shown, the absolute one is what gets pasted into a bug report.
 *
 * Rendered in the display timezone (`./timezone.ts`) rather than UTC, so it
 * agrees with the chart buckets and with what the operator sees on their own
 * clock. The zone abbreviation is always appended: a bare wall-clock time in
 * a bug report is worse than useless, because the reader cannot tell which
 * zone it was captured in.
 */
export function formatAbsolute(iso: string | undefined, zone: string = displayTimeZone()): string {
  if (!iso) return UNKNOWN;
  const parsed = Date.parse(iso);
  if (Number.isNaN(parsed)) return UNKNOWN;

  const at = new Date(parsed);
  const parts = new Intl.DateTimeFormat("en-CA", {
    timeZone: zone,
    year: "numeric",
    month: "2-digit",
    day: "2-digit",
    hour: "2-digit",
    minute: "2-digit",
    second: "2-digit",
    hour12: false,
  }).formatToParts(at);

  const get = (type: string): string => parts.find((p) => p.type === type)?.value ?? "";
  // Assembled from parts rather than the formatted string: locale output
  // order is not guaranteed, and `hour12: false` can yield "24" for midnight
  // in some runtimes, which this normalizes away.
  const hour = get("hour") === "24" ? "00" : get("hour");

  return (
    `${get("year")}-${get("month")}-${get("day")} ` +
    `${hour}:${get("minute")}:${get("second")} ${timeZoneAbbreviation(zone, at)}`
  );
}

/** Wall-clock `HH:MM:SS TZ` in the display timezone — the refresh status
 * line, where the date is redundant but the zone is not. */
export function formatClock(at: Date, zone: string = displayTimeZone()): string {
  const parts = new Intl.DateTimeFormat("en-CA", {
    timeZone: zone,
    hour: "2-digit",
    minute: "2-digit",
    second: "2-digit",
    hour12: false,
  }).formatToParts(at);

  const get = (type: string): string => parts.find((p) => p.type === type)?.value ?? "";
  const hour = get("hour") === "24" ? "00" : get("hour");
  return `${hour}:${get("minute")}:${get("second")} ${timeZoneAbbreviation(zone, at)}`;
}

export function formatCount(value: number | undefined): string {
  return value === undefined ? UNKNOWN : String(value);
}

/** `/repos/loom` → `"loom"` — the final path component, the same "root
 * basename" the daemon's own `RoleFailure::label()` uses so this UI never
 * shows a full filesystem path. Falls back to the whole string when it has
 * no `/` at all (already a bare name, or a wrong-typed value). */
function pathBasename(root: string): string {
  const parts = root.split("/").filter((part) => part.length > 0);
  return parts.length > 0 ? parts[parts.length - 1]! : root;
}

/** `{ role: "judge", root: "/repos/loom" }` → `"judge @ loom"` — mirrors the
 * daemon's own `RoleFailure::label()` so a persistent failure reads
 * identically here and in `loom-daemon health`'s own output. Missing
 * `role`/`root` degrade to `UNKNOWN` rather than an empty string. */
export function roleFailureLabel(failure: RoleTickFailure): string {
  const role = failure.role && failure.role.length > 0 ? failure.role : UNKNOWN;
  const root = failure.root && failure.root.length > 0 ? pathBasename(failure.root) : UNKNOWN;
  return `${role} @ ${root}`;
}

/** Compact `host.health.roles` indicator for the fleet-overview card, where
 * the full `roleTickSummaryText` sentence would not fit: `"ok"` / `"no role
 * ticks"` / `"2 failing"`. The host-detail drill-down uses the fuller
 * `roleTickSummaryText` instead, which names the failing role(s). */
export function roleTickCompactText(roles: RoleTickHealth | undefined): string {
  if (roles === undefined || roles.total === undefined) return UNKNOWN;
  if (roles.total === 0) return "no role ticks";
  const failing = roles.persistent?.length ?? 0;
  return failing > 0 ? `${failing} failing` : "ok";
}

/** One-line `host.health.roles` summary, mirroring `loom-daemon health`'s
 * own `roles` section line: `"10/12 ticks ok"` when healthy, or `"10/12
 * ticks ok; 1 persistent failure(s): judge @ loom"` when not. `undefined`
 * (a pre-#5022 daemon, or the field simply not parsed yet) and `total: 0`
 * (the role runner idle/disabled) both render distinctly from a real
 * failure — neither is an alarm. */
export function roleTickSummaryText(roles: RoleTickHealth | undefined): string {
  if (roles === undefined || roles.total === undefined) return UNKNOWN;
  if (roles.total === 0) return "no role ticks";
  const ok = roles.ok ?? 0;
  const persistent = roles.persistent ?? [];
  if (persistent.length === 0) return `${ok}/${roles.total} ticks ok`;
  const names = persistent.map(roleFailureLabel).join(", ");
  return `${ok}/${roles.total} ticks ok; ${persistent.length} persistent failure(s): ${names}`;
}

/** `undefined`/empty → `"—"`, for the optional string fields (`model`,
 * `effort`, `phase`, `repo`). */
export function formatText(value: string | undefined): string {
  return value && value.length > 0 ? value : UNKNOWN;
}

/**
 * The three-way badge status a `host.health.protection` record (#5352)
 * renders as. `"unprotected"` covers both daemon-side verdicts that mean "a
 * future daemon death will go undetected on this host" — `"no-marker"` (crash
 * protection disarmed entirely) and `"watchdog-not-provisioned"` (armed but
 * nothing is scheduled to notice) — the fleet overview/host-detail views do
 * not need the finer distinction to decide whether to show a warning; see
 * `protectionText` for the full wording.
 *
 * An absent record (a pre-#5352 daemon, or a probe that could not run at
 * all) reads as `"unknown"`, never `"unprotected"` — a host must never look
 * falsely alarmed just because an older binary hasn't started reporting this
 * field yet, or `state` itself is `"unknown"` (the probe ran but the
 * watchdog-provisioning check specifically could not answer).
 */
export type ProtectionBadgeStatus = "protected" | "unprotected" | "unknown";

export function protectionBadgeStatus(protection: HostProtection | undefined): ProtectionBadgeStatus {
  switch (protection?.state) {
    case "protected":
      return "protected";
    case "no-marker":
    case "watchdog-not-provisioned":
      return "unprotected";
    default:
      return "unknown";
  }
}

/** Operator-facing one-line description of `host.health.protection` (#5352),
 * mirroring `loom-daemon status`'s own `Protection:` line wording verbatim so
 * the two surfaces never disagree. Absent entirely (a pre-#5352 daemon, or a
 * probe that never ran) reads as "not reported", distinct from every named
 * verdict — including the daemon's own `"unknown"` state, which means the
 * probe ran but could not answer. */
export function protectionText(protection: HostProtection | undefined): string {
  switch (protection?.state) {
    case "protected":
      return "protected";
    case "no-marker":
      return "unprotected — no autonomy-desired marker";
    case "watchdog-not-provisioned":
      return "watchdog job not provisioned";
    case "unknown":
      return "unknown (provisioning probe could not answer)";
    default:
      return "not reported";
  }
}
