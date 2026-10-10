// No policy or shell interpolation in the harness binding.
import { tool } from "@opencode-ai/plugin";
import { execFile } from "node:child_process";
import { writeFileSync } from "node:fs";
// One module, two loader contracts, both read from upstream source:
// - 1.18.x (readV1Plugin, v1.18.31): default-exported object with a `server`
//   function; unknown keys such as `setup` are ignored.
// - 2.x (core/src/plugin/module.ts, v2.0.18): default-exported `{ id, effect }`
//   or `{ id, setup }`; `server` is not accepted, so `setup` registers the
//   same four tools through `context.tool.transform`.
// Neither path has been exercised against a live harness here (#11308).
function bind() {
  // Plugin-load receipt for the provider-free readiness probe (#8600): written
  // before anything can throw, only when the probe names a path, and never in a
  // production launch (the variable is absent there). A CLI that never loads
  // this binding cannot produce it, which is what makes it evidence.
  const receipt = process.env.LOOM_NATIVE_READINESS_RECEIPT;
  if (receipt) { try { writeFileSync(receipt, "loaded"); } catch {} }
  const workspace = process.env.LOOM_WORKSPACE;
  const cwd = process.cwd();
  const binary = process.env.LOOM_NATIVE_TOOL_BIN;
  if (!workspace || !binary) throw new Error("Loom native tool context is missing");
  return (name, input, signal) => new Promise((resolve, reject) => {
    const child = execFile(binary, ["runtime-tool", "--workspace", workspace, "--cwd", cwd],
      { timeout: 625000, maxBuffer: 1024 * 1024, signal }, (error, stdout) => {
        try {
          const result = JSON.parse(stdout);
          if (error || result.error || typeof result.text !== "string") reject(new Error(result.error || "Loom tool failed"));
          else resolve(result.text);
        } catch { reject(new Error("Loom native tool failed without a valid response")); }
      });
    child.stdin.on("error", () => {});
    child.stdin.end(JSON.stringify({ tool: name, input }));
  });
}
const str = { type: "string" };
const num = { type: "number" };
const specs = {
  read: ["Read a UTF-8 file, with optional 1-based offset and line limit", { path: str, offset: num, limit: num }, ["path"]],
  write: ["Write a file inside a Loom-managed worktree", { path: str, content: str }, ["path", "content"]],
  edit: ["Replace exactly one oldText occurrence; read the file first", { path: str, oldText: str, newText: str }, ["path", "oldText", "newText"]],
  bash: ["Run a shell command through Loom policy; use cd for worktree commands", { command: str, timeout: { type: "number", minimum: 1, maximum: 600 } }, ["command"]],
};
async function server() {
  const run = bind();
  const z = tool.schema;
  const args = {
    read: { path: z.string(), offset: z.number().optional(), limit: z.number().optional() },
    write: { path: z.string(), content: z.string() },
    edit: { path: z.string(), oldText: z.string(), newText: z.string() },
    bash: { command: z.string(), timeout: z.number().min(1).max(600).optional() },
  };
  return {
    tool: Object.fromEntries(Object.entries(specs).map(([name, [description]]) => [`loom_${name}`, tool({
      description, args: args[name],
      async execute(input, context) { return await run(name, input, context.abort); },
    })])),
  };
}
async function setup(context) {
  const run = bind();
  await context.tool.transform((editor) => {
    for (const [name, [description, properties, required]] of Object.entries(specs)) {
      editor.add({
        name: `loom_${name}`, description,
        input: { type: "object", properties, required },
        async execute(input, ctx) { return { content: await run(name, input, ctx.signal) }; },
      });
    }
  });
}
export default { id: "loom", server, setup };
