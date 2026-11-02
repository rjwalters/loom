/**
 * Optional `dispatch_sweep` operator options (issue #4317 `force`):
 * input-schema properties, defensive argument extraction, and wire-body mapping. Lives beside `sweeps.ts` so that
 * ratcheted file does not grow (.loom/docs/file-size-policy.md).
 */

export interface DispatchOptions {
  force?: boolean;
}

/** JSON-schema `properties` for the options, spread into `dispatch_sweep`. */
export const DISPATCH_OPTION_PROPERTIES = {
  force: {
    type: "boolean",
    description:
      "Optional operator override for the host-distress circuit " +
      "breaker (issue #4235). A tripped breaker represents sustained, " +
      "already-observed host distress, so it hard-blocks an explicit " +
      "dispatch_sweep by default. Set to `true` to say \"I know the " +
      "host is distressed, dispatch anyway\" and bypass that block. " +
      "Omit (or pass `false`) to preserve the default refuse-when-" +
      "tripped behavior — mirrors the CLI's `loom-daemon dispatch <N> " +
      "--force`.",
  },
};

/**
 * Defensive extraction from raw tool args. Issue #4317: only a literal boolean
 * `true` forwards `force` as `true` (the string `"true"` or `1` must NOT).
 */
export function extractDispatchOptions(args: Record<string, unknown> | undefined): DispatchOptions {
  return {
    force: args?.force === true,
  };
}

/**
 * Wire-body fields. `force` MUST default to `false` (never `null`/omitted:
 * `null` fails serde deserialization on every dispatch).
 */
export function dispatchOptionBodyFields(opts: DispatchOptions) {
  return {
    force: opts.force ?? false,
  };
}
