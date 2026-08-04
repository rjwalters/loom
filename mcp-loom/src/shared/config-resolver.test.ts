/**
 * Tests for the mcp-loom config resolver (issue #4064).
 *
 * Two responsibilities:
 *   1. Byte-parity with `loom-daemon/src/config_resolver.rs` — the tier
 *      precedence, `deepMerge` semantics, and soft-skip behavior must match
 *      the Rust reference, proven both by mirrored unit cases and by the
 *      shared cross-language conformance fixture.
 *   2. The #4064 write-policy (option 3) — `configure_terminal` must refuse to
 *      flatten a non-legacy tier into the legacy `.loom/config.json`.
 */

import { mkdir, mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { afterEach, describe, expect, it } from "vitest";
import { handleTerminalTool } from "../tools/terminals.js";
import {
  deepMerge,
  getPath,
  hasNonLegacyTier,
  LEGACY_CONFIG_REL,
  LOCAL_CONFIG_REL,
  privateDefaultsPath,
  PRIVATE_DEFAULTS_ENV,
  PROJECT_CONFIG_REL,
  resolveEffectiveConfig,
  softReadJsonObject,
} from "./config-resolver.js";

const testDir = dirname(fileURLToPath(import.meta.url));

// The shared conformance fixture tree, four levels up from src/shared/.
const CONFORMANCE_FIXTURE_DIR = join(
  testDir,
  "..",
  "..",
  "..",
  "defaults",
  "scripts",
  "tests",
  "fixtures",
  "config_resolver"
);

// ---- env isolation --------------------------------------------------------

const ORIGINAL_ENV = {
  defaults: process.env[PRIVATE_DEFAULTS_ENV],
  workspace: process.env.LOOM_WORKSPACE,
};

function restoreEnv(key: string, value: string | undefined): void {
  if (value === undefined) {
    delete process.env[key];
  } else {
    process.env[key] = value;
  }
}

const tempRoots: string[] = [];

afterEach(async () => {
  restoreEnv(PRIVATE_DEFAULTS_ENV, ORIGINAL_ENV.defaults);
  restoreEnv("LOOM_WORKSPACE", ORIGINAL_ENV.workspace);
  await Promise.all(tempRoots.splice(0).map((p) => rm(p, { recursive: true, force: true })));
});

/** Disable the private-defaults tier for deterministic, host-independent output. */
function disableDefaultsTier(): void {
  process.env[PRIVATE_DEFAULTS_ENV] = "";
}

/** Create a temp repo root and write the requested tier files into it. */
async function makeRepo(tiers: {
  legacy?: string;
  project?: string;
  local?: string;
}): Promise<string> {
  const root = await mkdtemp(join(tmpdir(), "loom-cfg-"));
  tempRoots.push(root);
  if (tiers.legacy !== undefined) {
    await mkdir(join(root, ".loom"), { recursive: true });
    await writeFile(join(root, LEGACY_CONFIG_REL), tiers.legacy);
  }
  if (tiers.project !== undefined) {
    await mkdir(join(root, ".loom-project"), { recursive: true });
    await writeFile(join(root, PROJECT_CONFIG_REL), tiers.project);
  }
  if (tiers.local !== undefined) {
    await mkdir(join(root, ".loom-local"), { recursive: true });
    await writeFile(join(root, LOCAL_CONFIG_REL), tiers.local);
  }
  return root;
}

// ===== deepMerge (mirrors config_resolver.rs) =====

describe("deepMerge", () => {
  it("unions disjoint keys", () => {
    expect(deepMerge({ a: 1 }, { b: 2 })).toEqual({ a: 1, b: 2 });
  });

  it("merges nested objects recursively", () => {
    expect(deepMerge({ a: { x: 1 } }, { a: { y: 2 } })).toEqual({ a: { x: 1, y: 2 } });
  });

  it("lets a scalar overlay replace the base", () => {
    expect(deepMerge({ a: 1 }, { a: 2 })).toEqual({ a: 2 });
  });

  it("lets an explicit null overlay clear the key", () => {
    expect(deepMerge({ a: 1 }, { a: null })).toEqual({ a: null });
  });

  it("treats an empty overlay as a no-op", () => {
    const base = { a: { x: 1 }, b: [1, 2] };
    expect(deepMerge(base, {})).toEqual(base);
  });

  it("replaces arrays rather than concatenating them", () => {
    expect(deepMerge({ a: [1, 2] }, { a: [3] })).toEqual({ a: [3] });
  });
});

// ===== softReadJsonObject (mirrors config_resolver.rs) =====

describe("softReadJsonObject", () => {
  it("returns {} for a missing file", async () => {
    const root = await mkdtemp(join(tmpdir(), "loom-cfg-"));
    tempRoots.push(root);
    expect(await softReadJsonObject(join(root, "nope.json"))).toEqual({});
  });

  it("returns {} for malformed JSON", async () => {
    const root = await mkdtemp(join(tmpdir(), "loom-cfg-"));
    tempRoots.push(root);
    const p = join(root, "bad.json");
    await writeFile(p, "not valid json");
    expect(await softReadJsonObject(p)).toEqual({});
  });

  it("returns {} for a non-object top-level value", async () => {
    const root = await mkdtemp(join(tmpdir(), "loom-cfg-"));
    tempRoots.push(root);
    const p = join(root, "array.json");
    await writeFile(p, "[1, 2, 3]");
    expect(await softReadJsonObject(p)).toEqual({});
  });

  it("passes a valid object through unchanged", async () => {
    const root = await mkdtemp(join(tmpdir(), "loom-cfg-"));
    tempRoots.push(root);
    const p = join(root, "ok.json");
    await writeFile(p, '{"a": 1}');
    expect(await softReadJsonObject(p)).toEqual({ a: 1 });
  });
});

// ===== privateDefaultsPath =====

describe("privateDefaultsPath", () => {
  it("honors a non-empty env override", () => {
    process.env[PRIVATE_DEFAULTS_ENV] = "/tmp/custom-defaults.json";
    expect(privateDefaultsPath()).toBe("/tmp/custom-defaults.json");
  });

  it("disables the tier when the env var is set to empty", () => {
    process.env[PRIVATE_DEFAULTS_ENV] = "";
    expect(privateDefaultsPath()).toBeNull();
  });
});

// ===== resolveEffectiveConfig: behavior preservation =====

describe("resolveEffectiveConfig", () => {
  it("matches the legacy file content exactly when only that tier is present", async () => {
    disableDefaultsTier();
    const root = await makeRepo({
      legacy: '{"nextAgentNumber": 3, "autonomous": {"perTokenConcurrency": 2}}',
    });
    expect(await resolveEffectiveConfig(root)).toEqual({
      nextAgentNumber: 3,
      autonomous: { perTokenConcurrency: 2 },
    });
  });

  it("returns {} when no files are present", async () => {
    disableDefaultsTier();
    const root = await makeRepo({});
    expect(await resolveEffectiveConfig(root)).toEqual({});
  });

  it("soft-fails a malformed legacy tier without aborting the others", async () => {
    disableDefaultsTier();
    const root = await makeRepo({
      legacy: "{not json",
      project: '{"buildGate": {"enabled": true}}',
    });
    expect(await resolveEffectiveConfig(root)).toEqual({ buildGate: { enabled: true } });
  });

  it("applies precedence local > project > legacy", async () => {
    disableDefaultsTier();
    const root = await makeRepo({
      legacy: '{"a": "legacy", "shared": 1}',
      project: '{"a": "project", "shared": 2}',
      local: '{"a": "local"}',
    });
    const effective = await resolveEffectiveConfig(root);
    expect(effective.a).toBe("local");
    expect(effective.shared).toBe(2);
  });

  it("unions disjoint keys across tiers", async () => {
    disableDefaultsTier();
    const root = await makeRepo({
      legacy: '{"legacyOnly": 1}',
      project: '{"projectOnly": 2}',
      local: '{"localOnly": 3}',
    });
    expect(await resolveEffectiveConfig(root)).toEqual({
      legacyOnly: 1,
      projectOnly: 2,
      localOnly: 3,
    });
  });

  it("merges a nested autonomous block across tiers", async () => {
    disableDefaultsTier();
    const root = await makeRepo({
      legacy: '{"autonomous": {"workFinder": {"enabled": true}}}',
      local: '{"autonomous": {"perTokenConcurrency": 4}}',
    });
    expect(await resolveEffectiveConfig(root)).toEqual({
      autonomous: { workFinder: { enabled: true }, perTokenConcurrency: 4 },
    });
  });
});

// ===== getPath =====

describe("getPath", () => {
  it("resolves a nested dotted key", () => {
    expect(getPath({ autonomous: { workFinder: { enabled: true } } }, "autonomous.workFinder.enabled")).toBe(
      true
    );
  });

  it("returns undefined for a missing segment", () => {
    expect(getPath({ autonomous: {} }, "autonomous.workFinder.enabled")).toBeUndefined();
  });

  it("returns undefined when indexing through a scalar", () => {
    expect(getPath({ a: 1 }, "a.b")).toBeUndefined();
  });

  it("resolves a top-level key", () => {
    expect(getPath({ nextAgentNumber: 3 }, "nextAgentNumber")).toBe(3);
  });
});

// ===== Cross-language conformance fixture (#4039 / #4064 parity) =====

describe("cross-language conformance", () => {
  it("resolves the shared fixture identically to the Rust reference's expected.json", async () => {
    disableDefaultsTier();
    const effective = await resolveEffectiveConfig(CONFORMANCE_FIXTURE_DIR);
    const expectedText = await readFile(join(CONFORMANCE_FIXTURE_DIR, "expected.json"), "utf-8");
    expect(effective).toEqual(JSON.parse(expectedText));
  });
});

// ===== Write-policy (option 3): no flattening of non-legacy tiers =====

describe("configure_terminal write policy", () => {
  it("refuses to flatten a non-legacy tier into .loom/config.json", async () => {
    disableDefaultsTier();
    const legacy = JSON.stringify(
      { version: "1.0", offlineMode: false, terminals: [{ id: "terminal-1", name: "Old" }] },
      null,
      2
    );
    const project = JSON.stringify(
      { projectOnly: "leaked-if-flattened", autonomous: { perTokenConcurrency: 9 } },
      null,
      2
    );
    const root = await makeRepo({ legacy, project });
    process.env.LOOM_WORKSPACE = root;

    const result = await handleTerminalTool("configure_terminal", {
      terminal_id: "terminal-1",
      name: "New",
    });
    const text = result[0]?.text ?? "";

    // (0) The write was refused with an actionable error.
    expect(text).toContain("Failed");
    expect(text).toContain("non-legacy config tier");

    // (a) The tier file is byte-unchanged.
    expect(await readFile(join(root, PROJECT_CONFIG_REL), "utf-8")).toBe(project);

    // (b) The legacy file gained no keys sourced from the project tier (in
    //     fact it is byte-unchanged — the refuse path never writes).
    const legacyAfter = await readFile(join(root, LEGACY_CONFIG_REL), "utf-8");
    expect(legacyAfter).toBe(legacy);
    expect(legacyAfter).not.toContain("projectOnly");
    expect(legacyAfter).not.toContain("perTokenConcurrency");
  });

  it("hasNonLegacyTier is true with a project tier and false without", async () => {
    disableDefaultsTier();
    const withProject = await makeRepo({ legacy: "{}", project: '{"a": 1}' });
    const legacyOnly = await makeRepo({ legacy: '{"a": 1}' });
    expect(await hasNonLegacyTier(withProject)).toBe(true);
    expect(await hasNonLegacyTier(legacyOnly)).toBe(false);
  });

  it("still edits the legacy file when it is the only tier present", async () => {
    disableDefaultsTier();
    const legacy = JSON.stringify(
      { version: "1.0", terminals: [{ id: "terminal-1", name: "Old" }] },
      null,
      2
    );
    const root = await makeRepo({ legacy });
    process.env.LOOM_WORKSPACE = root;

    const result = await handleTerminalTool("configure_terminal", {
      terminal_id: "terminal-1",
      name: "New",
    });
    expect(result[0]?.text ?? "").toContain("Success");

    const after = JSON.parse(await readFile(join(root, LEGACY_CONFIG_REL), "utf-8"));
    expect(after.terminals[0].name).toBe("New");
  });
});
