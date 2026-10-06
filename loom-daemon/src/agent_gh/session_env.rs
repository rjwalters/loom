//! Interactive sessions get the agent `gh` front too (#10516, slice A).
//!
//! Dispatched workers reach the front because `worker_spawn` puts it first on
//! their `PATH`. An operator's interactive Claude Code session — and every
//! Task subagent it launches — never went through that spawn, so its plain
//! `gh` reads all spent GraphQL. A `SessionStart` hook
//! (`defaults/hooks/gh-front-env.sh`, wired by
//! `scripts/install/provision-hooks.sh`) runs `loom-daemon gh-shim
//! session-env`, which appends one guarded `PATH` line to `$CLAUDE_ENV_FILE`.
//! Claude Code sources that file before every Bash tool call, in the session
//! and in its Task subagents alike (verified on Claude Code 2.1.291; a
//! `SubagentStart` hook is not given a `CLAUDE_ENV_FILE`, and does not need
//! one).
//!
//! The prefix is [`super::session_path`], the same composition a worker gets:
//! the managed launcher (#9987) first when a policy resolves one, then the
//! front, then whatever `PATH` already had — so with no policy the front's own
//! conditional reads go to the next `gh` (e.g. the 2am telemetry shim), which
//! keeps seeing every forge call.
//!
//! This is a cost optimisation, not a guard: every failure is a no-op with at
//! most a stderr warning, and [`run`] always exits 0. It does nothing when
//! `$CLAUDE_ENV_FILE` is unset, under `LOOM_GH_SHIM=0`, outside a Loom
//! workspace, or on Gitea. The written line is idempotent twice over: it is
//! not appended again when the file already carries it ([`MARKER`]) or when
//! `PATH` already starts with the prefix (a dispatched worker), and the line
//! itself skips the prepend when `PATH` already starts with the prefix.

use std::ffi::OsStr;
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::forge_egress::worker_env::WorkerEgress;

/// Trailing comment on the written line; its presence means "already done".
pub const MARKER: &str = "# loom gh front (#10516)";

/// What [`append_env`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Written {
    Appended,
    /// The env file already carries [`MARKER`].
    AlreadyPresent,
    /// `PATH` already starts with the prefix (e.g. a dispatched worker).
    AlreadyFirst,
}

/// `loom-daemon gh-shim session-env`. Always 0: a SessionStart hook must
/// never be the reason a session misbehaves.
#[must_use]
pub fn run() -> i32 {
    if let Err(why) = try_run() {
        eprintln!("loom gh front (session-env, #10516): {why}; session PATH unchanged");
    }
    0
}

/// `Ok` covers both "written" and every silent no-op.
fn try_run() -> Result<(), String> {
    let Some(env_file) = std::env::var_os("CLAUDE_ENV_FILE").filter(|f| !f.is_empty()) else {
        return Ok(());
    };
    if std::env::var(super::OPT_OUT_ENV).is_ok_and(|v| v == "0") {
        return Ok(());
    }
    let Some(root) = workspace_root().filter(|r| is_loom_workspace(r)) else {
        return Ok(());
    };
    if crate::forge_cmd::detect_forge(Some(&root)) == crate::forge_cmd::ForgeType::Gitea {
        return Ok(());
    }
    let egress = WorkerEgress::admit_process()
        .map_err(|f| crate::forge_egress::worker_env::refusal_message(&f))?
        .egress;
    let prefix = super::session_path(None, egress.as_ref())
        .ok_or("the gh shim directory could not be created")?;
    let current = std::env::var_os("PATH");
    append_env(Path::new(&env_file), &prefix, current.as_deref())
        .map(|_| ())
        .map_err(|e| format!("cannot write {}: {e}", Path::new(&env_file).display()))
}

/// The guarded `PATH` line for `prefix`, or `None` when `prefix` is not UTF-8.
/// Single-quoted, so a path with spaces or `$` stays literal; the `case`
/// skips the prepend when `PATH` already starts with `prefix`.
#[must_use]
pub fn env_line(prefix: &OsStr) -> Option<String> {
    let q = format!("'{}'", prefix.to_str()?.replace('\'', r"'\''"));
    Some(format!(
        "case \":$PATH:\" in :{q}:*) ;; *) PATH={q}${{PATH:+:$PATH}}; export PATH ;; esac {MARKER}\n"
    ))
}

