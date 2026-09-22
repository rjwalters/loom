//! A stdio MCP server that fronts the guarded `loom_*` tool surface (#8562).
//!
//! This is a **protocol binding only**, exactly like `pi.ts` and
//! `opencode.mjs`: every `tools/call` is normalised into a
//! [`super::Request`] and handed to [`super::execute`], so an MCP client
//! reaches the same guard bridge, worktree policy, destructive-command
//! policy, mutation lock, bounded executor and 64 KiB truncation as the Pi
//! and OpenCode bindings. No policy decision is made in this file.
//!
//! It exists because Kimi Code CLI has no extension/plugin mechanism a host
//! can point at a local file (Pi's `--extension`, OpenCode's `plugins/`);
//! its only route for adding tools is an MCP server entry in
//! `$KIMI_CODE_HOME/mcp.json`. Nothing here is Kimi-specific, though — any
//! MCP-capable harness can be bound the same way.
//!
//! Transport: newline-delimited JSON-RPC 2.0 on stdin/stdout, one message
//! per line. Notifications (no `id`) are consumed without a reply.
use super::{Request, ToolArgs};
use anyhow::Result;
use serde_json::{json, Value};
use std::io::{BufRead, Write};

/// The MCP protocol revision this server implements. An `initialize` naming
/// a different revision still gets a successful response carrying this
/// value — the spec's negotiation contract — rather than an error.
pub const PROTOCOL_VERSION: &str = "2025-06-18";

/// The four guarded tools, in the order `tools/list` reports them. The names
/// are the same ones Pi and OpenCode expose, so `launch_outcome`'s
/// `loom_*` classification (#8448) sees one shape across every native
/// harness. A harness that namespaces MCP tools (Kimi: `mcp__<server>__`)
/// prefixes these; the classifier strips that prefix.
const TOOLS: &[(&str, &str)] = &[
    ("loom_read", "Read a UTF-8 file, with optional 1-based offset and line limit"),
    ("loom_write", "Write a file inside a Loom-managed worktree"),
    ("loom_edit", "Replace exactly one oldText occurrence; read the file first"),
    (
        "loom_bash",
        "Run a shell command through Loom policy; use cd for worktree commands",
    ),
];

/// The tool-name prefix a harness that namespaces MCP tools applies. Public
/// so the generated Kimi allowlists and the toolless-launch classifier key
/// on one definition rather than three copies of the spelling.
pub const MCP_NAME_PREFIX: &str = "mcp__";

#[derive(clap::Args)]
pub struct McpArgs {
    #[command(flatten)]
    pub tool: ToolArgs,
    /// Run one `PreToolUse` hook check from stdin and exit, instead of
    /// serving MCP. Defense-in-depth only: the harness hook surface this
    /// serves is fail-OPEN (a crashed or timed-out hook allows the call),
    /// so it is never the enforcement boundary — the tool allowlists are.
    #[arg(long)]
    pub pretooluse_guard: bool,
}

fn input_schema(tool: &str) -> Value {
    match tool {
        "loom_read" => json!({
            "type":"object",
            "properties":{
                "path":{"type":"string"},
                "offset":{"type":"number"},
                "limit":{"type":"number"}
            },
            "required":["path"],
            "additionalProperties":false
        }),
        "loom_write" => json!({
            "type":"object",
            "properties":{"path":{"type":"string"},"content":{"type":"string"}},
            "required":["path","content"],
            "additionalProperties":false
        }),
        "loom_edit" => json!({
            "type":"object",
            "properties":{
                "path":{"type":"string"},
                "oldText":{"type":"string"},
                "newText":{"type":"string"}
            },
            "required":["path","oldText","newText"],
            "additionalProperties":false
        }),
        _ => json!({
            "type":"object",
            "properties":{
                "command":{"type":"string"},
                "timeout":{"type":"number","minimum":1,"maximum":600}
            },
            "required":["command"],
            "additionalProperties":false
        }),
    }
}

/// The exact `tools/list` payload. Exposed so a test can assert the served
/// surface is these four tools and nothing else.
pub fn tool_specs() -> Vec<Value> {
    TOOLS
        .iter()
        .map(|(name, description)| {
            json!({"name":name,"description":description,"inputSchema":input_schema(name)})
        })
        .collect()
}

