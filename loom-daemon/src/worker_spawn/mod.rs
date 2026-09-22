//! Native runtime dispatch behind the existing spawn-worker.sh invocation contract.
//! Unix exec keeps PID, signal semantics and streaming intact; the daemon remains
//! responsible for deadlines/process-group teardown. Models are profiles, not adapters.
pub mod containment;
// `pub(crate)` rather than private since #8436: `runtime_preference::
// availability` mirrors `credential::resolve`'s ladder to decide whether a
// native tap's API-key pool is the wall, and must read the profile the launch
// would actually use rather than re-deriving one.
pub(crate) mod credential;
mod harness;
pub mod launch_outcome;
mod opencode_version;
mod profile_check;
pub(crate) mod profiles;
mod prompt;

pub use profile_check::{cli as profile_cli, WorkerArgs as WorkerCommand};

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
    pub args: Vec<std::ffi::OsString>,
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
    fn parse(args: &[std::ffi::OsString]) -> Result<Self, LaunchError> {
        let mut out = Self::default();
        let mut args = args.iter();
        while let Some(arg) = args.next() {
            let arg = arg
                .to_str()
                .ok_or_else(|| LaunchError::config("native worker options must be UTF-8"))?;
            let (flag, inline) = arg
                .split_once('=')
                .map_or((arg, None), |(a, b)| (a, Some(b)));
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
                .or_else(|| args.next().and_then(|s| s.to_str()))
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
    /// A pre-exec failure discovered one step before `exec` would have hit
    /// the same wall itself — the oversized-prompt check in `prompt.rs`
    /// (issue #8506) is the first caller. Carries exit code `126`, matching
    /// `exec`'s own "cannot execute" code (below), rather than the `78`
    /// (`EX_CONFIG`) an ordinary startup-config error gets: this failure
    /// class is "the launch could never have succeeded", identical in kind
    /// to a genuine OS `exec` failure, just caught earlier with a much
    /// clearer message. `role_runner` and any backoff must be able to treat
    /// the two identically.
    fn launch_failure(message: impl Into<String>) -> Self {
        Self {
            code: 126,
            message: message.into(),
        }
    }
}
fn nonempty_env(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.trim().is_empty())
}

