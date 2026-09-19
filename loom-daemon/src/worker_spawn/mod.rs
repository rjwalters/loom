//! Native runtime dispatch behind the existing spawn-worker.sh invocation contract.
//! Unix exec keeps PID, signal semantics and streaming intact; the daemon remains
//! responsible for deadlines/process-group teardown. Models are profiles, not adapters.
mod harness;
mod profiles;
mod prompt;

use harness::Harness;
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    process::Command,
};

#[derive(clap::Args)]
pub struct WorkerArgs {
    /// Adapter directory supplied by the compatibility stub.
    #[arg(long)]
    pub scripts_dir: Option<PathBuf>,
    /// Worker options, forwarded unchanged to legacy adapters. Use -- before them.
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    pub args: Vec<String>,
}

#[derive(Default, Debug)]
pub struct Options {
    pub prompt: Option<String>,
    pub model: Option<String>,
    pub profile: Option<String>,
    pub effort: Option<String>,
    pub log: Option<PathBuf>,
    pub skip_permissions: bool,
}
impl Options {
    fn parse(args: &[String]) -> Result<Self, LaunchError> {
        let mut out = Self::default();
        let mut args = args.iter();
        while let Some(arg) = args.next() {
            let (flag, inline) = arg
                .split_once('=')
                .map_or((arg.as_str(), None), |(a, b)| (a, Some(b)));
            if inline.is_none() && flag == "--use-wrapper" {
                continue;
            }
            if inline.is_none() && flag == "--dangerously-skip-permissions" {
                out.skip_permissions = true;
                continue;
            }
            if ![
                "-p",
                "--prompt",
                "--model",
                "--profile",
                "--effort",
                "--log",
            ]
            .contains(&flag)
            {
                // Never echo unknown argument values: an accidental --api-key must not leak.
                return Err(LaunchError::config("unsupported worker option; expected --prompt, --model, --profile, --effort, --log, --use-wrapper or --dangerously-skip-permissions"));
            }
            let value = inline
                .or_else(|| args.next().map(String::as_str))
                .ok_or_else(|| LaunchError::config(format!("{flag} requires a value")))?;
            if value.is_empty() {
                return Err(LaunchError::config(format!("{flag} requires a nonempty value")));
            }
            match flag {
                "-p" | "--prompt" => out.prompt = Some(value.into()),
                "--model" => out.model = Some(value.into()),
                "--profile" => out.profile = Some(value.into()),
                "--effort" => out.effort = Some(value.into()),
                "--log" => out.log = Some(value.into()),
                _ => unreachable!(),
            }
        }
        if out.prompt.is_none() {
            return Err(LaunchError::config("native worker adapters require --prompt; use the harness CLI directly for interactive sessions"));
        }
        if out.model.is_none() {
            out.model = nonempty_env("LOOM_MODEL");
        }
        if out.profile.is_none() {
            out.profile = nonempty_env("LOOM_MODEL_PROFILE");
        }
        Ok(out)
    }
}

#[derive(Debug)]
pub struct LaunchError {
    pub code: i32,
    pub message: String,
}
impl LaunchError {
    fn config(message: impl Into<String>) -> Self {
        Self {
            code: 78,
            message: message.into(),
        }
    }
}
fn nonempty_env(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.trim().is_empty())
}

pub fn is_native(runtime: &str) -> bool {
    Harness::parse(runtime).is_some()
}

fn workspace(scripts: Option<&Path>) -> Result<PathBuf, LaunchError> {
    if let Some(root) = std::env::var_os("LOOM_WORKSPACE").filter(|v| !v.is_empty()) {
        return Ok(root.into());
    }
    let mut git = Command::new("git");
    git.args(["rev-parse", "--path-format=absolute", "--git-common-dir"]);
    if let Ok(crate::proc_exec::Completion::Exited(output)) =
        crate::proc_exec::run_bounded(git, std::time::Duration::from_secs(5))
    {
        if output.status.success() {
            if let Ok(text) = std::str::from_utf8(&output.stdout) {
                if let Some(parent) = Path::new(text.trim()).parent() {
                    return Ok(parent.into());
                }
            }
        }
    }
    scripts
        .and_then(Path::parent)
        .and_then(Path::parent)
        .map(Path::to_path_buf)
        .or_else(|| std::env::current_dir().ok())
        .ok_or_else(|| LaunchError::config("cannot locate workspace"))
}

fn scripts_dir(root: &Path) -> PathBuf {
    let installed = root.join(".loom/scripts");
    if installed.is_dir() {
        installed
    } else {
        root.join("defaults/scripts")
    }
}

