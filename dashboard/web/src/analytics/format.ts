/**
 * Display formatting for the token/cost analytics view (Epic #4702, Phase 3,
 * issue #4752).
 *
 * One rule runs through all of it: **an unknown value renders as `—`, never as
 * `0`, `0%`, or `never`.** The daemon's telemetry contract makes an
 * unmeasurable probe *absent* rather than zero, and the UI is the last place
 * that distinction can be destroyed.
 */

/** The em-dash every "not known" cell renders as. */
export const UNKNOWN = "—";

/** `0.4237` → `"42.4%"`; `undefined` → `"—"`. */
export function formatPercent(fraction: number | undefined, digits = 1): string {
  if (fraction === undefined || !Number.isFinite(fraction)) return UNKNOWN;
  return `${(fraction * 100).toFixed(digits)}%`;
}

/** Burn rate as a percentage of the limit window per hour. */
export function formatRatePerHour(fractionPerHour: number | undefined): string {
  if (fractionPerHour === undefined || !Number.isFinite(fractionPerHour)) return UNKNOWN;
  return `${(fractionPerHour * 100).toFixed(1)}%/h`;
}

/**
 * A signed, coarse duration: `"2h 5m"`, `"in 40m"`, `"12m ago"`.
 *
 * Coarse on purpose — a forecast accurate to the second would imply a
 * precision a two-point linear fit does not have.
 */
export function formatDuration(seconds: number | undefined): string {
  if (seconds === undefined || !Number.isFinite(seconds)) return UNKNOWN;
  const magnitude = Math.abs(Math.round(seconds));
  const days = Math.floor(magnitude / 86400);
  const hours = Math.floor((magnitude % 86400) / 3600);
  const minutes = Math.floor((magnitude % 3600) / 60);

  let text: string;
  if (days > 0) text = `${days}d ${hours}h`;
  else if (hours > 0) text = `${hours}h ${minutes}m`;
  else if (minutes > 0) text = `${minutes}m`;
  else text = `${magnitude}s`;
  return text;
}

/** `formatDuration` with a direction: future → `"in 40m"`, past → `"40m ago"`. */
export function formatRelative(seconds: number | undefined): string {
  if (seconds === undefined || !Number.isFinite(seconds)) return UNKNOWN;
  const text = formatDuration(seconds);
  if (Math.abs(Math.round(seconds)) < 30) return "now";
  return seconds >= 0 ? `in ${text}` : `${text} ago`;
}

/** Short local wall-clock time for an epoch-ms instant. */
export function formatInstant(at: number | undefined): string {
  if (at === undefined || !Number.isFinite(at)) return UNKNOWN;
  return new Date(at).toLocaleString(undefined, {
    month: "short",
    day: "numeric",
    hour: "2-digit",
    minute: "2-digit",
  });
}
