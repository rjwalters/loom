// Protocol binding only. Policy and execution stay in the native binary.
import { Type } from "typebox";
import { execFile } from "node:child_process";
export default function (pi) {
  const workspace = process.env.LOOM_WORKSPACE;
  const cwd = process.cwd();
  const binary = process.env.LOOM_NATIVE_TOOL_BIN;
  if (!workspace || !binary) throw new Error("Loom native tool context is missing");
  const specs = {
    read: ["Read a UTF-8 file, with optional 1-based offset and line limit", { path: Type.String(), offset: Type.Optional(Type.Number()), limit: Type.Optional(Type.Number()) }],
    write: ["Write a file inside a Loom-managed worktree", { path: Type.String(), content: Type.String() }],
    edit: ["Replace exactly one oldText occurrence; read the file first", { path: Type.String(), oldText: Type.String(), newText: Type.String() }],
    bash: ["Run a shell command through Loom policy; use cd for worktree commands", { command: Type.String(), timeout: Type.Optional(Type.Number({ minimum: 1, maximum: 600 })) }],
  };
  for (const [name, [description, properties]] of Object.entries(specs)) {
    pi.registerTool({
      name: `loom_${name}`, label: `Loom ${name}`, description,
      parameters: Type.Object(properties),
      async execute(_id, input, signal) {
        const text = await new Promise((resolve, reject) => {
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
        return { content: [{ type: "text", text }], details: {} };
      },
    });
  }
}