pub fn run(args: WorkerArgs) -> Result<(), LaunchError> {
    let root = workspace(args.scripts_dir.as_deref())?;
    let config = crate::config_resolver::resolve_effective_config(&root);
    let (runtime, source) = if let Some(role) = nonempty_env("LOOM_ROLE") {
        let (runtime, source) = crate::runtime_admission::resolve_binding(&root, &role, None)
            .map_err(|e| LaunchError::config(e.diagnostic()))?;
        (runtime, source.to_string())
    } else if let Some(runtime) = nonempty_env("LOOM_RUNTIME") {
        (runtime, "env (LOOM_RUNTIME)".to_string())
    } else if let Some(runtime) = config
        .pointer("/runtimes/default")
        .and_then(serde_json::Value::as_str)
        .filter(|s| !s.trim().is_empty())
    {
        (runtime.into(), "config (runtimes.default)".to_string())
    } else {
        ("claude".into(), "default".to_string())
    };
    if runtime.is_empty()
        || !runtime
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return Err(LaunchError::config("invalid runtime name"));
    }
    let scripts = args.scripts_dir.unwrap_or_else(|| scripts_dir(&root));
    let mut log: Box<dyn Write> = Box::new(std::io::stderr());
    let mut command = if let Some(harness) = Harness::parse(&runtime) {
        let options = Options::parse(&args.args)?;
        // A role-tagged invocation must meet the same capabilities as daemon dispatch.
        // Free-form prompts remain available for controlled, isolated experiments.
        let role = nonempty_env("LOOM_ROLE");
        let prompt_role = options
            .prompt
            .as_deref()
            .and_then(prompt::role_invocation)
            .map(|(r, _)| r);
        for role in role.as_deref().into_iter().chain(prompt_role) {
            crate::runtime_admission::resolve_and_admit(&root, role, Some(&runtime))
                .map_err(|e| LaunchError::config(e.diagnostic()))?;
        }
        let selection = profiles::select(&runtime, &options, &config)?;
        let expanded = options
            .prompt
            .as_deref()
            .map(|p| prompt::expand(&root, p))
            .transpose()?;
        let mut command = harness.command(&options, &selection, expanded.as_deref())?;
        if let Some(path) = &options.log {
            if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
                fs::create_dir_all(parent).map_err(|e| {
                    LaunchError::config(format!("cannot create log directory: {e}"))
                })?;
            }
            let file = fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .map_err(|e| LaunchError::config(format!("cannot open worker log: {e}")))?;
            command.stdout(
                file.try_clone()
                    .map_err(|e| LaunchError::config(e.to_string()))?,
            );
            command.stderr(
                file.try_clone()
                    .map_err(|e| LaunchError::config(e.to_string()))?,
            );
            log = Box::new(file);
        }
        writeln!(log, "# LOOM_LAUNCH {}", serde_json::json!({"schema":1,"runtime":runtime,"provider":selection.provider,"model":selection.model,"profile":selection.profile,"effort":selection.effort,"usage":"native-json-events","billing":"not-measured"})).map_err(|e| LaunchError::config(e.to_string()))?;
        command
    } else {
        let runner = scripts.join(format!("spawn-{runtime}.sh"));
        if !runner.is_file() {
            let available = fs::read_dir(&scripts)
                .into_iter()
                .flatten()
                .filter_map(Result::ok)
                .filter_map(|e| {
                    e.file_name()
                        .to_str()
                        .and_then(|s| s.strip_prefix("spawn-")?.strip_suffix(".sh"))
                        .map(str::to_string)
                })
                .filter(|s| s != "worker")
                .collect::<Vec<_>>();
            return Err(LaunchError::config(format!("Unknown runtime '{runtime}' (resolved from {source}): no runner found at {}. Available runtimes on disk: {}; native: pi opencode", runner.display(), available.join(" "))));
        }
        let mut command = Command::new(runner);
        command.args(&args.args);
        command
    };
    command.env("LOOM_RUNTIME", &runtime);
    // Preserve #8077 isolation defaults without repointing live IPC/token paths.
    if nonempty_env("LOOM_DAEMON_LOG").is_none() {
        command.env(
            "LOOM_DAEMON_LOG",
            std::env::temp_dir()
                .join(format!("loom-worker-isolation-{}/daemon.log", std::process::id())),
        );
    }
    if nonempty_env("LOOM_TEST_ALLOW_SYSTEMD").is_none() {
        command.env("LOOM_TEST_ALLOW_SYSTEMD", "0");
    }
    writeln!(log, "spawn-worker: runtime={runtime} (from {source})\n# LOOM_RUNTIME_RESOLVED runtime={runtime}").map_err(|e| LaunchError::config(e.to_string()))?;
    if is_native(&runtime) {
        writeln!(log, "# LOOM_CLI_START runtime={runtime}")
            .map_err(|e| LaunchError::config(e.to_string()))?;
    }
    log.flush()
        .map_err(|e| LaunchError::config(e.to_string()))?;
    exec(command)
}

#[cfg(unix)]
fn exec(mut command: Command) -> Result<(), LaunchError> {
    use std::os::unix::process::CommandExt;
    let error = command.exec();
    Err(LaunchError {
        code: if error.kind() == std::io::ErrorKind::NotFound {
            127
        } else {
            126
        },
        message: format!("cannot execute worker harness: {error}"),
    })
}
#[cfg(not(unix))]
fn exec(mut command: Command) -> Result<(), LaunchError> {
    match command.status() {
        Ok(status) => std::process::exit(status.code().unwrap_or(1)),
        Err(error) => Err(LaunchError {
            code: 127,
            message: error.to_string(),
        }),
    }
}
