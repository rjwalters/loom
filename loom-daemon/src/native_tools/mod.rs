//! A deliberately small tool surface shared by headless native runtimes.
//! Harness bindings have no policy: each call reaches the existing Loom guards
//! before execution. Unguarded harness tools are disabled independently of loading.
mod cancellation;
mod files;
mod guard;
pub mod provision;

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use serde_json::{json, Value};
use std::{io::Read, path::PathBuf, process::Command, time::Duration};

#[derive(clap::Args)]
pub struct ToolArgs {
    #[arg(long)]
    pub workspace: PathBuf,
    #[arg(long)]
    pub cwd: PathBuf,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub tool: String,
    pub input: Value,
}

pub fn cli(args: ToolArgs) -> Result<()> {
    let result = (|| {
        cancellation::install()?;
        let mut raw = String::new();
        std::io::stdin()
            .take(8 * 1024 * 1024 + 1)
            .read_to_string(&mut raw)?;
        if raw.len() > 8 * 1024 * 1024 {
            bail!("tool request exceeds 8 MiB");
        }
        let request: Request = serde_json::from_str(&raw).context("invalid tool request")?;
        execute(&args, &request)
    })();
    match result {
        Ok(text) => println!("{}", json!({"text":text})),
        Err(error) => {
            println!("{}", json!({"error":error.to_string()}));
            std::process::exit(78);
        }
    }
    Ok(())
}

pub fn execute(args: &ToolArgs, request: &Request) -> Result<String> {
    use crate::{
        observability::lifecycle,
        telemetry::trace::{SpanName, SpanStatus},
    };
    let tool = match request.tool.as_str() {
        "read" => "read",
        "write" => "write",
        "edit" => "edit",
        "bash" => "bash",
        _ => "unsupported",
    };
    let span = lifecycle::inherited(
        &args.workspace,
        SpanName::Tool,
        lifecycle::attributes(&[("loom.tool.name", tool)]),
    );
    let result = execute_inner(args, request, span.as_ref());
    if let Some(span) = span {
        span.finish_linked();
        span.finish(
            if result.is_ok() {
                "success"
            } else if cancellation::requested() {
                "cancelled"
            } else if result
                .as_ref()
                .err()
                .is_some_and(|error| error.is::<ToolTimeout>())
            {
                "timeout"
            } else {
                "failure"
            },
            if result.is_ok() {
                SpanStatus::Ok
            } else {
                SpanStatus::Error
            },
        );
    }
    result
}

fn execute_inner(
    args: &ToolArgs,
    request: &Request,
    span: Option<&crate::observability::lifecycle::Span>,
) -> Result<String> {
    let cwd = args
        .cwd
        .canonicalize()
        .context("tool working directory does not exist")?;
    guard::check(&args.workspace, &cwd, request)?;
    if cancellation::requested() {
        bail!("tool cancelled");
    }
    match request.tool.as_str() {
        "read" | "write" | "edit" => {
            let directory = args.workspace.join(".loom/native-tools");
            std::fs::create_dir_all(&directory)?;
            let _lock =
                crate::tokens_pool::locking::MkdirLock::acquire(&directory.join("file.lock"))
                    .map_err(anyhow::Error::msg)?;
            files::execute(&cwd, request)
        }
        "bash" => {
            let command = field(&request.input, "command")?;
            let seconds = request
                .input
                .get("timeout")
                .and_then(Value::as_u64)
                .unwrap_or(120)
                .clamp(1, 600);
            let mut child = Command::new("bash");
            child.args(["-c", command]).current_dir(cwd);
            if let Some(span) = span {
                span.command(&mut child);
            }
            let output = crate::proc_exec::run_bounded_cancellable(
                child,
                Duration::from_secs(seconds),
                cancellation::requested,
            )?;
            match output {
                crate::proc_exec::Completion::Exited(output) => {
                    let text = format!(
                        "exit={}\n{}{}",
                        output.status,
                        String::from_utf8_lossy(&output.stdout),
                        String::from_utf8_lossy(&output.stderr)
                    );
                    let text = truncate(text);
                    if !output.status.success() {
                        bail!("{text}");
                    }
                    Ok(text)
                }
                crate::proc_exec::Completion::TimedOut { .. } => Err(ToolTimeout.into()),
            }
        }
        _ => bail!("unsupported native tool"),
    }
}
#[derive(Debug)]
struct ToolTimeout;
impl std::fmt::Display for ToolTimeout {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("command timed out; its process group was terminated")
    }
}
impl std::error::Error for ToolTimeout {}

fn field<'a>(input: &'a Value, name: &str) -> Result<&'a str> {
    input
        .get(name)
        .and_then(Value::as_str)
        .with_context(|| format!("missing string field {name}"))
}
fn truncate(mut text: String) -> String {
    let mut end = 64 * 1024;
    if text.len() > end {
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
        text.push_str("\n[output truncated at 64 KiB; redirect large results to an artifact]");
    }
    text
}
