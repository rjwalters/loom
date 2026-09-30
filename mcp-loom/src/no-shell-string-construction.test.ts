/**
 * Structural guard: no shell-string construction anywhere in `mcp-loom/src`
 * (Issue #9107, acceptance criterion 1).
 *
 * `get_agent_metrics` used to build its child command by interpolating raw MCP
 * tool arguments into a template literal and handing the result to
 * `child_process.exec` — i.e. to `/bin/sh -c`. The tool's `inputSchema` `enum`
 * constraints are advisory to the client (the MCP SDK does not enforce them
 * server-side), so `role: "builder; touch /tmp/pwn#"` was arbitrary command
 * execution as the user running the server.
 *
 * The fix removed the shell from the picture entirely (`execFile` with an argv
 * array). This test keeps it removed: it fails the build if any `.ts` file
 * under `src/` reintroduces a shell-interpreting spawn API, a `shell: true`
 * option, or a template literal that looks like a shell command line. It is a
 * lint, not a proof — the behavioural tests live in
 * `tools/terminals.test.ts` — but it is the cheap check that catches the
 * pattern coming back in a file nobody thought to write tests for.
 */

import { readdir, readFile } from "node:fs/promises";
import { dirname, join, relative } from "node:path";
import { fileURLToPath } from "node:url";
import { describe, expect, it } from "vitest";

const THIS_FILE = fileURLToPath(import.meta.url);
const SRC_DIR = dirname(THIS_FILE);

/**
 * Recursively collect every TypeScript source file under `src/`.
 *
 * This file itself is excluded: it is the rule definition, so its pattern table
 * and fixtures necessarily *contain* the forbidden constructs as data. Its own
 * sensitivity is covered by the "is not vacuous" case below instead.
 */
async function collectSourceFiles(dir: string): Promise<string[]> {
  const entries = await readdir(dir, { withFileTypes: true });
  const files: string[] = [];
  for (const entry of entries) {
    const full = join(dir, entry.name);
    if (entry.isDirectory()) {
      files.push(...(await collectSourceFiles(full)));
    } else if (entry.name.endsWith(".ts") && full !== THIS_FILE) {
      files.push(full);
    }
  }
  return files.sort();
}

/**
 * Strip whole-line comments so prose that merely *mentions* `exec(...)` or
 * quotes a shell snippet (including the doc comments in this very file) does
 * not trip the scanner. Inline trailing comments are left in place: a line with
 * real code on it should not be able to hide a violation behind `//`.
 */
function isCommentLine(line: string): boolean {
  const trimmed = line.trimStart();
  return trimmed.startsWith("//") || trimmed.startsWith("*") || trimmed.startsWith("/*");
}

interface ForbiddenPattern {
  name: string;
  pattern: RegExp;
  why: string;
}

const FORBIDDEN_PATTERNS: ForbiddenPattern[] = [
  {
    name: "child_process.exec / execSync",
    // `\bexec\b` does not match `execFile`/`execFileAsync` (the `F` is a word
    // character, so there is no word boundary after `exec`), so this catches
    // exactly the shell-interpreting pair — including `promisify(exec)` and a
    // bare `import { exec }`.
    pattern: /\bexec(?:Sync)?\b/,
    why: "exec/execSync run their argument through /bin/sh -c; use execFile/spawn with an argv array instead (#9107)",
  },
  {
    name: "shell: true",
    pattern: /shell\s*:\s*true/,
    why: "shell: true reintroduces /bin/sh -c interpretation of the argv array (#9107)",
  },
  {
    name: "template-literal shell command line",
    // A template literal whose content opens with a shell/interpreter word —
    // the `` `bash "${scriptPath}" ${args.join(" ")}` `` shape this issue fixed.
    pattern: /`\s*(?:\/(?:usr\/)?bin\/)?(?:bash|sh|zsh|dash|env|eval)\s/,
    why: "a shell command line built from a template literal is string-interpolated shell syntax (#9107)",
  },
  {
    name: "template literal as a spawned program path",
    pattern: /\b(?:execFile|execFileSync|spawn|spawnSync)\s*\(\s*`/,
    why: "the program argument should be a literal or a validated path, not an interpolated string (#9107)",
  },
];

describe("mcp-loom/src contains no shell-string construction (#9107)", () => {
  it("scans a non-trivial number of source files", async () => {
    const files = await collectSourceFiles(SRC_DIR);
    // Guards against the scan silently matching nothing (e.g. a refactor that
    // moves sources elsewhere) and reporting a vacuous pass.
    expect(files.length).toBeGreaterThan(5);
  });

  it("has no forbidden shell construct on any code line", async () => {
    const files = await collectSourceFiles(SRC_DIR);
    const violations: string[] = [];

    for (const file of files) {
      const contents = await readFile(file, "utf-8");
      const lines = contents.split("\n");
      lines.forEach((line, index) => {
        if (isCommentLine(line)) {
          return;
        }
        for (const { name, pattern, why } of FORBIDDEN_PATTERNS) {
          if (pattern.test(line)) {
            violations.push(
              `${relative(SRC_DIR, file)}:${index + 1}: ${name} — ${why}\n    ${line.trim()}`
            );
          }
        }
      });
    }

    expect(violations).toEqual([]);
  });

  /** Which rules fire on a given line of would-be source. */
  function matchedRules(line: string): string[] {
    return FORBIDDEN_PATTERNS.filter(({ pattern }) => pattern.test(line)).map(({ name }) => name);
  }

  it("detects the two pre-fix lines this issue removed (the scanner is not vacuous)", () => {
    const tick = String.fromCharCode(96);
    const shellWord = ["b", "ash"].join("");
    const shellExec = ["ex", "ec"].join("");

    // The pre-#9107 promisify line: `const execAsync = promisify(exec);`
    const promisifyLine = `const ${shellExec}Async = promisify(${shellExec});`;
    expect(matchedRules(promisifyLine)).toContain("child_process.exec / execSync");

    // The pre-#9107 call site, whose first argument was a shell command line
    // built by interpolation:
    //   await execAsync(`bash "${scriptPath}" ${args.join(" ")}`, …)
    const callSiteLine =
      `const { stdout } = await ${shellExec}Async(${tick}${shellWord} ` +
      '"${scriptPath}" ${args.join(" ")}';
    expect(matchedRules(callSiteLine)).toContain("template-literal shell command line");
  });

  it("detects a re-shelled argv array (shell: true)", () => {
    const line = `await ${["ex", "ec"].join("")}FileAsync("bash", argv, { shell: true });`;
    expect(matchedRules(line)).toContain("shell: true");
  });

  it("does not flag the argv-array form the fix uses", () => {
    const line = 'await execFileAsync("bash", [scriptPath, ...argv], { cwd: workspacePath });';
    expect(matchedRules(line)).toEqual([]);
  });
});