/// Native sweep workers authenticate through their harness, not Claude's pool.
pub fn uses_native_sweep(root: &Path) -> bool {
    crate::runtime_admission::resolve_binding(root, "sweep-lifecycle", None)
        .is_ok_and(|(runtime, _)| is_native(&runtime))
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

/// Point `command`'s stdout/stderr at the `--log` file (creating its directory
/// if needed) and hand back a writer onto the SAME destination, so the
/// dispatcher's own marker lines interleave with the worker's output instead
/// of splitting across two sinks. With no `--log`, both stay on stderr.
fn attach_log(command: &mut Command, path: Option<&Path>) -> Result<Box<dyn Write>, LaunchError> {
    let Some(path) = path else {
        return Ok(Box::new(std::io::stderr()));
    };
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        fs::create_dir_all(parent)
            .map_err(|e| LaunchError::config(format!("cannot create log directory: {e}")))?;
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
    Ok(Box::new(file))
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
    use crate::{
        observability::lifecycle,
        telemetry::trace::{SpanName, SpanStatus},
    };
    let root = workspace(args.scripts_dir.as_deref())?;
    let role = nonempty_env("LOOM_ROLE").or_else(|| {
        Options::parse(&args.args).ok().and_then(|o| {
            o.prompt
                .and_then(|p| prompt::role_invocation(&p).map(|(r, _)| r.to_owned()))
        })
    });
    let attempt = role
        .as_deref()
        .and_then(|r| lifecycle::worker_attempt(&root, r));
    let preflight = attempt
        .as_ref()
        .and_then(|a| a.child(SpanName::RuntimePreflight, Default::default()))
        .or_else(|| lifecycle::inherited(&root, SpanName::RuntimePreflight, Default::default()));
    let result = run_preflight(args, &root, preflight.as_ref(), attempt.as_ref());
    if let Some(span) = preflight {
        span.finish("rejected", SpanStatus::Error);
    }
    if let Some(span) = attempt {
        let outcome = if result
            .as_ref()
            .err()
            .is_some_and(|error| matches!(error.code, 126 | 127))
        {
            "spawn_failed"
        } else {
            "rejected"
        };
        span.finish_attempt(outcome, SpanStatus::Error);
    }
    result
}

fn run_preflight(
    args: WorkerArgs,
    root: &Path,
    preflight: Option<&crate::observability::lifecycle::Span>,
    attempt: Option<&crate::observability::lifecycle::Span>,
) -> Result<(), LaunchError> {
    let mut trace_identity = crate::telemetry::trace::TraceAttributes::new();
    let config = crate::config_resolver::resolve_effective_config(root);
    let (runtime, source) = if let Some(role) = nonempty_env("LOOM_ROLE") {
        let (runtime, source) = crate::runtime_admission::resolve_binding(root, &role, None)
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
    let scripts = args.scripts_dir.unwrap_or_else(|| scripts_dir(root));
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
            crate::runtime_admission::resolve_and_admit(root, role, Some(&runtime))
                .map_err(|e| LaunchError::config(e.diagnostic()))?;
        }
        let selection = profiles::select(&runtime, &options, &config)?;
        trace_identity.insert("loom.provider".into(), selection.provider.clone());
        trace_identity.insert("loom.model".into(), selection.model.clone());
        if let Some(effort) = &selection.effort {
            trace_identity.insert("loom.effort".into(), effort.clone());
        }
        if let Some(model) = &options.model {
            trace_identity.insert("loom.configured_model".into(), model.clone());
        }

        // Per-sweep ephemeral containment (issue #8403). Decided here, after
        // admission and profile selection (so a misconfigured launch still
        // fails fast on the host) but BEFORE prompt expansion and binding
        // provisioning — both of those must happen inside the container,
        // against its own isolated directories, not against the shared
        // workspace the host would use.
        if let Some(profile) = containment::resolve(&config) {
            // Forward the profile's credential variables by NAME: every
            // declared source (profile resolution re-runs inside the container
            // and fails closed on an unset required source), plus each mapped
            // child variable, which covers an operator who exported the target
            // name directly instead of the source.
            let credentials: Vec<&str> = selection
                .credential_sources
                .iter()
                .map(String::as_str)
                .chain(
                    selection
                        .credentials
                        .iter()
                        .flat_map(|(source, target)| [source.as_str(), target.as_str()]),
                )
                .collect();
            let mut command = containment::docker_command(
                &profile,
                root,
                &std::env::current_dir().map_err(|e| LaunchError::config(e.to_string()))?,
                options.log.as_deref(),
                &args.args,
                &credentials,
            )?;
            let mut log = attach_log(&mut command, options.log.as_deref())?;
            writeln!(log, "{}", profile.dispatch_marker())
                .and_then(|()| log.flush())
                .map_err(|e| LaunchError::config(e.to_string()))?;
            return exec(command);
        }
        // Fails closed (78) only when this host has a pool for the profile's
        // credential provider and none of its accounts is usable (#8401).
        let credential = credential::resolve(root, &selection)?;
        let expanded = options
            .prompt
            .as_deref()
            .map(|p| prompt::expand(root, p))
            .transpose()?;
        let mut command = harness.command(
            &options,
            &selection,
            &credential,
            expanded.as_deref(),
            root,
            role.is_some() || prompt_role.is_some(),
        )?;
        log = attach_log(&mut command, options.log.as_deref())?;
        // `credentialAccount` is an account NAME, never key material (#8401).
        // `promptBytes` (#8506) is the expanded prompt's size, so an E2BIG-class
        // failure or a slow launch is diagnosable from the log alone.
        // `tap` (#8556) is the ordered-preference resolver's own identity for
        // this launch — `<runtime>[:<profile>]`, rendered through
        // `runtime_preference::Tap` so one grep finds the same string in the
        // `# LOOM_RUNTIME_PREFERENCE` marker, this record, and the tap-attributed
        // usage accounting read back off it (`crate::tap_usage`). Derivable from
        // `runtime` + `profile`, and emitted anyway so the accounting key is
        // stated rather than reconstructed by every reader; the profile is what
        // binds *which* provider and credential source, which is why a bare
        // runtime id cannot distinguish a flat-rate coding plan from a metered
        // endpoint reached through the same harness.
        let tap = match selection.profile.as_deref() {
            Some(profile) => crate::runtime_preference::Tap::with_profile(&runtime, profile),
            None => crate::runtime_preference::Tap::runtime(&runtime),
        };
        writeln!(log, "# LOOM_LAUNCH {}", serde_json::json!({"schema":1,"runtime":runtime,"tap":tap.to_string(),"provider":selection.provider,"model":selection.model,"profile":selection.profile,"effort":selection.effort,"credentialSource":credential.source.as_str(),"credentialProvider":credential.provider,"credentialAccount":credential.account,"usage":"native-json-events","billing":"not-measured","prompt_bytes":expanded.as_deref().map(str::len)})).map_err(|e| LaunchError::config(e.to_string()))?;
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
    // CARGO_INCREMENTAL=0 for every Loom-spawned worker (#8456, parent #8453
    // item 1). Cargo keys a crate's incremental session state by the crate's
    // ABSOLUTE source path, and every Loom worktree is a new path — so state
    // written under a discarded worktree is never reused and never cleaned
    // (213 GB across 6,402 orphaned session dirs on one shared-target-dir
    // fleet host) — while sccache cannot cache an incrementally-compiled
    // crate at all, so the same host pays the disk AND loses the cache hit.
    // Unconditional by design (not `${VAR:-default}` semantics): this is the
    // spawn-time default Loom owns, never the operator's interactive shell.
    // A worker wanting incremental for one command can still prefix it —
    // `CARGO_INCREMENTAL=1 cargo …` outranks the ambient value per-invocation.
    // Every dispatch surface (sweep registry, role runner, manual
    // spawn-worker.sh) converges on this seam for native harnesses and legacy
    // adapters alike, and a containment container's re-exec'd spawn-worker.sh
    // re-enters it inside the container, so one site covers all of them.
    // Docker boundaries that bypass it (spawn-claude.sh's containment,
    // spawn-codex.sh's session-exec) export it explicitly on their own.
    command.env("CARGO_INCREMENTAL", "0");
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
    trace_identity.insert("loom.runtime".into(), runtime.clone());
    let mut runtime_span = None;
    if let Some(preflight) = preflight {
        let mut accepted = trace_identity.clone();
        accepted.insert("loom.result".into(), "accepted".into());
        preflight.finish_attributes(crate::telemetry::trace::SpanStatus::Ok, accepted);
        if let Some(run) = attempt
            .and_then(|a| {
                a.child(crate::telemetry::trace::SpanName::RuntimeRun, trace_identity.clone())
            })
            .or_else(|| {
                crate::observability::lifecycle::inherited(
                    root,
                    crate::telemetry::trace::SpanName::RuntimeRun,
                    trace_identity.clone(),
                )
            })
        {
            run.command(&mut command);
            runtime_span = Some(run);
        }
    }
    let result = exec(command);
    if let Some(span) = runtime_span {
        span.finish("spawn_failed", crate::telemetry::trace::SpanStatus::Error);
    }
    result
}

#[cfg(unix)]
fn exec(mut command: Command) -> Result<(), LaunchError> {
    use std::os::unix::process::CommandExt;
    let error = command.exec();
    // #8506: E2BIG (`os error 7`, "Argument list too long") is a distinct
    // launch-class failure — the argv itself was too big for the kernel to
    // accept, never a runtime/config fault — so `role_runner` and any
    // backoff from #8505 can key off the message instead of lumping it in
    // with an arbitrary "cannot execute" failure. The exit code stays `126`
    // either way (unchanged classification for every existing caller); only
    // the message gains a named class prefix.
    let message = if error.raw_os_error() == Some(libc::E2BIG) {
        format!("cannot execute worker harness: argument list too long ({error}); an argv element exceeded the kernel's MAX_ARG_STRLEN")
    } else {
        format!("cannot execute worker harness: {error}")
    };
    Err(LaunchError {
        code: if error.kind() == std::io::ErrorKind::NotFound {
            127
        } else {
            126
        },
        message,
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

pub fn cli(args: WorkerArgs) -> anyhow::Result<()> {
    if let Err(error) = run(args) {
        eprintln!("{}", error.message);
        std::process::exit(error.code);
    }
    Ok(())
}