fn error(id: &Value, code: i64, message: &str) -> Value {
    json!({"jsonrpc":"2.0","id":id,"error":{"code":code,"message":message}})
}
fn success(id: &Value, result: Value) -> Value {
    json!({"jsonrpc":"2.0","id":id,"result":result})
}

/// Handle one decoded JSON-RPC message. Returns `None` for a notification
/// (no `id`), which per JSON-RPC must not be answered.
///
/// Deliberately total: every unrecognised method is an error response, never
/// a fallthrough that could invent a second execution path.
pub fn handle(args: &ToolArgs, message: &Value) -> Option<Value> {
    let id = message.get("id").cloned();
    let Some(method) = message.get("method").and_then(Value::as_str) else {
        return Some(error(
            id.as_ref().unwrap_or(&Value::Null),
            -32600,
            "invalid request: no method",
        ));
    };
    // A notification carries no id and takes no reply — including
    // `notifications/initialized`, which every MCP client sends.
    let id = id?;
    Some(match method {
        "initialize" => success(
            &id,
            json!({
                "protocolVersion":PROTOCOL_VERSION,
                "capabilities":{"tools":{"listChanged":false}},
                "serverInfo":{"name":"loom","version":env!("CARGO_PKG_VERSION")}
            }),
        ),
        "ping" => success(&id, json!({})),
        "tools/list" => success(&id, json!({"tools":tool_specs()})),
        "tools/call" => call(args, &id, message.get("params")),
        _ => error(&id, -32601, "method not found"),
    })
}

fn call(args: &ToolArgs, id: &Value, params: Option<&Value>) -> Value {
    let Some(name) = params.and_then(|p| p.get("name")).and_then(Value::as_str) else {
        return error(id, -32602, "tools/call requires a string name");
    };
    // An unknown tool is a protocol error, not a tool result: there is no
    // fallback surface for delegation or an unverified harness tool.
    let Some(tool) = TOOLS
        .iter()
        .find(|(known, _)| *known == name)
        .map(|(known, _)| known.trim_start_matches("loom_"))
    else {
        return error(id, -32602, "unknown tool; delegation and unverified tools are disabled");
    };
    let input = params
        .and_then(|p| p.get("arguments"))
        .cloned()
        .unwrap_or_else(|| json!({}));
    if !input.is_object() {
        return error(id, -32602, "tools/call arguments must be an object");
    }
    let request = Request {
        tool: tool.to_string(),
        input,
    };
    // A refusal is a TOOL result with `isError`, not a transport error: the
    // model has to see why Loom said no in order to change course, which is
    // the same contract the Pi/OpenCode bindings give it.
    match super::execute(args, &request) {
        Ok(text) => success(id, json!({"content":[{"type":"text","text":text}],"isError":false})),
        Err(failure) => success(
            id,
            json!({"content":[{"type":"text","text":failure.to_string()}],"isError":true}),
        ),
    }
}

/// Serve MCP on stdin/stdout until the client closes the stream.
///
/// # Errors
///
/// Propagates a signal-handler installation failure or an unrecoverable
/// stdin/stdout I/O failure. A malformed line is answered with a JSON-RPC
/// parse error and does not end the session.
pub fn serve(args: McpArgs) -> Result<()> {
    if args.pretooluse_guard {
        return pretooluse_guard();
    }
    super::cancellation::install()?;
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    for line in stdin.lock().lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let response = match serde_json::from_str::<Value>(&line) {
            // A JSON-RPC batch is not supported: the surface stays one
            // message per line so there is exactly one execution path.
            Ok(Value::Array(_)) => {
                Some(error(&Value::Null, -32600, "batch requests are not supported"))
            }
            Ok(message) => handle(&args.tool, &message),
            Err(_) => Some(error(&Value::Null, -32700, "parse error")),
        };
        if let Some(response) = response {
            writeln!(stdout, "{response}")?;
            stdout.flush()?;
        }
    }
    Ok(())
}

