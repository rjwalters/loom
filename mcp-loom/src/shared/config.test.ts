/**
 * Tests for CWD-based workspace discovery (issue #4230, epic #3835 Phase 3c).
 *
 * Under user-scope MCP registration a single mcp-loom server instance serves
 * every repo, so it can no longer rely on a per-repo `LOOM_WORKSPACE` baked into
 * a repo-local `.mcp.json`. `getWorkspacePath()` therefore resolves the repo the
 * server operates on from the process CWD:
 *   1. explicit `LOOM_WORKSPACE` env override still wins,
 *   2. otherwise walk up from `process.cwd()` to a `.loom/`/`.git` repo root
 *      (worktree CWDs resolve to the main checkout via the git common dir),
 *   3. otherwise fail loudly — NO silent `~/GitHub/loom` fallback.
 */

import { mkdir, mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { discoverWorkspaceRoot, getWorkspacePath } from "./config.js";

const ORIGINAL_WORKSPACE = process.env.LOOM_WORKSPACE;

let root: string;

beforeEach(async () => {
  root = await mkdtemp(join(tmpdir(), "loom-ws-"));
  delete process.env.LOOM_WORKSPACE;
});

afterEach(async () => {
  vi.restoreAllMocks();
  if (ORIGINAL_WORKSPACE === undefined) {
    delete process.env.LOOM_WORKSPACE;
  } else {
    process.env.LOOM_WORKSPACE = ORIGINAL_WORKSPACE;
  }
  await rm(root, { recursive: true, force: true });
});

describe("discoverWorkspaceRoot", () => {
  it("finds a repo root marked by .loom/ when starting at the root", async () => {
    const repo = join(root, "repoA");
    await mkdir(join(repo, ".loom"), { recursive: true });
    expect(discoverWorkspaceRoot(repo)).toBe(repo);
  });

  it("walks up from a nested subdirectory to the .loom/ root", async () => {
    const repo = join(root, "repoB");
    const nested = join(repo, "src", "shared", "deep");
    await mkdir(join(repo, ".loom"), { recursive: true });
    await mkdir(nested, { recursive: true });
    expect(discoverWorkspaceRoot(nested)).toBe(repo);
  });

  it("resolves a plain git checkout (no committed .loom) via its .git directory", async () => {
    const repo = join(root, "repoC");
    await mkdir(join(repo, ".git"), { recursive: true });
    const nested = join(repo, "pkg");
    await mkdir(nested, { recursive: true });
    expect(discoverWorkspaceRoot(nested)).toBe(repo);
  });

  it("resolves a linked-worktree CWD to the MAIN checkout, not the worktree", async () => {
    // Main checkout with committed .loom/ and a linked worktree registered.
    const main = join(root, "main");
    await mkdir(join(main, ".loom"), { recursive: true });
    await mkdir(join(main, ".git", "worktrees", "issue-1"), { recursive: true });

    // The worktree itself carries a real .loom/ (repos commit .loom/), plus a
    // `.git` FILE pointing back into the main checkout's worktrees dir.
    const wt = join(root, "wt-issue-1");
    await mkdir(join(wt, ".loom"), { recursive: true });
    await writeFile(
      join(wt, ".git"),
      `gitdir: ${join(main, ".git", "worktrees", "issue-1")}\n`,
      "utf-8"
    );

    // Must resolve to the main checkout (where .loom/config.json lives), even
    // though the worktree has its own .loom/ that a naive walk would return.
    expect(discoverWorkspaceRoot(wt)).toBe(main);
    // Also from a nested dir inside the worktree.
    const nested = join(wt, "src", "x");
    await mkdir(nested, { recursive: true });
    expect(discoverWorkspaceRoot(nested)).toBe(main);
  });

  it("handles a relative gitdir in the worktree .git file", async () => {
    const main = join(root, "main2");
    await mkdir(join(main, ".git", "worktrees", "wt"), { recursive: true });
    const wt = join(root, "wt2");
    await mkdir(wt, { recursive: true });
    // Relative gitdir, resolved against the worktree dir.
    await writeFile(join(wt, ".git"), "gitdir: ../main2/.git/worktrees/wt\n", "utf-8");
    expect(discoverWorkspaceRoot(wt)).toBe(main);
  });

  it("returns null for a non-repo directory (loud-failure signal, no silent fallback)", async () => {
    const plain = join(root, "not-a-repo", "sub");
    await mkdir(plain, { recursive: true });
    // No .loom/ or .git anywhere under the temp root.
    expect(discoverWorkspaceRoot(plain)).toBeNull();
  });
});

describe("getWorkspacePath", () => {
  it("returns LOOM_WORKSPACE verbatim when set (highest precedence)", () => {
    process.env.LOOM_WORKSPACE = "/explicit/override/path";
    // CWD is irrelevant when the override is present.
    vi.spyOn(process, "cwd").mockReturnValue(join(root, "somewhere"));
    expect(getWorkspacePath()).toBe("/explicit/override/path");
  });

  it("ignores an empty LOOM_WORKSPACE and falls through to discovery", async () => {
    const repo = join(root, "repoEmpty");
    await mkdir(join(repo, ".loom"), { recursive: true });
    process.env.LOOM_WORKSPACE = "";
    vi.spyOn(process, "cwd").mockReturnValue(repo);
    expect(getWorkspacePath()).toBe(repo);
  });

  it("discovers the workspace from CWD when LOOM_WORKSPACE is unset", async () => {
    const repo = join(root, "repoCwd");
    const nested = join(repo, "a", "b");
    await mkdir(join(repo, ".loom"), { recursive: true });
    await mkdir(nested, { recursive: true });
    vi.spyOn(process, "cwd").mockReturnValue(nested);
    expect(getWorkspacePath()).toBe(repo);
  });

  it("throws loudly when no workspace can be resolved (no ~/GitHub/loom fallback)", async () => {
    const plain = join(root, "orphan");
    await mkdir(plain, { recursive: true });
    vi.spyOn(process, "cwd").mockReturnValue(plain);
    // Silence the intentional stderr warning during the assertion.
    const errSpy = vi.spyOn(process.stderr, "write").mockReturnValue(true);
    expect(() => getWorkspacePath()).toThrow(/Could not resolve a Loom workspace/);
    expect(errSpy).toHaveBeenCalled();
  });
});
