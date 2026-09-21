//! Durable sweep completion markers; preserves the existing helper's CLI.
//! A checkpoint is a completion observation, never an inferred phase interval.
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use serde_json::{json, Value};

const PHASES: &[&str] = &[
    "curator-done",
    "builder-done",
    "judge-rejected",
    "judge-done",
    "doctor-done",
    "merge-done",
];

#[derive(clap::Args)]
pub(crate) struct SweepCheckpointArgs {
    /// write/read/phase/attempt/model/exists/delete/list, followed by the legacy arguments.
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    args: Vec<String>,
}

impl SweepCheckpointArgs {
    pub(crate) fn run(self) -> anyhow::Result<()> {
        let cwd = std::env::current_dir()?;
        let root = repo_root(&cwd);
        let code = match execute(&root, &self.args, &mut std::io::stdout()) {
            Ok(()) => 0,
            Err((code, message)) => {
                if !message.is_empty() {
                    eprintln!("ERROR: {message}");
                }
                code
            }
        };
        std::process::exit(code)
    }
}

fn repo_root(cwd: &Path) -> PathBuf {
    std::process::Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .current_dir(cwd)
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .map(|root| PathBuf::from(root.trim_end()))
        .unwrap_or_else(|| cwd.to_owned())
}

type Result<T> = std::result::Result<T, (i32, String)>;
fn io_error(error: impl std::fmt::Display) -> (i32, String) {
    (3, error.to_string())
}
fn invalid(message: &str) -> (i32, String) {
    (1, message.to_owned())
}
fn number(value: &str) -> Result<u32> {
    if value.is_empty() || !value.bytes().all(|b| b.is_ascii_digit()) {
        return Err(invalid("expected an unsigned integer"));
    }
    value.parse().map_err(|_| invalid("integer out of range"))
}

fn execute(root: &Path, args: &[String], out: &mut impl Write) -> Result<()> {
    let command = args.first().map(String::as_str).unwrap_or("");
    let dir = root.join(".loom/sweep-checkpoint");
    if command == "list" {
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(io_error(error)),
        };
        let mut issues = Vec::new();
        for entry in entries {
            let entry = entry.map_err(io_error)?;
            if !entry.file_type().map_err(io_error)?.is_file() {
                continue;
            }
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            if let Some(issue) = name
                .strip_prefix("issue-")
                .and_then(|s| s.strip_suffix(".json"))
            {
                if let Ok(n) = number(issue) {
                    issues.push(n);
                }
            }
        }
        issues.sort_unstable();
        for issue in issues {
            writeln!(out, "{issue}").map_err(io_error)?;
        }
        return Ok(());
    }
    if !matches!(command, "write" | "read" | "phase" | "attempt" | "model" | "exists" | "delete") {
        return Err(invalid("usage: sweep-checkpoint write ISSUE PHASE [--task-id ID] [--pr-number N] [--attempt N] [--model M]; read/phase/attempt/model/exists/delete ISSUE; list"));
    }
    let issue_text = args.get(1).ok_or_else(|| invalid("issue is required"))?;
    let issue = number(issue_text)?;
    let target = dir.join(format!("issue-{issue_text}.json"));
    if command == "write" {
        let record = parse_write(&args[2..])?;
        persist(&dir, &target, &record)?;
        // Telemetry remains best effort, and only follows a successful durable write.
        loom_daemon::observability::lifecycle::checkpoint_completed(
            root,
            issue,
            record["phase"].as_str().unwrap_or_default(),
            record["attempt"].as_u64().map(|n| n as u32),
            record["model"].as_str(),
            record["pr_number"].as_u64().map(|n| n as u32),
        );
        writeln!(
            out,
            "wrote {} (phase={})",
            target.display(),
            record["phase"].as_str().unwrap_or_default()
        )
        .map_err(io_error)?;
        return Ok(());
    }
    if command == "delete" {
        return match std::fs::remove_file(&target) {
            Ok(()) => {
                std::fs::File::open(&dir)
                    .and_then(|f| f.sync_all())
                    .map_err(io_error)?;
                writeln!(out, "deleted {}", target.display()).map_err(io_error)
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(io_error(error)),
        };
    }
    let file = match std::fs::File::open(&target) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return if matches!(command, "read" | "exists") {
                Err((1, String::new()))
            } else {
                Ok(())
            };
        }
        Err(error) => return Err(io_error(error)),
    };
    if command == "exists" {
        return Ok(());
    }
    let mut bytes = Vec::new();
    file.take(65537).read_to_end(&mut bytes).map_err(io_error)?;
    if bytes.len() > 65536 {
        return Err(io_error("checkpoint exceeds 64 KiB"));
    }
    if command == "read" {
        return out.write_all(&bytes).map_err(io_error);
    }
    let record: Value = serde_json::from_slice(&bytes).map_err(io_error)?;
    match &record[command] {
        Value::String(value) => writeln!(out, "{value}").map_err(io_error),
        Value::Number(value) => writeln!(out, "{value}").map_err(io_error),
        _ => Ok(()),
    }
}

fn parse_write(args: &[String]) -> Result<Value> {
    let phase = args.first().map(String::as_str).unwrap_or("");
    if !PHASES.contains(&phase) {
        return Err((2, format!("invalid phase '{phase}'")));
    }
    let mut record = json!({
        "phase": phase,
        "task_id": format!("sweep-run-fallback-{}", std::process::id()),
        "timestamp": chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        "pr_number": null,
    });
    let mut options = args[1..].iter();
    while let Some(flag) = options.next() {
        let value = options
            .next()
            .ok_or_else(|| invalid("option requires a value"))?;
        match flag.as_str() {
            "--task-id" if !value.is_empty() => record["task_id"] = json!(value),
            "--task-id" => {}
            "--pr-number" if value == "null" => record["pr_number"] = Value::Null,
            "--pr-number" => record["pr_number"] = json!(number(value)?),
            "--attempt" if value.is_empty() => {}
            "--attempt" => {
                let attempt = number(value)?;
                if attempt == 0 {
                    return Err(invalid("--attempt must be >= 1"));
                }
                record["attempt"] = json!(attempt);
            }
            "--model" if value.is_empty() => {}
            "--model" => {
                if !value
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
                {
                    return Err(invalid("--model must match [A-Za-z0-9._-]+"));
                }
                record["model"] = json!(value);
            }
            _ => return Err(invalid("unknown option")),
        }
    }
    if phase == "judge-rejected" && record["pr_number"].is_null() {
        return Err(invalid("judge-rejected requires --pr-number for resume routing"));
    }
    Ok(record)
}

fn persist(dir: &Path, target: &Path, record: &Value) -> Result<()> {
    let mut bytes = serde_json::to_vec_pretty(record).map_err(io_error)?;
    bytes.push(b'\n');
    if bytes.len() > 65536 {
        return Err(invalid("checkpoint exceeds 64 KiB"));
    }
    std::fs::create_dir_all(dir).map_err(io_error)?;
    let mut temp = tempfile::NamedTempFile::new_in(dir).map_err(io_error)?;
    temp.write_all(&bytes).map_err(io_error)?;
    temp.as_file().sync_all().map_err(io_error)?;
    temp.persist(target).map_err(io_error)?;
    std::fs::File::open(dir)
        .and_then(|file| file.sync_all())
        .map_err(io_error)
}

#[cfg(test)]
mod tests;