/// One fail-open `PreToolUse` hook check: deny any tool call whose name is
/// not one of this server's namespaced tools.
///
/// This adds nothing to the guarantee — the harness treats a crashed,
/// timed-out or non-2 exit as ALLOW, so it can only ever narrow, never
/// widen, what the allowlists already decided. It is here so a configuration
/// regression that re-enables a builtin tool is still visible/blocked at
/// runtime rather than silently effective.
fn pretooluse_guard() -> Result<()> {
    use std::io::Read as _;
    let mut raw = String::new();
    // Bounded: a hook payload carries the tool input, which is already
    // capped upstream; refuse rather than buffer an unbounded stream.
    std::io::stdin()
        .take(1024 * 1024)
        .read_to_string(&mut raw)
        .ok();
    let name = serde_json::from_str::<Value>(&raw)
        .ok()
        .and_then(|payload| {
            payload
                .get("toolName")
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .unwrap_or_default();
    if name.starts_with(MCP_NAME_PREFIX) && name.contains("__loom_") {
        return Ok(());
    }
    println!(
        "{}",
        json!({"hookSpecificOutput":{
            "hookEventName":"PreToolUse",
            "permissionDecision":"deny",
            "permissionDecisionReason":format!("Loom guarded launch: {name:?} is not a guarded loom_* tool")
        }})
    );
    // 2 is the harness's "block" exit code; every other nonzero exit is
    // treated as allow, which is precisely why this is not the boundary.
    std::process::exit(2);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn args() -> ToolArgs {
        ToolArgs {
            workspace: PathBuf::from("/nonexistent"),
            cwd: PathBuf::from("/nonexistent"),
        }
    }

    #[test]
    fn tools_list_serves_exactly_the_four_guarded_tools() {
        let response = handle(&args(), &json!({"jsonrpc":"2.0","id":1,"method":"tools/list"}))
            .expect("a request is answered");
        let tools = response["result"]["tools"].as_array().unwrap().clone();
        let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert_eq!(names, ["loom_read", "loom_write", "loom_edit", "loom_bash"]);
        for tool in &tools {
            assert!(tool["description"].is_string());
            assert_eq!(tool["inputSchema"]["additionalProperties"], json!(false));
        }
    }

    #[test]
    fn initialize_answers_with_the_tool_capability_and_ping_is_empty() {
        let response = handle(&args(), &json!({"jsonrpc":"2.0","id":0,"method":"initialize"}))
            .expect("a request is answered");
        assert_eq!(response["result"]["protocolVersion"], json!(PROTOCOL_VERSION));
        assert_eq!(response["result"]["capabilities"]["tools"]["listChanged"], json!(false));
        assert_eq!(response["result"]["serverInfo"]["name"], json!("loom"));
        let ping = handle(&args(), &json!({"jsonrpc":"2.0","id":7,"method":"ping"})).unwrap();
        assert_eq!(ping["result"], json!({}));
    }

    #[test]
    fn a_notification_is_never_answered() {
        assert!(handle(&args(), &json!({"jsonrpc":"2.0","method":"notifications/initialized"}))
            .is_none());
    }

    #[test]
    fn unknown_method_and_unknown_tool_are_errors_not_a_second_execution_path() {
        let unknown =
            handle(&args(), &json!({"jsonrpc":"2.0","id":1,"method":"resources/list"})).unwrap();
        assert_eq!(unknown["error"]["code"], json!(-32601));
        for name in ["task", "bash", "Bash", "loom_delegate", "loom_read_extra"] {
            let response = handle(
                &args(),
                &json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":name}}),
            )
            .unwrap();
            assert_eq!(response["error"]["code"], json!(-32602), "{name} must be refused");
            assert!(response.get("result").is_none(), "{name} must not produce a result");
        }
    }

    #[test]
    fn malformed_tools_call_params_are_refused_before_any_execution() {
        for params in [
            json!({}),
            json!({"name":123}),
            json!({"name":"loom_read","arguments":[]}),
        ] {
            let response = handle(
                &args(),
                &json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":params}),
            )
            .unwrap();
            assert_eq!(response["error"]["code"], json!(-32602), "{params}");
        }
    }

    #[test]
    fn a_missing_guard_refuses_the_call_as_a_tool_error_not_a_success() {
        // `workspace`/`cwd` do not exist, so the guard bridge cannot be
        // found or run. The call must come back `isError`, never as a
        // success with empty text.
        let response = handle(
            &args(),
            &json!({"jsonrpc":"2.0","id":4,"method":"tools/call","params":{
                "name":"loom_read","arguments":{"path":"anything"}}}),
        )
        .unwrap();
        assert_eq!(response["result"]["isError"], json!(true));
        assert!(!response["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .is_empty());
    }

    #[test]
    fn a_request_without_a_method_is_an_invalid_request() {
        let response = handle(&args(), &json!({"jsonrpc":"2.0","id":5})).unwrap();
        assert_eq!(response["error"]["code"], json!(-32600));
    }
}
