/**
 * Config resolution layer for mcp-loom (Epic #3835, issue #4064).
 *
 * A TypeScript port of `loom-daemon/src/config_resolver.rs` (the reference
 * implementation). It resolves the effective config by deep-merging the tier
 * chain, lowest to highest precedence:
 *
 *   1. Private/shared defaults (`$LOOM_CONFIG_DEFAULTS_FILE`, default
 *      `~/.local/share/loom/config/defaults.json`) — a no-op tier on every
 *      host until Epic #3835 Phase 3 ships.
 *   2. Legacy `.loom/config.json` — the sole tier every existing repo has.
 *   3. Tracked `.loom-project/project.json` — empty until a repo migrates.
 *   4. Ignored `.loom-local/local.json` — host-local override.
 *
 * The tier order, `deepMerge` semantics (objects merge recursively, arrays
 * replace, explicit `null` replaces), and soft-skip behavior (missing /
 * unreadable / malformed / non-object files contribute nothing, never fatal)
 * are kept byte-compatible with the Rust reference. The shared conformance
 * fixture in `loom-tools/tests/fixtures/config_resolver/` must resolve
 * identically here and in Rust/Python/Bash.
 *
 * NOTE: all existing resolvers (Rust/Python/Bash) are read-only. This module
 * is likewise read-only — it never writes. The write-target policy for
 * `configure_terminal` is enforced separately via `hasNonLegacyTier` (see
 * issue #4064's write-policy decision): the writer refuses to flatten the
 * merged view back into the legacy file when a non-legacy tier is present.
 */

import { readFile } from "node:fs/promises";
import { homedir } from "node:os";
import { join } from "node:path";

/** Repo-relative path to the legacy, currently load-bearing config file. */
export const LEGACY_CONFIG_REL = ".loom/config.json";

/** Repo-relative path to the tracked, project-specific config file. */
export const PROJECT_CONFIG_REL = ".loom-project/project.json";

/** Repo-relative path to the ignored, host-local config override file. */
export const LOCAL_CONFIG_REL = ".loom-local/local.json";

/** Env var overriding the private/shared defaults file location. */
export const PRIVATE_DEFAULTS_ENV = "LOOM_CONFIG_DEFAULTS_FILE";

/** Home-relative default location of the private/shared defaults file. */
const DEFAULT_PRIVATE_DEFAULTS_REL = ".local/share/loom/config/defaults.json";

/** A JSON object (the shape every tier is normalized to before merging). */
export type JsonObject = Record<string, unknown>;

/**
 * True for a plain JSON object — i.e. not `null`, not an array, not a scalar.
 * Mirrors Rust's `Value::Object(_)` match arm.
 */
function isPlainObject(value: unknown): value is JsonObject {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

/**
 * Resolve the path to the private/shared defaults file, honoring
 * {@link PRIVATE_DEFAULTS_ENV}. Returns `null` when the env var is explicitly
 * set to an empty string (tier disabled). Mirrors Rust's
 * `private_defaults_path`: an unset env var falls back to the home-relative
 * default; a set-but-empty env var disables the tier.
 */
export function privateDefaultsPath(): string | null {
  const env = process.env[PRIVATE_DEFAULTS_ENV];
  if (env !== undefined) {
    return env === "" ? null : env;
  }
  return join(homedir(), DEFAULT_PRIVATE_DEFAULTS_REL);
}

/**
 * Soft-fail JSON-object read: a missing file, an unreadable file, malformed
 * JSON, or a non-object top-level value all resolve to an empty object
 * (`{}`) — that tier simply contributes nothing to the merge. Never throws.
 * Mirrors Rust's `soft_read_json_object`.
 */
export async function softReadJsonObject(path: string): Promise<JsonObject> {
  let text: string;
  try {
    text = await readFile(path, "utf-8");
  } catch {
    return {};
  }

  let parsed: unknown;
  try {
    parsed = JSON.parse(text);
  } catch {
    return {};
  }

  return isPlainObject(parsed) ? parsed : {};
}

/**
 * Deep-merge `overlay` onto `base`, returning the merged value.
 *
 * Semantics (identical to Rust's `deep_merge` and `jq`'s `*` operator):
 * - When both sides are JSON objects, merge recursively key-by-key.
 * - Otherwise `overlay` replaces `base` outright — including when `overlay`
 *   is `null` (an explicit `null` in a higher tier clears the key) and when
 *   either side is an array (arrays replace, never concatenate).
 */
export function deepMerge(base: unknown, overlay: unknown): unknown {
  if (isPlainObject(base) && isPlainObject(overlay)) {
    const merged: JsonObject = { ...base };
    for (const key of Object.keys(overlay)) {
      merged[key] = Object.prototype.hasOwnProperty.call(merged, key)
        ? deepMerge(merged[key], overlay[key])
        : overlay[key];
    }
    return merged;
  }
  return overlay;
}

/**
 * Resolve the full effective config tree for `repoRoot` by deep-merging the
 * tier chain lowest → highest. A missing or malformed file at any tier
 * soft-fails to that tier contributing nothing. When only the legacy tier has
 * content — the state of every repo today — the result is byte-for-byte the
 * parsed content of `.loom/config.json`, preserving existing behavior.
 */
export async function resolveEffectiveConfig(repoRoot: string): Promise<JsonObject> {
  let effective: JsonObject = {};

  const defaultsPath = privateDefaultsPath();
  if (defaultsPath !== null) {
    effective = deepMerge(effective, await softReadJsonObject(defaultsPath)) as JsonObject;
  }

  effective = deepMerge(effective, await softReadJsonObject(join(repoRoot, LEGACY_CONFIG_REL))) as JsonObject;
  effective = deepMerge(effective, await softReadJsonObject(join(repoRoot, PROJECT_CONFIG_REL))) as JsonObject;
  effective = deepMerge(effective, await softReadJsonObject(join(repoRoot, LOCAL_CONFIG_REL))) as JsonObject;

  return effective;
}

/**
 * Look up a dotted key path (e.g. `"autonomous.workFinder.enabled"`) in an
 * already-resolved effective config tree. Returns `undefined` on any missing
 * segment or when a non-object value is indexed further. Mirrors Rust's
 * `get_path`.
 */
export function getPath(config: unknown, dotted: string): unknown {
  let cur: unknown = config;
  for (const segment of dotted.split(".")) {
    if (!isPlainObject(cur) || !Object.prototype.hasOwnProperty.call(cur, segment)) {
      return undefined;
    }
    cur = cur[segment];
  }
  return cur;
}

/**
 * True when a non-legacy tier (private defaults, `.loom-project/project.json`,
 * or `.loom-local/local.json`) contributes any content for `repoRoot`.
 *
 * This is the guard behind the #4064 write-policy decision (option 3): when a
 * non-legacy tier is present, `configure_terminal` must NOT write the merged
 * view back to `.loom/config.json`, because doing so would flatten every
 * higher-tier override into the legacy file and de-sync it from its source of
 * truth. The writer refuses instead, directing the operator to edit the tier
 * file directly.
 */
export async function hasNonLegacyTier(repoRoot: string): Promise<boolean> {
  const defaultsPath = privateDefaultsPath();
  if (defaultsPath !== null) {
    if (Object.keys(await softReadJsonObject(defaultsPath)).length > 0) {
      return true;
    }
  }
  if (Object.keys(await softReadJsonObject(join(repoRoot, PROJECT_CONFIG_REL))).length > 0) {
    return true;
  }
  if (Object.keys(await softReadJsonObject(join(repoRoot, LOCAL_CONFIG_REL))).length > 0) {
    return true;
  }
  return false;
}
