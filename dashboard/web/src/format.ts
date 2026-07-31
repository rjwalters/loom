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

/** Absolute timestamp for a `title=` tooltip — the relative form is what is
 * shown, the absolute one is what gets pasted into a bug report. */
export function formatAbsolute(iso: string | undefined): string {
  if (!iso) return UNKNOWN;
  const parsed = Date.parse(iso);
  if (Number.isNaN(parsed)) return UNKNOWN;
  return new Date(parsed).toISOString().replace("T", " ").replace(".000Z", "Z");
}

export function formatCount(value: number | undefined): string {
  return value === undefined ? UNKNOWN : String(value);
}

/** `undefined`/empty → `"—"`, for the optional string fields (`model`,
 * `effort`, `phase`, `repo`). */
export function formatText(value: string | undefined): string {
  return value && value.length > 0 ? value : UNKNOWN;
}