/// Append [`env_line`] to `file` unless it is already there or `current`
/// already starts with `prefix`.
///
/// # Errors
///
/// When `prefix` is not UTF-8 or `file` cannot be appended to.
pub fn append_env(
    file: &Path,
    prefix: &OsStr,
    current: Option<&OsStr>,
) -> std::io::Result<Written> {
    if std::fs::read_to_string(file).is_ok_and(|s| s.contains(MARKER)) {
        return Ok(Written::AlreadyPresent);
    }
    let want: Vec<PathBuf> = std::env::split_paths(prefix).collect();
    let have: Vec<PathBuf> = current.map(std::env::split_paths).into_iter().flatten().collect();
    if have.starts_with(&want) {
        return Ok(Written::AlreadyFirst);
    }
    let line = env_line(prefix)
        .ok_or_else(|| std::io::Error::other("the gh shim path is not valid UTF-8"))?;
    let mut f = std::fs::OpenOptions::new().create(true).append(true).open(file)?;
    f.write_all(line.as_bytes())?;
    Ok(Written::Appended)
}

/// The workspace the hook fired for: `LOOM_PROJECT_ROOT` (set by the hook
/// wrapper), else the main checkout of the cwd's repository, else
/// `CLAUDE_PROJECT_DIR`.
#[must_use]
pub fn workspace_root() -> Option<PathBuf> {
    let env_dir = |k: &str| std::env::var_os(k).filter(|v| !v.is_empty()).map(PathBuf::from);
    if let Some(root) = env_dir("LOOM_PROJECT_ROOT") {
        return Some(root);
    }
    let cwd = std::env::current_dir().ok();
    let common = std::process::Command::new("git")
        .args(["rev-parse", "--git-common-dir"])
        .stderr(std::process::Stdio::null())
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty());
    if let (Some(common), Some(cwd)) = (common, cwd.as_ref()) {
        if let Some(parent) = cwd.join(common).parent() {
            return Some(parent.to_path_buf());
        }
    }
    env_dir("CLAUDE_PROJECT_DIR").or(cwd)
}

/// A Loom workspace, by the same test the user-scope hook wrapper applies.
#[must_use]
pub fn is_loom_workspace(root: &Path) -> bool {
    root.join(".loom/config.json").is_file() || root.join(".loom-project/project.json").is_file()
}

/// `loom-daemon gh-shim status`: print `front|launcher|bypassed: <gh>` for
/// the first `gh` on this process's `PATH`. Exit 1 only for `bypassed`.
#[must_use]
pub fn status() -> i32 {
    let launcher = WorkerEgress::admit_process()
        .ok()
        .and_then(|a| a.egress)
        .map(|e| e.launcher);
    let Some(gh) = first_gh(std::env::var_os("PATH").as_deref()) else {
        println!("bypassed: no gh on PATH");
        return 1;
    };
    let kind = classify_gh(&gh, launcher.as_deref());
    println!("{kind}: {}", gh.display());
    i32::from(kind == "bypassed")
}

/// The first executable `gh` on `path`.
#[must_use]
pub fn first_gh(path: Option<&OsStr>) -> Option<PathBuf> {
    path.map(std::env::split_paths)
        .into_iter()
        .flatten()
        .map(|d| d.join("gh"))
        .find(|c| super::next_gh::is_executable(c))
}

/// `front` when `gh` resolves to a `loom-daemon`, `launcher` when it is the
/// policy's managed launcher, else `bypassed`.
#[must_use]
pub fn classify_gh(gh: &Path, launcher: Option<&Path>) -> &'static str {
    let canon = gh.canonicalize().unwrap_or_else(|_| gh.to_path_buf());
    if canon.file_name() == Some(OsStr::new("loom-daemon")) {
        "front"
    } else if launcher.is_some_and(|l| l.canonicalize().unwrap_or_else(|_| l.to_path_buf()) == canon)
    {
        "launcher"
    } else {
        "bypassed"
    }
}
