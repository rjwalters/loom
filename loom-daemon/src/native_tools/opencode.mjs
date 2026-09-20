// No policy or shell interpolation in the harness binding.
import { tool } from "@opencode-ai/plugin";
import { execFile } from "node:child_process";
export default async function () {
  const workspace = process.env.LOOM_WORKSPACE;
  const cwd = process.cwd();
  const binary = process.env.LOOM_NATIVE_TOOL_BIN;
  if (!workspace || !binary) throw new Error("Loom native tool context is missing");
  const specs = {
    read: ["Read a UTF-8 file, with optional 1-based offset and line limit", { path: tool.schema.string(), offset: tool.schema.number().optional(), limit: tool.schema.number().optional() }],
    write: ["Write a file inside a Loom-managed worktree", { path: tool.schema.string(), content: tool.schema.string() }],
    edit: ["Replace exactly one oldText occurrence; read the file first", { path: tool.schema.string(), oldText: tool.schema.string(), newText: tool.schema.string() }],
    bash: ["Run a shell command through Loom policy; use cd for worktree commands", { command: tool.schema.string(), timeout: tool.schema.number().min(1).max(600).optional() }],
  };
  return {
    tool: Object.fromEntries(Object.entries(specs).map(([name, [description, args]]) => [`loom_${name}`, tool({
      description, args,
      async execute(input, context) {
        return await new Promise((resolve, reject) => {
          const child = execFile(binary, ["runtime-tool", "--workspace", workspace, "--cwd", cwd],
            { timeout: 625000, maxBuffer: 1024 * 1024, signal: context.abort }, (error, stdout) => {
              try {
                const result = JSON.parse(stdout);
                if (error || result.error || typeof result.text !== "string") reject(new Error(result.error || "Loom tool failed"));
                else resolve(result.text);
              } catch { reject(new Error("Loom native tool failed without a valid response")); }
            });
          child.stdin.on("error", () => {});
          child.stdin.end(JSON.stringify({ tool: name, input }));
        });
      },
    })])),
  };
}
