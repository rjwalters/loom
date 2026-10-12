/**
 * Optional `dispatch_sweep` operator options (issue #4317 `force`, issue #11284
 * per-dispatch `runtime` / `model_profile`): input-schema properties, defensive
 * argument extraction, and wire-body mapping. Lives beside `sweeps.ts` so that
 * ratcheted file does not grow (.loom/docs/file-size-policy.md).
 */

export interface DispatchOptions {
  force?: boolean;
  runtime?: string;
  model_profile?: string;
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
  runtime: {
    type: "string",
    description:
      "Optional (issue #11284): run this sweep's Builder and Doctor " +
      "phases on an explicit runtime (e.g. `opencode`). Curator, " +
      "Judge and the orchestrator keep their role policy. An " +
      "unavailable or unadmitted runtime/profile is refused with the " +
      "reason - it never falls through to the preference list. Omit " +
      "to keep the configured role preference.",
  },
  model_profile: {
    type: "string",
    description:
      "Optional model profile paired with `runtime` (e.g. " +
      "`zai-flash`; `modelProfile` is accepted as an alias). Requires " +
      "`runtime`.",
  },
};

function nonEmpty(v: unknown): string | undefined {
  return typeof v === "string" && v.trim().length > 0 ? v.trim() : undefined;
}

/**
 * Defensive extraction from raw tool args. Issue #4317: only a literal boolean
 * `true` forwards `force` as `true` (the string `"true"` or `1` must NOT).
 * Issue #11284: blank strings are unset; shape/combination errors are reported
 * by the daemon with an actionable message.
 */
export function extractDispatchOptions(args: Record<string, unknown> | undefined): DispatchOptions {
  return {
    force: args?.force === true,
    runtime: nonEmpty(args?.runtime),
    model_profile: nonEmpty(args?.model_profile) ?? nonEmpty(args?.modelProfile),
  };
}

/**
 * Wire-body fields. `force` MUST default to `false` (never `null`/omitted:
 * `null` fails serde deserialization on every dispatch); `runtime` /
 * `model_profile` default to `null`, keeping the role policy.
 */
export function dispatchOptionBodyFields(opts: DispatchOptions) {
  return {
    force: opts.force ?? false,
    runtime: opts.runtime ?? null,
    model_profile: opts.model_profile ?? null,
  };
}
